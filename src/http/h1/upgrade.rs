use std::{
    pin::Pin,
    sync::{Arc, atomic::AtomicBool},
    task::ready,
};

use bytes::BufMut;
use futures_util::FutureExt;
use http::Request;
use http_body::Body;
use send_wrapper::SendWrapper;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct Upgraded {
    reader: SendWrapper<Pin<Box<dyn AsyncRead + Unpin>>>,
    writer: SendWrapper<Pin<Box<dyn AsyncWrite + Unpin>>>,
    leftover: Option<bytes::Bytes>,
}

impl Upgraded {
    #[inline]
    pub(super) fn new(
        io: impl AsyncRead + AsyncWrite + Unpin + 'static,
        leftover: Option<bytes::Bytes>,
    ) -> Self {
        let (reader, writer) = tokio::io::split(io);
        Self {
            reader: SendWrapper::new(Box::pin(reader)),
            writer: SendWrapper::new(Box::pin(writer)),
            leftover,
        }
    }
}

impl AsyncRead for Upgraded {
    #[inline]
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(leftover) = &mut self.leftover {
            let slice_len = leftover.len().min(buf.remaining());
            let leftover_to_write = leftover.split_to(slice_len);
            buf.put(leftover_to_write);
            if leftover.is_empty() {
                self.leftover = None;
            }
            return std::task::Poll::Ready(Ok(()));
        }
        (*self.reader).as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for Upgraded {
    #[inline]
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        (*self.writer).as_mut().poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        (*self.writer).as_mut().poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        (*self.writer).as_mut().poll_shutdown(cx)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.writer.is_write_vectored()
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        (*self.writer).as_mut().poll_write_vectored(cx, bufs)
    }
}

#[derive(Clone)]
pub(super) struct Upgrade {
    inner: Arc<futures_util::lock::Mutex<oneshot::AsyncReceiver<Upgraded>>>,
    pub(super) upgraded: Arc<AtomicBool>,
}

impl Upgrade {
    #[inline]
    pub(super) fn new(inner: oneshot::AsyncReceiver<Upgraded>) -> Self {
        Self {
            inner: Arc::new(futures_util::lock::Mutex::new(inner)),
            upgraded: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Future for Upgrade {
    type Output = Option<Upgraded>;

    #[inline]
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut inner = ready!(self.inner.lock().poll_unpin(cx));
        match inner.poll_unpin(cx) {
            std::task::Poll::Ready(result) => std::task::Poll::Ready(result.ok()),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[derive(Clone)]
pub struct OnUpgrade {
    inner: Upgrade,
}

impl Future for OnUpgrade {
    type Output = Option<Upgraded>;

    #[inline]
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.poll_unpin(cx)
    }
}

pub fn prepare_upgrade(req: &mut Request<impl Body>) -> Option<OnUpgrade> {
    req.extensions_mut().remove::<Upgrade>().map(|inner| {
        inner
            .upgraded
            .store(true, std::sync::atomic::Ordering::Relaxed);
        OnUpgrade { inner }
    })
}
