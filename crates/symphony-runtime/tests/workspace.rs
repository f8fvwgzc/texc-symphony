//! Workspace and hook tests: ports of the `workspace_and_config_test.exs` workspace cases (B.13.5)
//! plus the Rust improvements (real hook timeouts with process-group kill, remote containment check,
//! hook names on remote timeouts).

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use support::{TestWorkflow, executable_copy, fixture, read};
use symphony_core::Issue;
use symphony_core::path_safety::PathError;
use symphony_runtime::process::EnvPolicy;
use symphony_runtime::ssh::SshConfig;
use symphony_runtime::workspace::{IssueContext, WorkspaceError, WorkspaceManager};

fn manager(wf: &TestWorkflow) -> WorkspaceManager {
    WorkspaceManager::new(wf.store.settings(), &wf.path, SshConfig::default())
}

fn ctx(identifier: &str) -> IssueContext {
    IssueContext::identifier(identifier)
}

fn hooks(extra: serde_json::Value) -> serde_json::Value {
    let mut hooks = json!({"timeout_ms": 60000});
    support::merge(&mut hooks, &extra);
    json!({ "hooks": hooks })
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

#[tokio::test]
async fn after_create_bootstraps_a_new_workspace() {
    let wf = TestWorkflow::new(json!({}));
    let source = wf.path("source");
    fs::create_dir_all(source.join("keep")).unwrap();
    fs::write(source.join("README.md"), "# demo\n").unwrap();
    fs::write(source.join("keep/file.txt"), "kept\n").unwrap();
    wf.rewrite(hooks(
        json!({"after_create": format!("cp -R '{}'/. .", source.display())}),
    ));
    let ws = manager(&wf)
        .create_for_issue(&ctx("S-1"), None)
        .await
        .unwrap();
    assert_eq!(read(&Path::new(&ws).join("README.md")), "# demo\n");
    assert_eq!(read(&Path::new(&ws).join("keep/file.txt")), "kept\n");
}

#[tokio::test]
async fn workspace_path_is_deterministic_per_issue_identifier() {
    let wf = TestWorkflow::new(json!({}));
    let first = manager(&wf)
        .create_for_issue(&ctx("MT/Det"), None)
        .await
        .unwrap();
    let second = manager(&wf)
        .create_for_issue(&ctx("MT/Det"), None)
        .await
        .unwrap();
    assert_eq!(first, second);
    let name = Path::new(&first)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(name, symphony_core::workspace_key(Some("MT/Det")));
    assert!(name.starts_with("MT_Det--"));
}

#[tokio::test]
async fn relative_workspace_roots_resolve_from_the_workflow_directory() {
    let wf = TestWorkflow::new(json!({"workspace": {"root": "relative-workspaces"}}));
    let ws = manager(&wf)
        .create_for_issue(&ctx("MT-REL"), None)
        .await
        .unwrap();
    assert_eq!(
        PathBuf::from(&ws),
        canonical(wf.dir.path()).join("relative-workspaces/MT-REL")
    );
}

#[tokio::test]
async fn workspace_keys_disambiguate_identifiers_that_sanitize_alike() {
    let wf = TestWorkflow::new(json!({}));
    let m = manager(&wf);
    let issue = Issue {
        identifier: Some("team/a-1".into()),
        ..Issue::default()
    };
    let from_issue = m
        .create_for_issue(&IssueContext::from(&issue), None)
        .await
        .unwrap();
    let from_string = m.create_for_issue(&ctx("team/a-1"), None).await.unwrap();
    let plain = m.create_for_issue(&ctx("team_a-1"), None).await.unwrap();
    assert_eq!(from_issue, from_string);
    assert_ne!(from_issue, plain);
    assert!(plain.ends_with("/team_a-1"));
    m.remove_issue_workspaces(Some("team/a-1"), None).await;
    assert!(!Path::new(&from_issue).exists());
    assert!(Path::new(&plain).exists());
}

#[tokio::test]
async fn workspace_reuse_keeps_local_changes_and_skips_after_create() {
    let wf = TestWorkflow::new(json!({}));
    let log = wf.path("after_create.log");
    wf.rewrite(hooks(json!({"after_create": format!("echo created >> '{}'\necho original > README.md", log.display())})));
    let m = manager(&wf);
    let ws = m.create_for_issue(&ctx("MT-REUSE"), None).await.unwrap();
    let ws = Path::new(&ws);
    fs::write(ws.join("README.md"), "changed\n").unwrap();
    fs::create_dir_all(ws.join("deps")).unwrap();
    fs::create_dir_all(ws.join("_build")).unwrap();
    fs::write(ws.join("local.txt"), "local\n").unwrap();
    let again = m.create_for_issue(&ctx("MT-REUSE"), None).await.unwrap();
    assert_eq!(Path::new(&again), ws);
    assert_eq!(read(&ws.join("README.md")), "changed\n");
    assert!(ws.join("deps").is_dir() && ws.join("_build").is_dir());
    assert_eq!(read(&ws.join("local.txt")), "local\n");
    assert_eq!(read(&log).lines().count(), 1);
}

#[tokio::test]
async fn stale_non_directory_paths_are_replaced() {
    let wf = TestWorkflow::new(json!({}));
    fs::write(wf.workspace_root().join("MT-FILE"), "stale").unwrap();
    let ws = manager(&wf)
        .create_for_issue(&ctx("MT-FILE"), None)
        .await
        .unwrap();
    assert!(Path::new(&ws).is_dir());
    assert_eq!(PathBuf::from(&ws), wf.canonical_root().join("MT-FILE"));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escapes_under_the_root_are_rejected() {
    let wf = TestWorkflow::new(json!({}));
    let outside = wf.path("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, wf.workspace_root().join("MT-SYM")).unwrap();
    let err = manager(&wf)
        .create_for_issue(&ctx("MT-SYM"), None)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        WorkspaceError::OutsideRoot {
            workspace: canonical(&outside).to_string_lossy().into_owned(),
            root: wf.canonical_root().to_string_lossy().into_owned(),
        }
    );
}

#[cfg(unix)]
#[tokio::test]
async fn recorded_removal_rejects_symlink_escapes_before_hooks() {
    let wf = TestWorkflow::new(json!({}));
    let marker = wf.path("before_remove.marker");
    wf.rewrite(hooks(
        json!({"before_remove": format!("touch '{}'", marker.display())}),
    ));
    let recorded_root = canonical(&wf.workspace_root()).join("recorded");
    let outside = wf.path("outside");
    fs::create_dir_all(&recorded_root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let recorded_ws = recorded_root.join("MT-SYM");
    std::os::unix::fs::symlink(&outside, &recorded_ws).unwrap();
    let err = manager(&wf)
        .remove_recorded(&recorded_ws.to_string_lossy(), None)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        WorkspaceError::SymlinkEscape {
            workspace: recorded_ws.to_string_lossy().into_owned(),
            root: recorded_root.to_string_lossy().into_owned(),
        }
    );
    assert!(!marker.exists());
    assert!(outside.exists());
    let relative = manager(&wf)
        .remove_recorded("relative/ws", None)
        .await
        .unwrap_err();
    assert!(
        matches!(relative, WorkspaceError::PathUnreadable { reason, .. } if reason == "not_absolute")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_roots_are_canonicalized() {
    let wf = TestWorkflow::new(json!({}));
    let actual = wf.path("actual-root");
    fs::create_dir_all(&actual).unwrap();
    let link = wf.path("linked-root");
    std::os::unix::fs::symlink(&actual, &link).unwrap();
    wf.rewrite(json!({"workspace": {"root": link.to_string_lossy()}}));
    let ws = manager(&wf)
        .create_for_issue(&ctx("MT-LINK"), None)
        .await
        .unwrap();
    assert_eq!(PathBuf::from(ws), canonical(&actual).join("MT-LINK"));
}

#[tokio::test]
async fn remove_rejects_the_root_itself_and_ignores_missing_paths() {
    let wf = TestWorkflow::new(json!({}));
    let root = wf.canonical_root().to_string_lossy().into_owned();
    let err = manager(&wf).remove(&root, None).await.unwrap_err();
    assert_eq!(
        err,
        WorkspaceError::EqualsRoot {
            workspace: root.clone(),
            root
        }
    );
    let missing = wf.canonical_root().join("missing");
    assert_eq!(
        manager(&wf).remove(&missing.to_string_lossy(), None).await,
        Ok(())
    );
}

#[tokio::test]
async fn after_create_failure_is_fatal_and_reported() {
    let wf = TestWorkflow::new(json!({}));
    wf.rewrite(hooks(json!({"after_create": "echo nope && exit 17"})));
    let err = manager(&wf)
        .create_for_issue(&ctx("MT-FAIL"), None)
        .await
        .unwrap_err();
    match err {
        WorkspaceError::HookFailed {
            hook,
            status,
            output,
        } => {
            assert_eq!((hook.as_str(), status), ("after_create", 17));
            assert!(output.contains("nope"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        !wf.workspace_root().join("MT-FAIL").exists(),
        "partial workspace removed"
    );
}

#[tokio::test]
async fn after_create_is_retried_after_a_failed_bootstrap() {
    let wf = TestWorkflow::new(json!({}));
    let log = wf.path("bootstrap.log");
    let flag = wf.path("fail-once");
    fs::write(&flag, "").unwrap();
    wf.rewrite(hooks(json!({"after_create": format!(
        "echo run >> '{log}'\necho partial > partial.txt\nif [ -e '{flag}' ]; then rm '{flag}'; exit 1; fi\ntouch READY",
        log = log.display(), flag = flag.display()
    )})));
    let m = manager(&wf);
    assert!(m.create_for_issue(&ctx("MT-RETRY"), None).await.is_err());
    let ws = m.create_for_issue(&ctx("MT-RETRY"), None).await.unwrap();
    assert!(Path::new(&ws).join("READY").exists());
    assert_eq!(read(&log).lines().count(), 2);
}

#[tokio::test]
async fn after_create_timeout_is_reported_with_the_hook_name() {
    let wf = TestWorkflow::new(json!({}));
    wf.rewrite(json!({"hooks": {"after_create": "sleep 5", "timeout_ms": 100}}));
    let started = std::time::Instant::now();
    let err = manager(&wf)
        .create_for_issue(&ctx("MT-SLOW"), None)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        WorkspaceError::HookTimeout {
            hook: "after_create".into(),
            timeout_ms: 100
        }
    );
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn hook_timeout_kills_the_hook_process_group() {
    let wf = TestWorkflow::new(json!({}));
    let pidfile = wf.path("child.pid");
    wf.rewrite(json!({"hooks": {
        "before_run": format!("sleep 30 & echo $! > '{}'; wait", pidfile.display()),
        "timeout_ms": 300
    }}));
    let m = manager(&wf);
    let ws = m.create_for_issue(&ctx("MT-PG"), None).await.unwrap();
    let err = m
        .run_before_run_hook(&ws, &ctx("MT-PG"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::HookTimeout { ref hook, .. } if hook == "before_run"));
    let pid = read(&pidfile).trim().to_owned();
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

#[tokio::test]
async fn new_workspace_without_hooks_is_an_empty_directory() {
    let wf = TestWorkflow::new(json!({}));
    let ws = manager(&wf)
        .create_for_issue(&ctx("MT-EMPTY"), None)
        .await
        .unwrap();
    assert_eq!(PathBuf::from(&ws), wf.canonical_root().join("MT-EMPTY"));
    assert_eq!(fs::read_dir(&ws).unwrap().count(), 0);
}

#[tokio::test]
async fn removing_issue_workspaces_only_touches_the_key_directory() {
    let wf = TestWorkflow::new(json!({}));
    let m = manager(&wf);
    fs::create_dir_all(wf.workspace_root().join("S_1")).unwrap();
    fs::create_dir_all(wf.workspace_root().join("S_10")).unwrap();
    m.remove_issue_workspaces(Some("S_1"), None).await;
    assert!(!wf.workspace_root().join("S_1").exists());
    assert!(wf.workspace_root().join("S_10").exists());
    // missing root and missing identifier are fine
    let missing = TestWorkflow::new(json!({}));
    fs::remove_dir_all(missing.workspace_root()).unwrap();
    manager(&missing)
        .remove_issue_workspaces(Some("S_1"), None)
        .await;
    manager(&missing).remove_issue_workspaces(None, None).await;
}

#[tokio::test]
async fn hooks_run_once_per_creation_and_before_removal() {
    let wf = TestWorkflow::new(json!({}));
    let marker = wf.path("before_remove.marker");
    wf.rewrite(hooks(json!({
        "after_create": "echo after_create > after_create.log",
        "before_remove": format!("echo before_remove > '{}'", marker.display()),
    })));
    let m = manager(&wf);
    let ws = m.create_for_issue(&ctx("MT-HOOKS"), None).await.unwrap();
    fs::remove_file(Path::new(&ws).join("after_create.log")).unwrap();
    m.create_for_issue(&ctx("MT-HOOKS"), None).await.unwrap();
    assert!(
        !Path::new(&ws).join("after_create.log").exists(),
        "after_create ran once"
    );
    m.remove_issue_workspaces(Some("MT-HOOKS"), None).await;
    assert_eq!(read(&marker), "before_remove\n");
    assert!(!Path::new(&ws).exists());
}

#[tokio::test]
async fn failing_noisy_or_slow_before_remove_hooks_do_not_block_removal() {
    for hook in [
        "echo nope && exit 17".to_owned(),
        "i=0; while [ $i -lt 3000 ]; do printf x; i=$((i+1)); done; exit 1".to_owned(),
        "sleep 30".to_owned(),
    ] {
        let wf = TestWorkflow::new(json!({}));
        wf.rewrite(json!({"hooks": {"before_remove": hook, "timeout_ms": 300}}));
        let m = manager(&wf);
        let ws = m.create_for_issue(&ctx("MT-RM"), None).await.unwrap();
        let started = std::time::Instant::now();
        m.remove_issue_workspaces(Some("MT-RM"), None).await;
        assert!(!Path::new(&ws).exists());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

#[tokio::test]
async fn path_safety_reports_invalid_segments() {
    let wf = TestWorkflow::new(json!({}));
    let long = "a".repeat(300);
    let err = manager(&wf)
        .create_for_issue(&ctx(&long), None)
        .await
        .unwrap_err();
    match err {
        WorkspaceError::Path(PathError::CanonicalizeFailed { reason, .. }) => {
            assert_eq!(reason.name(), "enametoolong")
        }
        other => panic!("unexpected {other:?}"),
    }
}

// ----- remote workspaces ---------------------------------------------------------------------------

fn fake_ssh(wf: &TestWorkflow, name: &str) -> SshConfig {
    let exe = executable_copy(&fixture(&format!("ssh/{name}")), wf.dir.path(), "ssh");
    SshConfig::default().with_executable(exe)
}

fn remote_manager(wf: &TestWorkflow, ssh: SshConfig) -> WorkspaceManager {
    WorkspaceManager::new(wf.store.settings(), &wf.path, ssh)
}

#[tokio::test]
async fn remote_workspace_lifecycle_uses_ssh_host_aliases() {
    let wf = TestWorkflow::new(json!({}));
    wf.rewrite(json!({
        "workspace": {"root": "~/.symphony-remote-workspaces"},
        "worker": {"ssh_hosts": ["worker-01:2200"]},
        "hooks": {
            "before_run": "echo before-run-hook",
            "after_run": "echo after-run-hook",
            "before_remove": "echo before-remove-hook",
        },
    }));
    let m = remote_manager(&wf, fake_ssh(&wf, "fake_ssh.sh"));
    let host = Some("worker-01:2200");
    let ws = m.create_for_issue(&ctx("MT-SSH-WS"), host).await.unwrap();
    assert_eq!(ws, "/remote/home/.symphony-remote-workspaces/MT-SSH-WS");
    m.run_before_run_hook(&ws, &ctx("MT-SSH-WS"), host)
        .await
        .unwrap();
    m.run_after_run_hook(&ws, &ctx("MT-SSH-WS"), host).await;
    m.remove(&ws, host).await.unwrap();
    let trace = read(&wf.path("ssh.trace"));
    for needle in [
        "-p 2200 worker-01 bash -lc",
        "__SYMPHONY_WORKSPACE__",
        "~/.symphony-remote-workspaces/MT-SSH-WS",
        "${workspace#\\~/}",
        "before-run-hook",
        "after-run-hook",
        "before-remove-hook",
        "rm -rf",
        "/remote/home/.symphony-remote-workspaces/MT-SSH-WS",
    ] {
        assert!(trace.contains(needle), "trace lacks {needle:?}:\n{trace}");
    }
}

#[tokio::test]
async fn remote_prepare_failures_are_reported_with_host_and_status() {
    let wf = TestWorkflow::new(json!({"workspace": {"root": "~/ws"}}));
    fs::write(wf.path("fail_hosts"), "worker-a\n").unwrap();
    let m = remote_manager(&wf, fake_ssh(&wf, "fake_ssh.sh"));
    let err = m
        .create_for_issue(&ctx("MT-1"), Some("worker-a"))
        .await
        .unwrap_err();
    match err {
        WorkspaceError::PrepareFailed {
            host,
            status,
            output,
        } => {
            assert_eq!((host.as_str(), status), ("worker-a", 75));
            assert!(output.contains("worker-a prepare failed"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn remote_scripts_create_reuse_and_remove_for_real() {
    let wf = TestWorkflow::new(json!({}));
    let remote_root = wf.path("remote-root");
    wf.rewrite(json!({"workspace": {"root": remote_root.to_string_lossy()},
        "hooks": {"after_create": "echo created > created.txt"}}));
    let m = remote_manager(&wf, fake_ssh(&wf, "local_ssh.sh"));
    let ws = m
        .create_for_issue(&ctx("MT-LOCAL"), Some("host"))
        .await
        .unwrap();
    assert_eq!(PathBuf::from(&ws), canonical(&remote_root).join("MT-LOCAL"));
    assert_eq!(read(&Path::new(&ws).join("created.txt")), "created\n");
    fs::remove_file(Path::new(&ws).join("created.txt")).unwrap();
    let again = m
        .create_for_issue(&ctx("MT-LOCAL"), Some("host"))
        .await
        .unwrap();
    assert_eq!(again, ws);
    assert!(
        !Path::new(&ws).join("created.txt").exists(),
        "after_create not re-run on reuse"
    );
    m.remove(&ws, Some("host")).await.unwrap();
    assert!(!Path::new(&ws).exists());
}

#[cfg(unix)]
#[tokio::test]
async fn remote_workspaces_escaping_the_root_are_rejected() {
    let wf = TestWorkflow::new(json!({}));
    let remote_root = wf.path("remote-root");
    let outside = wf.path("outside");
    fs::create_dir_all(&remote_root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, remote_root.join("MT-ESC")).unwrap();
    wf.rewrite(json!({"workspace": {"root": remote_root.to_string_lossy()},
        "hooks": {"after_create": "touch should-not-run"}}));
    let m = remote_manager(&wf, fake_ssh(&wf, "local_ssh.sh"));
    let err = m
        .create_for_issue(&ctx("MT-ESC"), Some("host"))
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::OutsideRoot { .. }), "{err:?}");
    assert!(!outside.join("should-not-run").exists());
    let dots = m
        .create_for_issue(&ctx(".."), Some("host"))
        .await
        .unwrap_err();
    assert!(
        matches!(dots, WorkspaceError::OutsideRoot { .. }),
        "{dots:?}"
    );
}

#[tokio::test]
async fn remote_hook_timeouts_report_the_hook_name() {
    let wf = TestWorkflow::new(json!({}));
    let remote_root = wf.path("remote-root");
    wf.rewrite(json!({"workspace": {"root": remote_root.to_string_lossy()},
        "hooks": {"before_run": "sleep 30", "timeout_ms": 2000}}));
    let m = remote_manager(&wf, fake_ssh(&wf, "local_ssh.sh")).with_hook_env(EnvPolicy::inherit());
    let ws = m
        .create_for_issue(&ctx("MT-T"), Some("host"))
        .await
        .unwrap();
    let err = m
        .run_before_run_hook(&ws, &ctx("MT-T"), Some("host"))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        WorkspaceError::HookTimeout {
            hook: "before_run".into(),
            timeout_ms: 2000
        }
    );
}

#[tokio::test]
async fn issue_workspaces_are_removed_on_every_configured_host() {
    let wf = TestWorkflow::new(json!({"workspace": {"root": "~/ws"},
        "worker": {"ssh_hosts": ["worker-a", "worker-b", "worker-c"]}}));
    let m = remote_manager(&wf, fake_ssh(&wf, "fake_ssh.sh"));
    m.remove_issue_workspaces(Some("MT-ALL"), None).await;
    let trace = read(&wf.path("ssh.trace"));
    for host in ["worker-a", "worker-b", "worker-c"] {
        assert!(
            trace.contains(&format!("-T {host} bash -lc")),
            "{host} missing:\n{trace}"
        );
    }
}
