# Axum Socket Backpressure

Exposes a low-level socket handle to Axum handlers and provides
per-connection backpressure monitoring.

## Why this exists

In typical Axum / WebSocket setups, handlers receive fairly high-level
abstractions (`WebSocket`, `Sink`, `Stream`, etc.). Those types are
convenient, but they abstract away the underlying TCP socket. In
particular:

* `Sink::send` only means “frame accepted by the sink”.
* `Sink::flush` only means “internal buffers drained into the underlying
  transport abstraction”.

Neither guarantees that bytes have left the kernel send buffer, nor that
the peer is actually reading. This differs from a file flush, and matters
when diagnosing backpressure or long-lived stalled connections.

On top of that, the server stack commonly erases or wraps the transport:
Hyper will type-erase and box the I/O, and you typically end up interacting
with a dyn/boxed stream rather than a concrete `TcpStream`. Even when you do
have a concrete type, it may be wrapped (e.g. TLS), and that wrapper can
introduce its own buffering and flush semantics. In practice, this means
you cannot rely on being able to “reach down” and recover the original TCP
socket from inside a handler.

When running high-throughput or long-lived connections (e.g. WebSockets),
it is often useful to observe or tune socket-level state directly — for
example inspecting the OS send queue size, tuning buffers, or detecting
persistent kernel-level backpressure.

Axum does not normally expose the underlying socket to handlers. This
crate wraps a `TcpListener` so that each accepted connection carries a
duplicated socket file descriptor alongside its peer address. That socket
can then be accessed inside handlers via `socket2::SockRef`.

The file descriptor is duplicated (`dup`) on accept. This avoids lifetime
issues and prevents file descriptor reuse bugs if the original `TcpStream`
is dropped while the connection metadata is still alive.

## Basic usage

```rust
use axum::{routing::get, Router, extract::ConnectInfo};
use axum_socket_backpressure::{TcpListenerWithSocketRef, ConnectInfoWithSocket};
use std::net::SocketAddr;

async fn handler(ConnectInfo(info): ConnectInfo<ConnectInfoWithSocket>) -> &'static str {
    let _ = info.as_socket_ref().set_send_buffer_size(256 * 1024);
    "ok"
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = "127.0.0.1:0".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    let app = Router::new()
        .route("/", get(handler))
        .into_make_service_with_connect_info::<ConnectInfoWithSocket>();

    axum::serve(TcpListenerWithSocketRef::from(listener), app).await?;
    Ok(())
}
```

## Backpressure monitoring in a WebSocket handler

A common pattern is to race “send next message” against “persistent
backpressure detected” and terminate the connection if the kernel send queue
stays non-zero for too long.

```rust
use axum::{
    extract::{
        ConnectInfo, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use futures::{SinkExt, pin_mut};

use axum_socket_backpressure::{ConnectInfoWithSocket, PressureConfig};

async fn ws_route(
    ws: WebSocketUpgrade,
    ConnectInfo(info): ConnectInfo<ConnectInfoWithSocket>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_handler(socket, info))
}

async fn ws_handler(mut ws: WebSocket, info: ConnectInfoWithSocket) {
    let cfg = PressureConfig {
        zero_epsilon_bytes: 1024,
        max_nonzero_for: std::time::Duration::from_secs(1),
        sample_every: std::time::Duration::from_millis(50),
    };

    let back_pressure_task = info.error_on_backpressure(cfg);
    pin_mut!(back_pressure_task);

    loop {
        tokio::select! {
            // Your application send path.
            // Replace with your real message source / channel.
            _ = async {
                ws.send(Message::Text("tick".into())).await
            } => {}

            // Kernel-level persistent backpressure.
            res = &mut back_pressure_task => {
                // If this triggers, the peer likely isn't reading.
                // Decide whether to close, log, or shed load.
                let _persistent = res;
                let _ = ws.close().await;
                return;
            }
        }
    }
}
```

`error_on_backpressure` returns the first persistent backpressure sample as a
`PersistentBackPressure`, or an `io::Error` if the monitor fails.

## What is backpressure? 

On Linux, TCP has a fairly direct notion of “bytes currently queued to output buffer” that you
can query from userspace via `ioctl` [TIOCOUTQ](https://man.archlinux.org/man/TIOCOUTQ.2const.en#TIOCOUTQ).
This is:

> `SIOCOUTQ = unsent_bytes + sent_but_not_ACKed_bytes`


This tends to line up with what you care about for WebSocket backpressure: if the peer
stops reading, the kernel send queue grows and eventually stalls progress. Buffer 
sizing (`SO_SNDBUF` / `SO_RCVBUF`) is also aggressively managed by Linux 
autotuning; `getsockopt` may report values that are larger than what you set due to 
accounting/overhead, and the kernel may grow buffers beyond the requested baseline.

On macOS, the equivalent observability is different. Some ioctls and socket options you might
expect from Linux either do not exist, return different units/semantics, or are intentionally
more opaque.

> `SO_NWRITE = unsent_bytes + sent_but_not_ACKed_bytes`

Was the closest thing I could find that matched SIOCOUTQ; *as such macOS compatibility was
just provided so you can test this while developing*.

Buffer sizing _is_ available on macOs, but the relationship between “what you set”
and “what you observe” is not guaranteed to match Linux, and some kernel queues are
not exposed in the same way. For example `set_send_buffer_size` is usually ignored for values
under 256kb in macOS, while Linux trusts you with obscenely small values like 4kb.

## TODOs

 - [] Allow for more fine grained Linux specific ioctl/getsockopt parameters for backpressure monitoring
 - [] Add a test for MaybeTlsStream (generic likely needed but most people don't expose their axum apps directly to the web)

## Versioning 

Versioned against axum's major version. Only tested against axum@0.8.
