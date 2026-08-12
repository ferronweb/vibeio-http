//! HTTP/2 stream layer (RFC 9113 Sections 5 and 8): one task per
//! stream, message-passing with the connection task over bounded
//! channels.
//!
//! Division of labour:
//!
//! - The connection task ([`super::connection::Connection`]) owns the
//!   socket, the HPACK encoder/decoder and the frame writer. It
//!   validates HEADERS field blocks, runs the stream-state machine
//!   (idle/open/half-closed/closed), and turns messages from the
//!   stream tasks into frames.
//! - Each stream task runs the user's `request_fn` and pipes the
//!   response body back over a [`mpsc`] channel ([`StreamMsg`]). It
//!   never touches the wire or the HPACK tables.
//! - The request body handed to the user is [`H2Body`], fed by the
//!   connection task over a second bounded channel ([`BodyMsg`]); peer
//!   RST_STREAM frames arrive on a separate channel the task polls
//!   beside the response future.
//!
//! Flow control lives on the connection side (C2): this module queues
//! DATA via `StreamEntry::pending_data` and the connection drains it
//! under the flow-control windows.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_util::FutureExt;
use http::{HeaderMap, Method, Response, StatusCode, Uri};
use http_body::{Body, Frame};
use pin_project_lite::pin_project;

use super::hpack::Header;
use crate::early_hints::EarlyHintsReceiver;

/// Messages the connection task sends to a stream task: request body
/// frames and terminal signals.
#[derive(Debug)]
pub(crate) enum BodyMsg {
    Data(Bytes),
    Trailers(HeaderMap),
    EndStream,
}

/// Messages a stream task sends to the connection task. The connection
/// turns them into frames; order is preserved (FIFO), so the response
/// header always precedes its body.
#[derive(Debug)]
pub(crate) enum StreamMsg {
    /// Final response headers. `end_stream` sets END_STREAM on the
    /// HEADERS frame (only used when the response has no body).
    Headers {
        parts: http::response::Parts,
        end_stream: bool,
    },
    /// Interim response (100 Continue, 103 Early Hints); never carries
    /// END_STREAM.
    Informational {
        parts: http::response::Parts,
    },
    Data {
        data: Bytes,
        end_stream: bool,
    },
    Trailers {
        trailers: HeaderMap,
    },
    /// Stream error initiated by the stream task (e.g. a body error).
    Reset {
        error_code: u32,
    },
    /// The stream task has finished; the connection may drop its state.
    Closed,
}

/// Per-stream state kept by the connection task (RFC 9113 Section 5.1).
pub(crate) struct StreamEntry {
    /// Request body delivery (receiver lives in the task's [`H2Body`]).
    pub(crate) body_tx: kanal::AsyncSender<BodyMsg>,
    /// Peer RST_STREAM notifications (receiver lives in the task).
    pub(crate) reset_tx: kanal::AsyncSender<u32>,
    /// Outbound response messages (sender lives in the task).
    pub(crate) msg_rx: kanal::AsyncReceiver<StreamMsg>,
    /// Driver's sender clone; moved out when the task spawns.
    pub(crate) msg_tx: Option<kanal::AsyncSender<StreamMsg>>,
    /// Receiver half for the request body; moved out when the task
    /// spawns (the H2Body hands it to the user).
    pub(crate) body_rx: Option<kanal::AsyncReceiver<BodyMsg>>,
    /// Receiver half for peer resets; moved out when the task spawns.
    pub(crate) reset_rx: Option<kanal::AsyncReceiver<u32>>,
    /// Wakes the drive loop when the task's channel is full.
    pub(crate) wake_tx: Option<kanal::AsyncSender<()>>,
    /// Field block fragments between HEADERS and END_HEADERS.
    pub(crate) field_block: Vec<u8>,
    /// The END_STREAM flag of the frame that opened the field block.
    pub(crate) pending_end_stream: bool,
    /// The request HEADERS were parsed and the task spawned.
    pub(crate) request_started: bool,
    /// The peer sent END_STREAM on this stream (request side done).
    pub(crate) remote_ended: bool,
    /// We sent END_STREAM on this stream (response side done).
    pub(crate) local_ended: bool,
    /// Parsed `content-length`, when present; enforced at end-of-body.
    pub(crate) content_length: Option<u64>,
    /// Sum of DATA payload lengths received so far.
    pub(crate) data_sum: u64,
    /// A trailer section was already received (only one is allowed).
    pub(crate) trailers_seen: bool,
    /// The END_HEADERS frame closed the field block; the connection
    /// may now decode and act on it.
    pub(crate) field_block_complete: bool,
    /// The stream task has finished and signalled `StreamMsg::Closed`
    /// (its message channel is now empty). The stream itself lives on
    /// until `local_ended` so any flow-controlled `pending_data` can
    /// still be drained by WINDOW_UPDATE.
    pub(crate) task_done: bool,
    /// Server-side flow-control window for this stream (RFC 9113
    /// Section 6.9): DATA payloads we may still send.
    pub(crate) send_window: i64,
    /// DATA chunks queued because the peer's flow-control window ran
    /// out; each entry is `(bytes, end_stream)`. Drained as the window
    /// opens up (WINDOW_UPDATE or SETTINGS_INITIAL_WINDOW_SIZE).
    pub(crate) pending_data: VecDeque<(Bytes, bool)>,
}

impl StreamEntry {
    pub(crate) fn new(
        body_tx: kanal::AsyncSender<BodyMsg>,
        reset_tx: kanal::AsyncSender<u32>,
        msg_rx: kanal::AsyncReceiver<StreamMsg>,
    ) -> Self {
        StreamEntry {
            body_tx,
            reset_tx,
            msg_rx,
            msg_tx: None,
            body_rx: None,
            reset_rx: None,
            wake_tx: None,
            field_block: Vec::new(),
            pending_end_stream: false,
            request_started: false,
            remote_ended: false,
            local_ended: false,
            content_length: None,
            data_sum: 0,
            trailers_seen: false,
            field_block_complete: false,
            task_done: false,
            send_window: 65535,
            pending_data: VecDeque::new(),
        }
    }

    /// Extends the in-flight field block with one fragment (HEADERS or
    /// CONTINUATION payload).
    pub(crate) fn extend_block(&mut self, block: &[u8]) {
        self.field_block.extend_from_slice(block);
    }

    /// Takes and clears the accumulated field block.
    pub(crate) fn take_block(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.field_block)
    }

    /// Forwards a request body message to the task; `Ok(false)` when the
    /// task has gone away.
    pub(crate) async fn send_body(&mut self, msg: BodyMsg) -> bool {
        self.body_tx.send(msg).await.is_ok()
    }

    pub(crate) fn send_reset(&self, code: u32) {
        let _ = self.reset_tx.try_send(code);
    }
}

/// The request body type exposed as `Incoming::H2`.
///
/// Backed by a bounded channel the connection task fills with DATA
/// frames and trailers. Polling marks the `send_continue_body` flag so
/// the driver can emit `100 Continue` on first demand (RFC 9113
/// Section 8.1.1).
pub(crate) struct H2Body {
    inner: Pin<Box<kanal::AsyncReceiver<BodyMsg>>>,
    send_continue_body: Option<Arc<AtomicBool>>,
    ended: bool,
}

impl H2Body {
    pub(crate) fn new(
        rx: kanal::AsyncReceiver<BodyMsg>,
        send_continue_body: Option<Arc<AtomicBool>>,
    ) -> Self {
        H2Body {
            inner: Box::pin(rx),
            send_continue_body,
            ended: false,
        }
    }
}

impl Body for H2Body {
    type Data = Bytes;
    type Error = std::io::Error;

    #[inline]
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        match std::pin::pin!(this.inner.recv()).poll(cx) {
            Poll::Ready(Ok(BodyMsg::Data(data))) => Poll::Ready(Some(Ok(Frame::data(data)))),
            Poll::Ready(Ok(BodyMsg::Trailers(trailers))) => {
                Poll::Ready(Some(Ok(Frame::trailers(trailers))))
            }
            Poll::Ready(Ok(BodyMsg::EndStream)) => {
                this.ended = true;
                Poll::Ready(None)
            }
            Poll::Ready(Err(_)) => {
                // The connection dropped the stream (reset, closed,
                // connection gone).
                this.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => {
                if let Some(send_continue_body) = this.send_continue_body.as_ref() {
                    send_continue_body.store(true, Ordering::Relaxed);
                }
                Poll::Pending
            }
        }
    }
}

/// A decoded and validated request, ready for the stream task.
pub(crate) struct ParsedRequest {
    pub(crate) method: Method,
    pub(crate) uri: Uri,
    pub(crate) headers: HeaderMap,
    /// Parsed `content-length`, or `None` when absent.
    pub(crate) content_length: Option<u64>,
    /// The request carries `expect: 100-continue`.
    pub(crate) expect_continue: bool,
    pub(crate) is_connect: bool,
}

/// The HEADERS block was rejected as malformed (RFC 9113
/// Section 8.1.2.6): the connection answers with a stream error
/// PROTOCOL_ERROR.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MalformedRequest;

/// Header names a request must not carry (RFC 9113 Section 8.1.2.2).
const CONNECTION_SPECIFIC: &[&[u8]] = &[
    b"connection",
    b"keep-alive",
    b"proxy-connection",
    b"transfer-encoding",
    b"upgrade",
];

pub(crate) fn is_connection_specific(name: &[u8]) -> bool {
    CONNECTION_SPECIFIC
        .iter()
        .any(|forbidden| **forbidden == *name)
}

/// Validates a decoded request field block and builds an
/// `http::Request`-ready representation.
///
/// Every violation is a *stream* error (malformed request, RFC 9113
/// Section 8.1.2.6): pseudo-header rules (Section 8.1.2.1), required
/// and unique request pseudo-headers (Section 8.1.2.3),
/// connection-specific header fields (Section 8.1.2.2) and
/// `content-length` syntax (Section 8.1.2.6).
pub(crate) fn parse_request(headers: &[Header]) -> Result<ParsedRequest, MalformedRequest> {
    let mut method: Option<Bytes> = None;
    let mut scheme: Option<Bytes> = None;
    let mut authority: Option<Bytes> = None;
    let mut path: Option<Bytes> = None;
    let mut protocol: Option<Bytes> = None;
    let mut regular = HeaderMap::new();
    let mut content_lengths: Vec<u64> = Vec::new();

    let mut pseudo_phase = true;
    for header in headers {
        let name = header.name();
        let value = header.value();
        if name.first() == Some(&b':') {
            if !pseudo_phase {
                // A pseudo-header after a regular header (Section
                // 8.1.2.1).
                return Err(MalformedRequest);
            }
            match name {
                b":method" => {
                    if method.is_some() {
                        return Err(MalformedRequest);
                    }
                    method = Some(Bytes::copy_from_slice(value));
                }
                b":scheme" => {
                    if scheme.is_some() {
                        return Err(MalformedRequest);
                    }
                    scheme = Some(Bytes::copy_from_slice(value));
                }
                b":authority" => {
                    if authority.is_some() {
                        return Err(MalformedRequest);
                    }
                    authority = Some(Bytes::copy_from_slice(value));
                }
                b":path" => {
                    if path.is_some() {
                        return Err(MalformedRequest);
                    }
                    path = Some(Bytes::copy_from_slice(value));
                }
                b":protocol" => {
                    if protocol.is_some() {
                        return Err(MalformedRequest);
                    }
                    protocol = Some(Bytes::copy_from_slice(value));
                }
                // Unknown or response-defined pseudo-header (Sections
                // 8.1.2.1).
                _ => return Err(MalformedRequest),
            }
        } else {
            pseudo_phase = false;
            if is_connection_specific(name) {
                // Section 8.1.2.2.
                return Err(MalformedRequest);
            }
            if name == b"te" && !te_is_trailers(value) {
                return Err(MalformedRequest);
            }
            if name == b"content-length" {
                content_lengths.push(parse_content_length(value)?);
            }
            if name.iter().any(|byte| byte.is_ascii_uppercase()) {
                // Field names must be lowercase (Section 8.1.2.1);
                // `HeaderName::from_bytes` would quietly normalize.
                return Err(MalformedRequest);
            }
            let name = http::header::HeaderName::from_bytes(name).map_err(|_| MalformedRequest)?;
            let value =
                http::header::HeaderValue::from_bytes(value).map_err(|_| MalformedRequest)?;
            regular.append(name, value);
        }
    }

    let is_connect = method.as_deref() == Some(b"CONNECT");
    let Some(method) = method else {
        return Err(MalformedRequest);
    };
    if is_connect {
        // CONNECT (Sections 8.3, 8.5): only :authority (and optionally
        // :protocol, RFC 8441).
        let Some(authority) = authority else {
            return Err(MalformedRequest);
        };
        if scheme.is_some() || path.is_some() {
            return Err(MalformedRequest);
        }
        let uri = Uri::from_maybe_shared(authority.clone()).map_err(|_| MalformedRequest)?;
        return Ok(ParsedRequest {
            method: Method::from_bytes(&method).map_err(|_| MalformedRequest)?,
            uri,
            headers: regular,
            content_length: parse_content_lengths(&content_lengths)?,
            expect_continue: false,
            is_connect: true,
        });
    }
    if protocol.is_some() {
        // :protocol is only valid aboard CONNECT (RFC 8441).
        return Err(MalformedRequest);
    }
    let Some(scheme) = scheme else {
        return Err(MalformedRequest);
    };
    let Some(path) = path else {
        return Err(MalformedRequest);
    };
    if path.is_empty() {
        // Section 8.1.2.3: must not be empty for http/https URIs ('*'
        // is the asterisk form).
        return Err(MalformedRequest);
    }

    let uri = {
        let scheme = std::str::from_utf8(&scheme).map_err(|_| MalformedRequest)?;
        let mut builder = Uri::builder();
        builder = builder.scheme(scheme);
        if let Some(authority) = authority.as_ref() {
            let authority = std::str::from_utf8(authority).map_err(|_| MalformedRequest)?;
            builder = builder.authority(authority);
        }
        let path = std::str::from_utf8(&path).map_err(|_| MalformedRequest)?;
        match builder.path_and_query(path).build() {
            Ok(uri) => uri,
            // :authority was omitted: fall back to the origin form so
            // the target URI still round-trips.
            Err(_) => {
                // The borrowed `path` would be dropped with the block;
                // copy so the fallback URI outlives it.
                #[allow(clippy::unnecessary_to_owned)]
                Uri::from_maybe_shared(path.to_owned()).map_err(|_| MalformedRequest)?
            }
        }
    };

    let expect_continue = regular
        .get(http::header::EXPECT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"));

    Ok(ParsedRequest {
        method: Method::from_bytes(&method).map_err(|_| MalformedRequest)?,
        uri,
        headers: regular,
        content_length: parse_content_lengths(&content_lengths)?,
        expect_continue,
        is_connect: false,
    })
}

/// The TE field is only legal with the single value "trailers"
/// (RFC 9113 Section 8.1.2.2).
pub(crate) fn te_is_trailers(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .ok()
        .is_some_and(|value| value.split(',').all(|part| part.trim() == "trailers"))
}

/// Multiple content-length fields must be identical (RFC 9110
/// Section 8.6); any non-digit value is malformed.
fn parse_content_lengths(values: &[u64]) -> Result<Option<u64>, MalformedRequest> {
    match values {
        [] => Ok(None),
        [only] => Ok(Some(*only)),
        many => {
            if many.windows(2).all(|pair| pair[0] == pair[1]) {
                Ok(Some(many[0]))
            } else {
                Err(MalformedRequest)
            }
        }
    }
}

/// Validates a trailer field block (RFC 9113 Section 8.1.2.1):
/// pseudo-header fields are not allowed in trailers.
pub(crate) fn parse_trailers(headers: &[Header]) -> Result<HeaderMap, MalformedRequest> {
    let mut trailers = HeaderMap::new();
    for header in headers {
        let name = header.name();
        if name.first() == Some(&b':') {
            return Err(MalformedRequest);
        }
        if name.iter().any(|byte| byte.is_ascii_uppercase()) {
            return Err(MalformedRequest);
        }
        let name = http::header::HeaderName::from_bytes(name).map_err(|_| MalformedRequest)?;
        let value =
            http::header::HeaderValue::from_bytes(header.value()).map_err(|_| MalformedRequest)?;
        trailers.append(name, value);
    }
    Ok(trailers)
}

/// Parses a content-length header value into a `u64`; anything but
/// bare digits is malformed.
///
/// Callers fold the result into [`parse_content_lengths`].
pub(crate) fn parse_content_length(value: &[u8]) -> Result<u64, MalformedRequest> {
    let value = value
        .iter()
        .copied()
        .skip_while(|byte| *byte == b' ' || *byte == b'\t')
        .collect::<Vec<u8>>();
    // content-length = 1*DIGIT, and it must be a valid non-negative
    // octet count. Reject signs, leading +, and non-digits.
    let value = value
        .iter()
        .rposition(|byte| *byte != b' ' && *byte != b'\t')
        .map(|end| &value[..end + 1])
        .ok_or(MalformedRequest)?;
    if value.is_empty() || value.iter().any(|byte| !byte.is_ascii_digit()) {
        return Err(MalformedRequest);
    }
    let mut result: u64 = 0;
    for byte in value {
        result = result
            .checked_mul(10)
            .and_then(|n| n.checked_add((*byte - b'0') as u64))
            .ok_or(MalformedRequest)?;
    }
    Ok(result)
}

/// The outcome of polling the service state of a [`StreamDriver`].
enum ServicePoll<ResB> {
    /// The stream task is done (response fully handed over).
    Done,
    /// Waiting on a response, early hints, or peer reset.
    Pending,
    /// The final response is ready to be piped to the connection.
    Body(ResB),
}

pin_project! {
    /// Drives one stream task: runs the user's `request_fn`, forwards
    /// interim and final responses to the connection, and pipes the
    /// response body as chunks.
    ///
    /// The task stops when the response is fully handed over, when the
    /// peer resets the stream, or when the connection goes away. On
    /// every exit path it first enqueues [`StreamMsg::Closed`], so the
    /// connection can drop its per-stream state.
    pub(crate) struct StreamDriver<Fut, ResB> {
        msg_tx: kanal::AsyncSender<StreamMsg>,
        reset_rx: kanal::AsyncReceiver<u32>,
        // Pokes the connection's drive loop when a message lands in
        // the channel, so responses are delivered even while the peer
        // idles.
        wake_tx: kanal::AsyncSender<()>,
        #[pin]
        msg_tx_fut: Option<kanal::SendFuture<'static, StreamMsg>>,
        queue: VecDeque<StreamMsg>,
        // Set once the terminal StreamMsg::Closed lands in the queue;
        // the task only drains from then on.
        done: bool,
        #[pin]
        state: StreamDriverState<Fut, ResB>,
    }
}

pin_project! {
    #[project = StreamDriverProj]
    enum StreamDriverState<Fut, ResB> {
        Service {
            #[pin]
            response_fut: Fut,
            #[pin]
            early_hints_rx: EarlyHintsReceiver,
            response_done: bool,
            body: Option<ResB>,
            send_continue: bool,
            send_continue_body: Option<Arc<AtomicBool>>,
            continue_sent: bool,
            early_hints_open: bool,
        },
        Body {
            #[pin]
            body: ResB,
        },
    }
}

impl<Fut, ResB> StreamDriver<Fut, ResB> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        response_fut: Fut,
        reset_rx: kanal::AsyncReceiver<u32>,
        msg_tx: kanal::AsyncSender<StreamMsg>,
        wake_tx: kanal::AsyncSender<()>,
        early_hints_rx: EarlyHintsReceiver,
        send_continue: bool,
        send_continue_body: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            msg_tx,
            reset_rx,
            wake_tx,
            msg_tx_fut: None,
            queue: VecDeque::with_capacity(8),
            done: false,
            state: StreamDriverState::Service {
                response_fut,
                early_hints_rx,
                response_done: false,
                body: None,
                send_continue,
                send_continue_body,
                continue_sent: false,
                early_hints_open: true,
            },
        }
    }
}

impl<Fut, ResB, ResBE, ResE> Future for StreamDriver<Fut, ResB>
where
    Fut: Future<Output = Result<Response<ResB>, ResE>>,
    ResB: Body<Data = Bytes, Error = ResBE> + Unpin,
    ResBE: std::error::Error + 'static,
    ResE: std::error::Error + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            if let Some(msg_tx_fut) = this.msg_tx_fut.as_mut().as_pin_mut() {
                match msg_tx_fut.poll(cx) {
                    Poll::Ready(Ok(_)) => {
                        // SAFETY: Pin is re-borrowed here
                        let uckm = unsafe { this.msg_tx_fut.as_mut().get_unchecked_mut() };
                        uckm.take();
                        let _ = this.wake_tx.try_send(());
                    }
                    Poll::Ready(Err(_)) => {
                        // SAFETY: Pin is re-borrowed here
                        let uckm = unsafe { this.msg_tx_fut.as_mut().get_unchecked_mut() };
                        uckm.take();
                        return Poll::Ready(());
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            // Drain the outbound queue first: the states are only
            // driven again once the queue is empty, so a completed
            // response or body frame is never re-read.
            if let Some(msg) = this.queue.pop_front() {
                let msg_tx_fut = this.msg_tx.send(msg);
                // SAFETY: msg_tx_fut lives as long as msg_tx after storing in struct
                let msg_tx_fut = unsafe {
                    std::mem::transmute::<
                        kanal::SendFuture<'_, StreamMsg>,
                        kanal::SendFuture<'static, StreamMsg>,
                    >(msg_tx_fut)
                };
                // SAFETY: Pin is re-borrowed here
                let uckm = unsafe { this.msg_tx_fut.as_mut().get_unchecked_mut() };
                *uckm = Some(msg_tx_fut);
                continue;
            }
            if *this.done {
                return Poll::Ready(());
            }
            match this.state.as_mut().project() {
                StreamDriverProj::Service {
                    response_fut,
                    early_hints_rx,
                    response_done,
                    body,
                    send_continue,
                    send_continue_body,
                    continue_sent,
                    early_hints_open,
                } => {
                    match Self::poll_service(
                        this.msg_tx,
                        this.wake_tx,
                        this.msg_tx_fut.as_mut(),
                        this.reset_rx,
                        response_fut,
                        early_hints_rx,
                        response_done,
                        body,
                        send_continue,
                        send_continue_body,
                        continue_sent,
                        early_hints_open,
                        cx,
                    ) {
                        ServicePoll::Done => {}
                        ServicePoll::Pending => return Poll::Pending,
                        ServicePoll::Body(body) => {
                            this.state.set(StreamDriverState::Body { body });
                            continue;
                        }
                    }
                }
                StreamDriverProj::Body { body } => {
                    match Self::poll_body(
                        this.msg_tx,
                        this.msg_tx_fut.as_mut(),
                        this.reset_rx,
                        body,
                        cx,
                    ) {
                        Poll::Ready(()) => {}
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
            // Terminal message: the outer drain loop delivers it before
            // the task exits, so the connection always sees Closed.
            this.queue.push_back(StreamMsg::Closed);
            *this.done = true;
            continue;
        }
    }
}

impl<Fut, ResB, ResBE, ResE> StreamDriver<Fut, ResB>
where
    Fut: Future<Output = Result<Response<ResB>, ResE>>,
    ResB: Body<Data = Bytes, Error = ResBE> + Unpin,
    ResBE: std::error::Error + 'static,
    ResE: std::error::Error + 'static,
{
    /// Polls the service state: the response future, peer resets, the
    /// `100 Continue` trigger and early hints. Returns only with the
    /// outbound channel drained.
    #[allow(clippy::too_many_arguments)]
    fn poll_service(
        msg_tx: &mut kanal::AsyncSender<StreamMsg>,
        wake_tx: &kanal::AsyncSender<()>,
        mut msg_tx_fut: Pin<&mut Option<kanal::SendFuture<'static, StreamMsg>>>,
        reset_rx: &mut kanal::AsyncReceiver<u32>,
        mut response_fut: Pin<&mut Fut>,
        mut early_hints_rx: Pin<&mut EarlyHintsReceiver>,
        response_done: &mut bool,
        body: &mut Option<ResB>,
        send_continue: &bool,
        send_continue_body: &Option<Arc<AtomicBool>>,
        continue_sent: &mut bool,
        early_hints_open: &mut bool,
        cx: &mut Context<'_>,
    ) -> ServicePoll<ResB> {
        loop {
            if *response_done {
                match body.take() {
                    Some(body) => return ServicePoll::Body(body),
                    None => return ServicePoll::Done,
                }
            }
            if let Poll::Ready(result) = response_fut.as_mut().poll(cx) {
                let Ok(response) = result else {
                    // Handler error: the stream ends without a reply.
                    *response_done = true;
                    continue;
                };
                if *send_continue && !*continue_sent {
                    if !response.status().is_client_error() && !response.status().is_server_error()
                    {
                        let mut interim = Response::new(());
                        *interim.status_mut() = StatusCode::CONTINUE;
                        let (parts, _) = interim.into_parts();

                        match Self::send(
                            msg_tx,
                            msg_tx_fut.as_mut(),
                            wake_tx,
                            StreamMsg::Informational { parts },
                            cx,
                        ) {
                            Poll::Ready(()) => {}
                            Poll::Pending => return ServicePoll::Pending,
                        }
                    }
                    *continue_sent = true;
                }
                let response_is_end_stream = response.body().is_end_stream();
                let (parts, response_body) = response.into_parts();

                match Self::send(
                    msg_tx,
                    msg_tx_fut.as_mut(),
                    wake_tx,
                    StreamMsg::Headers {
                        parts,
                        end_stream: response_is_end_stream,
                    },
                    cx,
                ) {
                    Poll::Ready(()) => {}
                    Poll::Pending => return ServicePoll::Pending,
                }
                *body = Some(response_body);
                *response_done = true;
                continue;
            }
            match std::pin::pin!(reset_rx.recv()).poll(cx) {
                Poll::Ready(_) => return ServicePoll::Done,
                Poll::Pending => {}
            }
            if *send_continue
                && !*continue_sent
                && send_continue_body
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                let mut interim = Response::new(());
                *interim.status_mut() = StatusCode::CONTINUE;
                let (parts, _) = interim.into_parts();
                match Self::send(
                    msg_tx,
                    msg_tx_fut.as_mut(),
                    wake_tx,
                    StreamMsg::Informational { parts },
                    cx,
                ) {
                    Poll::Ready(()) => {}
                    Poll::Pending => return ServicePoll::Pending,
                }
                *continue_sent = true;
                continue;
            }
            if *early_hints_open {
                match early_hints_rx.poll_recv(cx) {
                    Poll::Ready(Some((headers, sender))) => {
                        let mut interim = Response::new(());
                        *interim.status_mut() = StatusCode::EARLY_HINTS;
                        *interim.headers_mut() = headers;
                        let (parts, _) = interim.into_parts();
                        match Self::send(
                            msg_tx,
                            msg_tx_fut.as_mut(),
                            wake_tx,
                            StreamMsg::Informational { parts },
                            cx,
                        ) {
                            Poll::Ready(()) => {}
                            Poll::Pending => return ServicePoll::Pending,
                        }
                        // The write itself happens later on the
                        // connection task; enqueueing is the failure
                        // surface that matters here.
                        sender.into_inner().send(Ok(())).ok();
                        continue;
                    }
                    Poll::Ready(None) => {
                        *early_hints_open = false;
                        continue;
                    }
                    Poll::Pending => {}
                }
            }
            return ServicePoll::Pending;
        }
    }

    /// Enqueues one outbound message, parking until the channel has
    /// room. `Pending` means the sender is parked (by `poll_ready` it
    /// will be woken when the connection task drains); `Ready(())`
    /// means the message was delivered or the connection is gone.
    fn send(
        msg_tx: &mut kanal::AsyncSender<StreamMsg>,
        mut msg_tx_fut: Pin<&mut Option<kanal::SendFuture<'static, StreamMsg>>>,
        wake_tx: &kanal::AsyncSender<()>,
        msg: StreamMsg,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        if let Some(msg_tx_fut2) = msg_tx_fut.as_mut().as_pin_mut() {
            match msg_tx_fut2.poll(cx) {
                Poll::Ready(Ok(_)) => {
                    // SAFETY: Pin is re-borrowed here
                    let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                    uckm.take();
                    let _ = wake_tx.try_send(());
                }
                Poll::Ready(Err(_)) => {
                    return Poll::Ready(());
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        let msg_tx_fut2 = msg_tx.send(msg);
        // SAFETY: msg_tx_fut lives as long as msg_tx after storing in struct
        let msg_tx_fut2 = unsafe {
            std::mem::transmute::<
                kanal::SendFuture<'_, StreamMsg>,
                kanal::SendFuture<'static, StreamMsg>,
            >(msg_tx_fut2)
        };
        // SAFETY: Pin is re-borrowed here
        let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
        *uckm = Some(msg_tx_fut2);

        if let Some(msg_tx_fut2) = msg_tx_fut.as_mut().as_pin_mut() {
            match msg_tx_fut2.poll(cx) {
                Poll::Ready(Ok(_)) => {
                    // SAFETY: Pin is re-borrowed here
                    let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                    uckm.take();
                    let _ = wake_tx.try_send(());
                    return Poll::Ready(());
                }
                Poll::Ready(Err(_)) => {
                    // SAFETY: Pin is re-borrowed here
                    let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                    uckm.take();
                    return Poll::Ready(());
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        Poll::Pending
    }

    /// Pipes the response body to the connection: DATA frames, then an
    /// END_STREAM (empty DATA when the body yields nothing more, or a
    /// HEADERS block carrying trailers).
    fn poll_body(
        msg_tx: &mut kanal::AsyncSender<StreamMsg>,
        mut msg_tx_fut: Pin<&mut Option<kanal::SendFuture<'static, StreamMsg>>>,
        reset_rx: &mut kanal::AsyncReceiver<u32>,
        mut body: Pin<&mut ResB>,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        let mut end = false;
        loop {
            if let Some(msg_tx_fut2) = msg_tx_fut.as_mut().as_pin_mut() {
                match msg_tx_fut2.poll(cx) {
                    Poll::Ready(Ok(_)) => {
                        // SAFETY: Pin is re-borrowed here
                        let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                        uckm.take();
                        if end {
                            return Poll::Ready(());
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        // SAFETY: Pin is re-borrowed here
                        let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                        uckm.take();
                        return Poll::Ready(());
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            if body.is_end_stream() {
                return Poll::Ready(());
            }

            match std::pin::pin!(reset_rx.recv()).poll(cx) {
                Poll::Ready(_) => return Poll::Ready(()),
                Poll::Pending => {}
            }
            match body.as_mut().poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => {
                        let msg = StreamMsg::Data {
                            data,
                            end_stream: false,
                        };

                        let msg_tx_fut2 = msg_tx.send(msg);
                        // SAFETY: msg_tx_fut lives as long as msg_tx after storing in struct
                        let msg_tx_fut2 = unsafe {
                            std::mem::transmute::<
                                kanal::SendFuture<'_, StreamMsg>,
                                kanal::SendFuture<'static, StreamMsg>,
                            >(msg_tx_fut2)
                        };
                        // SAFETY: Pin is re-borrowed here
                        let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                        *uckm = Some(msg_tx_fut2);
                    }
                    Err(frame) => match frame.into_trailers() {
                        Ok(trailers) => {
                            let msg_tx_fut2 = msg_tx.send(StreamMsg::Trailers { trailers });
                            // SAFETY: msg_tx_fut lives as long as msg_tx after storing in struct
                            let msg_tx_fut2 = unsafe {
                                std::mem::transmute::<
                                    kanal::SendFuture<'_, StreamMsg>,
                                    kanal::SendFuture<'static, StreamMsg>,
                                >(msg_tx_fut2)
                            };
                            // SAFETY: Pin is re-borrowed here
                            let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                            *uckm = Some(msg_tx_fut2);
                            end = true;
                        }
                        Err(_) => return Poll::Ready(()),
                    },
                },
                Poll::Ready(Some(Err(_))) => {
                    let msg = StreamMsg::Reset {
                        error_code: super::error::Reason::InternalError.code(),
                    };
                    let msg_tx_fut2 = msg_tx.send(msg);
                    // SAFETY: msg_tx_fut lives as long as msg_tx after storing in struct
                    let msg_tx_fut2 = unsafe {
                        std::mem::transmute::<
                            kanal::SendFuture<'_, StreamMsg>,
                            kanal::SendFuture<'static, StreamMsg>,
                        >(msg_tx_fut2)
                    };
                    // SAFETY: Pin is re-borrowed here
                    let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                    *uckm = Some(msg_tx_fut2);
                    end = true;
                }
                Poll::Ready(None) => {
                    // The body has no more frames: close with an empty
                    // END_STREAM DATA frame.
                    let msg = StreamMsg::Data {
                        data: Bytes::new(),
                        end_stream: true,
                    };
                    let msg_tx_fut2 = msg_tx.send(msg);
                    // SAFETY: msg_tx_fut lives as long as msg_tx after storing in struct
                    let msg_tx_fut2 = unsafe {
                        std::mem::transmute::<
                            kanal::SendFuture<'_, StreamMsg>,
                            kanal::SendFuture<'static, StreamMsg>,
                        >(msg_tx_fut2)
                    };
                    // SAFETY: Pin is re-borrowed here
                    let uckm = unsafe { msg_tx_fut.as_mut().get_unchecked_mut() };
                    *uckm = Some(msg_tx_fut2);
                    end = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// Debug-friendly message types for tests that decode wire replies.

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(pairs: &[(&str, &str)]) -> ParsedRequest {
        let headers = pairs
            .iter()
            .map(|(name, value)| Header::new(name.to_string(), value.to_string()))
            .collect::<Vec<_>>();
        parse_request(&headers).expect("request should parse")
    }

    fn pair_header(pair: &(&str, &str)) -> Header {
        Header::new(pair.0.to_string(), pair.1.to_string())
    }

    #[test]
    fn parses_a_regular_request() {
        let parsed = parse_ok(&[
            (":method", "GET"),
            (":scheme", "https"),
            (":authority", "example.com"),
            (":path", "/index.html?q=1"),
        ]);
        assert_eq!(parsed.method, Method::GET);
        assert_eq!(parsed.uri, "https://example.com/index.html?q=1");
        assert_eq!(parsed.content_length, None);
        assert!(!parsed.expect_continue);
    }

    #[test]
    fn parses_without_authority() {
        let parsed = parse_ok(&[(":method", "GET"), (":scheme", "http"), (":path", "/")]);
        assert_eq!(parsed.uri, "/");
    }

    #[test]
    fn parses_asterisk_form() {
        let parsed = parse_ok(&[(":method", "OPTIONS"), (":scheme", "http"), (":path", "*")]);
        assert_eq!(parsed.uri, "*");
        assert_eq!(parsed.method, Method::OPTIONS);
    }

    #[test]
    fn parses_connect_with_authority() {
        let parsed = parse_ok(&[(":method", "CONNECT"), (":authority", "example.com:443")]);
        assert_eq!(parsed.method, Method::CONNECT);
        assert!(parsed.is_connect);
        assert_eq!(parsed.uri, "example.com:443");
    }

    #[test]
    fn rejects_unknown_and_response_pseudo_headers() {
        for name in [":test", ":status", ":foo"] {
            assert!(parse_request(&[Header::new(name, "1")]).is_err());
        }
    }

    #[test]
    fn rejects_pseudo_after_regular() {
        let headers = vec![Header::new("x-test", "ok"), Header::new(":method", "GET")];
        assert!(parse_request(&headers).is_err());
    }

    #[test]
    fn rejects_duplicated_pseudo_headers() {
        let base = [(":method", "GET"), (":scheme", "http"), (":path", "/")];
        for dup in [":method", ":scheme", ":path"] {
            let mut pairs = base.to_vec();
            pairs.push((dup, "x"));
            assert!(parse_request(&pairs.iter().map(pair_header).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn rejects_missing_required_pseudo_headers() {
        let base = [(":method", "GET"), (":scheme", "http"), (":path", "/")];
        for skip in 0..base.len() {
            let pairs = base
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, pair)| *pair)
                .collect::<Vec<_>>();
            assert!(parse_request(&pairs.iter().map(pair_header).collect::<Vec<_>>()).is_err());
        }
        // Empty :path
        assert!(parse_request(&[
            Header::new(":method", "GET"),
            Header::new(":scheme", "http"),
            Header::new(":path", ""),
        ])
        .is_err());
    }

    #[test]
    fn rejects_connection_specific_headers() {
        for name in [
            "connection",
            "keep-alive",
            "proxy-connection",
            "transfer-encoding",
            "upgrade",
        ] {
            let mut pairs = vec![(":method", "GET"), (":scheme", "http"), (":path", "/")];
            pairs.push((name, "x"));
            let headers = pairs.iter().map(pair_header).collect::<Vec<_>>();
            assert!(
                parse_request(&headers).is_err(),
                "{name} should be rejected"
            );
        }
        // TE may only carry "trailers".
        for te in ["gzip", "trailers, deflate", ""] {
            let headers = vec![
                Header::new(":method", "GET"),
                Header::new(":scheme", "http"),
                Header::new(":path", "/"),
                Header::new("te", te),
            ];
            assert!(parse_request(&headers).is_err(), "te: {te:?}");
        }
        assert!(parse_request(&[
            Header::new(":method", "GET"),
            Header::new(":scheme", "http"),
            Header::new(":path", "/"),
            Header::new("te", "trailers"),
        ])
        .is_ok());
    }

    #[test]
    fn rejects_bad_content_lengths() {
        for value in [
            "1 2",
            "abc",
            "0x10",
            "+1",
            "-1",
            "1,2",
            "18446744073709551616",
        ] {
            let headers = vec![
                Header::new(":method", "POST"),
                Header::new(":scheme", "http"),
                Header::new(":path", "/"),
                Header::new("content-length", value),
            ];
            assert!(
                parse_request(&headers).is_err(),
                "content-length: {value:?}"
            );
        }
        // Identical duplicates are legal (RFC 9110 Section 8.6).
        let parsed = parse_request(&[
            Header::new(":method", "POST"),
            Header::new(":scheme", "http"),
            Header::new(":path", "/"),
            Header::new("content-length", "4"),
            Header::new("content-length", "4"),
        ])
        .expect("identical content-lengths are legal");
        assert_eq!(parsed.content_length, Some(4));
    }

    #[test]
    fn rejects_uppercase_and_invalid_header_names_and_values() {
        let headers = vec![
            Header::new(":method", "GET"),
            Header::new(":scheme", "http"),
            Header::new(":path", "/"),
            Header::new("X-Test", "ok"),
        ];
        assert!(parse_request(&headers).is_err());
        let headers = vec![
            Header::new(":method", "GET"),
            Header::new(":scheme", "http"),
            Header::new(":path", "/"),
            Header::new("x-test", "bad\x00value"),
        ];
        assert!(parse_request(&headers).is_err());
    }

    #[test]
    fn connect_rejects_scheme_path_and_protocol_misuse() {
        // :protocol outside CONNECT.
        let headers = vec![
            Header::new(":method", "POST"),
            Header::new(":scheme", "http"),
            Header::new(":path", "/"),
            Header::new(":protocol", "websocket"),
        ];
        assert!(parse_request(&headers).is_err());
        // CONNECT without :authority.
        let headers = vec![Header::new(":method", "CONNECT")];
        assert!(parse_request(&headers).is_err());
    }

    #[test]
    fn trailers_reject_pseudo_headers() {
        assert!(parse_trailers(&[Header::new(":method", "POST")]).is_err());
        assert!(parse_trailers(&[Header::new("x-checksum", "abc")]).is_ok());
    }

    #[test]
    fn content_length_parser() {
        assert_eq!(parse_content_length(b"42"), Ok(42));
        assert_eq!(parse_content_length(b" 42 "), Ok(42));
        assert!(parse_content_length(b"").is_err());
        assert!(parse_content_length(b"-1").is_err());
        assert!(parse_function_overflow());
    }

    fn parse_function_overflow() -> bool {
        // 2^64 overflows u64: rejected.
        parse_content_length(b"18446744073709551616").is_err()
    }
}
