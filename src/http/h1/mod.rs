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

pub use options::Http1Options;

use std::{
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};

use async_channel::Receiver;
use futures_util::stream::Stream;
use http::{HeaderName, HeaderValue, Method, Request, Response, Uri, header};
use http_body::Body;
use http_body_util::BodyExt;
use memchr::memchr2;
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
        content_length: usize,
    ) -> Result<(), std::io::Error> {
        let mut remaining = content_length;
        while remaining > 0 {
            let mut buf: Box<[u8]> = vec![0u8; remaining.min(16384)].into_boxed_slice();
            let n = self.io.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            let mut chunk = bytes::Bytes::from_owner(buf);
            chunk.truncate(n);
            remaining -= n;

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
        let mut buf: Box<[u8]> = vec![0u8; self.options.max_header_size].into_boxed_slice();
        let mut bytes_read: usize = 0;
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

            if let Some((separator_index, separator_len)) =
                search_header_body_separator(&buf[begin_search..])
            {
                let to_parse_length = begin_search + separator_index + separator_len;
                let mut buf_ro = bytes::Bytes::from_owner(buf);
                buf_ro.truncate(bytes_read);
                return Ok((buf_ro, to_parse_length));
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
    ) -> Result<(), std::io::Error> {
        let mut head = Vec::new();
        head.extend_from_slice(b"HTTP/1.1 ");
        head.extend_from_slice(response.status().as_str().as_bytes());
        head.extend_from_slice(b"\r\n");
        for (name, value) in response.headers() {
            head.extend_from_slice(name.as_str().as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(value.as_bytes());
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(b"\r\n");
        self.io.write_all(&head).await?;
        while let Some(chunk) = response.body_mut().frame().await {
            let chunk = chunk.map_err(|e| std::io::Error::other(e.to_string()))?;
            if let Ok(data) = chunk.into_data() {
                self.io.write_all(data.as_ref()).await?;
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
    fn handle<F, Fut, ResB, ResBE, ResE>(
        mut self,
        mut request_fn: F,
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>>
    where
        F: FnMut(Request<Incoming>) -> Fut,
        Fut: std::future::Future<Output = Result<Response<ResB>, ResE>>,
        ResB: Body<Data = bytes::Bytes, Error = ResBE> + Unpin,
        ResE: std::error::Error,
        ResBE: std::error::Error,
    {
        async move {
            // TODO: 400 Bad Request fn
            let (request, body_tx) = self.read_request().await?;

            // Content-Length header
            let content_length = request
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);

            // Get HTTP response
            let response = {
                let read_body_fut = Box::pin(self.read_body_fn(&body_tx, content_length));
                let request_fut = Box::pin(request_fn(request));
                let select_either = futures_util::future::select(request_fut, read_body_fut).await;

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

            // Write response to IO
            self.write_response(response).await?;

            Ok(())
        }
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
