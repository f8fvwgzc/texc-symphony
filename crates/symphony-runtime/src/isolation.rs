//! Per-task isolation of task runs (`isolation:` in WORKFLOW.md); tracker issues are unaffected.
//!
//! | Guarantee | Enforcement |
//! |---|---|
//! | Own uid per task (`task_uids`) | each run leases a free uid from the range ([`TaskUidPool`]), so no two live runs share one and no uid with a leftover process is reused; the workspace (`0700`) and `<workspace>/.home` belong to it; the clone, Codex, Claude Code and the api shell run as it; the api file tools hand what they write to it. At the run's end the uid's leftover processes are killed ([`kill_uid_processes`]). Needs root: without it the run fails with `isolation_unavailable`. |
//! | Short-lived repository credentials (`github_app`) | a GitHub App installation token scoped to the task's repository ([`crate::credentials`]), given to the clone and the agent through the environment only (never argv), redacted from the run's events and errors; the operator's own tokens stay stripped. The App key itself never reaches an agent: it is masked wherever agents run under a default-deny network, and otherwise `task_uids` with a key no task uid can read is required. |
//! | Default-deny network (`network.mode: deny`) | an [`EgressProxy`] allowing only the repository's git host on 443, the LLM gateway and `network.allow`. No task process can bypass it: the api shell (bubblewrap: empty network namespace + shim; seatbelt: everything but the proxy port denied) and Codex, Claude Code and an unconfined shell in the network jail (`symphony_agents::egress::network_jail`); where the jail cannot run they do not start. |
//!
//! Every task process gets `HOME=<workspace>/.home`, so the operator's `~/.gitconfig`, `gh` login
//! or agent logins are never visible to a task. SSH workers are not supported.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use symphony_agents::egress::{AllowList, EgressProxy};
use symphony_codex::{EventSink, StartOptions};
use symphony_core::Settings;
use symphony_core::config::IsolationSettings;

use crate::credentials::{TaskCredentials, mint_task_credentials};
use crate::redact::Redactor;
use crate::worker::{RunError, TaskSpec};

/// The task's home directory, relative to its workspace.
pub const HOME_DIR: &str = ".home";

fn unavailable(message: impl Into<String>) -> RunError {
    RunError::IsolationUnavailable(message.into())
}

/// Assigns each isolated task its own uid from `isolation.task_uids`. A task leases a free uid
/// for the length of its run instead of the old `first + (id mod count)` mapping, so no two live
/// runs share a uid and no uid with a live process is reused. "In use" is found by scanning
/// `/proc`, so a uid held by another replica on the same host counts too.
#[derive(Debug)]
pub struct TaskUidPool {
    first: u32,
    count: u32,
    leased: Mutex<HashSet<u32>>,
}

/// A uid held for one run; dropping it frees the uid for the next task.
#[derive(Debug)]
pub struct TaskUidLease {
    uid: u32,
    pool: Arc<TaskUidPool>,
}

impl TaskUidPool {
    /// A pool over `uids`'s range.
    pub fn new(uids: symphony_core::config::TaskUids) -> Arc<Self> {
        Arc::new(Self {
            first: uids.first,
            count: uids.count,
            leased: Mutex::new(HashSet::new()),
        })
    }

    /// Leases a uid no live run holds and no process currently runs as. `Err` when every uid in
    /// the range is busy: the task is refused rather than sharing a uid with another tenant.
    pub fn lease(self: &Arc<Self>) -> Result<TaskUidLease, RunError> {
        let mut leased = self.leased.lock().unwrap_or_else(PoisonError::into_inner);
        let busy = uids_in_use(self.first, self.count);
        let end = self.first.saturating_add(self.count);
        for uid in self.first..end {
            if !leased.contains(&uid) && !busy.contains(&uid) {
                leased.insert(uid);
                return Ok(TaskUidLease {
                    uid,
                    pool: Arc::clone(self),
                });
            }
        }
        Err(unavailable(format!(
            "no free task uid in {}..{end}: every uid in isolation.task_uids is busy (a live run \
             or a leftover process)",
            self.first
        )))
    }
}

impl TaskUidLease {
    /// The leased uid (used as both uid and gid).
    pub fn uid(&self) -> u32 {
        self.uid
    }
}

impl Drop for TaskUidLease {
    fn drop(&mut self) {
        self.pool
            .leased
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.uid);
    }
}

/// The uids in `first..first+count` that a live process runs as (Linux; empty elsewhere).
#[cfg(target_os = "linux")]
fn uids_in_use(first: u32, count: u32) -> HashSet<u32> {
    use std::os::unix::fs::MetadataExt;
    let end = first.saturating_add(count);
    let mut set = HashSet::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return set;
    };
    for entry in entries.flatten() {
        // Numeric entries are processes; `/proc/<pid>` is owned by the process's uid.
        if entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let uid = meta.uid();
            if (first..end).contains(&uid) {
                set.insert(uid);
            }
        }
    }
    set
}

#[cfg(not(target_os = "linux"))]
fn uids_in_use(_first: u32, _count: u32) -> HashSet<u32> {
    HashSet::new()
}

/// SIGKILLs every process running as `uid` (not PID 1), so a task's leftovers (a `setsid`
/// daemon outside the jail, anything under `sandbox: none`) cannot reach the next task that
/// leases the uid. Needs root; returns how many it signalled. A no-op off Linux.
#[cfg(target_os = "linux")]
pub fn kill_uid_processes(uid: u32) -> usize {
    use std::os::unix::fs::MetadataExt;
    let mut killed = 0;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        if entry.metadata().map(|meta| meta.uid()) == Ok(uid)
            && let Some(pid) = rustix::process::Pid::from_raw(pid)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            killed += 1;
        }
    }
    killed
}

/// Non-Linux stub for [`kill_uid_processes`].
#[cfg(not(target_os = "linux"))]
pub fn kill_uid_processes(_uid: u32) -> usize {
    0
}

/// Checks, before any work, that `isolation` can apply to a run on `worker_host`: no SSH worker,
/// a repository on an operator-allowed git host, a GitHub App key no agent can read, and root
/// for `task_uids` (switching and chowning to other users needs it).
pub fn preflight(
    isolation: &IsolationSettings,
    worker_host: Option<&str>,
    repo: Option<&str>,
) -> Result<(), RunError> {
    if let Some(host) = worker_host {
        return Err(unavailable(format!(
            "task isolation is not supported on SSH workers (worker_host={host})"
        )));
    }
    // The submitter chooses `repo`; in deny mode it must be a host the operator allowed, or the
    // task could reach (or exfiltrate to) any host, including internal services (F4).
    if let Err(host) = isolation.check_repo_host(repo) {
        return Err(unavailable(format!(
            "repository host {host:?} is not in isolation.network.git_hosts (or \
             isolation.github_app.git_host); the operator must allow it"
        )));
    }
    if let Some(app) = &isolation.github_app {
        app_key_out_of_reach(isolation, app)?;
    }
    if isolation.task_uids.is_some() {
        #[cfg(unix)]
        {
            let euid = rustix::process::geteuid();
            if !euid.is_root() {
                return Err(unavailable(format!(
                    "isolation.task_uids needs Symphony to run as root (CAP_SETUID, CAP_SETGID \
                     and CAP_CHOWN); it runs as uid {}",
                    euid.as_raw()
                )));
            }
        }
        #[cfg(not(unix))]
        return Err(unavailable("isolation.task_uids needs a Unix host"));
    }
    Ok(())
}

/// The GitHub App key mints tokens for every repository the App is installed on, so no agent may
/// read it: the task runs as its own uid, and the key must be readable by its owner only (it is
/// also masked wherever agents run under a default-deny network).
fn app_key_out_of_reach(
    isolation: &IsolationSettings,
    app: &symphony_core::config::GitHubAppSettings,
) -> Result<(), RunError> {
    if isolation.task_uids.is_none() {
        return Err(unavailable(
            "isolation.github_app needs isolation.task_uids: an agent running as Symphony's \
             user could read the App key and mint tokens for every repository of the App",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let key = &app.private_key_file;
        let mode = std::fs::metadata(key)
            .map_err(|err| unavailable(format!("cannot inspect {}: {err}", key.display())))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(unavailable(format!(
                "{} must not be readable by group or others (mode 0400 or 0600): the task users \
                 could read it",
                key.display()
            )));
        }
    }
    Ok(())
}

/// Changes the owner of `path` itself (never a symlink's target).
#[cfg(unix)]
fn chown(path: &Path, (uid, gid): (u32, u32)) -> Result<(), RunError> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
        .map_err(|err| unavailable(format!("cannot chown {}: {err}", path.display())))
}

#[cfg(not(unix))]
fn chown(path: &Path, _owner: (u32, u32)) -> Result<(), RunError> {
    Err(unavailable(format!("cannot chown {}", path.display())))
}

#[cfg(unix)]
fn make_private(path: &Path) -> Result<(), RunError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|err| unavailable(format!("cannot restrict {}: {err}", path.display())))
}

#[cfg(not(unix))]
fn make_private(_path: &Path) -> Result<(), RunError> {
    Ok(())
}

/// A real directory at `path` (not a symlink), created `0700` when missing.
fn private_dir(path: &Path) -> Result<(), RunError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) => Err(unavailable(format!(
            "{} exists and is not a directory",
            path.display()
        ))),
        Err(_) => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(path)
                .map_err(|err| unavailable(format!("cannot create {}: {err}", path.display())))
        }
    }
}

/// What isolates one task run; dropping it stops the egress proxy.
#[derive(Debug)]
pub struct TaskIsolation {
    run_as: Option<(u32, u32)>,
    home: PathBuf,
    credentials: Option<TaskCredentials>,
    egress: Option<EgressProxy>,
}

impl TaskIsolation {
    /// Sets up isolation for `task` in `workspace` (which [`preflight`] allowed): hands the
    /// workspace to the task uid, mints the repository token and starts the egress proxy.
    pub async fn prepare(
        settings: &Settings,
        isolation: &IsolationSettings,
        task: &TaskSpec,
        workspace: &Path,
        run_as: Option<(u32, u32)>,
    ) -> Result<Self, RunError> {
        debug_assert_eq!(run_as.is_some(), isolation.task_uids.is_some());
        if let Some(owner) = run_as {
            let meta = std::fs::symlink_metadata(workspace).map_err(|err| {
                unavailable(format!("cannot inspect {}: {err}", workspace.display()))
            })?;
            if !meta.file_type().is_dir() {
                return Err(unavailable(format!(
                    "{} is not a directory",
                    workspace.display()
                )));
            }
            // Restricted while still ours (chmod of another user's file would need CAP_FOWNER).
            make_private(workspace)?;
            chown(workspace, owner)?;
        }
        let credentials = match (&isolation.github_app, &task.repo) {
            (Some(app), Some(repo)) => mint_task_credentials(app, repo)
                .await
                .map_err(RunError::TaskCredentials)?,
            _ => None,
        };
        let egress = if isolation.network.denies() {
            let allow = AllowList::new(
                isolation.egress_allow_list(task.repo.as_deref(), settings.llm.as_ref()),
            );
            let proxy = EgressProxy::start(allow, run_as)
                .await
                .map_err(|err| unavailable(format!("cannot start the egress proxy: {err}")))?;
            Some(proxy)
        } else {
            None
        };
        Ok(Self {
            run_as,
            home: workspace.join(HOME_DIR),
            credentials,
            egress,
        })
    }

    /// `(uid, gid)` the task's processes run as.
    pub fn run_as(&self) -> Option<(u32, u32)> {
        self.run_as
    }

    /// `<workspace>/.home`.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The task's repository token, when one was minted.
    pub fn credentials(&self) -> Option<&TaskCredentials> {
        self.credentials.as_ref()
    }

    /// The task's egress proxy (deny mode).
    pub fn egress(&self) -> Option<&EgressProxy> {
        self.egress.as_ref()
    }

    /// Variables every task process gets after the secrets are removed: `HOME` and the
    /// repository credentials.
    pub fn task_env(&self) -> Vec<(String, String)> {
        std::iter::once(("HOME".to_owned(), self.home.display().to_string()))
            .chain(self.credentials.iter().flat_map(TaskCredentials::env))
            .collect()
    }

    /// Makes `command` (the clone) run as the task with its environment. Call after the
    /// operator's secrets were removed from it.
    pub fn apply_to_command(&self, command: &mut tokio::process::Command) {
        for (name, value) in self.task_env() {
            command.env(name, value);
        }
        #[cfg(unix)]
        if let Some((uid, gid)) = self.run_as {
            command.uid(uid).gid(gid);
        }
    }

    /// After the clone: creates `.home` (`0700`, the task's) and, right after a fresh clone,
    /// keeps it out of git (`.git/info/exclude`).
    pub fn finish_workspace(&self, workspace: &Path, cloned: bool) -> Result<(), RunError> {
        private_dir(&self.home)?;
        if let Some(owner) = self.run_as {
            chown(&self.home, owner)?;
        }
        if cloned {
            exclude_home(workspace);
        }
        Ok(())
    }

    /// The agent's start options with the task's uid, environment and egress route.
    pub fn apply(&self, mut options: StartOptions) -> StartOptions {
        for (name, value) in self.task_env() {
            options = options.with_task_env(name, value);
        }
        options
            .with_run_as(self.run_as)
            .with_egress(self.egress.as_ref().map(EgressProxy::route))
    }

    /// A redactor for the task's own secret (the repository token), if it has one.
    pub fn redactor(&self) -> Option<Arc<Redactor>> {
        self.credentials
            .as_ref()
            .map(|c| Arc::new(Redactor::new([c.token().to_owned()])))
    }

    /// `events` with the task's secret redacted before they leave the run.
    pub fn redacted_events(&self, events: &EventSink) -> EventSink {
        match self.redactor() {
            Some(redactor) => events
                .clone()
                .with_filter(Arc::new(move |event| redactor.redact_event(event))),
            None => events.clone(),
        }
    }
}

/// Appends `/.home/` to the fresh clone's `.git/info/exclude` (best effort; never follows a
/// symlink).
fn exclude_home(workspace: &Path) {
    let info = workspace.join(".git").join("info");
    if !std::fs::symlink_metadata(&info).is_ok_and(|m| m.file_type().is_dir()) {
        return;
    }
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc_nofollow());
    }
    let written = options.open(info.join("exclude")).and_then(|mut file| {
        use std::io::Write;
        file.write_all(format!("\n/{HOME_DIR}/\n").as_bytes())
    });
    if let Err(err) = written {
        tracing::warn!("Cannot exclude {HOME_DIR} from git in the task workspace: {err}");
    }
}

#[cfg(unix)]
fn libc_nofollow() -> i32 {
    // The flag is a small positive constant; the cast cannot wrap.
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_core::config::TaskUids;

    /// A range starting well above any real uid, so `uids_in_use` finds nothing in it.
    fn free_range(count: u32) -> TaskUids {
        TaskUids {
            first: 4_000_000_000,
            count,
        }
    }

    #[test]
    fn the_pool_leases_distinct_uids_refuses_when_exhausted_and_frees_on_drop() {
        let pool = TaskUidPool::new(free_range(2));
        let a = pool.lease().unwrap();
        let b = pool.lease().unwrap();
        assert_ne!(a.uid(), b.uid());
        assert!((4_000_000_000..4_000_000_002).contains(&a.uid()));
        // Both uids are leased: the next task is refused rather than sharing one.
        let err = pool.lease().unwrap_err();
        assert!(
            matches!(&err, RunError::IsolationUnavailable(m) if m.contains("no free task uid"))
        );
        // Dropping a lease frees its uid for the next task.
        let freed = a.uid();
        drop(a);
        let c = pool.lease().unwrap();
        assert_eq!(c.uid(), freed);
        drop((b, c));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uids_in_use_sees_a_live_process_and_skips_it() {
        use std::os::unix::fs::MetadataExt;
        let me = std::fs::metadata("/proc/self").unwrap().uid();
        // A range around our own uid: this process is live, so our uid is "in use".
        assert!(uids_in_use(me, 1).contains(&me));
        // A pool over just our uid has nothing free (the only uid is busy).
        let pool = TaskUidPool::new(TaskUids {
            first: me,
            count: 1,
        });
        assert!(pool.lease().is_err());
        // A high range nobody uses is free.
        assert!(uids_in_use(4_000_000_000, 1).is_empty());
    }

    #[test]
    fn killing_an_unused_uid_signals_nothing() {
        // A uid with no processes: the /proc scan runs and signals nothing (safe without root).
        assert_eq!(kill_uid_processes(4_000_000_001), 0);
    }
}
