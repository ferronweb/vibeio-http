//! HTTP/3 control plane: the control stream, QPACK streams, and the
//! connection-level state that governs them (RFC 9114 Section 6).
//!
//! This is the connection driver's control half. It owns:
//!
//! - the three outbound unidirectional streams this endpoint must open
//!   (control, QPACK encoder, QPACK decoder) via [`ControlStreams::poll_init`],
//!   the SETTINGS frame that must be its control stream's first frame, and
//!   the GOAWAY/CANCEL_PUSH/MAX_PUSH_ID frames the driver queues with the
//!   `send_*` methods and writes with [`ControlStreams::poll_flush`];
//! - the peer's inbound unidirectional streams, classified by stream
//!   type, and the peer's control stream, validated frame by frame
//!   ([`ControlStreams::poll_read`]);
//! - the QPACK decoder (sized by our own SETTINGS at construction) and
//!   encoder (created once the peer's SETTINGS bound its dynamic table).
//!   The encoder's stream instructions and the decoder's acknowledgements
//!   flow out on the QPACK streams; field sections the decoder unblocks
//!   are queued for the request-stream handler.
//!
//! Rules enforced here, all from RFC 9114 Section 6.2: a second control
//! stream, a push stream (server side), a duplicate QPACK stream, or an
//! unknown stream type is `H3_STREAM_CREATION_ERROR`; the first frame of
//! the control stream must be SETTINGS (`H3_MISSING_SETTINGS` otherwise);
//! a second SETTINGS or any DATA/HEADERS/PUSH_PROMISE on the control
//! stream is `H3_FRAME_UNEXPECTED`; a CANCEL_PUSH for a push this endpoint
//! never promised is `H3_ID_ERROR`; a reduced MAX_PUSH_ID is `H3_ID_ERROR`;
//! the peer closing any critical stream is `H3_CLOSED_CRITICAL_STREAM` (a
//! control stream that ends before SETTINGS is `H3_MISSING_SETTINGS`).
//!
//! The peer's QPACK decoder stream is drained and discarded: the encoder
//! emits instructions without tracking acknowledgements, so the stream is
//! consumed only to keep flow control moving.
#![allow(dead_code)] // consumed by the connection driver (step 15)

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_util::ready;

use crate::h3::error::{H3Error, TransportError};
use crate::h3::frame::{self, Frame, FrameDecoder, FrameError};
use crate::h3::qpack::{Encoder, QpackError, UnblockedSection};
use crate::h3::settings::{LocalSettings, PeerSettings};
use crate::h3::stream::SharedCodecs;
use crate::h3::transport::{Connection, UniStream};

/// Uni stream type: control stream (RFC 9114 Section 6.2.1).
pub(crate) const STREAM_TYPE_CONTROL: u64 = 0x0;
/// Uni stream type: push stream (RFC 9114 Section 6.2.2).
pub(crate) const STREAM_TYPE_PUSH: u64 = 0x1;
/// Uni stream type: QPACK encoder stream (RFC 9204 Section 4.2).
pub(crate) const STREAM_TYPE_QPACK_ENCODER: u64 = 0x2;
/// Uni stream type: QPACK decoder stream (RFC 9204 Section 4.4).
pub(crate) const STREAM_TYPE_QPACK_DECODER: u64 = 0x3;

/// A control-plane failure, each mapped to the connection error code the
/// driver closes with (see [`ControlError::h3_code`]).
#[derive(Debug)]
pub(crate) enum ControlError {
    /// The transport failed.
    Transport(TransportError),
    /// A critical stream (control, QPACK encoder or decoder) was closed or
    /// reset by the peer (`H3_CLOSED_CRITICAL_STREAM`).
    ClosedCriticalStream,
    /// The control stream's first frame was not SETTINGS, or the control
    /// stream ended without any (`H3_MISSING_SETTINGS`).
    MissingSettings,
    /// An unknown uni stream type, a duplicate control/QPACK stream, or a
    /// push stream on the server (`H3_STREAM_CREATION_ERROR`).
    StreamCreation,
    /// A second SETTINGS, or a frame that is not permitted on the control
    /// stream (`H3_FRAME_UNEXPECTED`).
    FrameUnexpected,
    /// A malformed frame payload (`H3_FRAME_ERROR`).
    Frame,
    /// Reserved or duplicate setting identifiers (`H3_SETTINGS_ERROR`).
    Settings,
    /// A CANCEL_PUSH for a never-promised push, or a reduced MAX_PUSH_ID
    /// (`H3_ID_ERROR`).
    Id,
    /// The peer's QPACK encoder stream was malformed (RFC 9204 Section 6
    /// error family `0x02xx`).
    Qpack(QpackError),
}

impl ControlError {
    /// The connection error code to close with.
    pub(crate) fn h3_code(&self) -> u64 {
        match self {
            ControlError::Transport(_) => H3Error::GeneralProtocol.code(),
            ControlError::ClosedCriticalStream => H3Error::ClosedCriticalStream.code(),
            ControlError::MissingSettings => H3Error::MissingSettings.code(),
            ControlError::StreamCreation => H3Error::StreamCreation.code(),
            ControlError::FrameUnexpected => H3Error::FrameUnexpected.code(),
            ControlError::Frame => H3Error::FrameError.code(),
            ControlError::Settings => H3Error::Settings.code(),
            ControlError::Id => H3Error::Id.code(),
            ControlError::Qpack(err) => u64::from(err.code()),
        }
    }
}

impl From<TransportError> for ControlError {
    fn from(err: TransportError) -> Self {
        ControlError::Transport(err)
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ControlError {}

fn map_frame_error(err: FrameError) -> ControlError {
    match err {
        FrameError::Frame => ControlError::Frame,
        FrameError::Unexpected(_) => ControlError::FrameUnexpected,
        FrameError::Settings => ControlError::Settings,
    }
}

/// A control-plane event for the connection driver.
#[derive(Debug)]
pub(crate) enum ControlEvent {
    /// The peer's SETTINGS, the first frame of its control stream.
    Settings(PeerSettings),
    /// The peer's GOAWAY. In the server-to-client direction `id` is a
    /// stream ID; in the client-to-server direction a push ID.
    Goaway { id: u64 },
    /// The peer's MAX_PUSH_ID (client to server; a server never sends it).
    MaxPushId { id: u64 },
    /// The peer's CANCEL_PUSH (client to server; it is `H3_ID_ERROR` for a
    /// server that never promised the push).
    CancelPush { push_id: u64 },
}

/// The peer's control stream plus its frame decoder.
struct PeerControl {
    stream: Box<dyn UniStream>,
    decoder: FrameDecoder,
}

/// A freshly accepted uni stream whose type varint is still being read.
struct PendingUni {
    stream: Box<dyn UniStream>,
    buf: BytesMut,
}

/// The connection's control plane (RFC 9114 Section 6).
///
/// Owned by the connection driver on its single task: `poll_init` opens
/// and primes the outbound streams, `poll_read` accepts and services the
/// peer's streams and yields [`ControlEvent`]s, `poll_flush` writes
/// everything queued on the outbound streams.
pub(crate) struct ControlStreams {
    local: LocalSettings,
    peer: PeerSettings,

    // Outbound streams and their pending bytes.
    out_control: Option<Box<dyn UniStream>>,
    control_buf: BytesMut,
    out_encoder: Option<Box<dyn UniStream>>,
    out_decoder: Option<Box<dyn UniStream>>,
    encoder_pending: VecDeque<Bytes>,
    decoder_pending: VecDeque<Bytes>,

    // Inbound streams.
    in_control: Option<PeerControl>,
    in_encoder: Option<Box<dyn UniStream>>,
    in_decoder: Option<Box<dyn UniStream>>,
    pending_uni: Option<PendingUni>,

    // The connection's QPACK codecs, shared with the request streams. The
    // decoder's capacity is fixed by our own SETTINGS; the encoder is
    // created when the peer's SETTINGS bound its table.
    shared: Arc<Mutex<SharedCodecs>>,

    settings_received: bool,
    max_push_id: Option<u64>,
    goaway_sent: Option<u64>,

    events: VecDeque<ControlEvent>,
}

impl ControlStreams {
    /// Creates the control plane for a connection with the given local
    /// settings. The QPACK decoder is sized by them; nothing is sent until
    /// [`ControlStreams::poll_init`].
    pub(crate) fn new(local: LocalSettings) -> Self {
        let shared = Arc::new(Mutex::new(SharedCodecs::new(&local)));
        Self {
            local,
            peer: PeerSettings::default(),
            out_control: None,
            control_buf: BytesMut::new(),
            out_encoder: None,
            out_decoder: None,
            encoder_pending: VecDeque::new(),
            decoder_pending: VecDeque::new(),
            in_control: None,
            in_encoder: None,
            in_decoder: None,
            pending_uni: None,
            shared,
            settings_received: false,
            max_push_id: None,
            goaway_sent: None,
            events: VecDeque::new(),
        }
    }

    /// The peer's settings once its SETTINGS frame arrived (defaults
    /// before that).
    pub(crate) fn peer_settings(&self) -> &PeerSettings {
        &self.peer
    }

    /// The QPACK codecs shared with the connection's request streams.
    ///
    /// The decoder's capacity is fixed by our own SETTINGS; the encoder is
    /// created once the peer's SETTINGS bound its table (see
    /// [`ControlStreams::poll_read`]).
    pub(crate) fn shared(&self) -> &Arc<Mutex<SharedCodecs>> {
        &self.shared
    }

    /// Field sections the peer's encoder stream unblocked, drained for the
    /// request-stream handler.
    pub(crate) fn take_unblocked(&mut self) -> Vec<UnblockedSection> {
        std::mem::take(
            &mut self
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unblocked,
        )
    }

    /// Whether the peer's SETTINGS frame was received.
    pub(crate) fn settings_received(&self) -> bool {
        self.settings_received
    }

    /// The peer's MAX_PUSH_ID, once received.
    pub(crate) fn max_push_id(&self) -> Option<u64> {
        self.max_push_id
    }

    /// The ID this endpoint announced in its GOAWAY, once sent.
    pub(crate) fn goaway_sent(&self) -> Option<u64> {
        self.goaway_sent
    }

    /// Whether a GOAWAY has been sent; the driver then rejects request
    /// streams above the announced ID and closes with `H3_NO_ERROR` once
    /// they drain.
    pub(crate) fn shutting_down(&self) -> bool {
        self.goaway_sent.is_some()
    }

    /// Opens the control, QPACK encoder, and QPACK decoder streams and
    /// queues the SETTINGS frame that must open the control stream.
    /// Idempotent; call until `Ready`.
    pub(crate) fn poll_init(
        &mut self,
        conn: &mut dyn Connection,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), ControlError>> {
        if self.out_control.is_none() {
            let stream = ready!(conn.poll_open_uni(cx).map_err(ControlError::from))?;
            self.out_control = Some(stream);
            let mut settings = BytesMut::new();
            Frame::Settings(self.local.to_frame()).encode(&mut settings);
            self.control_buf.extend_from_slice(&settings);
        }
        if self.out_encoder.is_none() {
            let stream = ready!(conn.poll_open_uni(cx).map_err(ControlError::from))?;
            self.out_encoder = Some(stream);
        }
        if self.out_decoder.is_none() {
            let stream = ready!(conn.poll_open_uni(cx).map_err(ControlError::from))?;
            self.out_decoder = Some(stream);
        }
        Poll::Ready(Ok(()))
    }

    /// Writes everything queued on the outbound control and QPACK streams.
    /// Call until `Ready`.
    pub(crate) fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), ControlError>> {
        if let Some(stream) = self.out_control.as_mut() {
            if !self.control_buf.is_empty() {
                ready!(stream
                    .poll_send(cx, &self.control_buf)
                    .map_err(ControlError::from)?);
                self.control_buf.clear();
            }
        }
        if let Some(stream) = self.out_encoder.as_mut() {
            while let Some(bytes) = self.encoder_pending.pop_front() {
                ready!(stream.poll_send(cx, &bytes).map_err(ControlError::from)?);
            }
        }
        if let Some(stream) = self.out_decoder.as_mut() {
            while let Some(bytes) = self.decoder_pending.pop_front() {
                ready!(stream.poll_send(cx, &bytes).map_err(ControlError::from)?);
            }
        }
        Poll::Ready(Ok(()))
    }

    /// Queues a GOAWAY frame for the control stream and remembers `id` as
    /// the last request stream this endpoint will process. Only the first
    /// GOAWAY is sent; later calls are no-ops.
    pub(crate) fn send_goaway(&mut self, id: u64) {
        if self.goaway_sent.is_some() {
            return;
        }
        let mut buf = BytesMut::new();
        Frame::Goaway(id).encode(&mut buf);
        self.control_buf.extend_from_slice(&buf);
        self.goaway_sent = Some(id);
    }

    /// Queues a MAX_PUSH_ID frame (client side only).
    pub(crate) fn send_max_push_id(&mut self, id: u64) {
        let mut buf = BytesMut::new();
        Frame::MaxPushId(id).encode(&mut buf);
        self.control_buf.extend_from_slice(&buf);
    }

    /// Queues a CANCEL_PUSH frame (client side only).
    pub(crate) fn send_cancel_push(&mut self, push_id: u64) {
        let mut buf = BytesMut::new();
        Frame::CancelPush(push_id).encode(&mut buf);
        self.control_buf.extend_from_slice(&buf);
    }

    /// Queues encoder stream instructions (e.g. the section-encoding
    /// output of [`Encoder::encode_section`]) for the QPACK encoder
    /// stream; [`ControlStreams::poll_flush`] writes them.
    pub(crate) fn queue_encoder_stream(&mut self, bytes: Bytes) {
        if !bytes.is_empty() {
            self.encoder_pending.push_back(bytes);
        }
    }

    /// Accepts and services the peer's unidirectional streams and reads
    /// its control stream, yielding at most one event per call.
    ///
    /// Polling this in a loop drains everything the peer sent; `Pending`
    /// means all inbound inputs are idle and the transport registered a
    /// wakeup for the next chunk.
    pub(crate) fn poll_read(
        &mut self,
        conn: &mut dyn Connection,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<ControlEvent>, ControlError>> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Poll::Ready(Ok(Some(event)));
            }
            let mut progressed = false;

            // Accept one new peer uni stream; only when the previous one
            // has been classified (the transport delivers in order).
            if self.pending_uni.is_none() {
                match conn.poll_accept_uni(cx).map_err(ControlError::from)? {
                    Poll::Ready(Some(stream)) => {
                        self.pending_uni = Some(PendingUni {
                            stream,
                            buf: BytesMut::new(),
                        });
                        progressed = true;
                    }
                    Poll::Ready(None) | Poll::Pending => {}
                }
            }

            // Advance classification of the pending stream's type varint.
            if self.pending_uni.is_some() {
                if let Poll::Ready(()) = self.classify_uni(cx)? {
                    progressed = true;
                }
            }

            // Read the peer's control stream and validate its frames.
            if let Some(control) = self.in_control.as_mut() {
                match control.stream.poll_recv(cx).map_err(ControlError::from)? {
                    Poll::Ready(Some(chunk)) => {
                        control.decoder.extend(chunk);
                        progressed = true;
                    }
                    Poll::Ready(None) => {
                        // Closing the control stream (a critical stream) is
                        // always a connection error, regardless of whether
                        // SETTINGS was seen first (RFC 9114 Section 6.2).
                        return Poll::Ready(Err(ControlError::ClosedCriticalStream));
                    }
                    Poll::Pending => {}
                }
            }
            loop {
                // The peer may open and send on its QPACK streams before
                // its control stream exists; only drain frames when it
                // does.
                let frame = {
                    let control = match self.in_control.as_mut() {
                        Some(control) => control,
                        None => break,
                    };
                    match control.decoder.next_frame() {
                        Ok(Some(frame)) => Some(frame),
                        Ok(None) => None,
                        Err(err) => return Poll::Ready(Err(map_frame_error(err))),
                    }
                };
                match frame {
                    Some(frame) => {
                        progressed = true;
                        self.handle_control_frame(frame)?;
                    }
                    None => break,
                }
            }

            // Feed the peer's QPACK encoder stream to the decoder.
            if let Some(stream) = self.in_encoder.as_mut() {
                match stream.poll_recv(cx).map_err(ControlError::from)? {
                    Poll::Ready(Some(chunk)) => {
                        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                        match shared.decoder.feed_encoder_stream(&chunk) {
                            Ok(mut unblocked) => shared.unblocked.append(&mut unblocked),
                            Err(err) => return Poll::Ready(Err(ControlError::Qpack(err))),
                        }
                        let acks = shared.decoder.take_decoder_stream();
                        let waiters = shared.take_waiters();
                        drop(shared);
                        for waker in waiters {
                            waker.wake();
                        }
                        if !acks.is_empty() {
                            self.decoder_pending.push_back(acks);
                        }
                        progressed = true;
                    }
                    Poll::Ready(None) => {
                        return Poll::Ready(Err(ControlError::ClosedCriticalStream));
                    }
                    Poll::Pending => {}
                }
            }

            // Feed the peer's QPACK decoder stream to the decoder. Its
            // instructions (Section Acknowledgments, Stream Cancellations,
            // Insert Count Increments) acknowledge our encoder's output; the
            // encoder does not track them, but an Insert Count Increment of 0
            // is a decoder stream error (RFC 9204 Section 4.4.3).
            if let Some(stream) = self.in_decoder.as_mut() {
                match stream.poll_recv(cx).map_err(ControlError::from)? {
                    Poll::Ready(Some(chunk)) => {
                        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                        if let Err(err) = shared.decoder.feed_decoder_stream(&chunk) {
                            drop(shared);
                            return Poll::Ready(Err(ControlError::Qpack(err)));
                        }
                        progressed = true;
                    }
                    Poll::Ready(None) => {
                        return Poll::Ready(Err(ControlError::ClosedCriticalStream));
                    }
                    Poll::Pending => {}
                }
            }

            if !progressed {
                return Poll::Pending;
            }
        }
    }

    /// Reads the type varint of the stream under classification and wires
    /// it to its handler. The stream stays pending while the varint is
    /// incomplete.
    fn classify_uni(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), ControlError>> {
        loop {
            let chunk = {
                let pending = self.pending_uni.as_mut().expect("pending uni stream");
                match pending.stream.poll_recv(cx).map_err(ControlError::from)? {
                    Poll::Ready(Some(chunk)) => chunk,
                    Poll::Ready(None) => {
                        // The stream ended without a complete type: an
                        // unknown stream type (RFC 9114 Section 6.2).
                        self.pending_uni = None;
                        return Poll::Ready(Err(ControlError::StreamCreation));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            };
            self.pending_uni
                .as_mut()
                .expect("pending uni stream")
                .buf
                .extend_from_slice(&chunk);
            let (ty, used) = {
                let pending = self.pending_uni.as_ref().expect("pending uni stream");
                match frame::parse_varint(&pending.buf).map_err(map_frame_error)? {
                    Some((ty, used)) => (ty, used),
                    None => continue,
                }
            };
            let mut pending = self.pending_uni.take().expect("pending uni stream");
            let leftover = pending.buf.split_off(used);
            let stream = pending.stream;
            match ty {
                STREAM_TYPE_CONTROL => {
                    if self.in_control.is_some() {
                        return Poll::Ready(Err(ControlError::StreamCreation));
                    }
                    let mut decoder = FrameDecoder::new();
                    decoder.extend(leftover.freeze());
                    self.in_control = Some(PeerControl { stream, decoder });
                }
                STREAM_TYPE_PUSH => return Poll::Ready(Err(ControlError::StreamCreation)),
                STREAM_TYPE_QPACK_ENCODER => {
                    if self.in_encoder.is_some() {
                        return Poll::Ready(Err(ControlError::StreamCreation));
                    }
                    if !leftover.is_empty() {
                        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                        match shared.decoder.feed_encoder_stream(&leftover) {
                            Ok(mut unblocked) => shared.unblocked.append(&mut unblocked),
                            Err(err) => return Poll::Ready(Err(ControlError::Qpack(err))),
                        }
                        let acks = shared.decoder.take_decoder_stream();
                        let waiters = shared.take_waiters();
                        drop(shared);
                        for waker in waiters {
                            waker.wake();
                        }
                        if !acks.is_empty() {
                            self.decoder_pending.push_back(acks);
                        }
                    }
                    self.in_encoder = Some(stream);
                }
                STREAM_TYPE_QPACK_DECODER => {
                    if self.in_decoder.is_some() {
                        return Poll::Ready(Err(ControlError::StreamCreation));
                    }
                    if !leftover.is_empty() {
                        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                        match shared.decoder.feed_decoder_stream(&leftover) {
                            Ok(()) => {}
                            Err(err) => return Poll::Ready(Err(ControlError::Qpack(err))),
                        }
                    }
                    self.in_decoder = Some(stream);
                }
                _ => return Poll::Ready(Err(ControlError::StreamCreation)),
            }
            return Poll::Ready(Ok(()));
        }
    }

    /// Validates one frame read from the peer's control stream.
    fn handle_control_frame(&mut self, frame: Frame) -> Result<(), ControlError> {
        // RFC 9114 Section 6.2.1: the first frame must be SETTINGS; any
        // other first frame is H3_MISSING_SETTINGS.
        if !self.settings_received && !matches!(&frame, Frame::Settings(_)) {
            return Err(ControlError::MissingSettings);
        }
        match frame {
            Frame::Settings(settings) => {
                if self.settings_received {
                    // RFC 9114 Section 7.2.4: a second SETTINGS frame is
                    // H3_FRAME_UNEXPECTED.
                    return Err(ControlError::FrameUnexpected);
                }
                self.settings_received = true;
                self.peer.apply(&settings);
                let waiters = {
                    let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                    shared.encoder = Some(Encoder::new(self.peer.qpack_max_table_capacity(), true));
                    shared.peer_max_field_section_size = self.peer.max_field_section_size();
                    shared.take_waiters()
                };
                for waker in waiters {
                    waker.wake();
                }
                self.events
                    .push_back(ControlEvent::Settings(self.peer.clone()));
            }
            Frame::Goaway(id) => {
                self.events.push_back(ControlEvent::Goaway { id });
            }
            Frame::MaxPushId(id) => {
                // RFC 9114 Section 7.2.7: the maximum push ID must not be
                // reduced.
                if let Some(prev) = self.max_push_id {
                    if id < prev {
                        return Err(ControlError::Id);
                    }
                }
                self.max_push_id = Some(id);
                self.events.push_back(ControlEvent::MaxPushId { id });
            }
            Frame::CancelPush(_push_id) => {
                // RFC 9114 Section 7.2.3: a server that never sends
                // PUSH_PROMISE treats any CANCEL_PUSH as referencing a
                // push ID it never mentioned, i.e. H3_ID_ERROR.
                return Err(ControlError::Id);
            }
            Frame::Data(_) | Frame::Headers(_) | Frame::PushPromise { .. } => {
                // RFC 9114 Sections 7.2.1, 7.2.2 and 7.2.5: not permitted
                // on the control stream.
                return Err(ControlError::FrameUnexpected);
            }
        }
        Ok(())
    }

    /// Moves the QPACK decoder's accumulated acknowledgements (Section
    /// Acknowledgment, Insert Count Increment) to the outbound decoder
    /// stream queue.
    fn queue_decoder_acks(&mut self) {
        let acks = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .decoder
            .take_decoder_stream();
        if !acks.is_empty() {
            self.decoder_pending.push_back(acks);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h3::frame::{Settings as FrameSettings, SETTINGS_QPACK_MAX_TABLE_CAPACITY};
    use crate::h3::transport::{Accept, Connection, OpenStreams, RecvStream, SendStream};
    use futures_util::task::noop_waker_ref;

    fn cx() -> Context<'static> {
        Context::from_waker(noop_waker_ref())
    }

    /// Shared send log: every stream this endpoint opens records the bytes
    /// written to it, in order.
    type SendLog = std::sync::Arc<std::sync::Mutex<Vec<Bytes>>>;

    /// In-memory uni stream: the test feeds peer data into `inbound`; bytes
    /// this endpoint sends land in the shared log and per-stream
    /// `outbound`. An empty inbound queue reads as `Pending`; a `None`
    /// marker means the peer finished the stream. `inbound` is shared so a
    /// stream can be fed even after it was moved into the plane.
    struct MockStream {
        inbound: std::sync::Arc<std::sync::Mutex<VecDeque<Option<Bytes>>>>,
        outbound: VecDeque<Bytes>,
        log: SendLog,
    }

    impl MockStream {
        fn new(log: SendLog) -> Self {
            Self {
                inbound: std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new())),
                outbound: VecDeque::new(),
                log,
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.inbound
                .lock()
                .unwrap()
                .push_back(Some(Bytes::copy_from_slice(bytes)));
        }

        fn finish(&mut self) {
            self.inbound.lock().unwrap().push_back(None);
        }

        /// A handle to the inbound queue, to feed the stream after it has
        /// been moved into the plane.
        fn sink(&self) -> std::sync::Arc<std::sync::Mutex<VecDeque<Option<Bytes>>>> {
            self.inbound.clone()
        }

        fn take_outbound(&mut self) -> Bytes {
            let mut joined = BytesMut::new();
            for chunk in self.outbound.drain(..) {
                joined.extend_from_slice(&chunk);
            }
            joined.freeze()
        }
    }

    impl RecvStream for MockStream {
        fn poll_recv(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<Option<Bytes>, TransportError>> {
            match self.inbound.lock().unwrap().pop_front() {
                Some(chunk) => Poll::Ready(Ok(chunk)),
                None => Poll::Pending,
            }
        }

        fn id(&self) -> u64 {
            0
        }
    }

    impl SendStream for MockStream {
        fn poll_send(
            &mut self,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<Result<(), TransportError>> {
            let bytes = Bytes::copy_from_slice(data);
            self.log.lock().unwrap().push(bytes.clone());
            self.outbound.push_back(bytes);
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

    impl UniStream for MockStream {}

    /// In-memory QUIC connection: queues the peer's uni streams and hands
    /// out fresh streams for this endpoint's opens.
    struct MockConn {
        peer_unis: VecDeque<Box<dyn UniStream>>,
        opened: usize,
        log: SendLog,
        shutdown_code: Option<u64>,
    }

    impl MockConn {
        fn new() -> Self {
            Self {
                peer_unis: VecDeque::new(),
                opened: 0,
                log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                shutdown_code: None,
            }
        }

        /// Queues a peer uni stream preloaded with a mock reader.
        fn peer_uni(log: SendLog) -> Box<dyn UniStream> {
            Box::new(MockStream::new(log))
        }
    }

    impl OpenStreams for MockConn {
        fn poll_open_uni(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<Box<dyn UniStream>, TransportError>> {
            self.opened += 1;
            Poll::Ready(Ok(Box::new(MockStream::new(self.log.clone()))))
        }
    }

    impl Accept for MockConn {
        fn poll_accept(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<Option<Box<dyn crate::h3::transport::BidiStream>>, TransportError>>
        {
            Poll::Pending
        }

        fn poll_accept_uni(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<Option<Box<dyn UniStream>>, TransportError>> {
            Poll::Ready(Ok(self.peer_unis.pop_front()))
        }
    }

    impl Connection for MockConn {
        fn stream_id_stream(&self) -> u64 {
            3
        }

        fn is_handshake_complete(&self) -> bool {
            true
        }

        fn poll_shutdown(
            &mut self,
            _cx: &mut Context<'_>,
            error_code: u64,
        ) -> Poll<Result<(), TransportError>> {
            self.shutdown_code = Some(error_code);
            Poll::Ready(Ok(()))
        }
    }

    fn encode_frames(frames: &[Frame]) -> Bytes {
        let mut buf = BytesMut::new();
        for frame in frames {
            frame.encode(&mut buf);
        }
        buf.freeze()
    }

    fn settings_frame(settings: &FrameSettings) -> Frame {
        Frame::Settings(settings.clone())
    }

    /// Bytes for a peer control stream: the control stream type byte then
    /// the frames.
    fn control_wire(frames: &[Frame]) -> Bytes {
        let mut buf = BytesMut::from(&[STREAM_TYPE_CONTROL as u8][..]);
        for frame in frames {
            frame.encode(&mut buf);
        }
        buf.freeze()
    }

    /// Polls `poll_read` until Pending, collecting every event yielded.
    fn drain_events(
        plane: &mut ControlStreams,
        conn: &mut MockConn,
    ) -> Result<Vec<ControlEvent>, ControlError> {
        let mut cx = cx();
        let mut events = Vec::new();
        loop {
            // The `?` unwraps the Result nested in the Poll, leaving
            // Poll<Option<ControlEvent>>.
            match plane.poll_read(conn, &mut cx)? {
                Poll::Ready(Some(event)) => events.push(event),
                Poll::Ready(None) => panic!("poll_read yields events or Pending"),
                Poll::Pending => break,
            }
        }
        Ok(events)
    }

    fn init_with(plane: &mut ControlStreams, conn: &mut MockConn) {
        let mut cx = cx();
        assert!(plane.poll_init(conn, &mut cx).is_ready());
        assert!(plane.poll_flush(&mut cx).is_ready());
    }

    #[test]
    fn init_opens_three_streams_and_sends_settings_first() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        init_with(&mut plane, &mut conn);

        assert_eq!(conn.opened, 3);
        let log = conn.log.lock().unwrap();
        assert_eq!(log.len(), 1, "only SETTINGS is written at init");
        assert_eq!(
            log[0],
            encode_frames(&[settings_frame(&LocalSettings::default().to_frame())])
        );
    }

    #[test]
    fn peer_settings_then_goaway_events() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        let mut settings = FrameSettings::new();
        settings.insert(SETTINGS_QPACK_MAX_TABLE_CAPACITY, 4096);
        peer.feed(&control_wire(&[
            settings_frame(&settings),
            Frame::Goaway(7),
        ]));
        conn.peer_unis.push_back(Box::new(peer));

        let events = drain_events(&mut plane, &mut conn).expect("control stream is valid");
        assert_eq!(events.len(), 2);
        match &events[0] {
            ControlEvent::Settings(peer_settings) => {
                assert_eq!(peer_settings.qpack_max_table_capacity(), 4096);
            }
            other => panic!("expected Settings, got {other:?}"),
        }
        assert!(matches!(events[1], ControlEvent::Goaway { id: 7 }));
        assert!(plane.settings_received());
        // The encoder is created once the peer's SETTINGS bound its table.
        assert_eq!(
            plane
                .shared()
                .lock()
                .unwrap()
                .encoder
                .as_ref()
                .expect("encoder")
                .max_capacity(),
            4096
        );
    }

    #[test]
    fn unknown_frames_on_control_stream_are_skipped() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        let mut wire = BytesMut::from(&[STREAM_TYPE_CONTROL as u8][..]);
        settings_frame(&LocalSettings::default().to_frame()).encode(&mut wire);
        // Unknown frame type 0x42 with payload, between SETTINGS and GOAWAY.
        frame::write_varint(0x42, &mut wire);
        frame::write_varint(3, &mut wire);
        wire.extend_from_slice(b"xyz");
        Frame::Goaway(1).encode(&mut wire);
        peer.feed(&wire);
        conn.peer_unis.push_back(Box::new(peer));

        let events = drain_events(&mut plane, &mut conn).expect("unknown frames are skipped");
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1], ControlEvent::Goaway { id: 1 }));
    }

    #[test]
    fn second_settings_is_frame_unexpected() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        let settings = settings_frame(&LocalSettings::default().to_frame());
        peer.feed(&control_wire(&[settings.clone(), settings]));
        conn.peer_unis.push_back(Box::new(peer));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::FrameUnexpected));
        assert_eq!(err.h3_code(), 0x0105);
    }

    #[test]
    fn non_settings_first_frame_is_missing_settings() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&control_wire(&[Frame::Goaway(0)]));
        conn.peer_unis.push_back(Box::new(peer));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::MissingSettings));
        assert_eq!(err.h3_code(), 0x010a);
    }

    #[test]
    fn data_and_push_promise_on_control_stream_are_frame_unexpected() {
        for frame in [
            Frame::Data(Bytes::from_static(b"x")),
            Frame::PushPromise {
                push_id: 0,
                field_section: Bytes::new(),
            },
        ] {
            let mut plane = ControlStreams::new(LocalSettings::default());
            let mut conn = MockConn::new();
            let mut peer = MockStream::new(conn.log.clone());
            peer.feed(&control_wire(&[
                settings_frame(&LocalSettings::default().to_frame()),
                frame,
            ]));
            conn.peer_unis.push_back(Box::new(peer));
            let err = drain_events(&mut plane, &mut conn).unwrap_err();
            assert!(matches!(err, ControlError::FrameUnexpected));
        }
    }

    #[test]
    fn cancel_push_for_never_promised_push_is_id_error() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&control_wire(&[
            settings_frame(&LocalSettings::default().to_frame()),
            Frame::CancelPush(0),
        ]));
        conn.peer_unis.push_back(Box::new(peer));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::Id));
        assert_eq!(err.h3_code(), 0x0108);
    }

    #[test]
    fn max_push_id_must_not_decrease() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&control_wire(&[
            settings_frame(&LocalSettings::default().to_frame()),
            Frame::MaxPushId(5),
            Frame::MaxPushId(7),
        ]));
        conn.peer_unis.push_back(Box::new(peer));

        let events = drain_events(&mut plane, &mut conn).expect("growing MAX_PUSH_ID is fine");
        assert_eq!(events.len(), 3);
        assert!(matches!(events[1], ControlEvent::MaxPushId { id: 5 }));
        assert!(matches!(events[2], ControlEvent::MaxPushId { id: 7 }));
        assert_eq!(plane.max_push_id(), Some(7));

        // A reduction is H3_ID_ERROR.
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&control_wire(&[
            settings_frame(&LocalSettings::default().to_frame()),
            Frame::MaxPushId(5),
            Frame::MaxPushId(3),
        ]));
        conn.peer_unis.push_back(Box::new(peer));
        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::Id));
    }

    #[test]
    fn control_stream_fin_after_settings_is_closed_critical_stream() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        // The type byte arrives in its own chunk, as it does on a real
        // stream: classification consumes it, then the frame bytes come
        // through the wired control stream.
        peer.feed(&[STREAM_TYPE_CONTROL as u8]);
        peer.feed(&encode_frames(&[settings_frame(
            &LocalSettings::default().to_frame(),
        )]));
        peer.finish();
        conn.peer_unis.push_back(Box::new(peer));

        let mut cx = cx();
        let first = plane.poll_read(&mut conn, &mut cx);
        assert!(matches!(
            &first,
            Poll::Ready(Ok(Some(ControlEvent::Settings(_))))
        ));
        let then = plane.poll_read(&mut conn, &mut cx);
        assert!(matches!(
            then,
            Poll::Ready(Err(ControlError::ClosedCriticalStream))
        ));
    }

    #[test]
    fn control_stream_fin_without_settings_is_missing_settings() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&[STREAM_TYPE_CONTROL as u8]);
        peer.finish();
        conn.peer_unis.push_back(Box::new(peer));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::MissingSettings));
    }

    #[test]
    fn unknown_and_push_uni_stream_types_are_stream_creation_error() {
        for (bytes, label) in [
            (&[0x42u8, 0x01, 0x00][..], "unknown type"),
            (&[STREAM_TYPE_PUSH as u8, 0x01, 0x00][..], "push stream"),
        ] {
            let mut plane = ControlStreams::new(LocalSettings::default());
            let mut conn = MockConn::new();
            let mut peer = MockStream::new(conn.log.clone());
            peer.feed(bytes);
            conn.peer_unis.push_back(Box::new(peer));

            let err = drain_events(&mut plane, &mut conn).unwrap_err();
            assert!(
                matches!(err, ControlError::StreamCreation),
                "{label}: {err:?}"
            );
            assert_eq!(err.h3_code(), 0x0103);
        }
    }

    #[test]
    fn duplicate_control_stream_is_stream_creation_error() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let settings = settings_frame(&LocalSettings::default().to_frame());
        let mut peer1 = MockStream::new(conn.log.clone());
        peer1.feed(&control_wire(std::slice::from_ref(&settings)));
        let mut peer2 = MockStream::new(conn.log.clone());
        peer2.feed(&control_wire(&[settings]));
        conn.peer_unis.push_back(Box::new(peer1));
        conn.peer_unis.push_back(Box::new(peer2));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::StreamCreation));
    }

    #[test]
    fn multi_byte_uni_stream_type_is_classified_incrementally() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        let sink = peer.sink();
        peer.feed(&[0x7f]); // first byte of a 2-byte type (0x7f00 = 16128)
        conn.peer_unis.push_back(Box::new(peer));

        // Type varint incomplete: classification is still pending, and the
        // stream stays queued for the rest of its type byte.
        assert!(plane.poll_read(&mut conn, &mut cx()).is_pending());
        assert!(plane.pending_uni.is_some());

        // The second byte completes type 16128, which is unknown.
        sink.lock()
            .unwrap()
            .push_back(Some(Bytes::from_static(&[0x00])));
        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::StreamCreation));
        assert_eq!(err.h3_code(), 0x0103);
    }

    #[test]
    fn non_minimal_uni_stream_type_is_frame_error() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&[0x40, 0x00]); // 0 encoded in 2 bytes: redundant
        conn.peer_unis.push_back(Box::new(peer));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::Frame));
        assert_eq!(err.h3_code(), 0x0106);
    }

    #[test]
    fn peer_encoder_stream_feeds_the_qpack_decoder() {
        let mut plane = ControlStreams::new(LocalSettings {
            qpack_max_table_capacity: 4096,
            ..LocalSettings::default()
        });
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        // Type byte + a Set Capacity (0x3f 0x45 = 100) + Insert with
        // Literal Name "foo"="bar" in one chunk: leftover bytes after the
        // type must be fed to the decoder. Capacity 100 is big enough for
        // the 38-byte entry (RFC 9204 Section 3.2.1).
        peer.feed(&[
            STREAM_TYPE_QPACK_ENCODER as u8,
            0x3f,
            0x45,
            0x43,
            b'f',
            b'o',
            b'o',
            0x03,
            b'b',
            b'a',
            b'r',
        ]);
        conn.peer_unis.push_back(Box::new(peer));

        let events = drain_events(&mut plane, &mut conn).expect("encoder stream is valid");
        assert!(events.is_empty());
        assert_eq!(plane.shared().lock().unwrap().decoder.inserted(), 1);
        // Nothing was blocked, so nothing was unblocked, and a plain insert
        // needs no acknowledgement yet.
        assert!(plane.take_unblocked().is_empty());
    }

    #[test]
    fn malformed_encoder_stream_is_qpack_error() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        // Duplicate instruction (index 0) on an empty table.
        peer.feed(&[STREAM_TYPE_QPACK_ENCODER as u8, 0x00]);
        conn.peer_unis.push_back(Box::new(peer));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::Qpack(_)));
        assert_eq!(err.h3_code(), 0x0201);
    }

    #[test]
    fn peer_encoder_stream_fin_is_closed_critical_stream() {
        let mut plane = ControlStreams::new(LocalSettings {
            qpack_max_table_capacity: 4096,
            ..LocalSettings::default()
        });
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&[STREAM_TYPE_QPACK_ENCODER as u8, 0x2a]);
        peer.finish();
        conn.peer_unis.push_back(Box::new(peer));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        assert!(matches!(err, ControlError::ClosedCriticalStream));
    }

    #[test]
    fn peer_decoder_stream_is_drained_and_its_fin_is_critical() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&[STREAM_TYPE_QPACK_DECODER as u8, 0x00, 0x01]);
        conn.peer_unis.push_back(Box::new(peer));

        // Junk ack bytes are consumed without error.
        assert!(drain_events(&mut plane, &mut conn).is_ok());

        // Closing it is a connection error.
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&[STREAM_TYPE_QPACK_DECODER as u8]);
        peer.finish();
        conn.peer_unis.push_back(Box::new(peer));
        let mut plane = ControlStreams::new(LocalSettings::default());
        assert!(matches!(
            plane.poll_read(&mut conn, &mut cx()),
            Poll::Ready(Err(ControlError::ClosedCriticalStream))
        ));
    }

    #[test]
    fn peer_decoder_stream_zero_insert_count_increment_is_decoder_stream_error() {
        // h3spec QPACK 4.4.3: a single 0x00 byte on the peer's decoder
        // stream is an Insert Count Increment of 0 -> QPACK_DECODER_STREAM_ERROR.
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        let mut peer = MockStream::new(conn.log.clone());
        peer.feed(&[STREAM_TYPE_QPACK_DECODER as u8, 0x00]);
        conn.peer_unis.push_back(Box::new(peer));

        let err = drain_events(&mut plane, &mut conn).unwrap_err();
        match err {
            ControlError::Qpack(QpackError::DecoderStream) => {}
            other => panic!("expected DecoderStream, got {:?}", other),
        }
    }

    #[test]
    fn send_goaway_queues_the_frame_and_marks_shutdown() {
        let mut plane = ControlStreams::new(LocalSettings::default());
        let mut conn = MockConn::new();
        init_with(&mut plane, &mut conn);

        plane.send_goaway(7);
        assert!(plane.shutting_down());
        assert_eq!(plane.goaway_sent(), Some(7));
        let mut cx = cx();
        assert!(plane.poll_flush(&mut cx).is_ready());

        {
            let log = conn.log.lock().unwrap();
            assert_eq!(log.len(), 2);
            assert_eq!(log[1], encode_frames(&[Frame::Goaway(7)]));
        }
        assert_eq!(conn.shutdown_code, None, "closing is the driver's job");

        // Only one GOAWAY is ever sent.
        plane.send_goaway(9);
        assert_eq!(plane.goaway_sent(), Some(7));

        // Client-only frames queue for the control stream too; the flush
        // coalesces them into a single write.
        plane.send_max_push_id(2);
        plane.send_cancel_push(1);
        assert!(plane.poll_flush(&mut cx).is_ready());
        {
            let log = conn.log.lock().unwrap();
            assert_eq!(log.len(), 3);
            assert_eq!(
                log[2],
                encode_frames(&[Frame::MaxPushId(2), Frame::CancelPush(1)])
            );
        }
    }
}
