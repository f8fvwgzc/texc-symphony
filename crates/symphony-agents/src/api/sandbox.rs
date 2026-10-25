//! Confinement of the API agent loop's `shell` tool (`sandbox` / `network` of an `api` backend).
//!
//! * **bubblewrap** (Linux, the only production sandbox): only the system directories
//!   ([`SYSTEM_DIRS`], [`ETC_ENTRIES`]) are bound read-only, so `$HOME`, `/run`, `/var`, the data
//!   directory, other workspaces, credential files and filesystem sockets are not there; the
//!   workspace and a private `/tmp` are writable; every namespace is new (`--unshare-all`: user,
//!   PID with its own `/proc`, IPC, UTS, cgroup, and network unless allowed); a new session; the
//!   environment is cleared and rebuilt from an allow-list.
//! * **seatbelt** (macOS `sandbox-exec`, development only, never picked by `auto`): file writes
//!   only to the workspace and temp directories, network denied unless allowed, a few launchers
//!   denied by path. Reads are unrestricted and a copied launcher escapes it: not a security
//!   boundary.
//! * **none**: no confinement; needs `unconfined: true` on the backend.
//!
//! `auto` picks bubblewrap and refuses without it.

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
            Err(
                "the shell tool needs a sandbox: install bubblewrap (`bwrap`, Linux, with \
                 unprivileged user namespaces). For development on macOS set `sandbox: \
                 seatbelt` (not a security boundary); `sandbox: none` with `unconfined: true` \
                 runs commands with Symphony's own access"
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
pub fn seatbelt_profile(
    workspace: &Path,
    writable: &[PathBuf],
    network: bool,
    masked: &[PathBuf],
) -> String {
    let mut roots = vec![workspace.to_path_buf(), PathBuf::from("/private/tmp")];
    roots.extend(writable.iter().cloned());
    let subpaths: Vec<String> = roots
        .iter()
        .map(|root| format!("(subpath {})", sb_string(root)))
        .collect();
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    // Programs started through launchd/LaunchServices or AppleEvents run outside the sandbox.
    profile.push_str(
        "(deny mach-lookup (global-name-regex #\"^com\\.apple\\.coreservices\\.\") \
         (global-name \"com.apple.lsd.mapdb\") (global-name \"com.apple.lsd.modifydb\") \
         (global-name \"com.apple.coreservices.launchservicesd\"))\n\
         (deny appleevent-send)\n\
         (deny process-exec (literal \"/usr/bin/open\") (literal \"/usr/bin/osascript\") \
         (literal \"/bin/launchctl\") (literal \"/usr/bin/automator\"))\n",
    );
    profile.push_str(&format!(
        "(allow file-write* {} (literal \"/dev/null\") (literal \"/dev/tty\") \
         (literal \"/dev/stdout\") (literal \"/dev/stderr\") (regex #\"^/dev/fd/\"))\n",
        subpaths.join(" ")
    ));
    if !network {
        profile.push_str("(deny network*)\n");
    }
    for file in masked {
        profile.push_str(&format!(
            "(deny file-read* (literal {}))\n",
            sb_string(file)
        ));
    }
    profile
}

/// System directories bound read-only into bubblewrap (when they exist).
pub const SYSTEM_DIRS: [&str; 9] = [
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/opt",
    "/nix/store",
];

/// The parts of `/etc` toolchains need (bound read-only when they exist); the rest of `/etc`
/// (service configs such as `/etc/symphony`) stays out.
pub const ETC_ENTRIES: [&str; 26] = [
    "/etc/alternatives",
    "/etc/bash.bashrc",
    "/etc/ca-certificates",
    "/etc/ca-certificates.conf",
    "/etc/environment",
    "/etc/gai.conf",
    "/etc/gitconfig",
    "/etc/group",
    "/etc/host.conf",
    "/etc/hostname",
    "/etc/hosts",
    "/etc/inputrc",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/localtime",
    "/etc/mime.types",
    "/etc/nsswitch.conf",
    "/etc/os-release",
    "/etc/passwd",
    "/etc/pki",
    "/etc/profile",
    "/etc/profile.d",
    "/etc/resolv.conf",
    "/etc/ssl",
    "/etc/timezone",
];

/// Variables copied from Symphony's environment into bubblewrap (everything else is cleared).
pub const PASSED_ENV: [&str; 6] = ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ"];

/// `bash -lc <script>` in `workspace` under `confinement`. stderr of bash goes to stdout (so
/// the model sees one stream in order); the wrapper's own stderr stays separate. `tmp` is the
/// command's private temp directory (seatbelt: the only writable temp dir besides
/// `/private/tmp`, so the shared `$TMPDIR` of Symphony stays out of reach).
pub fn command(
    confinement: &Confinement,
    workspace: &Path,
    script: &str,
    network: bool,
    tmp: Option<&Path>,
    masked: &[PathBuf],
    env: &[(String, String)],
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
            for dir in SYSTEM_DIRS.iter().chain(ETC_ENTRIES.iter()) {
                command.args(["--ro-bind-try", dir, dir]);
            }
            command
                .args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"])
                // Every namespace new (user, PID with its own /proc, IPC, UTS, cgroup, network)
                // and a new session; everything inside dies with the command.
                .args(["--unshare-all", "--new-session"]);
            if network {
                command.arg("--share-net");
            }
            command
                .args([
                    "--clearenv",
                    "--setenv",
                    "HOME",
                    "/tmp",
                    "--setenv",
                    "TMPDIR",
                    "/tmp",
                ])
                .arg("--bind")
                .arg(workspace)
                .arg(workspace);
            for name in PASSED_ENV {
                if let Some(value) = std::env::var_os(name) {
                    command.arg("--setenv").arg(name).arg(value);
                }
            }
            for (name, value) in env {
                command.args(["--setenv", name, value]);
            }
            // Secret files (the LLM key, ...) read as empty inside the sandbox.
            for file in masked.iter().filter(|f| f.is_file()) {
                command.args(["--ro-bind", "/dev/null"]).arg(file);
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
            let writable: Vec<PathBuf> = tmp.map(Path::to_path_buf).into_iter().collect();
            let mut command = Command::new(sandbox_exec);
            command
                .arg("-p")
                .arg(seatbelt_profile(workspace, &writable, network, masked))
                .args(["bash", "-lc"])
                .arg(&script);
            command
        }
    };
    if let (Some(tmp), Confinement::Seatbelt(_) | Confinement::None) = (tmp, confinement) {
        command.env("TMPDIR", tmp);
    }
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
        let profile = seatbelt_profile(
            Path::new("/w/MT-1"),
            &[PathBuf::from("/t")],
            false,
            &[PathBuf::from("/run/secrets/llm/api-key")],
        );
        assert!(profile.contains("(deny file-read* (literal \"/run/secrets/llm/api-key\"))"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(subpath \"/w/MT-1\")"));
        assert!(profile.contains("(subpath \"/t\")"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(deny appleevent-send)"));
        assert!(profile.contains("(literal \"/usr/bin/open\")"));
        assert!(profile.contains("com.apple.lsd.mapdb"));
        assert!(!seatbelt_profile(Path::new("/w"), &[], true, &[]).contains("network*"));
        assert_eq!(sb_string(Path::new("/a\"b")), "\"/a\\\"b\"");
    }

    #[test]
    fn bubblewrap_binds_only_system_dirs_and_clears_the_environment() {
        let argv = |network: bool| -> String {
            command(
                &Confinement::Bubblewrap(PathBuf::from("/usr/bin/bwrap")),
                Path::new("/w/MT-1"),
                "ls",
                network,
                None,
                &[],
                &[("GIT_AUTHOR_NAME".to_owned(), "Agent".to_owned())],
            )
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
        };
        let joined = argv(false);
        assert!(
            !joined.contains("--ro-bind / /"),
            "the host root is not bound"
        );
        assert!(joined.contains("--ro-bind-try /usr /usr"));
        assert!(joined.contains("--ro-bind-try /etc/ssl /etc/ssl"));
        assert!(!joined.contains("--ro-bind-try /etc /etc"));
        assert!(joined.contains("--unshare-all --new-session --clearenv"));
        assert!(!joined.contains("--share-net"));
        assert!(joined.contains("--bind /w/MT-1 /w/MT-1"));
        assert!(joined.contains("--setenv GIT_AUTHOR_NAME Agent"));
        assert!(joined.ends_with("-- bash -lc exec 2>&1\nls"));
        assert!(argv(true).contains("--unshare-all --new-session --share-net"));
    }

    #[test]
    fn auto_never_picks_seatbelt() {
        if cfg!(target_os = "macos") {
            let err = resolve(SandboxMode::Auto).unwrap_err();
            assert!(err.contains("sandbox: seatbelt"), "{err}");
        }
    }
}
