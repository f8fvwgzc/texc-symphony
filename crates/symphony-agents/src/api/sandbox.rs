//! Confinement of the API agent loop's `shell` tool (`sandbox` / `network` of an `api` backend).
//!
//! * **bubblewrap** (Linux): the root file system read-only, the workspace and a private `/tmp`
//!   writable, a fresh `/dev` and `/proc`, no network (`--unshare-net`) unless allowed.
//! * **seatbelt** (macOS `sandbox-exec`): everything allowed except file writes outside the
//!   workspace, `/private/tmp`, `$TMPDIR` and a few device files; network denied unless allowed.
//! * **none**: no confinement beyond the workspace cwd and the filtered environment.
//!
//! `auto` picks bubblewrap or seatbelt when installed and refuses otherwise: running commands of
//! a remote model unconfined has to be an explicit `sandbox: none`.

use std::path::{Path, PathBuf};

use symphony_codex::launch::find_executable;
use symphony_core::config::SandboxMode;
use tokio::process::Command;

/// The confinement a command runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confinement {
    /// `bwrap` at this path.
    Bubblewrap(PathBuf),
    /// `sandbox-exec` at this path.
    Seatbelt(PathBuf),
    /// Unconfined.
    None,
}

fn seatbelt_binary() -> Option<PathBuf> {
    let system = Path::new("/usr/bin/sandbox-exec");
    if system.is_file() {
        return Some(system.to_path_buf());
    }
    find_executable("sandbox-exec")
}

/// Resolves `mode` on this host; `Err` explains what to install or configure.
pub fn resolve(mode: SandboxMode) -> Result<Confinement, String> {
    match mode {
        SandboxMode::None => Ok(Confinement::None),
        SandboxMode::Bubblewrap => find_executable("bwrap")
            .map(Confinement::Bubblewrap)
            .ok_or_else(|| "sandbox `bubblewrap` is configured but `bwrap` is not on PATH".into()),
        SandboxMode::Seatbelt => seatbelt_binary().map(Confinement::Seatbelt).ok_or_else(|| {
            "sandbox `seatbelt` is configured but `sandbox-exec` was not found".into()
        }),
        SandboxMode::Auto => {
            if cfg!(target_os = "linux")
                && let Some(bwrap) = find_executable("bwrap")
            {
                return Ok(Confinement::Bubblewrap(bwrap));
            }
            if cfg!(target_os = "macos")
                && let Some(seatbelt) = seatbelt_binary()
            {
                return Ok(Confinement::Seatbelt(seatbelt));
            }
            Err(
                "the shell tool needs a sandbox: install bubblewrap (`bwrap`, Linux) or use \
                 macOS sandbox-exec, or set `sandbox: none` on the backend to run commands \
                 unconfined (for example inside a container that is the sandbox)"
                    .into(),
            )
        }
    }
}

/// Escapes `path` for a seatbelt string literal.
fn sb_string(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The seatbelt profile for `workspace` (`writable` adds further writable directories).
pub fn seatbelt_profile(workspace: &Path, writable: &[PathBuf], network: bool) -> String {
    let mut roots = vec![workspace.to_path_buf(), PathBuf::from("/private/tmp")];
    roots.extend(writable.iter().cloned());
    let subpaths: Vec<String> = roots
        .iter()
        .map(|root| format!("(subpath {})", sb_string(root)))
        .collect();
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    profile.push_str(&format!(
        "(allow file-write* {} (literal \"/dev/null\") (literal \"/dev/tty\") \
         (literal \"/dev/stdout\") (literal \"/dev/stderr\") (regex #\"^/dev/fd/\"))\n",
        subpaths.join(" ")
    ));
    if !network {
        profile.push_str("(deny network*)\n");
    }
    profile
}

/// The temp directory commands may write to besides `/private/tmp` (`$TMPDIR`, canonical).
fn temp_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("TMPDIR")?;
    std::fs::canonicalize(dir).ok()
}

/// `bash -lc <script>` in `workspace` under `confinement`. stderr of bash goes to stdout (so
/// the model sees one stream in order); the wrapper's own stderr stays separate.
pub fn command(
    confinement: &Confinement,
    workspace: &Path,
    script: &str,
    network: bool,
) -> Command {
    let script = format!("exec 2>&1\n{script}");
    let mut command = match confinement {
        Confinement::None => {
            let mut command = Command::new("bash");
            command.arg("-lc").arg(&script);
            command
        }
        Confinement::Bubblewrap(bwrap) => {
            let mut command = Command::new(bwrap);
            command
                .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"])
                .args(["--tmpfs", "/tmp"])
                .arg("--bind")
                .arg(workspace)
                .arg(workspace);
            if !network {
                command.arg("--unshare-net");
            }
            command
                .arg("--die-with-parent")
                .arg("--chdir")
                .arg(workspace)
                .args(["--", "bash", "-lc"])
                .arg(&script);
            command
        }
        Confinement::Seatbelt(sandbox_exec) => {
            let writable: Vec<PathBuf> = temp_dir().into_iter().collect();
            let mut command = Command::new(sandbox_exec);
            command
                .arg("-p")
                .arg(seatbelt_profile(workspace, &writable, network))
                .args(["bash", "-lc"])
                .arg(&script);
            command
        }
    };
    command.current_dir(workspace);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_never_needs_a_binary() {
        assert_eq!(resolve(SandboxMode::None), Ok(Confinement::None));
    }

    #[test]
    fn seatbelt_profiles_confine_writes_and_network() {
        let profile = seatbelt_profile(Path::new("/w/MT-1"), &[PathBuf::from("/t")], false);
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(subpath \"/w/MT-1\")"));
        assert!(profile.contains("(subpath \"/t\")"));
        assert!(profile.contains("(deny network*)"));
        assert!(!seatbelt_profile(Path::new("/w"), &[], true).contains("network"));
        assert_eq!(sb_string(Path::new("/a\"b")), "\"/a\\\"b\"");
    }

    #[test]
    fn bubblewrap_commands_bind_the_workspace_and_drop_the_network() {
        let command = command(
            &Confinement::Bubblewrap(PathBuf::from("/usr/bin/bwrap")),
            Path::new("/w/MT-1"),
            "ls",
            false,
        );
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.starts_with("--ro-bind / / --dev /dev --proc /proc --tmpfs /tmp"));
        assert!(joined.contains("--bind /w/MT-1 /w/MT-1 --unshare-net"));
        assert!(joined.ends_with("-- bash -lc exec 2>&1\nls"));
    }
}
