//! Process hardening for the orchestrator.
//!
//! Agents (the API loop's shell, Codex, Claude Code and their commands) run as Symphony's own user
//! unless isolation gives them another one. On Linux that user could read
//! `/proc/<symphony pid>/environ` and `/proc/<pid>/mem`, which hold the tracker token, the API
//! token and the LLM key. [`protect_secrets`] marks the process non-dumpable, which makes those
//! files root-owned. Children start dumpable again (the flag resets on `execve`), so this does
//! not change how they run.

/// Makes this process's memory and environment unreadable to other processes of the same user
/// (Linux; a no-op elsewhere).
pub fn protect_secrets() {
    #[cfg(target_os = "linux")]
    if let Err(err) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        tracing::warn!("Failed to mark the process non-dumpable: {err}");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_child_of_the_same_user_cannot_read_our_environment() {
        let path = format!("/proc/{}/environ", std::process::id());
        let before = std::process::Command::new("cat")
            .arg(&path)
            .output()
            .unwrap();
        assert!(before.status.success(), "readable before");
        protect_secrets();
        let after = std::process::Command::new("cat")
            .arg(&path)
            .output()
            .unwrap();
        assert!(!after.status.success(), "unreadable after");
    }
}
