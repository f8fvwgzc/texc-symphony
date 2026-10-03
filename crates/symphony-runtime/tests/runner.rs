//! Agent runner tests: ports of the `core_test.exs` runner cases (B.13.2) driven by the fake
//! app-servers of symphony-codex, plus the Rust improvements (after_run on cancellation, typed
//! blockers, remote launch through the SSH launcher).

mod support;

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use support::{TestWorkflow, codex_fixture, executable_copy, fixture, issue, read, sh_quote};
use symphony_codex::{CodexEvent, CodexEventKind, EventSink};
use symphony_core::Issue;
use symphony_runtime::runner::continue_with_issue;
use symphony_runtime::{
    AgentRunner, Continuation, FetchError, IssueFetcher, RunError, RunnerOptions, SshConfig,
    TrackerClient, WorkerContext, WorkspaceError,
};
use symphony_trackers::{MemoryIssues, MemoryTracker, TrackerDeps};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Returns scripted fetch results in order (the last one repeats); counts calls.
#[derive(Default)]
struct ScriptedFetcher {
    results: Mutex<VecDeque<Vec<Issue>>>,
    calls: Mutex<usize>,
}

impl ScriptedFetcher {
    fn new(results: Vec<Vec<Issue>>) -> Arc<Self> {
        Arc::new(Self {
            results: Mutex::new(results.into()),
            calls: Mutex::new(0),
        })
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl IssueFetcher for ScriptedFetcher {
    async fn fetch_issues_by_ids(&self, _ids: &[String]) -> Result<Vec<Issue>, FetchError> {
        *self.calls.lock().unwrap() += 1;
        let mut results = self.results.lock().unwrap();
        Ok(if results.len() > 1 {
            results.pop_front().unwrap_or_default()
        } else {
            results.front().cloned().unwrap_or_default()
        })
    }
}

struct Setup {
    wf: TestWorkflow,
    runner: AgentRunner,
    trace: PathBuf,
}

fn codex_command(script: &str) -> String {
    format!(
        "sh {} app-server",
        sh_quote(&codex_fixture(script).to_string_lossy())
    )
}

fn setup(config: Value, fetcher: Option<Arc<dyn IssueFetcher>>, ssh: Option<SshConfig>) -> Setup {
    let wf = TestWorkflow::new(config);
    let home = wf.path("home");
    fs::create_dir_all(&home).unwrap();
    let trace = wf.path("codex.trace");
    let client = TrackerClient::new(TrackerDeps::new().unwrap())
        .with_tracker(Arc::new(MemoryTracker::new(MemoryIssues::new())));
    let mut runner = AgentRunner::new(Arc::clone(&wf.store), client, ssh.unwrap_or_default())
        .with_options(RunnerOptions {
            codex_env: vec![
                ("HOME".into(), home.to_string_lossy().into_owned()),
                (
                    "SYMP_TEST_CODEX_TRACE".into(),
                    trace.to_string_lossy().into_owned(),
                ),
            ],
            codex_stop_grace: Duration::from_millis(500),
            ..RunnerOptions::default()
        });
    if let Some(fetcher) = fetcher {
        runner = runner.with_fetcher(fetcher);
    }
    Setup { wf, runner, trace }
}

fn traced_messages(trace: &Path) -> Vec<Value> {
    read(trace)
        .lines()
        .filter_map(|line| line.strip_prefix("JSON:"))
        .filter_map(|json| serde_json::from_str(json).ok())
        .collect()
}

fn methods(messages: &[Value], method: &str) -> Vec<Value> {
    messages
        .iter()
        .filter(|m| m["method"] == json!(method))
        .cloned()
        .collect()
}

fn context(issue: Issue) -> (WorkerContext, mpsc::UnboundedReceiver<CodexEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut ctx = WorkerContext::standalone(issue);
    ctx.events = EventSink::new(tx);
    (ctx, rx)
}

#[tokio::test]
async fn agent_runner_keeps_workspace_after_successful_codex_run() {
    let s = setup(
        json!({"codex": {"command": codex_command("basic.sh")}}),
        None,
        None,
    );
    let source = s.wf.path("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("README.md"), "# keep\n").unwrap();
    s.wf.rewrite(json!({
        "codex": {"command": codex_command("basic.sh")},
        "hooks": {"after_create": format!("cp '{}' README.md", source.join("README.md").display())},
    }));
    // No issue id: the run ends after one turn without a tracker refresh.
    let issue = Issue {
        identifier: Some("S-99".into()),
        title: Some("Smoke".into()),
        state: Some("In Progress".into()),
        ..Issue::default()
    };
    s.runner
        .run(WorkerContext::standalone(issue))
        .await
        .unwrap();
    let ws = s.wf.workspace_root().join("S-99");
    assert_eq!(read(&ws.join("README.md")), "# keep\n");
}

#[tokio::test]
async fn agent_runner_forwards_timestamped_codex_updates() {
    let fetcher = ScriptedFetcher::new(vec![vec![issue("issue-live-updates", "MT-LIVE", "Done")]]);
    let s = setup(
        json!({"codex": {"command": codex_command("basic.sh")}}),
        Some(fetcher.clone()),
        None,
    );
    let (ctx, mut rx) = context(issue("issue-live-updates", "MT-LIVE", "In Progress"));
    let before = chrono::Utc::now();
    s.runner.run(ctx).await.unwrap();
    let first = rx.recv().await.unwrap();
    assert_eq!(first.kind(), CodexEventKind::SessionStarted);
    assert_eq!(first.session_id(), Some("thread-1001-turn-1001"));
    assert!(first.timestamp >= before);
    assert!(first.codex_app_server_pid.is_some());
    assert_eq!(fetcher.calls(), 1);
}

#[tokio::test]
async fn agent_runner_reports_runtime_info_and_runs_after_run() {
    let s = setup(json!({}), None, None);
    let marker = s.wf.path("after_run.marker");
    s.wf.rewrite(json!({
        "codex": {"command": codex_command("basic.sh")},
        "hooks": {"after_run": format!("pwd > '{}'", marker.display())},
    }));
    let issue = Issue {
        id: None,
        ..issue("x", "MT-INFO", "In Progress")
    };
    s.runner
        .run(WorkerContext::standalone(issue))
        .await
        .unwrap();
    assert_eq!(
        PathBuf::from(read(&marker).trim()),
        s.wf.canonical_root().join("MT-INFO")
    );
}

#[tokio::test]
async fn agent_runner_continues_with_a_follow_up_turn_while_the_issue_remains_active() {
    let fetcher = ScriptedFetcher::new(vec![
        vec![issue("i-1", "MT-247", "In Progress")],
        vec![issue("i-1", "MT-247", "Done")],
    ]);
    let s = setup(
        json!({"codex": {"command": codex_command("multi_turn.sh")}, "agent": {"max_turns": 3}}),
        Some(fetcher.clone()),
        None,
    );
    s.runner
        .run(WorkerContext::standalone(issue(
            "i-1",
            "MT-247",
            "In Progress",
        )))
        .await
        .unwrap();
    assert_eq!(fetcher.calls(), 2);
    let messages = traced_messages(&s.trace);
    assert_eq!(
        methods(&messages, "initialize").len(),
        1,
        "one app-server process"
    );
    assert_eq!(methods(&messages, "thread/start").len(), 1);
    let turns = methods(&messages, "turn/start");
    assert_eq!(turns.len(), 2);
    let first = turns[0]["params"]["input"][0]["text"].as_str().unwrap();
    let second = turns[1]["params"]["input"][0]["text"].as_str().unwrap();
    assert!(first.contains("You are an agent for this repository."));
    assert!(!second.contains("You are an agent for this repository."));
    assert!(second.contains("Continuation guidance:"));
    assert!(second.contains("continuation turn #2 of 3"));
}

#[tokio::test]
async fn agent_runner_stops_continuing_once_max_turns_is_reached() {
    let fetcher = ScriptedFetcher::new(vec![vec![issue("i-1", "MT-248", "In Progress")]]);
    let s = setup(
        json!({"codex": {"command": codex_command("multi_turn.sh")}, "agent": {"max_turns": 2}}),
        Some(fetcher.clone()),
        None,
    );
    s.runner
        .run(WorkerContext::standalone(issue(
            "i-1",
            "MT-248",
            "In Progress",
        )))
        .await
        .unwrap();
    let messages = traced_messages(&s.trace);
    assert_eq!(methods(&messages, "initialize").len(), 1);
    assert_eq!(methods(&messages, "turn/start").len(), 2);
}

#[tokio::test]
async fn agent_runner_does_not_continue_after_a_required_label_is_removed() {
    let wf = TestWorkflow::new(json!({"tracker": {"required_labels": ["symphony"]}}));
    let labeled = Issue {
        labels: vec!["symphony".into()],
        ..issue("i-1", "MT-1", "In Progress")
    };
    let fetcher = ScriptedFetcher::new(vec![vec![issue("i-1", "MT-1", "In Progress")]]);
    let result = continue_with_issue(&labeled, fetcher.as_ref(), &wf.store.settings())
        .await
        .unwrap();
    assert_eq!(
        result,
        Continuation::Done(issue("i-1", "MT-1", "In Progress"))
    );
    let fetcher = ScriptedFetcher::new(vec![vec![labeled.clone()]]);
    let result = continue_with_issue(&labeled, fetcher.as_ref(), &wf.store.settings())
        .await
        .unwrap();
    assert_eq!(result, Continuation::Continue(labeled.clone()));
    let empty = ScriptedFetcher::new(vec![vec![]]);
    assert_eq!(
        continue_with_issue(&labeled, empty.as_ref(), &wf.store.settings())
            .await
            .unwrap(),
        Continuation::Done(labeled)
    );
}

#[tokio::test]
async fn agent_runner_surfaces_ssh_startup_failures_instead_of_hopping_hosts() {
    let wf_dir = tempfile::tempdir().unwrap();
    let exe = executable_copy(&fixture("ssh/fake_ssh.sh"), wf_dir.path(), "ssh");
    fs::write(wf_dir.path().join("fail_hosts"), "worker-a\n").unwrap();
    let s = setup(
        json!({"workspace": {"root": "~/.symphony-remote-workspaces"},
            "worker": {"ssh_hosts": ["worker-a", "worker-b"]}}),
        None,
        Some(SshConfig::default().with_executable(exe)),
    );
    let mut ctx = WorkerContext::standalone(issue("i-1", "MT-SSH", "In Progress"));
    ctx.worker_host = Some("worker-a".into());
    let err = s.runner.run(ctx).await.unwrap_err();
    assert!(
        matches!(err, RunError::Workspace(WorkspaceError::PrepareFailed { ref host, status: 75, .. }) if host == "worker-a"),
        "{err:?}"
    );
    assert!(err.to_string().starts_with("workspace_prepare_failed"));
    let trace = read(&wf_dir.path().join("ssh.trace"));
    assert!(trace.contains("worker-a bash -lc"));
    assert!(!trace.contains("worker-b bash -lc"));
}

#[tokio::test]
async fn agent_runner_runs_remote_codex_sessions_over_ssh() {
    let ssh_dir = tempfile::tempdir().unwrap();
    let exe = executable_copy(&fixture("ssh/fake_ssh.sh"), ssh_dir.path(), "ssh");
    let fetcher = ScriptedFetcher::new(vec![vec![issue("i-1", "MT-R", "Done")]]);
    let s = setup(
        json!({"workspace": {"root": "~/.symphony-remote-workspaces"},
            "worker": {"ssh_hosts": ["worker-a:2200"]}}),
        Some(fetcher),
        Some(SshConfig::default().with_executable(exe)),
    );
    let (mut ctx, mut rx) = context(issue("i-1", "MT-R", "In Progress"));
    ctx.worker_host = Some("worker-a:2200".into());
    s.runner.run(ctx).await.unwrap();
    let first = rx.recv().await.unwrap();
    assert_eq!(first.session_id(), Some("thread-remote-turn-remote"));
    assert_eq!(first.worker_host.as_deref(), Some("worker-a:2200"));
    let trace = read(&ssh_dir.path().join("ssh.trace"));
    assert!(trace.contains("-T -p 2200 worker-a bash -lc"));
    assert!(trace.contains("exec codex app-server"));
    assert!(trace.contains("/remote/home/.symphony-remote-workspaces/MT-SSH-WS"));
}

#[tokio::test]
async fn input_required_turns_fail_with_a_typed_blocker() {
    let s = setup(json!({}), None, None);
    s.wf.rewrite(json!({"codex": {"command": codex_command("single_request.sh")}}));
    let runner = s.runner.clone().with_options(RunnerOptions {
        codex_env: vec![
            (
                "HOME".into(),
                s.wf.path("home").to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CODEX_MESSAGE".into(),
                r#"{"method":"turn/input_required","id":"resp-1","params":{"requiresInput":true}}"#
                    .into(),
            ),
        ],
        codex_stop_grace: Duration::from_millis(200),
        ..RunnerOptions::default()
    });
    let (ctx, mut rx) = context(issue("i-1", "MT-IN", "In Progress"));
    let err = runner.run(ctx).await.unwrap_err();
    assert_eq!(err.blocker(), Some(symphony_codex::Blocker::InputRequired));
    let mut last = None;
    while let Ok(event) = rx.try_recv() {
        last = Some(event.kind());
    }
    assert_eq!(last, Some(CodexEventKind::TurnInputRequired));
}

#[tokio::test]
async fn after_run_still_runs_when_before_run_fails() {
    let s = setup(json!({}), None, None);
    let marker = s.wf.path("after_run.marker");
    s.wf.rewrite(json!({"hooks": {
        "before_run": "echo nope; exit 3",
        "after_run": format!("touch '{}'", marker.display()),
    }}));
    let err = s
        .runner
        .run(WorkerContext::standalone(issue(
            "i-1",
            "MT-BR",
            "In Progress",
        )))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RunError::Workspace(WorkspaceError::HookFailed { ref hook, status: 3, .. }) if hook == "before_run")
    );
    assert!(marker.exists());
}

#[tokio::test]
async fn cancelled_runs_stop_codex_and_still_run_after_run() {
    let s = setup(json!({}), None, None);
    let script = s.wf.path("server.script");
    fs::write(
        &script,
        "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{\"thread\":{\"id\":\"thread-c\"}}}\n4 {\"id\":3,\"result\":{\"turn\":{\"id\":\"turn-c\"}}}\n4 @sleep 30\n",
    )
    .unwrap();
    let marker = s.wf.path("after_run.marker");
    s.wf.rewrite(json!({
        "codex": {"command": format!("sh {} {}", sh_quote(&codex_fixture("scripted.sh").to_string_lossy()), sh_quote(&script.to_string_lossy()))},
        "hooks": {"after_run": format!("touch '{}'", marker.display())},
    }));
    let (mut ctx, mut rx) = context(issue("i-1", "MT-CANCEL", "In Progress"));
    let cancel = CancellationToken::new();
    ctx.cancel = cancel.clone();
    let runner = s.runner.clone();
    let task = tokio::spawn(async move { runner.run(ctx).await });
    let started = rx.recv().await.unwrap();
    assert_eq!(started.kind(), CodexEventKind::SessionStarted);
    let pid = started.codex_app_server_pid.clone().unwrap();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, Err(RunError::Cancelled));
    assert!(marker.exists(), "after_run runs on cancellation");
    support::eventually(Duration::from_secs(3), || {
        !std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
    .await;
}
