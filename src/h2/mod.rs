// TODO: add support for extended CONNECT

mod date;
mod options;
mod send;
mod upgrade;

pub use options::*;
use pin_project_lite::pin_project;
use tokio_util::sync::CancellationToken;

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{Request, Response};
use http_body::{Body, Frame};

use crate::{
    early_hints::EarlyHintsReceiver,
    h2::{
        date::DateCache,
        send::{PipeToSendStream, SendBuf},
    },
    EarlyHints, HttpProtocol, Incoming, Upgrade, Upgraded,
};

static HTTP2_INVALID_HEADERS: [http::header::HeaderName; 5] = [
    http::header::HeaderName::from_static("keep-alive"),
    http::header::HeaderName::from_static("proxy-connection"),
    http::header::CONNECTION,
    http::header::TRANSFER_ENCODING,
    http::header::UPGRADE,
];

pub(crate) struct H2Body {
    recv: h2::RecvStream,
    data_done: bool,
}

impl H2Body {
    #[inline]
    fn new(recv: h2::RecvStream) -> Self {
        Self {
            recv,
            data_done: false,
        }
    }
}

impl Body for H2Body {
    type Data = Bytes;
    type Error = std::io::Error;

    #[inline]
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if !self.data_done {
            match self.recv.poll_data(cx) {
                Poll::Ready(Some(Ok(data))) => {
                    let _ = self.recv.flow_control().release_capacity(data.len());
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(h2_error_to_io(err)))),
                Poll::Ready(None) => self.data_done = true,
                Poll::Pending => return Poll::Pending,
            }
        }

        match self.recv.poll_trailers(cx) {
            Poll::Ready(Ok(Some(trailers))) => Poll::Ready(Some(Ok(Frame::trailers(trailers)))),
            Poll::Ready(Ok(None)) => Poll::Ready(None),
            Poll::Ready(Err(err)) => Poll::Ready(Some(Err(h2_error_to_io(err)))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[inline]
pub(super) fn h2_error_to_io(error: h2::Error) -> std::io::Error {
    if error.is_io() {
        error.into_io().unwrap_or(std::io::Error::other("io error"))
    } else {
        std::io::Error::other(error)
    }
}

#[inline]
pub(super) fn h2_reason_to_io(reason: h2::Reason) -> std::io::Error {
    std::io::Error::other(h2::Error::from(reason))
}

#[inline]
fn sanitize_response<ResB>(
    response: &mut Response<ResB>,
    send_date_header: bool,
    date_cache: &DateCache,
) where
    ResB: Body<Data = bytes::Bytes>,
{
    let response_headers = response.headers_mut();
    if send_date_header {
        if let Some(http_date) = date_cache.get_date_header_value() {
            response_headers
                .entry(http::header::DATE)
                .or_insert(http_date);
        }
    }
    for header in &HTTP2_INVALID_HEADERS {
        if let http::header::Entry::Occupied(entry) = response_headers.entry(header) {
            entry.remove();
        }
    }
    if response_headers
        .get(http::header::TE)
        .is_some_and(|v| v != "trailers")
    {
        response_headers.remove(http::header::TE);
    }
}

struct PendingUpgrade {
    tx: oneshot::Sender<Upgraded>,
    upgraded: std::sync::Arc<std::sync::atomic::AtomicBool>,
    recv_stream: h2::RecvStream,
}

pin_project! {
    struct H2Stream<Fut, ResB>
    where
        Fut: Future,
        ResB: Body<Data = bytes::Bytes>,
    {
        stream: h2::server::SendResponse<SendBuf<ResB::Data>>,
        #[pin]
        state: H2StreamState<Fut, ResB>,
    }
}

pin_project! {
    #[project = H2StreamStateProj]
    enum H2StreamState<Fut, ResB>
    where
        Fut: Future,
        ResB: Body<Data = bytes::Bytes>,
    {
        Service {
            #[pin]
            response_fut: Fut,
            early_hints_rx: EarlyHintsReceiver,
            date_cache: DateCache,
            send_date_header: bool,
            upgrade: Option<PendingUpgrade>,
            send_continue: bool,
            early_hints_open: bool,
        },
        Body {
            #[pin]
            pipe: PipeToSendStream<ResB>,
        },
    }
}

impl<Fut, ResB> H2Stream<Fut, ResB>
where
    Fut: Future,
    ResB: Body<Data = bytes::Bytes>,
{
    #[inline]
    const fn new(
        stream: h2::server::SendResponse<SendBuf<ResB::Data>>,
        response_fut: Fut,
        early_hints_rx: EarlyHintsReceiver,
        date_cache: DateCache,
        send_date_header: bool,
        upgrade: Option<PendingUpgrade>,
        send_continue: bool,
    ) -> Self {
        Self {
            stream,
            state: H2StreamState::Service {
                response_fut,
                early_hints_rx,
                date_cache,
                send_date_header,
                upgrade,
                send_continue,
                early_hints_open: true,
            },
        }
    }
}

impl<Fut, ResB, ResBE, ResE> Future for H2Stream<Fut, ResB>
where
    Fut: Future<Output = Result<Response<ResB>, ResE>>,
    ResB: Body<Data = bytes::Bytes, Error = ResBE>,
    ResE: std::error::Error,
    ResBE: std::error::Error,
{
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        loop {
            match this.state.as_mut().project() {
                H2StreamStateProj::Service {
                    response_fut,
                    early_hints_rx,
                    date_cache,
                    send_date_header,
                    upgrade,
                    send_continue,
                    early_hints_open,
                } => {
                    if *send_continue {
                        let mut response = Response::new(());
                        *response.status_mut() = http::StatusCode::CONTINUE;
                        let _ = this
                            .stream
                            .send_informational(response)
                            .map_err(h2_error_to_io);
                        *send_continue = false;
                    }

                    if let Poll::Ready(response_result) = response_fut.poll(cx) {
                        let Ok(mut response) = response_result else {
                            return Poll::Ready(());
                        };

                        sanitize_response(&mut response, *send_date_header, date_cache);

                        let response_is_end_stream = response.body().is_end_stream();
                        if !response_is_end_stream {
                            if let Some(content_length) = response.body().size_hint().exact() {
                                if !response
                                    .headers()
                                    .contains_key(http::header::CONTENT_LENGTH)
                                {
                                    response.headers_mut().insert(
                                        http::header::CONTENT_LENGTH,
                                        content_length.into(),
                                    );
                                }
                            }
                        }

                        let (response_parts, response_body) = response.into_parts();
                        let Ok(send) = this.stream.send_response(
                            Response::from_parts(response_parts, ()),
                            response_is_end_stream && upgrade.is_none(),
                        ) else {
                            return Poll::Ready(());
                        };

                        if let Some(PendingUpgrade {
                            tx,
                            upgraded,
                            recv_stream,
                        }) = upgrade.take()
                        {
                            if upgraded.load(std::sync::atomic::Ordering::Relaxed) {
                                let (upgraded, task) = self::upgrade::pair(send, recv_stream);
                                let _ = tx.send(Upgraded::new(upgraded, None));
                                vibeio::spawn(task);
                                return Poll::Ready(());
                            }
                        }

                        if response_is_end_stream {
                            return Poll::Ready(());
                        }

                        this.state.set(H2StreamState::Body {
                            pipe: PipeToSendStream::new(send, response_body),
                        });
                        continue;
                    }

                    match this.stream.poll_reset(cx) {
                        Poll::Ready(Ok(_)) | Poll::Ready(Err(_)) => return Poll::Ready(()),
                        Poll::Pending => {}
                    }

                    if *early_hints_open {
                        match early_hints_rx.poll_recv(cx) {
                            Poll::Ready(Some((headers, sender))) => {
                                let mut response = Response::new(());
                                *response.status_mut() = http::StatusCode::EARLY_HINTS;
                                *response.headers_mut() = headers;
                                sender
                                    .into_inner()
                                    .send(
                                        this.stream
                                            .send_informational(response)
                                            .map_err(h2_error_to_io),
                                    )
                                    .ok();
                                continue;
                            }
                            Poll::Ready(None) => {
                                *early_hints_open = false;
                                continue;
                            }
                            Poll::Pending => {}
                        }
                    }

                    return Poll::Pending;
                }
                H2StreamStateProj::Body { pipe } => {
                    return pipe.poll(cx).map(|_| ());
                }
            }
        }
    }
}

/// An HTTP/2 connection handler.
///
/// `Http2` wraps an async I/O stream (`Io`) and drives the HTTP/2 server
/// connection using the [`h2`] crate. It supports:
///
/// - Concurrent request stream handling
/// - Streaming request/response bodies and trailers
/// - Automatic `100 Continue` and `103 Early Hints` interim responses
/// - Per-connection `Date` header caching
/// - Graceful shutdown via a [`CancellationToken`]
///
/// # Construction
///
/// ```rust,ignore
/// let http2 = Http2::new(tcp_stream, Http2Options::default());
/// ```
///
/// # Serving requests
///
/// Use the [`HttpProtocol`] trait methods ([`handle`](HttpProtocol::handle) /
/// [`handle_with_error_fn`](HttpProtocol::handle_with_error_fn)) to drive the
/// connection to completion.
pub struct Http2<Io> {
    io_to_handshake: Option<Io>,
    date_header_value_cached: DateCache,
    options: Http2Options,
    cancel_token: Option<CancellationToken>,
}

impl<Io> Http2<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    /// Creates a new `Http2` connection handler wrapping the given I/O stream.
    ///
    /// The `options` value controls HTTP/2 protocol configuration, handshake
    /// and accept timeouts, and optional behaviour such as automatic
    /// `100 Continue` responses; see [`Http2Options`] for details.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let http2 = Http2::new(tcp_stream, Http2Options::default());
    /// ```
    #[inline]
    pub fn new(io: Io, options: Http2Options) -> Self {
        Self {
            io_to_handshake: Some(io),
            date_header_value_cached: DateCache::default(),
            options,
            cancel_token: None,
        }
    }

    /// Attaches a [`CancellationToken`] for graceful shutdown.
    ///
    /// When the token is cancelled, the handler sends HTTP/2 graceful shutdown
    /// signals (GOAWAY), stops accepting new streams, and exits cleanly.
    #[inline]
    pub fn graceful_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }
}

impl<Io> HttpProtocol for Http2<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
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
        ResB: http_body::Body<Data = bytes::Bytes, Error = ResBE> + Unpin + 'static,
        ResE: std::error::Error,
        ResBE: std::error::Error,
    {
        async move {
            let handshake_fut = self.options.h2.handshake(
                self.io_to_handshake
                    .take()
                    .ok_or_else(|| std::io::Error::other("no io to handshake"))?,
            );
            let mut h2 = (if let Some(timeout) = self.options.handshake_timeout {
                vibeio::time::timeout(timeout, handshake_fut).await
            } else {
                Ok(handshake_fut.await)
            })
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "handshake timeout"))?
            .map_err(|e| {
                if e.is_io() {
                    e.into_io().unwrap_or(std::io::Error::other("io error"))
                } else {
                    std::io::Error::other(e)
                }
            })?;

            while let Some(request) = {
                let res = {
                    let accept_fut_orig = h2.accept();
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
                    (Some(request), _) => request,
                    (None, graceful) => {
                        h2.graceful_shutdown();
                        let _ = h2.accept().await;
                        if graceful {
                            return Ok(());
                        }
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "accept timeout",
                        ));
                    }
                }
            } {
                let (request, stream) = match request {
                    Ok(d) => d,
                    Err(e) if e.is_go_away() => {
                        continue;
                    }
                    Err(e) if e.is_io() => {
                        return Err(e.into_io().unwrap_or(std::io::Error::other("io error")));
                    }
                    Err(e) => {
                        return Err(std::io::Error::other(e));
                    }
                };

                let date_cache = self.date_header_value_cached.clone();
                let (request_parts, recv_stream) = request.into_parts();
                let (request_body, upgrade) = if request_parts.method == http::Method::CONNECT {
                    (Incoming::Empty, Some(recv_stream))
                } else {
                    (Incoming::H2(H2Body::new(recv_stream)), None)
                };
                let mut request = Request::from_parts(request_parts, request_body);

                // 100 Continue
                let is_100_continue = self.options.send_continue_response
                    && request
                        .headers()
                        .get(http::header::EXPECT)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));

                // Install early hints
                let (early_hints, early_hints_rx) = EarlyHints::new_lazy();
                request.extensions_mut().insert(early_hints);

                // Install HTTP upgrade
                let upgrade = if let Some(recv_stream) = upgrade {
                    let (upgrade_tx, upgrade_rx) = oneshot::async_channel();
                    let upgrade = Upgrade::new(upgrade_rx);
                    let upgraded = upgrade.upgraded.clone();
                    request.extensions_mut().insert(upgrade);
                    Some(PendingUpgrade {
                        tx: upgrade_tx,
                        upgraded,
                        recv_stream,
                    })
                } else {
                    None
                };

                vibeio::spawn(H2Stream::new(
                    stream,
                    request_fn(request),
                    early_hints_rx,
                    date_cache,
                    self.options.send_date_header,
                    upgrade,
                    is_100_continue,
                ));
            }

            Ok(())
        }
    }
}
