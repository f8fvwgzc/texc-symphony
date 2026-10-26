//! Default-deny network for task runs (`isolation.network.mode: deny`): an allow-list HTTP proxy
//! owned by the runner, and the shim that carries a sandbox's traffic to it.
//!
//! ```text
//! api shell in bubblewrap, codex and claude in the network jail (empty network namespace)
//!   └ HTTPS_PROXY=http://127.0.0.1:3128 ─▶ egress-shim ─Unix socket─▶ EgressProxy ─▶ allowed host:port
//! api shell in seatbelt (network denied except localhost:<port>)
//!   └ HTTPS_PROXY=http://127.0.0.1:<port> ─────────────────────────▶ EgressProxy ─▶ allowed host:port
//! ```
//!
//! * [`EgressProxy`] listens on a Unix socket in a fresh `0700` directory and on
//!   `127.0.0.1:<ephemeral>`. It serves `CONNECT host:port` (tunnels, used for https) and
//!   absolute-form plain HTTP requests (`GET http://host:port/path`), and only to entries of the
//!   task's [`AllowList`] (exact host, case-insensitive, and exact port); anything else gets
//!   `403`. Request heads are capped at [`MAX_HEAD_BYTES`] and must arrive within
//!   [`HEAD_TIMEOUT`]; upstream connects are bounded by [`CONNECT_TIMEOUT`].
//! * [`run_shim`] (`symphony egress-shim --socket S --port P -- CMD...`) runs inside the
//!   bubblewrap sandbox, whose network namespace is empty: it listens on `127.0.0.1:P` there,
//!   relays every connection to the proxy's socket, and runs `CMD` with the proxy variables.
//!
//! * [`network_jail`] is the launch prefix for Codex and Claude Code: bubblewrap with the host's
//!   file system but an empty network namespace, and the shim inside, so the agent and every
//!   command it runs have no route out but the proxy. It needs Linux and unprivileged user
//!   namespaces; without them a task with a default-deny network does not start.
//!
//! Enforcement: the sandboxed api shell and the jailed Codex and Claude Code have no other route
//! out (an empty network namespace), and seatbelt denies every other connection in the kernel
//! (macOS development). Only an unconfined api shell (`sandbox: none`) is left with the proxy
//! variables alone.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use symphony_codex::EgressRoute;
use symphony_codex::launch::find_executable;
use symphony_core::config::HostPort;
use symphony_core::config::isolation::url_host_port;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

/// The launch prefix that confines a whole agent process (Codex, Claude Code) of a task to its
/// egress proxy: `bwrap --dev-bind / / --unshare-net ... -- <shim> egress-shim --socket S
/// --port 3128 --`. The agent keeps the host's file system and tools; only its network is
/// replaced by the shim's relay to the proxy, and the `masked` secret files (Symphony's own keys,
/// such as the GitHub App key) read as empty. `Err` when this cannot be enforced (not Linux, no
/// `bwrap`, a proxy without a socket, no shim): the agent must not start then.
pub fn network_jail(
    route: &EgressRoute,
    shim: Option<&Path>,
    cwd: &Path,
    masked: &[PathBuf],
) -> Result<Vec<std::ffi::OsString>, String> {
    const NEEDS: &str = "isolation.network.mode: deny confines Codex and Claude Code with \
                         bubblewrap (Linux, with unprivileged user namespaces)";
    if !cfg!(target_os = "linux") {
        return Err(format!("{NEEDS}; this host is not Linux"));
    }
    let bwrap =
        find_executable("bwrap").ok_or_else(|| format!("{NEEDS}; `bwrap` is not on PATH"))?;
    let socket = route
        .socket
        .as_ref()
        .ok_or_else(|| "the task's egress proxy has no Unix socket".to_owned())?;
    let shim =
        shim.ok_or_else(|| "the `symphony` executable (the egress shim) is unknown".to_owned())?;
    let mut argv: Vec<std::ffi::OsString> = vec![bwrap.into()];
    argv.extend(["--dev-bind", "/", "/", "--unshare-net", "--die-with-parent"].map(Into::into));
    // Over the file's real path, so every name of it (a Kubernetes Secret's symlinks) is masked.
    for file in masked {
        if let Ok(real) = std::fs::canonicalize(file)
            && real.is_file()
        {
            argv.extend(["--ro-bind".into(), "/dev/null".into(), real.into()]);
        }
    }
    argv.push("--chdir".into());
    argv.push(cwd.into());
    argv.push("--".into());
    argv.push(shim.into());
    argv.extend([SHIM_SUBCOMMAND, "--socket"].map(Into::into));
    argv.push(socket.into());
    argv.extend(["--port".into(), SHIM_PORT.to_string().into(), "--".into()]);
    Ok(argv)
}

/// The hidden `symphony` subcommand that runs [`run_shim`].
pub const SHIM_SUBCOMMAND: &str = "egress-shim";
/// Port the shim listens on inside a bubblewrap sandbox (its network namespace is private).
pub const SHIM_PORT: u16 = 3128;
/// Largest request head (request line + headers) the proxy reads.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// How long a client may take to send its request head.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the proxy waits for an upstream TCP connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The proxy variables for `url` (both cases; `NO_PROXY` emptied so nothing bypasses it).
pub fn proxy_env(url: &str) -> Vec<(String, String)> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), url.to_owned()))
    .chain(["NO_PROXY", "no_proxy"].map(|name| (name.to_owned(), String::new())))
    .collect()
}

/// The `host:port` entries a task may reach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowList(Vec<HostPort>);

impl AllowList {
    /// An allow-list of `entries`.
    pub fn new(entries: impl IntoIterator<Item = HostPort>) -> Self {
        Self(entries.into_iter().collect())
    }

    /// `true` when `host` (exact, any case) and `port` are listed.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        self.0.iter().any(|entry| entry.matches(host, port))
    }

    /// The entries.
    pub fn entries(&self) -> &[HostPort] {
        &self.0
    }
}

static PROXIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The allow-list proxy of one task run (see the module docs). Dropping it stops the listeners,
/// closes every relayed connection and removes its socket directory.
#[derive(Debug)]
pub struct EgressProxy {
    dir: Option<PathBuf>,
    socket: Option<PathBuf>,
    tcp: SocketAddr,
    allow: Arc<AllowList>,
    tasks: Vec<JoinHandle<()>>,
}

impl EgressProxy {
    /// Starts the proxy for `allow`. With `owner` (the task's uid/gid) the socket directory and
    /// socket belong to that user, so processes running as it can connect.
    pub async fn start(allow: AllowList, owner: Option<(u32, u32)>) -> io::Result<Self> {
        let allow = Arc::new(allow);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let tcp = listener.local_addr()?;
        let mut tasks = vec![tokio::spawn(accept_tcp(listener, Arc::clone(&allow)))];
        let mut proxy = Self {
            dir: None,
            socket: None,
            tcp,
            allow: Arc::clone(&allow),
            tasks: Vec::new(),
        };
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(format!(
                "symphony-egress-{}-{}",
                std::process::id(),
                PROXIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
            }
            proxy.dir = Some(dir.clone());
            let socket = dir.join("proxy.sock");
            let unix = tokio::net::UnixListener::bind(&socket)?;
            if let Some((uid, gid)) = owner {
                std::os::unix::fs::chown(&dir, Some(uid), Some(gid))?;
                std::os::unix::fs::chown(&socket, Some(uid), Some(gid))?;
            }
            proxy.socket = Some(socket);
            tasks.push(tokio::spawn(accept_unix(unix, Arc::clone(&allow))));
        }
        #[cfg(not(unix))]
        let _ = (owner, &PROXIES);
        proxy.tasks = tasks;
        tracing::info!(
            "Egress proxy started tcp={tcp} allow={}",
            allow
                .entries()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        Ok(proxy)
    }

    /// Where it listens (for [`symphony_codex::StartOptions::egress`]).
    pub fn route(&self) -> EgressRoute {
        EgressRoute {
            socket: self.socket.clone(),
            tcp: self.tcp,
        }
    }

    /// The Unix socket (Unix only).
    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    /// The loopback TCP listener.
    pub fn tcp_addr(&self) -> SocketAddr {
        self.tcp
    }

    /// The allow-list.
    pub fn allow_list(&self) -> &AllowList {
        &self.allow
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Accepts connections until aborted; aborting drops the set and so every open connection.
async fn accept_tcp(listener: TcpListener, allow: Arc<AllowList>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(serve(stream, Arc::clone(&allow)));
                }
                Err(err) => {
                    tracing::warn!("Egress proxy accept failed: {err}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            Some(_) = connections.join_next() => {}
        }
    }
}

#[cfg(unix)]
async fn accept_unix(listener: tokio::net::UnixListener, allow: Arc<AllowList>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(serve(stream, Arc::clone(&allow)));
                }
                Err(err) => {
                    tracing::warn!("Egress proxy accept failed: {err}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            Some(_) = connections.join_next() => {}
        }
    }
}

/// Why a request head could not be read.
#[derive(Debug, PartialEq, Eq)]
enum HeadError {
    TooLarge,
    Closed,
    TimedOut,
}

/// Reads up to the end of the request head; returns the head and any bytes read after it.
async fn read_head<S: AsyncRead + Unpin>(client: &mut S) -> Result<(Vec<u8>, Vec<u8>), HeadError> {
    let read = async {
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 4096];
        loop {
            let n = client
                .read(&mut chunk)
                .await
                .map_err(|_| HeadError::Closed)?;
            if n == 0 {
                return Err(HeadError::Closed);
            }
            let searched_from = buf.len().saturating_sub(3);
            buf.extend_from_slice(&chunk[..n]);
            if let Some(at) = buf[searched_from..]
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
            {
                let end = searched_from + at + 4;
                if end > MAX_HEAD_BYTES {
                    return Err(HeadError::TooLarge);
                }
                let rest = buf.split_off(end);
                return Ok((buf, rest));
            }
            if buf.len() > MAX_HEAD_BYTES {
                return Err(HeadError::TooLarge);
            }
        }
    };
    tokio::time::timeout(HEAD_TIMEOUT, read)
        .await
        .unwrap_or(Err(HeadError::TimedOut))
}

async fn respond<S: AsyncWrite + Unpin>(client: &mut S, status: &str, message: &str) {
    let body = format!("symphony egress: {message}\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(response.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// What a request head asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// `CONNECT host:port`.
    Connect(HostPort),
    /// An absolute-form `http://` request, rewritten to origin form.
    Forward { target: HostPort, head: Vec<u8> },
}

/// Parses a request head; `Err` is the `400` message.
fn parse_request(head: &[u8]) -> Result<Request, &'static str> {
    let text = std::str::from_utf8(head).map_err(|_| "the request head is not UTF-8")?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed request line");
    };
    if !version.starts_with("HTTP/1.") || method.is_empty() {
        return Err("malformed request line");
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        return HostPort::parse(target)
            .map(Request::Connect)
            .ok_or("CONNECT needs host:port");
    }
    let Some(rest) = target
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .map(|_| &target[7..])
    else {
        return Err("only CONNECT and absolute http:// requests are proxied");
    };
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    if authority.contains('@') {
        return Err("credentials in the URL are not proxied");
    }
    let target_host =
        url_host_port(&format!("http://{authority}")).ok_or("malformed request target")?;
    let path = match path {
        "" => "/".to_owned(),
        p if p.starts_with('?') => format!("/{p}"),
        p => p.to_owned(),
    };
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines.filter(|l| !l.is_empty()) {
        let name = line.split(':').next().unwrap_or_default().trim();
        let hop_by_hop = [
            "proxy-connection",
            "proxy-authorization",
            "connection",
            "keep-alive",
        ]
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h));
        if !hop_by_hop {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    Ok(Request::Forward {
        target: target_host,
        head: out.into_bytes(),
    })
}

/// `true` for an address a task must not reach through the proxy: loopback, private, link-local,
/// unspecified, carrier-grade NAT or an IPv4-mapped form of one. The host's own services and
/// other pods live at such addresses, so a tenant name resolving there is refused.
pub(crate) fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_internal_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_internal_v4(v4),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
            }
        },
    }
}

fn is_internal_v4(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.octets()[0] == 0 // "this network" 0.0.0.0/8
        || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64) // CGNAT 100.64/10
}

/// A vetted address for `target`: an IP literal (the allow-list already vetted the exact IP an
/// operator named) as is; a hostname resolved to its first non-internal address, so a
/// tenant-controlled name cannot point the proxy at an internal service (no DNS rebinding needed).
async fn resolve_upstream(target: &HostPort) -> Result<SocketAddr, (&'static str, String)> {
    if let Ok(ip) = target.host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, target.port));
    }
    let resolved = tokio::net::lookup_host((target.host.as_str(), target.port))
        .await
        .map_err(|err| ("502 Bad Gateway", format!("cannot resolve {target}: {err}")))?;
    resolved
        .into_iter()
        .find(|addr| !is_internal(addr.ip()))
        .ok_or((
            "403 Forbidden",
            format!("{target} resolves only to internal addresses"),
        ))
}

async fn connect_upstream(target: &HostPort) -> Result<TcpStream, (&'static str, String)> {
    let addr = resolve_upstream(target).await?;
    let connect = TcpStream::connect(addr);
    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => Err((
            "502 Bad Gateway",
            format!("cannot connect to {target}: {err}"),
        )),
        Err(_) => Err((
            "504 Gateway Timeout",
            format!("connecting to {target} timed out"),
        )),
    }
}

/// Serves one client connection.
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut client: S, allow: Arc<AllowList>) {
    let (head, rest) = match read_head(&mut client).await {
        Ok(read) => read,
        Err(HeadError::TooLarge) => {
            respond(
                &mut client,
                "431 Request Header Fields Too Large",
                "request head too large",
            )
            .await;
            return;
        }
        Err(HeadError::TimedOut) => {
            respond(&mut client, "408 Request Timeout", "request head timed out").await;
            return;
        }
        Err(HeadError::Closed) => return,
    };
    let request = match parse_request(&head) {
        Ok(request) => request,
        Err(message) => {
            respond(&mut client, "400 Bad Request", message).await;
            return;
        }
    };
    let target = match &request {
        Request::Connect(target) | Request::Forward { target, .. } => target.clone(),
    };
    if !allow.allows(&target.host, target.port) {
        tracing::warn!("Egress refused target={target} (not in the task allow-list)");
        respond(
            &mut client,
            "403 Forbidden",
            &format!("{target} is not in this task's allow-list"),
        )
        .await;
        return;
    }
    let mut upstream = match connect_upstream(&target).await {
        Ok(stream) => stream,
        Err((status, message)) => {
            respond(&mut client, status, &message).await;
            return;
        }
    };
    let ready = match &request {
        Request::Connect(_) => client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_ok(),
        Request::Forward { head, .. } => upstream.write_all(head).await.is_ok(),
    };
    if !ready || (!rest.is_empty() && upstream.write_all(&rest).await.is_err()) {
        return;
    }
    let _ = copy_bidirectional(&mut client, &mut upstream).await;
}

/// `symphony egress-shim`: listens on `127.0.0.1:port` (`0` picks a free port), relays every
/// connection to the proxy at `socket`, runs `argv` with the proxy variables pointing at the
/// listener and returns its exit code (`128 + signal` when it was killed).
#[cfg(unix)]
pub async fn run_shim(socket: &Path, port: u16, argv: &[std::ffi::OsString]) -> io::Result<i32> {
    use std::os::unix::process::ExitStatusExt;

    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no command to run"))?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    let socket = socket.to_path_buf();
    let relay = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((mut stream, _)) => {
                        let socket = socket.clone();
                        connections.spawn(async move {
                            if let Ok(mut proxy) = tokio::net::UnixStream::connect(&socket).await {
                                let _ = copy_bidirectional(&mut stream, &mut proxy).await;
                            }
                        });
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                },
                Some(_) = connections.join_next() => {}
            }
        }
    });
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    for (name, value) in proxy_env(&format!("http://{addr}")) {
        command.env(name, value);
    }
    let status = command.status().await;
    relay.abort();
    let status = status?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_parsed_and_rewritten_to_origin_form() {
        assert_eq!(
            parse_request(b"CONNECT GitHub.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n"),
            Ok(Request::Connect(HostPort::new("github.com", 443)))
        );
        let Ok(Request::Forward { target, head }) = parse_request(
            b"GET http://Example.test:8080?q=1 HTTP/1.1\r\nHost: example.test:8080\r\n\
              Proxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n",
        ) else {
            panic!("a forward request")
        };
        assert_eq!(target, HostPort::new("example.test", 8080));
        assert_eq!(
            String::from_utf8(head).unwrap(),
            "GET /?q=1 HTTP/1.1\r\nHost: example.test:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        let Ok(Request::Forward { target, .. }) =
            parse_request(b"GET http://plain.test/x HTTP/1.0\r\n\r\n")
        else {
            panic!("a forward request")
        };
        assert_eq!(target, HostPort::new("plain.test", 80));
        for bad in [
            &b"GET /relative HTTP/1.1\r\n\r\n"[..],
            b"GET https://x.test/ HTTP/1.1\r\n\r\n",
            b"GET http://user@x.test/ HTTP/1.1\r\n\r\n",
            b"CONNECT x.test HTTP/1.1\r\n\r\n",
            b"GARBAGE\r\n\r\n",
        ] {
            assert!(
                parse_request(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn the_network_jail_runs_the_agent_under_the_shim_without_a_network() {
        let route = EgressRoute {
            socket: Some(PathBuf::from("/tmp/symphony-egress-1/proxy.sock")),
            tcp: "127.0.0.1:41000".parse().unwrap(),
        };
        let shim = Path::new("/usr/local/bin/symphony");
        let key = tempfile::NamedTempFile::new().unwrap();
        let real_key = std::fs::canonicalize(key.path()).unwrap();
        let jail = network_jail(
            &route,
            Some(shim),
            Path::new("/w/.tasks/7"),
            &[
                key.path().to_path_buf(),
                PathBuf::from("/nonexistent/key.pem"),
            ],
        );
        if !cfg!(target_os = "linux") || find_executable("bwrap").is_none() {
            let err = jail.unwrap_err();
            assert!(err.contains("bubblewrap"), "{err}");
            return;
        }
        let argv: Vec<String> = jail
            .unwrap()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv[1..].join(" "),
            format!(
                "--dev-bind / / --unshare-net --die-with-parent --ro-bind /dev/null {} \
                 --chdir /w/.tasks/7 -- /usr/local/bin/symphony egress-shim --socket \
                 /tmp/symphony-egress-1/proxy.sock --port 3128 --",
                real_key.display()
            ),
            "the existing secret file is masked at its real path, a missing one skipped"
        );
        // Without a socket or a shim there is nothing to relay through: refused, not unconfined.
        let no_socket = EgressRoute {
            socket: None,
            ..route.clone()
        };
        assert!(network_jail(&no_socket, Some(shim), Path::new("/w"), &[]).is_err());
        assert!(network_jail(&route, None, Path::new("/w"), &[]).is_err());
    }

    #[test]
    fn internal_addresses_are_refused() {
        for internal in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "0.0.0.0",
            "100.100.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_internal(internal.parse().unwrap()), "{internal}");
        }
        for public in ["8.8.8.8", "140.82.121.4", "2606:4700::1111", "172.32.0.1"] {
            assert!(!is_internal(public.parse().unwrap()), "{public}");
        }
    }

    #[tokio::test]
    async fn a_hostname_that_resolves_to_a_loopback_address_is_refused() {
        // localhost resolves to 127.0.0.1 / ::1: a tenant name pointing inward is refused.
        let err = resolve_upstream(&HostPort::new("localhost", 443))
            .await
            .unwrap_err();
        assert_eq!(err.0, "403 Forbidden", "{err:?}");
        // An operator-named IP literal is taken as given (the allow-list vetted it).
        let addr = resolve_upstream(&HostPort::new("127.0.0.1", 8080))
            .await
            .unwrap();
        assert_eq!(addr, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn proxy_variables_cover_both_spellings_and_clear_no_proxy() {
        let env = proxy_env("http://127.0.0.1:9");
        for name in ["HTTPS_PROXY", "http_proxy", "ALL_PROXY"] {
            assert!(env.contains(&(name.to_owned(), "http://127.0.0.1:9".to_owned())));
        }
        assert!(env.contains(&("NO_PROXY".to_owned(), String::new())));
    }
}
