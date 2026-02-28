#![doc = include_str!("../README.md")]
use std::{
    io,
    net::SocketAddr,
    ops::{Deref, DerefMut},
    os::fd::{AsFd, FromRawFd, OwnedFd},
    sync::Arc,
};

use axum::{
    extract::connect_info::Connected,
    serve::{IncomingStream, Listener},
};
use futures::{StreamExt, TryStreamExt};
use socket2::SockRef;
use std::os::fd::AsRawFd;
use tokio::net::{TcpListener, TcpStream};

mod monitor;
mod socket_backpressure;

pub use socket_backpressure::os_sendq_bytes;

pub use crate::monitor::{PersistentBackPressure, PressureConfig, PressureEvent, PressureMonitor};

/// Provides a socket ref to an axum handler:
///
/// Usage:
///
/// Use `into_make_service_with_connect_info` as you normally would with
/// `axum::extract::ConnectInfo`
///
/// ```rust
/// use axum::{routing::get, Router};
/// # use std::net::SocketAddr;
/// # async fn run() {
/// # let addr: SocketAddr = todo!();
/// # async fn example_handler() -> &'static str { "test" }
/// use axum_socket_backpressure::{TcpListenerWithSocketRef, ConnectInfoWithSocket};
///
/// let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
/// let app = Router::new()
///     .route("/example", get(example_handler))
///     .into_make_service_with_connect_info::<ConnectInfoWithSocket>();
///
/// let listener_with_socket = TcpListenerWithSocketRef::from(listener);
/// axum::serve(listener_with_socket, app).await;
/// # }
pub struct TcpListenerWithSocketRef {
    inner: TcpListener,
}

impl Deref for TcpListenerWithSocketRef {
    type Target = TcpListener;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for TcpListenerWithSocketRef {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<TcpListener> for TcpListenerWithSocketRef {
    fn from(value: TcpListener) -> Self {
        Self::new(value)
    }
}

impl TcpListenerWithSocketRef {
    pub fn new(inner: TcpListener) -> Self {
        Self { inner }
    }
}

impl TcpListenerWithSocketRef {
    fn dup_socket(tcp: &impl AsFd) -> OwnedFd {
        let raw = tcp.as_fd().as_raw_fd();
        // SAFETY: If you create an alias to an existing socket
        // and the original closes the alias will do operations on
        // the same file descriptor. This would normally be harmless
        // returning errors like "socket already closed" etc, HOWEVER
        // file descriptors can be re-used.
        //
        // libc dup avoids this problem by cloning the underlying file
        // descriptor. Avoiding this from being recycled until the
        // the owned file descriptor below is closed via drop()
        //
        // The socket.close on a duped socket doesn't actually close
        // the parent socket. It mearly decrements the internal ref
        // count of the parent socket.
        let dup_raw = unsafe { nix::libc::dup(raw) };
        if dup_raw < 0 {
            // whatever your listener contract is for retry/logging;
            // if you can't return Result here, you probably need to loop+log.
            panic!("dup failed: {}", io::Error::last_os_error());
        }

        // SAFETY: As above this should wrapped in an OwnedFd to
        // hook into the the closing logic implemented in the standard
        // library. It's safe because we know it's a file descriptor
        // of a socket and we checked for errors.
        //
        // On the drop logic: Socket RST will only be sent on the last
        // socket closing, so the drop order of these file descriptors
        // does not matter.
        unsafe { OwnedFd::from_raw_fd(dup_raw) }
    }
}

impl Listener for TcpListenerWithSocketRef {
    type Io = TcpStream;
    type Addr = ConnectInfoWithSocket;

    fn local_addr(&self) -> io::Result<Self::Addr> {
        let socket_fd = Arc::new(Self::dup_socket(&self.inner));
        Ok(ConnectInfoWithSocket {
            peer: Listener::local_addr(&self.inner)?,
            socket_fd,
        })
    }

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (tcp, peer) = Listener::accept(&mut self.inner).await;
        let socket_fd = Arc::new(Self::dup_socket(&tcp));
        (tcp, ConnectInfoWithSocket { peer, socket_fd })
    }
}

/// Provides connection info with a socket
///
/// Implements `Deref<Target=SocketAddr>` for backwards
/// compatibility with axum's built in ConnectionInfo structure
#[derive(Debug, Clone)]
pub struct ConnectInfoWithSocket {
    peer: SocketAddr,
    socket_fd: Arc<OwnedFd>,
}

impl Deref for ConnectInfoWithSocket {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.peer
    }
}

impl ConnectInfoWithSocket {
    /// Provides a reference to a socket. Example use:
    ///
    /// ```rust
    /// use axum::{extract::Path, response::IntoResponse};
    /// use axum_socket_backpressure::ConnectInfoWithSocket;
    ///
    /// async fn example_handler(
    ///     Path(buffer_size): Path<u32>,
    ///     connection_info: ConnectInfoWithSocket
    /// ) ->  impl IntoResponse {
    ///
    ///     // Dynamically setting the send buffer size
    ///     if let Err(_) = connection_info.as_socket_ref().set_send_buffer_size(buffer_size as usize) {
    ///         return "could not set buffer size"
    ///     }
    ///
    ///     "hello"
    /// }
    pub fn as_socket_ref(&self) -> SockRef<'_> {
        SockRef::from(&self.socket_fd)
    }

    /// Waits until the first `PressureEvent::Persistent` is observed and returns it as
    /// `PersistentBackPressure`.
    ///
    /// Any `io::Error` produced by the underlying monitor is returned immediately.
    ///
    /// If the backpressure event stream ends without yielding a persistent event, this returns
    /// `UnexpectedEof`.
    pub async fn error_on_backpressure(
        &self,
        cfg: PressureConfig,
    ) -> io::Result<PersistentBackPressure> {
        let s = self.backpressure_events(cfg).try_filter_map(|event| {
            futures::future::ready(match event {
                PressureEvent::Persistent {
                    nonzero_for,
                    q,
                    peak_q,
                } => Ok(Some(PersistentBackPressure {
                    nonzero_for,
                    q,
                    peak_q,
                })),
                _ => Ok(None),
            })
        });

        futures::pin_mut!(s);

        s.try_next().await?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "backpressure stream ended")
        })
    }

    /// Produces a sampled stream of `PressureEvent`s from `PressureMonitor::tick`, sleeping for
    /// `cfg.sample_every` between ticks.
    ///
    /// A tick error is emitted once as `Err(...)`
    /// and then the stream terminates.
    pub fn backpressure_events(
        &self,
        cfg: PressureConfig,
    ) -> impl futures::stream::Stream<Item = io::Result<PressureEvent>> {
        use std::time::Instant;
        let monitor = PressureMonitor::new(cfg);

        futures::stream::unfold(
            (monitor, true, false),
            move |(mut monitor, init, done)| async move {
                if done {
                    return None;
                }

                if !init {
                    tokio::time::sleep(cfg.sample_every).await;
                }

                match monitor.tick(&self.socket_fd, Instant::now()) {
                    Ok(Some(event)) => Some((Ok(Some(event)), (monitor, false, false))),
                    Ok(None) => Some((Ok(None), (monitor, false, false))),
                    Err(err) => Some((Err(err), (monitor, false, true))), // emit once, then terminate next poll
                }
            },
        )
        .filter_map(|v| {
            futures::future::ready(match v {
                Ok(Some(event)) => Some(Ok(event)),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            })
        })
    }
}

impl Connected<IncomingStream<'_, TcpListenerWithSocketRef>> for ConnectInfoWithSocket {
    fn connect_info(stream: IncomingStream<'_, TcpListenerWithSocketRef>) -> Self {
        stream.remote_addr().clone()
    }
}
