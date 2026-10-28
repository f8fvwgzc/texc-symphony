//! The tracker's scoped tools for Claude Code, over MCP, without handing Claude the token.
//!
//! ```text
//! claude ──stdio MCP──▶ `symphony agent-tools --socket S` ──Unix socket S──▶ ToolRelay (in the
//!                                                                          Symphony process)
//!                                                                          └▶ DynamicToolHandler
//! ```
//!
//! * [`ToolRelay`] lives in the runner for one session: it listens on a Unix socket in a fresh
//!   `0700` directory and executes calls with the session's [`DynamicToolHandler`] (the same
//!   scoped tools Codex gets), so the tracker credential never leaves Symphony. It also writes
//!   the `--mcp-config` file Claude Code is started with.
//! * [`serve_stdio`] is the MCP server Claude Code starts (`symphony agent-tools`): it answers
//!   `initialize`, `ping`, `tools/list` and `tools/call` by relaying to the socket.
//!
//! Relay protocol (one JSON object per line): `{"op":"list"}` → `{"tools":[spec...]}`;
//! `{"op":"call","name":N,"arguments":A}` → `{"result":{"success","output","contentItems"}}`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use symphony_codex::DynamicToolHandler;
use symphony_codex::dynamic_tool::{normalize_tool_result, tool_response};
use symphony_core::Issue;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// MCP server name Claude Code sees; tools are `mcp__symphony__<tool>`.
pub const SERVER_NAME: &str = "symphony";
/// MCP protocol version answered when the client does not name one.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The Claude Code tool name of a bridged tool.
pub fn claude_tool_name(tool: &str) -> String {
    format!("mcp__{SERVER_NAME}__{tool}")
}

static RELAYS: AtomicU64 = AtomicU64::new(0);

/// Executes one session's tool calls arriving on a Unix socket (see the module docs).
#[derive(Debug)]
pub struct ToolRelay {
    dir: PathBuf,
    socket: PathBuf,
    config: PathBuf,
    tools: Vec<String>,
    issue: Arc<Mutex<Issue>>,
    task: JoinHandle<()>,
}

impl ToolRelay {
    /// Listens for `handler`'s tools and writes an MCP config that starts `program agent-tools
    /// --socket <socket>`. Each call is bounded by `call_timeout`.
    pub async fn start(
        handler: Arc<dyn DynamicToolHandler>,
        program: &Path,
        call_timeout: Duration,
    ) -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "symphony-tools-{}-{}",
            std::process::id(),
            RELAYS.fetch_add(1, Ordering::Relaxed)
        ));
        create_private_dir(&dir)?;
        let socket = dir.join("tools.sock");
        let listener = UnixListener::bind(&socket)?;
        let config = dir.join("mcp.json");
        let body = json!({"mcpServers": {SERVER_NAME: {
            "type": "stdio",
            "command": program.display().to_string(),
            "args": ["agent-tools", "--socket", socket.display().to_string()],
        }}});
        write_private_file(&config, body.to_string().as_bytes())?;
        let tools = handler
            .tool_specs()
            .iter()
            .filter_map(|spec| spec.get("name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        let issue = Arc::new(Mutex::new(Issue::default()));
        let task = tokio::spawn(accept(listener, handler, Arc::clone(&issue), call_timeout));
        Ok(Self {
            dir,
            socket,
            config,
            tools,
            issue,
            task,
        })
    }

    /// The socket path.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The `--mcp-config` file.
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// Claude Code names of the bridged tools (for `--allowedTools`).
    pub fn claude_tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| claude_tool_name(t)).collect()
    }

    /// The issue later calls run for.
    pub fn set_issue(&self, issue: &Issue) {
        *self.issue.lock().unwrap_or_else(PoisonError::into_inner) = issue.clone();
    }

    /// Hands the relay's directory, socket and config to `uid`/`gid`, so an agent running as
    /// that task user can reach it (the directory stays `0700`).
    pub fn grant(&self, uid: u32, gid: u32) -> io::Result<()> {
        for path in [&self.dir, &self.socket, &self.config] {
            std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
        }
        Ok(())
    }
}

impl Drop for ToolRelay {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir(dir)
}

#[cfg(unix)]
fn write_private_file(path: &Path, body: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(body)
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, body: &[u8]) -> io::Result<()> {
    std::fs::write(path, body)
}

async fn accept(
    listener: UnixListener,
    handler: Arc<dyn DynamicToolHandler>,
    issue: Arc<Mutex<Issue>>,
    call_timeout: Duration,
) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(serve_relay(
            stream,
            Arc::clone(&handler),
            Arc::clone(&issue),
            call_timeout,
        ));
    }
}

/// Longest relay request accepted, in bytes (tool arguments are small JSON documents).
pub const MAX_RELAY_LINE: usize = 4 * 1024 * 1024;

/// The next `\n`-terminated line of at most `max` bytes; `Ok(None)` at EOF or when a line is
/// longer (the connection is then dropped instead of buffering it).
async fn next_capped_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max: usize,
) -> std::io::Result<Option<String>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let read = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut buf)
        .await?;
    if read == 0 || (buf.len() > max && buf.last() != Some(&b'\n')) {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

async fn serve_relay(
    stream: UnixStream,
    handler: Arc<dyn DynamicToolHandler>,
    issue: Arc<Mutex<Issue>>,
    call_timeout: Duration,
) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    while let Ok(Some(line)) = next_capped_line(&mut reader, MAX_RELAY_LINE).await {
        let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let reply = match request.get("op").and_then(Value::as_str) {
            Some("list") => json!({"tools": handler.tool_specs()}),
            Some("call") => {
                let name = request
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                let arguments = request.get("arguments").cloned().unwrap_or(json!({}));
                let current = issue.lock().unwrap_or_else(PoisonError::into_inner).clone();
                let result =
                    tokio::time::timeout(call_timeout, handler.execute(name, arguments, &current))
                        .await
                        .map_or_else(
                            |_| {
                                tool_response(
                                    false,
                                    json!({"error": {"message": format!(
                                        "Tool call timed out after {}ms.",
                                        call_timeout.as_millis()
                                    )}})
                                    .to_string(),
                                )
                            },
                            normalize_tool_result,
                        );
                json!({"result": result})
            }
            _ => json!({"error": "unknown relay request"}),
        };
        let mut text = reply.to_string();
        text.push('\n');
        if write.write_all(text.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// A connection to a [`ToolRelay`].
struct RelayClient {
    socket: PathBuf,
    conn: Option<(
        tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        tokio::net::unix::OwnedWriteHalf,
    )>,
}

impl RelayClient {
    async fn request(&mut self, request: Value) -> io::Result<Value> {
        if self.conn.is_none() {
            let (read, write) = UnixStream::connect(&self.socket).await?.into_split();
            self.conn = Some((BufReader::new(read).lines(), write));
        }
        let Some((lines, write)) = self.conn.as_mut() else {
            return Err(io::Error::other("relay connection unavailable"));
        };
        let mut text = request.to_string();
        text.push('\n');
        write.write_all(text.as_bytes()).await?;
        match lines.next_line().await? {
            Some(line) => serde_json::from_str(&line).map_err(io::Error::other),
            None => {
                self.conn = None;
                Err(io::Error::other("the relay closed the connection"))
            }
        }
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

async fn handle(client: &mut RelayClient, message: &Value) -> Option<Value> {
    let id = message.get("id")?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(json!({}));
    Some(match method {
        "initialize" => rpc_result(
            id,
            json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => match client.request(json!({"op": "list"})).await {
            Ok(reply) => {
                let tools: Vec<Value> = reply
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|spec| {
                        json!({
                            "name": spec.get("name").cloned().unwrap_or(Value::Null),
                            "description": spec.get("description").cloned()
                                .unwrap_or(Value::String(String::new())),
                            "inputSchema": spec.get("inputSchema").cloned()
                                .unwrap_or_else(|| json!({"type": "object"})),
                        })
                    })
                    .collect();
                rpc_result(id, json!({"tools": tools}))
            }
            Err(err) => rpc_error(id, -32603, &format!("tool relay unavailable: {err}")),
        },
        "tools/call" => {
            let request = json!({"op": "call", "name": params.get("name"),
                "arguments": params.get("arguments").cloned().unwrap_or(json!({}))});
            match client.request(request).await {
                Ok(reply) => {
                    let result = reply.get("result").cloned().unwrap_or(Value::Null);
                    let success = result.get("success") == Some(&Value::Bool(true));
                    let output = result
                        .get("output")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    rpc_result(
                        id,
                        json!({"content": [{"type": "text", "text": output}], "isError": !success}),
                    )
                }
                Err(err) => rpc_error(id, -32603, &format!("tool relay unavailable: {err}")),
            }
        }
        _ => rpc_error(id, -32601, "Method not found"),
    })
}

/// The `symphony agent-tools` MCP server: JSON-RPC lines on `input`, answers on `output`,
/// tool calls relayed to `socket`. Returns at end of input.
pub async fn serve_stdio<R, W>(socket: &Path, input: R, mut output: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut client = RelayClient {
        socket: socket.to_path_buf(),
        conn: None,
    };
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) if line.trim().is_empty() => continue,
            Err(_) => rpc_error(&Value::Null, -32700, "Parse error"),
        };
        let reply = if message.get("error").is_some() && message.get("method").is_none() {
            Some(message)
        } else {
            handle(&mut client, &message).await
        };
        if let Some(reply) = reply {
            let mut text = reply.to_string();
            text.push('\n');
            output.write_all(text.as_bytes()).await?;
            output.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_lines_are_capped() {
        let input: &[u8] = b"short\r\nanother line that is far too long\nnext\n";
        let mut reader = BufReader::new(input);
        assert_eq!(
            next_capped_line(&mut reader, 16).await.unwrap().as_deref(),
            Some("short")
        );
        assert_eq!(next_capped_line(&mut reader, 16).await.unwrap(), None);
        let mut empty = BufReader::new(&b""[..]);
        assert_eq!(next_capped_line(&mut empty, 16).await.unwrap(), None);
    }
    use async_trait::async_trait;
    use tokio::io::AsyncBufReadExt;

    /// `(tool, arguments, issue identifier)` of one call.
    type Call = (Option<String>, Value, Option<String>);

    #[derive(Default)]
    struct Recording {
        calls: Mutex<Vec<Call>>,
    }

    #[async_trait]
    impl DynamicToolHandler for Recording {
        fn tool_specs(&self) -> Vec<Value> {
            vec![json!({"name": "github_api", "description": "GitHub",
                "inputSchema": {"type": "object", "required": ["method", "path"]}})]
        }

        async fn execute(&self, tool: Option<&str>, arguments: Value, issue: &Issue) -> Value {
            self.calls.lock().unwrap().push((
                tool.map(str::to_owned),
                arguments.clone(),
                issue.identifier.clone(),
            ));
            if arguments.get("path").and_then(Value::as_str) == Some("/denied") {
                return tool_response(false, "{\"error\":{\"rule\":\"path_out_of_scope\"}}");
            }
            tool_response(true, "{\"status\":200}")
        }
    }

    #[tokio::test]
    async fn mcp_calls_are_relayed_to_the_session_handler() {
        let handler = Arc::new(Recording::default());
        let relay = ToolRelay::start(
            handler.clone(),
            Path::new("/usr/local/bin/symphony"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        relay.set_issue(&Issue {
            identifier: Some("TASK-7".into()),
            ..Issue::default()
        });
        assert_eq!(relay.claude_tool_names(), vec!["mcp__symphony__github_api"]);
        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(relay.config()).unwrap()).unwrap();
        assert_eq!(
            config["mcpServers"]["symphony"]["args"],
            json!([
                "agent-tools",
                "--socket",
                relay.socket().display().to_string()
            ])
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = relay.socket().parent().unwrap();
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        let requests = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-03-26"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": "github_api", "arguments": {"method": "GET", "path": "/repos/o/r"}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
                "name": "github_api", "arguments": {"method": "GET", "path": "/denied"}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}),
        ];
        let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
        let (out_write, out_read) = tokio::io::duplex(1 << 16);
        serve_stdio(relay.socket(), input.as_bytes(), out_write)
            .await
            .unwrap();
        let mut replies = Vec::new();
        let mut lines = BufReader::new(out_read).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            replies.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(replies.len(), 5, "no reply to the notification");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "symphony");
        assert_eq!(replies[1]["result"]["tools"][0]["name"], "github_api");
        assert_eq!(
            replies[1]["result"]["tools"][0]["inputSchema"]["required"],
            json!(["method", "path"])
        );
        assert_eq!(
            replies[2]["result"],
            json!({"content": [{"type": "text", "text": "{\"status\":200}"}], "isError": false})
        );
        assert_eq!(replies[3]["result"]["isError"], true);
        assert_eq!(replies[4]["error"]["code"], -32601);

        let calls = handler.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0.as_deref(), Some("github_api"));
        assert_eq!(calls[0].1["path"], "/repos/o/r");
        assert_eq!(calls[0].2.as_deref(), Some("TASK-7"));

        let dir = relay.socket().parent().unwrap().to_path_buf();
        drop(relay);
        assert!(!dir.exists(), "the relay directory is removed");
    }

    #[tokio::test]
    async fn a_missing_relay_is_an_rpc_error_not_a_crash() {
        let input = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
        );
        let mut output = Vec::new();
        serve_stdio(
            Path::new("/nonexistent/tools.sock"),
            input.as_bytes(),
            &mut output,
        )
        .await
        .unwrap();
        let reply: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(reply["error"]["code"], -32603);
    }
}
