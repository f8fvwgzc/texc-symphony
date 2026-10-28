//! Task isolation in the agent runner: refused runs (also a default-deny network that cannot be
//! enforced), and an isolated task's environment, token and redacted events, driven by a fake
//! app-server.

#![cfg(unix)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use support::{TestWorkflow, read, sh_quote};
use symphony_codex::{CodexEvent, EventSink};
use symphony_core::Issue;
use symphony_runtime::{AgentRunner, RunError, RunnerOptions, TrackerClient, WorkerContext};
use symphony_trackers::{MemoryIssues, MemoryTracker, TrackerDeps};
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "ghs_IsolatedTaskToken0123456789abcdefghij";

/// A fake app-server that records its environment and prints the task token in a command's
/// output (which must reach the orchestrator redacted).
const ENV_FIXTURE: &str = r#"#!/bin/sh
trace="$SYMP_TEST_CODEX_TRACE"
{
  printf 'HOME=%s\n' "$HOME"
  printf 'GH_TOKEN=%s\n' "$GH_TOKEN"
  printf 'SYMPHONY_GIT_TOKEN=%s\n' "$SYMPHONY_GIT_TOKEN"
  printf 'HELPER_KEY=%s\n' "$GIT_CONFIG_KEY_1"
  printf 'HTTPS_PROXY=%s\n' "$HTTPS_PROXY"
} >> "$trace"
count=0
while IFS= read -r _line; do
  printf 'IN=%s\n' "$_line" >> "$trace"
  count=$((count + 1))
  case "$count" in
    1) printf '%s\n' '{"id":1,"result":{}}' ;;
    2) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-iso"}}}' ;;
    3) printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-iso"}}}' ;;
    4)
      printf '{"method":"item/completed","params":{"item":{"type":"commandExecution","aggregatedOutput":"token %s"}}}\n' "$GH_TOKEN"
      printf '%s\n' '{"method":"turn/completed"}'
      exit 0 ;;
    *) exit 0 ;;
  esac
done
"#;

struct Setup {
    wf: TestWorkflow,
    runner: AgentRunner,
    trace: PathBuf,
}

fn setup(isolation: Value) -> Setup {
    setup_with(isolation, RunnerOptions::default().symphony_executable)
}

/// `symphony`: the executable whose `egress-shim` the network jail would run.
fn setup_with(isolation: Value, symphony: Option<PathBuf>) -> Setup {
    setup_tracker(
        isolation,
        symphony,
        Arc::new(MemoryTracker::new(MemoryIssues::new())),
    )
}

fn setup_tracker(
    isolation: Value,
    symphony: Option<PathBuf>,
    tracker: Arc<dyn symphony_trackers::Tracker>,
) -> Setup {
    let wf = TestWorkflow::new(json!({}));
    let fixture = wf.path("env_codex.sh");
    fs::write(&fixture, ENV_FIXTURE).unwrap();
    wf.rewrite(json!({
        "codex": {"command": format!("sh {} app-server", sh_quote(&fixture.to_string_lossy()))},
        "isolation": isolation,
    }));
    let operator_home = wf.path("operator-home");
    fs::create_dir_all(&operator_home).unwrap();
    let trace = wf.path("codex.trace");
    let client = TrackerClient::new(TrackerDeps::new().unwrap()).with_tracker(tracker);
    let runner = AgentRunner::new(Arc::clone(&wf.store), client, Default::default()).with_options(
        RunnerOptions {
            codex_env: vec![
                ("HOME".into(), operator_home.to_string_lossy().into_owned()),
                ("GH_TOKEN".into(), "operator-gh-token-must-not-leak".into()),
                (
                    "SYMP_TEST_CODEX_TRACE".into(),
                    trace.to_string_lossy().into_owned(),
                ),
            ],
            codex_stop_grace: Duration::from_millis(500),
            symphony_executable: symphony,
            ..RunnerOptions::default()
        },
    );
    Setup { wf, runner, trace }
}

fn task_context(repo: Option<&str>) -> (WorkerContext, mpsc::UnboundedReceiver<CodexEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut ctx = WorkerContext::standalone(Issue {
        id: Some("task:7".into()),
        identifier: Some("task-7".into()),
        title: Some("A task".into()),
        state: Some("task".into()),
        ..Issue::default()
    });
    ctx.events = EventSink::new(tx);
    ctx.task = Some(symphony_runtime::TaskSpec {
        id: 7,
        prompt: "do it".into(),
        repo: repo.map(str::to_owned),
        ..Default::default()
    });
    (ctx, rx)
}

/// `<root>/.tasks/7`: a task's workspace (never a tracker identifier's).
fn workspace(s: &Setup) -> PathBuf {
    s.wf.workspace_root()
        .join(symphony_runtime::workspace::TASK_WORKSPACES)
        .join("7")
}

fn trace_value(trace: &str, key: &str) -> String {
    trace
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("{key} missing from {trace}"))
        .to_owned()
}

#[tokio::test]
async fn task_uids_without_root_fail_before_anything_runs() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: running as root, the uid switch is available");
        return;
    }
    let s = setup(json!({"task_uids": {"first": 200000, "count": 100}}));
    let (ctx, _rx) = task_context(None);
    let err = s.runner.run(ctx).await.unwrap_err();
    let RunError::IsolationUnavailable(message) = &err else {
        panic!("expected isolation_unavailable, got {err:?}");
    };
    assert!(
        message.contains("needs Symphony to run as root"),
        "{message}"
    );
    assert!(
        err.to_string().starts_with("isolation_unavailable: "),
        "{err}"
    );
    assert!(!workspace(&s).exists(), "no workspace was created");
    assert_eq!(read(&s.trace), "", "the agent never started");
}

#[tokio::test]
async fn a_repo_on_a_host_the_operator_did_not_allow_is_refused() {
    // deny mode, no git_hosts, no github_app: the submitter cannot pick a host to reach.
    let s = setup(json!({"network": {"mode": "deny", "allow": ["pypi.org:443"]}}));
    fs::create_dir_all(workspace(&s)).unwrap();
    let (ctx, _rx) = task_context(Some("https://evil.example/a/b"));
    let err = s.runner.run(ctx).await.unwrap_err();
    assert!(
        matches!(&err, RunError::IsolationUnavailable(m)
            if m.contains("evil.example") && m.contains("git_hosts")),
        "{err:?}"
    );
    assert!(read(&s.trace).is_empty(), "the agent never started");
}

#[tokio::test]
async fn isolation_is_refused_on_ssh_workers() {
    let s = setup(json!({}));
    let (mut ctx, _rx) = task_context(None);
    ctx.worker_host = Some("worker-a".into());
    let err = s.runner.run(ctx).await.unwrap_err();
    assert!(
        matches!(&err, RunError::IsolationUnavailable(m) if m.contains("SSH workers")),
        "{err:?}"
    );
}

#[tokio::test]
async fn tracker_issues_are_not_isolated() {
    let s = setup(json!({"network": {"mode": "deny"}}));
    let mut ctx = WorkerContext::standalone(support::issue("issue-1", "MT-1", "In Progress"));
    ctx.events = EventSink::none();
    let runner = s.runner.clone().with_options(RunnerOptions {
        max_turns: Some(1),
        codex_env: vec![
            (
                "HOME".into(),
                s.wf.path("operator-home").display().to_string(),
            ),
            (
                "SYMP_TEST_CODEX_TRACE".into(),
                s.trace.display().to_string(),
            ),
        ],
        ..RunnerOptions::default()
    });
    let _ = runner.run(ctx).await;
    let trace = read(&s.trace);
    assert_eq!(trace_value(&trace, "HTTPS_PROXY"), "", "{trace}");
    assert!(
        trace_value(&trace, "HOME").ends_with("operator-home"),
        "{trace}"
    );
}

fn generate_key(dir: &Path) -> Option<PathBuf> {
    let key = dir.join("app.pem");
    let ok = Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&key)
        .arg("2048")
        .output()
        .is_ok_and(|o| o.status.success());
    ok.then_some(key)
}

/// Wiremock for the App's installation lookup and token, called `calls` times.
async fn app_api(calls: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 31})))
        .expect(calls)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/31/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"token": TOKEN})))
        .expect(calls)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn an_app_key_the_task_users_could_read_is_refused_before_a_token_is_minted() {
    use std::os::unix::fs::PermissionsExt;
    let keys = tempfile::tempdir().unwrap();
    let key = keys.path().join("app.pem");
    fs::write(&key, "not used").unwrap();
    let server = app_api(0).await;
    // A 0644 key (a Kubernetes Secret's default mode) is readable by the task uids; it is
    // refused before the root check, so any user can run this test.
    for mode in [0o644, 0o640, 0o604] {
        fs::set_permissions(&key, fs::Permissions::from_mode(mode)).unwrap();
        let s = setup(json!({
            "github_app": {"app_id": 99, "private_key_file": key.to_string_lossy(), "api_url": server.uri()},
            "task_uids": {"first": 200000, "count": 10},
            "network": {"mode": "allow"},
        }));
        let (ctx, _rx) = task_context(Some("https://github.com/acme/widgets"));
        let err = s.runner.run(ctx).await.unwrap_err();
        assert!(
            matches!(&err, RunError::IsolationUnavailable(m) if m.contains("must not be readable by group or others")),
            "{mode:o}: {err:?}"
        );
        assert!(read(&s.trace).is_empty(), "the agent never started");
    }
}

#[tokio::test]
async fn an_isolated_task_gets_its_own_home_and_a_redacted_token() {
    use symphony_runtime::isolation::TaskIsolation;
    let keys = tempfile::tempdir().unwrap();
    let Some(key) = generate_key(keys.path()) else {
        eprintln!("skipped: openssl is not available");
        return;
    };
    let server = app_api(1).await;
    let isolation = symphony_core::config::IsolationSettings {
        task_uids: None,
        github_app: Some(symphony_core::config::GitHubAppSettings {
            app_id: "99".into(),
            private_key_file: key,
            api_url: server.uri(),
            git_host: "github.com".into(),
        }),
        network: allow_all(),
    };
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("7");
    fs::create_dir(&ws).unwrap();
    let mut spec = task(7);
    spec.repo = Some("https://github.com/acme/widgets".into());
    let prepared = TaskIsolation::prepare(&Default::default(), &isolation, &spec, &ws, None)
        .await
        .unwrap();
    let env: std::collections::HashMap<String, String> = prepared.task_env().into_iter().collect();
    assert_eq!(env["HOME"], ws.join(".home").display().to_string());
    assert_eq!(
        env["GH_TOKEN"], TOKEN,
        "the task's token, not the operator's"
    );
    assert_eq!(env["SYMPHONY_GIT_TOKEN"], TOKEN);
    assert_eq!(
        env["GIT_CONFIG_KEY_1"],
        "credential.https://github.com.helper"
    );

    // The token never leaves the run in an event or its error.
    let redactor = prepared.redactor().unwrap();
    assert_eq!(
        redactor.redact_str(&format!("token {TOKEN}")),
        "token [REDACTED]"
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = prepared.redacted_events(&EventSink::new(tx));
    events.emit(CodexEvent::new(
        symphony_codex::CodexEventData::TurnEndedWithError {
            session_id: "s".into(),
            reason: symphony_codex::CodexError::Backend(format!("push failed: {TOKEN}")),
        },
        None,
        None,
    ));
    let json = rx.try_recv().unwrap().to_json().to_string();
    assert!(!json.contains(TOKEN), "{json}");
    assert!(json.contains("[REDACTED]"), "{json}");
}

#[tokio::test]
async fn a_task_whose_network_cannot_be_confined_does_not_start_its_agent() {
    // No `symphony` executable for the shim (and, off Linux, no bubblewrap): proxy variables
    // alone are no boundary, so the run fails before the app-server starts.
    let s = setup_with(json!({"network": {"allow": ["pypi.org:443"]}}), None);
    fs::create_dir_all(workspace(&s)).unwrap();
    let (ctx, _rx) = task_context(None);
    let err = s.runner.run(ctx).await.unwrap_err().to_string();
    assert!(
        err.contains("bubblewrap") || err.contains("egress shim"),
        "{err}"
    );
    assert!(read(&s.trace).is_empty(), "the app-server never started");
}

/// Isolation without `task_uids` runs the task as Symphony's own uid, so it needs no extra
/// traversal of `<root>`/`.tasks` and must not chmod them (an unconditional chmod EPERM'd on a
/// root-owned chart volume Symphony does not own). The run completes and the workspace stays
/// private. The traverse-bit logic itself is unit-tested in `workspace::tests`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_isolated_task_without_task_uids_leaves_provisioned_parents_untouched() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup(json!({"network": {"mode": "allow"}}));
    // Provision the root 0700, as a restrictive operator would; the run must not change it.
    let root = s.wf.workspace_root();
    fs::create_dir_all(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let (ctx, _rx) = task_context(None);
    s.runner.run(ctx).await.unwrap();
    let root_mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
    assert_eq!(root_mode, 0o700, "the provisioned root is left untouched");
    // The task's own workspace stays private.
    let ws_mode = fs::metadata(workspace(&s)).unwrap().permissions().mode() & 0o777;
    assert_eq!(ws_mode, 0o700, "the task workspace is private");
}

/// Fix 3: the runner seals a task workspace before the uid lease drops on every exit path.
/// `seal_workspace` always restricts the top directory to `0700` first, then hands it to root. A
/// non-root process cannot complete the chown, so the failure is surfaced as an error — the runner
/// turns that into a failed run rather than freeing the uid beside a uid-owned tree. As root the
/// directory ends up owned by root. (Full root ordering is covered by the root-gated suite.)
#[cfg(unix)]
#[test]
fn seal_workspace_restricts_then_reports_or_hands_to_root() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir(&ws).unwrap();
    fs::set_permissions(&ws, fs::Permissions::from_mode(0o777)).unwrap();
    let result = symphony_runtime::isolation::seal_workspace(&ws);
    let meta = fs::metadata(&ws).unwrap();
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o700,
        "the sealed workspace is always restricted first"
    );
    match result {
        Ok(()) => assert_eq!(meta.uid(), 0, "a sealed workspace is handed to root"),
        Err(_) => assert_ne!(
            meta.uid(),
            0,
            "a non-root process cannot hand it to root, and the failure is surfaced"
        ),
    }
}

fn task(id: i64) -> symphony_runtime::TaskSpec {
    symphony_runtime::TaskSpec {
        id,
        prompt: "p".into(),
        ..Default::default()
    }
}

fn allow_all() -> symphony_core::config::NetworkIsolation {
    symphony_core::config::NetworkIsolation {
        mode: symphony_core::config::NetworkMode::Allow,
        allow: Vec::new(),
        git_hosts: Vec::new(),
    }
}

#[tokio::test]
async fn the_workspace_and_home_are_handed_to_the_task_user() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use symphony_runtime::isolation::TaskIsolation;
    let (euid, egid) = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    // uid = gid = the task's: without root that is only possible as "myself" when they match.
    let first = if euid == 0 {
        4242
    } else if euid == egid {
        euid
    } else {
        eprintln!("skipped: needs root or a uid equal to the gid");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("TASK-3");
    fs::create_dir(&ws).unwrap();
    let isolation = symphony_core::config::IsolationSettings {
        task_uids: Some(symphony_core::config::TaskUids { first, count: 1 }),
        github_app: None,
        network: allow_all(),
    };
    let prepared = TaskIsolation::prepare(
        &Default::default(),
        &isolation,
        &task(3),
        &ws,
        Some((first, first)),
    )
    .await
    .unwrap();
    assert_eq!(prepared.run_as(), Some((first, first)));
    prepared.finish_workspace(&ws, false).unwrap();
    for path in [ws.clone(), ws.join(".home")] {
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(
            (meta.uid(), meta.gid()),
            (first, first),
            "{}",
            path.display()
        );
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o700,
            "{}",
            path.display()
        );
    }
    assert!(
        prepared.egress().is_none(),
        "network allow mode starts no proxy"
    );
}

#[tokio::test]
async fn a_planted_home_symlink_is_refused() {
    use symphony_runtime::isolation::TaskIsolation;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let ws = dir.path().join("TASK-4");
    fs::create_dir(&ws).unwrap();
    std::os::unix::fs::symlink(outside.path(), ws.join(".home")).unwrap();
    let isolation = symphony_core::config::IsolationSettings {
        task_uids: None,
        github_app: None,
        network: allow_all(),
    };
    let prepared = TaskIsolation::prepare(&Default::default(), &isolation, &task(4), &ws, None)
        .await
        .unwrap();
    let err = prepared.finish_workspace(&ws, true).unwrap_err();
    assert!(
        matches!(&err, RunError::IsolationUnavailable(m) if m.contains("is not a directory")),
        "{err:?}"
    );
    let env = prepared.task_env();
    assert_eq!(
        env,
        vec![("HOME".to_owned(), ws.join(".home").display().to_string())]
    );
}

/// A tracker whose agent tool acts with the operator's credential.
struct ToolTracker;

#[async_trait::async_trait]
impl symphony_trackers::Tracker for ToolTracker {
    fn kind(&self) -> &'static str {
        "memory"
    }

    async fn fetch_issues_by_states(
        &self,
        _settings: &symphony_core::config::TrackerSettings,
        _states: &[String],
    ) -> Result<Vec<Issue>, symphony_trackers::TrackerError> {
        Ok(Vec::new())
    }

    async fn fetch_issues_by_ids(
        &self,
        _settings: &symphony_core::config::TrackerSettings,
        _ids: &[String],
    ) -> Result<Vec<Issue>, symphony_trackers::TrackerError> {
        Ok(Vec::new())
    }

    fn secret_environment_names(
        &self,
        _settings: &symphony_core::config::TrackerSettings,
    ) -> Vec<String> {
        Vec::new()
    }

    fn agent_tool_specs(&self, _settings: &symphony_core::config::TrackerSettings) -> Vec<Value> {
        vec![json!({"name": "operator_tracker_tool", "description": "x",
            "inputSchema": {"type": "object"}})]
    }
}

#[tokio::test]
async fn an_isolated_task_gets_no_tracker_tools() {
    let symphony = RunnerOptions::default().symphony_executable;
    // A tracker issue still gets the operator's tracker tool...
    let s = setup_tracker(
        json!({"network": {"mode": "allow"}}),
        symphony.clone(),
        Arc::new(ToolTracker),
    );
    let mut ctx = WorkerContext::standalone(support::issue("issue-1", "MT-1", "In Progress"));
    ctx.events = EventSink::none();
    let runner = s.runner.clone().with_options(RunnerOptions {
        max_turns: Some(1),
        codex_env: vec![(
            "SYMP_TEST_CODEX_TRACE".into(),
            s.trace.display().to_string(),
        )],
        ..RunnerOptions::default()
    });
    let _ = runner.run(ctx).await;
    assert!(
        read(&s.trace).contains("operator_tracker_tool"),
        "{}",
        read(&s.trace)
    );

    // ...an isolated task (another tenant's work) does not.
    let s = setup_tracker(
        json!({"network": {"mode": "allow"}}),
        symphony,
        Arc::new(ToolTracker),
    );
    fs::create_dir_all(workspace(&s)).unwrap();
    let (ctx, _rx) = task_context(None);
    s.runner.run(ctx).await.unwrap();
    let trace = read(&s.trace);
    assert!(trace.contains("IN="), "the app-server ran: {trace}");
    assert!(!trace.contains("operator_tracker_tool"), "{trace}");
}
