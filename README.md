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
