mod options;
mod tests;
mod upgrade;

pub use options::*;
use tokio_util::sync::CancellationToken;
pub use upgrade::*;

use std::{
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};

use async_channel::Receiver;
use bytes::{Buf, Bytes};
use futures_util::stream::Stream;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, Uri, Version, header};
use http_body::Body;
use http_body_util::{BodyExt, Empty};
use memchr::{memchr2, memchr3_iter, memmem};
use smallvec::SmallVec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::{HttpProtocol, Incoming};

pub struct Http1<Io> {
    io: Io,
    leftover: Option<bytes::Bytes>,
    options: options::Http1Options,
    cancel_token: Option<CancellationToken>,
}

impl<Io> Http1<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    #[inline]
    pub fn new(io: Io, options: options::Http1Options) -> Self {
        Self {
            io,
            leftover: None,
            options,
            cancel_token: None,
        }
    }

    #[inline]
    pub fn graceful_shutdown_token(&mut self, token: CancellationToken) {
        self.cancel_token = Some(token);
    }

    #[inline]
    async fn read_with_leftover(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        if let Some(leftover) = &mut self.leftover {
            let len = leftover.remaining().min(buf.len());
            if len != 0 {
                leftover.copy_to_slice(&mut buf[..len]);
            }
            if !leftover.has_remaining() {
                self.leftover = None;
            }
            if len != 0 {
                return Ok(len);
            }
        };
        self.io.read(buf).await
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
            let n = self.read_with_leftover(&mut buf).await?;
            if n == 0 {
                break;
            }
            let mut chunk = bytes::Bytes::from_owner(buf);
            chunk.truncate(n);
            remaining -= n as u64;

            let _ = body_tx.send(Ok(http_body::Frame::data(chunk))).await;
        }
        body_tx.close(); // Close the body_tx channel to signal EOF
        Ok(())
    }

    #[inline]
    async fn read_body_chunk(
        &mut self,
        would_have_trailers: bool,
    ) -> Result<bytes::Bytes, std::io::Error> {
        let len = {
            let mut len_buf: Box<[u8]> = vec![0u8; 48].into_boxed_slice();
            let mut len_buf_pos = 0;
            loop {
                if len_buf_pos >= len_buf.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "chunk length buffer overflow",
                    ));
                }
                let n = self.read_with_leftover(&mut len_buf[len_buf_pos..]).await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "unexpected EOF",
                    ));
                }
                len_buf_pos += n;
                if let Some(pos) = memmem::find(&len_buf[..len_buf_pos], b"\r\n") {
                    let numbers = std::str::from_utf8(&len_buf[..pos]).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid chunk length")
                    })?;
                    let len = usize::from_str_radix(numbers, 16).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid chunk length")
                    })?;
                    // Ignore the trailing CRLF
                    if let Some(leftover) = self.leftover.take() {
                        let mut new_leftover = Vec::new();
                        new_leftover.extend_from_slice(&len_buf[pos + 2..]);
                        new_leftover.extend(leftover);
                        self.leftover = Some(bytes::Bytes::from(new_leftover));
                    } else {
                        let mut leftover = Bytes::from_owner(len_buf);
                        leftover.advance(pos + 2);
                        self.leftover = Some(leftover);
                    }
                    break len;
                }
            }
        };
        let mut chunk = vec![0u8; len + 2];
        let mut read = 0;
        if len == 0 && would_have_trailers {
            return Ok(bytes::Bytes::new()); // Empty terminating chunk
        }
        // + 2, because we need to read the trailing CRLF
        while read < len + 2 {
            let n = self.read_with_leftover(&mut chunk[read..]).await?;
            read += n;
        }
        chunk.truncate(len);
        Ok(bytes::Bytes::from(chunk))
    }

    #[inline]
    async fn read_trailers(&mut self) -> Result<Option<http::HeaderMap>, std::io::Error> {
        let mut buf: Box<[u8]> = vec![0u8; self.options.max_header_size].into_boxed_slice();
        let mut bytes_read: usize = 0;
        loop {
            let mut temp_buf = [0u8; 4096];
            let n = self.read_with_leftover(&mut temp_buf).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF",
                ));
            }
            let begin_search = bytes_read.saturating_sub(3);
            buf[bytes_read..(bytes_read + n).min(self.options.max_header_size)]
                .copy_from_slice(&temp_buf[..n]);
            bytes_read = (bytes_read + n).min(self.options.max_header_size);

            if bytes_read > 2 && buf[0] == b'\r' && buf[1] == b'\n' {
                // No trailers, return None
                return Ok(None);
            }

            if let Some(separator_index) = memmem::find(&buf[begin_search..bytes_read], b"\r\n\r\n")
            {
                let to_parse_length = begin_search + separator_index + 4;
                let mut buf_ro = bytes::Bytes::from_owner(buf);
                buf_ro.truncate(bytes_read);
                let leftover_to_add = buf_ro.split_off(to_parse_length);
                if let Some(leftover) = self.leftover.take() {
                    let mut new_leftover = Vec::new();
                    new_leftover.extend(leftover_to_add);
                    new_leftover.extend(leftover);
                    self.leftover = Some(bytes::Bytes::from(new_leftover));
                } else {
                    self.leftover = Some(leftover_to_add);
                }

                // Parse trailers using `httparse` crate's header parsing
                let mut httparse_trailers =
                    vec![httparse::EMPTY_HEADER; self.options.max_header_count].into_boxed_slice();
                let status = httparse::parse_headers(&buf_ro[..], &mut httparse_trailers)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                if let httparse::Status::Complete((_, trailers)) = status {
                    let mut trailers_constructed = HeaderMap::new();
                    for header in trailers {
                        if header == &httparse::EMPTY_HEADER {
                            // No more headers...
                            break;
                        }
                        let name = HeaderName::from_bytes(header.name.as_bytes())
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                        let value = HeaderValue::from_bytes(header.value)
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                        trailers_constructed.append(name, value);
                    }

                    return Ok(Some(trailers_constructed));
                } else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "trailer headers incomplete",
                    ));
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
    async fn read_chunked_body_fn(
        &mut self,
        body_tx: &async_channel::Sender<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>,
        would_have_trailers: bool,
    ) -> Result<(), std::io::Error> {
        loop {
            let chunk = self.read_body_chunk(would_have_trailers).await?;
            if chunk.is_empty() {
                break;
            }

            let _ = body_tx.send(Ok(http_body::Frame::data(chunk))).await;
        }
        if would_have_trailers {
            // Trailers
            let trailers = self.read_trailers().await?;
            if let Some(trailers) = trailers {
                let _ = body_tx.send(Ok(http_body::Frame::trailers(trailers))).await;
            }
        }
        body_tx.close(); // Close the body_tx channel to signal EOF
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
        let leftover_to_add = bytes::Bytes::from(head_buf[to_parse_length..].to_vec());
        if let Some(leftover) = self.leftover.take() {
            let mut new_leftover = Vec::new();
            new_leftover.extend(leftover_to_add);
            new_leftover.extend(leftover);
            self.leftover = Some(bytes::Bytes::from(new_leftover));
        } else {
            self.leftover = Some(leftover_to_add);
        }

        // Convert httparse HTTP request to `http` one
        let (body_tx, body_rx) = async_channel::bounded(2);
        let request_body = Http1Body {
            inner: Box::pin(body_rx),
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
            let n = self.read_with_leftover(&mut temp_buf).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF",
                ));
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
        write_trailers: bool,
    ) -> Result<(), std::io::Error> {
        // Date header
        if self.options.send_date_header {
            response.headers_mut().insert(
                header::DATE,
                HeaderValue::from_str(&httpdate::fmt_http_date(std::time::SystemTime::now()))
                    .map_err(|e| std::io::Error::other(e.to_string()))?,
            );
        }

        // If the body has a size hint, set the Content-Length header if it's not already set
        if let Some(suggested_content_length) = response.body().size_hint().exact() {
            let headers = response.headers_mut();
            if !headers.contains_key(header::CONTENT_LENGTH) {
                headers.insert(header::CONTENT_LENGTH, suggested_content_length.into());
            }
        }

        let chunked = response
            .headers()
            .get(header::TRANSFER_ENCODING)
            .map(|v| {
                v.to_str().ok().is_some_and(|s| {
                    s.split(',')
                        .any(|s| s.trim().eq_ignore_ascii_case("chunked"))
                })
            })
            .unwrap_or_else(|| {
                response
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .is_none_or(|s| s.parse::<u64>().is_err())
            });

        if chunked {
            response.headers_mut().insert(
                header::TRANSFER_ENCODING,
                HeaderValue::from_static("chunked"),
            );
            while response
                .headers_mut()
                .remove(header::CONTENT_LENGTH)
                .is_some()
            {}
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
            let mut trailers_written = false;
            while let Some(chunk) = response.body_mut().frame().await {
                let chunk = chunk.map_err(|e| std::io::Error::other(e.to_string()))?;
                match chunk.into_data() {
                    Ok(data) => {
                        if !head_written {
                            // Write head with first chunk at once
                            let mut head = head.split_off(0);
                            if chunked {
                                // Chunked encoding
                                head.extend_from_slice(format!("{:X}", data.len()).as_bytes());
                                head.extend_from_slice(b"\r\n");
                            }
                            head.extend(data);
                            if chunked {
                                head.extend_from_slice(b"\r\n");
                            }
                            write_queue_tx
                                .send(Bytes::from_owner(head))
                                .await
                                .map_err(|e| std::io::Error::other(e.to_string()))?;
                            head_written = true;
                        } else if chunked {
                            let chunked_data = Vec::new();
                            head.extend_from_slice(
                                format!("{:X}", data.len()).to_string().as_bytes(),
                            );
                            head.extend_from_slice(b"\r\n");
                            head.extend(data);
                            head.extend_from_slice(b"\r\n");
                            write_queue_tx
                                .send(Bytes::from_owner(chunked_data))
                                .await
                                .map_err(|e| std::io::Error::other(e.to_string()))?;
                        } else {
                            write_queue_tx
                                .send(data)
                                .await
                                .map_err(|e| std::io::Error::other(e.to_string()))?;
                        }
                    }
                    Err(chunk) => {
                        if let Ok(trailers) = chunk.into_trailers() {
                            if write_trailers {
                                let mut trail = Vec::new();
                                trail.extend_from_slice(b"0\r\n");
                                let mut current_header_name = None;
                                for (name, value) in trailers {
                                    if let Some(name) = name {
                                        current_header_name = Some(name);
                                    };
                                    if let Some(current_header_name) = &current_header_name {
                                        trail.extend_from_slice(
                                            current_header_name.as_str().as_bytes(),
                                        );
                                        trail.extend_from_slice(b": ");
                                        trail.extend_from_slice(value.as_bytes());
                                        trail.extend_from_slice(b"\r\n");
                                    }
                                }
                                trail.extend_from_slice(b"\r\n");
                                write_queue_tx
                                    .send(Bytes::from_owner(trail))
                                    .await
                                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                                trailers_written = true;
                            }
                            break;
                        }
                    }
                };
            }
            if !head_written {
                write_queue_tx
                    .send(Bytes::from_owner(head))
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            if chunked && !trailers_written {
                // Terminating chunk
                write_queue_tx
                    .send(Bytes::from_static(b"0\r\n\r\n"))
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
            self.io.flush().await?;
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

    #[inline]
    async fn write_100_continue(&mut self, version: Version) -> Result<(), std::io::Error> {
        if version == Version::HTTP_10 {
            self.io.write_all(b"HTTP/1.0 100 Continue\r\n\r\n").await?;
        } else {
            self.io.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        }
        self.io.flush().await?;

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
        F: Fn(Request<Incoming>) -> Fut,
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
                let (mut request, body_tx) = match if let Some(timeout) =
                    self.options.header_read_timeout
                {
                    vibeio::time::timeout(timeout, self.read_request()).await
                } else {
                    Ok(self.read_request().await)
                } {
                    Ok(Ok(d)) => d,
                    Ok(Err(e)) => {
                        // Parse error
                        if let Ok(mut response) = error_fn(false).await {
                            response
                                .headers_mut()
                                .insert(header::CONNECTION, HeaderValue::from_static("close"));

                            let _ = self.write_response(response, Version::HTTP_11, false).await;
                        }
                        return Err(e);
                    }
                    Err(_) => {
                        // Timeout error
                        if let Ok(mut response) = error_fn(true).await {
                            response
                                .headers_mut()
                                .insert(header::CONNECTION, HeaderValue::from_static("close"));

                            let _ = self.write_response(response, Version::HTTP_11, false).await;
                        }
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "header read timeout",
                        ));
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

                // 100 Continue
                if self.options.send_continue_response {
                    let is_100_continue = request
                        .headers()
                        .get(header::EXPECT)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
                    if is_100_continue {
                        self.write_100_continue(version).await?;
                    }
                }

                // Content-Length header
                let content_length = request
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let chunked = request
                    .headers()
                    .get(header::TRANSFER_ENCODING)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| {
                        v.split(',')
                            .any(|v| v.trim().eq_ignore_ascii_case("chunked"))
                    });
                let has_trailers = request
                    .headers()
                    .get(header::TRAILER)
                    .map(|v| v.to_str().ok().is_some_and(|s| !s.is_empty()))
                    .unwrap_or(false);
                let write_trailers = request
                    .headers()
                    .get(header::TE)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| {
                        v.split(',')
                            .any(|v| v.trim().eq_ignore_ascii_case("trailers"))
                    })
                    .unwrap_or(false);

                // Install HTTP upgrade
                let (upgrade_tx, upgrade_rx) = oneshot::async_channel();
                let upgrade = Upgrade::new(upgrade_rx);
                let upgraded = upgrade.upgraded.clone();
                request.extensions_mut().insert(upgrade);

                // Get HTTP response
                let mut response = {
                    let read_body_fut = Box::pin(async {
                        if chunked {
                            self.read_chunked_body_fn(&body_tx, has_trailers).await
                        } else {
                            self.read_body_fn(&body_tx, content_length).await
                        }
                    });
                    let request_fut = Box::pin(request_fn(request));
                    let select_either =
                        futures_util::future::select(request_fut, read_body_fut).await;

                    let (response, body_fut) = match select_either {
                        futures_util::future::Either::Left((response, read_body_fut)) => {
                            (response, Some(read_body_fut))
                        }
                        futures_util::future::Either::Right((result, request_fut)) => {
                            result?;
                            (request_fut.await, None)
                        }
                    };

                    // Drain away remaining body
                    if let Some(body_fut) = body_fut {
                        body_fut.await?;
                    }

                    response.map_err(|e| std::io::Error::other(e.to_string()))?
                };

                let mut was_upgraded = false;
                if upgraded.load(std::sync::atomic::Ordering::Relaxed) {
                    was_upgraded = true;
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
                } else if keep_alive {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
                } else {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }

                // Write response to IO
                self.write_response(response, version, write_trailers)
                    .await?;

                if was_upgraded {
                    // HTTP upgrade
                    let _ = upgrade_tx.send(Upgraded::new(self.io, self.leftover));
                    return Ok(());
                }

                if self.cancel_token.as_ref().is_some_and(|t| t.is_cancelled()) {
                    // Graceful shutdown requested, break out of loop
                    break;
                }
            }
            Ok(())
        }
    }

    fn handle<F, Fut, ResB, ResBE, ResE>(
        self,
        request_fn: F,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: Fn(Request<Incoming>) -> Fut,
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
            response.body(Empty::new())
        })
    }
}

struct Http1Body {
    #[allow(clippy::type_complexity)]
    inner: Pin<Box<Receiver<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>>>,
}

impl Body for Http1Body {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        match self.inner.as_mut().poll_next(cx) {
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
