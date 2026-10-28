//! Process hardening for the orchestrator.
//!
//! Agents (the API loop's shell, Codex, Claude Code and their commands) run as Symphony's own user
//! unless isolation gives them another one. On Linux that user could read
//! `/proc/<symphony pid>/environ` and `/proc/<pid>/mem`, which hold the tracker token, the API
//! token and the LLM key. [`protect_secrets`] marks the process non-dumpable, which makes those
//! files root-owned. Children start dumpable again (the flag resets on `execve`), so this does
//! not change how they run. In the container image, PID 1 ([`crate::init`]) holds the same
//! environment and protects itself the same way; `tests/init.rs` checks it from child processes.

/// Makes this process's memory and environment unreadable to other processes of the same user
/// (Linux; a no-op elsewhere).
pub fn protect_secrets() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    Ok(())
}

/// Keeps files Symphony creates (the run-history database and logs, workspaces) from being
/// readable by group or others, so a task running as its own uid cannot read them. Set only
/// when `isolation` is configured, where foreign uids run; a no-op elsewhere.
#[cfg(unix)]
pub fn restrict_new_files() {
    // 0o077: new files lose every group and other permission bit.
    rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));
}

#[cfg(not(unix))]
pub fn restrict_new_files() {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn restrict_new_files_sets_an_owner_only_umask() {
        // Save the original mask (set a known one), apply ours, then read it back and restore.
        let original = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o022));
        restrict_new_files();
        let applied = rustix::process::umask(original);
        assert_eq!(applied.bits() & 0o777, 0o077);
    }
}
