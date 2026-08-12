//! HTTP/2 connection (RFC 9113 Sections 3.5, 5.1, 6.5, 8.1).
//!
//! Drives one HTTP/2 connection over an async I/O stream: reads the
//! client's 24-octet preface (with a timeout), sends the server
//! SETTINGS, maintains the peer's SETTINGS state (applying
//! `SETTINGS_HEADER_TABLE_SIZE` to the HPACK encoder and
//! `SETTINGS_MAX_FRAME_SIZE` to the outgoing frame writer), answers
//! SETTINGS frames with ACK, echoes PING, and reports protocol
//! violations with GOAWAY before closing.
//!
//! Frame parsing and validation happen in [`super::codec`]; this module
//! adds the connection- and stream-level wiring: per-stream state,
//! request/response header parsing, request dispatch to the handler,
//! response framing, and the connection-level error handling that
//! h2spec's `http2/4.3`, `http2/5.1`, `http2/6.1`, `http2/6.2`,
//! `http2/6.4` and `http2/8.1` groups cover.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{atomic::AtomicBool, Arc},
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures_util::{pin_mut, FutureExt};
use http::{Request, Response, StatusCode};
use http_body::Body;

use super::codec::{
    Frame, FrameDecoder, FrameWriter, Setting, CLIENT_PREFACE, DEFAULT_INITIAL_WINDOW_SIZE,
    DEFAULT_MAX_FRAME_SIZE,
};
use super::date::DateCache;
use super::error::Reason;
use super::hpack::{Decoder as HpackDecoder, Encoder, Header as HpackHeader};
use super::sanitize_response;
use super::stream::{
    BodyMsg, H2Body, MalformedRequest, ParsedRequest, StreamDriver, StreamEntry, StreamMsg,
};
use crate::early_hints::EarlyHints;
use crate::Incoming;

/// Per-connection behavior options.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionOptions {
    /// Answer requests with `100 Continue` when they carry
    /// `expect: 100-continue`.
    pub send_continue_response: bool,
    /// Add a `Date` header to responses.
    pub send_date_header: bool,
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` announced to the peer.
    pub max_concurrent_streams: u32,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        ConnectionOptions {
            send_continue_response: false,
            send_date_header: true,
            max_concurrent_streams: 100,
        }
    }
}

/// The peer's current SETTINGS values. Defaults are the RFC 9113
/// Section 6.5.2 initial values; they change as non-ACK SETTINGS frames
/// arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerSettings {
    pub(crate) header_table_size: u32,
    pub(crate) enable_push: u32,
    pub(crate) initial_window_size: u32,
    pub(crate) max_frame_size: usize,
    /// Kept for the field block decoder's bomb protection (C2).
    #[allow(dead_code)]
    pub(crate) max_header_list_size: u32,
}

impl Default for PeerSettings {
    fn default() -> Self {
        PeerSettings {
            header_table_size: 4096,
            enable_push: 1,
            initial_window_size: DEFAULT_INITIAL_WINDOW_SIZE,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_header_list_size: u32::MAX,
        }
    }
}

/// An HTTP/2 server connection.
///
/// `Io` must be a raw transport: the preface is read byte-exact, so
/// any buffering layer between the socket and this type breaks the
/// initial read.
pub struct Connection<Io> {
    io: Io,
    decoder: FrameDecoder,
    writer: FrameWriter,
    out: Vec<u8>,
    /// Encoder for the field blocks this connection sends; its table
    /// size follows the peer's `SETTINGS_HEADER_TABLE_SIZE`.
    encoder: Encoder,
    /// Decoder for the peer's field blocks; sees every header block on
    /// this connection (RFC 9113 Section 4.3).
    request_decoder: HpackDecoder,
    /// The peer's settings, updated by non-ACK SETTINGS frames.
    peer: PeerSettings,
    /// The settings this connection announced (public RFC defaults
    /// plus `SETTINGS_MAX_CONCURRENT_STREAMS`).
    #[allow(dead_code)]
    local: PeerSettings,
    /// Bounds the wait for the client's 24-octet preface. A peer that
    /// is too slow is disconnected without a GOAWAY.
    preface_timeout: Option<Duration>,
    /// Active streams, keyed by stream id (RFC 9113 Section 5.1).
    streams: HashMap<u32, StreamEntry>,
    /// Connection-level send window for DATA payloads (RFC 9113
    /// Section 6.9.1); initial 65,535 octets.
    conn_window: i64,
    /// Stream ids whose streams have ended (closed state per RFC 9113
    /// Section 5.1); used to tell closed-stream frames apart from
    /// idle-stream frames.
    closed_streams: HashSet<u32>,
    /// Behavior options for this connection (used by [`Connection::handle`]).
    #[allow(dead_code)]
    opts: ConnectionOptions,
    /// Wakes the drive loop when a stream task fills its outbound
    /// channel; the loop drains channels between reads.
    wake_tx: Option<tokio::sync::mpsc::Sender<()>>,
    /// Highest stream id opened by the peer (RFC 9113 Section 5.1.1).
    highest_stream_id: u32,
    /// A connection error is pending; the loop stops after flushing.
    closing: bool,
    /// Shared `Date` header value, refreshed periodically for the
    /// responses the stream tasks emit.
    date_cache: Arc<DateCache>,
}

impl<Io> Connection<Io>
where
    Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    /// Creates a connection over `io`.
    pub fn new(io: Io, preface_timeout: Option<Duration>) -> Connection<Io> {
        Connection {
            io,
            decoder: FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE),
            writer: FrameWriter::new(DEFAULT_MAX_FRAME_SIZE),
            out: Vec::new(),
            encoder: Encoder::new(4096),
            request_decoder: HpackDecoder::new(4096),
            peer: PeerSettings::default(),
            local: PeerSettings::default(),
            preface_timeout,
            streams: HashMap::new(),
            conn_window: DEFAULT_INITIAL_WINDOW_SIZE as i64,
            closed_streams: HashSet::new(),
            opts: ConnectionOptions::default(),
            wake_tx: None,
            highest_stream_id: 0,
            closing: false,
            date_cache: Arc::new(DateCache::new()),
        }
    }

    /// Drives a connection that never serves requests: the preface
    /// handshake, SETTINGS/PING maintenance and error handling, but no
    /// request dispatch (any request stream is refused).
    ///
    /// Equivalent to [`Connection::handle`] with a handler that never
    /// completes; kept for tests and for callers that only want the
    /// connection-level behavior.
    pub async fn drive(self) -> std::io::Result<()> {
        self.handle(
            |_| std::future::pending::<Result<Response<Incoming>, std::io::Error>>(),
            ConnectionOptions::default(),
        )
        .await
    }

    /// Serves requests to completion: peer EOF, GOAWAY received,
    /// preface timeout, or an unrecoverable protocol error.
    ///
    /// Each decoded request starts a stream task running `request_fn`
    /// (a clone-free sequential borrow: the loop owns the closure for
    /// the connection's lifetime). Responses are framed onto the wire
    /// by this task as the stream tasks emit them.
    pub async fn handle<F, Fut, ResB, ResBE, ResE>(
        mut self,
        mut request_fn: F,
        options: ConnectionOptions,
    ) -> std::io::Result<()>
    where
        F: FnMut(Request<Incoming>) -> Fut + Clone + 'static,
        Fut: Future<Output = Result<Response<ResB>, ResE>> + Send + 'static,
        ResB: Body<Data = Bytes, Error = ResBE> + Send + 'static,
        ResBE: std::error::Error + Send + Sync + 'static,
        ResE: std::error::Error + Send + Sync + 'static,
    {
        self.opts = options;
        match self.read_preface().await? {
            None => return Ok(()), // preface timeout: close quietly
            Some(false) => {
                // Invalid preface: connection error PROTOCOL_ERROR
                // (RFC 9113 Section 3.5), then close.
                self.goaway(Reason::ProtocolError, b"invalid connection preface");
                self.flush().await?;
                return Ok(());
            }
            Some(true) => {}
        }

        // Our connection preface: SETTINGS announcing our
        // MAX_CONCURRENT_STREAMS plus the RFC defaults (RFC 9113
        // Sections 3.5 and 6.5.2).
        self.writer.write_settings(
            &mut self.out,
            &[Setting {
                id: 0x03,
                value: self.opts.max_concurrent_streams,
            }],
        );
        self.flush().await?;

        let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel(1);
        self.wake_tx = Some(wake_tx);

        let mut buf = [0u8; 8192];
        let mut peer_goaway = false;
        while !peer_goaway {
            let wake_recv = wake_rx.recv().fuse();
            let read = tokio::io::AsyncReadExt::read(&mut self.io, &mut buf).fuse();
            pin_mut!(wake_recv);
            pin_mut!(read);
            futures_util::select! {
                n = read => {
                    let n = n?;
                    if n == 0 {
                        break; // peer closed; nothing more to say
                    }
                    self.decoder.extend(&buf[..n]);
                    peer_goaway = self.process_frames(&mut request_fn).await?;
                    self.drain_outbound();
                    self.flush().await?;
                }
                _ = wake_recv => {
                    // A stream task parked on a full channel; drain it.
                    self.drain_outbound();
                    self.flush().await?;
                }
            }
        }
        self.flush().await?;
        Ok(())
    }

    /// Reads the 24-octet client preface.
    ///
    /// Returns `Ok(None)` on timeout (the connection closes quietly —
    /// answering a peer that never spoke is meaningless), `Ok(Some(
    /// true))` on a match, and `Ok(Some(false))` when the peer sent
    /// something else (the caller answers with GOAWAY).
    async fn read_preface(&mut self) -> std::io::Result<Option<bool>> {
        let mut magic = [0u8; CLIENT_PREFACE.len()];
        match self.preface_timeout {
            Some(timeout) => {
                match vibeio::time::timeout(
                    timeout,
                    tokio::io::AsyncReadExt::read_exact(&mut self.io, &mut magic),
                )
                .await
                {
                    Ok(result) => {
                        result?;
                    }
                    Err(_elapsed) => return Ok(None),
                }
            }
            None => {
                tokio::io::AsyncReadExt::read_exact(&mut self.io, &mut magic).await?;
            }
        }
        Ok(Some(magic == CLIENT_PREFACE))
    }

    /// Decodes and handles every frame currently buffered. Returns
    /// `Ok(true)` when the connection should end (peer GOAWAY or an
    /// error we answered with GOAWAY).
    /// Decodes and handles every frame currently buffered. Returns
    /// `Ok(true)` when the connection should end (peer GOAWAY or an
    /// error we answered with GOAWAY).
    async fn process_frames<F, Fut, ResB, ResBE, ResE>(
        &mut self,
        request_fn: &mut F,
    ) -> std::io::Result<bool>
    where
        F: FnMut(Request<Incoming>) -> Fut + Clone + 'static,
        Fut: Future<Output = Result<Response<ResB>, ResE>> + Send + 'static,
        ResB: Body<Data = Bytes, Error = ResBE> + Send + 'static,
        ResBE: std::error::Error + Send + Sync + 'static,
        ResE: std::error::Error + Send + Sync + 'static,
    {
        loop {
            let frame = match self.decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ok(false),
                Err(error) => {
                    // Frame-level violation: GOAWAY with the code the
                    // codec determined (RFC 9113 Sections 6.10, 6.5.2),
                    // then close.
                    self.goaway(error.reason, b"frame error");
                    self.flush().await?;
                    return Ok(true);
                }
            };
            match frame {
                Frame::Settings {
                    ack: false,
                    settings,
                } => {
                    self.apply_peer_settings(&settings);
                    self.writer.write_settings_ack(&mut self.out);
                }
                Frame::Settings { ack: true, .. } => {}
                Frame::Ping {
                    ack: false,
                    payload,
                } => {
                    self.writer.write_ping_ack(&mut self.out, &payload);
                }
                Frame::Ping { ack: true, .. } => {}
                Frame::GoAway { .. } => return Ok(true),
                Frame::Headers {
                    stream_id,
                    end_stream,
                    end_headers,
                    block,
                    ..
                } => {
                    self.handle_headers_frame(stream_id, end_stream, end_headers, &block);
                }
                Frame::Continuation {
                    stream_id,
                    end_headers,
                    block,
                } => {
                    self.handle_continuation(stream_id, end_headers, &block);
                }
                Frame::Data {
                    stream_id,
                    end_stream,
                    data,
                } => {
                    self.handle_data_frame(stream_id, end_stream, data).await;
                }
                Frame::Reset {
                    stream_id,
                    error_code,
                } => {
                    self.handle_reset_frame(stream_id, error_code);
                }
                Frame::Priority { .. } => {}
                Frame::WindowUpdate {
                    stream_id,
                    increment,
                } => self.handle_window_update(stream_id, increment),
                Frame::PushPromise { .. } => {
                    // A server must not receive PUSH_PROMISE (RFC 9113
                    // Section 8.2.1): connection error PROTOCOL_ERROR.
                    self.goaway(Reason::ProtocolError, b"push promise to server");
                }
                Frame::Unknown { .. } => {}
            }
            // One HEADERS/CONTINUATION chain completes at most one
            // field block per frame arrival; act on it while the
            // handler closure is in scope.
            if let Some(id) = self.take_complete_block() {
                self.finalize_field_block(id, request_fn).await;
            }
            if self.closing {
                self.flush().await?;
                return Ok(true);
            }
            if !self.out.is_empty() {
                self.flush().await?;
            }
        }
    }

    /// A HEADERS frame: the start of a request field block, a trailer
    /// section, or a protocol violation.
    fn handle_headers_frame(
        &mut self,
        stream_id: u32,
        end_stream: bool,
        end_headers: bool,
        block: &[u8],
    ) {
        let (start_new, remote_ended) = match self.streams.get_mut(&stream_id) {
            None => (true, false),
            Some(entry) => {
                if entry.remote_ended {
                    (false, true)
                } else {
                    entry.pending_end_stream = end_stream;
                    entry.extend_block(block);
                    entry.field_block_complete = end_headers;
                    (false, false)
                }
            }
        };
        if remote_ended {
            // Trailing frames after the request ended (RFC 9113
            // Section 6.2): stream error STREAM_CLOSED.
            self.stream_error(stream_id, Reason::StreamClosed);
        } else if start_new {
            self.open_request_stream(stream_id, end_stream, end_headers, block);
        }
    }

    /// A CONTINUATION fragment of the open field block.
    fn handle_continuation(&mut self, stream_id: u32, end_headers: bool, block: &[u8]) {
        let Some(entry) = self.streams.get_mut(&stream_id) else {
            // The codec already rejects stray CONTINUATION frames;
            // this guard keeps the module safe on its own.
            self.goaway(Reason::ProtocolError, b"continuation on unknown stream");
            return;
        };
        entry.extend_block(block);
        entry.field_block_complete = end_headers;
    }

    /// A HEADERS block arrived on a stream with no entry: validate and
    /// open it (RFC 9113 Sections 5.1.1 and 6.2).
    fn open_request_stream(
        &mut self,
        stream_id: u32,
        end_stream: bool,
        end_headers: bool,
        block: &[u8],
    ) {
        if stream_id == 0 || stream_id.is_multiple_of(2) {
            // The server initiates odd streams only (RFC 9113
            // Section 5.1.1): connection error PROTOCOL_ERROR.
            self.goaway(Reason::ProtocolError, b"headers on invalid stream");
            return;
        }
        if stream_id <= self.highest_stream_id {
            // Stream ids must increase (RFC 9113 Section 5.1.1):
            // connection error PROTOCOL_ERROR.
            self.goaway(Reason::ProtocolError, b"non-increasing stream id");
            return;
        }
        self.highest_stream_id = stream_id;
        if self.streams.len() as u32 >= self.opts.max_concurrent_streams {
            // Advertising is a limit the peer must respect; refuse
            // politely instead of opening the stream (RFC 9113
            // Section 5.1.2).
            self.writer
                .write_reset(&mut self.out, stream_id, Reason::RefusedStream.code());
            return;
        }
        let (body_tx, body_rx) = tokio::sync::mpsc::channel(32);
        let (reset_tx, reset_rx) = tokio::sync::mpsc::channel(1);
        let (msg_tx, msg_rx) = futures_channel::mpsc::channel(16);
        let mut entry = StreamEntry::new(body_tx, reset_tx, msg_rx);
        entry.send_window = self.peer.initial_window_size as i64;
        entry.msg_tx = Some(msg_tx);
        entry.body_rx = Some(body_rx);
        entry.reset_rx = Some(reset_rx);
        entry.wake_tx = Some(self.wake_tx.as_ref().expect("wake sender").clone());
        entry.pending_end_stream = end_stream;
        entry.extend_block(block);
        entry.field_block_complete = end_headers;
        self.streams.insert(stream_id, entry);
    }

    /// Removes the completed-block marker for a stream, if any.
    fn take_complete_block(&mut self) -> Option<u32> {
        let id = self
            .streams
            .iter()
            .find_map(|(id, entry)| entry.field_block_complete.then_some(*id))?;
        if let Some(entry) = self.streams.get_mut(&id) {
            entry.field_block_complete = false;
        }
        Some(id)
    }

    /// The field block is complete: decode it and, depending on the
    /// stream's phase, build and dispatch the request or the trailers.
    async fn finalize_field_block<F, Fut, ResB, ResBE, ResE>(
        &mut self,
        stream_id: u32,
        request_fn: &mut F,
    ) where
        F: FnMut(Request<Incoming>) -> Fut + Clone + 'static,
        Fut: Future<Output = Result<Response<ResB>, ResE>> + Send + 'static,
        ResB: Body<Data = Bytes, Error = ResBE> + Send + 'static,
        ResBE: std::error::Error + Send + Sync + 'static,
        ResE: std::error::Error + Send + Sync + 'static,
    {
        let Some(entry) = self.streams.get_mut(&stream_id) else {
            return;
        };
        let block = entry.take_block();
        let end_stream = entry.pending_end_stream;
        let decoded = match self.request_decoder.decode(&block) {
            Ok(headers) => headers,
            Err(_) => {
                // Compression errors are connection errors (RFC 9113
                // Section 4.3).
                self.goaway(Reason::CompressionError, b"hpack decode error");
                return;
            }
        };
        if entry.request_started {
            // Trailer section (RFC 9113 Section 8.1).
            let trailers = match super::stream::parse_trailers(&decoded) {
                Ok(trailers) => trailers,
                Err(MalformedRequest) => {
                    self.stream_error(stream_id, Reason::ProtocolError);
                    return;
                }
            };
            if entry.trailers_seen {
                self.stream_error(stream_id, Reason::ProtocolError);
                return;
            }
            entry.trailers_seen = true;
            if !end_stream {
                // Trailers must end the stream (RFC 9113 Section 8.1).
                self.stream_error(stream_id, Reason::ProtocolError);
                return;
            }
            if !entry.send_body(BodyMsg::Trailers(trailers)).await {
                self.mark_closed(stream_id);
                self.streams.remove(&stream_id);
                return;
            }
            if end_stream {
                self.end_request_body(stream_id).await;
            }
            return;
        }
        let parsed = match super::stream::parse_request(&decoded) {
            Ok(parsed) => parsed,
            Err(MalformedRequest) => {
                // Malformed requests are stream errors (RFC 9113
                // Section 8.1.2.6).
                self.stream_error(stream_id, Reason::ProtocolError);
                return;
            }
        };
        self.spawn_request(stream_id, end_stream, parsed, request_fn);
    }

    /// Spawns the stream task for a parsed request (RFC 9113
    /// Section 8.1.1): builds the `Request<Incoming>`, boxes the
    /// handler response, and hands the channels to a [`StreamDriver`].
    fn spawn_request<F, Fut, ResB, ResBE, ResE>(
        &mut self,
        stream_id: u32,
        end_stream: bool,
        parsed: ParsedRequest,
        request_fn: &mut F,
    ) where
        F: FnMut(Request<Incoming>) -> Fut + Clone + 'static,
        Fut: Future<Output = Result<Response<ResB>, ResE>> + Send + 'static,
        ResB: Body<Data = Bytes, Error = ResBE> + Send + 'static,
        ResBE: std::error::Error + Send + Sync + 'static,
        ResE: std::error::Error + Send + Sync + 'static,
    {
        let Some(entry) = self.streams.get_mut(&stream_id) else {
            return;
        };
        entry.request_started = true;
        entry.content_length = parsed.content_length;
        // Sender halves stay in the entry; receiver halves move to the
        // task and the request body.
        let wake_tx = entry.wake_tx.take().expect("wake sender");
        let msg_tx = entry.msg_tx.take().expect("message sender");
        let body_rx = entry.body_rx.take().expect("body receiver");
        let reset_rx = entry.reset_rx.take().expect("reset receiver");

        let send_continue = self.opts.send_continue_response && parsed.expect_continue;
        let send_continue_body = send_continue.then(|| Arc::new(AtomicBool::new(false)));
        let (early_hints, early_hints_rx) = EarlyHints::new_lazy();

        let mut request = Request::new(if parsed.is_connect {
            Incoming::Empty
        } else {
            Incoming::H2(H2Body::new(body_rx, send_continue_body.clone()))
        });
        *request.method_mut() = parsed.method;
        *request.uri_mut() = parsed.uri;
        *request.version_mut() = http::Version::HTTP_2;
        *request.headers_mut() = parsed.headers;
        request.extensions_mut().insert(early_hints);
        if end_stream && parsed.content_length.is_some_and(|cl| cl != 0) {
            // A request that ended without delivering its declared
            // body: stream error (RFC 9113 Section 8.1.2.6).
            self.stream_error(stream_id, Reason::ProtocolError);
            return;
        }
        if let Some(entry) = self.streams.get_mut(&stream_id) {
            entry.remote_ended = end_stream;
        }

        let date_cache = self.date_cache.clone();
        let send_date_header = self.opts.send_date_header;
        let mut request_fn = request_fn.clone();
        let response_fut = Box::pin(async move {
            let mut response = request_fn(request).await.map_err(std::io::Error::other)?;
            sanitize_response(&mut response, send_date_header, &date_cache);
            Ok::<Response<ConnBody>, std::io::Error>(response.map(ConnBody::new))
        });

        vibeio::spawn(StreamDriver::new(
            response_fut,
            reset_rx,
            msg_tx,
            wake_tx,
            early_hints_rx,
            send_continue,
            send_continue_body,
        ));
    }

    /// Remembers a stream id whose stream has ended for good, so
    /// frames for it can be told apart from idle-stream frames
    /// (RFC 9113 Section 5.1). Bounded to avoid unbounded growth on
    /// hostile input.
    fn mark_closed(&mut self, stream_id: u32) {
        if self.closed_streams.len() >= 4096 {
            self.closed_streams.clear();
        }
        self.closed_streams.insert(stream_id);
    }

    /// A DATA frame: forward to the task and restore flow-control
    /// windows (RFC 9113 Sections 6.1 and 6.9.2).
    async fn handle_data_frame(&mut self, stream_id: u32, end_stream: bool, data: Bytes) {
        self.writer
            .write_window_update(&mut self.out, stream_id, data.len() as u32);
        self.writer
            .write_window_update(&mut self.out, 0, data.len() as u32);
        let state = match self.streams.get_mut(&stream_id) {
            None => {
                if self.closed_streams.contains(&stream_id) {
                    StreamDataState::Closed
                } else {
                    StreamDataState::Idle
                }
            }
            Some(entry) => {
                if !entry.request_started || entry.remote_ended {
                    StreamDataState::Bad
                } else {
                    entry.data_sum += data.len() as u64;
                    if !entry.send_body(BodyMsg::Data(data)).await {
                        StreamDataState::Gone
                    } else {
                        StreamDataState::Ok
                    }
                }
            }
        };
        match state {
            StreamDataState::Idle => {
                // DATA on an idle stream: connection error
                // (RFC 9113 Section 5.1).
                self.goaway(Reason::ProtocolError, b"data on idle stream");
            }
            StreamDataState::Closed => {
                // DATA on a closed stream: stream error
                // (RFC 9113 Section 5.1).
                self.writer
                    .write_reset(&mut self.out, stream_id, Reason::StreamClosed.code());
            }
            StreamDataState::Bad => self.stream_error(stream_id, Reason::StreamClosed),
            StreamDataState::Gone => {
                self.mark_closed(stream_id);
                self.streams.remove(&stream_id);
            }
            StreamDataState::Ok => {
                if end_stream {
                    self.end_request_body(stream_id).await;
                }
            }
        }
    }

    /// The request body ended: close the request side and enforce the
    /// declared `content-length` (RFC 9113 Section 8.1.2.6).
    async fn end_request_body(&mut self, stream_id: u32) {
        let (mismatch, gone) = {
            let entry = match self.streams.get_mut(&stream_id) {
                Some(entry) => entry,
                None => return,
            };
            if entry.remote_ended {
                return;
            }
            entry.remote_ended = true;
            if entry.content_length.is_some_and(|cl| entry.data_sum != cl) {
                (true, false)
            } else {
                let ok = entry.send_body(BodyMsg::EndStream).await;
                (false, !ok)
            }
        };
        if mismatch {
            self.stream_error(stream_id, Reason::ProtocolError);
        } else if gone {
            self.mark_closed(stream_id);
            self.streams.remove(&stream_id);
        }
    }

    /// A RST_STREAM frame from the peer.
    fn handle_reset_frame(&mut self, stream_id: u32, error_code: u32) {
        // RFC 9113 Section 5.1: RST_STREAM on a stream that never
        // existed is a connection error.
        let Some(entry) = self.streams.remove(&stream_id) else {
            if !self.closed_streams.contains(&stream_id) {
                self.goaway(Reason::ProtocolError, b"rst on idle stream");
            }
            return;
        };
        self.mark_closed(stream_id);
        // Unblock the task: the body reader reports the reset and the
        // task ends. Dropping the entry also severs the message
        // channel, which finishes the task if it was parked.
        if !entry.remote_ended {
            entry.send_reset(error_code);
        }
    }
    fn apply_peer_settings(&mut self, settings: &[super::codec::Setting]) {
        for setting in settings {
            match setting.id {
                0x01 => {
                    // SETTINGS_HEADER_TABLE_SIZE: the peer's decode
                    // table, i.e. our encode table (RFC 9113
                    // Section 6.5.2, RFC 7541 Section 4.2).
                    self.peer.header_table_size = setting.value;
                    self.encoder.queue_size_update(setting.value as usize);
                }
                0x02 => self.peer.enable_push = setting.value,
                0x04 => {
                    // Each setting applies to all open streams at the
                    // moment it is processed (RFC 9113 Section 6.9.2);
                    // a window beyond 2^31-1 is a connection error.
                    let delta = setting.value as i64 - self.peer.initial_window_size as i64;
                    self.peer.initial_window_size = setting.value;
                    for entry in self.streams.values_mut() {
                        entry.send_window += delta;
                    }
                    if self
                        .streams
                        .values()
                        .any(|entry| entry.send_window > i32::MAX as i64)
                    {
                        self.goaway(Reason::FlowControlError, b"initial window overflow");
                    }
                }
                0x05 => {
                    // SETTINGS_MAX_FRAME_SIZE: cap for frames we send.
                    self.peer.max_frame_size = setting.value as usize;
                    self.writer.max_frame_size = setting.value as usize;
                }
                0x06 => self.peer.max_header_list_size = setting.value,
                _ => {}
            }
        }
    }

    /// Queues a GOAWAY frame; the connection closes after it flushes.
    fn goaway(&mut self, reason: Reason, debug: &[u8]) {
        self.closing = true;
        self.writer
            .write_goaway(&mut self.out, self.highest_stream_id, reason.code(), debug);
    }

    /// Queues a RST_STREAM for a stream error and forgets the stream.
    /// The task is severed by dropping the entry's channels, so it
    /// ends on its next poll.
    fn stream_error(&mut self, stream_id: u32, reason: Reason) {
        self.writer
            .write_reset(&mut self.out, stream_id, reason.code());
        self.mark_closed(stream_id);
        self.streams.remove(&stream_id);
    }

    /// A WINDOW_UPDATE frame: grow the sender window, checking for the
    /// 2^31-1 overflow (RFC 9113 Sections 6.9 and 6.9.1).
    fn handle_window_update(&mut self, stream_id: u32, increment: u32) {
        if increment == 0 {
            return;
        }
        let inc = increment as i64;
        if stream_id == 0 {
            if self.conn_window > i32::MAX as i64 - inc {
                self.goaway(Reason::FlowControlError, b"connection window overflow");
                return;
            }
            self.conn_window += inc;
        } else {
            let Some(entry) = self.streams.get_mut(&stream_id) else {
                // WINDOW_UPDATE on an idle stream is a connection
                // error (RFC 9113 Section 5.1); closed streams may
                // legitimately receive it.
                if !self.closed_streams.contains(&stream_id) {
                    self.goaway(Reason::ProtocolError, b"window update on idle stream");
                }
                return;
            };
            if entry.send_window > i32::MAX as i64 - inc {
                self.stream_error(stream_id, Reason::FlowControlError);
                return;
            }
            entry.send_window += inc;
        }
        self.drain_pending_data();
    }

    /// Sends queued DATA chunks for one stream, respecting the flow
    /// control windows and the peer's max frame size. Returns when the
    /// window is exhausted or the queue is empty.
    fn pump_stream_data(&mut self, stream_id: u32) {
        loop {
            // Decide how much (if any) of the front chunk to send.
            let amount = match self.streams.get_mut(&stream_id) {
                None => return,
                Some(entry) => {
                    if entry.local_ended {
                        return;
                    }
                    let Some((data, end_stream)) = entry.pending_data.front() else {
                        return;
                    };
                    if data.is_empty() {
                        // Zero-length frames are not flow controlled.
                        let end = *end_stream;
                        self.writer.write_data(&mut self.out, stream_id, end, data);
                        let retire = {
                            let entry = self.streams.get_mut(&stream_id).unwrap();
                            entry.pending_data.pop_front();
                            if end {
                                entry.local_ended = true;
                                entry.task_done
                            } else {
                                false
                            }
                        };
                        if retire {
                            self.mark_closed(stream_id);
                            self.streams.remove(&stream_id);
                            return;
                        }
                        continue;
                    }
                    let available = self.conn_window.min(entry.send_window);
                    if available <= 0 {
                        return;
                    }
                    (data.len() as i64)
                        .min(available)
                        .min(self.peer.max_frame_size as i64) as usize
                }
            };
            // Send `amount` bytes from the front chunk; the entry borrow
            // ends before we may remove the stream below.
            let (frame_end, all, chunk) = {
                let entry = self.streams.get_mut(&stream_id).unwrap();
                let (data, end_stream) = entry.pending_data.front_mut().unwrap();
                let all = amount == data.len();
                let frame_end = *end_stream && all;
                let chunk = data.split_to(amount);
                entry.send_window -= amount as i64;
                (frame_end, all, chunk)
            };
            self.writer
                .write_data(&mut self.out, stream_id, frame_end, &chunk);
            self.conn_window -= amount as i64;
            if all {
                // The chunk is fully consumed; pop it and, if it carried
                // END_STREAM and the task is gone, retire the stream.
                let retire = {
                    let entry = self.streams.get_mut(&stream_id).unwrap();
                    entry.pending_data.pop_front();
                    if frame_end {
                        entry.local_ended = true;
                        entry.task_done
                    } else {
                        false
                    }
                };
                if retire {
                    self.mark_closed(stream_id);
                    self.streams.remove(&stream_id);
                    return;
                }
            } else {
                // The tail waits for the window to open again.
                break;
            }
        }
    }

    /// Attempts to drain every stream's queued DATA after the flow
    /// control windows opened up.
    fn drain_pending_data(&mut self) {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            self.pump_stream_data(id);
        }
    }

    /// Drains every stream task's outbound channel, turning messages
    /// into frames. Called after each read and whenever a wake fires.
    fn drain_outbound(&mut self) {
        let pending: Vec<(u32, Vec<StreamMsg>)> = self
            .streams
            .iter_mut()
            .filter_map(|(id, entry)| {
                let mut msgs = Vec::new();
                while let Ok(msg) = entry.msg_rx.try_recv() {
                    msgs.push(msg);
                }
                if msgs.is_empty() {
                    None
                } else {
                    Some((*id, msgs))
                }
            })
            .collect();
        for (stream_id, msgs) in pending {
            for msg in msgs {
                self.handle_stream_msg(stream_id, msg);
            }
        }
    }

    /// One response-side message from a stream task.
    fn handle_stream_msg(&mut self, stream_id: u32, msg: StreamMsg) {
        match msg {
            StreamMsg::Informational { parts, .. } => {
                self.encode_field_block(stream_id, false, parts.status, &parts.headers);
            }
            StreamMsg::Headers {
                parts, end_stream, ..
            } => {
                let entry = self.streams.get_mut(&stream_id);
                match entry {
                    None => {}
                    Some(entry) => {
                        if entry.local_ended {
                            // No double END_STREAM (the task's body
                            // continued after the trailer section).
                            return;
                        }
                        entry.local_ended = end_stream;
                    }
                }
                self.encode_field_block(stream_id, end_stream, parts.status, &parts.headers);
            }
            StreamMsg::Data {
                data, end_stream, ..
            } => {
                let Some(entry) = self.streams.get_mut(&stream_id) else {
                    return;
                };
                if entry.local_ended {
                    // No DATA after END_STREAM (the task's body
                    // continued after the trailer section).
                    return;
                }
                if entry.pending_data.len() >= 32 {
                    // The window never opened: give up on the stream.
                    self.stream_error(stream_id, Reason::InternalError);
                    return;
                }
                entry.pending_data.push_back((data, end_stream));
                self.pump_stream_data(stream_id);
            }
            StreamMsg::Trailers { trailers, .. } => {
                let entry = match self.streams.get_mut(&stream_id) {
                    Some(entry) => entry,
                    None => return,
                };
                if entry.local_ended {
                    return;
                }
                entry.local_ended = true;
                let mut block = Vec::new();
                let mut headers: Vec<HpackHeader> = Vec::with_capacity(trailers.len());
                for (name, value) in trailers.iter() {
                    headers.push(HpackHeader::new(
                        name.as_str().as_bytes().to_vec(),
                        value.as_bytes().to_vec(),
                    ));
                }
                self.encoder.encode(&headers, &mut block);
                self.writer
                    .write_field_block(&mut self.out, stream_id, true, &block);
            }
            StreamMsg::Reset { error_code, .. } => {
                self.writer
                    .write_reset(&mut self.out, stream_id, error_code);
                self.mark_closed(stream_id);
                self.streams.remove(&stream_id);
            }
            StreamMsg::Closed => {
                // The task ended for good. If the whole response was
                // already sent (END_STREAM flushed), tear down now.
                // Otherwise flow control left DATA queued in
                // `pending_data`; keep the stream alive so a later
                // WINDOW_UPDATE (via `drain_pending_data`) can flush it.
                let entry = match self.streams.get_mut(&stream_id) {
                    Some(entry) => entry,
                    None => return,
                };
                entry.task_done = true;
                if entry.local_ended {
                    self.mark_closed(stream_id);
                    self.streams.remove(&stream_id);
                }
            }
        }
    }

    /// Encodes a response (or interim) field block: a `:status` pseudo
    /// header followed by the response headers, skipping the
    /// connection-specific fields the codec would reject anyway.
    fn encode_field_block(
        &mut self,
        stream_id: u32,
        end_stream: bool,
        status: StatusCode,
        headers: &http::HeaderMap,
    ) {
        let mut fields: Vec<HpackHeader> = Vec::with_capacity(headers.len() + 1);
        fields.push(HpackHeader::new(
            Bytes::from_static(b":status"),
            status.as_u16().to_string().into_bytes(),
        ));
        for (name, value) in headers.iter() {
            let name_bytes = name.as_str().as_bytes();
            if super::stream::is_connection_specific(name_bytes) {
                continue;
            }
            if name == http::header::TE && !super::stream::te_is_trailers(value.as_bytes()) {
                continue;
            }
            fields.push(HpackHeader::new(
                name_bytes.to_vec(),
                value.as_bytes().to_vec(),
            ));
        }
        let mut block = Vec::new();
        self.encoder.encode(&fields, &mut block);
        self.writer
            .write_field_block(&mut self.out, stream_id, end_stream, &block);
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        if !self.out.is_empty() {
            tokio::io::AsyncWriteExt::write_all(&mut self.io, &self.out).await?;
            self.out.clear();
        }
        Ok(())
    }
}

/// A boxed response body: the handler's body type is erased at the
/// stream boundary so `Connection` stays monomorphic.
struct ConnBody {
    inner: Pin<Box<dyn Body<Data = Bytes, Error = std::io::Error> + Send>>,
}

impl ConnBody {
    fn new<ResB, ResBE>(body: ResB) -> Self
    where
        ResB: Body<Data = Bytes, Error = ResBE> + Send + 'static,
        ResBE: std::error::Error + Send + Sync + 'static,
    {
        ConnBody {
            inner: Box::pin(BodyAdapter(Some(Box::pin(body)))),
        }
    }
}

impl Body for ConnBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.inner.as_mut().poll_frame(cx)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Adapts an arbitrary body whose error is not `io::Error` into one
/// that is (the stream layer only deals in `io::Error`).
struct BodyAdapter<ResB>(Option<Pin<Box<ResB>>>);

impl<ResB, ResBE> Body for BodyAdapter<ResB>
where
    ResB: Body<Data = Bytes, Error = ResBE>,
    ResBE: std::error::Error + Send + Sync + 'static,
{
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let Some(inner) = this.0.as_mut() else {
            return Poll::Ready(None);
        };
        match inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(std::io::Error::other(error)))),
            Poll::Ready(None) => {
                this.0 = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match &self.0 {
            Some(body) => body.size_hint(),
            None => http_body::SizeHint::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamDataState {
    Idle,
    Closed,
    Bad,
    Gone,
    Ok,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h2::codec::Setting;

    /// Runs a connection against a scripted peer over an in-memory
    /// duplex stream and collects the server's reply bytes.
    ///
    /// The preface timeout needs the vibeio timer, which does not run
    /// under a plain tokio test runtime (same pattern as the h1
    /// slowloris test), so a vibeio runtime is built per call.
    fn run_connection(preface: &[u8], frames: &[u8], preface_timeout: Option<Duration>) -> Vec<u8> {
        let script: Vec<u8> = [preface, frames].concat();
        vibeio::RuntimeBuilder::new()
            .enable_timer(true)
            .build()
            .unwrap()
            .block_on(async move {
                let (client_end, server_end) = tokio::io::duplex(1 << 16);
                let script = script;

                let server = vibeio::spawn(async move {
                    let conn = Connection::new(server_end, preface_timeout);
                    let _ = conn.drive().await;
                });

                let mut client = client_end;
                tokio::io::AsyncWriteExt::write_all(&mut client, &script)
                    .await
                    .expect("write script");
                vibeio::time::sleep(Duration::from_millis(50)).await;

                let mut reply = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    // The server answers in one burst and then waits for
                    // our EOF; give up after a short idle gap instead of
                    // blocking on the half-open duplex.
                    let read = vibeio::time::timeout(
                        Duration::from_millis(100),
                        tokio::io::AsyncReadExt::read(&mut client, &mut buf),
                    )
                    .await;
                    match read {
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                        Ok(Ok(n)) => reply.extend_from_slice(&buf[..n]),
                    }
                }
                // Close our half so the server sees EOF and exits.
                drop(client);
                vibeio::time::timeout(Duration::from_secs(2), server)
                    .await
                    .expect("server did not finish");
                reply
            })
    }

    fn decode_frames(wire: &[u8]) -> Vec<Frame> {
        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        decoder.extend(wire);
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame().expect("reply decode") {
            frames.push(frame);
        }
        frames
    }

    fn client_script(writer: impl FnOnce(&mut FrameWriter, &mut Vec<u8>)) -> Vec<u8> {
        let mut script = Vec::new();
        FrameWriter::new(DEFAULT_MAX_FRAME_SIZE).write_settings(&mut script, &[]);
        writer(&mut FrameWriter::new(DEFAULT_MAX_FRAME_SIZE), &mut script);
        script
    }

    #[test]
    fn valid_preface_completes_handshake() {
        let reply = run_connection(
            CLIENT_PREFACE,
            &client_script(|_, _| {}),
            Some(Duration::from_secs(5)),
        );
        let decoded = decode_frames(&reply);

        // The server's connection preface: its own SETTINGS.
        assert!(matches!(
            decoded.first(),
            Some(Frame::Settings { ack: false, .. })
        ));
        // The client's SETTINGS is acknowledged.
        assert!(decoded
            .iter()
            .any(|f| matches!(f, Frame::Settings { ack: true, .. })));
    }

    #[test]
    fn invalid_preface_answers_goaway_protocol_error() {
        let reply = run_connection(
            b"INVALID CONNECTION PREFACE!!",
            &[],
            Some(Duration::from_secs(5)),
        );
        let decoded = decode_frames(&reply);
        assert_eq!(decoded.len(), 1);
        assert!(matches!(
            &decoded[0],
            Frame::GoAway {
                error_code: 0x01,
                ..
            }
        ));
    }

    #[test]
    fn settings_are_applied_and_acked() {
        let reply = run_connection(
            CLIENT_PREFACE,
            &client_script(|writer, script| {
                writer.write_settings(
                    script,
                    &[
                        Setting {
                            id: 0x01,
                            value: 16_384,
                        },
                        Setting { id: 0x02, value: 0 },
                        Setting {
                            id: 0x04,
                            value: 1_048_576,
                        },
                        Setting {
                            id: 0x05,
                            value: 32_768,
                        },
                        Setting {
                            id: 0x06,
                            value: 65_536,
                        },
                        Setting { id: 0x63, value: 7 }, // unknown: ignored
                    ],
                );
            }),
            Some(Duration::from_secs(5)),
        );
        let decoded = decode_frames(&reply);
        // One ACK per non-ACK SETTINGS frame, in order.
        let acks: Vec<&Frame> = decoded
            .iter()
            .filter(|f| matches!(f, Frame::Settings { ack: true, .. }))
            .collect();
        assert_eq!(acks.len(), 2);
    }

    #[test]
    fn settings_ack_with_payload_is_connection_error() {
        // SETTINGS with the ACK flag and a non-empty payload: the codec
        // rejects it and the connection reports FRAME_SIZE_ERROR.
        // Payload: one Setting { id: 0x04, value: 0 }.
        let bad = [
            0x00, 0x00, 0x06, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
            0x00,
        ];
        let mut frames = client_script(|_, _| {});
        frames.extend_from_slice(&bad);
        let reply = run_connection(CLIENT_PREFACE, &frames, Some(Duration::from_secs(5)));
        let decoded = decode_frames(&reply);
        assert!(matches!(
            decoded.last().and_then(|f| match f {
                Frame::GoAway { error_code, .. } => Some(*error_code),
                _ => None,
            }),
            Some(0x06) // FRAME_SIZE_ERROR
        ));
    }

    #[test]
    fn ping_is_echoed() {
        let reply = run_connection(
            CLIENT_PREFACE,
            &client_script(|writer, script| {
                writer.write_ping(script, &[1, 2, 3, 4, 5, 6, 7, 8]);
            }),
            Some(Duration::from_secs(5)),
        );
        let decoded = decode_frames(&reply);
        assert!(decoded.iter().any(|f| matches!(
            f,
            Frame::Ping { ack: true, payload } if *payload == [1, 2, 3, 4, 5, 6, 7, 8]
        )));
    }

    #[test]
    fn goaway_from_peer_ends_connection() {
        let mut script = client_script(|_, _| {});
        FrameWriter::new(DEFAULT_MAX_FRAME_SIZE).write_goaway(&mut script, 0, 0x00, b"bye");

        let reply = run_connection(CLIENT_PREFACE, &script, Some(Duration::from_secs(5)));
        let decoded = decode_frames(&reply);
        // The peer's GOAWAY draws no reply beyond the SETTINGS
        // exchange; the server closes quietly.
        assert!(!decoded.iter().any(|f| matches!(f, Frame::GoAway { .. })));
    }
}
