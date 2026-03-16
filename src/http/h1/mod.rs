/*
TODO:

HTTP/1.1:
 - keep-alive
 - chunked encoding (req + resp)
 - request body streaming
 - trailers
 - 1xx responses
 - header size limits
 - slowloris protection
 - HTTP upgrades (CONNECT method and "Upgrade" header)
 - returning upgraded connection
 - handling buffered bytes during upgrade
 - pipelining handling (optional)

Performance:
 - zerocopy file responses
 - vectored writes (writev)
 - buffer reuse

Security:
 - read timeout
 - body size limits

Others:
 - 400 Bad Request fn for request parsing errors
 */

mod options;

use bytes::Bytes;
pub use options::Http1Options;

use std::{
    convert::Infallible,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};

use async_channel::Receiver;
use futures_util::stream::Stream;
use http::{HeaderName, HeaderValue, Method, Request, Response, Uri, Version, header};
use http_body::Body;
use http_body_util::{BodyExt, Empty};
use memchr::{memchr2, memchr3_iter};
use smallvec::SmallVec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::{HttpProtocol, Incoming};

pub struct Http1<Io> {
    io: Io,
    options: options::Http1Options,
}

impl<Io> Http1<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    #[inline]
    pub fn new(io: Io, options: options::Http1Options) -> Self {
        Self { io, options }
    }

    #[inline]
    async fn read_body_fn(
        &mut self,
        body_tx: &async_channel::Sender<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>,
        content_length: u64,
    ) -> Result<(), std::io::Error> {
        let mut remaining = content_length;
        while remaining > 0 {
            let mut buf: Box<[u8]> = vec![0u8; remaining.min(16384) as usize].into_boxed_slice();
            let n = self.io.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            let mut chunk = bytes::Bytes::from_owner(buf);
            chunk.truncate(n);
            remaining -= n as u64;

            let _ = body_tx.send(Ok(http_body::Frame::data(chunk))).await;
        }
        Ok(())
    }

    #[inline]
    async fn read_request(
        &mut self,
    ) -> Result<
        (
            Request<Incoming>,
            async_channel::Sender<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>,
        ),
        std::io::Error,
    > {
        let (head_buf, to_parse_length) = self.get_head().await?;

        // Parse HTTP request using httparse
        let mut headers =
            vec![httparse::EMPTY_HEADER; self.options.max_header_count].into_boxed_slice();
        let mut req = httparse::Request::new(&mut headers);
        let status = req
            .parse(&head_buf[..to_parse_length])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if status.is_partial() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "partial request head",
            ));
        }
        let leftover = bytes::Bytes::from(head_buf[to_parse_length..].to_vec());

        // Convert httparse HTTP request to `http` one
        let (body_tx, body_rx) = async_channel::bounded(2);
        let request_body = Http1Body {
            inner: Box::pin(body_rx),
            leftover,
        };
        let mut request = Request::new(Incoming::new(request_body));
        match req.version {
            Some(0) => *request.version_mut() = http::Version::HTTP_10,
            Some(1) => *request.version_mut() = http::Version::HTTP_11,
            _ => *request.version_mut() = http::Version::HTTP_11,
        };
        if let Some(method) = req.method {
            *request.method_mut() = Method::from_bytes(method.as_bytes())
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        if let Some(path) = req.path {
            *request.uri_mut() =
                Uri::from_str(path).map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        for header in req.headers {
            if header == &httparse::EMPTY_HEADER {
                // No more headers...
                break;
            }
            let name = HeaderName::from_bytes(header.name.as_bytes())
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let value = HeaderValue::from_bytes(header.value)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            request.headers_mut().append(name, value);
        }

        Ok((request, body_tx))
    }

    #[inline]
    async fn get_head(&mut self) -> Result<(bytes::Bytes, usize), std::io::Error> {
        let mut request_line_read = false;
        let mut buf: Box<[u8]> = vec![0u8; self.options.max_header_size].into_boxed_slice();
        let mut bytes_read: usize = 0;
        let mut whitespace_trimmed = None;
        loop {
            let mut temp_buf = [0u8; 4096];
            let n = self.io.read(&mut temp_buf).await?;
            if n == 0 {
                break;
            }
            let begin_search = bytes_read.saturating_sub(3);
            buf[bytes_read..(bytes_read + n).min(self.options.max_header_size)]
                .copy_from_slice(&temp_buf[..n]);
            bytes_read = (bytes_read + n).min(self.options.max_header_size);

            if whitespace_trimmed.is_none() {
                whitespace_trimmed = buf[..bytes_read]
                    .iter()
                    .position(|b| !b.is_ascii_whitespace());
            }

            if let Some(whitespace_trimmed) = whitespace_trimmed {
                // Validate first line (request line) before checking for header/body separator
                if !request_line_read {
                    let memchr =
                        memchr3_iter(b' ', b'\r', b'\n', &buf[whitespace_trimmed..bytes_read]);
                    let mut spaces = 0;
                    for separator_index in memchr {
                        if buf[whitespace_trimmed + separator_index] == b' ' {
                            if spaces >= 2 {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidInput,
                                    "bad request first line",
                                ));
                            }
                            spaces += 1;
                        } else if spaces == 2 {
                            request_line_read = true;
                            break;
                        } else {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "bad request first line",
                            ));
                        }
                    }
                }

                if request_line_read {
                    let begin_search = begin_search.max(whitespace_trimmed);
                    if let Some((separator_index, separator_len)) =
                        search_header_body_separator(&buf[begin_search..bytes_read])
                    {
                        let to_parse_length =
                            begin_search + separator_index + separator_len - whitespace_trimmed;
                        let mut buf_ro = bytes::Bytes::from_owner(buf);
                        buf_ro.truncate(bytes_read);
                        return Ok((
                            if whitespace_trimmed > 0 {
                                buf_ro.split_off(whitespace_trimmed)
                            } else {
                                buf_ro
                            },
                            to_parse_length,
                        ));
                    }
                }
            }

            if bytes_read >= self.options.max_header_size {
                break;
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request too large",
        ))
    }

    #[inline]
    async fn write_response(
        &mut self,
        mut response: Response<
            impl Body<Data = bytes::Bytes, Error = impl std::error::Error> + Unpin,
        >,
        version: Version,
    ) -> Result<(), std::io::Error> {
        // If the body has a size hint, set the Content-Length header if it's not already set
        if let Some(suggested_content_length) = response.body().size_hint().exact() {
            let headers = response.headers_mut();
            if !headers.contains_key(header::CONTENT_LENGTH) {
                headers.insert(
                    header::CONTENT_LENGTH,
                    suggested_content_length.try_into().map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid content length",
                        )
                    })?,
                );
            }
        }

        let mut head = Vec::new();
        if version == Version::HTTP_10 {
            head.extend_from_slice(b"HTTP/1.0 ");
        } else {
            head.extend_from_slice(b"HTTP/1.1 ");
        }
        head.extend_from_slice(response.status().as_str().as_bytes());
        if let Some(canonical_reason) = response.status().canonical_reason() {
            head.extend_from_slice(b" ");
            head.extend_from_slice(canonical_reason.as_bytes());
        }
        head.extend_from_slice(b"\r\n");
        for (name, value) in response.headers() {
            head.extend_from_slice(name.as_str().as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(value.as_bytes());
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(b"\r\n");

        let (write_queue_tx, write_queue_rx) = async_channel::bounded::<bytes::Bytes>(2);
        let body_write_future = Box::pin(async {
            let mut head_written = false;
            while let Some(chunk) = response.body_mut().frame().await {
                let chunk = chunk.map_err(|e| std::io::Error::other(e.to_string()))?;
                if let Ok(data) = chunk.into_data() {
                    if !head_written {
                        // Write head with first chunk at once
                        let mut head = head.split_off(0);
                        head.extend(data);
                        write_queue_tx
                            .send(Bytes::from_owner(head))
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                        head_written = true;
                    } else {
                        write_queue_tx
                            .send(data)
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                    }
                }
            }
            if !head_written {
                write_queue_tx
                    .send(Bytes::from_owner(head))
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            write_queue_tx.close();

            futures_util::future::pending::<Result<(), std::io::Error>>().await
        });
        let write_future = Box::pin(async {
            let mut write_buf = Vec::new();
            loop {
                let chunk = if write_buf.is_empty() {
                    let Some(chunk) = write_queue_rx.recv().await.ok() else {
                        // Channel closed, break out of loop
                        break;
                    };
                    Some(chunk)
                } else {
                    let result = write_queue_rx.try_recv();
                    if result.as_ref().is_err_and(|e| e.is_closed()) {
                        break;
                    }
                    result.ok()
                };
                if let Some(chunk) = chunk {
                    write_buf.extend(chunk);
                }
                let written = self.io.write(&write_buf).await?;
                let to_write = write_buf.split_off(written);
                write_buf = to_write;
            }
            self.io.write_all(&write_buf).await?;
            Ok::<(), std::io::Error>(())
        });

        match futures_util::future::select(body_write_future, write_future).await {
            futures_util::future::Either::Left((result, _)) => {
                result?;
            }
            futures_util::future::Either::Right((result, _)) => {
                result?;
            }
        }

        Ok(())
    }
}

impl<Io> HttpProtocol for Http1<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    #[allow(clippy::manual_async_fn)]
    #[inline]
    fn handle_with_error_fn<F, Fut, ResB, ResBE, ResE, EF, EFut, EResB, EResBE, EResE>(
        mut self,
        mut request_fn: F,
        error_fn: EF,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: FnMut(Request<Incoming>) -> Fut,
        Fut: std::future::Future<Output = Result<Response<ResB>, ResE>>,
        ResB: Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error,
        EF: FnOnce(bool) -> EFut,
        EFut: std::future::Future<Output = Result<Response<EResB>, EResE>>,
        EResB: Body<Data = bytes::Bytes, Error = EResBE> + Unpin,
        EResE: std::error::Error,
        EResBE: std::error::Error,
    {
        let mut keep_alive = true;

        async move {
            while keep_alive {
                let (request, body_tx) = match self.read_request().await {
                    Ok((request, body_tx)) => (request, body_tx),
                    Err(e) => {
                        if let Ok(response) = error_fn(false).await {
                            let _ = self.write_response(response, Version::HTTP_11).await;
                        }
                        return Err(e);
                    }
                };

                // Connection header detection
                let connection_header_split = request
                    .headers()
                    .get(header::CONNECTION)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.split(",").map(|v| v.trim()));
                let is_connection_close = connection_header_split
                    .clone()
                    .is_some_and(|mut split| split.any(|v| v.eq_ignore_ascii_case("close")));
                let is_connection_keep_alive = connection_header_split
                    .is_some_and(|mut split| split.any(|v| v.eq_ignore_ascii_case("keep-alive")));
                keep_alive = !is_connection_close
                    && (is_connection_keep_alive || request.version() == http::Version::HTTP_11);

                let version = request.version();

                // Content-Length header
                let content_length = request
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);

                // Get HTTP response
                let mut response = {
                    let read_body_fut = Box::pin(self.read_body_fn(&body_tx, content_length));
                    let request_fut = Box::pin(request_fn(request));
                    let select_either =
                        futures_util::future::select(request_fut, read_body_fut).await;

                    let (response, body_fut) = match select_either {
                        futures_util::future::Either::Left((response, read_body_fut)) => {
                            (response, Some(read_body_fut))
                        }
                        futures_util::future::Either::Right((result, request_fut)) => {
                            if let Err(e) = result {
                                return Err(e);
                            }
                            (request_fut.await, None)
                        }
                    };

                    // Drain away remaining body
                    if let Some(body_fut) = body_fut {
                        body_fut.await?;
                    }

                    response.map_err(|e| std::io::Error::other(e.to_string()))?
                };

                if keep_alive {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
                } else {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }

                if self.options.send_date_header {
                    response.headers_mut().insert(
                        header::DATE,
                        HeaderValue::from_str(&httpdate::fmt_http_date(
                            std::time::SystemTime::now(),
                        ))
                        .map_err(|e| std::io::Error::other(e.to_string()))?,
                    );
                }

                // Write response to IO
                self.write_response(response, version).await?;
            }
            Ok(())
        }
    }

    fn handle<F, Fut, ResB, ResBE, ResE>(
        self,
        request_fn: F,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: FnMut(Request<Incoming>) -> Fut,
        Fut: std::future::Future<Output = Result<Response<ResB>, ResE>>,
        ResB: Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error,
    {
        self.handle_with_error_fn(request_fn, |is_timeout| async move {
            let mut response = Response::builder();
            if is_timeout {
                response = response.status(http::StatusCode::REQUEST_TIMEOUT);
            } else {
                response = response.status(http::StatusCode::BAD_REQUEST);
            }
            Ok::<_, http::Error>(response.body(Empty::new())?)
        })
    }
}

struct Http1Body {
    #[allow(clippy::type_complexity)]
    inner: Pin<Box<Receiver<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>>>,
    leftover: bytes::Bytes,
}

impl Body for Http1Body {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self;
        if !this.leftover.is_empty() {
            let chunk = this.leftover.split_to(4096);
            return Poll::Ready(Some(Ok(http_body::Frame::data(chunk))));
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Searches for the header/body separator in a given slice.
/// Returns the index of the separator and the length of the separator.
#[inline]
fn search_header_body_separator(slice: &[u8]) -> Option<(usize, usize)> {
    if slice.len() < 2 {
        // Slice too short
        return None;
    }
    let mut last_chars: SmallVec<[u8; 4]> = SmallVec::with_capacity(4);
    let mut index = 0;
    while let Some(found_index) = memchr2(b'\r', b'\n', &slice[index..]) {
        if found_index > 0 {
            // Not "\n\n", "\r\n\r\n", "\r\r", nor "\n\n"...
            last_chars.clear();
        }
        let ch = slice[index + found_index];
        if last_chars.get(last_chars.len().saturating_sub(1)) == Some(&ch) {
            // "\n\n" or "\r\r"
            return Some((index + found_index - 1, 2));
        } else {
            last_chars.push(ch);
        }
        if last_chars.len() == 4 {
            // "\r\n\r\n" or "\n\r\n\r"
            return Some((index + found_index - 3, 4));
        }
        index += found_index + 1;
        if index >= slice.len() {
            break;
        }
    }
    None
}
