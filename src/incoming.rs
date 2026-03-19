use std::{
    pin::Pin,
    task::{Context, Poll},
};

use http_body::Body;

/// A type-erased, boxed HTTP request body.
///
/// `Incoming` is the concrete body type placed inside every
/// [`Request`](http::Request) passed to the user-supplied handler. It wraps
/// any [`Body`] implementation behind a single, heap-allocated trait object,
/// keeping the handler signature simple regardless of the underlying transport
/// or encoding (content-length, chunked, etc.).
///
/// Data frames are yielded as [`bytes::Bytes`] chunks. Trailer frames (like
/// HTTP/1.1 chunked trailers) are forwarded transparently. The stream ends when
/// [`Body::poll_frame`] returns `Poll::Ready(None)`.
pub struct Incoming {
    inner: Pin<Box<dyn Body<Data = bytes::Bytes, Error = std::io::Error> + Send + Sync>>,
}

impl Incoming {
    #[inline]
    pub(crate) fn new(
        inner: impl Body<Data = bytes::Bytes, Error = std::io::Error> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl Body for Incoming {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    #[inline]
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.inner.as_mut().poll_frame(cx)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
