//! `symphony egress-shim` end to end: the real binary relays a command's traffic to a task's
//! egress proxy, also from inside a bubblewrap sandbox without a network and from the network
//! jail Codex and Claude Code run in (Linux, when bubblewrap can create namespaces here; CI
//! requires it with `SYMPHONY_REQUIRE_SANDBOX`).

#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use symphony_agents::egress::{AllowList, EgressProxy};
use symphony_codex::launch::find_executable;
use symphony_core::config::HostPort;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SYMPHONY: &str = env!("CARGO_BIN_EXE_symphony");

async fn http_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                    )
                    .await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

async fn proxy_allowing(addr: SocketAddr) -> EgressProxy {
    EgressProxy::start(
        AllowList::new([HostPort::new("127.0.0.1", addr.port())]),
        None,
    )
    .await
    .unwrap()
}

/// Runs `command` off the async runtime (the proxy keeps serving meanwhile).
async fn output(mut command: Command) -> std::process::Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_relays_a_command_through_the_proxy() {
    let Some(curl) = find_executable("curl") else {
        eprintln!("skipped: curl is not installed");
        return;
    };
    let allowed = http_server().await;
    let other = http_server().await;
    let proxy = proxy_allowing(allowed).await;
    let shim = |url: String| {
        let mut command = Command::new(SYMPHONY);
        command
            .arg("egress-shim")
            .arg("--socket")
            .arg(proxy.socket().unwrap())
            .args(["--port", "0", "--"])
            .arg(&curl)
            .args(["-sS", "--fail", "--max-time", "10"])
            .arg(url);
        command
    };
    let out = output(shim(format!("http://{allowed}/"))).await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");

    let out = output(shim(format!("http://{other}/"))).await;
    assert_eq!(out.status.code(), Some(22), "refused with 403: {out:?}");
}

fn bwrap_usable(bwrap: &Path) -> bool {
    let usable = Command::new(bwrap)
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--unshare-pid",
            "--unshare-net",
        ])
        .args(["--proc", "/proc", "--", "true"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !usable && std::env::var_os("SYMPHONY_REQUIRE_SANDBOX").is_some_and(|v| !v.is_empty()) {
        panic!("SYMPHONY_REQUIRE_SANDBOX is set but bubblewrap cannot create namespaces here");
    }
    usable
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bubblewrap_shell_reaches_only_the_proxy() {
    use symphony_agents::api::sandbox::{self, Confinement, NetworkAccess};
    let (Some(bwrap), Some(_)) = (find_executable("bwrap"), find_executable("curl")) else {
        eprintln!("skipped: bwrap or curl is not installed");
        return;
    };
    if !bwrap_usable(&bwrap) {
        eprintln!("skipped: bubblewrap cannot create namespaces here");
        return;
    }
    let allowed = http_server().await;
    let other = http_server().await;
    let proxy = proxy_allowing(allowed).await;
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let network = NetworkAccess::Proxy {
        route: proxy.route(),
        shim: Some(PathBuf::from(SYMPHONY)),
    };
    let run = |script: String| {
        let command = sandbox::command(
            &Confinement::Bubblewrap(bwrap.clone()),
            &workspace,
            &script,
            &network,
            None,
            &[],
            &[],
        )
        .unwrap();
        output(command.into_std())
    };
    let curl = "curl -sS --fail --max-time 10";
    let out = run(format!("{curl} http://{allowed}/")).await;
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));
    let out = run(format!("{curl} http://{other}/")).await;
    assert!(!out.status.success(), "refused by the proxy: {out:?}");
    let out = run(format!("{curl} --noproxy '*' http://{other}/")).await;
    assert!(!out.status.success(), "no route around the proxy: {out:?}");
}

/// The confinement Codex and Claude Code run under when a task's network is default-deny: no
/// route out except the proxy (not `--noproxy`, not the address directly), and the api sandbox's
/// filesystem shape — the workspace is writable, but host files outside it, host `/proc` and
/// masked secrets are not reachable.
#[tokio::test(flavor = "multi_thread")]
async fn a_jailed_agent_is_confined_to_the_proxy_and_its_workspace() {
    let (Some(bwrap), Some(_)) = (find_executable("bwrap"), find_executable("curl")) else {
        eprintln!("skipped: bwrap or curl is not installed");
        return;
    };
    if !bwrap_usable(&bwrap) {
        eprintln!("skipped: bubblewrap cannot create namespaces here");
        return;
    }
    let allowed = http_server().await;
    let other = http_server().await;
    let proxy = proxy_allowing(allowed).await;
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    // A host secret outside the workspace and system dirs.
    let outside = tempfile::tempdir().unwrap();
    let host_secret = std::fs::canonicalize(outside.path())
        .unwrap()
        .join("host-secret");
    std::fs::write(&host_secret, "other tenant's data").unwrap();
    // The masked key sits INSIDE the workspace (which the jail binds read-write), so the mask —
    // not a tmpfs hiding it — is what makes it unreadable. An unmasked sibling stays readable.
    let key = workspace.join("private-key.pem");
    std::fs::write(&key, "app-private-key").unwrap();
    std::fs::write(workspace.join("plain.txt"), "readable").unwrap();
    // A process on the host, to prove its /proc entry is not visible in the jail.
    let mut sentinel = Command::new("sleep").arg("30").spawn().unwrap();
    let sentinel_pid = sentinel.id();

    let jail = symphony_agents::egress::network_jail(
        &proxy.route(),
        Some(Path::new(SYMPHONY)),
        &workspace,
        std::slice::from_ref(&key),
    )
    .unwrap();
    let run = |script: String| {
        let mut command = Command::new(&jail[0]);
        command.args(&jail[1..]).args(["bash", "-c"]).arg(script);
        output(command)
    };
    let curl = "curl -sS --fail --max-time 10";
    let out = run(format!("{curl} http://{allowed}/")).await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    let out = run(format!("{curl} http://{other}/")).await;
    assert!(!out.status.success(), "refused by the proxy: {out:?}");
    let out = run(format!("{curl} --noproxy '*' http://{allowed}/")).await;
    assert!(
        !out.status.success(),
        "no route around the proxy, even to an allowed host: {out:?}"
    );

    // The workspace is writable, and the chdir is the workspace.
    let out = run("pwd; touch made-in-the-jail".to_owned()).await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        workspace.display().to_string()
    );
    assert!(workspace.join("made-in-the-jail").exists());

    // A host file outside the workspace is not even present.
    let out = run(format!("cat {}", host_secret.display())).await;
    assert!(!out.status.success(), "host file reachable: {out:?}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("other tenant"),
        "{out:?}"
    );

    // The masked key is in the workspace yet yields nothing — the /dev/null bind reads empty or
    // is refused — while its unmasked sibling in the same workspace is readable, so the mask
    // (not a tmpfs hiding the file) is what blocks it.
    let out = run(format!("cat {}", key.display())).await;
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("app-private-key"),
        "the masked key leaked: {out:?}"
    );
    let out = run(format!("cat {}", workspace.join("plain.txt").display())).await;
    assert_eq!(String::from_utf8_lossy(&out.stdout), "readable", "{out:?}");

    // Host /proc is not visible: the sentinel's entry is absent in the jail's own PID namespace.
    let out = run(format!(
        "cat /proc/{sentinel_pid}/comm 2>/dev/null; echo done"
    ))
    .await;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "done",
        "{out:?}"
    );

    let _ = sentinel.kill();
    let _ = sentinel.wait();
}
