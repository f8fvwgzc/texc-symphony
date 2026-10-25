//! The `claude_code` backend: turns run by the Claude Code CLI.
//!
//! Each Symphony turn is one `claude -p` process in the issue workspace, launched like Codex
//! (`bash -lc '[unset SECRETS && ]exec <command> <args>'`, its own process group, the tracker and
//! API secrets removed from the environment):
//!
//! ```text
//! <command> -p --output-format stream-json --verbose [--model M] --permission-mode MODE
//!   [--allowedTools T...] [--disallowedTools T...] [--max-turns N]
//!   [--append-system-prompt S] [--resume SESSION] [extra_args...]
//! ```
//!
//! The prompt goes to stdin. The first turn starts a new Claude session; its id (from the `init`
//! or `result` message) is passed as `--resume` on later turns, so a multi-turn run keeps its
//! context. `--output-format stream-json` lines become events ([`stream`]). When
//! `api_key_env` is set its value is passed as `ANTHROPIC_API_KEY`; otherwise Claude Code uses
//! its own login. A turn that prints nothing for `turn_timeout_ms` is killed (`turn_timeout`);
//! a dropped turn kills its whole process group.
//!
//! The tracker's scoped tools reach Claude Code through MCP ([`crate::mcp`]): when the session
//! has tools, each turn also gets `--mcp-config <file> --strict-mcp-config` (a `symphony
//! agent-tools` server relaying to this process) and the `mcp__symphony__<tool>` names in
//! `--allowedTools`. The tracker token stays in Symphony.
//!
//! Not supported yet: SSH workers (`agent_backend_unsupported`).

mod stream;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use symphony_codex::launch::{
    find_executable, local_launch_command, shell_escape, valid_secret_names,
    validate_local_workspace,
};
use symphony_codex::{CodexError, EventSink, StartOptions, TurnOutcome};
use symphony_core::config::{BackendKind, BackendSettings, ClaudeCodeSettings};
use symphony_core::{EnvSource, Issue, ProcessEnv};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use self::stream::{Finished, TurnStream};
use crate::events::{Emitter, Usage};
use crate::process::GroupChild;
use crate::{AgentBackend, AgentSession};

/// Env var Claude Code reads its API key from.
pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
/// Longest stdout line accepted (longer lines are skipped).
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
/// stderr lines kept for error messages.
const STDERR_TAIL_LINES: usize = 20;
/// Characters kept per stderr line.
const STDERR_LINE_CHARS: usize = 300;
/// How long a finished turn's process may take to exit before its group is killed.
const EXIT_GRACE: Duration = Duration::from_secs(10);

/// Runs turns with the Claude Code CLI (see the module docs).
#[derive(Debug, Clone)]
pub struct ClaudeCodeBackend {
    settings: BackendSettings,
    env: Arc<dyn EnvSource>,
    /// The `symphony` executable that serves `agent-tools`; `None` disables the tool bridge.
    bridge_program: Option<PathBuf>,
}

impl ClaudeCodeBackend {
    /// The backend for `settings` (kind `claude_code`), reading `api_key_env` from the process
    /// environment.
    pub fn new(settings: BackendSettings) -> Self {
        Self {
            settings,
            env: Arc::new(ProcessEnv),
            bridge_program: std::env::current_exe().ok(),
        }
    }

    /// Reads `api_key_env` from `env` instead (tests, embedding).
    pub fn with_env(mut self, env: Arc<dyn EnvSource>) -> Self {
        self.env = env;
        self
    }

    /// The executable whose `agent-tools` subcommand serves the tracker tools (default: the
    /// running executable, which is `symphony` in production); `None` disables the bridge.
    pub fn with_tool_bridge(mut self, program: Option<PathBuf>) -> Self {
        self.bridge_program = program;
        self
    }

    fn claude(&self) -> ClaudeCodeSettings {
        match &self.settings.kind {
            BackendKind::ClaudeCode(claude) => claude.clone(),
            _ => ClaudeCodeSettings::default(),
        }
    }
}

/// The MCP tool bridge of a turn: the `--mcp-config` file and the tool names to allow.
#[derive(Debug, Clone, Copy)]
pub struct ToolBridge<'a> {
    /// `--mcp-config` path.
    pub config: &'a str,
    /// `mcp__symphony__<tool>` names added to `--allowedTools`.
    pub tools: &'a [String],
}

/// The shell code that runs one turn: `<command>` (raw shell code, like `codex.command`)
/// followed by the escaped arguments.
pub fn turn_command(
    claude: &ClaudeCodeSettings,
    model: Option<&str>,
    system_prompt: Option<&str>,
    resume: Option<&str>,
    bridge: Option<ToolBridge<'_>>,
) -> String {
    let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    if let Some(model) = model {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    args.extend([
        "--permission-mode".to_owned(),
        claude.permission_mode.clone(),
    ]);
    let bridged = bridge.map_or(&[][..], |b| b.tools);
    if !claude.allowed_tools.is_empty() || !bridged.is_empty() {
        args.push("--allowedTools".to_owned());
        args.extend(claude.allowed_tools.iter().cloned());
        args.extend(bridged.iter().cloned());
    }
    if !claude.disallowed_tools.is_empty() {
        args.push("--disallowedTools".to_owned());
        args.extend(claude.disallowed_tools.iter().cloned());
    }
    if let Some(max_turns) = claude.max_turns {
        args.extend(["--max-turns".to_owned(), max_turns.to_string()]);
    }
    if let Some(system_prompt) = system_prompt.filter(|s| !s.trim().is_empty()) {
        args.extend([
            "--append-system-prompt".to_owned(),
            system_prompt.to_owned(),
        ]);
    }
    if let Some(bridge) = bridge {
        args.extend([
            "--mcp-config".to_owned(),
            bridge.config.to_owned(),
            "--strict-mcp-config".to_owned(),
        ]);
    }
    if let Some(resume) = resume {
        args.extend(["--resume".to_owned(), resume.to_owned()]);
    }
    args.extend(claude.extra_args.iter().cloned());
    std::iter::once(claude.command.clone())
        .chain(args.iter().map(|arg| shell_escape(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A session id that is unique enough for event correlation (no RNG dependency needed).
fn local_thread_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("claude-{:x}-{:x}", std::process::id(), nanos)
}

#[async_trait]
impl AgentBackend for ClaudeCodeBackend {
    fn name(&self) -> &str {
        &self.settings.name
    }

    async fn start(&self, options: StartOptions) -> Result<Box<dyn AgentSession>, CodexError> {
        if let Some(host) = &options.worker_host {
            return Err(CodexError::Unsupported(format!(
                "the claude_code backend runs locally only; worker_host={host} is not supported"
            )));
        }
        let root = options
            .settings
            .local_workspace_root(&options.workflow_path);
        let workspace = validate_local_workspace(&options.workspace, &root)
            .map_err(CodexError::InvalidWorkspaceCwd)?;
        let bash = find_executable("bash").ok_or(CodexError::BashNotFound)?;
        let claude = self.claude();
        let api_key = claude
            .api_key_env
            .as_deref()
            .and_then(|name| self.env.var(name))
            .filter(|key| !key.is_empty());
        let mut secret_names = valid_secret_names(
            options
                .settings
                .secret_environment_names()
                .into_iter()
                .chain(options.tool_handler.secret_environment_names()),
        );
        if api_key.is_some() {
            // Replaced below by the configured key rather than removed.
            secret_names.retain(|name| name != ANTHROPIC_API_KEY);
        }
        let relay = self.relay(&options).await;
        Ok(Box::new(ClaudeSession {
            backend: self.settings.clone(),
            claude,
            workspace,
            bash,
            secret_names,
            env: options.env,
            api_key,
            thread_id: local_thread_id(),
            claude_session: None,
            usage: Usage::default(),
            turns: 0,
            relay,
        }))
    }
}

impl ClaudeCodeBackend {
    #[cfg(unix)]
    async fn relay(&self, options: &StartOptions) -> Option<crate::mcp::ToolRelay> {
        let program = self.bridge_program.as_deref()?;
        if options.tool_handler.tool_specs().is_empty() {
            return None;
        }
        let timeout = Duration::from_millis(self.settings.turn_timeout_ms);
        match crate::mcp::ToolRelay::start(Arc::clone(&options.tool_handler), program, timeout)
            .await
        {
            Ok(relay) => Some(relay),
            Err(err) => {
                tracing::warn!("Claude Code runs without the tracker tools: {err}");
                None
            }
        }
    }

    #[cfg(not(unix))]
    async fn relay(&self, _options: &StartOptions) -> Option<()> {
        None
    }
}

/// One Symphony session: a sequence of `claude -p` processes sharing a Claude session.
struct ClaudeSession {
    backend: BackendSettings,
    claude: ClaudeCodeSettings,
    workspace: PathBuf,
    bash: PathBuf,
    secret_names: Vec<String>,
    env: Vec<(String, String)>,
    api_key: Option<String>,
    thread_id: String,
    claude_session: Option<String>,
    usage: Usage,
    turns: u64,
    /// The tracker tool bridge; dropped (socket closed, files removed) with the session.
    #[cfg(unix)]
    relay: Option<crate::mcp::ToolRelay>,
    #[cfg(not(unix))]
    relay: Option<()>,
}

impl ClaudeSession {
    fn command(&self) -> Command {
        #[cfg(unix)]
        let (config, tools) = match &self.relay {
            Some(relay) => (
                Some(relay.config().display().to_string()),
                relay.claude_tool_names(),
            ),
            None => (None, Vec::new()),
        };
        #[cfg(not(unix))]
        let (config, tools): (Option<String>, Vec<String>) = (None, Vec::new());
        let bridge = config.as_deref().map(|config| ToolBridge {
            config,
            tools: &tools,
        });
        let line = turn_command(
            &self.claude,
            self.backend.model.as_deref(),
            self.backend.system_prompt.as_deref(),
            self.claude_session.as_deref(),
            bridge,
        );
        let mut command = Command::new(&self.bash);
        command
            .arg("-lc")
            .arg(local_launch_command(&line, &self.secret_names))
            .current_dir(&self.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in &self.env {
            command.env(name, value);
        }
        for name in &self.secret_names {
            command.env_remove(name);
        }
        if let Some(key) = &self.api_key {
            command.env(ANTHROPIC_API_KEY, key);
        }
        command
    }

    async fn turn(
        &mut self,
        prompt: &str,
        emitter_sink: &EventSink,
        turn_id: &str,
    ) -> (String, Result<TurnOutcome, CodexError>) {
        let mut child = match GroupChild::spawn(&mut self.command()) {
            Ok(child) => child,
            Err(err) => {
                let emitter = Emitter::new(emitter_sink, None);
                let session_id = emitter.session_started(&self.thread_id, turn_id);
                return (session_id, Err(CodexError::SpawnFailed(err.to_string())));
            }
        };
        let emitter = Emitter::new(emitter_sink, child.pid());
        let session_id = emitter.session_started(&self.thread_id, turn_id);
        let (Some(mut stdin), Some(stdout), Some(stderr)) = (
            child.child_mut().stdin.take(),
            child.child_mut().stdout.take(),
            child.child_mut().stderr.take(),
        ) else {
            return (
                session_id,
                Err(CodexError::SpawnFailed("stdio pipes unavailable".into())),
            );
        };
        // Written concurrently so a large prompt cannot deadlock against a full stdout pipe.
        let prompt = prompt.to_owned();
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let mut stderr_task = tokio::spawn(collect_stderr(stderr, Arc::clone(&tail)));

        let silence = Duration::from_millis(self.backend.turn_timeout_ms);
        let mut lines = LineReader::new(stdout);
        let mut stream = TurnStream::new(&self.thread_id, turn_id);
        let mut usage = self.usage;
        let outcome = loop {
            let line = match tokio::time::timeout(silence, lines.next_line()).await {
                Err(_) => {
                    child.kill().await;
                    break Err(CodexError::TurnTimeout);
                }
                Ok(None) => {
                    let status = tokio::time::timeout(EXIT_GRACE, child.wait()).await;
                    let status = match status {
                        Ok(Ok(status)) => status
                            .code()
                            .map_or_else(|| "a signal".to_owned(), |c| format!("status {c}")),
                        _ => "an unknown status".to_owned(),
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await;
                    break Err(CodexError::Backend(format!(
                        "claude exited with {status} before reporting a result{}",
                        stderr_suffix(&tail)
                    )));
                }
                Ok(Some(line)) => line,
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                if !line.trim().is_empty() {
                    tracing::debug!(
                        "Ignoring a non-JSON line from claude: {}",
                        truncate(&line, 300)
                    );
                }
                continue;
            };
            let finished = stream.handle(&value, &emitter, &mut usage);
            if let Some(id) = stream.session_id.clone() {
                self.claude_session = Some(id);
            }
            let Some(finished) = finished else {
                continue;
            };
            if tokio::time::timeout(EXIT_GRACE, child.wait())
                .await
                .is_err()
            {
                child.kill().await;
            }
            break match finished {
                Finished::Success(text) => {
                    emitter.turn_completed(&self.thread_id, turn_id);
                    Ok(TurnOutcome {
                        session_id: session_id.clone(),
                        thread_id: self.thread_id.clone(),
                        turn_id: turn_id.to_owned(),
                        final_message: text,
                    })
                }
                Finished::Error { subtype, message } => {
                    let detail = message
                        .map(|m| format!(": {}", truncate(&m, 500)))
                        .unwrap_or_default();
                    Err(CodexError::Backend(format!(
                        "claude reported {subtype}{detail}{}",
                        stderr_suffix(&tail)
                    )))
                }
            };
        };
        self.usage = usage;
        writer.abort();
        stderr_task.abort();
        (session_id, outcome)
    }
}

#[async_trait]
impl AgentSession for ClaudeSession {
    async fn run_turn(
        &mut self,
        prompt: &str,
        issue: &Issue,
        events: &EventSink,
    ) -> Result<TurnOutcome, CodexError> {
        self.turns += 1;
        let turn_id = format!("turn-{}", self.turns);
        #[cfg(unix)]
        if let Some(relay) = &self.relay {
            relay.set_issue(issue);
        }
        tracing::info!(
            issue_identifier = issue.identifier.as_deref().unwrap_or(""),
            "Starting Claude Code turn {turn_id} resume={}",
            self.claude_session.is_some()
        );
        let (session_id, outcome) = self.turn(prompt, events, &turn_id).await;
        if let Err(reason) = &outcome {
            Emitter::new(events, None).turn_failed(&session_id, reason);
        }
        outcome
    }

    fn pid(&self) -> Option<u32> {
        None
    }

    async fn stop(self: Box<Self>) {
        // Nothing outlives a turn: each turn's process group is gone when the turn returns or
        // is dropped.
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn stderr_suffix(tail: &Mutex<VecDeque<String>>) -> String {
    let tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
    let lines: Vec<&String> = tail.iter().rev().take(5).collect();
    if lines.is_empty() {
        return String::new();
    }
    let text = lines
        .into_iter()
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    format!(" (stderr: {text})")
}

async fn collect_stderr<R: AsyncRead + Unpin>(stderr: R, tail: Arc<Mutex<VecDeque<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        tracing::debug!("claude stderr: {}", truncate(&line, STDERR_LINE_CHARS));
        let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(truncate(&line, STDERR_LINE_CHARS));
    }
}

/// Newline-delimited stdout with a per-line size cap ([`MAX_LINE_BYTES`]).
struct LineReader<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    fn new(stream: R) -> Self {
        Self {
            reader: BufReader::new(stream),
            buf: Vec::new(),
        }
    }

    /// The next line without its `\n` (a final unterminated line counts), `None` at EOF.
    async fn next_line(&mut self) -> Option<String> {
        loop {
            self.buf.clear();
            let limit = (MAX_LINE_BYTES + 1) as u64;
            let read = (&mut self.reader)
                .take(limit)
                .read_until(b'\n', &mut self.buf)
                .await;
            match read {
                Ok(0) | Err(_) => return None,
                Ok(_) if self.buf.last() == Some(&b'\n') => {
                    self.buf.pop();
                    return Some(String::from_utf8_lossy(&self.buf).into_owned());
                }
                Ok(n) if n as u64 == limit => {
                    tracing::warn!("Skipping a claude output line over {MAX_LINE_BYTES} bytes");
                    if !self.skip_rest().await {
                        return None;
                    }
                }
                Ok(_) => return Some(String::from_utf8_lossy(&self.buf).into_owned()),
            }
        }
    }

    /// Discards up to the next `\n`; `false` at EOF.
    async fn skip_rest(&mut self) -> bool {
        loop {
            let available = match self.reader.fill_buf().await {
                Ok([]) | Err(_) => return false,
                Ok(bytes) => bytes,
            };
            if let Some(at) = available.iter().position(|b| *b == b'\n') {
                self.reader.consume(at + 1);
                return true;
            }
            let len = available.len();
            self.reader.consume(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_turn_command_quotes_every_argument() {
        let claude = ClaudeCodeSettings {
            command: "claude".into(),
            allowed_tools: vec!["Read".into(), "Bash(git log:*)".into()],
            disallowed_tools: vec!["WebFetch".into()],
            permission_mode: "acceptEdits".into(),
            max_turns: Some(30),
            api_key_env: None,
            extra_args: vec!["--setting-sources".into(), "project".into()],
        };
        let tools = vec!["mcp__symphony__github_api".to_owned()];
        let bridge = ToolBridge {
            config: "/tmp/s/mcp.json",
            tools: &tools,
        };
        assert_eq!(
            turn_command(
                &claude,
                Some("sonnet"),
                Some("Be brief."),
                Some("abc"),
                Some(bridge)
            ),
            "claude '-p' '--output-format' 'stream-json' '--verbose' '--model' 'sonnet' \
             '--permission-mode' 'acceptEdits' '--allowedTools' 'Read' 'Bash(git log:*)' \
             'mcp__symphony__github_api' '--disallowedTools' 'WebFetch' '--max-turns' '30' \
             '--append-system-prompt' 'Be brief.' '--mcp-config' '/tmp/s/mcp.json' \
             '--strict-mcp-config' '--resume' 'abc' '--setting-sources' 'project'"
        );
        let minimal = ClaudeCodeSettings::default();
        assert_eq!(
            turn_command(&minimal, None, None, None, None),
            "claude '-p' '--output-format' 'stream-json' '--verbose' '--permission-mode' 'acceptEdits'"
        );
    }

    #[tokio::test]
    async fn long_lines_are_skipped_and_short_ones_kept() {
        let mut data = vec![b'x'; MAX_LINE_BYTES + 10];
        data.extend_from_slice(b"\nnext\nlast");
        let mut reader = LineReader::new(&data[..]);
        assert_eq!(reader.next_line().await.as_deref(), Some("next"));
        assert_eq!(reader.next_line().await.as_deref(), Some("last"));
        assert_eq!(reader.next_line().await, None);
    }
}
