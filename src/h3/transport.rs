//! QUIC transport abstraction for the HTTP/3 implementation.
//!
//! The HTTP/3 layer (RFC 9114) is written against these traits instead of a
//! concrete QUIC stack, so any QUIC implementation can be adapted (the
//! `quinn` adapter behind the `h3-quinn` feature is one such adapter). The
//! shape mirrors the `h3` crate's `quic` module so adapters are mechanical:
//! polling only, `Unpin`, fallible operations returning
//! [`TransportError`].
//!
//! The HTTP/3 layer needs three things from a QUIC stack:
//!
//! - this endpoint's own outbound unidirectional streams (control stream,
//!   QPACK encoder/decoder streams) via [`OpenStreams`];
//! - the peer's inbound streams — request streams via [`Accept`], plus the
//!   peer's unidirectional streams (their control stream and QPACK
//!   streams) — and
//! - connection-level state: handshake completion and graceful shutdown.
//!
//! Streams are consumed as trait objects (`Box<dyn BidiStream>`,
//! `Box<dyn UniStream>`): the connection driver is a single task that
//! spawns per-request tasks, so the boxes it hands off must be neither
//! generic-bound nor borrowed. QUIC stream handles are cheap to move, so
//! boxing adds no meaningful cost.

use std::task::{Context, Poll};

use bytes::Bytes;

use crate::h3::error::TransportError;

/// The receive half of a QUIC stream.
///
/// Polls for the next chunk of received bytes; the stream is finished when
/// `Ok(None)` is returned (the peer closed its sending side with `FIN`).
/// A `TransportError::Reset` is returned when the peer resets the stream
/// instead.
pub trait RecvStream: Unpin {
    /// Polls for the next chunk of data on the stream.
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, TransportError>>;
}

/// The send half of a QUIC stream.
pub trait SendStream: Unpin {
    /// Polls to send `data` on the stream, consuming it entirely.
    ///
    /// `Poll::Pending` means the stream's flow-control window is exhausted;
    /// the caller must poll again later. A `TransportError::Stopped` is
    /// returned when the peer sends `STOP_SENDING`.
    fn poll_send(&mut self, cx: &mut Context<'_>, data: &[u8]) -> Poll<Result<(), TransportError>>;

    /// Polls to finish the sending side of the stream (`FIN`). The peer
    /// then observes `Ok(None)` from its receive side when all data is
    /// delivered.
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>>;

    /// Polls to reset the sending side of the stream with an application
    /// error `code` (RFC 9114 error codes), discarding buffered data.
    fn poll_reset(&mut self, cx: &mut Context<'_>, code: u64) -> Poll<Result<(), TransportError>>;

    /// Polls to stop the peer from sending on the stream (`STOP_SENDING`)
    /// with an application error `code`.
    fn poll_stop_sending(
        &mut self,
        cx: &mut Context<'_>,
        code: u64,
    ) -> Poll<Result<(), TransportError>>;
}

/// A bidirectional QUIC stream: an HTTP/3 request stream.
pub trait BidiStream: RecvStream + SendStream {}

/// A unidirectional QUIC stream: the control stream, the QPACK encoder and
/// decoder streams, and any extension stream types.
pub trait UniStream: RecvStream + SendStream {}

/// Opens outbound unidirectional streams.
///
/// The HTTP/3 layer uses this to create its control stream and QPACK
/// encoder/decoder streams.
pub trait OpenStreams {
    /// Polls the connection to open a new unidirectional stream.
    fn poll_open_uni(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Box<dyn UniStream>, TransportError>>;
}

/// Accepts inbound streams.
///
/// Inbound streams must not be accepted out of order: the HTTP/3 layer
/// assigns meanings to unidirectional streams by stream type and to
/// request streams by stream ID (RFC 9114 Section 6), so an adapter must
/// deliver streams in the order the peer created them.
pub trait Accept {
    /// Polls for the next accepted bidirectional (request) stream.
    ///
    /// `Ok(None)` means no more streams will be accepted (the connection is
    /// closing or closed).
    fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Box<dyn BidiStream>>, TransportError>>;

    /// Polls for the next accepted unidirectional stream (the peer's
    /// control stream and QPACK streams).
    ///
    /// `Ok(None)` means no more streams will be accepted.
    fn poll_accept_uni(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Box<dyn UniStream>>, TransportError>>;
}

/// A QUIC connection as seen by the HTTP/3 layer.
pub trait Connection: OpenStreams + Accept {
    /// The stream ID of this endpoint's control stream.
    ///
    /// For a server this is the first server-initiated unidirectional
    /// stream (ID 3); for a client the first client-initiated one (ID 2).
    /// The driver uses it to detect when the control stream must be
    /// reopened and to reason about stream ID parity.
    fn stream_id_stream(&self) -> u64;

    /// Whether the handshake has completed.
    ///
    /// The accept and handshake timeouts of the HTTP/3 layer are measured
    /// against this: a connection must not be timed out before its
    /// handshake completes.
    fn is_handshake_complete(&self) -> bool;

    /// Polls to shut down the connection gracefully with an application
    /// error `code`, allowing in-flight streams to drain.
    fn poll_shutdown(
        &mut self,
        cx: &mut Context<'_>,
        error_code: u64,
    ) -> Poll<Result<(), TransportError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::task::noop_waker_ref;

    fn cx() -> Context<'static> {
        Context::from_waker(noop_waker_ref())
    }

    /// Minimal mock streams proving the traits are object-safe and usable
    /// through `Box<dyn ...>` the way the connection driver consumes them.
    struct Mock {
        data: Option<Bytes>,
    }

    impl RecvStream for Mock {
        fn poll_recv(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<Option<Bytes>, TransportError>> {
            Poll::Ready(Ok(self.data.take()))
        }
    }

    impl SendStream for Mock {
        fn poll_send(
            &mut self,
            _cx: &mut Context<'_>,
            _data: &[u8],
        ) -> Poll<Result<(), TransportError>> {
            Poll::Ready(Ok(()))
        }

        fn poll_finish(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
            Poll::Ready(Ok(()))
        }

        fn poll_reset(
            &mut self,
            _cx: &mut Context<'_>,
            _code: u64,
        ) -> Poll<Result<(), TransportError>> {
            Poll::Ready(Ok(()))
        }

        fn poll_stop_sending(
            &mut self,
            _cx: &mut Context<'_>,
            _code: u64,
        ) -> Poll<Result<(), TransportError>> {
            Poll::Ready(Ok(()))
        }
    }

    impl BidiStream for Mock {}
    impl UniStream for Mock {}

    #[test]
    fn bidi_stream_object_drains() {
        let mut stream: Box<dyn BidiStream> = Box::new(Mock {
            data: Some(Bytes::from_static(b"hi")),
        });
        let mut cx = cx();
        assert_eq!(
            stream.poll_recv(&mut cx),
            Poll::Ready(Ok(Some(Bytes::from_static(b"hi"))))
        );
        assert_eq!(stream.poll_recv(&mut cx), Poll::Ready(Ok(None)));
        assert!(stream.poll_send(&mut cx, b"x").is_ready());
        assert!(stream.poll_finish(&mut cx).is_ready());
        assert!(stream.poll_reset(&mut cx, 0x10c).is_ready());
        assert!(stream.poll_stop_sending(&mut cx, 0x10c).is_ready());
    }

    #[test]
    fn uni_stream_object_drains() {
        let mut stream: Box<dyn UniStream> = Box::new(Mock { data: None });
        let mut cx = cx();
        assert_eq!(stream.poll_recv(&mut cx), Poll::Ready(Ok(None)));
        assert!(stream.poll_send(&mut cx, b"y").is_ready());
        assert!(stream.poll_finish(&mut cx).is_ready());
    }
}
