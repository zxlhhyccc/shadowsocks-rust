//! TcpStream wrappers that supports connecting with options

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};
use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{self, Poll},
};

use futures::{future, ready};
use log::warn;
use pin_project::pin_project;
use socket2::{Socket, TcpKeepalive};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener as TokioTcpListener, TcpSocket, TcpStream as TokioTcpStream},
};

use crate::{context::Context, relay::socks5::Address, ServerAddr};

use super::{
    sys::{set_tcp_fastopen, TcpStream as SysTcpStream},
    AcceptOpts,
    ConnectOpts,
};

/// TcpStream for outbound connections
#[pin_project]
pub struct TcpStream(#[pin] SysTcpStream);

impl TcpStream {
    /// Connects to address
    pub async fn connect_with_opts(addr: &SocketAddr, opts: &ConnectOpts) -> io::Result<TcpStream> {
        // tcp_stream_connect(addr, opts).await.map(TcpStream)
        SysTcpStream::connect(*addr, opts).await.map(TcpStream)
    }

    /// Connects shadowsocks server
    pub async fn connect_server_with_opts(
        context: &Context,
        addr: &ServerAddr,
        opts: &ConnectOpts,
    ) -> io::Result<TcpStream> {
        let stream = match *addr {
            ServerAddr::SocketAddr(ref addr) => SysTcpStream::connect(*addr, opts).await?,
            ServerAddr::DomainName(ref domain, port) => {
                lookup_then!(context, domain, port, |addr| {
                    SysTcpStream::connect(addr, opts).await
                })?
                .1
            }
        };

        Ok(TcpStream(stream))
    }

    /// Connects proxy remote target
    pub async fn connect_remote_with_opts(
        context: &Context,
        addr: &Address,
        opts: &ConnectOpts,
    ) -> io::Result<TcpStream> {
        let stream = match *addr {
            Address::SocketAddress(ref addr) => SysTcpStream::connect(*addr, opts).await?,
            Address::DomainNameAddress(ref domain, port) => {
                lookup_then!(context, domain, port, |addr| {
                    SysTcpStream::connect(addr, opts).await
                })?
                .1
            }
        };

        Ok(TcpStream(stream))
    }
}

impl Deref for TcpStream {
    type Target = TokioTcpStream;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TcpStream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.project().0.poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.project().0.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().0.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().0.poll_shutdown(cx)
    }
}

/// `TcpListener` for accepting inbound connections
pub struct TcpListener {
    inner: TokioTcpListener,
    accept_opts: AcceptOpts,
}

impl TcpListener {
    /// Creates a new TcpListener, which will be bound to the specified address.
    pub async fn bind_with_opts(addr: &SocketAddr, accept_opts: AcceptOpts) -> io::Result<TcpListener> {
        let socket = match *addr {
            SocketAddr::V4(..) => TcpSocket::new_v4()?,
            SocketAddr::V6(..) => TcpSocket::new_v6()?,
        };

        // On platforms with Berkeley-derived sockets, this allows to quickly
        // rebind a socket, without needing to wait for the OS to clean up the
        // previous one.
        //
        // On Windows, this allows rebinding sockets which are actively in use,
        // which allows “socket hijacking”, so we explicitly don't set it here.
        // https://docs.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse
        #[cfg(not(windows))]
        socket.set_reuseaddr(true)?;

        let set_dual_stack = if let SocketAddr::V6(ref v6) = *addr {
            v6.ip().is_unspecified()
        } else {
            false
        };

        if set_dual_stack {
            // Set to DUAL STACK mode by default.
            // WARNING: This would fail if you want to start another program listening on the same port.
            //
            // Should this behavior be configurable?
            fn set_only_v6(socket: &TcpSocket, only_v6: bool) {
                unsafe {
                    // WARN: If the following code panics, FD will be closed twice.
                    #[cfg(unix)]
                    let s = Socket::from_raw_fd(socket.as_raw_fd());
                    #[cfg(windows)]
                    let s = Socket::from_raw_socket(socket.as_raw_socket());
                    if let Err(err) = s.set_only_v6(only_v6) {
                        warn!("failed to set IPV6_V6ONLY: {} for listener, error: {}", only_v6, err);

                        // This is not a fatal error, just warn and skip
                    }

                    #[cfg(unix)]
                    let _ = s.into_raw_fd();
                    #[cfg(windows)]
                    let _ = s.into_raw_socket();
                }
            }

            set_only_v6(&socket, false);
            match socket.bind(*addr) {
                Ok(..) => {}
                Err(ref err) if err.kind() == ErrorKind::AddrInUse => {
                    // This is probably 0.0.0.0 with the same port has already been occupied
                    warn!(
                        "0.0.0.0:{} may have already been occupied, retry with IPV6_V6ONLY",
                        addr.port()
                    );

                    set_only_v6(&socket, true);
                    socket.bind(*addr)?;
                }
                Err(err) => return Err(err),
            }
        } else {
            socket.bind(*addr)?;
        }

        // mio's default backlog is 1024
        let inner = socket.listen(1024)?;

        // Enable TFO if supported
        // macos requires TCP_FASTOPEN to be set after listen(), but other platform doesn't have this constraint
        if accept_opts.tcp.fastopen {
            set_tcp_fastopen(&inner)?;
        }

        Ok(TcpListener { inner, accept_opts })
    }

    /// Create a `TcpListener` from tokio's `TcpListener`
    pub fn from_listener(listener: TokioTcpListener, accept_opts: AcceptOpts) -> TcpListener {
        TcpListener {
            inner: listener,
            accept_opts,
        }
    }

    /// Polls to accept a new incoming connection to this listener.
    pub fn poll_accept(&self, cx: &mut task::Context<'_>) -> Poll<io::Result<(TokioTcpStream, SocketAddr)>> {
        let (stream, peer_addr) = ready!(self.inner.poll_accept(cx))?;
        setsockopt_with_opt(&stream, &self.accept_opts)?;
        Poll::Ready(Ok((stream, peer_addr)))
    }

    /// Accept a new incoming connection to this listener
    pub async fn accept(&self) -> io::Result<(TokioTcpStream, SocketAddr)> {
        future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    /// Unwraps and take the internal `TcpListener`
    pub fn into_inner(self) -> TokioTcpListener {
        self.inner
    }
}

impl Deref for TcpListener {
    type Target = TokioTcpListener;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for TcpListener {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<TcpListener> for TokioTcpListener {
    fn from(listener: TcpListener) -> TokioTcpListener {
        listener.inner
    }
}

#[cfg(unix)]
fn setsockopt_with_opt(f: &tokio::net::TcpStream, opts: &AcceptOpts) -> io::Result<()> {
    let socket = unsafe { Socket::from_raw_fd(f.as_raw_fd()) };

    macro_rules! try_sockopt {
        ($socket:ident . $func:ident ($($arg:expr),*)) => {
            match $socket . $func ($($arg),*) {
                Ok(e) => e,
                Err(err) => {
                    let _ = socket.into_raw_fd();
                    return Err(err);
                }
            }
        };
    }

    if let Some(buf_size) = opts.tcp.send_buffer_size {
        try_sockopt!(socket.set_send_buffer_size(buf_size as usize));
    }

    if let Some(buf_size) = opts.tcp.recv_buffer_size {
        try_sockopt!(socket.set_recv_buffer_size(buf_size as usize));
    }

    try_sockopt!(socket.set_nodelay(opts.tcp.nodelay));

    if let Some(keepalive_duration) = opts.tcp.keepalive {
        #[allow(unused_mut)]
        let mut keepalive = TcpKeepalive::new().with_time(keepalive_duration);

        #[cfg(any(
            target_os = "freebsd",
            target_os = "fuchsia",
            target_os = "linux",
            target_os = "netbsd",
            target_vendor = "apple",
        ))]
        {
            keepalive = keepalive.with_interval(keepalive_duration);
        }

        try_sockopt!(socket.set_tcp_keepalive(&keepalive));
    }

    let _ = socket.into_raw_fd();
    Ok(())
}

#[cfg(windows)]
fn setsockopt_with_opt(f: &tokio::net::TcpStream, opts: &AcceptOpts) -> io::Result<()> {
    let socket = unsafe { Socket::from_raw_socket(f.as_raw_socket()) };

    macro_rules! try_sockopt {
        ($socket:ident . $func:ident ($($arg:expr),*)) => {
            match $socket . $func ($($arg),*) {
                Ok(e) => e,
                Err(err) => {
                    let _ = socket.into_raw_socket();
                    return Err(err);
                }
            }
        };
    }

    if let Some(buf_size) = opts.tcp.send_buffer_size {
        try_sockopt!(socket.set_send_buffer_size(buf_size as usize));
    }

    if let Some(buf_size) = opts.tcp.recv_buffer_size {
        try_sockopt!(socket.set_recv_buffer_size(buf_size as usize));
    }

    try_sockopt!(socket.set_nodelay(opts.tcp.nodelay));

    if let Some(keepalive_duration) = opts.tcp.keepalive {
        let keepalive = TcpKeepalive::new()
            .with_time(keepalive_duration)
            .with_interval(keepalive_duration);
        try_sockopt!(socket.set_tcp_keepalive(&keepalive));
    }

    let _ = socket.into_raw_socket();
    Ok(())
}

#[cfg(all(not(windows), not(unix)))]
fn setsockopt_with_opt(f: &tokio::net::TcpStream, opts: &AcceptOpts) -> io::Result<()> {
    f.set_nodelay(opts.tcp.nodelay)?;
    Ok(())
}

#[cfg(unix)]
impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for TcpStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.0.as_raw_socket()
    }
}
