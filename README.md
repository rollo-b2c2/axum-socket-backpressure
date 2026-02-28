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


## Why Ping/Pong and Healthchecks Aren't Enough

Correctly implemented Websockets (or any socket connection) will have a liveness
probe. These generally come in two forms:

 - Websocket ping/pong frames
 - Application Level healthchecks like a server sending `{ "type": "healthcheck", "id": 1 }` messages to a client and timing out on failure.

Naively we can assume this is sufficient — it is not. Browsers are quasi-operating systems and
effectively create a hidden OSI layer in the networking stack. Ping/Pong frames and
healthchecks, while technically both application level, for the JavaScript app they sit beneath; 
as ping-pongs are eagerly responded to by the browser, and not the JavaScript application.
If a JavaScript app is sleeping (power saving) or busy the browser will _still_ respond
to the ping with a pong.

Conversely Application level healthchecks need the JavaScript app to be awake to respond
to server liveness checks. This nuance between ping-pongs and app level healthchecks can
be a footgun where the server aggressively disconnects JavaScript apps that Chrome put to
sleeping due to lack of focus or power management settings.

Already we can see that there are multiple event queues. The browser level queue for processing
ping-pong frames and the app level queue for processing all other Websocket messages.

What neither of these methods tell us: has the client's networking stack is received packets?
Like how a browser may preemptively sleep a tab, the client's operating system may preemptively
sleep a browser. If this happens, the operating system's network stack will continue 
to receive network packets and buffer them in the _operating system's_ socket receive queue.

While the above methods will work for applications that only care about browser stalls, 
you **can not differentiate** between a network stall and an application stall with healthchecks
alone. This is useful because the recovery between an application stall and a network stall
is different.

 - If the JavaScript application stalls it's because the JavaScript app can't keep up - recoverable.
 - If the ping-pong stalls it's the browser that can't keep up - recoverable.
 - If the network stalls for small frame it's usually a dropped connection  - not recoverable.

Because application stalls are nearly always recoverable, e.g you send a burst of small messages
to a JavaScript application that takes 100ms of CPU time to process each item (more common than 
you'd think with applications that do a lot of painting for visualisation), not only will the page
stall, but Chrome might _also_ stall and stop responding to ping pong frames. But this is recoverable,
once the burst is over the application will start responding to healthchecks again.

However, if a client: drops off a VPN; unplugs an Ethernet cable; or losses cell reception; the socket
will nearly always remain open, as it's the responsibility of the client's _operating system to send_ a FIN
packet.  If the server is prepared to accept client lagging at the application level, it has no way
of knowing if they're actually lagging at the network level.

What usually happens is that a server will keep sending data into a network socket until the operating
system gives the server application a `ENOTREADY` error when writing to a non-blocking socket.
There is absolutely no way to determine when you will get this; the size of the send buffer is a target,
not a cap if `tcp_moderate_rcvbuf` is set to true. It's also pretty damn large at 256kb minimum, so
if you're sending lots of small messages you can wait for an eternity for the socket to tell
you, "please no more". On Linux the default is 2 hours unless you set the keepalive time yourself.

This crate allows you to configure this on a per handler basis in axum.  Some websockets in your
application may want a smaller send buffer. Other websockets may want different TCP keep alive settings
because they have different connection profiles, even setting it dynamically:

```rust
use axum::{
    body::{Body, Bytes},
    response::IntoResponse,
    routing::get,
    Router,
    extract::ConnectInfo
};

use futures::StreamExt;
use std::{convert::Infallible, time::Duration};
use socket2::TcpKeepalive;
use axum_socket_backpressure::ConnectInfoWithSocket;
use tokio_stream::{wrappers::IntervalStream};


async fn handler(ConnectInfo(info): ConnectInfo<ConnectInfoWithSocket>) -> impl IntoResponse {
    let s = IntervalStream::new(tokio::time::interval(Duration::from_millis(200)))
        .enumerate()
        .map(move |(i, _)| {
            if i == 5 {
                info.as_socket_ref().set_tcp_keepalive(
                    &TcpKeepalive::new()
                        .with_time(Duration::from_secs(1))
                        .with_interval(Duration::from_secs(1))
                        .with_retries(1),
                ).unwrap();
            }

            Ok::<Bytes, Infallible>(Bytes::from(format!("tick {i}\n")))
        });

    Body::from_stream(s)
}
```

### Advice - Serious

Linux does not set tcp_keepalive on by default. This is an incredible footgun for anyone
serving a websocket.  You _may_ be saved from this dangerous default by reverse proxies.
But for the love of God, don't rely on AWS or DevOps to set them to rational defaults;
especially when it's so easy to set it yourself in Axum via the tap_io function on the
Listener trait.


```rust,no_run
// Provides tap_io
use axum::listener::Listener;

let default_keep_alive = TcpKeepalive::new()
   .with_time(Duration::from_secs(1))
   .with_interval(Duration::from_secs(1))
   .with_retries(1),
);

let listener = tokio::net::TcpListener::bind(addr).await.unwrap()
    // Always use this
    .tap_io(|tcp_stream: &mut TcpStream| {
        socket2::SockRef::from(tcp_stream).set_tcp_keepalive(default_keep_alive)
    });

let app = Router::new().into_make_service()
axum::serve(listener, app).await;
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
