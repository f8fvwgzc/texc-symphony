//! The app-server child process: spawn, stdout line framing, stderr capture, deterministic shutdown.
//!
//! - stdout: newline-delimited frames, decoded as lossy UTF-8. A line longer than
//!   [`MAX_LINE_BYTES`] is skipped with a warning (Elixir concatenated 1 MiB `noeol` fragments
//!   without limit, so one runaway line could exhaust memory), and the lines waiting for the session
//!   are bounded by count and by bytes. A trailing fragment without `\n` at EOF is dropped, like the
//!   BEAM's final `noeol` chunk followed by `exit_status`.
//! - stderr: kept separate (SPEC; Elixir merged it into stdout), with the same line cap. Each line is
//!   logged with the same rules as non-JSON stdout lines and kept in a small tail buffer for
//!   diagnostics.
//! - The child runs in its own process group (`process_group(0)`) with `kill_on_drop(true)`; shutdown
//!   closes stdin, waits a grace period, then `SIGKILL`s the whole group (Elixir only closed the port).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};

use crate::error::CodexError;

/// Non-JSON stream lines are truncated to this many characters before logging.
pub const MAX_STREAM_LOG_CHARS: usize = 1_000;
/// Number of stderr lines kept for [`crate::AppServerSession::stderr_tail`].
pub const STDERR_TAIL_LINES: usize = 50;
/// Longest stdout or stderr line kept, without its `\n` (SPEC 10.1 recommends 10 MB); a longer
/// one is skipped.
pub const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;
/// Buffered stdout lines between the reader task and the session (backpressure beyond this).
const LINE_CHANNEL_CAPACITY: usize = 1_024;
/// Bytes of buffered stdout lines between the reader task and the session (backpressure beyond
/// this): [`LINE_CHANNEL_CAPACITY`] alone would let that many lines of [`MAX_LINE_BYTES`] wait.
const LINE_BUFFER_BYTES: usize = 4 * MAX_LINE_BYTES;
/// Longest decoded line: lossy UTF-8 decoding replaces each invalid byte with a 3-byte U+FFFD.
const MAX_DECODED_LINE_BYTES: usize = 3 * MAX_LINE_BYTES;
// A decoded line must fit the byte budget, or its reader would wait for room forever.
const _: () =
    assert!(MAX_DECODED_LINE_BYTES <= LINE_BUFFER_BYTES && LINE_BUFFER_BYTES <= u32::MAX as usize);
/// How long shutdown waits for the reader tasks to drain after the process group is gone.
const READER_DRAIN: Duration = Duration::from_secs(1);
/// Upper bound for reaping the child after `SIGKILL`.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

const LOG_KEYWORDS: [&str; 7] = [
    "error",
    "warn",
    "warning",
    "failed",
    "fatal",
    "panic",
    "exception",
];

/// Which wait a stream line arrived in (Elixir's log label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum StreamLabel {
    /// Awaiting a request's response.
    Response = 0,
    /// Inside a turn.
    Turn = 1,
}

impl StreamLabel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Response => "response stream",
            Self::Turn => "turn stream",
        }
    }

    fn from_u8(value: u8) -> Self {
        if value == Self::Turn as u8 {
            Self::Turn
        } else {
            Self::Response
        }
    }
}

/// `\b(error|warn|warning|failed|fatal|panic|exception)\b` (case-insensitive, ASCII word characters).
fn has_log_keyword(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| LOG_KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k)))
}

/// `log_non_json_stream_line/2`: trim, truncate to 1000 chars, skip empty; warn on error-ish keywords,
/// debug otherwise. Returns the logged text.
pub(crate) fn log_non_json_stream_line(line: &str, label: &str) -> Option<String> {
    let text: String = line.trim().chars().take(MAX_STREAM_LOG_CHARS).collect();
    if text.is_empty() {
        return None;
    }
    if has_log_keyword(&text) {
        tracing::warn!("Codex {label} output: {text}");
    } else {
        tracing::debug!("Codex {label} output: {text}");
    }
    Some(text)
}

/// One step of the incoming stream.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// A complete stdout line (without `\n`).
    Line(String),
    /// The process exited with this status (`128 + signal` for signal deaths).
    Exit(i32),
    /// Nothing arrived before the deadline.
    Timeout,
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// Newline framing over a byte stream with a per-line size cap.
struct FrameReader<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
    max_line_bytes: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Frames of at most [`MAX_LINE_BYTES`].
    fn new(stream: R) -> Self {
        Self::with_limit(stream, MAX_LINE_BYTES)
    }

    fn with_limit(stream: R, max_line_bytes: usize) -> Self {
        Self {
            reader: BufReader::new(stream),
            buf: Vec::new(),
            max_line_bytes,
        }
    }

    /// The next complete line without its `\n`, or `None` at EOF / on a read error. A line over
    /// the cap is skipped (its bytes are read and dropped, never buffered).
    async fn next_line(&mut self) -> Option<String> {
        loop {
            self.buf.clear();
            // The line and its `\n`; one byte more than that without a `\n` is over the cap.
            let limit = self.max_line_bytes as u64 + 1;
            let read = (&mut self.reader)
                .take(limit)
                .read_until(b'\n', &mut self.buf)
                .await;
            match read {
                Ok(0) => return None,
                Ok(_) if self.buf.last() == Some(&b'\n') => {
                    self.buf.pop();
                    return Some(String::from_utf8_lossy(&self.buf).into_owned());
                }
                Ok(read) if read as u64 == limit => {
                    tracing::warn!(
                        "Skipping a Codex output line over {} bytes",
                        self.max_line_bytes
                    );
                    // Do not keep a cap-sized buffer around for the rest of the session.
                    self.buf = Vec::new();
                    if !self.skip_rest_of_line().await {
                        return None;
                    }
                }
                Ok(_) => {
                    tracing::debug!(
                        bytes = self.buf.len(),
                        "Codex stream ended with an unterminated line; dropping it"
                    );
                    return None;
                }
                Err(err) => {
                    tracing::debug!("Codex stream read failed: {err}");
                    return None;
                }
            }
        }
    }

    /// Discards up to and including the next `\n`; `false` at EOF or on a read error.
    async fn skip_rest_of_line(&mut self) -> bool {
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

/// A stdout line on its way to the session, with its share of the byte budget (given back when
/// the session takes the line).
type BufferedLine = (String, OwnedSemaphorePermit);

/// Forwards complete lines to the session until EOF, or until the session is gone. Backpressure:
/// while the session is not reading (between turns), at most [`LINE_CHANNEL_CAPACITY`] lines and
/// `budget` bytes wait here; then the pipe fills up and the server blocks, instead of this process
/// buffering without bound.
async fn pump_lines<R: AsyncRead + Unpin>(
    mut frames: FrameReader<R>,
    lines: mpsc::Sender<BufferedLine>,
    budget: Arc<Semaphore>,
) {
    while let Some(line) = frames.next_line().await {
        // A decoded line is at most `MAX_DECODED_LINE_BYTES` long, which fits both `u32` and the
        // budget (checked at compile time above).
        let bytes = u32::try_from(line.len()).unwrap_or(u32::MAX);
        let Ok(permit) = Arc::clone(&budget).acquire_many_owned(bytes).await else {
            break;
        };
        if lines.send((line, permit)).await.is_err() {
            break;
        }
    }
}

/// The spawned app-server and its I/O tasks.
pub(crate) struct Transport {
    child: Child,
    pid: Option<u32>,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<BufferedLine>,
    stdout_closed: bool,
    exit_status: Option<i32>,
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    label: Arc<AtomicU8>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stopped: bool,
}

impl Transport {
    /// Spawns `command` with piped stdio in a new process group, applying `extra_env`, then removing
    /// `secret_names` from the child environment, then applying `task_env`.
    pub(crate) fn spawn(
        mut command: Command,
        secret_names: &[String],
        extra_env: &[(String, String)],
        task_env: &[(String, String)],
    ) -> Result<Self, CodexError> {
        for (name, value) in extra_env {
            command.env(name, value);
        }
        for name in secret_names {
            command.env_remove(name);
        }
        for (name, value) in task_env {
            command.env(name, value);
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|err| CodexError::SpawnFailed(err.to_string()))?;
        let pid = child.id();
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(CodexError::SpawnFailed("stdio pipes unavailable".into()));
        };

        let (tx, lines) = mpsc::channel(LINE_CHANNEL_CAPACITY);
        let stdout_task = tokio::spawn(pump_lines(
            FrameReader::new(stdout),
            tx,
            Arc::new(Semaphore::new(LINE_BUFFER_BYTES)),
        ));

        let label = Arc::new(AtomicU8::new(StreamLabel::Response as u8));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let stderr_task = {
            let label = Arc::clone(&label);
            let tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut frames = FrameReader::new(stderr);
                while let Some(line) = frames.next_line().await {
                    let current = StreamLabel::from_u8(label.load(Ordering::Relaxed));
                    if let Some(text) = log_non_json_stream_line(&line, current.as_str()) {
                        let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(text);
                    }
                }
            })
        };

        Ok(Self {
            child,
            pid,
            stdin: Some(stdin),
            lines,
            stdout_closed: false,
            exit_status: None,
            stdout_task: Some(stdout_task),
            stderr_task: Some(stderr_task),
            label,
            stderr_tail,
            stopped: false,
        })
    }

    /// OS pid of the spawned process.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Sets the log label used for stderr lines.
    pub(crate) fn set_label(&self, label: StreamLabel) {
        self.label.store(label as u8, Ordering::Relaxed);
    }

    /// The last stderr lines (trimmed, truncated).
    pub(crate) fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Writes one JSON message plus `\n`. Write failures (the process went away) are logged and the next
    /// read reports the exit.
    pub(crate) async fn send(&mut self, message: &Value) {
        let Some(stdin) = self.stdin.as_mut() else {
            tracing::debug!("Codex app-server stdin is closed; dropping outgoing message");
            return;
        };
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        let result = match stdin.write_all(&line).await {
            Ok(()) => stdin.flush().await,
            Err(err) => Err(err),
        };
        if let Err(err) = result {
            tracing::debug!("Codex app-server stdin write failed: {err}");
            self.stdin = None;
        }
    }

    /// Next line, exit or timeout. The deadline restarts on every call (a silence timeout).
    pub(crate) async fn next(&mut self, wait: Duration) -> Incoming {
        let deadline = Instant::now() + wait;
        if !self.stdout_closed {
            match timeout_at(deadline, self.lines.recv()).await {
                Err(_) => return Incoming::Timeout,
                // The line's share of the byte budget is given back here.
                Ok(Some((line, _budget))) => return Incoming::Line(line),
                Ok(None) => self.stdout_closed = true,
            }
        }
        if let Some(code) = self.exit_status {
            return Incoming::Exit(code);
        }
        match timeout_at(deadline, self.child.wait()).await {
            Err(_) => Incoming::Timeout,
            Ok(Ok(status)) => {
                let code = exit_code(status);
                self.exit_status = Some(code);
                Incoming::Exit(code)
            }
            Ok(Err(err)) => {
                tracing::warn!("Failed to wait for the Codex app-server process: {err}");
                self.exit_status = Some(-1);
                Incoming::Exit(-1)
            }
        }
    }

    #[cfg(unix)]
    fn kill_group(&self) {
        use rustix::process::{Pid, Signal, kill_process_group};
        let Some(pid) = self
            .pid
            .and_then(|p| i32::try_from(p).ok())
            .and_then(Pid::from_raw)
        else {
            return;
        };
        // ESRCH (group already gone) is expected after a clean exit.
        let _ = kill_process_group(pid, Signal::KILL);
    }

    #[cfg(not(unix))]
    fn kill_group(&self) {}

    /// Deterministic shutdown: close stdin (EOF), wait up to `grace` for the process to exit, `SIGKILL` the
    /// whole process group, reap the child and drain the reader tasks. Idempotent.
    pub(crate) async fn shutdown(&mut self, grace: Duration) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        drop(self.stdin.take());

        let exited = self.exit_status.is_some()
            || matches!(timeout(grace, self.child.wait()).await, Ok(Ok(_)));
        // Kill the group even after a clean exit: grandchildren left behind share the group.
        self.kill_group();
        if !exited {
            let _ = self.child.start_kill();
            if timeout(REAP_TIMEOUT, self.child.wait()).await.is_err() {
                tracing::warn!(pid = ?self.pid, "Codex app-server did not exit after SIGKILL");
            }
        }

        close_lines(&mut self.lines);
        for task in [self.stdout_task.take(), self.stderr_task.take()]
            .into_iter()
            .flatten()
        {
            let abort = task.abort_handle();
            if timeout(READER_DRAIN, task).await.is_err() {
                abort.abort();
            }
        }
    }
}

/// Closes the line channel and drops the lines still in it. Their share of the byte budget is
/// given back with them, so a reader waiting for room wakes up, finds the channel closed and ends
/// (a closed channel alone would leave it waiting on the budget).
fn close_lines(lines: &mut mpsc::Receiver<BufferedLine>) {
    lines.close();
    while lines.try_recv().is_ok() {}
}

impl Drop for Transport {
    fn drop(&mut self) {
        if !self.stopped {
            // `kill_on_drop` only signals the leader; take the whole group down too.
            self.kill_group();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_match_on_word_boundaries() {
        assert!(has_log_keyword("warning: this is stderr noise"));
        assert!(has_log_keyword("Something FAILED."));
        assert!(has_log_keyword("x_y panic"));
        assert!(!has_log_keyword("errors happen"));
        assert!(!has_log_keyword("warned_about"));
        assert!(!has_log_keyword("all good"));
    }

    /// One runaway line (no `\n` for megabytes) must not be buffered: it is skipped, and the
    /// lines around it still arrive.
    #[tokio::test]
    async fn a_line_over_the_cap_is_skipped_and_the_stream_goes_on() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"before\n");
        stream.extend_from_slice(b"12345678\n"); // exactly the cap: kept
        stream.extend_from_slice(&[b'x'; 9]); // one byte over
        stream.extend_from_slice(b"\n");
        stream.extend_from_slice(&vec![b'y'; 100_000]); // far over, across many reads
        stream.extend_from_slice(b"\nafter\nunterminated");
        let mut frames = FrameReader::with_limit(stream.as_slice(), 8);
        let mut lines = Vec::new();
        while let Some(line) = frames.next_line().await {
            lines.push(line);
        }
        assert_eq!(lines, ["before", "12345678", "after"]);
        assert!(
            frames.buf.capacity() <= 64,
            "the skipped line was buffered ({} bytes)",
            frames.buf.capacity()
        );

        // An over-long line that never ends: nothing is returned and nothing is kept.
        let endless = vec![b'z'; 100_000];
        let mut frames = FrameReader::with_limit(endless.as_slice(), 8);
        assert_eq!(frames.next_line().await, None);
        assert!(frames.buf.capacity() <= 64);
    }

    /// Lines the session has not taken yet are bounded in bytes, not only in number: the reader
    /// waits for room instead of buffering more.
    #[tokio::test]
    async fn waiting_lines_are_bounded_by_the_byte_budget() {
        let stream = b"aaaaaaaaaa\nbbbbbbbbbb\ncc\n".to_vec();
        let (tx, mut rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
        let budget = Arc::new(Semaphore::new(16));
        let pump = tokio::spawn(pump_lines(
            FrameReader::new(std::io::Cursor::new(stream)),
            tx,
            Arc::clone(&budget),
        ));
        // The first line (10 of 16 bytes) is through; the second (10 more) has to wait.
        let (first, first_share) = rx.recv().await.unwrap();
        assert_eq!(first, "aaaaaaaaaa");
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(rx.try_recv().is_err(), "the second line did not wait");
        // The session takes the first line: its bytes are free again and the rest follows.
        drop(first_share);
        let (second, second_share) = rx.recv().await.unwrap();
        assert_eq!(second, "bbbbbbbbbb");
        drop(second_share);
        assert_eq!(rx.recv().await.unwrap().0, "cc");
        assert!(rx.recv().await.is_none());
        pump.await.unwrap();
        assert_eq!(budget.available_permits(), 16);
    }

    /// Shutdown must not wait for a reader that is waiting for room in the byte budget.
    #[tokio::test]
    async fn closing_the_lines_ends_a_reader_that_waits_for_room() {
        let stream = b"aaaaaaaaaa\nbbbbbbbbbb\n".to_vec();
        let (tx, mut rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
        let pump = tokio::spawn(pump_lines(
            FrameReader::new(std::io::Cursor::new(stream)),
            tx,
            Arc::new(Semaphore::new(16)),
        ));
        // The first line is buffered (10 of 16 bytes); the reader waits for room for the second.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(!pump.is_finished());
        close_lines(&mut rx);
        timeout(Duration::from_secs(5), pump)
            .await
            .expect("the reader ended")
            .unwrap();
    }

    #[test]
    fn stream_lines_are_trimmed_truncated_and_skipped_when_blank() {
        assert_eq!(log_non_json_stream_line("   ", "turn stream"), None);
        let long = "a".repeat(2_000);
        assert_eq!(
            log_non_json_stream_line(&format!("  {long}  "), "turn stream").map(|t| t.len()),
            Some(MAX_STREAM_LOG_CHARS)
        );
    }
}
