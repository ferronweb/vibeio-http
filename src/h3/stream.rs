//! HTTP/3 request stream handling (RFC 9114 Sections 4.1, 4.2 and 6.1).
//!
//! A request stream is a bidirectional QUIC stream that carries a single
//! HTTP message exchange: the client's request HEADERS, body DATA and
//! trailers, then the server's response HEADERS, DATA and trailers. This
//! module wraps such a stream in a [`RequestStream`] that owns the frame
//! decoder, decodes the requests' and response field sections with the
//! connection's shared QPACK codecs, and abides by the RFC 9114 frame
//! ordering rules:
//!
//! - the first frame must be HEADERS (anything else is
//!   `H3_FRAME_UNEXPECTED`); the field section must form a valid request
//!   with exactly the RFC 9114 Section 4.1 pseudo-headers, appearing
//!   before any regular field
//!   (`H3_MESSAGE_ERROR` otherwise);
//! - DATA frames follow; then at most one trailing HEADERS frame, after
//!   which only unknown frames may still appear
//!   (`H3_FRAME_UNEXPECTED` otherwise);
//! - control-plane frames (SETTINGS, GOAWAY, MAX_PUSH_ID, PUSH_PROMISE,
//!   CANCEL_PUSH) are `H3_FRAME_UNEXPECTED` on a request stream.
//!
//! The response side encodes the response field section with the shared
//! encoder (queuing its encoder-stream instructions for the control
//! plane) and respects the peer's `SETTINGS_MAX_FIELD_SECTION_SIZE`.
//!
//! All methods are polling-based: the connection driver runs the request
//! stream on its own task, and the transport's flow control shows up as
//! `Pending`.
#![allow(dead_code)] // consumed by the connection driver (step 15)

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::{Bytes, BytesMut};
use futures_util::ready;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, StatusCode, Uri, Version};

use crate::h3::error::{H3Error, TransportError};
use crate::h3::frame::{Frame, FrameDecoder, FrameError};
use crate::h3::qpack::{Decoder, Encoder, QpackError, UnblockedSection};
use crate::h3::settings::LocalSettings;
use crate::h3::transport::BidiStream;

/// A failure on a request stream.
///
/// Most variants are connection-scoped protocol errors: the driver closes
/// the connection with [`StreamError::h3_code`]. [`StreamError::Transport`]
/// with a `Reset`/`Stopped` error is stream-scoped: the driver abandons
/// the stream and the exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamError {
    /// The transport failed: the stream was reset by the peer
    /// (`TransportError::Reset`), it stopped our sending side (`Stopped`),
    /// or the connection closed underneath the stream.
    Transport(TransportError),
    /// A malformed frame or payload (`H3_FRAME_ERROR`).
    Frame,
    /// An invalid message: malformed, duplicated or missing pseudo-headers,
    /// a field section that violates RFC 9114 Section 4.1, or trailers
    /// with pseudo-headers (`H3_MESSAGE_ERROR`).
    Message,
    /// A frame that is not permitted on a request stream
    /// (`H3_FRAME_UNEXPECTED`).
    FrameUnexpected,
    /// The peer's QPACK encoder stream state made a field section
    /// undecodable (RFC 9204 Section 6).
    Qpack(QpackError),
    /// An encoded field section larger than the peer's announced
    /// `SETTINGS_MAX_FIELD_SECTION_SIZE`; the peer will likely refuse it
    /// (RFC 9114 Sections 4.2.2 and 7.2.4.1).
    HeadersTooBig { size: u64, limit: u64 },
}

impl StreamError {
    /// The connection error code to close with, when the error is
    /// connection-scoped.
    pub(crate) fn h3_code(&self) -> u64 {
        match self {
            StreamError::Transport(TransportError::Closed { code }) => *code,
            StreamError::Transport(_) => H3Error::Internal.code(),
            StreamError::Frame => H3Error::FrameError.code(),
            StreamError::Message => H3Error::Message.code(),
            StreamError::FrameUnexpected => H3Error::FrameUnexpected.code(),
            StreamError::Qpack(err) => u64::from(err.code()),
            StreamError::HeadersTooBig { .. } => H3Error::Message.code(),
        }
    }

    /// Whether the failure is stream-scoped (a reset or stop-sending from
    /// the peer), rather than a connection error.
    pub(crate) fn is_stream_scoped(&self) -> bool {
        matches!(
            self,
            StreamError::Transport(TransportError::Reset { .. } | TransportError::Stopped { .. })
        )
    }
}

impl From<TransportError> for StreamError {
    fn from(err: TransportError) -> Self {
        StreamError::Transport(err)
    }
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for StreamError {}

fn map_frame_error(err: FrameError) -> StreamError {
    match err {
        // A malformed frame payload anywhere is H3_FRAME_ERROR.
        FrameError::Frame => StreamError::Frame,
        // Any control-plane frame on a request stream is
        // H3_FRAME_UNEXPECTED (RFC 9114 Sections 7.2.3-7.2.7).
        FrameError::Unexpected(_) | FrameError::Settings => StreamError::FrameUnexpected,
    }
}

/// The connection's QPACK codecs plus the state they exchange with the
/// control plane and the request streams.
///
/// One instance per connection: the control plane ([`super::control`])
/// feeds the peer's encoder stream into the decoder and drains
/// [`SharedCodecs::encoder_stream`] onto the QPACK encoder stream; every
/// request stream decodes and encodes through the same instances, so
/// dynamic table state stays coherent across the connection.
#[derive(Debug)]
pub(crate) struct SharedCodecs {
    /// Decoder for the peer's field sections, sized by our own SETTINGS.
    pub(crate) decoder: Decoder,
    /// Encoder for our field sections, created once the peer's SETTINGS
    /// bound its dynamic table; `None` before that (RFC 9204 Section 5).
    pub(crate) encoder: Option<Encoder>,
    /// Encoder stream instructions (RFC 9204 Section 4.2) queued by the
    /// send path; the control plane writes them on the QPACK encoder
    /// stream.
    pub(crate) encoder_stream: VecDeque<Bytes>,
    /// Field sections the peer's encoder stream unblocked, in arrival
    /// order; the request streams waiting on them take the entries
    /// matching their stream ID.
    pub(crate) unblocked: Vec<UnblockedSection>,
    /// The peer's `SETTINGS_MAX_FIELD_SECTION_SIZE` once its SETTINGS
    /// frame arrived; `None` means unlimited.
    pub(crate) peer_max_field_section_size: Option<u64>,
    /// Wakers of request-stream tasks parked on shared codec state: a
    /// field section blocked on a dynamic-table entry, or the send side
    /// waiting for the peer's SETTINGS to create the encoder. Keyed by
    /// stream ID; the control plane wakes them when the state changes.
    pub(crate) waiters: HashMap<u64, Waker>,
}

impl SharedCodecs {
    /// Creates the codecs for a connection with the given local settings.
    pub(crate) fn new(local: &LocalSettings) -> Self {
        Self {
            decoder: Decoder::new(
                local.qpack_max_table_capacity,
                local.qpack_blocked_streams as usize,
            ),
            encoder: None,
            encoder_stream: VecDeque::new(),
            unblocked: Vec::new(),
            peer_max_field_section_size: None,
            waiters: HashMap::new(),
        }
    }

    /// Drains and returns every waiter parked on shared codec state, so
    /// the caller can wake them after releasing the lock.
    ///
    /// Waking a waiter whose section is still blocked is harmless: it
    /// re-polls, finds nothing, and re-registers.
    pub(crate) fn take_waiters(&mut self) -> Vec<Waker> {
        self.waiters.drain().map(|(_, waker)| waker).collect()
    }
}

/// One HTTP/3 request stream: the request/response exchange on a single
/// bidirectional QUIC stream.
///
/// Owned by the connection driver, which reads the request
/// ([`RequestStream::poll_headers`]), streams the body
/// ([`RequestStream::poll_recv_data`], [`RequestStream::poll_recv_trailers`])
/// and writes the response ([`RequestStream::poll_send_response`],
/// [`RequestStream::poll_send_data`], [`RequestStream::poll_send_trailers`],
/// [`RequestStream::poll_finish`]).
pub(crate) struct RequestStream {
    stream: Box<dyn BidiStream>,
    stream_id: u64,
    frame_decoder: FrameDecoder,
    shared: Arc<Mutex<SharedCodecs>>,

    // Receive state.
    awaiting_headers: bool,
    headers_blocked: bool,
    recv_data: VecDeque<Bytes>,
    trailers_lines: Option<Vec<(Bytes, Bytes)>>,
    trailers_blocked: bool,
    trailers_done: bool,
    recv_finished: bool,

    // Send state.
    sent_headers: bool,
    #[allow(dead_code)] // written by the driver (step 15)
    sent_trailers: bool,
    #[allow(dead_code)] // written by the driver (step 15)
    sent_fin: bool,
    send_buf: VecDeque<Bytes>,
}

impl RequestStream {
    /// Wraps an accepted request stream. `stream.id()` is its QUIC stream
    /// ID, used to correlate QPACK state (blocked field sections, section
    /// acknowledgements).
    pub(crate) fn new(stream: Box<dyn BidiStream>, shared: Arc<Mutex<SharedCodecs>>) -> Self {
        let stream_id = stream.id();
        Self {
            stream,
            stream_id,
            frame_decoder: FrameDecoder::new(),
            shared,
            awaiting_headers: true,
            headers_blocked: false,
            recv_data: VecDeque::new(),
            trailers_lines: None,
            trailers_blocked: false,
            trailers_done: false,
            recv_finished: false,
            sent_headers: false,
            sent_trailers: false,
            sent_fin: false,
            send_buf: VecDeque::new(),
        }
    }

    /// The QUIC stream ID of this request stream.
    #[allow(dead_code)] // consumed by the connection driver (step 15)
    pub(crate) fn id(&self) -> u64 {
        self.stream_id
    }

    /// Polls for the initial HEADERS frame and decodes it into the
    /// request. `Ok(None)` when the stream ended without one.
    ///
    /// While the field section is blocked on a QPACK dynamic table entry,
    /// polls `Pending`; it returns once the control plane's processing of
    /// the peer's encoder stream unblocks it.
    pub(crate) fn poll_headers(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Request<()>>, StreamError>> {
        if self.headers_blocked {
            let section = take_unblocked_for(&self.shared, self.stream_id, cx);
            if let Some(section) = section {
                self.headers_blocked = false;
                self.awaiting_headers = false;
                return Poll::Ready(build_request(section.headers).map(Some));
            }
            return Poll::Pending;
        }
        if !self.awaiting_headers {
            return Poll::Ready(Ok(None));
        }
        match ready!(self.poll_frame(cx)?) {
            None => {
                self.awaiting_headers = false;
                self.recv_finished = true;
                Poll::Ready(Ok(None))
            }
            Some(Frame::Headers(block)) => match self.decode_block(&block) {
                Ok(Some(headers)) => match build_request(headers) {
                    Ok(request) => {
                        self.awaiting_headers = false;
                        Poll::Ready(Ok(Some(request)))
                    }
                    Err(err) => Poll::Ready(Err(err)),
                },
                Ok(None) => {
                    self.headers_blocked = true;
                    Poll::Pending
                }
                Err(err) => Poll::Ready(Err(err)),
            },
            Some(_) => {
                // RFC 9114 Section 4.1: the first frame on a request
                // stream must be HEADERS.
                Poll::Ready(Err(StreamError::FrameUnexpected))
            }
        }
    }

    /// Polls for the next chunk of the request body.
    ///
    /// `Ok(None)` when the body is complete (the stream ended or the
    /// trailers HEADERS frame arrived); any buffered trailers are then
    /// available via [`RequestStream::poll_recv_trailers`]. A frame after
    /// the trailers is `H3_FRAME_UNEXPECTED`.
    pub(crate) fn poll_recv_data(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Bytes>, StreamError>> {
        if let Some(data) = self.recv_data.pop_front() {
            return Poll::Ready(Ok(Some(data)));
        }
        if self.recv_finished {
            return Poll::Ready(Ok(None));
        }
        if self.awaiting_headers || self.headers_blocked || self.trailers_blocked {
            // The request is not past its HEADERS yet; the driver must
            // poll headers first.
            return Poll::Pending;
        }
        if self.trailers_done {
            // After the trailers, only unknown frames may appear.
            match ready!(self.poll_after_trailers(cx)?) {
                Some(()) => {
                    self.recv_finished = true;
                    return Poll::Ready(Ok(None));
                }
                None => return Poll::Pending,
            }
        }
        match ready!(self.poll_frame(cx)?) {
            None => {
                self.recv_finished = true;
                Poll::Ready(Ok(None))
            }
            Some(Frame::Data(data)) => Poll::Ready(Ok(Some(data))),
            Some(Frame::Headers(block)) => {
                // Trailers: the body is over. The field section is
                // buffered and validated by poll_recv_trailers.
                match self.decode_block(&block) {
                    Ok(Some(headers)) => {
                        self.trailers_done = true;
                        self.trailers_lines = Some(headers);
                        Poll::Ready(Ok(None))
                    }
                    Ok(None) => {
                        self.trailers_blocked = true;
                        Poll::Ready(Ok(None))
                    }
                    Err(err) => Poll::Ready(Err(err)),
                }
            }
            Some(_) => Poll::Ready(Err(StreamError::FrameUnexpected)),
        }
    }

    /// Polls for the request's trailers, decoded into a `HeaderMap`.
    ///
    /// `Ok(None)` when there were none. Any known frame after the trailers
    /// is `H3_FRAME_UNEXPECTED` (RFC 9114 Section 4.1).
    pub(crate) fn poll_recv_trailers(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, StreamError>> {
        if let Some(lines) = self.trailers_lines.take() {
            return Poll::Ready(header_map(lines).map(Some));
        }
        if self.trailers_blocked {
            if let Some(section) = take_unblocked_for(&self.shared, self.stream_id, cx) {
                self.trailers_blocked = false;
                self.trailers_done = true;
                return Poll::Ready(header_map(section.headers).map(Some));
            }
            return Poll::Pending;
        }
        if self.recv_finished {
            return Poll::Ready(Ok(None));
        }
        if self.awaiting_headers || self.headers_blocked {
            return Poll::Pending;
        }
        if self.trailers_done {
            match ready!(self.poll_after_trailers(cx)?) {
                Some(()) => {
                    self.recv_finished = true;
                    return Poll::Ready(Ok(None));
                }
                None => return Poll::Pending,
            }
        }
        match ready!(self.poll_frame(cx)?) {
            None => {
                self.recv_finished = true;
                Poll::Ready(Ok(None))
            }
            Some(Frame::Headers(block)) => match self.decode_block(&block) {
                Ok(Some(headers)) => {
                    self.trailers_done = true;
                    Poll::Ready(header_map(headers).map(Some))
                }
                Ok(None) => {
                    self.trailers_blocked = true;
                    Poll::Pending
                }
                Err(err) => Poll::Ready(Err(err)),
            },
            Some(Frame::Data(_)) => {
                // Data after the body was reported over: the caller
                // polled trailers before the body was done, or the
                // peer violated the frame ordering.
                Poll::Ready(Err(StreamError::FrameUnexpected))
            }
            Some(_) => Poll::Ready(Err(StreamError::FrameUnexpected)),
        }
    }

    /// Writes the response: the HEADERS frame for `status` and `headers`.
    ///
    /// The field section is encoded with the shared encoder (queuing its
    /// encoder-stream instructions) and must fit the peer's
    /// `SETTINGS_MAX_FIELD_SECTION_SIZE`. Informational responses (1xx:
    /// 100 Continue, 103 Early Hints) may precede the final one; only one
    /// final response may be sent.
    pub(crate) fn poll_send_response(
        &mut self,
        cx: &mut Context<'_>,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Poll<Result<(), StreamError>> {
        if self.sent_headers && !status.is_informational() {
            // RFC 9114 Section 4.1: a second response on the same stream
            // is invalid; the server resets it.
            return Poll::Ready(Err(StreamError::Message));
        }
        let mut lines = Vec::with_capacity(headers.len() + 1);
        lines.push((
            Bytes::from_static(b":status"),
            Bytes::copy_from_slice(status.as_str().as_bytes()),
        ));
        for (name, value) in headers {
            if name.as_str().starts_with(':') {
                return Poll::Ready(Err(StreamError::Message));
            }
            lines.push((
                Bytes::copy_from_slice(name.as_str().as_bytes()),
                Bytes::copy_from_slice(value.as_bytes()),
            ));
        }
        let bytes = self.encode_headered_byte(&lines)?;
        if !status.is_informational() {
            self.sent_headers = true;
        }
        self.poll_write(cx, bytes)
    }

    /// Writes one DATA frame with `data`.
    pub(crate) fn poll_send_data(
        &mut self,
        cx: &mut Context<'_>,
        data: Bytes,
    ) -> Poll<Result<(), StreamError>> {
        let mut buf = BytesMut::new();
        Frame::Data(data).encode(&mut buf);
        self.poll_write(cx, buf.freeze())
    }

    /// Writes the trailers HEADERS frame. No DATA may follow.
    pub(crate) fn poll_send_trailers(
        &mut self,
        cx: &mut Context<'_>,
        trailers: &HeaderMap,
    ) -> Poll<Result<(), StreamError>> {
        if !self.sent_headers {
            return Poll::Ready(Err(StreamError::Message));
        }
        let mut lines = Vec::with_capacity(trailers.len());
        for (name, value) in trailers {
            if name.as_str().starts_with(':') {
                return Poll::Ready(Err(StreamError::Message));
            }
            lines.push((
                Bytes::copy_from_slice(name.as_str().as_bytes()),
                Bytes::copy_from_slice(value.as_bytes()),
            ));
        }
        let bytes = self.encode_headered_byte(&lines)?;
        self.sent_trailers = true;
        self.poll_write(cx, bytes)
    }

    /// Finishes the response (`FIN`), after all queued data was written.
    pub(crate) fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        ready!(self.poll_write(cx, Bytes::new())?);
        self.stream.poll_finish(cx).map_err(StreamError::Transport)
    }

    /// Resets the sending side of the stream with `code` (RFC 9114
    /// Section 4.1), discarding buffered data.
    pub(crate) fn poll_reset(
        &mut self,
        cx: &mut Context<'_>,
        code: u64,
    ) -> Poll<Result<(), StreamError>> {
        self.stream
            .poll_reset(cx, code)
            .map_err(StreamError::Transport)
    }

    /// Stops the peer from sending on the stream with `code`.
    pub(crate) fn poll_stop_sending(
        &mut self,
        cx: &mut Context<'_>,
        code: u64,
    ) -> Poll<Result<(), StreamError>> {
        self.stream
            .poll_stop_sending(cx, code)
            .map_err(StreamError::Transport)
    }

    /// Encodes a field section with the shared encoder, enforces the peer's
    /// field-section size limit, and frames it as HEADERS.
    fn encode_headered_byte(&mut self, lines: &[(Bytes, Bytes)]) -> Result<Bytes, StreamError> {
        {
            let shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if shared.encoder.is_none() {
                // The peer's SETTINGS (which bound its dynamic table) has
                // not arrived; its encoder is unusable (RFC 9204
                // Section 5).
                return Err(StreamError::Message);
            }
        }
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let encoder = shared.encoder.as_mut().expect("encoder ensured");
        let section = encoder.encode_section(lines);
        let size = section.block.len() as u64;
        if let Some(limit) = shared.peer_max_field_section_size {
            if size > limit {
                return Err(StreamError::HeadersTooBig { size, limit });
            }
        }
        shared.encoder_stream.push_back(section.encoder_stream);
        let mut buf = BytesMut::new();
        Frame::Headers(section.block).encode(&mut buf);
        Ok(buf.freeze())
    }

    /// Decodes one encoded field section with the shared decoder, marking
    /// it blocked when a dynamic table entry is missing.
    fn decode_block(&self, block: &[u8]) -> Result<Option<Vec<(Bytes, Bytes)>>, StreamError> {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .decoder
            .decode_block(block, self.stream_id, now())
            .map_err(StreamError::Qpack)
    }

    /// After the trailers, reads and discards unknown frames; a known
    /// frame is `H3_FRAME_UNEXPECTED` (RFC 9114 Section 4.1).
    ///
    /// `Ok(Some(()))` when the stream ended cleanly after the trailers.
    fn poll_after_trailers(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<()>, StreamError>> {
        loop {
            match ready!(self.poll_frame(cx)?) {
                None => return Poll::Ready(Ok(Some(()))),
                Some(frame) => {
                    if frame.is_known() {
                        return Poll::Ready(Err(StreamError::FrameUnexpected));
                    }
                    // Unknown frames are ignored.
                }
            }
        }
    }

    /// Reads frames until one is available (buffering chunks into the
    /// frame decoder, the way a real transport delivers them). A stream
    /// that ends mid-frame is `H3_FRAME_ERROR`.
    fn poll_frame(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Frame>, StreamError>> {
        loop {
            match self.frame_decoder.next_frame() {
                Ok(Some(frame)) => return Poll::Ready(Ok(Some(frame))),
                Ok(None) => {}
                Err(err) => return Poll::Ready(Err(map_frame_error(err))),
            }
            match ready!(self.stream.poll_recv(cx))? {
                Some(chunk) => self.frame_decoder.extend(chunk),
                None => {
                    if self.frame_decoder.buffered() != 0 {
                        return Poll::Ready(Err(StreamError::Frame));
                    }
                    self.recv_finished = true;
                    return Poll::Ready(Ok(None));
                }
            }
        }
    }

    /// Queues `bytes` behind any bytes already waiting on the transport
    /// and drains the queue.
    fn poll_write(&mut self, cx: &mut Context<'_>, bytes: Bytes) -> Poll<Result<(), StreamError>> {
        if !bytes.is_empty() {
            self.send_buf.push_back(bytes);
        }
        while let Some(bytes) = self.send_buf.front() {
            match self.stream.poll_send(cx, bytes) {
                Poll::Ready(Ok(())) => {
                    self.send_buf.pop_front();
                }
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err.into())),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }
}

/// Takes the oldest unblocked field section for `stream_id`.
fn take_unblocked_for(
    shared: &Arc<Mutex<SharedCodecs>>,
    stream_id: u64,
    cx: &mut Context<'_>,
) -> Option<UnblockedSection> {
    let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
    match shared
        .unblocked
        .iter()
        .position(|section| section.stream_id == stream_id)
    {
        Some(index) => Some(shared.unblocked.remove(index)),
        // No unblocked section yet: park this task until the control
        // plane feeds the peer's encoder stream and unblocks it.
        None => {
            shared.waiters.insert(stream_id, cx.waker().clone());
            None
        }
    }
}

/// The monotonic clock the QPACK decoder records blocked sections with.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Builds an `http::Request` from a decoded field section, enforcing the
/// RFC 9114 Section 4.1 pseudo-header rules:
///
/// - pseudo-headers must precede all regular fields;
/// - duplicate pseudo-headers and unknown ones are `H3_MESSAGE_ERROR`;
/// - `:method`, `:scheme` and `:path` are required (except CONNECT, which
///   requires only `:authority`, plus `:scheme` and `:protocol` for an
///   extended CONNECT; a plain CONNECT must not carry `:scheme`/`:path`);
/// - `:protocol` is only valid on CONNECT requests (RFC 9220).
fn build_request(headers: Vec<(Bytes, Bytes)>) -> Result<Request<()>, StreamError> {
    let mut method = None;
    let mut scheme = None;
    let mut authority = None;
    let mut path = None;
    let mut protocol = None;
    let mut regular = Vec::new();
    let mut pseudo_done = false;

    for (name, value) in headers {
        if name.first() == Some(&b':') {
            if pseudo_done {
                // RFC 9114 Section 4.1: pseudo-headers must precede
                // regular fields.
                return Err(StreamError::Message);
            }
            match name.as_ref() {
                b":method" => take_pseudo(&mut method, value)?,
                b":scheme" => take_pseudo(&mut scheme, value)?,
                b":authority" => take_pseudo(&mut authority, value)?,
                b":path" => take_pseudo(&mut path, value)?,
                b":protocol" => take_pseudo(&mut protocol, value)?,
                _ => return Err(StreamError::Message),
            }
        } else {
            pseudo_done = true;
            regular.push((
                HeaderName::from_bytes(&name).map_err(|_| StreamError::Message)?,
                HeaderValue::from_bytes(&value).map_err(|_| StreamError::Message)?,
            ));
        }
    }

    let method_str = String::from_utf8(method.ok_or(StreamError::Message)?.to_vec())
        .map_err(|_| StreamError::Message)?;
    let method = Method::from_bytes(method_str.as_bytes()).map_err(|_| StreamError::Message)?;
    let connect = method == Method::CONNECT;
    let extended = protocol.is_some();
    if extended && !connect {
        // RFC 9220 Section 3: `:protocol` only in CONNECT requests.
        return Err(StreamError::Message);
    }
    if connect {
        if extended {
            // RFC 9220 Section 3: an extended CONNECT carries `:scheme`,
            // `:authority`, `:method` and `:protocol`, never `:path`.
            if scheme.is_none() || authority.is_none() || path.is_some() {
                return Err(StreamError::Message);
            }
        } else {
            // RFC 9114 Section 4.1: a plain CONNECT carries only `:method`
            // and `:authority`; `:scheme` and `:path` must be omitted.
            if authority.is_none() || scheme.is_some() || path.is_some() {
                return Err(StreamError::Message);
            }
        }
    } else if scheme.is_none() || path.is_none() {
        return Err(StreamError::Message);
    }

    let uri = build_uri(connect, &scheme, &authority, &path)?;

    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .version(Version::HTTP_3)
        .body(())
        .expect("validated parts build a request");
    for (name, value) in regular {
        request.headers_mut().append(name, value);
    }
    Ok(request)
}

fn take_pseudo(slot: &mut Option<Bytes>, value: Bytes) -> Result<(), StreamError> {
    if slot.is_some() {
        // RFC 9114 Section 4.1: duplicate pseudo-headers are invalid.
        return Err(StreamError::Message);
    }
    *slot = Some(value);
    Ok(())
}

fn build_uri(
    connect: bool,
    scheme: &Option<Bytes>,
    authority: &Option<Bytes>,
    path: &Option<Bytes>,
) -> Result<Uri, StreamError> {
    if connect {
        // RFC 9220: the target is (scheme, authority). The request URI
        // conventionally carries the authority; step 15 passes the tunnel
        // target on.
        let authority = authority.as_ref().expect("CONNECT authority validated");
        let scheme = scheme
            .as_ref()
            .map(|s| String::from_utf8_lossy(s))
            .unwrap_or_else(|| "http".into());
        return Uri::builder()
            .scheme(scheme.as_ref())
            .authority(String::from_utf8_lossy(authority).into_owned())
            .path_and_query("")
            .build()
            .map_err(|_| StreamError::Message);
    }
    let scheme = scheme.as_ref().expect("scheme validated");
    let path = path.as_ref().expect("path validated");
    let text = match authority {
        Some(authority) => format!(
            "{}://{}{}",
            String::from_utf8_lossy(scheme),
            String::from_utf8_lossy(authority),
            String::from_utf8_lossy(path)
        ),
        None => String::from_utf8_lossy(path).into_owned(),
    };
    Uri::try_from(text).map_err(|_| StreamError::Message)
}

/// Converts a decoded header list into a `HeaderMap`; pseudo-headers are
/// invalid in a trailer section (RFC 9114 Section 4.1).
fn header_map(headers: Vec<(Bytes, Bytes)>) -> Result<HeaderMap, StreamError> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if name.first() == Some(&b':') {
            return Err(StreamError::Message);
        }
        map.append(
            HeaderName::from_bytes(&name).map_err(|_| StreamError::Message)?,
            HeaderValue::from_bytes(&value).map_err(|_| StreamError::Message)?,
        );
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h3::frame;
    use crate::h3::frame::{FrameDecoder, Settings as FrameSettings};
    use futures_util::task::noop_waker_ref;
    use http::header::CONTENT_TYPE;

    fn cx() -> Context<'static> {
        Context::from_waker(noop_waker_ref())
    }

    /// In-memory bidirectional stream for a request exchange. Outbound
    /// bytes go into a shared sink so a test can inspect the wire after
    /// moving the stream into a `Box<dyn BidiStream>`.
    struct MockBidi {
        inbound: VecDeque<Option<Bytes>>,
        outbound: std::sync::Arc<std::sync::Mutex<VecDeque<Bytes>>>,
        id: u64,
        reset_code: Option<u64>,
        stop_code: Option<u64>,
        finished: bool,
    }

    impl MockBidi {
        fn new(id: u64) -> Self {
            Self::with_sink(
                id,
                std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new())),
            )
        }

        fn with_sink(id: u64, outbound: std::sync::Arc<std::sync::Mutex<VecDeque<Bytes>>>) -> Self {
            Self {
                inbound: VecDeque::new(),
                outbound,
                id,
                reset_code: None,
                stop_code: None,
                finished: false,
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.inbound.push_back(Some(Bytes::copy_from_slice(bytes)));
        }

        fn finish(&mut self) {
            self.inbound.push_back(None);
        }
    }

    impl crate::h3::transport::RecvStream for MockBidi {
        fn poll_recv(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<Option<Bytes>, TransportError>> {
            match self.inbound.pop_front() {
                Some(chunk) => Poll::Ready(Ok(chunk)),
                None => Poll::Pending,
            }
        }

        fn id(&self) -> u64 {
            self.id
        }
    }

    impl crate::h3::transport::SendStream for MockBidi {
        fn poll_send(
            &mut self,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<Result<(), TransportError>> {
            self.outbound
                .lock()
                .unwrap()
                .push_back(Bytes::copy_from_slice(data));
            Poll::Ready(Ok(()))
        }

        fn poll_finish(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
            self.finished = true;
            Poll::Ready(Ok(()))
        }

        fn poll_reset(
            &mut self,
            _cx: &mut Context<'_>,
            code: u64,
        ) -> Poll<Result<(), TransportError>> {
            self.reset_code = Some(code);
            Poll::Ready(Ok(()))
        }

        fn poll_stop_sending(
            &mut self,
            _cx: &mut Context<'_>,
            code: u64,
        ) -> Poll<Result<(), TransportError>> {
            self.stop_code = Some(code);
            Poll::Ready(Ok(()))
        }
    }

    impl crate::h3::transport::BidiStream for MockBidi {}

    fn local_settings() -> LocalSettings {
        LocalSettings {
            qpack_max_table_capacity: 4096,
            qpack_blocked_streams: 16,
            ..LocalSettings::default()
        }
    }

    /// A shared codec pair where the encoder is already usable (the peer's
    /// SETTINGS arrived).
    fn shared_with_encoder() -> Arc<Mutex<SharedCodecs>> {
        let mut shared = SharedCodecs::new(&local_settings());
        shared.encoder = Some(Encoder::new(4096, true));
        Arc::new(Mutex::new(shared))
    }

    /// Returns the encoder for hand-encoding wire blocks, plus the shared
    /// handle.
    fn shared_and_peer_encoder() -> (Arc<Mutex<SharedCodecs>>, Encoder) {
        let mut shared = SharedCodecs::new(&local_settings());
        shared.encoder = Some(Encoder::new(4096, true));
        let peer_encoder = Encoder::new(4096, true);
        (Arc::new(Mutex::new(shared)), peer_encoder)
    }

    fn request_lines(method: &str, path: &str) -> Vec<(Bytes, Bytes)> {
        vec![
            (
                Bytes::from_static(b":method"),
                Bytes::copy_from_slice(method.as_bytes()),
            ),
            (Bytes::from_static(b":scheme"), Bytes::from_static(b"https")),
            (
                Bytes::from_static(b":authority"),
                Bytes::from_static(b"example.com"),
            ),
            (
                Bytes::from_static(b":path"),
                Bytes::copy_from_slice(path.as_bytes()),
            ),
        ]
    }

    #[test]
    fn full_request_exchange() {
        let (shared, mut enc) = shared_and_peer_encoder();
        let mut wire = BytesMut::new();
        let section = enc.encode_section(&request_lines("POST", "/submit"));
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        Frame::Data(Bytes::from_static(b"hello")).encode(&mut wire);
        let body2 = Bytes::from_static(b"world");
        Frame::Data(body2).encode(&mut wire);
        let section = enc.encode_section(&[(
            Bytes::from_static(b"x-checksum"),
            Bytes::from_static(b"sum"),
        )]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);

        let mut stream = MockBidi::new(11);
        stream.feed(&wire);
        stream.finish();
        let mut request = RequestStream::new(Box::new(stream), shared.clone());

        let mut cx = cx();
        let req = match request.poll_headers(&mut cx) {
            Poll::Ready(Ok(Some(request))) => request,
            other => panic!("expected request, got {other:?}"),
        };
        assert_eq!(req.method(), Method::POST);
        assert_eq!(req.uri().path(), "/submit");
        assert_eq!(req.uri().scheme_str(), Some("https"));
        assert_eq!(
            req.uri().authority().map(|a| a.as_str()),
            Some("example.com")
        );
        assert_eq!(req.version(), Version::HTTP_3);

        assert_eq!(
            request.poll_recv_data(&mut cx),
            Poll::Ready(Ok(Some(Bytes::from_static(b"hello"))))
        );
        assert_eq!(
            request.poll_recv_data(&mut cx),
            Poll::Ready(Ok(Some(Bytes::from_static(b"world"))))
        );
        assert_eq!(request.poll_recv_data(&mut cx), Poll::Ready(Ok(None)));

        let trailers = match request.poll_recv_trailers(&mut cx) {
            Poll::Ready(Ok(Some(trailers))) => trailers,
            other => panic!("expected trailers, got {other:?}"),
        };
        assert_eq!(
            trailers.get("x-checksum"),
            Some(&HeaderValue::from_static("sum"))
        );
    }

    #[test]
    fn request_without_body_is_finished_by_fin() {
        let (shared, mut enc) = shared_and_peer_encoder();
        let mut wire = BytesMut::new();
        let section = enc.encode_section(&request_lines("GET", "/index"));
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);

        let mut stream = MockBidi::new(13);
        stream.feed(&wire);
        stream.finish();
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        let req = match request.poll_headers(&mut cx) {
            Poll::Ready(Ok(Some(request))) => request,
            other => panic!("expected request, got {other:?}"),
        };
        assert_eq!(req.method(), Method::GET);
        assert_eq!(request.poll_recv_data(&mut cx), Poll::Ready(Ok(None)));
    }

    #[test]
    fn data_before_headers_is_frame_unexpected() {
        let shared = shared_with_encoder();
        let mut wire = BytesMut::new();
        Frame::Data(Bytes::from_static(b"x")).encode(&mut wire);
        let mut stream = MockBidi::new(15);
        stream.feed(&wire);
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        let result = request.poll_headers(&mut cx);
        assert!(matches!(
            result,
            Poll::Ready(Err(StreamError::FrameUnexpected))
        ));
        // The error is not cached as a completion: a re-poll waits for more
        // input.
        assert!(request.poll_headers(&mut cx).is_pending());
    }

    #[test]
    fn control_frames_on_request_stream_are_frame_unexpected() {
        for frame in [
            Frame::Settings(FrameSettings::new()),
            Frame::Goaway(0),
            Frame::MaxPushId(3),
            Frame::CancelPush(0),
        ] {
            let shared = shared_with_encoder();
            let mut wire = BytesMut::new();
            frame.encode(&mut wire);
            let mut stream = MockBidi::new(17);
            stream.feed(&wire);
            let mut request = RequestStream::new(Box::new(stream), shared.clone());
            let mut cx = cx();
            let result = request.poll_headers(&mut cx);
            assert!(
                matches!(result, Poll::Ready(Err(StreamError::FrameUnexpected))),
                "{frame:?}: {result:?}"
            );
        }
    }

    #[test]
    fn truncated_frame_is_frame_error() {
        let shared = shared_with_encoder();
        let mut stream = MockBidi::new(19);
        // HEADERS frame with length 5 but no payload.
        stream.feed(&[0x01, 0x05]);
        stream.finish();
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        let result = request.poll_headers(&mut cx);
        assert!(matches!(result, Poll::Ready(Err(StreamError::Frame))));
        // The truncated frame is still buffered: a re-poll waits rather
        // than fabricating a request.
        assert!(request.poll_headers(&mut cx).is_pending());
    }

    #[test]
    fn empty_body_with_trailers() {
        let (shared, mut enc) = shared_and_peer_encoder();
        let mut wire = BytesMut::new();
        let section = enc.encode_section(&request_lines("PUT", "/x"));
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        let section = enc.encode_section(&[(Bytes::from_static(b"x-a"), Bytes::from_static(b"1"))]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(21);
        stream.feed(&wire);
        stream.finish();
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        assert!(request.poll_headers(&mut cx).is_ready());
        assert_eq!(request.poll_recv_data(&mut cx), Poll::Ready(Ok(None)));
        let trailers = match request.poll_recv_trailers(&mut cx) {
            Poll::Ready(Ok(Some(trailers))) => trailers,
            other => panic!("expected trailers, got {other:?}"),
        };
        assert_eq!(trailers.get("x-a"), Some(&HeaderValue::from_static("1")));
    }

    #[test]
    fn trailers_with_pseudo_headers_are_message_error() {
        let (shared, mut enc) = shared_and_peer_encoder();
        let mut wire = BytesMut::new();
        let section = enc.encode_section(&request_lines("GET", "/x"));
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        let section =
            enc.encode_section(&[(Bytes::from_static(b":status"), Bytes::from_static(b"200"))]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(23);
        stream.feed(&wire);
        stream.finish();
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        assert!(request.poll_headers(&mut cx).is_ready());
        assert_eq!(request.poll_recv_data(&mut cx), Poll::Ready(Ok(None)));
        let result = request.poll_recv_trailers(&mut cx);
        assert!(matches!(result, Poll::Ready(Err(StreamError::Message))));
    }

    #[test]
    fn known_frame_after_trailers_is_frame_unexpected() {
        let (shared, mut enc) = shared_and_peer_encoder();
        let mut wire = BytesMut::new();
        let section = enc.encode_section(&request_lines("GET", "/x"));
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        let section = enc.encode_section(&[]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        // A DATA frame after the trailers is invalid.
        Frame::Data(Bytes::from_static(b"late")).encode(&mut wire);
        let mut stream = MockBidi::new(25);
        stream.feed(&wire);
        stream.finish();
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        assert!(request.poll_headers(&mut cx).is_ready());
        assert_eq!(request.poll_recv_data(&mut cx), Poll::Ready(Ok(None)));
        let _ = request.poll_recv_trailers(&mut cx);
        let result = request.poll_recv_data(&mut cx);
        assert!(matches!(
            result,
            Poll::Ready(Err(StreamError::FrameUnexpected))
        ));
    }

    #[test]
    fn unknown_frames_after_trailers_are_ignored() {
        let (shared, mut enc) = shared_and_peer_encoder();
        let mut wire = BytesMut::new();
        let section = enc.encode_section(&request_lines("GET", "/x"));
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        let section = enc.encode_section(&[]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        Frame::Headers(section.block).encode(&mut wire);
        // An unknown frame type 0x42 (grease-shaped but fine) after the
        // trailers is skipped.
        frame::write_varint(0x42, &mut wire);
        frame::write_varint(1, &mut wire);
        wire.extend_from_slice(b"z");
        let mut stream = MockBidi::new(27);
        stream.feed(&wire);
        stream.finish();
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        assert!(request.poll_headers(&mut cx).is_ready());
        assert_eq!(request.poll_recv_data(&mut cx), Poll::Ready(Ok(None)));
        let _ = request.poll_recv_trailers(&mut cx);
        assert_eq!(request.poll_recv_data(&mut cx), Poll::Ready(Ok(None)));
    }

    #[test]
    fn headers_blocked_then_unblocked_by_encoder_stream() {
        let shared = shared_with_encoder();
        // A request whose field section references a dynamic table entry:
        // `x-warmup` is absent from the static table, so the peer's
        // encoder inserts it and the section carries a non-zero Required
        // Insert Count.
        let mut lines = request_lines("GET", "/blocked");
        lines.push((
            Bytes::from_static(b"x-warmup"),
            Bytes::from_static(b"wednesday"),
        ));
        let mut peer_enc = Encoder::new(64, true);
        let section = peer_enc.encode_section(&lines);
        assert!(!section.encoder_stream.is_empty());

        let mut wire = BytesMut::new();
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(33);
        stream.feed(&wire);
        stream.finish();
        let mut request = RequestStream::new(Box::new(stream), shared.clone());

        let mut cx = cx();
        // The encoder stream instructions have not arrived yet: the
        // section is blocked.
        let result = request.poll_headers(&mut cx);
        assert!(result.is_pending(), "expected blocked, got {result:?}");

        // The control plane feeds the peer's encoder stream (as it would
        // on the QPACK encoder stream).
        let unblocked = {
            let mut shared = shared.lock().unwrap();
            shared
                .decoder
                .feed_encoder_stream(&section.encoder_stream)
                .expect("valid encoder stream")
        };
        assert_eq!(unblocked.len(), 1);
        assert_eq!(unblocked[0].stream_id, 33);
        shared.lock().unwrap().unblocked.extend(unblocked);

        let req = match request.poll_headers(&mut cx) {
            Poll::Ready(Ok(Some(request))) => request,
            other => panic!("expected unblocked request, got {other:?}"),
        };
        assert_eq!(req.uri().path(), "/blocked");
        assert_eq!(
            req.headers().get("x-warmup"),
            Some(&HeaderValue::from_static("wednesday"))
        );
        // The body was buffered behind the blocked section and is still
        // read in order.
        assert_eq!(request.poll_recv_data(&mut cx), Poll::Ready(Ok(None)));
    }

    #[test]
    fn send_response_encodes_headers_and_queue_encoder_stream() {
        let shared = shared_with_encoder();
        let sink = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let mut request = RequestStream::new(
            Box::new(MockBidi::with_sink(41, sink.clone())),
            shared.clone(),
        );

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        headers.insert("x-server", HeaderValue::from_static("vibeio"));
        let mut cx = cx();
        assert!(matches!(
            request.poll_send_response(&mut cx, StatusCode::OK, &headers),
            Poll::Ready(Ok(()))
        ));

        // The encoder produced encoder-stream instructions for the control
        // plane.
        assert!(!shared.lock().unwrap().encoder_stream.is_empty());

        // The wire carries one HEADERS frame with a QPACK-encoded field
        // section (never empty: it encodes `:status` and the headers).
        let outbound = sink
            .lock()
            .unwrap()
            .pop_front()
            .expect("response wrote bytes");
        let mut decoder = FrameDecoder::new();
        decoder.extend(outbound);
        match decoder.next_frame().expect("valid frame").expect("a frame") {
            Frame::Headers(block) => assert!(!block.is_empty()),
            other => panic!("expected HEADERS frame, got {other:?}"),
        }
        assert!(decoder.next_frame().expect("valid frame").is_none());
    }

    #[test]
    fn send_data_writes_data_frame() {
        let shared = shared_with_encoder();
        let stream = MockBidi::new(43);
        let mut request = RequestStream::new(Box::new(stream), shared);
        let mut cx = cx();
        let result = request.poll_send_data(&mut cx, Bytes::from_static(b"abc"));
        assert!(result.is_ready());
        assert!(matches!(result, Poll::Ready(Ok(()))));
    }

    #[test]
    fn send_trailers_after_response() {
        let shared = shared_with_encoder();
        let stream = MockBidi::new(45);
        let mut request = RequestStream::new(Box::new(stream), shared.clone());
        let mut cx = cx();

        let mut headers = HeaderMap::new();
        headers.insert("x-a", HeaderValue::from_static("1"));
        assert!(matches!(
            request.poll_send_response(&mut cx, StatusCode::NO_CONTENT, &headers),
            Poll::Ready(Ok(()))
        ));

        let mut trailers = HeaderMap::new();
        trailers.insert("x-sum", HeaderValue::from_static("7"));
        assert!(matches!(
            request.poll_send_trailers(&mut cx, &trailers),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(request.poll_finish(&mut cx), Poll::Ready(Ok(()))));
    }

    #[test]
    fn response_before_peer_settings_is_message_error() {
        let shared = Arc::new(Mutex::new(SharedCodecs::new(&local_settings())));
        let stream = MockBidi::new(47);
        let mut request = RequestStream::new(Box::new(stream), shared);
        let mut cx = cx();
        let headers = HeaderMap::new();
        let result = request.poll_send_response(&mut cx, StatusCode::OK, &headers);
        assert!(matches!(result, Poll::Ready(Err(StreamError::Message))));
    }

    #[test]
    fn response_over_peer_field_section_limit() {
        let mut shared = SharedCodecs::new(&local_settings());
        // Capacity 0 disables the dynamic table (RFC 9204 Section 3.2.3),
        // so the field section is encoded inline as literal lines instead
        // of shrinking to a dynamic-table reference.
        shared.encoder = Some(Encoder::new(0, true));
        shared.peer_max_field_section_size = Some(10);
        let shared = Arc::new(Mutex::new(shared));
        let stream = MockBidi::new(49);
        let mut request = RequestStream::new(Box::new(stream), shared);
        let mut cx = cx();

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-long-header",
            HeaderValue::from_static("something bigger than ten bytes"),
        );
        let result = request.poll_send_response(&mut cx, StatusCode::OK, &headers);
        assert!(matches!(
            result,
            Poll::Ready(Err(StreamError::HeadersTooBig { .. }))
        ));
    }

    #[test]
    fn duplicate_response_is_message_error() {
        let shared = shared_with_encoder();
        let stream = MockBidi::new(51);
        let mut request = RequestStream::new(Box::new(stream), shared);
        let mut cx = cx();
        let headers = HeaderMap::new();
        assert!(matches!(
            request.poll_send_response(&mut cx, StatusCode::OK, &headers),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            request.poll_send_response(&mut cx, StatusCode::OK, &headers),
            Poll::Ready(Err(StreamError::Message))
        ));
    }

    #[test]
    fn missing_method_is_message_error() {
        let shared = shared_with_encoder();
        let mut peer_enc = Encoder::new(4096, true);
        let section = peer_enc.encode_section(&[
            (Bytes::from_static(b":scheme"), Bytes::from_static(b"https")),
            (Bytes::from_static(b":authority"), Bytes::from_static(b"x")),
            (Bytes::from_static(b":path"), Bytes::from_static(b"/")),
        ]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        let mut wire = BytesMut::new();
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(53);
        stream.feed(&wire);
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        let result = request.poll_headers(&mut cx);
        match result {
            Poll::Ready(Err(err)) => assert_eq!(err.h3_code(), H3Error::Message.code()),
            other => panic!("expected message error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_pseudo_header_is_message_error() {
        let shared = shared_with_encoder();
        let mut peer_enc = Encoder::new(4096, true);
        let section = peer_enc.encode_section(&[
            (Bytes::from_static(b":method"), Bytes::from_static(b"GET")),
            (Bytes::from_static(b":scheme"), Bytes::from_static(b"https")),
            (Bytes::from_static(b":authority"), Bytes::from_static(b"x")),
            (Bytes::from_static(b":path"), Bytes::from_static(b"/")),
            (Bytes::from_static(b":frobnicate"), Bytes::from_static(b"1")),
        ]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        let mut wire = BytesMut::new();
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(55);
        stream.feed(&wire);
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        let result = request.poll_headers(&mut cx);
        assert!(matches!(result, Poll::Ready(Err(StreamError::Message))));
    }

    #[test]
    fn pseudo_header_after_regular_is_message_error() {
        let shared = shared_with_encoder();
        let mut peer_enc = Encoder::new(4096, true);
        let section = peer_enc.encode_section(&[
            (Bytes::from_static(b":method"), Bytes::from_static(b"GET")),
            (Bytes::from_static(b":scheme"), Bytes::from_static(b"https")),
            (Bytes::from_static(b":authority"), Bytes::from_static(b"x")),
            (Bytes::from_static(b":path"), Bytes::from_static(b"/")),
            (Bytes::from_static(b"host"), Bytes::from_static(b"x")),
            (
                Bytes::from_static(b":trailer-late"),
                Bytes::from_static(b"1"),
            ),
        ]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        let mut wire = BytesMut::new();
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(57);
        stream.feed(&wire);
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        let result = request.poll_headers(&mut cx);
        assert!(matches!(result, Poll::Ready(Err(StreamError::Message))));
    }

    #[test]
    fn connect_request_shape() {
        // Plain CONNECT: only :method and :authority.
        let shared = shared_with_encoder();
        let mut peer_enc = Encoder::new(4096, true);
        let section = peer_enc.encode_section(&[
            (
                Bytes::from_static(b":method"),
                Bytes::from_static(b"CONNECT"),
            ),
            (
                Bytes::from_static(b":authority"),
                Bytes::from_static(b"example.com:443"),
            ),
        ]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        let mut wire = BytesMut::new();
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(59);
        stream.feed(&wire);
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx1 = cx();
        let req = match request.poll_headers(&mut cx1) {
            Poll::Ready(Ok(Some(request))) => request,
            other => panic!("expected CONNECT request, got {other:?}"),
        };
        assert_eq!(req.method(), Method::CONNECT);
        assert_eq!(
            req.uri().authority().map(|a| a.as_str()),
            Some("example.com:443")
        );

        // Extended CONNECT: adds :scheme and :protocol, never :path.
        let shared = shared_with_encoder();
        let mut peer_enc = Encoder::new(4096, true);
        let section = peer_enc.encode_section(&[
            (
                Bytes::from_static(b":method"),
                Bytes::from_static(b"CONNECT"),
            ),
            (Bytes::from_static(b":scheme"), Bytes::from_static(b"https")),
            (
                Bytes::from_static(b":authority"),
                Bytes::from_static(b"example.com"),
            ),
            (
                Bytes::from_static(b":protocol"),
                Bytes::from_static(b"webtransport"),
            ),
        ]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        let mut wire = BytesMut::new();
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(61);
        stream.feed(&wire);
        let mut request = RequestStream::new(Box::new(stream), shared.clone());
        let mut cx2 = cx();
        let req2 = match request.poll_headers(&mut cx2) {
            Poll::Ready(Ok(Some(request))) => request,
            other => panic!("expected extended CONNECT, got {other:?}"),
        };
        assert_eq!(req2.method(), Method::CONNECT);
        assert_eq!(req2.headers().get(":protocol"), None);
    }

    #[test]
    fn connect_with_path_is_message_error() {
        let shared = shared_with_encoder();
        let mut peer_enc = Encoder::new(4096, true);
        let section = peer_enc.encode_section(&[
            (
                Bytes::from_static(b":method"),
                Bytes::from_static(b"CONNECT"),
            ),
            (Bytes::from_static(b":scheme"), Bytes::from_static(b"https")),
            (Bytes::from_static(b":authority"), Bytes::from_static(b"x")),
            (Bytes::from_static(b":path"), Bytes::from_static(b"/")),
        ]);
        shared
            .lock()
            .unwrap()
            .decoder
            .feed_encoder_stream(&section.encoder_stream)
            .expect("valid");
        let mut wire = BytesMut::new();
        Frame::Headers(section.block).encode(&mut wire);
        let mut stream = MockBidi::new(63);
        stream.feed(&wire);
        let mut request = RequestStream::new(Box::new(stream), shared);

        let mut cx = cx();
        let result = request.poll_headers(&mut cx);
        assert!(matches!(result, Poll::Ready(Err(StreamError::Message))));
    }

    #[test]
    fn reset_by_peer_is_stream_scoped() {
        // A reset surfaces through the transport; the stream scope is
        // detected by the driver.
        let err = StreamError::Transport(TransportError::Reset { code: 0x010c });
        assert!(err.is_stream_scoped());
        let err = StreamError::Transport(TransportError::Stopped { code: 0x010c });
        assert!(err.is_stream_scoped());
        assert!(!StreamError::Frame.is_stream_scoped());
    }

    #[test]
    fn connection_codes_map_per_rfc() {
        assert_eq!(StreamError::Frame.h3_code(), 0x0106);
        assert_eq!(StreamError::Message.h3_code(), 0x010e);
        assert_eq!(StreamError::FrameUnexpected.h3_code(), 0x0105);
        assert_eq!(
            StreamError::Qpack(QpackError::DecompressionFailed).h3_code(),
            0x0200
        );
        assert_eq!(
            StreamError::HeadersTooBig { size: 5, limit: 3 }.h3_code(),
            0x010e
        );
    }
}
