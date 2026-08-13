//! HTTP/2 frame codec (RFC 9113 Section 6).
//!
//! Parses and writes the 9-octet frame header and every frame type:
//! DATA, HEADERS, PRIORITY, RST_STREAM, SETTINGS, PUSH_PROMISE, PING,
//! GOAWAY, WINDOW_UPDATE and CONTINUATION, including PADDED and
//! PRIORITY flags. Validation covers frame-size limits, stream-id rules,
//! settings values and field-block continuation discipline. Unknown
//! frame types are ignored (RFC 9113 Section 4.1).
//!
//! The decoder is incremental: feed it bytes with [`FrameDecoder::extend`]
//! and call [`FrameDecoder::next_frame`]; it returns `Ok(None)` until a
//! complete frame is buffered.
//!
//! This module is the frame layer of the native HTTP/2 implementation
//! (see CUSTOM_HTTP2_IMPL.md); stream and connection semantics
//! (flow control, settings tracking, lifecycle) live in later steps on
//! top of it.

use super::error::{H2Error, Reason};
use bytes::{Bytes, BytesMut};

/// Length of the fixed frame header (RFC 9113 Section 4.1).
pub const FRAME_HEADER_LEN: usize = 9;
/// The initial maximum frame payload size (RFC 9113 Section 4.2).
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16_384;
/// The largest settable frame payload size (2^24-1).
pub const MAX_FRAME_SIZE_LIMIT: usize = 16_777_215;
/// The initial connection flow-control window (RFC 9113 Section 5.2.1).
pub const DEFAULT_INITIAL_WINDOW_SIZE: u32 = 1_048_576;
/// The largest legal flow-control window (2^31-1).
pub const MAX_WINDOW_SIZE: u32 = 2_147_483_647;
/// The HTTP/2 connection preface (RFC 9113 Section 3.5).
pub const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

const DATA_TYPE: u8 = 0x00;
const HEADERS_TYPE: u8 = 0x01;
const PRIORITY_TYPE: u8 = 0x02;
const RST_STREAM_TYPE: u8 = 0x03;
const SETTINGS_TYPE: u8 = 0x04;
const PUSH_PROMISE_TYPE: u8 = 0x05;
const PING_TYPE: u8 = 0x06;
const GOAWAY_TYPE: u8 = 0x07;
const WINDOW_UPDATE_TYPE: u8 = 0x08;
const CONTINUATION_TYPE: u8 = 0x09;

/// Flag bits shared across frame types (RFC 9113 Section 6).
const FLAG_END_STREAM: u8 = 0x01;
const FLAG_ACK: u8 = 0x01;
const FLAG_END_HEADERS: u8 = 0x04;
const FLAG_PADDED: u8 = 0x08;
const FLAG_PRIORITY: u8 = 0x20;

/// Stream priority (RFC 9113 Section 6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Priority {
    pub exclusive: bool,
    pub dependency: u32,
    pub weight: u8,
}

/// A SETTINGS parameter (RFC 9113 Section 6.5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    pub id: u16,
    pub value: u32,
}

/// A parsed frame. Payloads are zero-copy views over the decoder's
/// buffer (kept alive by `Bytes` refcounting until the frame is
/// dropped); the connection layer reassembles field blocks from the
/// `block` fragments and applies stream semantics. Padding, when
/// present, is validated and stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data {
        stream_id: u32,
        end_stream: bool,
        data: Bytes,
    },
    Headers {
        stream_id: u32,
        end_stream: bool,
        end_headers: bool,
        priority: Option<Priority>,
        block: Bytes,
    },
    Priority {
        stream_id: u32,
        priority: Priority,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Settings {
        ack: bool,
        settings: Vec<Setting>,
    },
    PushPromise {
        stream_id: u32,
        end_headers: bool,
        promised_stream_id: u32,
        block: Bytes,
    },
    Ping {
        ack: bool,
        payload: [u8; 8],
    },
    GoAway {
        last_stream_id: u32,
        error_code: u32,
        debug: Bytes,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    Continuation {
        stream_id: u32,
        end_headers: bool,
        block: Bytes,
    },
    /// A frame type this implementation does not understand; ignored by
    /// the connection (RFC 9113 Section 4.1).
    Unknown {
        typ: u8,
        flags: u8,
        stream_id: u32,
        payload: Bytes,
    },
}

/// Incremental frame decoder over a growing buffer.
#[derive(Debug)]
pub struct FrameDecoder {
    buf: BytesMut,
    max_frame_size: usize,
    /// The stream whose field block is open (HEADERS/PUSH_PROMISE
    /// without END_HEADERS was seen); only CONTINUATION frames for that
    /// stream may follow.
    block_stream: Option<u32>,
}

impl FrameDecoder {
    /// Creates a decoder enforcing the given maximum frame payload
    /// size.
    #[inline]
    pub fn new(max_frame_size: usize) -> FrameDecoder {
        FrameDecoder {
            buf: BytesMut::new(),
            max_frame_size,
            block_stream: None,
        }
    }

    /// Appends bytes to the decode buffer.
    #[inline]
    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The maximum frame payload size the peer may send.
    #[inline]
    pub fn max_frame_size(&self) -> usize {
        self.max_frame_size
    }

    /// Adjusts the frame-size limit after SETTINGS_MAX_FRAME_SIZE.
    #[inline]
    pub fn set_max_frame_size(&mut self, max_frame_size: usize) {
        self.max_frame_size = max_frame_size;
    }

    /// The stream with an open field block, if any.
    #[inline]
    pub fn block_stream(&self) -> Option<u32> {
        self.block_stream
    }

    /// Parses the next complete frame. Returns `Ok(None)` when more
    /// bytes are needed.
    #[inline]
    pub fn next_frame(&mut self) -> Result<Option<Frame>, H2Error> {
        if self.buf.len() < FRAME_HEADER_LEN {
            return Ok(None);
        }

        // 24-bit big-endian payload length. The zero padding leads the
        // three length octets.
        let payload_len = u32::from_be_bytes([0, self.buf[0], self.buf[1], self.buf[2]]) as usize;
        let typ = self.buf[3];
        let flags = self.buf[4];
        let raw_stream_id =
            u32::from_be_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]]);
        // RFC 9113 Section 4.1: the high bit of the stream identifier
        // field is reserved and MUST be ignored, not treated as an
        // error.
        let stream_id = raw_stream_id & 0x7fff_ffff;

        if payload_len > self.max_frame_size {
            return Err(H2Error::frame_size(
                "frame payload exceeds SETTINGS_MAX_FRAME_SIZE",
            ));
        }

        let total = FRAME_HEADER_LEN + payload_len;
        if self.buf.len() < total {
            return Ok(None);
        }

        let _header = self.buf.split_to(FRAME_HEADER_LEN);
        let payload = self.buf.split_to(payload_len).freeze();
        let frame = parse_frame(typ, flags, stream_id, payload, self)?;
        Ok(Some(frame))
    }
}

/// Parses one complete frame payload (after the 9-octet header).
#[inline]
fn parse_frame(
    typ: u8,
    flags: u8,
    stream_id: u32,
    payload: Bytes,
    decoder: &mut FrameDecoder,
) -> Result<Frame, H2Error> {
    let mut body = &payload[..];

    // Field-block continuation discipline (RFC 9113 Section 6.10).
    if let Some(open_stream) = decoder.block_stream {
        if typ != CONTINUATION_TYPE {
            return Err(H2Error::protocol(
                "frame received while field block was open",
            ));
        }
        if stream_id != open_stream {
            return Err(H2Error::protocol(
                "CONTINUATION frame on a different stream than the open field block",
            ));
        }
    } else if typ == CONTINUATION_TYPE {
        return Err(H2Error::protocol(
            "CONTINUATION frame without a preceding HEADERS or PUSH_PROMISE",
        ));
    }

    let frame = match typ {
        DATA_TYPE => {
            require_stream(stream_id)?;
            let end_stream = flags & FLAG_END_STREAM != 0;
            let pad_len = take_padding(&mut body, flags & FLAG_PADDED != 0)?;
            Frame::Data {
                stream_id,
                end_stream,
                data: payload_slice(&payload, body, pad_len),
            }
        }
        HEADERS_TYPE => {
            require_stream(stream_id)?;
            let end_stream = flags & FLAG_END_STREAM != 0;
            let end_headers = flags & FLAG_END_HEADERS != 0;
            let pad_len = take_padding(&mut body, flags & FLAG_PADDED != 0)?;
            let priority = if flags & FLAG_PRIORITY != 0 {
                Some(read_priority(&mut body, stream_id)?)
            } else {
                None
            };
            if !end_headers {
                decoder.block_stream = Some(stream_id);
            }
            Frame::Headers {
                stream_id,
                end_stream,
                end_headers,
                priority,
                block: payload_slice(&payload, body, pad_len),
            }
        }
        PRIORITY_TYPE => {
            require_stream(stream_id)?;
            if body.len() != 5 {
                return Err(H2Error::frame_size(
                    "PRIORITY frame payload must be exactly 5 octets",
                ));
            }
            let priority = read_priority(&mut body, stream_id)?;
            Frame::Priority {
                stream_id,
                priority,
            }
        }
        RST_STREAM_TYPE => {
            require_stream(stream_id)?;
            if body.len() != 4 {
                return Err(H2Error::frame_size(
                    "RST_STREAM frame payload must be exactly 4 octets",
                ));
            }
            Frame::Reset {
                stream_id,
                error_code: read_u32(body),
            }
        }
        SETTINGS_TYPE => {
            if stream_id != 0 {
                return Err(H2Error::protocol(
                    "SETTINGS frame must use stream identifier 0",
                ));
            }
            let ack = flags & FLAG_ACK != 0;
            if ack && !body.is_empty() {
                return Err(H2Error::frame_size(
                    "SETTINGS frame with ACK flag must have an empty payload",
                ));
            }
            if !body.len().is_multiple_of(6) {
                return Err(H2Error::frame_size(
                    "SETTINGS frame payload must be a multiple of 6 octets",
                ));
            }
            let mut settings = Vec::with_capacity(body.len() / 6);
            while !body.is_empty() {
                let id = u16::from_be_bytes([body[0], body[1]]);
                let value = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                validate_setting(id, value)?;
                settings.push(Setting { id, value });
                body = &body[6..];
            }
            // The peer's announced SETTINGS_MAX_FRAME_SIZE governs what it
            // may send; adopt it immediately so later frames in this
            // buffer are checked against it.
            if !ack {
                for setting in &settings {
                    if setting.id == 0x05 {
                        decoder.set_max_frame_size(setting.value as usize);
                    }
                }
            }
            Frame::Settings { ack, settings }
        }
        PUSH_PROMISE_TYPE => {
            require_stream(stream_id)?;
            let end_headers = flags & FLAG_END_HEADERS != 0;
            let pad_len = take_padding(&mut body, flags & FLAG_PADDED != 0)?;
            if body.len() < 4 {
                return Err(H2Error::frame_size(
                    "PUSH_PROMISE frame payload must be at least 4 octets",
                ));
            }
            let promised_stream_id = read_u32(&body[..4]) & 0x7fff_ffff;
            if promised_stream_id == 0 {
                return Err(H2Error::protocol(
                    "PUSH_PROMISE promised stream identifier is 0",
                ));
            }
            if !end_headers {
                decoder.block_stream = Some(stream_id);
            }
            body = &body[4..];
            Frame::PushPromise {
                stream_id,
                end_headers,
                promised_stream_id,
                block: payload_slice(&payload, body, pad_len),
            }
        }
        PING_TYPE => {
            if stream_id != 0 {
                return Err(H2Error::protocol("PING frame must use stream identifier 0"));
            }
            if body.len() != 8 {
                return Err(H2Error::frame_size(
                    "PING frame payload must be exactly 8 octets",
                ));
            }
            Frame::Ping {
                ack: flags & FLAG_ACK != 0,
                payload: body[..8]
                    .try_into()
                    .expect("PING payload is exactly 8 octets (validated above)"),
            }
        }
        GOAWAY_TYPE => {
            if stream_id != 0 {
                return Err(H2Error::protocol(
                    "GOAWAY frame must use stream identifier 0",
                ));
            }
            if body.len() < 8 {
                return Err(H2Error::frame_size(
                    "GOAWAY frame payload must be at least 8 octets",
                ));
            }
            let last_stream_id = read_u32(&body[..4]) & 0x7fff_ffff;
            let error_code = read_u32(&body[4..8]);
            Frame::GoAway {
                last_stream_id,
                error_code,
                debug: payload.slice(8..),
            }
        }
        WINDOW_UPDATE_TYPE => {
            if body.len() != 4 {
                return Err(H2Error::frame_size(
                    "WINDOW_UPDATE frame payload must be exactly 4 octets",
                ));
            }
            let increment = read_u32(body) & 0x7fff_ffff;
            if increment == 0 {
                return Err(H2Error::protocol("WINDOW_UPDATE frame with zero increment"));
            }
            Frame::WindowUpdate {
                stream_id,
                increment,
            }
        }
        CONTINUATION_TYPE => {
            let end_headers = flags & FLAG_END_HEADERS != 0;
            if end_headers {
                decoder.block_stream = None;
            }
            Frame::Continuation {
                stream_id,
                end_headers,
                block: payload,
            }
        }
        _ => Frame::Unknown {
            typ,
            flags,
            stream_id,
            payload,
        },
    };

    Ok(frame)
}

/// A zero-copy view of `payload` covering exactly the range `body`
/// points at: the leading octets were trimmed by padding/priority
/// fields and `pad_len` trailing octets by padding.
#[inline]
fn payload_slice(payload: &Bytes, body: &[u8], pad_len: usize) -> Bytes {
    let start = payload.len() - pad_len - body.len();
    payload.slice(start..start + body.len())
}

/// Reads the pad-length octet when `padded` is set and strips that many
/// trailing octets from `body`, returning the padding length.
///
/// The padding must be strictly shorter than the payload (RFC 9113
/// Section 6.1).
#[inline]
fn take_padding(body: &mut &[u8], padded: bool) -> Result<usize, H2Error> {
    if !padded {
        return Ok(0);
    }
    let pad_len = *body
        .first()
        .ok_or_else(|| H2Error::frame_size("PADDED frame payload too short for pad length"))?
        as usize;
    if pad_len >= body.len() {
        return Err(H2Error::protocol(
            "padding length is the length of the frame payload or greater",
        ));
    }
    *body = &body[1..body.len() - pad_len];
    Ok(pad_len)
}

/// Reads the 5-octet priority fields (Exclusive + Stream Dependency +
/// Weight).
#[inline]
fn read_priority(body: &mut &[u8], stream_id: u32) -> Result<Priority, H2Error> {
    if body.len() < 5 {
        return Err(H2Error::frame_size(
            "frame payload too short for priority fields",
        ));
    }
    let exclusive = body[0] & 0x80 != 0;
    let dependency = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) & 0x7fff_ffff;
    if dependency == stream_id {
        return Err(H2Error::protocol(
            "stream priority depends on its own stream identifier",
        ));
    }
    let weight = body[4];
    *body = &body[5..];
    Ok(Priority {
        exclusive,
        dependency,
        weight,
    })
}

#[inline]
fn require_stream(stream_id: u32) -> Result<(), H2Error> {
    if stream_id == 0 {
        Err(H2Error::new(
            Reason::ProtocolError,
            "frame must use a non-zero stream identifier",
        ))
    } else {
        Ok(())
    }
}

/// Validates the value of a known SETTINGS parameter (RFC 9113
/// Section 6.5.2). Unknown identifiers are accepted and ignored by the
/// caller.
#[inline]
fn validate_setting(id: u16, value: u32) -> Result<(), H2Error> {
    match id {
        0x02 => {
            // SETTINGS_ENABLE_PUSH.
            if value > 1 {
                return Err(H2Error::protocol("SETTINGS_ENABLE_PUSH must be 0 or 1"));
            }
        }
        0x04 => {
            // SETTINGS_INITIAL_WINDOW_SIZE.
            if value > MAX_WINDOW_SIZE {
                return Err(H2Error::new(
                    Reason::FlowControlError,
                    "SETTINGS_INITIAL_WINDOW_SIZE exceeds 2^31-1",
                ));
            }
        }
        0x05 if !(DEFAULT_MAX_FRAME_SIZE..=MAX_FRAME_SIZE_LIMIT).contains(&(value as usize)) => {
            // SETTINGS_MAX_FRAME_SIZE.
            return Err(H2Error::protocol(
                "SETTINGS_MAX_FRAME_SIZE outside 16384..16777215",
            ));
        }
        _ => {}
    }
    Ok(())
}

#[inline]
fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Writes frames to an output buffer. The field-block writer
/// ([`FrameWriter::write_field_block`]) splits the block across
/// HEADERS/CONTINUATION frames at the peer's frame-size limit.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameWriter {
    /// The maximum payload size the peer accepts (the peer's
    /// SETTINGS_MAX_FRAME_SIZE; 16384 by default).
    pub max_frame_size: usize,
}

impl FrameWriter {
    pub const fn new(max_frame_size: usize) -> FrameWriter {
        FrameWriter { max_frame_size }
    }

    #[inline]
    fn header(out: &mut Vec<u8>, payload_len: usize, typ: u8, flags: u8, stream_id: u32) {
        out.push(((payload_len >> 16) & 0xff) as u8);
        out.push(((payload_len >> 8) & 0xff) as u8);
        out.push((payload_len & 0xff) as u8);
        out.push(typ);
        out.push(flags);
        out.push(((stream_id >> 24) & 0x7f) as u8);
        out.push(((stream_id >> 16) & 0xff) as u8);
        out.push(((stream_id >> 8) & 0xff) as u8);
        out.push((stream_id & 0xff) as u8);
    }

    #[inline]
    pub fn write_data(&self, out: &mut Vec<u8>, stream_id: u32, end_stream: bool, data: &[u8]) {
        let flags = if end_stream { FLAG_END_STREAM } else { 0 };
        FrameWriter::header(out, data.len(), DATA_TYPE, flags, stream_id);
        out.extend_from_slice(data);
    }

    /// Writes a HEADERS frame (single frame, no field-block splitting).
    #[inline]
    pub fn write_headers(
        &self,
        out: &mut Vec<u8>,
        stream_id: u32,
        end_stream: bool,
        end_headers: bool,
        priority: Option<Priority>,
        block: &[u8],
    ) {
        let mut flags = 0;
        if end_stream {
            flags |= FLAG_END_STREAM;
        }
        if end_headers {
            flags |= FLAG_END_HEADERS;
        }
        let extra = if priority.is_some() { 5 } else { 0 };
        if extra != 0 {
            flags |= FLAG_PRIORITY;
        }
        FrameWriter::header(out, block.len() + extra, HEADERS_TYPE, flags, stream_id);
        if let Some(p) = priority {
            let dep = p.dependency & 0x7fff_ffff | if p.exclusive { 0x8000_0000 } else { 0 };
            out.extend_from_slice(&dep.to_be_bytes());
            out.push(p.weight);
        }
        out.extend_from_slice(block);
    }

    /// Writes a field block as a HEADERS frame (optionally with
    /// END_STREAM) followed by as many CONTINUATION frames as needed to
    /// stay within the peer's frame-size limit.
    #[inline]
    pub fn write_field_block(
        &self,
        out: &mut Vec<u8>,
        stream_id: u32,
        end_stream: bool,
        block: &[u8],
    ) {
        let capacity = self.max_frame_size;
        if block.len() <= capacity {
            self.write_headers(out, stream_id, end_stream, true, None, block);
            return;
        }
        let first = &block[..capacity];
        self.write_headers(out, stream_id, end_stream, false, None, first);
        let mut rest = &block[capacity..];
        while rest.len() > capacity {
            self.write_continuation(out, stream_id, false, &rest[..capacity]);
            rest = &rest[capacity..];
        }
        self.write_continuation(out, stream_id, true, rest);
    }

    #[inline]
    pub fn write_priority(&self, out: &mut Vec<u8>, stream_id: u32, priority: Priority) {
        let dep =
            priority.dependency & 0x7fff_ffff | if priority.exclusive { 0x8000_0000 } else { 0 };
        FrameWriter::header(out, 5, PRIORITY_TYPE, 0, stream_id);
        out.extend_from_slice(&dep.to_be_bytes());
        out.push(priority.weight);
    }

    #[inline]
    pub fn write_reset(&self, out: &mut Vec<u8>, stream_id: u32, error_code: u32) {
        FrameWriter::header(out, 4, RST_STREAM_TYPE, 0, stream_id);
        out.extend_from_slice(&error_code.to_be_bytes());
    }

    #[inline]
    pub fn write_settings(&self, out: &mut Vec<u8>, settings: &[Setting]) {
        FrameWriter::header(out, settings.len() * 6, SETTINGS_TYPE, 0, 0);
        for setting in settings {
            out.extend_from_slice(&setting.id.to_be_bytes());
            out.extend_from_slice(&setting.value.to_be_bytes());
        }
    }

    #[inline]
    pub fn write_settings_ack(&self, out: &mut Vec<u8>) {
        FrameWriter::header(out, 0, SETTINGS_TYPE, FLAG_ACK, 0);
    }

    #[inline]
    pub fn write_push_promise(
        &self,
        out: &mut Vec<u8>,
        stream_id: u32,
        promised_stream_id: u32,
        block: &[u8],
    ) {
        FrameWriter::header(
            out,
            4 + block.len(),
            PUSH_PROMISE_TYPE,
            FLAG_END_HEADERS,
            stream_id,
        );
        out.extend_from_slice(&(promised_stream_id & 0x7fff_ffff).to_be_bytes());
        out.extend_from_slice(block);
    }

    #[inline]
    pub fn write_ping(&self, out: &mut Vec<u8>, payload: &[u8; 8]) {
        FrameWriter::header(out, 8, PING_TYPE, 0, 0);
        out.extend_from_slice(payload);
    }

    #[inline]
    pub fn write_ping_ack(&self, out: &mut Vec<u8>, payload: &[u8; 8]) {
        FrameWriter::header(out, 8, PING_TYPE, FLAG_ACK, 0);
        out.extend_from_slice(payload);
    }

    #[inline]
    pub fn write_goaway(
        &self,
        out: &mut Vec<u8>,
        last_stream_id: u32,
        error_code: u32,
        debug: &[u8],
    ) {
        FrameWriter::header(out, 8 + debug.len(), GOAWAY_TYPE, 0, 0);
        out.extend_from_slice(&(last_stream_id & 0x7fff_ffff).to_be_bytes());
        out.extend_from_slice(&error_code.to_be_bytes());
        out.extend_from_slice(debug);
    }

    #[inline]
    pub fn write_window_update(&self, out: &mut Vec<u8>, stream_id: u32, increment: u32) {
        FrameWriter::header(out, 4, WINDOW_UPDATE_TYPE, 0, stream_id);
        out.extend_from_slice(&(increment & 0x7fff_ffff).to_be_bytes());
    }

    #[inline]
    pub fn write_continuation(
        &self,
        out: &mut Vec<u8>,
        stream_id: u32,
        end_headers: bool,
        block: &[u8],
    ) {
        let flags = if end_headers { FLAG_END_HEADERS } else { 0 };
        FrameWriter::header(out, block.len(), CONTINUATION_TYPE, flags, stream_id);
        out.extend_from_slice(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline]
    fn hex_to_bytes(hex: impl AsRef<str>) -> Vec<u8> {
        hex.as_ref()
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let hi = (pair[0] as char).to_digit(16).unwrap() as u8;
                let lo = (pair[1] as char).to_digit(16).unwrap() as u8;
                (hi << 4) | lo
            })
            .collect()
    }

    #[inline]
    fn decode_all(wire: &[u8]) -> Result<Vec<Frame>, H2Error> {
        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        decoder.extend(wire);
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame()? {
            frames.push(frame);
        }
        Ok(frames)
    }

    #[inline]
    fn decode_one(wire: &[u8]) -> Result<Frame, H2Error> {
        let mut frames = decode_all(wire)?;
        assert_eq!(frames.len(), 1);
        Ok(frames.remove(0))
    }

    #[test]
    fn preface_bytes() {
        assert_eq!(
            CLIENT_PREFACE,
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".as_slice()
        );
    }

    #[test]
    fn writer_data_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_data(&mut out, 1, true, &[0xab, 0xcd]);
        assert_eq!(out, hex_to_bytes("000002000100000001abcd"));
    }

    #[test]
    fn writer_headers_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_headers(&mut out, 3, false, true, None, &[0x88, 0x84]);
        assert_eq!(out, hex_to_bytes("0000020104000000038884"));
    }

    #[test]
    fn writer_headers_with_priority_byte_exact() {
        let mut out = Vec::new();
        let priority = Priority {
            exclusive: true,
            dependency: 3,
            weight: 200,
        };
        FrameWriter::new(16384).write_headers(&mut out, 1, false, true, Some(priority), &[0x88]);
        // flags = END_HEADERS | PRIORITY = 0x24; payload = 80 00 00 03 (E, dep 3) c8 88
        assert_eq!(out, hex_to_bytes("00000601240000000180000003c888"));
    }

    #[test]
    fn writer_priority_byte_exact() {
        let mut out = Vec::new();
        let priority = Priority {
            exclusive: false,
            dependency: 4,
            weight: 22,
        };
        FrameWriter::new(16384).write_priority(&mut out, 5, priority);
        assert_eq!(out, hex_to_bytes("0000050200000000050000000416"));
    }

    #[test]
    fn writer_reset_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_reset(&mut out, 7, 0x06);
        assert_eq!(out, hex_to_bytes("00000403000000000700000006"));
    }

    #[test]
    fn writer_settings_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_settings(
            &mut out,
            &[
                Setting {
                    id: 0x04,
                    value: 1024,
                },
                Setting {
                    id: 0x01,
                    value: 4096,
                },
            ],
        );
        assert_eq!(
            out,
            hex_to_bytes("00000c040000000000000400000400000100001000")
        );
    }

    #[test]
    fn writer_settings_ack_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_settings_ack(&mut out);
        assert_eq!(out, hex_to_bytes("000000040100000000"));
    }

    #[test]
    fn writer_ping_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_ping(&mut out, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(out, hex_to_bytes("0000080600000000000102030405060708"));
        let mut ack = Vec::new();
        FrameWriter::new(16384).write_ping_ack(&mut ack, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(ack, hex_to_bytes("0000080601000000000102030405060708"));
    }

    #[test]
    fn writer_goaway_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_goaway(&mut out, 13, 0, b"bye");
        assert_eq!(
            out,
            hex_to_bytes("00000b0700000000000000000d00000000627965")
        );
    }

    #[test]
    fn writer_window_update_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_window_update(&mut out, 0, 65_535);
        assert_eq!(out, hex_to_bytes("0000040800000000000000ffff"));
    }

    #[test]
    fn writer_continuation_byte_exact() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_continuation(&mut out, 3, false, &[0x88]);
        assert_eq!(out, hex_to_bytes("00000109000000000388"));
        let mut end = Vec::new();
        FrameWriter::new(16384).write_continuation(&mut end, 3, true, &[0x88]);
        assert_eq!(end, hex_to_bytes("00000109040000000388"));
    }

    #[test]
    fn writer_field_block_splits_at_frame_limit() {
        let limit = 32;
        let block: Vec<u8> = (0..100).map(|i| i as u8).collect();
        let mut out = Vec::new();
        FrameWriter::new(limit).write_field_block(&mut out, 9, true, &block);

        let frames = decode_all(&out).unwrap();
        assert_eq!(frames.len(), 4);
        match &frames[0] {
            Frame::Headers {
                stream_id,
                end_stream,
                end_headers,
                block: first,
                ..
            } => {
                assert_eq!(*stream_id, 9);
                assert!(*end_stream);
                assert!(!*end_headers);
                assert_eq!(&block[..limit], first);
            }
            other => panic!("unexpected first frame {other:?}"),
        }
        for (i, frame) in frames[1..].iter().enumerate() {
            let (end_headers, cont_block) = match frame {
                Frame::Continuation {
                    end_headers, block, ..
                } => (*end_headers, block),
                other => panic!("unexpected continuation {other:?}"),
            };
            assert_eq!(end_headers, i == 2, "only last CONTINUATION ends headers");
            let start = limit + i * limit;
            let end = (start + limit).min(block.len());
            assert_eq!(cont_block, &block[start..end]);
        }
    }

    #[test]
    fn writer_field_block_single_frame_when_small() {
        let mut out = Vec::new();
        FrameWriter::new(16384).write_field_block(&mut out, 1, false, &[0x88]);
        let frames = decode_all(&out).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            frames[0],
            Frame::Headers {
                end_headers: true,
                ..
            }
        ));
    }

    #[test]
    fn decode_settings() {
        let frame =
            decode_one(&hex_to_bytes("00000c040000000000000400000400000100001000")).unwrap();
        assert_eq!(
            frame,
            Frame::Settings {
                ack: false,
                settings: vec![
                    Setting {
                        id: 0x04,
                        value: 1024
                    },
                    Setting {
                        id: 0x01,
                        value: 4096
                    },
                ],
            }
        );
    }

    #[test]
    fn decode_settings_ack() {
        let frame = decode_one(&hex_to_bytes("000000040100000000")).unwrap();
        assert_eq!(
            frame,
            Frame::Settings {
                ack: true,
                settings: vec![],
            }
        );
    }

    #[test]
    fn settings_ack_with_payload_is_frame_size_error() {
        assert_eq!(
            decode_one(&hex_to_bytes("000006040100000000000100000400"))
                .unwrap_err()
                .reason,
            Reason::FrameSizeError
        );
    }

    #[test]
    fn settings_nonzero_stream_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("000000040000000001")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn settings_length_not_multiple_of_six_is_frame_size_error() {
        let err = decode_one(&hex_to_bytes("00000704000000000000010000040000")).unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
    }

    #[test]
    fn settings_enable_push_out_of_range_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("000006040000000000000200000002")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn settings_initial_window_too_large_is_flow_control_error() {
        let err = decode_one(&hex_to_bytes("000006040000000000000480000000")).unwrap_err();
        assert_eq!(err.reason, Reason::FlowControlError);
    }

    #[test]
    fn settings_initial_window_at_limit_is_accepted() {
        let frame = decode_one(&hex_to_bytes("00000604000000000000047fffffff")).unwrap();
        assert!(matches!(frame, Frame::Settings { .. }));
    }

    #[test]
    fn settings_max_frame_size_out_of_range_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("000006040000000000000500003fff")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
        let ok = decode_one(&hex_to_bytes("000006040000000000000500ffffff")).unwrap();
        assert!(matches!(ok, Frame::Settings { .. }));
    }

    #[test]
    fn unknown_setting_is_accepted_and_parsed() {
        let frame = decode_one(&hex_to_bytes("000006040000000000009900005abc")).unwrap();
        assert_eq!(
            frame,
            Frame::Settings {
                ack: false,
                settings: vec![Setting {
                    id: 0x99,
                    value: 0x5abc
                }],
            }
        );
    }

    #[test]
    fn decode_ping_with_ack_flag() {
        let frame = decode_one(&hex_to_bytes("0000080601000000000102030405060708")).unwrap();
        assert_eq!(
            frame,
            Frame::Ping {
                ack: true,
                payload: [1, 2, 3, 4, 5, 6, 7, 8],
            }
        );
    }

    #[test]
    fn ping_wrong_length_is_frame_size_error() {
        let err = decode_one(&hex_to_bytes("00000706000000000001020304050607")).unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
    }

    #[test]
    fn ping_nonzero_stream_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("0000080600000000010102030405060708")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn decode_goaway_with_debug_data() {
        let frame = decode_one(&hex_to_bytes("00000b0700000000000000000d00000000627965")).unwrap();
        assert_eq!(
            frame,
            Frame::GoAway {
                last_stream_id: 13,
                error_code: 0,
                debug: b"bye".as_slice().into(),
            }
        );
    }

    #[test]
    fn goaway_too_short_is_frame_size_error() {
        let err = decode_one(&hex_to_bytes("0000070700000000000000000d00000000")).unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
    }

    #[test]
    fn goaway_nonzero_stream_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("0000080700000000010000000d00000000")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn goaway_reserved_last_stream_bit_is_masked() {
        let frame = decode_one(&hex_to_bytes("0000080700000000008000000d00000000")).unwrap();
        assert_eq!(
            frame,
            Frame::GoAway {
                last_stream_id: 13,
                error_code: 0,
                debug: Bytes::new(),
            }
        );
    }

    #[test]
    fn decode_window_update() {
        let frame = decode_one(&hex_to_bytes("0000040800000000000000ffff")).unwrap();
        assert_eq!(
            frame,
            Frame::WindowUpdate {
                stream_id: 0,
                increment: 65_535,
            }
        );
    }

    #[test]
    fn window_update_zero_increment_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("00000408000000000000000000")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn window_update_reserved_increment_bit_is_masked() {
        let frame = decode_one(&hex_to_bytes("00000408000000000180000001")).unwrap();
        assert_eq!(
            frame,
            Frame::WindowUpdate {
                stream_id: 1,
                increment: 1,
            }
        );
    }

    #[test]
    fn window_update_wrong_length_is_frame_size_error() {
        let err = decode_one(&hex_to_bytes("000005080000000000000000ffff00")).unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
    }

    #[test]
    fn rst_stream_wrong_length_is_frame_size_error() {
        let err = decode_one(&hex_to_bytes("000003030000000007000000")).unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
    }

    #[test]
    fn rst_stream_zero_stream_id_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("00000403000000000000000000")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn data_on_stream_zero_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("00000200000000000000abcd")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn padded_data_is_stripped() {
        let frame = decode_one(&hex_to_bytes("000005000800000001026f6b0000")).unwrap();
        assert_eq!(
            frame,
            Frame::Data {
                stream_id: 1,
                end_stream: false,
                data: b"ok".as_slice().into(),
            }
        );
    }

    #[test]
    fn padding_length_not_less_than_payload_is_protocol_error() {
        // pad_len 5, payload 5 octets: padding not strictly shorter than
        // the payload.
        let err = decode_one(&hex_to_bytes("0000050008000000010500000000")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn padding_of_all_but_one_octet_is_legal() {
        // pad_len 5, payload 6 octets: the strict-lower-bound rule allows
        // the maximum padding with an empty data field.
        let frame = decode_one(&hex_to_bytes("000006000800000001050000000000")).unwrap();
        assert_eq!(
            frame,
            Frame::Data {
                stream_id: 1,
                end_stream: false,
                data: Bytes::new(),
            }
        );
    }

    #[test]
    fn padded_data_without_payload_is_frame_size_error() {
        // PADDED flag but no payload at all.
        let err = decode_one(&hex_to_bytes("000000000800000001")).unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
    }

    #[test]
    fn padded_headers_with_priority_decode() {
        // payload: 01 (pad len 1) | 80 00 00 03 (E, dep 3) | 88 (weight 136) |
        // 88 (block) | 00 (pad); flags 0x2c = END_HEADERS | PADDED | PRIORITY.
        let frame = decode_one(&hex_to_bytes("000008012c000000010180000003888800")).unwrap();
        assert_eq!(
            frame,
            Frame::Headers {
                stream_id: 1,
                end_stream: false,
                end_headers: true,
                priority: Some(Priority {
                    exclusive: true,
                    dependency: 3,
                    weight: 0x88,
                }),
                block: hex_to_bytes("88").into(),
            }
        );
    }

    #[test]
    fn headers_self_dependency_is_protocol_error() {
        // stream 3, HEADERS with PRIORITY depending on stream 3.
        let err = decode_one(&hex_to_bytes("000006012000000003000000030010")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn priority_frame_wrong_length_is_frame_size_error() {
        let err = decode_one(&hex_to_bytes("0000040200000000050000000310")).unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
        let err2 = decode_one(&hex_to_bytes("0000060200000000050000000300000010")).unwrap_err();
        assert_eq!(err2.reason, Reason::FrameSizeError);
    }

    #[test]
    fn priority_frame_self_dependency_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("0000050200000000050000000510")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn priority_frame_zero_stream_id_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("0000050200000000000000000310")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn push_promise_promised_stream_zero_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("0000050504000000010000000088")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn push_promise_zero_stream_id_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("0000050504000000000000000288")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn push_promise_too_short_is_frame_size_error() {
        let err = decode_one(&hex_to_bytes("0000020504000000010000")).unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
    }

    #[test]
    fn push_promise_padded_decode() {
        // payload: 01 (pad len) | 00 00 00 02 (promised) | 88 (block) | 00 (pad)
        let frame = decode_one(&hex_to_bytes("000007050c0000000101000000028800")).unwrap();
        assert_eq!(
            frame,
            Frame::PushPromise {
                stream_id: 1,
                end_headers: true,
                promised_stream_id: 2,
                block: hex_to_bytes("88").into(),
            }
        );
    }

    #[test]
    fn continuation_without_headers_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("00000109040000000388")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn continuation_on_different_stream_is_protocol_error() {
        // HEADERS (no END_HEADERS) on stream 3, then CONTINUATION on stream 5.
        let err = decode_all(&hex_to_bytes(
            "00000101000000000388 00000109000000000588".replace(' ', ""),
        ))
        .unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn non_continuation_while_block_open_is_protocol_error() {
        // HEADERS without END_HEADERS on stream 3, then a DATA frame.
        let err = decode_all(&hex_to_bytes(
            "00000101000000000388 000001000000000003ab".replace(' ', ""),
        ))
        .unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn continuation_closes_block_then_headers_allowed() {
        // HEADERS (no END_HEADERS) + CONTINUATION (END_HEADERS) on stream 3,
        // then a fresh HEADERS (END_HEADERS) on stream 5.
        let frames = decode_all(&hex_to_bytes(
            "00000101000000000388 00000109040000000388 00000101040000000584".replace(' ', ""),
        ))
        .unwrap();
        assert_eq!(frames.len(), 3);
        let Frame::Headers { end_headers, .. } = &frames[0] else {
            panic!();
        };
        assert!(!end_headers);
        let Frame::Continuation { end_headers, .. } = &frames[1] else {
            panic!();
        };
        assert!(*end_headers);
        assert!(matches!(
            &frames[2],
            Frame::Headers {
                end_headers: true,
                ..
            }
        ));
    }

    #[test]
    fn reserved_stream_bit_is_protocol_error() {
        let err = decode_one(&hex_to_bytes("000000040000000081")).unwrap_err();
        assert_eq!(err.reason, Reason::ProtocolError);
    }

    #[test]
    fn oversized_frame_is_frame_size_error() {
        // HEADERS frame claiming 16385 octets of payload: rejected from the
        // header alone, before the payload arrives.
        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        decoder.extend(&hex_to_bytes("004001010000000001"));
        let err = decoder.next_frame().unwrap_err();
        assert_eq!(err.reason, Reason::FrameSizeError);
    }

    #[test]
    fn max_frame_size_can_be_increased() {
        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        decoder.extend(&hex_to_bytes("004001010000000001"));
        assert_eq!(
            decoder.next_frame().unwrap_err().reason,
            Reason::FrameSizeError
        );

        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        decoder.set_max_frame_size(32_768);
        decoder.extend(&hex_to_bytes("004001010000000001"));
        // 16385 octets payload is now legal, but incomplete.
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn settings_max_frame_size_is_adopted_by_decoder() {
        // SETTINGS announcing SETTINGS_MAX_FRAME_SIZE = 65536, followed by
        // a 20000-octet DATA frame that would violate the default limit.
        let mut session = hex_to_bytes("000006040000000000000500010000");
        let mut data = Vec::new();
        FrameWriter::new(16_384).write_data(&mut data, 1, true, &vec![0x61; 20_000]);
        session.extend_from_slice(&data);

        let frames = decode_all(&session).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(matches!(
            &frames[1],
            Frame::Data { data: body, .. } if body.len() == 20_000
        ));
    }

    #[test]
    fn unknown_frame_type_is_preserved() {
        let frame = decode_one(&hex_to_bytes("0000020a0800000007abcd")).unwrap();
        assert_eq!(
            frame,
            Frame::Unknown {
                typ: 0x0a,
                flags: 0x08,
                stream_id: 7,
                payload: hex_to_bytes("abcd").into(),
            }
        );
    }

    #[test]
    fn incremental_feeding_matches_batch() {
        let mut out = Vec::new();
        let writer = FrameWriter::new(16384);
        writer.write_settings(
            &mut out,
            &[Setting {
                id: 0x01,
                value: 4096,
            }],
        );
        writer.write_headers(&mut out, 1, false, true, None, &[0x82, 0x84, 0x86]);
        writer.write_data(&mut out, 1, true, b"hello, h2!");
        writer.write_window_update(&mut out, 0, 1024);
        writer.write_goaway(&mut out, 1, 0, &[]);

        let batch = decode_all(&out).unwrap();

        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        let mut incremental = Vec::new();
        for byte in &out {
            decoder.extend(&[*byte]);
            while let Some(frame) = decoder.next_frame().unwrap() {
                incremental.push(frame);
            }
        }
        assert_eq!(batch, incremental);
    }

    #[test]
    fn round_trip_all_frame_kinds() {
        let mut out = Vec::new();
        let writer = FrameWriter::new(16384);
        writer.write_data(&mut out, 1, true, b"payload");
        writer.write_headers(&mut out, 3, false, true, None, &[0x88]);
        writer.write_headers(
            &mut out,
            5,
            true,
            true,
            Some(Priority {
                exclusive: true,
                dependency: 3,
                weight: 1,
            }),
            &[0x84],
        );
        writer.write_priority(
            &mut out,
            7,
            Priority {
                exclusive: false,
                dependency: 1,
                weight: 255,
            },
        );
        writer.write_reset(&mut out, 9, 0x08);
        writer.write_settings(
            &mut out,
            &[Setting {
                id: 0x05,
                value: 65536,
            }],
        );
        writer.write_settings_ack(&mut out);
        writer.write_push_promise(&mut out, 11, 2, &[0x84]);
        writer.write_ping(&mut out, &[0; 8]);
        writer.write_goaway(&mut out, 13, 0x01, b"debug info");
        writer.write_window_update(&mut out, 0, 1);
        writer.write_window_update(&mut out, 15, 4096);

        let expected: Vec<Frame> = vec![
            Frame::Data {
                stream_id: 1,
                end_stream: true,
                data: b"payload".as_slice().into(),
            },
            Frame::Headers {
                stream_id: 3,
                end_stream: false,
                end_headers: true,
                priority: None,
                block: hex_to_bytes("88").into(),
            },
            Frame::Headers {
                stream_id: 5,
                end_stream: true,
                end_headers: true,
                priority: Some(Priority {
                    exclusive: true,
                    dependency: 3,
                    weight: 1,
                }),
                block: hex_to_bytes("84").into(),
            },
            Frame::Priority {
                stream_id: 7,
                priority: Priority {
                    exclusive: false,
                    dependency: 1,
                    weight: 255,
                },
            },
            Frame::Reset {
                stream_id: 9,
                error_code: 0x08,
            },
            Frame::Settings {
                ack: false,
                settings: vec![Setting {
                    id: 0x05,
                    value: 65536,
                }],
            },
            Frame::Settings {
                ack: true,
                settings: vec![],
            },
            Frame::PushPromise {
                stream_id: 11,
                end_headers: true,
                promised_stream_id: 2,
                block: hex_to_bytes("84").into(),
            },
            Frame::Ping {
                ack: false,
                payload: [0; 8],
            },
            Frame::GoAway {
                last_stream_id: 13,
                error_code: 0x01,
                debug: b"debug info".as_slice().into(),
            },
            Frame::WindowUpdate {
                stream_id: 0,
                increment: 1,
            },
            Frame::WindowUpdate {
                stream_id: 15,
                increment: 4096,
            },
        ];

        assert_eq!(decode_all(&out).unwrap(), expected);
    }
}
