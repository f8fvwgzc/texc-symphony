//! Log sinks (Elixir `LogFile.configure/0`, blueprint A.7 / E.6).
//!
//! * Always: the size-rotating file `<logs_root>/log/symphony.log` (10 MiB × 5), single-line text
//!   without ANSI colours. Context travels as `key=value` pairs (`issue_id`, `issue_identifier`,
//!   `session_id`), so the `debug` skill's grep patterns keep working.
//! * Additionally, **stdout** when it is not a terminal or the terminal dashboard is off
//!   (improvement: Elixir removed the console handler unconditionally, so `docker logs` and
//!   journald saw nothing). `SYMPHONY_LOG_FORMAT=json` switches that stream to JSON lines.
//!
//! The level filter comes from `RUST_LOG` (default `info`). If the file cannot be opened, a warning
//! goes to stderr and logging falls back to stderr (Elixir kept the console handler too).

use std::path::Path;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use crate::rotating::{DEFAULT_MAX_BYTES, DEFAULT_MAX_FILES, RotatingFile, default_log_file};

/// Environment variable selecting the stdout log format (`text` or `json`).
pub const ENV_LOG_FORMAT: &str = "SYMPHONY_LOG_FORMAT";

/// Format of the stdout log stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleFormat {
    /// Human-readable single lines (default).
    Text,
    /// One JSON object per line.
    Json,
}

impl ConsoleFormat {
    /// Parses `SYMPHONY_LOG_FORMAT` (`text`/`plain`/empty or `json`, case-insensitive).
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "text" | "plain") => Ok(Self::Text),
            Some("json") => Ok(Self::Json),
            Some(_) => Err(format!(
                "Invalid {ENV_LOG_FORMAT}={:?}: expected text or json",
                value.unwrap_or_default()
            )),
        }
    }
}

/// What [`init`] installed.
#[derive(Debug, Clone)]
pub struct Logging {
    /// The rotating log file, `None` when it could not be opened.
    pub file: Option<RotatingFile>,
    /// Whether logs are also written to stdout/stderr.
    pub console: bool,
}

impl Logging {
    /// Flushes the log file (on shutdown).
    pub fn flush(&self) {
        if let Some(file) = &self.file {
            let _ = file.flush();
        }
    }
}

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Builds the sinks: the rotating file under `logs_root` plus, when `console` is `Some`, a stdout
/// stream in that format (stderr text when the file cannot be opened).
fn build(logs_root: &Path, console: Option<ConsoleFormat>) -> (Logging, Vec<BoxedLayer>) {
    let path = default_log_file(logs_root);
    let mut layers: Vec<BoxedLayer> = Vec::new();
    let file = match RotatingFile::open(&path, DEFAULT_MAX_BYTES, DEFAULT_MAX_FILES) {
        Ok(file) => {
            layers.push(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(file.clone())
                    .boxed(),
            );
            Some(file)
        }
        Err(err) => {
            eprintln!(
                "Failed to configure rotating log file handler: {}: {err}",
                path.display()
            );
            None
        }
    };
    let console = match (console, &file) {
        (Some(format), _) => Some((format, false)),
        // The file sink failed: keep a console sink (stderr) like Elixir kept its default handler.
        (None, None) => Some((ConsoleFormat::Text, true)),
        (None, Some(_)) => None,
    };
    if let Some((format, stderr)) = console {
        let layer = tracing_subscriber::fmt::layer().with_ansi(false);
        layers.push(match (format, stderr) {
            (ConsoleFormat::Text, false) => layer.with_writer(std::io::stdout).boxed(),
            (ConsoleFormat::Text, true) => layer.with_writer(std::io::stderr).boxed(),
            (ConsoleFormat::Json, false) => layer.json().with_writer(std::io::stdout).boxed(),
            (ConsoleFormat::Json, true) => layer.json().with_writer(std::io::stderr).boxed(),
        });
    }
    let logging = Logging {
        file,
        console: console.is_some(),
    };
    (logging, layers)
}

/// Installs the global subscriber (see [`build`]) with the `RUST_LOG` filter (default `info`).
pub fn init(logs_root: &Path, console: Option<ConsoleFormat>) -> Logging {
    let (logging, layers) = build(logs_root, console);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` fails only when a subscriber is already installed; keep that one.
    let _ = tracing_subscriber::registry()
        .with(layers)
        .with(filter)
        .try_init();
    logging
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_format_parsing() {
        assert_eq!(ConsoleFormat::parse(None), Ok(ConsoleFormat::Text));
        assert_eq!(ConsoleFormat::parse(Some(" ")), Ok(ConsoleFormat::Text));
        assert_eq!(ConsoleFormat::parse(Some("Plain")), Ok(ConsoleFormat::Text));
        assert_eq!(ConsoleFormat::parse(Some("JSON")), Ok(ConsoleFormat::Json));
        assert_eq!(
            ConsoleFormat::parse(Some("xml")),
            Err("Invalid SYMPHONY_LOG_FORMAT=\"xml\": expected text or json".to_owned())
        );
    }

    #[test]
    fn build_creates_the_rotating_log_file_under_the_logs_root() {
        let dir = tempfile::tempdir().unwrap();
        let (logging, layers) = build(dir.path(), None);
        assert_eq!(layers.len(), 1);
        assert!(!logging.console);
        let file = logging.file.expect("file sink");
        assert_eq!(file.path(), dir.path().join("log/symphony.log"));
        assert!(file.path().exists());
    }

    #[test]
    fn build_falls_back_to_the_console_when_the_file_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        // `log` is a regular file, so `log/symphony.log` cannot be created.
        std::fs::write(dir.path().join("log"), "not a directory").unwrap();
        let (logging, layers) = build(dir.path(), None);
        assert_eq!(layers.len(), 1);
        assert!(logging.file.is_none());
        assert!(logging.console);
    }
}

#[cfg(test)]
mod console_tests {
    use super::*;

    #[test]
    fn console_layer_is_added_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        let (logging, layers) = build(dir.path(), Some(ConsoleFormat::Json));
        assert!(logging.console);
        assert!(logging.file.is_some());
        assert_eq!(layers.len(), 2);
    }
}
