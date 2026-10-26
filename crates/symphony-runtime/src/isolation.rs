//! Per-task isolation of task runs (`isolation:` in WORKFLOW.md); tracker issues are unaffected.
//!
//! | Guarantee | Enforcement |
//! |---|---|
//! | Own uid per task (`task_uids`) | the workspace (`0700`) and `<workspace>/.home` belong to the task uid; the clone, Codex, Claude Code and the api shell run as it; the api file tools hand what they write to it. Needs root: without it the run fails with `isolation_unavailable`. |
//! | Short-lived repository credentials (`github_app`) | a GitHub App installation token scoped to the task's repository ([`crate::credentials`]), given to the clone and the agent through the environment only (never argv), redacted from the run's events; the operator's own tokens stay stripped. |
//! | Default-deny network (`network.mode: deny`) | an [`EgressProxy`] allowing only the repository's git host on 443, the LLM gateway and `network.allow`. No task process can bypass it: the api shell (bubblewrap: empty network namespace + shim; seatbelt: everything but the proxy port denied) and Codex, Claude Code and an unconfined shell in the network jail (`symphony_agents::egress::network_jail`); where the jail cannot run they do not start. |
//!
//! Every task process gets `HOME=<workspace>/.home`, so the operator's `~/.gitconfig`, `gh` login
//! or agent logins are never visible to a task. SSH workers are not supported.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Checks, before any work, that `isolation` can apply to a run on `worker_host`: no SSH worker,
/// and root for `task_uids` (switching and chowning to other users needs it).
pub fn preflight(isolation: &IsolationSettings, worker_host: Option<&str>) -> Result<(), RunError> {
    if let Some(host) = worker_host {
        return Err(unavailable(format!(
            "task isolation is not supported on SSH workers (worker_host={host})"
        )));
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
    ) -> Result<Self, RunError> {
        let run_as = isolation.task_uids.map(|uids| {
            let id = uids.for_task(task.id);
            (id, id)
        });
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
