mod date;
mod options;
mod upgrade;

use futures_util::FutureExt;
pub use options::*;
use tokio_util::sync::CancellationToken;

use std::{
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use http::{Request, Response};
use http_body::{Body, Frame};
use http_body_util::BodyExt;

use crate::{h3::date::DateCache, EarlyHints, HttpProtocol, Incoming, Upgrade, Upgraded};

static HTTP3_INVALID_HEADERS: [http::header::HeaderName; 5] = [
    http::header::HeaderName::from_static("keep-alive"),
    http::header::HeaderName::from_static("proxy-connection"),
    http::header::CONNECTION,
    http::header::TRANSFER_ENCODING,
    http::header::UPGRADE,
];

struct H3BodyState<S, B> {
    recv: h3::server::RequestStream<S, B>,
    data_done: bool,
}

pub(crate) struct H3Body<S, B> {
    inner: futures_util::lock::Mutex<H3BodyState<S, B>>,
}

impl<S, B> H3Body<S, B> {
    #[inline]
    fn new(recv: h3::server::RequestStream<S, B>) -> Self {
        Self {
            inner: futures_util::lock::Mutex::new(H3BodyState {
                recv,
                data_done: false,
            }),
        }
    }
}

impl<S, B> Body for H3Body<S, B>
where
    S: h3::quic::RecvStream + Send,
    B: bytes::Buf + Send,
{
    type Data = Bytes;
    type Error = std::io::Error;

    #[inline]
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut inner = match self.inner.lock().poll_unpin(cx) {
            Poll::Ready(inner) => inner,
            Poll::Pending => return Poll::Pending,
        };

        if !inner.data_done {
            match inner.recv.poll_recv_data(cx) {
                Poll::Ready(Ok(Some(mut data))) => {
                    let data = data.copy_to_bytes(data.remaining());
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Poll::Ready(Ok(None)) => inner.data_done = true,
                Poll::Ready(Err(err)) => {
                    return Poll::Ready(Some(Err(h3_stream_error_to_io(err))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        match inner.recv.poll_recv_trailers(cx) {
            Poll::Ready(Ok(Some(trailers))) => Poll::Ready(Some(Ok(Frame::trailers(trailers)))),
            Poll::Ready(Ok(None)) => Poll::Ready(None),
            Poll::Ready(Err(err)) => Poll::Ready(Some(Err(h3_stream_error_to_io(err)))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[inline]
fn h3_connection_error_to_io(error: h3::error::ConnectionError) -> std::io::Error {
    // There could be logic to construct different error kinds,
    // but for now we just convert to other, because for private enum variants.
    std::io::Error::other(error)
}

#[inline]
fn h3_stream_error_to_io(error: h3::error::StreamError) -> std::io::Error {
    // There could be logic to construct different error kinds,
    // but for now we just convert to other, because for private enum variants.
    std::io::Error::other(error)
}

#[inline]
fn remove_invalid_http3_headers(headers: &mut http::HeaderMap) {
    for header in &HTTP3_INVALID_HEADERS {
        if let http::header::Entry::Occupied(entry) = headers.entry(header) {
            entry.remove();
        }
    }
    if headers
        .get(http::header::TE)
        .is_some_and(|v| v != "trailers")
    {
        headers.remove(http::header::TE);
    }
}

/// An HTTP/3 connection handler.
///
/// `Http3` wraps a QUIC connection (`Io`) and drives the HTTP/3 server
/// connection using the [`h3`] crate. It supports:
///
/// - Concurrent request stream handling
/// - Streaming request/response bodies and trailers
/// - Automatic `100 Continue` and `103 Early Hints` interim responses
/// - Per-connection `Date` header caching
/// - Graceful shutdown via a [`CancellationToken`]
///
/// > **Note:** The underlying [`h3`] crate is still experimental. The API may
/// > change in future releases and there may be occasional bugs. Use with care
/// > in production environments.
///
/// # Construction
///
/// ```rust,ignore
/// let http3 = Http3::new(quic_connection, Http3Options::default());
/// ```
///
/// # Serving requests
///
/// Use the [`HttpProtocol`] trait methods ([`handle`](HttpProtocol::handle) /
/// [`handle_with_error_fn`](HttpProtocol::handle_with_error_fn)) to drive the
/// connection to completion.
pub struct Http3<Io> {
    io_to_handshake: Option<Io>,
    date_header_value_cached: DateCache,
    options: Http3Options,
    cancel_token: Option<CancellationToken>,
}

impl<Io> Http3<Io>
where
    Io: h3::quic::Connection<bytes::Bytes> + Unpin + 'static,
{
    /// Creates a new `Http3` connection handler wrapping the given QUIC
    /// connection.
    ///
    /// The `options` value controls HTTP/3 protocol configuration, connection
    /// setup and accept timeouts, and optional behaviour such as automatic
    /// `100 Continue` responses; see [`Http3Options`] for details.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let http3 = Http3::new(quic_connection, Http3Options::default());
    /// ```
    #[inline]
    pub fn new(io: Io, options: Http3Options) -> Self {
        Self {
            io_to_handshake: Some(io),
            date_header_value_cached: DateCache::default(),
            options,
            cancel_token: None,
        }
    }

    /// Attaches a [`CancellationToken`] for graceful shutdown.
    ///
    /// When the token is cancelled, the handler sends an HTTP/3 graceful
    /// shutdown signal (via `h3`'s `shutdown`), stops accepting new request
    /// streams, and exits cleanly.
    #[inline]
    pub fn graceful_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }
}

impl<Io> HttpProtocol for Http3<Io>
where
    Io: h3::quic::Connection<bytes::Bytes> + Unpin + 'static,
    <Io as h3::quic::OpenStreams<bytes::Bytes>>::BidiStream:
        h3::quic::BidiStream<bytes::Bytes> + Send + 'static,
    <<Io as h3::quic::OpenStreams<bytes::Bytes>>::BidiStream as h3::quic::BidiStream<
        bytes::Bytes,
    >>::RecvStream: Send,
{
    #[allow(clippy::manual_async_fn)]
    #[inline]
    fn handle<F, Fut, ResB, ResBE, ResE>(
        mut self,
        request_fn: F,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: Fn(Request<super::Incoming>) -> Fut + 'static,
        Fut: std::future::Future<Output = Result<Response<ResB>, ResE>> + 'static,
        ResB: http_body::Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error,
    {
        async move {
            let request_fn = Rc::new(request_fn);
            let handshake_fut = self.options.h3.build(
                self.io_to_handshake
                    .take()
                    .ok_or_else(|| std::io::Error::other("no io to handshake"))?,
            );
            let mut h3 = (if let Some(timeout) = self.options.handshake_timeout {
                vibeio::time::timeout(timeout, handshake_fut).await
            } else {
                Ok(handshake_fut.await)
            })
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "handshake timeout"))?
            .map_err(h3_connection_error_to_io)?;

            loop {
                let resolver = {
                    let res = {
                        let accept_fut_orig = h3.accept();
                        let accept_fut_orig_pin = std::pin::pin!(accept_fut_orig);
                        let cancel_token = self.cancel_token.clone();
                        let cancel_fut = async move {
                            if let Some(token) = cancel_token {
                                token.cancelled().await
                            } else {
                                futures_util::future::pending().await
                            }
                        };
                        let cancel_fut_pin = std::pin::pin!(cancel_fut);
                        let accept_fut =
                            futures_util::future::select(cancel_fut_pin, accept_fut_orig_pin);

                        match if let Some(timeout) = self.options.accept_timeout {
                            vibeio::time::timeout(timeout, accept_fut).await
                        } else {
                            Ok(accept_fut.await)
                        } {
                            Ok(futures_util::future::Either::Right((request, _))) => {
                                (Some(request), false)
                            }
                            Ok(futures_util::future::Either::Left((_, _))) => {
                                // Canceled
                                (None, true)
                            }
                            Err(_) => {
                                // Timeout
                                (None, false)
                            }
                        }
                    };
                    match res {
                        (Some(resolver), _) => resolver,
                        (None, graceful) => {
                            if let Err(err) = h3.shutdown(0).await {
                                if !err.is_h3_no_error() {
                                    return Err(h3_connection_error_to_io(err));
                                }
                            }
                            let _ = h3.accept().await;
                            if graceful {
                                return Ok(());
                            }
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "accept timeout",
                            ));
                        }
                    }
                };

                let resolver = match resolver {
                    Ok(Some(resolver)) => resolver,
                    Ok(None) => break,
                    Err(err) if err.is_h3_no_error() => break,
                    Err(err) => return Err(h3_connection_error_to_io(err)),
                };

                let date_cache = self.date_header_value_cached.clone();
                let request_fn = request_fn.clone();
                let send_continue_response = self.options.send_continue_response;
                vibeio::spawn(async move {
                    let (request, stream) = match resolver.resolve_request().await {
                        Ok(resolved) => resolved,
                        Err(_) => {
                            return;
                        }
                    };
                    let (mut send, receive) = stream.split();
                    let (request_parts, _) = request.into_parts();
                    let (request_body, upgrade) = if request_parts.method == http::Method::CONNECT {
                        (Incoming::Empty, Some(receive))
                    } else {
                        (Incoming::Boxed(Box::pin(H3Body::new(receive))), None)
                    };
                    let mut request = Request::from_parts(request_parts, request_body);

                    // 100 Continue
                    if send_continue_response {
                        let is_100_continue = request
                            .headers()
                            .get(http::header::EXPECT)
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
                        if is_100_continue {
                            let mut response = Response::new(());
                            *response.status_mut() = http::StatusCode::CONTINUE;
                            if send.send_response(response).await.is_err() {
                                return;
                            }
                        }
                    }

                    // Install early hints
                    let (early_hints_tx, early_hints_rx) = kanal::unbounded_async();
                    let early_hints = EarlyHints::new(early_hints_tx);
                    request.extensions_mut().insert(early_hints);

                    // Install HTTP upgrade
                    let upgrade = if let Some(recv_stream) = upgrade {
                        let (upgrade_tx, upgrade_rx) = oneshot::async_channel();
                        let upgrade = Upgrade::new(upgrade_rx);
                        let upgraded = upgrade.upgraded.clone();
                        request.extensions_mut().insert(upgrade);
                        Some((upgrade_tx, upgraded, recv_stream))
                    } else {
                        None
                    };

                    let mut response_fut = std::pin::pin!(request_fn(request));
                    let mut early_hints_open = true;
                    let response_result = loop {
                        if !early_hints_open {
                            break response_fut.as_mut().await;
                        }

                        let early_hints_recv_fut = early_hints_rx.recv();
                        let mut early_hints_recv_fut = std::pin::pin!(early_hints_recv_fut);
                        let next = std::future::poll_fn(|cx| {
                            if let Poll::Ready(res) = response_fut.as_mut().poll(cx) {
                                return Poll::Ready(futures_util::future::Either::Left(res));
                            }

                            match early_hints_recv_fut.as_mut().poll(cx) {
                                Poll::Ready(msg) => {
                                    Poll::Ready(futures_util::future::Either::Right(msg))
                                }
                                Poll::Pending => Poll::Pending,
                            }
                        })
                        .await;

                        match next {
                            futures_util::future::Either::Left(response_result) => {
                                break response_result;
                            }
                            futures_util::future::Either::Right(Ok((headers, sender))) => {
                                let mut response = Response::new(());
                                *response.status_mut() = http::StatusCode::EARLY_HINTS;
                                *response.headers_mut() = headers;
                                sender
                                    .into_inner()
                                    .send(
                                        send.send_response(response)
                                            .await
                                            .map_err(h3_stream_error_to_io),
                                    )
                                    .ok();
                            }
                            futures_util::future::Either::Right(Err(_)) => {
                                early_hints_open = false;
                            }
                        }
                    };

                    let Ok(mut response) = response_result else {
                        // Return early if the request handler returns an error
                        return;
                    };

                    {
                        let response_headers = response.headers_mut();
                        if self.options.send_date_header {
                            if let Some(http_date) = date_cache.get_date_header_value() {
                                response_headers
                                    .entry(http::header::DATE)
                                    .or_insert(http_date);
                            }
                        }
                        remove_invalid_http3_headers(response_headers);
                    }

                    let response_is_end_stream = response.body().is_end_stream();
                    if !response_is_end_stream {
                        if let Some(content_length) = response.body().size_hint().exact() {
                            if !response
                                .headers()
                                .contains_key(http::header::CONTENT_LENGTH)
                            {
                                response
                                    .headers_mut()
                                    .insert(http::header::CONTENT_LENGTH, content_length.into());
                            }
                        }
                    }

                    let (response_parts, mut response_body) = response.into_parts();
                    if send
                        .send_response(Response::from_parts(response_parts, ()))
                        .await
                        .is_err()
                    {
                        return;
                    }

                    if let Some((upgrade_tx, upgraded, recv_stream)) = upgrade {
                        if upgraded.load(std::sync::atomic::Ordering::Relaxed) {
                            let (upgraded, task) = self::upgrade::pair(send, recv_stream);
                            let _ = upgrade_tx.send(Upgraded::new(upgraded, None));
                            task.await;
                            return;
                        }
                    }

                    if !response_is_end_stream {
                        while let Some(chunk) = response_body.frame().await {
                            match chunk {
                                Ok(frame) => {
                                    if frame.is_data() {
                                        match frame.into_data() {
                                            Ok(data) => {
                                                if send.send_data(data).await.is_err() {
                                                    return;
                                                }
                                            }
                                            Err(_) => {
                                                return;
                                            }
                                        }
                                    } else if frame.is_trailers() {
                                        match frame.into_trailers() {
                                            Ok(mut trailers) => {
                                                remove_invalid_http3_headers(&mut trailers);
                                                if send.send_trailers(trailers).await.is_err() {
                                                    return;
                                                }
                                                break;
                                            }
                                            Err(_) => {
                                                return;
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    return;
                                }
                            }
                        }
                    }

                    if let Err(err) = send.finish().await {
                        if !err.is_h3_no_error() {
                            // No-op: stream is already aborted.
                        }
                    }
                });
            }

            Ok(())
        }
    }
}
