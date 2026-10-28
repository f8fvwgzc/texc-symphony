//! The task egress proxy and its shim, against real local servers.

#![cfg(unix)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use symphony_agents::egress::{AllowList, EgressProxy, MAX_HEAD_BYTES, run_shim};
use symphony_core::config::HostPort;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

/// A TCP echo server.
async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = stream.into_split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    addr
}

/// An HTTP server answering `hello` and recording each request head.
async fn http_server() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&heads);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let head = read_until_blank_line(&mut stream).await;
                seen.lock().unwrap().push(head);
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                    )
                    .await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (addr, heads)
}

async fn read_until_blank_line<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(1) => buf.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

async fn read_all<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn proxy_for(entries: &[SocketAddr]) -> EgressProxy {
    EgressProxy::start(
        AllowList::new(
            entries
                .iter()
                .map(|addr| HostPort::new(&addr.ip().to_string(), addr.port())),
        ),
        None,
    )
    .await
    .unwrap()
}

async fn connect_through<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    target: &str,
) -> String {
    stream
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    read_until_blank_line(stream).await
}

#[tokio::test]
async fn connect_reaches_allowed_targets_only() {
    let echo = echo_server().await;
    let proxy = proxy_for(&[echo]).await;

    let mut client = TcpStream::connect(proxy.tcp_addr()).await.unwrap();
    let status = connect_through(&mut client, &echo.to_string()).await;
    assert!(
        status.starts_with("HTTP/1.1 200 Connection Established\r\n"),
        "{status}"
    );
    client.write_all(b"ping").await.unwrap();
    let mut pong = [0u8; 4];
    client.read_exact(&mut pong).await.unwrap();
    assert_eq!(&pong, b"ping");

    // The Unix socket serves the same proxy.
    let mut unix = UnixStream::connect(proxy.socket().unwrap()).await.unwrap();
    let status = connect_through(&mut unix, &echo.to_string()).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");

    // Exact host and port: another name for the same address, or another port, is refused.
    for target in [
        format!("localhost:{}", echo.port()),
        format!("127.0.0.1:{}", echo.port() ^ 1),
        "github.com:22".to_owned(),
    ] {
        let mut client = TcpStream::connect(proxy.tcp_addr()).await.unwrap();
        client
            .write_all(format!("CONNECT {target} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let response = read_all(&mut client).await;
        assert!(
            response.starts_with("HTTP/1.1 403 Forbidden"),
            "{target}: {response}"
        );
        assert!(
            response.contains("not in this task's allow-list"),
            "{response}"
        );
    }
}

#[tokio::test]
async fn absolute_form_http_is_forwarded_in_origin_form() {
    let (server, heads) = http_server().await;
    let proxy = proxy_for(&[server]).await;
    let mut client = TcpStream::connect(proxy.tcp_addr()).await.unwrap();
    client
        .write_all(
            format!(
                "GET http://{server}/simple/pip/?x=1 HTTP/1.1\r\nHost: {server}\r\n\
                 Proxy-Connection: keep-alive\r\nUser-Agent: test\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let response = read_all(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("hello"), "{response}");
    let head = heads.lock().unwrap()[0].clone();
    assert!(
        head.starts_with("GET /simple/pip/?x=1 HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(head.contains("User-Agent: test\r\n"), "{head}");
    assert!(head.contains("Connection: close\r\n"), "{head}");
    assert!(
        !head.to_ascii_lowercase().contains("proxy-connection"),
        "{head}"
    );

    let mut client = TcpStream::connect(proxy.tcp_addr()).await.unwrap();
    client
        .write_all(b"GET http://example.invalid/ HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    assert!(read_all(&mut client).await.starts_with("HTTP/1.1 403"));
}

#[tokio::test]
async fn oversized_and_malformed_heads_are_rejected() {
    let echo = echo_server().await;
    let proxy = proxy_for(&[echo]).await;
    let mut client = TcpStream::connect(proxy.tcp_addr()).await.unwrap();
    let filler = format!("X-Fill: {}\r\n", "a".repeat(1000));
    let mut head = format!("CONNECT {echo} HTTP/1.1\r\n");
    while head.len() <= MAX_HEAD_BYTES {
        head.push_str(&filler);
    }
    head.push_str("\r\n");
    // The proxy may answer and close before it has read everything.
    let _ = client.write_all(head.as_bytes()).await;
    let response = read_all(&mut client).await;
    assert!(
        response.starts_with("HTTP/1.1 431 Request Header Fields Too Large"),
        "{response}"
    );

    let mut client = TcpStream::connect(proxy.tcp_addr()).await.unwrap();
    client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    assert!(read_all(&mut client).await.starts_with("HTTP/1.1 400"));
}

#[tokio::test]
async fn dropping_the_proxy_removes_its_socket() {
    let proxy = proxy_for(&[]).await;
    let socket = proxy.socket().unwrap().to_path_buf();
    assert!(socket.exists());
    let dir_mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(dir_mode, 0o700);
    drop(proxy);
    assert!(!socket.exists());
    assert!(!socket.parent().unwrap().exists());
}

fn curl() -> Option<std::path::PathBuf> {
    symphony_codex::launch::find_executable("curl")
}

#[tokio::test]
async fn the_shim_relays_a_child_through_the_proxy_socket() {
    let Some(curl) = curl() else {
        eprintln!("skipped: curl is not installed");
        return;
    };
    let (server, heads) = http_server().await;
    // Reachable directly, but not allowed.
    let (refused, refused_heads) = http_server().await;
    let proxy = proxy_for(&[server]).await;
    let socket = proxy.socket().unwrap().to_path_buf();
    let dir = tempfile::tempdir().unwrap();
    let body = dir.path().join("body.txt");
    let argv = |url: String, tunnel: bool| {
        let mut argv: Vec<std::ffi::OsString> = vec![
            curl.clone().into(),
            "-sS".into(),
            "--fail".into(),
            "--max-time".into(),
            "10".into(),
            "-o".into(),
            body.clone().into(),
        ];
        if tunnel {
            argv.push("--proxytunnel".into());
        }
        argv.push(url.into());
        argv
    };

    let code = run_shim(
        &socket,
        0,
        &argv(format!("http://{server}/via-shim"), false),
    )
    .await
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(std::fs::read_to_string(&body).unwrap(), "hello");
    let head = heads.lock().unwrap().last().cloned().unwrap();
    assert!(head.starts_with("GET /via-shim HTTP/1.1"), "{head}");
    assert!(
        head.contains("Connection: close"),
        "went through the proxy: {head}"
    );

    // CONNECT through the shim, too.
    std::fs::remove_file(&body).unwrap();
    let code = run_shim(&socket, 0, &argv(format!("http://{server}/tunnel"), true))
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(std::fs::read_to_string(&body).unwrap(), "hello");

    // Not allowed: the proxy answers 403 and curl --fail exits non-zero.
    let code = run_shim(&socket, 0, &argv(format!("http://{refused}/"), false))
        .await
        .unwrap();
    assert_ne!(code, 0);
    assert!(refused_heads.lock().unwrap().is_empty());

    // The shim passes the child's exit status through.
    let code = run_shim(&socket, 0, &["sh".into(), "-c".into(), "exit 7".into()])
        .await
        .unwrap();
    assert_eq!(code, 7);
}
