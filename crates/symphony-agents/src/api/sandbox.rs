//! Confinement of the API agent loop's `shell` tool (`sandbox` / `network` of an `api` backend).
//!
//! * **bubblewrap** (Linux): the root file system read-only, the workspace and a private `/tmp`
//!   writable, a fresh `/dev`, a private PID namespace with its own `/proc` (host processes and
//!   their environments are invisible; everything dies with the command), a new session, and no
//!   network (`--unshare-net`) unless allowed.
//! * **seatbelt** (macOS `sandbox-exec`): file writes only to the workspace, `/private/tmp`, a
//!   private per-command temp directory (`$TMPDIR` for the command) and a few device files; no
//!   LaunchServices, AppleEvents, `open`, `osascript` or `launchctl` (which would start programs
//!   outside the sandbox); network denied unless allowed.
//! * **none**: no confinement beyond the workspace cwd and the filtered environment.
//!
//! `auto` picks bubblewrap or seatbelt when installed and refuses otherwise: running commands of
//! a remote model unconfined has to be an explicit `sandbox: none`.
//!
//! A task under `isolation.network.mode: deny` gets [`NetworkAccess::Proxy`] whatever the
//! backend's `network` says: bubblewrap keeps `--unshare-net` and runs the command under the
//! egress shim (`symphony egress-shim`, see [`crate::egress`]) with the proxy's socket directory
//! bound in; seatbelt denies the network except `localhost:<proxy port>`; `none` only sets the
//! proxy variables (advisory).

use std::path::{Path, PathBuf};

use symphony_codex::EgressRoute;
use symphony_codex::launch::find_executable;
use symphony_core::config::SandboxMode;
use tokio::process::Command;

use crate::egress::{SHIM_PORT, SHIM_SUBCOMMAND, proxy_env};

/// What network a command may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkAccess {
    /// None.
    Denied,
    /// Unrestricted (`network: true` on the backend, no isolation).
    Allowed,
    /// Only the task's egress proxy.
    Proxy {
        /// Where the proxy listens.
        route: EgressRoute,
        /// The executable whose `egress-shim` subcommand relays inside bubblewrap (`None`:
        /// bubblewrap commands get no network at all).
        shim: Option<PathBuf>,
    },
}

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
pub fn seatbelt_profile(
    workspace: &Path,
    writable: &[PathBuf],
    network: &NetworkAccess,
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
    match network {
        NetworkAccess::Allowed => {}
        NetworkAccess::Denied => profile.push_str("(deny network*)\n"),
        NetworkAccess::Proxy { route, .. } => {
            // Later rules win: everything is denied except the proxy's loopback port.
            profile.push_str(&format!(
                "(deny network*)\n(allow network-outbound (remote ip \"localhost:{}\"))\n",
                route.tcp.port()
            ));
        }
    }
    for file in masked {
        profile.push_str(&format!(
            "(deny file-read* (literal {}))\n",
            sb_string(file)
        ));
    }
    profile
}

/// `bash -lc <script>` in `workspace` under `confinement`. stderr of bash goes to stdout (so
/// the model sees one stream in order); the wrapper's own stderr stays separate. `tmp` is the
/// command's private temp directory (seatbelt: the only writable temp dir besides
/// `/private/tmp`, so the shared `$TMPDIR` of Symphony stays out of reach).
pub fn command(
    confinement: &Confinement,
    workspace: &Path,
    script: &str,
    network: &NetworkAccess,
    tmp: Option<&Path>,
    masked: &[PathBuf],
) -> Command {
    let script = format!("exec 2>&1\n{script}");
    let mut command = match confinement {
        Confinement::None => {
            let mut command = Command::new("bash");
            command.arg("-lc").arg(&script);
            command
        }
        Confinement::Bubblewrap(bwrap) => {
            // The shim needs the proxy's socket (bound in below) and itself.
            let shim = match network {
                NetworkAccess::Proxy {
                    route,
                    shim: Some(shim),
                } => route.socket.as_ref().map(|socket| (socket, shim)),
                _ => None,
            };
            let mut command = Command::new(bwrap);
            command
                .args(["--ro-bind", "/", "/", "--dev", "/dev"])
                // A PID namespace (needed for `--proc` without setuid, and it hides host
                // processes) and a new session; everything inside dies with the command.
                .args(["--unshare-pid", "--new-session", "--proc", "/proc"])
                .args(["--tmpfs", "/tmp", "--setenv", "TMPDIR", "/tmp"])
                .arg("--bind")
                .arg(workspace)
                .arg(workspace);
            if *network != NetworkAccess::Allowed {
                command.arg("--unshare-net");
            }
            if let Some(dir) = shim.and_then(|(socket, _)| socket.parent()) {
                // After `--tmpfs /tmp`, which would hide a socket directory under /tmp.
                command.arg("--bind").arg(dir).arg(dir);
            }
            // Secret files (the LLM key, ...) read as empty inside the sandbox.
            for file in masked.iter().filter(|f| f.is_file()) {
                command.args(["--ro-bind", "/dev/null"]).arg(file);
            }
            command
                .arg("--die-with-parent")
                .arg("--chdir")
                .arg(workspace)
                .arg("--");
            if let Some((socket, shim)) = shim {
                command
                    .arg(shim)
                    .arg(SHIM_SUBCOMMAND)
                    .arg("--socket")
                    .arg(socket)
                    .arg("--port")
                    .arg(SHIM_PORT.to_string())
                    .arg("--");
            }
            command.args(["bash", "-lc"]).arg(&script);
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
    if let (NetworkAccess::Proxy { route, .. }, Confinement::Seatbelt(_) | Confinement::None) =
        (network, confinement)
    {
        for (name, value) in proxy_env(&route.proxy_url()) {
            command.env(name, value);
        }
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
            &NetworkAccess::Denied,
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
        assert!(
            !seatbelt_profile(Path::new("/w"), &[], &NetworkAccess::Allowed, &[])
                .contains("network*")
        );
        assert_eq!(sb_string(Path::new("/a\"b")), "\"/a\\\"b\"");
    }

    #[test]
    fn bubblewrap_commands_bind_the_workspace_and_drop_the_network() {
        let command = command(
            &Confinement::Bubblewrap(PathBuf::from("/usr/bin/bwrap")),
            Path::new("/w/MT-1"),
            "ls",
            &NetworkAccess::Denied,
            None,
            &[],
        );
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.starts_with(
            "--ro-bind / / --dev /dev --unshare-pid --new-session --proc /proc --tmpfs /tmp"
        ));
        assert!(joined.contains("--bind /w/MT-1 /w/MT-1 --unshare-net"));
        assert!(joined.ends_with("-- bash -lc exec 2>&1\nls"));
    }

    fn route(socket: Option<&str>) -> EgressRoute {
        EgressRoute {
            socket: socket.map(PathBuf::from),
            tcp: "127.0.0.1:41234".parse().unwrap(),
        }
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_proxied_bubblewrap_command_runs_under_the_shim_without_a_network() {
        let network = NetworkAccess::Proxy {
            route: route(Some("/tmp/symphony-egress-1-0/proxy.sock")),
            shim: Some(PathBuf::from("/usr/local/bin/symphony")),
        };
        let command = command(
            &Confinement::Bubblewrap(PathBuf::from("/usr/bin/bwrap")),
            Path::new("/w/MT-1"),
            "git push",
            &network,
            None,
            &[],
        );
        let joined = args(&command).join(" ");
        assert!(joined.contains("--unshare-net"), "{joined}");
        let tmpfs = joined.find("--tmpfs /tmp").unwrap();
        let bind = joined
            .find("--bind /tmp/symphony-egress-1-0 /tmp/symphony-egress-1-0")
            .expect("the socket directory is bound in");
        assert!(bind > tmpfs, "bound after the private /tmp: {joined}");
        assert!(
            joined.ends_with(
                "-- /usr/local/bin/symphony egress-shim --socket \
                 /tmp/symphony-egress-1-0/proxy.sock --port 3128 -- bash -lc exec 2>&1\ngit push"
            ),
            "{joined}"
        );
        // No proxy variables outside: the shim sets them inside the sandbox.
        assert!(
            command
                .as_std()
                .get_envs()
                .all(|(name, _)| name != "HTTPS_PROXY")
        );
        // Without a shim (or a socket) the command just has no network.
        for network in [
            NetworkAccess::Proxy {
                route: route(Some("/tmp/x/proxy.sock")),
                shim: None,
            },
            NetworkAccess::Proxy {
                route: route(None),
                shim: Some(PathBuf::from("/s")),
            },
        ] {
            let joined = args(&super::command(
                &Confinement::Bubblewrap(PathBuf::from("/usr/bin/bwrap")),
                Path::new("/w"),
                "ls",
                &network,
                None,
                &[],
            ))
            .join(" ");
            assert!(joined.contains("--unshare-net"), "{joined}");
            assert!(!joined.contains("egress-shim"), "{joined}");
        }
    }

    #[test]
    fn a_proxied_seatbelt_profile_allows_only_the_proxy_port() {
        let network = NetworkAccess::Proxy {
            route: route(None),
            shim: None,
        };
        let profile = seatbelt_profile(Path::new("/w"), &[], &network, &[]);
        assert!(profile.contains("(deny network*)"), "{profile}");
        assert!(
            profile.contains("(allow network-outbound (remote ip \"localhost:41234\"))"),
            "{profile}"
        );
        let command = command(
            &Confinement::Seatbelt(PathBuf::from("/usr/bin/sandbox-exec")),
            Path::new("/w"),
            "ls",
            &network,
            None,
            &[],
        );
        let envs: Vec<(String, String)> = command
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v?.to_str()?.to_owned())))
            .collect();
        assert!(envs.contains(&("HTTPS_PROXY".into(), "http://127.0.0.1:41234".into())));
    }
}
