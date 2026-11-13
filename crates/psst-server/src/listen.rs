use std::{
    collections::HashMap,
    fs, io,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::fs::{FileTypeExt, PermissionsExt},
    },
    path::Path,
    pin::{Pin, pin},
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use axum::{Router, extract::ConnectInfo, serve::Listener};
use hyper::{body::Incoming, server::conn::http1};
use hyper_util::rt::{TokioIo, TokioTimer};
use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::watch,
};
use tower_service::Service;

pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(3600);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);
const KEEPALIVE_RETRIES: u32 = 5;
/// hyper's default, which it applies only with a timer, and `axum::serve` sets none: without it, a
/// connection that never finishes its headers stays open for good.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// So that one machine can't use up the server's file descriptors. A browser opens six.
pub const CONNECTIONS_PER_ADDR: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    Unix { uid: Option<u32> },
    Tcp(SocketAddr),
}

#[derive(Debug)]
pub struct UnixSocket(tokio::net::UnixListener);

#[derive(Debug)]
pub struct TcpSocket {
    listener: tokio::net::TcpListener,
    open: OpenConns,
}

/// Open connections by [`network`].
#[derive(Debug, Default, Clone)]
struct OpenConns(Arc<Mutex<HashMap<IpAddr, usize>>>);

impl OpenConns {
    fn take(&self, addr: IpAddr) -> Option<Slot> {
        let addr = network(addr);
        let mut open = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = open.entry(addr).or_default();
        if *count >= CONNECTIONS_PER_ADDR {
            return None;
        }
        *count += 1;
        Some(Slot { open: self.clone(), addr })
    }
}

#[derive(Debug)]
struct Slot {
    open: OpenConns,
    addr: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut open = self.open.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = open.get_mut(&self.addr) {
            *count -= 1;
            if *count == 0 {
                open.remove(&self.addr);
            }
        }
    }
}

/// A connection that holds its slot until it closes, after an upgrade to a WebSocket too.
#[derive(Debug)]
pub struct TcpConn {
    stream: tokio::net::TcpStream,
    _slot: Slot,
}

impl AsyncRead for TcpConn {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpConn {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// One machine on IPv6 usually holds a whole /64, and can send from any address in it.
pub fn network(addr: IpAddr) -> IpAddr {
    match addr.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & !u128::from(u64::MAX))),
        v4 => v4,
    }
}

impl UnixSocket {
    pub fn new(listener: std::os::unix::net::UnixListener) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        Ok(Self(tokio::net::UnixListener::from_std(listener)?))
    }
}

impl TcpSocket {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

async fn accept_error(err: io::Error) {
    tracing::warn!("accept failed: {err}");
    tokio::time::sleep(Duration::from_millis(100)).await;
}

impl Listener for UnixSocket {
    type Io = tokio::net::UnixStream;
    type Addr = Peer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok((stream, _)) => {
                    let uid = stream.peer_cred().ok().map(|cred| cred.uid());
                    return (stream, Peer::Unix { uid });
                }
                Err(err) => accept_error(err).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(Peer::Unix { uid: None })
    }
}

impl Listener for TcpSocket {
    type Io = TcpConn;
    type Addr = Peer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, addr)) => {
                    let Some(slot) = self.open.take(addr.ip()) else {
                        tracing::debug!("closed a connection from {addr}: it has {CONNECTIONS_PER_ADDR} open");
                        continue;
                    };
                    if let Err(err) = stream.set_nodelay(true) {
                        tracing::debug!("cannot set TCP_NODELAY: {err}");
                    }
                    let keepalive = TcpKeepalive::new()
                        .with_time(KEEPALIVE_IDLE)
                        .with_interval(KEEPALIVE_INTERVAL)
                        .with_retries(KEEPALIVE_RETRIES);
                    if let Err(err) = SockRef::from(&stream).set_tcp_keepalive(&keepalive) {
                        tracing::debug!("cannot set SO_KEEPALIVE: {err}");
                    }
                    return (TcpConn { stream, _slot: slot }, Peer::Tcp(addr));
                }
                Err(err) => accept_error(err).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr().map(Peer::Tcp)
    }
}

/// Serves `app` until `stop`, then waits for the requests in flight. Upgraded WebSockets aren't
/// waited for: they watch `stop` themselves.
pub async fn serve<L: Listener<Addr = Peer>>(mut listener: L, app: Router, mut stop: watch::Receiver<bool>) {
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT);
    let (all_closed, open) = watch::channel(());
    loop {
        let (io, peer) = tokio::select! {
            accepted = listener.accept() => accepted,
            () = stopped(&mut stop) => break,
        };
        let app = app.clone();
        let service = hyper::service::service_fn(move |request: axum::http::Request<Incoming>| {
            let mut request = request.map(axum::body::Body::new);
            request.extensions_mut().insert(ConnectInfo(peer.clone()));
            app.clone().call(request)
        });
        let conn = http.serve_connection(TokioIo::new(io), service).with_upgrades();
        let (mut stop, open) = (stop.clone(), open.clone());
        tokio::spawn(async move {
            let mut conn = pin!(conn);
            let result = tokio::select! {
                result = conn.as_mut() => result,
                () = stopped(&mut stop) => {
                    conn.as_mut().graceful_shutdown();
                    conn.await
                }
            };
            if let Err(err) = result {
                tracing::debug!("a connection ended: {err}");
            }
            drop(open);
        });
    }
    drop(open);
    all_closed.closed().await;
}

async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

pub fn bind_tcp(addr: SocketAddr) -> io::Result<TcpSocket> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        // So `0.0.0.0` and `[::]` can both be bound.
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(TcpSocket { listener: tokio::net::TcpListener::from_std(socket.into())?, open: OpenConns::default() })
}

pub fn bind_unix(path: &Path, mode: u32) -> io::Result<std::os::unix::net::UnixListener> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is in use by another server", path.display()),
                ));
            }
            fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and isn't a socket", path.display()),
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

pub fn systemd_socket() -> Option<std::os::unix::net::UnixListener> {
    const FIRST_FD: i32 = 3;
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let fds: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || fds == 0 {
        return None;
    }
    if fds > 1 {
        tracing::warn!("systemd passed {fds} sockets; using the first");
    }
    // SAFETY: LISTEN_PID names this process, so systemd passed fd 3 to it, and nothing else in
    // the process has claimed it.
    let fd = unsafe { OwnedFd::from_raw_fd(FIRST_FD) };
    if let Err(err) = nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC)) {
        tracing::warn!("cannot set FD_CLOEXEC on the systemd socket: {err}");
    }
    let listener = std::os::unix::net::UnixListener::from(fd);
    match listener.local_addr() {
        Ok(_) => Some(listener),
        Err(err) => {
            tracing::error!("the socket systemd passed isn't a Unix socket: {err}");
            None
        }
    }
}
