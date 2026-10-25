//! The API agent loop's tools: workspace files, a sandboxed shell and the tracker's scoped tools.
//!
//! | Tool | Arguments | Limits |
//! |---|---|---|
//! | `read_file` | `path`, `offset?` (1-based line), `limit?` (lines, default 2000) | 256 KiB |
//! | `write_file` | `path`, `content` | parents are created |
//! | `edit_file` | `path`, `old_string`, `new_string`, `replace_all?` | `old_string` must be unique unless `replace_all` |
//! | `list_files` | `path?`, `max_depth?` (default 3, at most 10) | 1000 entries, `.git` not descended |
//! | `shell` | `command`, `timeout_ms?` | `bash -lc` in the sandbox, last 64 KiB of output |
//! | `tracker` | the tracker tools' own arguments | as configured (scoped) |
//!
//! Paths are relative to the workspace. Absolute paths must lie inside it, `..` is refused, and
//! the nearest existing ancestor is canonicalized so symlinks cannot lead outside.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use symphony_codex::DynamicToolHandler;
use symphony_codex::dynamic_tool::normalize_tool_result;
use symphony_core::Issue;
use symphony_core::config::SandboxMode;
use tokio::io::AsyncReadExt;

use super::sandbox::{self, Confinement};
use super::wire::{ToolCall, ToolDef, ToolOutcome};
use crate::events::Emitter;
use crate::process::GroupChild;

/// Bytes `read_file` returns at most.
pub const MAX_READ_BYTES: usize = 256 * 1024;
/// Lines `read_file` returns by default.
pub const DEFAULT_READ_LINES: usize = 2000;
/// Entries `list_files` returns at most.
pub const MAX_LIST_ENTRIES: usize = 1000;
/// Trailing bytes of `shell` output kept.
pub const MAX_SHELL_OUTPUT: usize = 64 * 1024;
/// Largest file `read_file` reads and `edit_file` edits.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// How the `shell` tool runs commands.
#[derive(Debug, Clone)]
pub struct ShellPolicy {
    /// Confinement mode.
    pub sandbox: SandboxMode,
    /// Allow network access.
    pub network: bool,
    /// Upper bound of one command.
    pub timeout: Duration,
    /// Env vars removed from commands.
    pub secret_names: Vec<String>,
    /// Env vars added before the secrets are removed.
    pub extra_env: Vec<(String, String)>,
}

/// The tools of one session.
pub struct Toolbox {
    workspace: PathBuf,
    enabled: Vec<String>,
    handler: Arc<dyn DynamicToolHandler>,
    tracker_specs: Vec<Value>,
    shell: ShellPolicy,
}

impl std::fmt::Debug for Toolbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toolbox")
            .field("workspace", &self.workspace)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required,
        "additionalProperties": false})
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{key}` must be a string"))
}

fn usize_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

/// The last `max` bytes of `text`, cut on a char boundary.
fn tail(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (&text[start..], true)
}

/// The first `max` bytes of `text`, cut on a char boundary.
fn head(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

impl Toolbox {
    /// Tools for `workspace` (canonical): the enabled built-ins plus, with `tracker`, every tool
    /// `handler` advertises.
    pub fn new(
        workspace: PathBuf,
        enabled: Vec<String>,
        handler: Arc<dyn DynamicToolHandler>,
        shell: ShellPolicy,
    ) -> Self {
        let tracker_specs = if enabled.iter().any(|t| t == "tracker") {
            handler.tool_specs()
        } else {
            Vec::new()
        };
        Self {
            workspace,
            enabled,
            handler,
            tracker_specs,
            shell,
        }
    }

    fn enabled(&self, name: &str) -> bool {
        name != "tracker" && self.enabled.iter().any(|t| t == name)
    }

    fn is_tracker_tool(&self, name: &str) -> bool {
        self.tracker_specs
            .iter()
            .any(|spec| spec.get("name").and_then(Value::as_str) == Some(name))
    }

    /// Tool definitions for the model.
    pub fn definitions(&self) -> Vec<ToolDef> {
        let mut tools = Vec::new();
        let mut add = |name: &str, description: &str, schema: Value| {
            if self.enabled(name) {
                tools.push(ToolDef {
                    name: name.to_owned(),
                    description: description.to_owned(),
                    schema,
                });
            }
        };
        add(
            "read_file",
            "Read a text file in the workspace. Returns up to `limit` lines from line `offset`.",
            schema(
                json!({"path": {"type": "string", "description": "Path relative to the workspace."},
                    "offset": {"type": "integer", "minimum": 1, "description": "First line (1-based)."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Number of lines."}}),
                &["path"],
            ),
        );
        add(
            "write_file",
            "Create or overwrite a file in the workspace (parent directories are created).",
            schema(
                json!({"path": {"type": "string"}, "content": {"type": "string"}}),
                &["path", "content"],
            ),
        );
        add(
            "edit_file",
            "Replace `old_string` with `new_string` in a workspace file. `old_string` must occur \
             exactly once unless `replace_all` is true.",
            schema(
                json!({"path": {"type": "string"}, "old_string": {"type": "string"},
                    "new_string": {"type": "string"}, "replace_all": {"type": "boolean"}}),
                &["path", "old_string", "new_string"],
            ),
        );
        add(
            "list_files",
            "List files and directories (directories end with `/`) under a workspace path.",
            schema(
                json!({"path": {"type": "string", "description": "Directory; default the workspace."},
                    "max_depth": {"type": "integer", "minimum": 1, "maximum": 10}}),
                &[],
            ),
        );
        add(
            "shell",
            "Run a bash command in the workspace (sandboxed). Returns the exit code and output.",
            schema(
                json!({"command": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 1}}),
                &["command"],
            ),
        );
        for spec in &self.tracker_specs {
            let Some(name) = spec.get("name").and_then(Value::as_str) else {
                continue;
            };
            tools.push(ToolDef {
                name: name.to_owned(),
                description: spec
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned(),
                schema: spec
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            });
        }
        tools
    }

    /// Resolves a model-supplied path inside the workspace (see the module docs).
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
        let raw = raw.trim();
        let relative = if raw.is_empty() { "." } else { raw };
        let path = Path::new(relative);
        if path.components().any(|c| c == Component::ParentDir) {
            return Err(format!("`{raw}`: `..` is not allowed"));
        }
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };
        if !candidate.starts_with(&self.workspace) {
            return Err(format!("`{raw}` is outside the workspace"));
        }
        let mut existing = candidate.as_path();
        while existing.symlink_metadata().is_err() {
            existing = existing
                .parent()
                .ok_or_else(|| format!("`{raw}` is outside the workspace"))?;
        }
        let canonical = std::fs::canonicalize(existing)
            .map_err(|err| format!("`{raw}` cannot be resolved: {err}"))?;
        if !canonical.starts_with(&self.workspace) {
            return Err(format!(
                "`{raw}` resolves outside the workspace (through a symlink)"
            ));
        }
        Ok(candidate)
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .map(|p| p.to_string_lossy().into_owned())
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ".".into())
    }

    /// Runs one call, emitting its events.
    pub async fn run(&self, call: &ToolCall, issue: &Issue, emitter: &Emitter<'_>) -> ToolOutcome {
        let outcome = |content: String, is_error: bool| ToolOutcome {
            id: call.id.clone(),
            content,
            is_error,
        };
        let args = match &call.arguments {
            Ok(args) => args.clone(),
            Err(raw) => {
                let (excerpt, _) = head(raw, 200);
                emitter.tool_call(&call.id, &call.name, &json!({"raw": excerpt}), false);
                return outcome(
                    format!("The arguments of `{}` are not valid JSON.", call.name),
                    true,
                );
            }
        };
        if self.is_tracker_tool(&call.name) {
            let result = normalize_tool_result(
                self.handler
                    .execute(Some(&call.name), args.clone(), issue)
                    .await,
            );
            let success = result.get("success") == Some(&Value::Bool(true));
            emitter.tool_call(&call.id, &call.name, &args, success);
            let output = result
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            return outcome(output, !success);
        }
        if !self.enabled(&call.name) {
            emitter.tool_call(&call.id, &call.name, &args, false);
            return outcome(format!("Unknown tool `{}`.", call.name), true);
        }
        match call.name.as_str() {
            "shell" => self.shell(call, &args, emitter).await,
            "write_file" | "edit_file" => {
                let result = if call.name == "write_file" {
                    self.write_file(&args)
                } else {
                    self.edit_file(&args)
                };
                match result {
                    Ok((path, kind, message)) => {
                        emitter.file_changed(&call.id, &path, kind);
                        outcome(message, false)
                    }
                    Err(message) => {
                        emitter.tool_call(&call.id, &call.name, &args, false);
                        outcome(message, true)
                    }
                }
            }
            name => {
                let result = if name == "read_file" {
                    self.read_file(&args)
                } else {
                    self.list_files(&args)
                };
                emitter.tool_call(&call.id, name, &args, result.is_ok());
                match result {
                    Ok(text) => outcome(text, false),
                    Err(message) => outcome(message, true),
                }
            }
        }
    }

    /// Opens an existing or new regular file inside the workspace without following a symlink at
    /// the last component and without blocking (FIFOs, devices and directories are refused),
    /// then checks that the opened file is the one the canonical path names inside the
    /// workspace, so a path swapped between the checks cannot redirect the access.
    fn open_regular(&self, raw: &str, path: &Path, write: bool) -> Result<File, String> {
        // The canonical target (symlinks inside the workspace are fine), or for a new file the
        // canonical parent plus its name.
        let target = match std::fs::canonicalize(path) {
            Ok(target) => target,
            Err(_) if write => {
                let parent = path
                    .parent()
                    .and_then(|p| std::fs::canonicalize(p).ok())
                    .ok_or_else(|| format!("`{raw}` cannot be resolved"))?;
                let name = path
                    .file_name()
                    .ok_or_else(|| format!("`{raw}` has no file name"))?;
                parent.join(name)
            }
            Err(err) => return Err(format!("cannot open `{raw}`: {err}")),
        };
        if !target.starts_with(&self.workspace) {
            return Err(format!("`{raw}` resolves outside the workspace"));
        }
        let path = target.as_path();
        let mut options = std::fs::OpenOptions::new();
        options.read(!write).write(write).create(write);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .map_err(|err| format!("cannot open `{raw}`: {err}"))?;
        let meta = file
            .metadata()
            .map_err(|err| format!("cannot open `{raw}`: {err}"))?;
        if !meta.is_file() {
            return Err(format!("`{raw}` is not a regular file"));
        }
        let canonical = std::fs::canonicalize(path)
            .map_err(|err| format!("`{raw}` cannot be resolved: {err}"))?;
        if !canonical.starts_with(&self.workspace) {
            return Err(format!("`{raw}` resolves outside the workspace"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let named = std::fs::symlink_metadata(&canonical)
                .map_err(|err| format!("`{raw}` cannot be resolved: {err}"))?;
            if (named.dev(), named.ino()) != (meta.dev(), meta.ino()) {
                return Err(format!("`{raw}` changed while it was opened"));
            }
        }
        Ok(file)
    }

    /// Reads at most [`MAX_FILE_BYTES`] of a regular file; `true` when it was longer.
    fn read_capped(&self, raw: &str, path: &Path) -> Result<(Vec<u8>, bool), String> {
        let file = self.open_regular(raw, path, false)?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| format!("cannot read `{raw}`: {err}"))?;
        let longer = bytes.len() as u64 > MAX_FILE_BYTES;
        bytes.truncate(usize::try_from(MAX_FILE_BYTES).unwrap_or(usize::MAX));
        Ok((bytes, longer))
    }

    /// Replaces the contents of `path` (created if missing) after [`Self::open_regular`] checked it.
    fn write_contents(&self, raw: &str, path: &Path, content: &str) -> Result<(), String> {
        let mut file = self.open_regular(raw, path, true)?;
        file.set_len(0)
            .and_then(|()| file.write_all(content.as_bytes()))
            .map_err(|err| format!("cannot write `{raw}`: {err}"))
    }

    fn read_file(&self, args: &Value) -> Result<String, String> {
        let raw = str_arg(args, "path")?;
        let path = self.resolve(raw)?;
        let (bytes, longer) = self.read_capped(raw, &path)?;
        let text = String::from_utf8_lossy(&bytes);
        let offset = usize_arg(args, "offset").unwrap_or(1).max(1);
        let limit = usize_arg(args, "limit")
            .unwrap_or(DEFAULT_READ_LINES)
            .max(1);
        let total = text.lines().count();
        let selected: Vec<&str> = text.lines().skip(offset - 1).take(limit).collect();
        let joined = selected.join("\n");
        let (body, truncated) = head(&joined, MAX_READ_BYTES);
        let last = offset - 1 + selected.len();
        let mut out = body.to_owned();
        if longer {
            out.push_str("\n[the file is larger than 8 MiB; only its first 8 MiB were read]");
        }
        if truncated || last < total {
            out.push_str(&format!(
                "\n[showing lines {offset}-{last} of {total}{}; use offset/limit for more]",
                if truncated { ", cut at 256 KiB" } else { "" }
            ));
        }
        Ok(out)
    }

    fn write_file(&self, args: &Value) -> Result<(String, &'static str, String), String> {
        let raw = str_arg(args, "path")?;
        let content = str_arg(args, "content")?;
        let path = self.resolve(raw)?;
        if path.is_dir() {
            return Err(format!("`{raw}` is a directory"));
        }
        let existed = path.exists();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create the parent of `{raw}`: {err}"))?;
        }
        // The parent now exists; check again so a symlinked parent cannot redirect the write.
        let path = self.resolve(raw)?;
        self.write_contents(raw, &path, content)?;
        let kind = if existed { "update" } else { "add" };
        Ok((
            self.display(&path),
            kind,
            format!("Wrote {} bytes to {}.", content.len(), self.display(&path)),
        ))
    }

    fn edit_file(&self, args: &Value) -> Result<(String, &'static str, String), String> {
        let raw = str_arg(args, "path")?;
        let old = str_arg(args, "old_string")?;
        let new = str_arg(args, "new_string")?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old.is_empty() {
            return Err("`old_string` must not be empty".into());
        }
        let path = self.resolve(raw)?;
        let (bytes, longer) = self.read_capped(raw, &path)?;
        if longer {
            return Err(format!(
                "`{raw}` is larger than 8 MiB; edit it with the shell"
            ));
        }
        let text = String::from_utf8(bytes).map_err(|_| format!("`{raw}` is not UTF-8 text"))?;
        let count = text.matches(old).count();
        if count == 0 {
            return Err(format!("`old_string` was not found in `{raw}`"));
        }
        if count > 1 && !replace_all {
            return Err(format!(
                "`old_string` occurs {count} times in `{raw}`; add context or set replace_all"
            ));
        }
        let updated = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        self.write_contents(raw, &path, &updated)?;
        Ok((
            self.display(&path),
            "update",
            format!("Replaced {count} occurrence(s) in {}.", self.display(&path)),
        ))
    }

    fn list_files(&self, args: &Value) -> Result<String, String> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let root = self.resolve(raw)?;
        if !root.is_dir() {
            return Err(format!("`{raw}` is not a directory"));
        }
        let max_depth = usize_arg(args, "max_depth").unwrap_or(3).clamp(1, 10);
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut stack = vec![(root.clone(), 1usize)];
        while let Some((dir, depth)) = stack.pop() {
            let mut children: Vec<_> = match std::fs::read_dir(&dir) {
                Ok(read) => read.filter_map(Result::ok).collect(),
                Err(err) => return Err(format!("cannot list `{raw}`: {err}")),
            };
            children.sort_by_key(|e| e.file_name());
            for child in children.into_iter().rev() {
                let path = child.path();
                let is_dir = child.file_type().is_ok_and(|t| t.is_dir());
                if is_dir && depth < max_depth && child.file_name() != ".git" {
                    stack.push((path.clone(), depth + 1));
                }
                entries.push((self.display(&path), is_dir));
            }
        }
        entries.sort();
        if entries.len() > MAX_LIST_ENTRIES {
            entries.truncate(MAX_LIST_ENTRIES);
            truncated = true;
        }
        let mut out: Vec<String> = entries
            .into_iter()
            .map(|(path, is_dir)| if is_dir { format!("{path}/") } else { path })
            .collect();
        if out.is_empty() {
            out.push("(empty)".into());
        }
        if truncated {
            out.push(format!("[cut at {MAX_LIST_ENTRIES} entries]"));
        }
        Ok(out.join("\n"))
    }

    async fn shell(&self, call: &ToolCall, args: &Value, emitter: &Emitter<'_>) -> ToolOutcome {
        let outcome = |content: String, is_error: bool| ToolOutcome {
            id: call.id.clone(),
            content,
            is_error,
        };
        let command_text = match str_arg(args, "command") {
            Ok(command) => command.to_owned(),
            Err(message) => {
                emitter.tool_call(&call.id, "shell", args, false);
                return outcome(message, true);
            }
        };
        emitter.command_started(&call.id, &command_text);
        let (text, exit_code) = match self.exec(&command_text, args).await {
            Ok(result) => result,
            Err(message) => {
                emitter.command_completed(&call.id, &command_text, &message, None);
                return outcome(message, true);
            }
        };
        emitter.command_completed(&call.id, &command_text, &text, exit_code);
        let code = exit_code.map_or_else(|| "none (killed)".to_owned(), |c| c.to_string());
        outcome(format!("exit code: {code}\n{text}"), exit_code != Some(0))
    }

    /// Runs `script`; `Ok((output, exit code))`, `Err` when it could not run or timed out.
    async fn exec(&self, script: &str, args: &Value) -> Result<(String, Option<i32>), String> {
        let confinement = sandbox::resolve(self.shell.sandbox)?;
        let limit = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .map_or(self.shell.timeout, |t| t.min(self.shell.timeout));
        // A login shell sources profiles that could export the removed secrets again.
        let names =
            symphony_codex::launch::valid_secret_names(self.shell.secret_names.iter().cloned());
        let script = if names.is_empty() {
            script.to_owned()
        } else {
            format!("unset {}\n{script}", names.join(" "))
        };
        // A private temp dir per command: Symphony's own `$TMPDIR` (MCP relay configs, ...) stays
        // out of the sandbox's reach.
        let tmp = PrivateTmp::create().map_err(|err| format!("cannot create a temp dir: {err}"))?;
        let mut command = sandbox::command(
            &confinement,
            &self.workspace,
            &script,
            self.shell.network,
            Some(tmp.path()),
        );
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in &self.shell.extra_env {
            command.env(name, value);
        }
        for name in &self.shell.secret_names {
            command.env_remove(name);
        }
        let mut child =
            GroupChild::spawn(&mut command).map_err(|err| format!("cannot run bash: {err}"))?;
        let mut stdout = child.child_mut().stdout.take();
        let mut stderr = child.child_mut().stderr.take();
        let run = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            // Only the tail is kept, so a chatty command cannot fill Symphony's memory.
            let read_out = async {
                if let Some(pipe) = stdout.as_mut() {
                    read_tail(pipe, &mut out, MAX_SHELL_OUTPUT * 2).await;
                }
            };
            let read_err = async {
                if let Some(pipe) = stderr.as_mut() {
                    read_tail(pipe, &mut err, MAX_SHELL_OUTPUT).await;
                }
            };
            // The leader's exit kills its group (GroupChild::wait), which closes pipes that
            // background processes still held, so the reads finish too.
            let ((), (), status) = tokio::join!(read_out, read_err, child.wait());
            (out, err, status)
        };
        let finished = tokio::time::timeout(limit, run).await;
        let (out, err, status) = match finished {
            Ok(result) => result,
            Err(_) => {
                child.kill().await;
                return Err(format!(
                    "the command timed out after {}ms and was killed",
                    limit.as_millis()
                ));
            }
        };
        let status = status.map_err(|err| format!("waiting for bash failed: {err}"))?;
        let mut text = String::from_utf8_lossy(&out).into_owned();
        let wrapper = String::from_utf8_lossy(&err);
        if !wrapper.trim().is_empty() {
            if confinement != Confinement::None {
                text.push_str("\n[sandbox] ");
            }
            text.push_str(wrapper.trim_end());
        }
        let (kept, truncated) = tail(&text, MAX_SHELL_OUTPUT);
        let text = if truncated {
            format!("[output cut to the last 64 KiB]\n{kept}")
        } else {
            kept.to_owned()
        };
        Ok((text, status.code()))
    }
}

/// Reads `pipe` to the end, keeping only its last `max` bytes in `out`.
async fn read_tail<R: tokio::io::AsyncRead + Unpin>(pipe: &mut R, out: &mut Vec<u8>, max: usize) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > max * 2 {
                    let cut = out.len() - max;
                    out.drain(..cut);
                }
            }
        }
    }
}

/// A fresh 0700 temp directory, removed when dropped.
struct PrivateTmp(PathBuf);

impl PrivateTmp {
    fn create() -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::fs::canonicalize(std::env::temp_dir())?;
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = base.join(format!("symphony-shell-{}-{n}-{nanos}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateTmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_codex::NoDynamicTools;

    fn toolbox(dir: &Path, tools: &[&str]) -> Toolbox {
        Toolbox::new(
            std::fs::canonicalize(dir).unwrap(),
            tools.iter().map(|t| (*t).to_owned()).collect(),
            Arc::new(NoDynamicTools),
            ShellPolicy {
                sandbox: SandboxMode::None,
                network: false,
                timeout: Duration::from_secs(10),
                secret_names: Vec::new(),
                extra_env: Vec::new(),
            },
        )
    }

    #[test]
    fn paths_stay_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.join("link")).unwrap();
        let tools = toolbox(&ws, &["read_file"]);
        assert!(tools.resolve("src/new.rs").is_ok());
        assert!(tools.resolve(".").is_ok());
        assert!(tools.resolve("../x").unwrap_err().contains("`..`"));
        assert!(
            tools
                .resolve("/etc/passwd")
                .unwrap_err()
                .contains("outside")
        );
        assert!(tools.resolve("link/x").unwrap_err().contains("symlink"));
        let inside = std::fs::canonicalize(&ws).unwrap().join("a.txt");
        assert!(tools.resolve(&inside.to_string_lossy()).is_ok());
    }

    #[test]
    fn edits_need_a_unique_match_and_lists_are_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let tools = toolbox(dir.path(), &["edit_file", "list_files", "read_file"]);
        std::fs::write(dir.path().join("a.txt"), "x y x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "1\n2\n3").unwrap();
        let err = tools
            .edit_file(&json!({"path": "a.txt", "old_string": "x", "new_string": "z"}))
            .unwrap_err();
        assert!(err.contains("occurs 2 times"), "{err}");
        tools
            .edit_file(
                &json!({"path": "a.txt", "old_string": "x", "new_string": "z",
                "replace_all": true}),
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "z y z"
        );
        assert_eq!(
            tools.list_files(&json!({})).unwrap(),
            "a.txt\nsub/\nsub/b.txt"
        );
        let read = tools
            .read_file(&json!({"path": "sub/b.txt", "offset": 2, "limit": 1}))
            .unwrap();
        assert_eq!(
            read,
            "2\n[showing lines 2-2 of 3; use offset/limit for more]"
        );
    }

    #[test]
    fn only_regular_files_are_read_and_big_files_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let tools = toolbox(dir.path(), &["read_file", "write_file", "edit_file"]);
        // A FIFO would block a plain open() forever.
        let status = std::process::Command::new("mkfifo")
            .arg(dir.path().join("pipe"))
            .status()
            .unwrap();
        assert!(status.success());
        let err = tools.read_file(&json!({"path": "pipe"})).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        // Refused without blocking (ENXIO without a reader, or the regular-file check).
        assert!(
            tools
                .write_file(&json!({"path": "pipe", "content": "x"}))
                .is_err()
        );
        // A sparse 20 MiB file is not read whole.
        let big = File::create(dir.path().join("big.bin")).unwrap();
        big.set_len(20 * 1024 * 1024).unwrap();
        let out = tools.read_file(&json!({"path": "big.bin"})).unwrap();
        assert!(
            out.contains("larger than 8 MiB"),
            "{}",
            &out[out.len() - 120..]
        );
        let err = tools
            .edit_file(&json!({"path": "big.bin", "old_string": "a", "new_string": "b"}))
            .unwrap_err();
        assert!(err.contains("larger than 8 MiB"), "{err}");
        // Symlinks that stay inside the workspace keep working.
        std::fs::write(dir.path().join("real.txt"), "hello").unwrap();
        std::os::unix::fs::symlink("real.txt", dir.path().join("alias.txt")).unwrap();
        assert_eq!(
            tools.read_file(&json!({"path": "alias.txt"})).unwrap(),
            "hello"
        );
        tools
            .write_file(&json!({"path": "new/dir/file.txt", "content": "made"}))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new/dir/file.txt")).unwrap(),
            "made"
        );
    }

    #[tokio::test]
    async fn the_login_shell_cannot_bring_secrets_back_and_temp_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".bash_profile"),
            "export GITHUB_TOKEN=from-profile\n",
        )
        .unwrap();
        let mut tools = toolbox(dir.path(), &["shell"]);
        tools.shell.secret_names = vec!["GITHUB_TOKEN".into()];
        tools.shell.extra_env = vec![("HOME".into(), home.path().display().to_string())];
        let (out, code) = tools
            .exec("echo \"[${GITHUB_TOKEN:-}]\"; echo \"$TMPDIR\"", &json!({}))
            .await
            .unwrap();
        assert_eq!(code, Some(0), "{out}");
        assert!(out.contains("[]"), "{out}");
        assert!(out.contains("symphony-shell-"), "{out}");
    }

    #[tokio::test]
    async fn output_capture_keeps_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let tools = toolbox(dir.path(), &["shell"]);
        let (out, code) = tools
            .exec(
                "head -c 5000000 /dev/zero | tr '\\0' a; echo END",
                &json!({}),
            )
            .await
            .unwrap();
        assert_eq!(code, Some(0));
        assert!(out.len() < 2 * MAX_SHELL_OUTPUT, "{}", out.len());
        assert!(out.trim_end().ends_with("END"));
        assert!(out.starts_with("[output cut"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_blocks_launching_programs_outside_the_sandbox() {
        if sandbox::resolve(SandboxMode::Seatbelt).is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut tools = toolbox(dir.path(), &["shell"]);
        tools.shell.sandbox = SandboxMode::Seatbelt;
        let (out, code) = tools
            .exec("/usr/bin/osascript -e 'return 1'", &json!({}))
            .await
            .unwrap();
        assert_ne!(code, Some(0), "{out}");
        let tmp = std::env::temp_dir().join(format!("symphony-escape-{}", std::process::id()));
        let (_, code) = tools
            .exec(&format!("touch '{}'", tmp.display()), &json!({}))
            .await
            .unwrap();
        assert_ne!(
            code,
            Some(0),
            "Symphony's own TMPDIR is not writable from the sandbox"
        );
        assert!(!tmp.exists());
    }

    #[test]
    fn output_is_cut_on_char_boundaries() {
        let text = "é".repeat(10);
        let (kept, cut) = tail(&text, 5);
        assert!(cut);
        assert_eq!(kept, "éé");
        let (kept, cut) = head(&text, 3);
        assert!(cut);
        assert_eq!(kept, "é");
    }
}
