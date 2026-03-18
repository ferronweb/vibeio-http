mod options;
mod tests;
mod upgrade;
mod writebuf;
mod zerocopy;

pub use options::*;
use tokio_util::sync::CancellationToken;
pub use upgrade::*;
pub use zerocopy::*;

#[cfg(unix)]
pub(crate) type RawHandle = std::os::fd::RawFd;
#[cfg(windows)]
pub(crate) type RawHandle = std::os::windows::io::RawHandle;

use std::{
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
use http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, Response, Uri, Version};
use http_body::Body;
use http_body_util::{BodyExt, Empty};
use memchr::{memchr3_iter, memmem};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::{h1::writebuf::WriteBuf, EarlyHints, HttpProtocol, Incoming};

const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

/// An HTTP/1.x connection handler.
///
/// `Http1` wraps an async I/O stream (`Io`) and provides a complete
/// HTTP/1.0 and HTTP/1.1 server implementation, including:
///
/// - Request head parsing (via [`httparse`])
/// - Streaming request bodies (content-length and chunked transfer-encoding)
/// - Chunked response encoding and trailer support
/// - `100 Continue` and `103 Early Hints` interim responses
/// - HTTP connection upgrades (e.g. WebSocket)
/// - Optional zero-copy response sending on Linux (see [`Http1::zerocopy`])
/// - Keep-alive connection reuse
/// - Graceful shutdown via a [`CancellationToken`]
///
/// # Construction
///
/// ```rust,ignore
/// let http1 = Http1::new(tcp_stream, Http1Options::default());
/// ```
///
/// # Serving requests
///
/// Use the [`HttpProtocol`] trait methods ([`handle`](HttpProtocol::handle) /
/// [`handle_with_error_fn`](HttpProtocol::handle_with_error_fn)) to drive the
/// connection to completion:
///
/// ```rust,ignore
/// http1.handle(|req| async move {
///     Ok::<_, Infallible>(Response::new(Full::new(Bytes::from("Hello!"))))
/// }).await?;
/// ```
pub struct Http1<Io> {
    io: Io,
    options: options::Http1Options,
    cancel_token: Option<CancellationToken>,
    parsed_headers: Box<[MaybeUninit<httparse::Header<'static>>]>,
    date_header_value_cached: Option<(String, std::time::SystemTime)>,
    cached_headers: Option<HeaderMap>,
    read_buf: BytesMut,
    write_buf: WriteBuf,
}

#[cfg(all(target_os = "linux", feature = "h1-zerocopy"))]
impl<Io> Http1<Io>
where
    for<'a> Io: tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + vibeio::io::AsInnerRawHandle<'a>
        + Unpin
        + 'static,
{
    /// Converts this `Http1` into an [`Http1Zerocopy`] that uses emulated
    /// sendfile (Linux only) to send response bodies without copying data
    /// through user space.
    ///
    /// The response body must have a `ZerocopyResponse` extension installed
    /// (via [`install_zerocopy`]) containing the file descriptor to send from.
    /// Responses without that extension are sent normally.
    ///
    /// Only available on Linux (`target_os = "linux"`), and only when `Io`
    /// implements [`vibeio::io::AsInnerRawHandle`].
    #[inline]
    pub fn zerocopy(self) -> Http1Zerocopy<Io> {
        Http1Zerocopy { inner: self }
    }
}

impl<Io> Http1<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    /// Creates a new `Http1` connection handler wrapping the given I/O stream.
    ///
    /// The `options` value controls limits, timeouts, and optional features;
    /// see [`Http1Options`] for details.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let http1 = Http1::new(tcp_stream, Http1Options::default());
    /// ```
    #[inline]
    pub fn new(io: Io, options: options::Http1Options) -> Self {
        // Safety: u8 is a primitive type, so we can safely assume initialization
        let read_buf = BytesMut::with_capacity(options.max_header_size);
        let parsed_headers: Box<[MaybeUninit<httparse::Header<'static>>]> =
            Box::new_uninit_slice(options.max_header_count);
        Self {
            io,
            options,
            cancel_token: None,
            parsed_headers,
            date_header_value_cached: None,
            cached_headers: None,
            read_buf,
            write_buf: WriteBuf::new(),
        }
    }

    #[inline]
    fn get_date_header_value(&mut self) -> &str {
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

    /// Attaches a [`CancellationToken`] for graceful shutdown.
    ///
    /// After the current in-flight request has been fully handled and its
    /// response written, the connection loop checks whether the token has been
    /// cancelled. If it has, the loop exits cleanly instead of waiting for the
    /// next request.
    ///
    /// This allows the server to drain active connections without abruptly
    /// closing them mid-response.
    #[inline]
    pub fn graceful_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    #[inline]
    async fn fill_buf(&mut self) -> Result<usize, std::io::Error> {
        if self.read_buf.remaining() < 1024 {
            self.read_buf.reserve(1024);
        }
        let spare_capacity = self.read_buf.spare_capacity_mut();
        // Safety: The buffer is are read only after the request head has been parsed
        let n = self
            .io
            .read(unsafe {
                &mut *std::ptr::slice_from_raw_parts_mut(
                    spare_capacity.as_mut_ptr() as *mut u8,
                    spare_capacity.len(),
                )
            })
            .await?;
        if n == 0 {
            return Ok(0);
        }
        unsafe { self.read_buf.set_len(self.read_buf.len() + n) };
        Ok(n)
    }

    #[inline]
    async fn read_body_fn(
        &mut self,
        body_tx: &async_channel::Sender<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>,
        content_length: u64,
    ) -> Result<(), std::io::Error> {
        let mut remaining = content_length;
        let mut just_started = true;
        while remaining > 0 {
            let have_to_read_buf = !just_started || self.read_buf.is_empty();
            just_started = false;
            if have_to_read_buf {
                let n = self.fill_buf().await?;
                if n == 0 {
                    break;
                }
            }
            let chunk = self
                .read_buf
                .split_to(
                    self.read_buf
                        .len()
                        .min(remaining.min(usize::MAX as u64) as usize),
                )
                .freeze();
            remaining -= chunk.len() as u64;

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
            let mut len_buf_pos: usize = 0;
            let mut just_started = true;
            loop {
                if len_buf_pos >= 48 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "chunk length buffer overflow",
                    ));
                }

                let begin_search = len_buf_pos.saturating_sub(1);

                let have_to_read_buf = !just_started || self.read_buf.is_empty();
                just_started = false;
                if have_to_read_buf {
                    let n = self.fill_buf().await?;
                    if n == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "unexpected EOF",
                        ));
                    }
                    len_buf_pos += n;
                } else {
                    len_buf_pos += self.read_buf.len();
                }

                if let Some(pos) =
                    memmem::find(&self.read_buf[begin_search..len_buf_pos.min(48)], b"\r\n")
                {
                    let numbers =
                        std::str::from_utf8(&self.read_buf[begin_search..begin_search + pos])
                            .map_err(|_| {
                                std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "invalid chunk length",
                                )
                            })?;
                    let len = usize::from_str_radix(numbers, 16).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid chunk length")
                    })?;
                    // Ignore the trailing CRLF
                    self.read_buf.advance(begin_search + pos + 2);
                    break len;
                }
            }
        };
        // Safety: u8 is a primitive type, so we can safely assume initialization
        let mut read = 0;
        if len == 0 && would_have_trailers {
            return Ok(bytes::Bytes::new()); // Empty terminating chunk
        }
        let mut just_started = true;
        // + 2, because we need to read the trailing CRLF
        while read < len + 2 {
            let have_to_read_buf = !just_started || self.read_buf.is_empty();
            just_started = false;
            if have_to_read_buf {
                let n = self.fill_buf().await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "unexpected EOF",
                    ));
                }
                read += n;
            } else {
                read += self.read_buf.len();
            }
        }
        let chunk = self.read_buf.split_to(len).freeze();
        self.read_buf.advance(2); // Ignore the trailing CRLF
        Ok(chunk)
    }

    #[inline]
    async fn read_trailers(&mut self) -> Result<Option<http::HeaderMap>, std::io::Error> {
        // Safety: u8 is a primitive type, so we can safely assume initialization
        let mut bytes_read: usize = 0;
        let mut just_started = true;
        while bytes_read < self.options.max_header_size {
            let old_bytes_read = bytes_read;
            let begin_search = old_bytes_read.saturating_sub(3);

            let have_to_read_buf = !just_started || self.read_buf.is_empty();
            just_started = false;
            if have_to_read_buf {
                let n = self.fill_buf().await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "unexpected EOF",
                    ));
                }
                bytes_read = (old_bytes_read + n).min(self.options.max_header_size);
            } else {
                bytes_read =
                    (old_bytes_read + self.read_buf.len()).min(self.options.max_header_size)
            }

            if bytes_read > 2 && self.read_buf[0] == b'\r' && self.read_buf[1] == b'\n' {
                // No trailers, return None
                return Ok(None);
            }

            if let Some(separator_index) =
                memmem::find(&self.read_buf[begin_search..bytes_read], b"\r\n\r\n")
            {
                let to_parse_length = begin_search + separator_index + 4;
                let buf_ro = self.read_buf.split_to(to_parse_length).freeze();

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
                        let value_start = header.value.as_ptr() as usize - buf_ro.as_ptr() as usize;
                        let value_len = header.value.len();
                        // Safety: the header value is already validated by httparse
                        let value = unsafe {
                            HeaderValue::from_maybe_shared_unchecked(
                                buf_ro.slice(value_start..(value_start + value_len)),
                            )
                        };
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
        Option<(
            Request<Incoming>,
            async_channel::Sender<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>,
        )>,
        std::io::Error,
    > {
        // Parse HTTP request using httparse
        let (request, body_tx) = {
            let Some((head, headers)) = self.get_head().await? else {
                return Ok(None);
            };
            // Safety: The headers are read only after the request head has been parsed
            let headers = unsafe {
                std::mem::transmute::<
                    &mut [MaybeUninit<httparse::Header<'static>>],
                    &mut [MaybeUninit<httparse::Header<'_>>],
                >(headers)
            };
            let mut req = httparse::Request::new(&mut []);
            let status = req
                .parse_with_uninit_headers(&head, headers)
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
            let mut header_map = self.cached_headers.take().unwrap_or_default();
            header_map.clear();
            let additional_capacity = req.headers.len().saturating_sub(header_map.capacity());
            if additional_capacity > 0 {
                header_map.reserve(additional_capacity);
            }
            for header in req.headers {
                if header == &httparse::EMPTY_HEADER {
                    // No more headers...
                    break;
                }
                let name = HeaderName::from_bytes(header.name.as_bytes())
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let value_start = header.value.as_ptr() as usize - head.as_ptr() as usize;
                let value_len = header.value.len();
                // Safety: the header value is already validated by httparse
                let value = unsafe {
                    HeaderValue::from_maybe_shared_unchecked(
                        head.slice(value_start..(value_start + value_len)),
                    )
                };
                header_map.append(name, value);
            }
            *request.headers_mut() = header_map;

            (request, body_tx)
        };
        Ok(Some((request, body_tx)))
    }

    #[inline]
    async fn get_head(
        &mut self,
    ) -> Result<Option<(Bytes, &mut [MaybeUninit<httparse::Header<'static>>])>, std::io::Error>
    {
        let mut request_line_read = false;
        let mut bytes_read: usize = 0;
        let mut whitespace_trimmed = None;
        let mut just_started = true;
        while bytes_read < self.options.max_header_size {
            let old_bytes_read = bytes_read;
            let begin_search = old_bytes_read.saturating_sub(3);

            let have_to_read_buf = !just_started || self.read_buf.is_empty();
            just_started = false;
            if have_to_read_buf {
                let n = self.fill_buf().await?;
                if n == 0 {
                    if whitespace_trimmed.is_none() {
                        return Ok(None);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "unexpected EOF",
                    ));
                }
                bytes_read = (old_bytes_read + n).min(self.options.max_header_size);
            } else {
                bytes_read =
                    (old_bytes_read + self.read_buf.len()).min(self.options.max_header_size)
            }

            if whitespace_trimmed.is_none() {
                whitespace_trimmed = self.read_buf[old_bytes_read..bytes_read]
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
                        &self.read_buf[whitespace_trimmed..bytes_read],
                    );
                    let mut spaces = 0;
                    for separator_index in memchr {
                        if self.read_buf[whitespace_trimmed + separator_index] == b' ' {
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
                        search_header_body_separator(&self.read_buf[begin_search..bytes_read])
                    {
                        let to_parse_length =
                            begin_search + separator_index + separator_len - whitespace_trimmed;
                        self.read_buf.advance(whitespace_trimmed);
                        let head = self.read_buf.split_to(to_parse_length);
                        return Ok(Some((head.freeze(), &mut self.parsed_headers)));
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
    async fn write_response<Z, ZFut>(
        &mut self,
        mut response: Response<
            impl Body<Data = bytes::Bytes, Error = impl std::error::Error> + Unpin,
        >,
        version: Version,
        write_trailers: bool,
        zerocopy_fn: Option<Z>,
    ) -> Result<(), std::io::Error>
    where
        Z: FnMut(RawHandle, &'static Io, u64) -> ZFut,
        ZFut: std::future::Future<Output = Result<(), std::io::Error>>,
    {
        // Date header
        if self.options.send_date_header {
            response.headers_mut().insert(
                header::DATE,
                HeaderValue::from_str(self.get_date_header_value())
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

        let (parts, mut body) = response.into_parts();

        let mut head = Vec::with_capacity(30 + parts.headers.len() * 30); // Similar to Hyper's heuristic
        if version == Version::HTTP_10 {
            head.extend_from_slice(b"HTTP/1.0 ");
        } else {
            head.extend_from_slice(b"HTTP/1.1 ");
        }
        let status = parts.status;
        head.extend_from_slice(status.as_str().as_bytes());
        if let Some(canonical_reason) = status.canonical_reason() {
            head.extend_from_slice(b" ");
            head.extend_from_slice(canonical_reason.as_bytes());
        }
        head.extend_from_slice(b"\r\n");
        for (name, value) in &parts.headers {
            head.extend_from_slice(name.as_str().as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(value.as_bytes());
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(b"\r\n");
        unsafe {
            self.write_buf.push(IoSlice::new(&head));
        }

        if !chunked {
            if let Some(content_length) = parts
                .headers
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
            {
                if let Some(zero_copy) = parts.extensions.get::<ZerocopyResponse>() {
                    if let Some(mut zerocopy_fn) = zerocopy_fn {
                        // Zerocopy
                        unsafe {
                            self.write_buf
                                .flush(&mut self.io, self.options.enable_vectored_write)
                                .await?
                        };
                        zerocopy_fn(
                            zero_copy.handle,
                            // Safety: the lifetime of the static reference is bound by the lifetime of the Io struct
                            unsafe { std::mem::transmute::<&Io, &'static Io>(&self.io) },
                            content_length,
                        )
                        .await?;
                        self.io.flush().await?;
                        let reclaimed_headers = parts.headers;
                        self.cached_headers = Some(reclaimed_headers);
                        return Ok(());
                    }
                }
            }
        }

        let mut trailers_written = false;
        while let Some(chunk) = body.frame().await {
            let chunk = chunk.map_err(|e| std::io::Error::other(e.to_string()))?;
            match chunk.into_data() {
                Ok(data) => {
                    if chunked {
                        let mut data_len_buf = Vec::with_capacity(16);
                        write_chunk_size(&mut data_len_buf, data.len());
                        unsafe {
                            self.write_buf.push(IoSlice::new(&data_len_buf));
                            self.write_buf.push(IoSlice::new(&data));
                            self.write_buf.push(IoSlice::new(b"\r\n"));
                            self.write_buf
                                .write(&mut self.io, self.options.enable_vectored_write)
                                .await?;
                        };
                    } else {
                        unsafe {
                            self.write_buf.push(IoSlice::new(&data));
                            self.write_buf
                                .write(&mut self.io, self.options.enable_vectored_write)
                                .await?;
                        }
                    }
                }
                Err(chunk) => {
                    if let Ok(trailers) = chunk.into_trailers() {
                        if write_trailers {
                            unsafe {
                                self.write_buf.push(IoSlice::new(b"0\r\n"));
                                for (name, value) in &trailers {
                                    self.write_buf.push(IoSlice::new(name.as_str().as_bytes()));
                                    self.write_buf.push(IoSlice::new(b": "));
                                    self.write_buf.push(IoSlice::new(value.as_bytes()));
                                    self.write_buf.push(IoSlice::new(b"\r\n"));
                                }
                                self.write_buf.push(IoSlice::new(b"\r\n"));
                                self.write_buf
                                    .write(&mut self.io, self.options.enable_vectored_write)
                                    .await?;
                            }
                            trailers_written = true;
                        }
                        break;
                    }
                }
            };
        }
        if chunked && !trailers_written {
            // Terminating chunk
            unsafe {
                self.write_buf.push(IoSlice::new(b"0\r\n\r\n"));
            }
        }
        unsafe {
            self.write_buf
                .flush(&mut self.io, self.options.enable_vectored_write)
                .await?;
        }
        self.io.flush().await?;
        let reclaimed_headers = parts.headers;
        self.cached_headers = Some(reclaimed_headers);

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
                if value.is_empty() {
                    head.extend_from_slice(b":\r\n");
                    continue;
                }
                head.extend_from_slice(b": ");
                head.extend_from_slice(value.as_bytes());
                head.extend_from_slice(b"\r\n");
            }
        }
        head.extend_from_slice(b"\r\n");

        self.io.write_all(&head).await?;

        Ok(())
    }

    #[inline]
    pub(crate) async fn handle_with_error_fn_and_zerocopy<
        F,
        Fut,
        ResB,
        ResBE,
        ResE,
        EF,
        EFut,
        EResB,
        EResBE,
        EResE,
        ZF,
        ZFut,
    >(
        mut self,
        request_fn: F,
        error_fn: EF,
        mut zerocopy_fn: Option<ZF>,
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
        ZF: FnMut(RawHandle, &'static Io, u64) -> ZFut,
        ZFut: std::future::Future<Output = Result<(), std::io::Error>>,
    {
        let mut keep_alive = true;

        while keep_alive {
            let (mut request, body_tx) = match if let Some(timeout) =
                self.options.header_read_timeout
            {
                vibeio::time::timeout(timeout, self.read_request()).await
            } else {
                Ok(self.read_request().await)
            } {
                Ok(Ok(Some(d))) => d,
                Ok(Ok(None)) => {
                    return Ok(());
                }
                Ok(Err(e)) => {
                    // Parse error
                    if let Ok(mut response) = error_fn(false).await {
                        response
                            .headers_mut()
                            .insert(header::CONNECTION, HeaderValue::from_static("close"));

                        let _ = self
                            .write_response(response, Version::HTTP_11, false, zerocopy_fn.as_mut())
                            .await;
                    }
                    return Err(e);
                }
                Err(_) => {
                    // Timeout error
                    if let Ok(mut response) = error_fn(true).await {
                        response
                            .headers_mut()
                            .insert(header::CONNECTION, HeaderValue::from_static("close"));

                        let _ = self
                            .write_response(response, Version::HTTP_11, false, zerocopy_fn.as_mut())
                            .await;
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
            let early_hints_fut = if self.options.enable_early_hints {
                let (early_hints_tx, early_hints_rx) = async_channel::unbounded();
                let early_hints = EarlyHints::new(early_hints_tx);
                request.extensions_mut().insert(early_hints);
                // Safety: the function below is used only in futures_util::future::select
                // Also, another function that would borrow self would read data,
                // while this function would write data
                let mut_self = unsafe { std::mem::transmute::<&mut Self, &mut Self>(&mut self) };
                Some(async {
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
                None
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
                let read_body_fut = async {
                    if chunked {
                        self.read_chunked_body_fn(&body_tx, has_trailers).await
                    } else {
                        self.read_body_fn(&body_tx, content_length).await
                    }
                };
                let read_body_fut_pin = std::pin::pin!(read_body_fut);
                let request_fut = request_fn(request);
                let request_fut_pin = std::pin::pin!(request_fut);
                let early_hints_fut: Pin<
                    Box<dyn std::future::Future<Output = Result<(), std::io::Error>>>,
                > = if let Some(early_hints) = early_hints_fut {
                    Box::pin(early_hints)
                } else {
                    Box::pin(futures_util::future::pending::<Result<(), std::io::Error>>())
                };

                let select_read_body_either =
                    futures_util::future::select(request_fut_pin, early_hints_fut);
                let select_either =
                    futures_util::future::select(read_body_fut_pin, select_read_body_either).await;

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
                if version == Version::HTTP_10
                    || response.headers().contains_key(header::CONNECTION)
                {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
                }
            } else if version == Version::HTTP_11
                || response.headers().contains_key(header::CONNECTION)
            {
                response
                    .headers_mut()
                    .insert(header::CONNECTION, HeaderValue::from_static("close"));
            }

            // Write response to IO
            self.write_response(response, version, write_trailers, zerocopy_fn.as_mut())
                .await?;

            if was_upgraded {
                // HTTP upgrade
                let frozen_buf = self.read_buf.freeze();
                let _ = upgrade_tx.send(Upgraded::new(
                    self.io,
                    if frozen_buf.is_empty() {
                        None
                    } else {
                        Some(frozen_buf)
                    },
                ));
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

impl<Io> HttpProtocol for Http1<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    #[inline]
    fn handle_with_error_fn<F, Fut, ResB, ResBE, ResE, EF, EFut, EResB, EResBE, EResE>(
        self,
        request_fn: F,
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
        #[allow(clippy::type_complexity)]
        let no_zerocopy: Option<
            Box<
                dyn FnMut(
                    RawHandle,
                    &Io,
                    u64,
                ) -> Box<
                    dyn std::future::Future<Output = Result<(), std::io::Error>>
                        + Unpin
                        + Send
                        + Sync,
                >,
            >,
        > = None;
        self.handle_with_error_fn_and_zerocopy(request_fn, error_fn, no_zerocopy)
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
    for (i, b) in slice.iter().copied().enumerate() {
        if b == b'\r' {
            if slice[i + 1..].chunks(3).next() == Some(&b"\n\r\n"[..]) {
                return Some((i, 4));
            }
        } else if b == b'\n' && slice.get(i + 1) == Some(&b'\n') {
            return Some((i, 2));
        }
    }
    None
}

/// Writes the chunk size to the given buffer in hexadecimal format, followed by `\r\n`.
#[inline]
fn write_chunk_size(dst: &mut Vec<u8>, len: usize) {
    let mut buf = [0u8; 18];
    let mut n = len;
    let mut pos = buf.len();
    loop {
        pos -= 1;
        buf[pos] = HEX_DIGITS[n & 0xF];
        n >>= 4;
        if n == 0 {
            break;
        }
    }
    dst.extend_from_slice(&buf[pos..]);
    dst.extend_from_slice(b"\r\n");
}
