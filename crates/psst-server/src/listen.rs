use std::{
    fs, io,
    net::SocketAddr,
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::fs::{FileTypeExt, PermissionsExt},
    },
    path::Path,
    time::Duration,
};

use axum::{
    extract::connect_info::Connected,
    serve::{IncomingStream, Listener},
};
use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};

pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(3600);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);
const KEEPALIVE_RETRIES: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    Unix { uid: Option<u32> },
    Tcp(SocketAddr),
}

#[derive(Debug)]
pub struct UnixSocket(tokio::net::UnixListener);

#[derive(Debug)]
pub struct TcpSocket(tokio::net::TcpListener);

impl UnixSocket {
    pub fn new(listener: std::os::unix::net::UnixListener) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        Ok(Self(tokio::net::UnixListener::from_std(listener)?))
    }
}

impl TcpSocket {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
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
    type Io = tokio::net::TcpStream;
    type Addr = Peer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok((stream, addr)) => {
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
                    return (stream, Peer::Tcp(addr));
                }
                Err(err) => accept_error(err).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr().map(Peer::Tcp)
    }
}

impl Connected<IncomingStream<'_, UnixSocket>> for Peer {
    fn connect_info(stream: IncomingStream<'_, UnixSocket>) -> Self {
        stream.remote_addr().clone()
    }
}

impl Connected<IncomingStream<'_, TcpSocket>> for Peer {
    fn connect_info(stream: IncomingStream<'_, TcpSocket>) -> Self {
        stream.remote_addr().clone()
    }
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
    Ok(TcpSocket(tokio::net::TcpListener::from_std(socket.into())?))
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
