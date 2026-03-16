use std::{
    pin::Pin,
    task::{Context, Poll},
};

use http_body::Body;

pub struct Incoming {
    inner: Pin<Box<dyn Body<Data = bytes::Bytes, Error = std::io::Error> + Send + Sync>>,
}

impl Incoming {
    #[inline]
    pub fn new(
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
