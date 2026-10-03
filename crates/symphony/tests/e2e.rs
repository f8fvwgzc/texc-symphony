//! End-to-end smoke test of the real binary: a memory-tracker workflow with one issue, a fake
//! `codex app-server` (the symphony-codex fixture scripts), an ephemeral HTTP port, with and
//! without the run-history database; then the API, SSE, the dashboard page and a SIGTERM stop.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

const ACK: &str = "--i-understand-that-this-will-be-running-without-the-usual-guardrails";

fn codex_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../symphony-codex/tests/fixtures/codex")
        .join(name)
}

fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn write_workflow(dir: &Path) -> PathBuf {
    let codex = format!(
        "sh {}",
        sh_quote(&codex_fixture("basic.sh").to_string_lossy())
    );
    let workflow = format!(
        r#"---
tracker:
  kind: memory
  provider:
    issues:
      - id: "issue-e2e-1"
        identifier: "E2E-1"
        title: "Smoke test issue"
        description: "Exercise the whole pipeline."
        state: "Todo"
        url: "https://example.org/issues/E2E-1"
polling:
  interval_ms: 500
workspace:
  root: {root}
agent:
  max_turns: 1
codex:
  command: {codex:?}
observability:
  dashboard_enabled: false
---
Work on {{{{ issue.identifier }}}}: {{{{ issue.title }}}}
"#,
        root = sh_quote(&dir.join("workspaces").to_string_lossy()),
    );
    let path = dir.join("WORKFLOW.md");
    std::fs::write(&path, workflow).unwrap();
    path
}

/// A running `symphony` process; killed on drop if the test failed half-way.
struct Symphony {
    child: Child,
    port: u16,
    stdout: mpsc::Receiver<String>,
}

impl Drop for Symphony {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(dir: &Path, extra: &[&str]) -> Symphony {
    let workflow = write_workflow(dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_symphony"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SYMPHONY_") {
            cmd.env_remove(name);
        }
    }
    cmd.current_dir(dir)
        .env("RUST_LOG", "info")
        .env("SYMP_TEST_CODEX_TRACE", dir.join("codex.trace"))
        .arg(ACK)
        .args(["--port", "0", "--host", "127.0.0.1"])
        .args(extra)
        .arg(&workflow)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let port = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = rx
            .recv_timeout(remaining)
            .expect("symphony never printed its listening URL");
        if let Some(rest) = line.strip_prefix("Symphony listening on http://127.0.0.1:") {
            break rest.trim_end_matches('/').parse::<u16>().unwrap();
        }
    };
    Symphony {
        child,
        port,
        stdout: rx,
    }
}

/// HTTP/1.0 (no chunked encoding, close-delimited bodies).
fn http(port: u16, method: &str, path: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_ascii_lowercase(), body.to_owned())
}

fn json(port: u16, method: &str, path: &str) -> (u16, Value) {
    let (status, _, body) = http(port, method, path);
    let value = serde_json::from_str(&body).unwrap_or_else(|err| panic!("{path}: {err}: {body}"));
    (status, value)
}

/// Reads the first SSE `snapshot` event.
fn first_snapshot_event(port: u16) -> Value {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(
        stream,
        "GET /api/v1/events HTTP/1.0\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    let mut in_snapshot = false;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).unwrap();
        assert!(read > 0, "event stream ended before a snapshot event");
        let line = line.trim_end_matches(['\r', '\n']);
        if head.len() < 4096 {
            head.push_str(line);
            head.push('\n');
        }
        if line == "event: snapshot" {
            in_snapshot = true;
        } else if in_snapshot && let Some(data) = line.strip_prefix("data:") {
            assert!(
                head.to_ascii_lowercase().contains("text/event-stream"),
                "{head}"
            );
            return serde_json::from_str(data.trim_start()).unwrap();
        }
    }
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

fn terminate(mut symphony: Symphony) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(symphony.child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = symphony.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "symphony did not stop after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        status.code(),
        Some(0),
        "exit status after SIGTERM: {status:?}"
    );
    // Drain whatever was printed after startup (keeps the reader thread from blocking).
    while symphony.stdout.try_recv().is_ok() {}
}

fn exercise_api(port: u16, dir: &Path) {
    let (status, health) = json(port, "GET", "/api/v1/health");
    assert_eq!(status, 200);
    assert_eq!(health["status"], "ok", "{health}");
    assert_eq!(
        health["version"].as_str(),
        Some(symphony_cli::version::version())
    );

    let (status, state) = json(port, "GET", "/api/v1/state");
    assert_eq!(status, 200);
    assert!(state["counts"].is_object(), "{state}");
    assert!(state["generated_at"].is_string());

    let snapshot = first_snapshot_event(port);
    assert!(snapshot["counts"].is_object(), "{snapshot}");

    let (status, ack) = json(port, "POST", "/api/v1/refresh");
    assert_eq!(status, 202);
    assert_eq!(ack["queued"], true);
    assert_eq!(ack["operations"], serde_json::json!(["poll", "reconcile"]));

    let (status, head, body) = http(port, "GET", "/");
    assert_eq!(status, 200);
    assert!(head.contains("content-type: text/html"), "{head}");
    assert!(body.to_ascii_lowercase().contains("<html"), "{body}");

    let (status, missing) = json(port, "GET", "/api/v1/NOPE-404");
    assert_eq!(status, 404);
    assert_eq!(missing["error"]["code"], "issue_not_found");

    // The fake Codex ran a full turn for the seeded issue.
    let trace = dir.join("codex.trace");
    wait_until("a codex turn for E2E-1", Duration::from_secs(60), || {
        std::fs::read_to_string(&trace)
            .is_ok_and(|t| t.contains("turn/start") && t.contains("E2E-1"))
    });
    assert!(dir.join("workspaces/E2E-1").is_dir());
}

#[test]
fn smoke_without_database() {
    let dir = tempfile::tempdir().unwrap();
    let symphony = start(dir.path(), &["--no-db"]);
    let port = symphony.port;
    exercise_api(port, dir.path());
    let (status, body) = json(port, "GET", "/api/v1/runs");
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "store_disabled");
    terminate(symphony);
    let log = std::fs::read_to_string(dir.path().join("log/symphony.log")).unwrap();
    assert!(log.contains("Starting Symphony version="), "{log}");
    assert!(log.contains("Symphony stopped"), "{log}");
    assert!(!dir.path().join("data").exists());
}

#[test]
fn smoke_with_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history/symphony.db");
    let db_arg = db.to_string_lossy().into_owned();
    let symphony = start(dir.path(), &["--db-path", &db_arg]);
    let port = symphony.port;
    exercise_api(port, dir.path());
    wait_until("a recorded run for E2E-1", Duration::from_secs(60), || {
        let (status, body) = json(port, "GET", "/api/v1/runs?issue=E2E-1");
        status == 200
            && body["runs"]
                .as_array()
                .is_some_and(|runs| runs.iter().any(|r| r["issue_identifier"] == "E2E-1"))
    });
    let (status, totals) = json(port, "GET", "/api/v1/totals");
    assert_eq!(status, 200, "{totals}");
    terminate(symphony);
    assert!(db.is_file());
}
