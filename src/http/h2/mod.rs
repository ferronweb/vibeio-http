mod date;
mod options;

pub use options::*;
use tokio_util::sync::CancellationToken;

use std::rc::Rc;

use bytes::Bytes;
use http::{Request, Response};
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};

use crate::http::{h2::date::DateCache, EarlyHints, HttpProtocol, Incoming};

static HTTP2_INVALID_HEADERS: [http::header::HeaderName; 5] = [
    http::header::HeaderName::from_static("keep-alive"),
    http::header::HeaderName::from_static("proxy-connection"),
    http::header::TRANSFER_ENCODING,
    http::header::TE,
    http::header::UPGRADE,
];

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
    #[inline]
    pub fn new(io: Io, options: Http2Options) -> Self {
        Self {
            io_to_handshake: Some(io),
            date_header_value_cached: DateCache::default(),
            options,
            cancel_token: None,
        }
    }

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
        F: Fn(http::Request<super::Incoming>) -> Fut + 'static,
        Fut: std::future::Future<Output = Result<http::Response<ResB>, ResE>>,
        ResB: http_body::Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error,
    {
        async move {
            let request_fn = Rc::new(request_fn);
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
                match {
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

                    match if let Some(timeout) = self.options.handshake_timeout {
                        vibeio::time::timeout(timeout, accept_fut).await
                    } else {
                        Ok(accept_fut.await)
                    } {
                        Ok(futures_util::future::Either::Right((request, _))) => {
                            (Some(request), false)
                        }
                        Ok(futures_util::future::Either::Left((_, _))) => {
                            // Cancelled
                            (None, true)
                        }
                        Err(_) => {
                            // Timeout
                            (None, false)
                        }
                    }
                } {
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
                let (request, mut stream) = match request {
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
                let request_fn = request_fn.clone();
                let send_continue_response = self.options.send_continue_response;
                vibeio::spawn(async move {
                    let (request_parts, recv_stream) = request.into_parts();
                    let request_body_stream = futures_util::stream::unfold(
                        (recv_stream, false),
                        |(mut receive, mut is_body_finished)| async move {
                            loop {
                                if !is_body_finished {
                                    match receive.data().await {
                                        Some(Ok(data)) => {
                                            return Some((Ok(Frame::data(data)), (receive, false)))
                                        }
                                        Some(Err(err)) => {
                                            return Some((
                                                Err(std::io::Error::other(err.to_string())),
                                                (receive, false),
                                            ))
                                        }
                                        None => is_body_finished = true,
                                    }
                                } else {
                                    match receive.trailers().await {
                                        Ok(Some(trailers)) => {
                                            return Some((
                                                Ok(Frame::trailers(trailers)),
                                                (receive, true),
                                            ))
                                        }
                                        Ok(None) => {
                                            return None;
                                        }
                                        Err(err) => {
                                            return Some((
                                                Err(std::io::Error::other(err.to_string())),
                                                (receive, true),
                                            ))
                                        }
                                    }
                                }
                            }
                        },
                    );
                    let request_body = Incoming::new(StreamBody::new(request_body_stream));
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
                            let _ = stream.send_informational(response).map_err(|e| {
                                if e.is_io() {
                                    e.into_io().unwrap_or(std::io::Error::other("io error"))
                                } else {
                                    std::io::Error::other(e)
                                }
                            });
                        }
                    }

                    let (early_hints_tx, early_hints_rx) = async_channel::unbounded();
                    let early_hints = EarlyHints::new(early_hints_tx);
                    request.extensions_mut().insert(early_hints);

                    let response_result = {
                        let early_hints_fut = async {
                            let early_hints_rx = early_hints_rx;
                            while let Ok((headers, sender)) = early_hints_rx.recv().await {
                                let mut response = Response::new(());
                                *response.status_mut() = http::StatusCode::EARLY_HINTS;
                                *response.headers_mut() = headers;
                                sender
                                    .into_inner()
                                    .send(stream.send_informational(response).map_err(|e| {
                                        if e.is_io() {
                                            e.into_io().unwrap_or(std::io::Error::other("io error"))
                                        } else {
                                            std::io::Error::other(e)
                                        }
                                    }))
                                    .ok();
                            }
                            futures_util::future::pending::<Result<(), std::io::Error>>().await
                        };
                        let early_hints_fut_pin = std::pin::pin!(early_hints_fut);
                        let response_fut = request_fn(request);
                        let response_fut_pin = std::pin::pin!(response_fut);

                        match futures_util::future::select(early_hints_fut_pin, response_fut_pin)
                            .await
                        {
                            futures_util::future::Either::Left((_, _)) => unreachable!(),
                            futures_util::future::Either::Right((res, _)) => res,
                        }
                    };
                    let Ok(mut response) = response_result else {
                        // Return early if the request handler returns an error
                        return;
                    };

                    let response_headers = response.headers_mut();
                    if let Some(http_date) = date_cache.get_date_header_value() {
                        response_headers
                            .entry(http::header::DATE)
                            .or_insert(http_date);
                    }
                    if let Some(connection_header) = response_headers
                        .remove(http::header::CONNECTION)
                        .as_ref()
                        .and_then(|v| v.to_str().ok())
                    {
                        for name in connection_header.split(',') {
                            response_headers.remove(name.trim());
                        }
                    }
                    while response_headers.remove(http::header::CONNECTION).is_some() {}
                    for header in &HTTP2_INVALID_HEADERS {
                        while response_headers.remove(header).is_some() {}
                    }

                    let (response_parts, mut response_body) = response.into_parts();
                    let mut send = match stream
                        .send_response(Response::from_parts(response_parts, ()), false)
                    {
                        Ok(send) => send,
                        Err(_) => {
                            return;
                        }
                    };
                    let mut had_trailers = false;
                    while let Some(chunk) = response_body.frame().await {
                        match chunk {
                            Ok(frame) => {
                                if frame.is_data() {
                                    match frame.into_data() {
                                        Ok(data) => {
                                            send.reserve_capacity(data.len());
                                            std::future::poll_fn(|cx| send.poll_capacity(cx)).await;
                                            if send.send_data(data, false).is_err() {
                                                return;
                                            }
                                        }
                                        Err(_) => {
                                            return;
                                        }
                                    }
                                } else if frame.is_trailers() {
                                    match frame.into_trailers() {
                                        Ok(trailers) => {
                                            had_trailers = true;
                                            if send.send_trailers(trailers).is_err() {
                                                return;
                                            }
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
                    if !had_trailers {
                        if let Err(_err) = send.send_data(Bytes::new(), true) {}
                    }
                });
            }

            Ok(())
        }
    }
}
