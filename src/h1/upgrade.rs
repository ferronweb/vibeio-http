use std::{
    future::Future,
    pin::Pin,
    sync::{atomic::AtomicBool, Arc},
    task::ready,
};

use bytes::BufMut;
use futures_util::FutureExt;
use http::Request;
use http_body::Body;
use send_wrapper::SendWrapper;
use tokio::io::{AsyncRead, AsyncWrite};

/// Represents a successfully upgraded HTTP/1.x connection.
///
/// After a successful HTTP upgrade handshake (e.g. WebSocket or HTTP/2
/// cleartext), the original TCP stream is handed off as an [`Upgraded`] value.
/// It implements both [`AsyncRead`] and [`AsyncWrite`], so it can be used as a
/// plain async I/O object by the protocol taking over the connection.
///
/// Any bytes that were already read from the socket as part of the HTTP request
/// but not yet consumed are prepended to the read stream via the `leftover`
/// buffer, ensuring no data is lost during the transition.
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

/// A future that resolves to an [`Upgraded`] connection once the HTTP upgrade
/// handshake has been completed by the server side.
///
/// Obtain an `OnUpgrade` by calling [`prepare_upgrade`] on an incoming
/// request. The future will yield `Some(Upgraded)` when the server has
/// finished writing the upgrade response, or `None` if the upgrade was
/// cancelled or the connection was closed before the handshake completed.
///
/// # Example
///
/// ```rust,ignore
/// if let Some(on_upgrade) = prepare_upgrade(&mut request) {
///     tokio::spawn(async move {
///         if let Some(upgraded) = on_upgrade.await {
///             // `upgraded` is now a raw async I/O stream
///         }
///     });
/// }
/// ```
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

/// Prepares an HTTP upgrade on the given request.
///
/// This function removes the internal `Upgrade` token from the request's
/// extensions and marks the connection as "to be upgraded". The returned
/// [`OnUpgrade`] future resolves to the raw [`Upgraded`] I/O stream after the
/// server has sent the `101 Switching Protocols` response.
///
/// Returns `None` if the request does not carry an upgrade token, which
/// happens when the connection handler was not configured to support upgrades
/// or the upgrade extension has already been consumed.
///
/// # Panics
///
/// Does not panic; returns `None` instead of panicking on missing state.
pub fn prepare_upgrade(req: &mut Request<impl Body>) -> Option<OnUpgrade> {
    req.extensions_mut().remove::<Upgrade>().map(|inner| {
        inner
            .upgraded
            .store(true, std::sync::atomic::Ordering::Relaxed);
        OnUpgrade { inner }
    })
}
