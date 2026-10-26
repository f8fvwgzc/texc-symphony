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
