//! A size-rotating log file (Elixir `LogFile`: `:logger_disk_log_h` wrap log, 10 MiB × 5).
//!
//! `symphony.log` is the active file. When a write would push it past `max_bytes`, it becomes
//! `symphony.log.1`, the older generations shift up (`.1` → `.2`, ...) and the oldest beyond
//! `max_files` is deleted. Rotation happens between writes, and `tracing`'s fmt layer writes one
//! whole event per call, so lines are never split across files. (`tracing-appender` only rotates by
//! time, hence this small writer.)

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

/// Default rotation size (Elixir `@default_max_bytes`).
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Default number of rotated files kept (Elixir `@default_max_files`).
pub const DEFAULT_MAX_FILES: usize = 5;
/// Log file location under the logs root (Elixir `@default_log_relative_path`).
pub const LOG_RELATIVE_PATH: &str = "log/symphony.log";

/// `LogFile.default_log_file/1`: `<root>/log/symphony.log` (`log/` is always appended).
pub fn default_log_file(root: &Path) -> PathBuf {
    root.join(LOG_RELATIVE_PATH)
}

#[derive(Debug)]
struct State {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max_bytes: u64,
    max_files: usize,
}

/// A thread-safe, size-rotating append-only file. Cheap to clone.
#[derive(Debug, Clone)]
pub struct RotatingFile {
    state: Arc<Mutex<State>>,
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl RotatingFile {
    /// Opens (creating the parent directories) `path` for appending.
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = open_append(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                path,
                file: Some(file),
                size,
                max_bytes: max_bytes.max(1),
                max_files,
            })),
        })
    }

    /// The active file's path.
    pub fn path(&self) -> PathBuf {
        self.lock().path.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic mid-write leaves at worst a partial line; the state itself stays consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends `buf`, rotating first when it would not fit.
    pub fn append(&self, buf: &[u8]) -> io::Result<()> {
        let mut state = self.lock();
        let len = buf.len() as u64;
        if state.size > 0 && state.size.saturating_add(len) > state.max_bytes {
            state.rotate()?;
        }
        let file = match state.file.as_mut() {
            Some(file) => file,
            None => {
                let file = open_append(&state.path)?;
                state.size = file.metadata()?.len();
                state.file.insert(file)
            }
        };
        file.write_all(buf)?;
        state.size = state.size.saturating_add(len);
        Ok(())
    }

    /// Flushes the active file.
    pub fn flush(&self) -> io::Result<()> {
        match self.lock().file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl State {
    fn generation(&self, n: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        if self.max_files == 0 {
            remove_if_exists(&self.path)?;
        } else {
            remove_if_exists(&self.generation(self.max_files))?;
            for n in (1..self.max_files).rev() {
                rename_if_exists(&self.generation(n), &self.generation(n + 1))?;
            }
            rename_if_exists(&self.path, &self.generation(1))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.file = Some(file);
        self.size = 0;
        Ok(())
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Per-event writer handed out by [`RotatingFile`]'s [`MakeWriter`] impl.
#[derive(Debug)]
pub struct RotatingWriter(RotatingFile);

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.append(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = RotatingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingWriter(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_log_file_appends_log_dir() {
        assert_eq!(
            default_log_file(Path::new("/tmp/symphony-logs")),
            PathBuf::from("/tmp/symphony-logs/log/symphony.log")
        );
    }

    #[test]
    fn creates_parent_directories_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/log/symphony.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old\n").unwrap();
        let file = RotatingFile::open(&path, 1024, 5).unwrap();
        file.append(b"new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\nnew\n");
        assert_eq!(file.path(), path);
    }

    #[test]
    fn rotates_by_size_and_keeps_max_files_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log/symphony.log");
        let file = RotatingFile::open(&path, 10, 2).unwrap();
        for line in [
            "aaaa\n", "bbbb\n", "cccc\n", "dddd\n", "eeee\n", "ffff\n", "gggg\n",
        ] {
            file.append(line.as_bytes()).unwrap();
        }
        let read = |p: &Path| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(&path), "gggg\n");
        assert_eq!(read(&dir.path().join("log/symphony.log.1")), "eeee\nffff\n");
        assert_eq!(read(&dir.path().join("log/symphony.log.2")), "cccc\ndddd\n");
        assert!(!dir.path().join("log/symphony.log.3").exists());
    }

    #[test]
    fn an_oversized_write_still_lands_in_a_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symphony.log");
        let file = RotatingFile::open(&path, 4, 1).unwrap();
        file.append(b"ab").unwrap();
        file.append(b"0123456789").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0123456789");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("symphony.log.1")).unwrap(),
            "ab"
        );
    }

    #[test]
    fn works_as_a_tracing_writer() {
        use tracing_subscriber::layer::SubscriberExt;
        let dir = tempfile::tempdir().unwrap();
        let file = RotatingFile::open(
            dir.path().join("t.log"),
            DEFAULT_MAX_BYTES,
            DEFAULT_MAX_FILES,
        )
        .unwrap();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file.clone()),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(issue_id = "i-1", "Dispatching issue_identifier=MT-1");
        });
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("INFO"), "{text}");
        assert!(
            text.contains("Dispatching issue_identifier=MT-1 issue_id=\"i-1\""),
            "{text}"
        );
    }
}
