//! Records of running local agent processes (new in the Rust port).
//!
//! A hard-killed Symphony (`SIGKILL`, power loss of the supervisor, OOM) cannot stop its agents. Most
//! exit on their own when their stdin closes, but one that is hung at that moment keeps running, and
//! the restarted orchestrator would start a second agent for the same issue next to it. Each local
//! agent is therefore recorded in `<workspace root>/.symphony/agents/<pid>.json` while it runs, and
//! startup kills the process groups of recorded agents whose owner is gone.
//!
//! A pid can be reused, so a record carries the start time of both processes (`ps -o lstart=`) and
//! nothing is killed unless the agent's still matches. Records whose owner is alive (another Symphony
//! on the same workspace root) are left alone.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::process::kill_process_group;

/// One running agent and the Symphony process that started it.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    owner_pid: u32,
    owner_started: String,
    pid: u32,
    started: String,
    issue: Option<String>,
}

/// `<workspace root>/.symphony/agents`.
pub(crate) fn registry_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".symphony").join("agents")
}

/// When `pid` started, as `ps` prints it; `None` when there is no such process (or no `ps`).
async fn started_at(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .await
        .ok()?;
    let started = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !started.is_empty()).then_some(started)
}

/// Removes the agent's record when the agent is gone.
#[derive(Debug)]
pub(crate) struct Registration {
    path: PathBuf,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Records agent `pid` (a process-group leader) under `dir` until the returned guard is dropped.
/// Best effort: `None` when the record cannot be written, which only costs the cleanup after a
/// hard kill.
pub(crate) async fn register(dir: &Path, pid: u32, issue: Option<&str>) -> Option<Registration> {
    let owner_pid = std::process::id();
    let record = Record {
        owner_pid,
        owner_started: started_at(owner_pid).await?,
        pid,
        started: started_at(pid).await?,
        issue: issue.map(str::to_owned),
    };
    tokio::fs::create_dir_all(dir).await.ok()?;
    let path = dir.join(format!("{pid}.json"));
    let body = serde_json::to_vec(&record).ok()?;
    tokio::fs::write(&path, body).await.ok()?;
    Some(Registration { path })
}

/// Kills the agents a dead Symphony left behind and drops their records. Returns how many process
/// groups were killed.
pub(crate) async fn reap_stale(dir: &Path) -> usize {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let mut killed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let record = tokio::fs::read(&path)
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<Record>(&body).ok());
        if let Some(record) = record {
            if started_at(record.owner_pid).await.as_deref() == Some(&record.owner_started) {
                continue;
            }
            if started_at(record.pid).await.as_deref() == Some(&record.started) {
                tracing::warn!(
                    "Killing agent left behind by an earlier Symphony process pid={} issue_identifier={}",
                    record.pid,
                    record.issue.as_deref().unwrap_or("n/a")
                );
                kill_process_group(record.pid);
                killed += 1;
            }
        }
        let _ = tokio::fs::remove_file(&path).await;
    }
    killed
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A sleeping process-group leader, as agents are.
    fn sleeper() -> tokio::process::Child {
        Command::new("sleep")
            .arg("300")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    async fn write(dir: &Path, record: &Record) -> PathBuf {
        tokio::fs::create_dir_all(dir).await.unwrap();
        let path = dir.join(format!("{}.json", record.pid));
        tokio::fs::write(&path, serde_json::to_vec(record).unwrap())
            .await
            .unwrap();
        path
    }

    async fn exited(child: &mut tokio::process::Child) -> bool {
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn a_record_lives_as_long_as_its_registration() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = sleeper();
        let pid = agent.id().unwrap();
        let registration = register(dir.path(), pid, Some("MT-1")).await.unwrap();
        let path = dir.path().join(format!("{pid}.json"));
        let record: Record = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.owner_pid, std::process::id());
        assert_eq!(record.issue.as_deref(), Some("MT-1"));

        // The owner (this test process) is alive: nothing is reaped.
        assert_eq!(reap_stale(dir.path()).await, 0);
        assert!(path.exists());
        drop(registration);
        assert!(!path.exists());
        agent.kill().await.unwrap();
    }

    #[tokio::test]
    async fn agents_of_a_dead_owner_are_killed_but_reused_pids_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let (mut orphan, mut reused) = (sleeper(), sleeper());
        let (orphan_pid, reused_pid) = (orphan.id().unwrap(), reused.id().unwrap());
        let dead_owner = |pid: u32, started: String| Record {
            owner_pid: 1,
            owner_started: "not when pid 1 started".into(),
            pid,
            started,
            issue: None,
        };
        let orphan_record = write(
            dir.path(),
            &dead_owner(orphan_pid, started_at(orphan_pid).await.unwrap()),
        )
        .await;
        // Same pid, different start time: some unrelated process now has the recorded pid.
        let reused_record = write(
            dir.path(),
            &dead_owner(reused_pid, "Thu Jan  1 00:00:00 1970".into()),
        )
        .await;
        tokio::fs::write(dir.path().join("garbage.json"), b"{")
            .await
            .unwrap();

        assert_eq!(reap_stale(dir.path()).await, 1);
        assert!(exited(&mut orphan).await);
        assert!(reused.try_wait().unwrap().is_none());
        assert!(!orphan_record.exists() && !reused_record.exists());
        assert!(!dir.path().join("garbage.json").exists());
        reused.kill().await.unwrap();

        assert_eq!(reap_stale(&dir.path().join("missing")).await, 0);
    }
}
