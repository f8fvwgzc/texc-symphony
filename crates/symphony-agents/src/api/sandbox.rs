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
//!   boundary, so it needs `unconfined: true` on the backend.
//! * **none**: no confinement; needs `unconfined: true` on the backend.
//!
//! `auto` picks bubblewrap and refuses without it.
//!
//! A task under `isolation.network.mode: deny` gets [`NetworkAccess::Proxy`] whatever the
//! backend's `network` says: bubblewrap keeps the network namespace empty and runs the command
//! under the egress shim (`symphony egress-shim`, see [`crate::egress`]) with the proxy's socket
//! directory and the shim bound in; seatbelt denies the network except `localhost:<proxy port>`;
//! `none` runs the command in the network jail ([`crate::egress::network_jail`], Linux; the
//! command is refused where the jail cannot run).

use std::path::{Path, PathBuf};

use symphony_codex::EgressRoute;
use symphony_codex::launch::find_executable;
use symphony_core::config::SandboxMode;
use tokio::process::Command;

use crate::egress::{SHIM_PORT, SHIM_SUBCOMMAND, network_jail, proxy_env};

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
            Err(
                "the shell tool needs a sandbox: install bubblewrap (`bwrap`, Linux, with \
                 unprivileged user namespaces). For development on macOS set `sandbox: \
                 seatbelt` with `unconfined: true` (not a security boundary); `sandbox: none` \
                 with `unconfined: true` runs commands with Symphony's own access"
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
/// `/private/tmp`, so the shared `$TMPDIR` of Symphony stays out of reach). `Err` when a
/// default-deny network cannot be enforced for an unconfined command.
pub fn command(
    confinement: &Confinement,
    workspace: &Path,
    script: &str,
    network: &NetworkAccess,
    tmp: Option<&Path>,
    masked: &[PathBuf],
    env: &[(String, String)],
) -> Result<Command, String> {
    let script = format!("exec 2>&1\n{script}");
    let mut command = match confinement {
        Confinement::None => {
            // Unconfined, but a task's default-deny network still holds: the command runs in the
            // network jail (the shim inside sets the proxy variables).
            let mut command = match network {
                NetworkAccess::Proxy { route, shim } => {
                    let jail = network_jail(route, shim.as_deref(), workspace)?;
                    let (program, args) = jail
                        .split_first()
                        .ok_or_else(|| "the network jail is empty".to_owned())?;
                    let mut command = Command::new(program);
                    command.args(args).arg("bash");
                    command
                }
                _ => Command::new("bash"),
            };
            command.arg("-lc").arg(&script);
            command
        }
        Confinement::Bubblewrap(bwrap) => {
            // The shim needs the proxy's socket and itself (both bound in below).
            let shim = match network {
                NetworkAccess::Proxy {
                    route,
                    shim: Some(shim),
                } => route.socket.as_ref().map(|socket| (socket, shim)),
                _ => None,
            };
            let mut command = Command::new(bwrap);
            for dir in SYSTEM_DIRS.iter().chain(ETC_ENTRIES.iter()) {
                command.args(["--ro-bind-try", dir, dir]);
            }
            command
                .args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"])
                // Every namespace new (user, PID with its own /proc, IPC, UTS, cgroup, network)
                // and a new session; everything inside dies with the command.
                .args(["--unshare-all", "--new-session"]);
            if *network == NetworkAccess::Allowed {
                command.arg("--share-net");
            }
            if let Some((socket, shim)) = shim {
                // After `--tmpfs /tmp`, which would hide a socket directory under /tmp.
                if let Some(dir) = socket.parent() {
                    command.arg("--bind").arg(dir).arg(dir);
                }
                command.arg("--ro-bind").arg(shim).arg(shim);
            }
            command.arg("--bind").arg(workspace).arg(workspace);
            // The environment is rebuilt from an allow-list as bwrap's own (it passes it on), not
            // with `--setenv`: bwrap's argv, and the values in it, are world-readable in
            // `/proc/<pid>/cmdline`. Variables the caller adds later (a task's token) get the
            // same treatment.
            command
                .env_clear()
                .env("HOME", "/tmp")
                .env("TMPDIR", "/tmp");
            for name in PASSED_ENV {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            for (name, value) in env {
                command.env(name, value);
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
    if let (NetworkAccess::Proxy { route, .. }, Confinement::Seatbelt(_)) = (network, confinement) {
        for (name, value) in proxy_env(&route.proxy_url()) {
            command.env(name, value);
        }
    }
    command.current_dir(workspace);
    Ok(command)
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
    fn bubblewrap_binds_only_system_dirs_and_clears_the_environment() {
        let argv = |network: NetworkAccess| -> String {
            command(
                &Confinement::Bubblewrap(PathBuf::from("/usr/bin/bwrap")),
                Path::new("/w/MT-1"),
                "ls",
                &network,
                None,
                &[],
                &[("GIT_AUTHOR_NAME".to_owned(), "Agent".to_owned())],
            )
            .unwrap()
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
        };
        let joined = argv(NetworkAccess::Denied);
        assert!(
            !joined.contains("--ro-bind / /"),
            "the host root is not bound"
        );
        assert!(joined.contains("--ro-bind-try /usr /usr"));
        assert!(joined.contains("--ro-bind-try /etc/ssl /etc/ssl"));
        assert!(!joined.contains("--ro-bind-try /etc /etc"));
        assert!(joined.contains("--unshare-all --new-session --bind /w/MT-1 /w/MT-1"));
        assert!(!joined.contains("--share-net"));
        assert!(
            !joined.contains("--setenv"),
            "no values in the world-readable argv"
        );
        assert!(joined.ends_with("-- bash -lc exec 2>&1\nls"));
        assert!(argv(NetworkAccess::Allowed).contains("--unshare-all --new-session --share-net"));
        let command = command(
            &Confinement::Bubblewrap(PathBuf::from("/usr/bin/bwrap")),
            Path::new("/w"),
            "ls",
            &NetworkAccess::Denied,
            None,
            &[],
            &[("GIT_AUTHOR_NAME".to_owned(), "Agent".to_owned())],
        )
        .unwrap();
        let envs: Vec<(String, Option<String>)> = command
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for (name, value) in [
            ("GIT_AUTHOR_NAME", "Agent"),
            ("HOME", "/tmp"),
            ("TMPDIR", "/tmp"),
        ] {
            assert!(
                envs.contains(&(name.to_owned(), Some(value.to_owned()))),
                "{name}: {envs:?}"
            );
        }
    }

    #[test]
    fn auto_never_picks_seatbelt() {
        if cfg!(target_os = "macos") {
            let err = resolve(SandboxMode::Auto).unwrap_err();
            assert!(err.contains("sandbox: seatbelt"), "{err}");
        }
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
            &[],
        )
        .unwrap();
        let joined = args(&command).join(" ");
        assert!(!joined.contains("--share-net"), "{joined}");
        assert!(
            joined.contains("--ro-bind /usr/local/bin/symphony /usr/local/bin/symphony"),
            "the shim is reachable inside: {joined}"
        );
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
            let joined = args(
                &super::command(
                    &Confinement::Bubblewrap(PathBuf::from("/usr/bin/bwrap")),
                    Path::new("/w"),
                    "ls",
                    &network,
                    None,
                    &[],
                    &[],
                )
                .unwrap(),
            )
            .join(" ");
            assert!(!joined.contains("--share-net"), "{joined}");
            assert!(!joined.contains("egress-shim"), "{joined}");
        }
    }

    #[test]
    fn an_unconfined_command_with_a_default_deny_network_runs_in_the_network_jail() {
        let network = NetworkAccess::Proxy {
            route: route(Some("/tmp/symphony-egress-1-0/proxy.sock")),
            shim: Some(PathBuf::from("/usr/local/bin/symphony")),
        };
        let command = command(
            &Confinement::None,
            Path::new("/w/MT-1"),
            "git push",
            &network,
            None,
            &[],
            &[],
        );
        if !cfg!(target_os = "linux") || find_executable("bwrap").is_none() {
            // No jail here: refused, never run with proxy variables alone.
            assert!(command.unwrap_err().contains("bubblewrap"));
            return;
        }
        let joined = args(&command.unwrap()).join(" ");
        assert!(
            joined.contains(
                "--unshare-net --die-with-parent --chdir /w/MT-1 -- \
                 /usr/local/bin/symphony egress-shim"
            ),
            "{joined}"
        );
        assert!(
            joined.ends_with("-- bash -lc exec 2>&1\ngit push"),
            "{joined}"
        );
        // Without a shim there is nothing to relay through: refused too.
        let no_shim = NetworkAccess::Proxy {
            route: route(Some("/tmp/x/proxy.sock")),
            shim: None,
        };
        assert!(
            super::command(
                &Confinement::None,
                Path::new("/w"),
                "ls",
                &no_shim,
                None,
                &[],
                &[]
            )
            .is_err()
        );
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
            &[],
        )
        .unwrap();
        let envs: Vec<(String, String)> = command
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v?.to_str()?.to_owned())))
            .collect();
        assert!(envs.contains(&("HTTPS_PROXY".into(), "http://127.0.0.1:41234".into())));
    }
}
