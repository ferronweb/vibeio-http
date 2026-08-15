//! `quinn` transport adapter (behind the `h3-quinn` feature).
//!
//! Implements the transport traits of [`crate::h3::transport`] over the real
//! `quinn` 0.11 API, keeping the native HTTP/3 logic 100% quinn-free. The
//! mapping is mechanical:
//!
//! - [`quinn::Connection`] ↔ [`transport::Connection`]: `open_uni`/
//!   `accept_uni`/`accept_bi` are driven through persistent `stream::unfold`
//!   futures. The quinn accept/open futures are stateful — each poll
//!   registers a waiter that must stay alive across polls, so creating a
//!   fresh future every poll loses wakeups. The `unfold` streams keep one
//!   in-flight future for the adapter's whole lifetime, `close` for
//!   `poll_shutdown`, `side()` for stream-id parity, `handshake_data()` for
//!   handshake state.
//! - `quinn` uni streams are half-open: `open_uni` yields a write-only
//!   [`quinn::SendStream`] and `accept_uni` a read-only [`quinn::RecvStream`].
//!   The HTTP/3 layer needs both halves on one `UniStream` handle, so the
//!   adapter maps them onto an internal enum (see [`UniStream`]).
//! - `RecvStream::poll_recv` uses a [`ReusableBoxFuture`] wrapping
//!   `read_chunk(usize::MAX, true)` (ordered reads only), the same
//!   zero-copy pattern `h3-quinn` uses.
//! - `SendStream::poll_send` must consume its whole buffer, so partial
//!   `poll_write` results are staged in an internal `BytesMut` remainder.
//!
//! Error mapping follows RFC 9114 Section 8.1 semantics: application close
//! codes and stream errors carry their wire `u64` codes, everything else
//! degenerates to [`TransportError::Transport`].

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use futures_util::{ready, stream, Stream, StreamExt};
use tokio_util::sync::ReusableBoxFuture;

use crate::h3::error::TransportError;
use crate::h3::transport::{
    self, Accept, BidiStream as BidiStreamTrait, OpenStreams, RecvStream as RecvStreamTrait,
    SendStream as SendStreamTrait, UniStream as UniStreamTrait,
};

/// An infinite stream of `quinn` accept/open outcomes.
type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + std::marker::Send + 'static>>;

type AcceptBiItem = Result<(quinn::SendStream, quinn::RecvStream), quinn::ConnectionError>;
type AcceptUniItem = Result<quinn::RecvStream, quinn::ConnectionError>;
type OpenUniItem = Result<quinn::SendStream, quinn::ConnectionError>;

/// A `quinn` connection adapted to the HTTP/3 transport traits.
///
/// Wraps a[`quinn::Connection`]; clone the underlying connection and adapt
/// each side independently as needed.
pub struct Connection {
    conn: quinn::Connection,
    /// Persistent `accept_bi` futures.
    ///
    /// The quinn futures are stateful: a poll registers a waiter which must
    /// stay alive across polls, otherwise a wakeup arriving between polls is
    /// lost and the accept never completes. The `unfold` streams keep one
    /// future alive for the lifetime of the adapter.
    accept_bi: BoxStream<AcceptBiItem>,
    accept_uni: BoxStream<AcceptUniItem>,
    /// Lazily created the first time a stream is opened.
    open_uni: Option<BoxStream<OpenUniItem>>,
}

impl Connection {
    /// Adapts an established `quinn` connection.
    ///
    /// The given connection must already have completed its handshake (for
    /// example `Connecting::await` on the client side, or awaiting an
    /// `Incoming` on the server side).
    #[inline]
    pub fn new(conn: quinn::Connection) -> Self {
        let accept_bi = Box::pin(stream::unfold(conn.clone(), |conn| async move {
            Some((conn.accept_bi().await, conn))
        }));
        let accept_uni = Box::pin(stream::unfold(conn.clone(), |conn| async move {
            Some((conn.accept_uni().await, conn))
        }));
        Self {
            conn,
            accept_bi,
            accept_uni,
            open_uni: None,
        }
    }

    /// The reason the connection was closed, if it has been.
    ///
    /// Exposes the raw `quinn` close reason; the transport traits only report
    /// a closed connection via [`Accept::poll_accept`], so this is used to
    /// observe the application close code directly (diagnostics, tests).
    #[inline]
    pub fn close_reason(&self) -> Option<quinn::ConnectionError> {
        self.conn.close_reason()
    }
}

impl OpenStreams for Connection {
    #[inline]
    fn poll_open_uni(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Box<dyn UniStreamTrait>, TransportError>> {
        let stream = self.open_uni.get_or_insert_with(|| {
            let conn = self.conn.clone();
            Box::pin(stream::unfold(conn, |conn| async move {
                Some((conn.open_uni().await, conn))
            }))
        });
        match ready!(stream.poll_next_unpin(cx)) {
            Some(Ok(stream)) => Poll::Ready(Ok(Box::new(UniStream::Send(Send::new(stream))))),
            Some(Err(err)) => Poll::Ready(Err(map_connection_error(err))),
            None => unreachable!("unfold stream never ends"),
        }
    }
}

impl Accept for Connection {
    #[inline]
    fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Box<dyn BidiStreamTrait>>, TransportError>> {
        match ready!(self.accept_bi.as_mut().poll_next_unpin(cx)) {
            Some(Ok((send, recv))) => Poll::Ready(Ok(Some(Box::new(BidiStream {
                send: Send::new(send),
                recv: Recv::new(recv),
            })))),
            // The accept stream yields `Err` once the connection is closed
            // and every already-received stream has been drained; the trait
            // contract says that is `Ok(None)`.
            Some(Err(_)) => Poll::Ready(Ok(None)),
            None => unreachable!("unfold stream never ends"),
        }
    }

    #[inline]
    fn poll_accept_uni(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Box<dyn UniStreamTrait>>, TransportError>> {
        match ready!(self.accept_uni.as_mut().poll_next_unpin(cx)) {
            Some(Ok(stream)) => Poll::Ready(Ok(Some(Box::new(UniStream::Recv(Recv::new(stream)))))),
            Some(Err(_)) => Poll::Ready(Ok(None)),
            None => unreachable!("unfold stream never ends"),
        }
    }
}

impl transport::Connection for Connection {
    #[inline]
    fn stream_id_stream(&self) -> u64 {
        // RFC 9000 Section 2.1: the least significant bits encode the
        // initiator and directionality; the first client-initiated
        // unidirectional stream is 2, the first server-initiated one is 3.
        match self.conn.side() {
            quinn::Side::Client => 2,
            quinn::Side::Server => 3,
        }
    }

    #[inline]
    fn is_handshake_complete(&self) -> bool {
        self.conn.handshake_data().is_some()
    }

    #[inline]
    fn poll_shutdown(
        &mut self,
        _cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), TransportError>> {
        let code = match quinn::VarInt::from_u64(error_code) {
            Ok(code) => code,
            Err(_) => return Poll::Ready(Err(TransportError::Other)),
        };
        // quinn's `close` is synchronous and idempotent; it sends
        // CONNECTION_CLOSE immediately.
        self.conn.close(code, b"");
        Poll::Ready(Ok(()))
    }
}

/// The send half of a QUIC stream, adapted to [`SendStreamTrait`].
pub(crate) struct Send {
    stream: quinn::SendStream,
    /// Bytes queued for transmission but not yet consumed by `poll_write`.
    ///
    /// `poll_write` may only accept a prefix of the buffer before the
    /// stream's flow-control window is exhausted, so the remainder is kept
    /// here between polls.
    buf: bytes::BytesMut,
    /// Identity (data pointer + length) of the most recent caller slice that
    /// is still (partially) buffered in `buf`.
    ///
    /// Callers re-pass the same `&[u8]` on every re-poll after a
    /// flow-control `Pending` (their own queues keep the unsent bytes and
    /// are only popped once `Ready`). Without this marker, each re-poll would
    /// `extend_from_slice` the same bytes again, duplicating and growing
    /// `buf` without bound — fatal for large responses under backpressure.
    in_flight: Option<(usize, usize)>,
}

impl Send {
    #[inline]
    fn new(stream: quinn::SendStream) -> Self {
        Self {
            stream,
            buf: bytes::BytesMut::new(),
            in_flight: None,
        }
    }

    #[inline]
    fn id(&self) -> u64 {
        self.stream.id().into()
    }

    #[inline]
    fn poll_send(&mut self, cx: &mut Context<'_>, data: &[u8]) -> Poll<Result<(), TransportError>> {
        // Only buffer `data` when it is a fresh slice. Callers re-pass the
        // same `&[u8]` on each re-poll (their queue is popped only on
        // `Ready`); re-appending it would duplicate and grow `buf` forever.
        let id = (data.as_ptr() as usize, data.len());
        if !data.is_empty() && self.in_flight != Some(id) {
            self.buf.extend_from_slice(data);
            self.in_flight = Some(id);
        }
        loop {
            if self.buf.is_empty() {
                self.in_flight = None;
                return Poll::Ready(Ok(()));
            }
            // quinn's `poll_write` either writes a non-empty prefix (and
            // reports how much) or returns `Pending` due to flow control
            // with nothing written and a waker registered, so the loop can
            // never spin: once the window is full it parks.
            let written = ready!(Pin::new(&mut self.stream).poll_write(cx, &self.buf))
                .map_err(map_write_error)?;
            debug_assert!(written > 0);
            self.buf.advance(written);
        }
    }

    #[inline]
    fn poll_finish(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        // quinn's `finish` is synchronous; its only failure mode is an
        // already-finished or reset stream, which is not an error for the
        // caller.
        let _ = self.stream.finish();
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_reset(&mut self, _cx: &mut Context<'_>, code: u64) -> Poll<Result<(), TransportError>> {
        let code = match quinn::VarInt::from_u64(code) {
            Ok(code) => code,
            Err(_) => return Poll::Ready(Err(TransportError::Other)),
        };
        if self.stream.reset(code).is_err() {
            return Poll::Ready(Err(TransportError::Other));
        }
        Poll::Ready(Ok(()))
    }
}

type ReadChunkFuture = ReusableBoxFuture<
    'static,
    (
        quinn::RecvStream,
        Result<Option<quinn::Chunk>, quinn::ReadError>,
    ),
>;

/// The receive half of a QUIC stream, adapted to [`RecvStreamTrait`].
pub(crate) struct Recv {
    stream: Option<quinn::RecvStream>,
    /// In-flight `read_chunk` future.
    ///
    /// `ReusableBoxFuture` holds the borrowing `read_chunk` future across
    /// polls; the boxed stream is returned alongside the result and stashed
    /// back so the future can be re-armed for the next chunk.
    read_chunk_fut: ReadChunkFuture,
}

impl Recv {
    #[inline]
    fn new(stream: quinn::RecvStream) -> Self {
        Self {
            stream: Some(stream),
            read_chunk_fut: ReusableBoxFuture::new(async { unreachable!("armed before poll") }),
        }
    }

    #[inline]
    fn id(&self) -> u64 {
        self.stream.as_ref().map_or(0, |stream| stream.id().into())
    }

    #[inline]
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, TransportError>> {
        if let Some(mut stream) = self.stream.take() {
            self.read_chunk_fut.set(async move {
                let chunk = stream.read_chunk(usize::MAX, true).await;
                (stream, chunk)
            });
        }
        let (stream, chunk) = ready!(self.read_chunk_fut.poll(cx));
        self.stream = Some(stream);
        Poll::Ready(
            chunk
                .map_err(map_read_error)
                .map(|chunk| chunk.map(|chunk| chunk.bytes)),
        )
    }

    #[inline]
    fn stop_sending(&mut self, code: u64) -> Result<(), TransportError> {
        let code = match quinn::VarInt::from_u64(code) {
            Ok(code) => code,
            Err(_) => return Err(TransportError::Other),
        };
        match self.stream.as_mut() {
            Some(stream) => stream.stop(code).map_err(|_| TransportError::Other),
            None => Err(TransportError::Other),
        }
    }
}

/// A bidirectional QUIC stream (an HTTP/3 request stream).
pub(crate) struct BidiStream {
    send: Send,
    recv: Recv,
}

impl RecvStreamTrait for BidiStream {
    #[inline]
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, TransportError>> {
        self.recv.poll_recv(cx)
    }

    #[inline]
    fn id(&self) -> u64 {
        self.send.id()
    }
}

impl SendStreamTrait for BidiStream {
    #[inline]
    fn poll_send(&mut self, cx: &mut Context<'_>, data: &[u8]) -> Poll<Result<(), TransportError>> {
        self.send.poll_send(cx, data)
    }

    #[inline]
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        self.send.poll_finish(cx)
    }

    #[inline]
    fn poll_reset(&mut self, cx: &mut Context<'_>, code: u64) -> Poll<Result<(), TransportError>> {
        self.send.poll_reset(cx, code)
    }

    #[inline]
    fn poll_stop_sending(
        &mut self,
        _cx: &mut Context<'_>,
        code: u64,
    ) -> Poll<Result<(), TransportError>> {
        // `STOP_SENDING` is a direction-specific frame: on a bidirectional
        // stream it asks the peer to stop sending in *our* receive
        // direction, which is the receive half's job in quinn.
        Poll::Ready(self.recv.stop_sending(code))
    }
}

impl BidiStreamTrait for BidiStream {}

/// A unidirectional QUIC stream (control stream, QPACK encoder/decoder
/// streams, and any extension stream types).
///
/// The HTTP/3 layer observes both directions through this handle even though
/// quinn's uni streams are half-open: streams we opened are write-only
/// ([`UniStream::Send`]) and streams we accepted are read-only
/// ([`UniStream::Recv`]). Operations in the wrong direction surface a
/// [`TransportError::Transport`].
pub(crate) enum UniStream {
    /// A stream this endpoint opened: write-only.
    Send(Send),
    /// A stream the peer opened: read-only.
    Recv(Recv),
}

impl RecvStreamTrait for UniStream {
    #[inline]
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, TransportError>> {
        match self {
            UniStream::Recv(recv) => recv.poll_recv(cx),
            UniStream::Send(_) => Poll::Ready(Err(TransportError::Transport)),
        }
    }

    #[inline]
    fn id(&self) -> u64 {
        match self {
            UniStream::Send(send) => send.id(),
            UniStream::Recv(recv) => recv.id(),
        }
    }
}

impl SendStreamTrait for UniStream {
    #[inline]
    fn poll_send(&mut self, cx: &mut Context<'_>, data: &[u8]) -> Poll<Result<(), TransportError>> {
        match self {
            UniStream::Send(send) => send.poll_send(cx, data),
            UniStream::Recv(_) => Poll::Ready(Err(TransportError::Transport)),
        }
    }

    #[inline]
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        match self {
            UniStream::Send(send) => send.poll_finish(cx),
            UniStream::Recv(_) => Poll::Ready(Err(TransportError::Transport)),
        }
    }

    #[inline]
    fn poll_reset(&mut self, cx: &mut Context<'_>, code: u64) -> Poll<Result<(), TransportError>> {
        match self {
            UniStream::Send(send) => send.poll_reset(cx, code),
            UniStream::Recv(_) => Poll::Ready(Err(TransportError::Transport)),
        }
    }

    #[inline]
    fn poll_stop_sending(
        &mut self,
        _cx: &mut Context<'_>,
        code: u64,
    ) -> Poll<Result<(), TransportError>> {
        match self {
            UniStream::Recv(recv) => Poll::Ready(recv.stop_sending(code)),
            UniStream::Send(_) => Poll::Ready(Err(TransportError::Transport)),
        }
    }
}

impl UniStreamTrait for UniStream {}

#[inline]
fn map_connection_error(err: quinn::ConnectionError) -> TransportError {
    match err {
        quinn::ConnectionError::ApplicationClosed(close) => TransportError::Closed {
            code: close.error_code.into_inner(),
        },
        quinn::ConnectionError::TimedOut => TransportError::Timeout,
        // VersionMismatch, TransportError, ConnectionClosed, Reset,
        // LocallyClosed and CidsExhausted carry no application error code.
        _ => TransportError::Transport,
    }
}

#[inline]
fn map_read_error(err: quinn::ReadError) -> TransportError {
    match err {
        quinn::ReadError::Reset(code) => TransportError::Reset {
            code: code.into_inner(),
        },
        quinn::ReadError::ConnectionLost(err) => map_connection_error(err),
        // The stream was stopped, finished, or reset before any read.
        quinn::ReadError::ClosedStream => TransportError::Closed { code: 0 },
        // The adapter only performs ordered reads, so this cannot arise.
        quinn::ReadError::IllegalOrderedRead => TransportError::Transport,
        quinn::ReadError::ZeroRttRejected => TransportError::Transport,
    }
}

#[inline]
fn map_write_error(err: quinn::WriteError) -> TransportError {
    match err {
        quinn::WriteError::Stopped(code) => TransportError::Stopped {
            code: code.into_inner(),
        },
        quinn::WriteError::ConnectionLost(err) => map_connection_error(err),
        // The stream was finished or reset locally before the write.
        quinn::WriteError::ClosedStream => TransportError::Closed { code: 0 },
        quinn::WriteError::ZeroRttRejected => TransportError::Transport,
    }
}
