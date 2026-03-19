# vibeio-http

High-performance HTTP server primitives for the `vibeio` runtime.

`vibeio-http` provides HTTP/1.0, HTTP/1.1, HTTP/2, and experimental HTTP/3
connection handlers behind a shared `HttpProtocol` trait. Each handler receives
an `http::Request<Incoming>` and returns an `http::Response<B>`, where
`B: http_body::Body<Data = bytes::Bytes>`.

## Highlights

- HTTP/1.x, HTTP/2, and HTTP/3 handlers with a common interface
- Streaming request/response bodies and trailers
- Automatic `100 Continue` support
- `103 Early Hints` support via `send_early_hints`
- HTTP/1 upgrade support (`prepare_upgrade` / `OnUpgrade`)
- Linux zero-copy response sending for HTTP/1.x (`h1-zerocopy` feature)
- Graceful shutdown support for all protocol handlers via `CancellationToken`

## Installation

```toml
[dependencies]
vibeio-http = "0.1"
```

By default, this crate enables: `h1`, `h1-zerocopy`, and `h2`.

### Feature flags

- `h1`: HTTP/1.x support
- `h2`: HTTP/2 support
- `h3`: HTTP/3 support (experimental, based on the `h3` crate)
- `h1-zerocopy`: Linux-only zero-copy HTTP/1.x response sending (`splice`-based)

For a smaller build, disable default features and opt in explicitly:

```toml
[dependencies]
vibeio-http = { version = "0.1", default-features = false, features = ["h1"] }
```

## Quickstart (HTTP/1.1)

```rust
use bytes::Bytes;
use http::Response;
use http_body_util::Full;
use vibeio::net::TcpListener;
use vibeio::RuntimeBuilder;
use vibeio_http::{Http1, Http1Options, HttpProtocol};

fn main() -> std::io::Result<()> {
    let runtime = RuntimeBuilder::new().enable_timer(true).build()?;

    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:8080")?;
        loop {
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true)?;
            let stream = stream.into_poll()?;

            vibeio::spawn(async move {
                if let Err(e) = Http1::new(stream, Http1Options::default())
                    .handle(|_request| async move {
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(
                            Bytes::from_static(b"Hello World"),
                        )))
                    })
                    .await
                {
                    eprintln!("HTTP error: {:?}", e);
                }
            });
        }
    })
}
```

## Early hints (`103`)

Use `send_early_hints` from your handler before returning the final response.

Notes:
- HTTP/2 and HTTP/3: available by default
- HTTP/1.x: requires `Http1Options::enable_early_hints(true)`

```rust
use http::{header, HeaderMap, Response};
use http_body_util::Empty;
use vibeio_http::send_early_hints;

let handler = |mut req| async move {
    let mut hints = HeaderMap::new();
    hints.insert(
        header::LINK,
        "</app.css>; rel=preload; as=style".parse().unwrap(),
    );
    let _ = send_early_hints(&mut req, hints).await;

    Ok::<_, std::convert::Infallible>(Response::new(Empty::<bytes::Bytes>::new()))
};
```

## HTTP/1 options

`Http1Options` supports:
- request head size and header count limits
- request head read timeout
- automatic `Date` header injection
- automatic `100 Continue`
- optional `103 Early Hints`
- vectored write toggle

`Http2Options` and `Http3Options` similarly expose:
- handshake/accept timeouts
- automatic `100 Continue`
- direct access to underlying protocol builders (`h2_builder`, `h3_builder`)

## HTTP/1 upgrades

For upgrade workflows (for example WebSocket-style handoff), call
`prepare_upgrade(&mut request)` in your handler and await the returned
`OnUpgrade` future after sending a `101 Switching Protocols` response.

The resolved `Upgraded` type implements `tokio::io::AsyncRead + AsyncWrite`.

## Linux zero-copy (HTTP/1.x)

When built with `h1-zerocopy` on Linux:

1. Convert your handler with `.zerocopy()`
2. Mark responses with `unsafe install_zerocopy(response, raw_fd)`

Responses carrying that extension are sent via kernel-assisted transfer. Other
responses fall back to normal HTTP/1 writes.

## Graceful shutdown

`Http1`, `Http2`, and `Http3` expose:

```rust,ignore
.graceful_shutdown_token(token)
```

Cancel the token to stop accepting new work and shut down the connection
cleanly.

## Crate API at a glance

- `HttpProtocol`: common protocol trait (`handle`, `handle_with_error_fn`)
- `Incoming`: type-erased request body type used in all handlers
- `send_early_hints`: send a `103 Early Hints` interim response
- `Http1` / `Http1Options`
- `Http2` / `Http2Options`
- `Http3` / `Http3Options`
- `prepare_upgrade`, `OnUpgrade`, `Upgraded` (HTTP/1 upgrade flow)

## License

[MIT](./LICENSE)
