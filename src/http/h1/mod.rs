mod options;
mod tests;
mod upgrade;

pub use options::*;
use tokio_util::sync::CancellationToken;
pub use upgrade::*;

use std::{
    cell::UnsafeCell,
    io::IoSlice,
    mem::MaybeUninit,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::UNIX_EPOCH,
};

use async_channel::Receiver;
use bytes::{Buf, Bytes, BytesMut};
use futures_util::stream::Stream;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, Uri, Version, header};
use http_body::Body;
use http_body_util::{BodyExt, Empty};
use memchr::{memchr2, memchr3_iter, memmem};
use smallvec::SmallVec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::{EarlyHints, HttpProtocol, Incoming};

pub struct Http1<Io> {
    io: Io,
    leftover: Option<bytes::Bytes>,
    options: options::Http1Options,
    cancel_token: Option<CancellationToken>,
    head_buf: Box<[u8]>,
    parsed_headers: Box<[MaybeUninit<httparse::Header<'static>>]>,
    date_header_value_cached: Option<(String, std::time::SystemTime)>,
}

impl<Io> Http1<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    #[inline]
    pub fn new(io: Io, options: options::Http1Options) -> Self {
        // Safety: u8 is a primitive type, so we can safely assume initialization
        let head_buf: Box<[u8]> =
            unsafe { Box::new_uninit_slice(options.max_header_size).assume_init() };
        let parsed_headers: Box<[MaybeUninit<httparse::Header<'static>>]> =
            Box::new_uninit_slice(options.max_header_count);
        Self {
            io,
            leftover: None,
            options,
            cancel_token: None,
            head_buf,
            parsed_headers,
            date_header_value_cached: None,
        }
    }

    #[inline]
    pub fn get_date_header_value(&mut self) -> &str {
        let now = std::time::SystemTime::now();
        if self.date_header_value_cached.as_ref().is_none_or(|v| {
            v.1.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
                != now.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
        }) {
            let value = httpdate::fmt_http_date(now).to_string();
            self.date_header_value_cached = Some((value, now));
        }
        self.date_header_value_cached
            .as_ref()
            .map(|v| v.0.as_str())
            .unwrap_or("")
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
    async fn write_all_vectored(&mut self, mut bufs: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        while !bufs.is_empty() {
            let n = self.io.write_vectored(bufs).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            IoSlice::advance_slices(&mut bufs, n);
        }
        Ok(())
    }

    #[inline]
    async fn read_body_fn(
        &mut self,
        body_tx: &async_channel::Sender<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>,
        content_length: u64,
    ) -> Result<(), std::io::Error> {
        let mut remaining = content_length;
        while remaining > 0 {
            // Safety: u8 is a primitive type, so we can safely assume initialization
            let mut buf = unsafe {
                #[allow(invalid_value)]
                #[allow(clippy::uninit_assumed_init)]
                MaybeUninit::<[u8; 16384]>::uninit().assume_init()
            };
            let to_read_ln = remaining.min(16384) as usize;
            let n = self.read_with_leftover(&mut buf[..to_read_ln]).await?;
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
            // Safety: u8 is a primitive type, so we can safely assume initialization
            let mut len_buf = unsafe {
                #[allow(invalid_value)]
                #[allow(clippy::uninit_assumed_init)]
                MaybeUninit::<[u8; 48]>::uninit().assume_init()
            };
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
                        let mut new_leftover =
                            Vec::with_capacity(leftover.len() + (len_buf.len() - pos - 2));
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
        // Safety: u8 is a primitive type, so we can safely assume initialization
        let mut chunk: Box<[u8]> = unsafe { Box::new_uninit_slice(len + 2).assume_init() };
        let mut read = 0;
        if len == 0 && would_have_trailers {
            return Ok(bytes::Bytes::new()); // Empty terminating chunk
        }
        // + 2, because we need to read the trailing CRLF
        while read < len + 2 {
            let n = self.read_with_leftover(&mut chunk[read..]).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF",
                ));
            }
            read += n;
        }
        let mut chunk = bytes::Bytes::from_owner(chunk);
        chunk.truncate(len);
        Ok(chunk)
    }

    #[inline]
    async fn read_trailers(&mut self) -> Result<Option<http::HeaderMap>, std::io::Error> {
        // Safety: u8 is a primitive type, so we can safely assume initialization
        let mut buf: Box<[u8]> =
            unsafe { Box::new_uninit_slice(self.options.max_header_size).assume_init() };
        let mut bytes_read: usize = 0;
        while bytes_read < self.options.max_header_size {
            let n = self.read_with_leftover(&mut buf[bytes_read..]).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF",
                ));
            }
            let begin_search = bytes_read.saturating_sub(3);
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
                    let mut new_leftover =
                        Vec::with_capacity(leftover_to_add.len() + leftover.len());
                    new_leftover.extend(leftover_to_add);
                    new_leftover.extend(leftover);
                    self.leftover = Some(bytes::Bytes::from(new_leftover));
                } else {
                    self.leftover = Some(leftover_to_add);
                }

                // Parse trailers using `httparse` crate's header parsing
                let mut httparse_trailers =
                    vec![httparse::EMPTY_HEADER; self.options.max_header_count].into_boxed_slice();
                let status = httparse::parse_headers(&buf_ro, &mut httparse_trailers)
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
        // Parse HTTP request using httparse
        let (request, body_tx, leftover_to_add) = {
            let (head_buf, to_parse_length, headers) = self.get_head().await?;
            // Safety: The headers are read only after the request head has been parsed
            let headers = unsafe {
                std::mem::transmute::<
                    &mut [MaybeUninit<httparse::Header<'static>>],
                    &mut [MaybeUninit<httparse::Header<'_>>],
                >(headers)
            };
            let mut req = httparse::Request::new(&mut []);
            let status = req
                .parse_with_uninit_headers(&head_buf[..to_parse_length], headers)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if status.is_partial() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "partial request head",
                ));
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
            let mut header_map = http::HeaderMap::try_with_capacity(req.headers.len())
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            for header in req.headers {
                if header == &httparse::EMPTY_HEADER {
                    // No more headers...
                    break;
                }
                let name = HeaderName::from_bytes(header.name.as_bytes())
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let value = HeaderValue::from_bytes(header.value)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                header_map.append(name, value);
            }
            *request.headers_mut() = header_map;

            (
                request,
                body_tx,
                Bytes::copy_from_slice(&head_buf[to_parse_length..]),
            )
        };
        if let Some(leftover) = self.leftover.take() {
            let mut new_leftover = BytesMut::from(leftover_to_add);
            new_leftover.extend(leftover);
            self.leftover = Some(new_leftover.freeze());
        } else {
            self.leftover = Some(leftover_to_add);
        }
        Ok((request, body_tx))
    }

    #[inline]
    async fn get_head(
        &mut self,
    ) -> Result<(&[u8], usize, &mut [MaybeUninit<httparse::Header<'static>>]), std::io::Error> {
        let mut request_line_read = false;
        let mut bytes_read: usize = 0;
        let mut whitespace_trimmed = None;
        while bytes_read < self.options.max_header_size {
            // Safety: The buffer is are read only after the request head has been parsed
            let temp_buf = unsafe {
                std::mem::transmute::<&mut [u8], &mut [u8]>(&mut self.head_buf[bytes_read..])
            };
            let n = self.read_with_leftover(temp_buf).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF",
                ));
            }
            let old_bytes_read = bytes_read;
            let begin_search = old_bytes_read.saturating_sub(3);
            bytes_read = (bytes_read + n).min(self.options.max_header_size);

            if whitespace_trimmed.is_none() {
                whitespace_trimmed = self.head_buf[old_bytes_read..bytes_read]
                    .iter()
                    .position(|b| !b.is_ascii_whitespace());
            }

            if let Some(whitespace_trimmed) = whitespace_trimmed {
                // Validate first line (request line) before checking for header/body separator
                if !request_line_read {
                    let memchr = memchr3_iter(
                        b' ',
                        b'\r',
                        b'\n',
                        &self.head_buf[whitespace_trimmed..bytes_read],
                    );
                    let mut spaces = 0;
                    for separator_index in memchr {
                        if self.head_buf[whitespace_trimmed + separator_index] == b' ' {
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
                        search_header_body_separator(&self.head_buf[begin_search..bytes_read])
                    {
                        let to_parse_length =
                            begin_search + separator_index + separator_len - whitespace_trimmed;
                        return Ok((
                            &self.head_buf[whitespace_trimmed..bytes_read],
                            to_parse_length,
                            &mut self.parsed_headers,
                        ));
                    }
                }
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
                HeaderValue::from_str(&self.get_date_header_value())
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

        if self.io.is_write_vectored() && self.options.enable_vectored_write {
            let response = UnsafeCell::new(response);
            // Safety: There's only one "response_mut" mutable reference use, and it's used to obtain data from the body only.
            // The "response_ref" reference is used for read-only access to headers and status.
            let response_ref = unsafe { &*response.get() };
            let response_mut = unsafe { &mut *response.get() };
            let mut head: Vec<IoSlice<'_>> =
                Vec::with_capacity(6 + (response_ref.headers().len() * 4));
            if version == Version::HTTP_10 {
                head.push(IoSlice::new(b"HTTP/1.0 "));
            } else {
                head.push(IoSlice::new(b"HTTP/1.1 "));
            }
            let status = response_ref.status();
            head.push(IoSlice::new(status.as_str().as_bytes()));
            if let Some(canonical_reason) = status.canonical_reason() {
                head.push(IoSlice::new(b" "));
                head.push(IoSlice::new(canonical_reason.as_bytes()));
            }
            head.push(IoSlice::new(b"\r\n"));
            for (name, value) in response_ref.headers() {
                head.push(IoSlice::new(name.as_str().as_bytes()));
                head.push(IoSlice::new(b": "));
                head.push(IoSlice::new(value.as_bytes()));
                head.push(IoSlice::new(b"\r\n"));
            }
            head.push(IoSlice::new(b"\r\n"));

            let mut trailers_written = false;
            let mut head_written = false;
            while let Some(chunk) = response_mut.body_mut().frame().await {
                let chunk = chunk.map_err(|e| std::io::Error::other(e.to_string()))?;
                match chunk.into_data() {
                    Ok(data) => {
                        if chunked {
                            let data_len = format!("{:X}", data.len()).to_string();
                            let mut iovecs = [
                                IoSlice::new(data_len.as_bytes()),
                                IoSlice::new(b"\r\n"),
                                IoSlice::new(&data),
                                IoSlice::new(b"\r\n"),
                            ];
                            if head_written {
                                self.write_all_vectored(&mut iovecs).await?;
                            } else {
                                let mut iovecs2 = head.split_off(0);
                                iovecs2.extend_from_slice(&iovecs);
                                self.write_all_vectored(&mut iovecs2).await?;
                                head_written = true;
                            }
                        } else if head_written {
                            self.write_all_vectored(&mut [IoSlice::new(&data)]).await?;
                        } else {
                            head_written = true;
                            let mut iovecs2 = head.split_off(0);
                            iovecs2.push(IoSlice::new(&data));
                            self.write_all_vectored(&mut iovecs2).await?;
                        }
                    }
                    Err(chunk) => {
                        if let Ok(trailers) = chunk.into_trailers() {
                            if write_trailers {
                                let mut trail: Vec<IoSlice<'_>> = Vec::new();
                                trail.push(IoSlice::new(b"0\r\n"));
                                for (name, value) in &trailers {
                                    trail.push(IoSlice::new(name.as_str().as_bytes()));
                                    trail.push(IoSlice::new(b": "));
                                    trail.push(IoSlice::new(value.as_bytes()));
                                    trail.push(IoSlice::new(b"\r\n"));
                                }
                                trail.push(IoSlice::new(b"\r\n"));
                                self.write_all_vectored(&mut trail).await?;
                                trailers_written = true;
                            }
                            break;
                        }
                    }
                };
            }
            if !head_written {
                self.write_all_vectored(&mut head).await?;
            }
            if chunked && !trailers_written {
                // Terminating chunk
                self.write_all_vectored(&mut [IoSlice::new(b"0\r\n\r\n")])
                    .await?;
            }
            self.io.flush().await?;
        } else {
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
                            if chunked {
                                let mut chunked_data = if head_written {
                                    Vec::with_capacity(
                                        data.len() + 4 + (data.len().ilog2() / 4) as usize,
                                    )
                                } else {
                                    head_written = true;
                                    head.split_off(0)
                                };
                                chunked_data.extend_from_slice(
                                    format!("{:X}", data.len()).to_string().as_bytes(),
                                );
                                chunked_data.extend_from_slice(b"\r\n");
                                chunked_data.extend(data);
                                chunked_data.extend_from_slice(b"\r\n");
                                write_queue_tx
                                    .send(Bytes::from_owner(chunked_data))
                                    .await
                                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                            } else if head_written {
                                write_queue_tx
                                    .send(data)
                                    .await
                                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                            } else {
                                head_written = true;
                                let mut data_to_write = head.split_off(0);
                                data_to_write.extend_from_slice(data.as_ref());
                                write_queue_tx
                                    .send(Bytes::from_owner(data_to_write))
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

    #[inline]
    async fn write_early_hints(
        &mut self,
        version: Version,
        headers: http::HeaderMap,
    ) -> Result<(), std::io::Error> {
        let mut head = Vec::new();
        if version == Version::HTTP_10 {
            head.extend_from_slice(b"HTTP/1.0 103 Early Hints\r\n");
        } else {
            head.extend_from_slice(b"HTTP/1.1 103 Early Hints\r\n");
        }
        let mut current_header_name = None;
        for (name, value) in headers {
            if let Some(name) = name {
                current_header_name = Some(name);
            };
            if let Some(current_header_name) = &current_header_name {
                head.extend_from_slice(current_header_name.as_str().as_bytes());
                head.extend_from_slice(b": ");
                head.extend_from_slice(value.as_bytes());
                head.extend_from_slice(b"\r\n");
            }
        }
        head.extend_from_slice(b"\r\n");

        self.io.write_all(&head).await?;

        Ok(())
    }
}

impl<Io> HttpProtocol for Http1<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    #[inline]
    async fn handle_with_error_fn<F, Fut, ResB, ResBE, ResE, EF, EFut, EResB, EResBE, EResE>(
        mut self,
        request_fn: F,
        error_fn: EF,
    ) -> Result<(), std::io::Error>
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

        while keep_alive {
            let (mut request, body_tx) =
                match if let Some(timeout) = self.options.header_read_timeout {
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

            // 103 Early Hints
            let early_hints_fut: Pin<
                Box<dyn std::future::Future<Output = Result<(), std::io::Error>>>,
            > = if self.options.enable_early_hints {
                let (early_hints_tx, early_hints_rx) = async_channel::unbounded();
                let early_hints = EarlyHints::new(early_hints_tx);
                request.extensions_mut().insert(early_hints);
                // Safety: the function below is used only in futures_util::future::select
                // Also, another function that would borrow self would read data,
                // while this function would write data
                let mut_self = unsafe { std::mem::transmute::<&mut Self, &mut Self>(&mut self) };
                Box::pin(async {
                    let early_hints_rx = early_hints_rx;
                    while let Ok((headers, sender)) = early_hints_rx.recv().await {
                        sender
                            .into_inner()
                            .send(mut_self.write_early_hints(version, headers).await)
                            .ok();
                    }
                    futures_util::future::pending::<Result<(), std::io::Error>>().await
                })
            } else {
                Box::pin(futures_util::future::pending())
            };

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

                let select_read_body_either =
                    futures_util::future::select(request_fut, early_hints_fut);
                let select_either =
                    futures_util::future::select(read_body_fut, select_read_body_either).await;

                let (response, body_fut) = match select_either {
                    futures_util::future::Either::Left((result, request_fut)) => {
                        result?;
                        (
                            match request_fut.await {
                                futures_util::future::Either::Left((response, _)) => response,
                                futures_util::future::Either::Right((_, _)) => unreachable!(),
                            },
                            None,
                        )
                    }
                    futures_util::future::Either::Right((response, read_body_fut)) => (
                        match response {
                            futures_util::future::Either::Left((response, _)) => response,
                            futures_util::future::Either::Right((_, _)) => unreachable!(),
                        },
                        Some(read_body_fut),
                    ),
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

    #[inline]
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

    #[inline]
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
