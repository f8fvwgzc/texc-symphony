//! Child processes in their own process group, killed (group and all) when dropped.
//!
//! Backends run agent CLIs and shell commands that start further processes. Every child leads a
//! new process group; [`GroupChild`] kills the whole group on drop, so a cancelled turn (the
//! runner drops the turn future) leaves nothing behind.

use std::io;
use std::process::ExitStatus;

use tokio::process::{Child, Command};

/// A spawned child that leads its own process group.
#[derive(Debug)]
pub(crate) struct GroupChild {
    child: Child,
    pid: Option<u32>,
    reaped: bool,
}

impl GroupChild {
    /// Spawns `command` as the leader of a new process group.
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        command.process_group(0).kill_on_drop(true);
        let child = command.spawn()?;
        let pid = child.id();
        Ok(Self {
            child,
            pid,
            reaped: false,
        })
    }

    /// OS pid (also the process group id).
    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The child, for taking its pipes.
    pub(crate) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Waits for the leader to exit, then kills what is left of its group.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await;
        self.reaped = true;
        kill_group(self.pid);
        status
    }

    /// Kills the whole group and reaps the leader.
    pub(crate) async fn kill(&mut self) {
        kill_group(self.pid);
        let _ = self.child.kill().await;
        self.reaped = true;
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        // Also after a normal exit: background processes the leader left in its group go too.
        kill_group(self.pid);
        if !self.reaped {
            let _ = self.child.start_kill();
        }
    }
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    use rustix::process::{Pid, Signal, kill_process_group};
    let Some(pid) = pid
        .and_then(|p| i32::try_from(p).ok())
        .and_then(Pid::from_raw)
    else {
        return;
    };
    let _ = kill_process_group(pid, Signal::KILL);
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn dropping_the_child_kills_its_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild.pid");
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", marker.display()));
        let child = GroupChild::spawn(&mut command).unwrap();
        let mut grandchild = None;
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                grandchild = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let grandchild = grandchild.expect("grandchild started");
        drop(child);
        let pid = rustix::process::Pid::from_raw(grandchild).unwrap();
        for _ in 0..100 {
            if rustix::process::test_kill_process(pid).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("grandchild {grandchild} survived");
    }
}
