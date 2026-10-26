//! `symphony egress-shim` end to end: the real binary relays a command's traffic to a task's
//! egress proxy, also from inside a bubblewrap sandbox without a network (Linux, when bubblewrap
//! can create namespaces here).

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
    Command::new(bwrap)
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
        .is_ok_and(|o| o.status.success())
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
        );
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
