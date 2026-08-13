//! HTTP/3 frame codec (RFC 9114 Section 7).
//!
//! Frames on HTTP/3 streams have the layout `Type (i), Length (i), Frame
//! Payload (..)` where `Type` and `Length` are QUIC variable-length
//! integers (RFC 9000 Section 16). This module provides:
//!
//! - [`FrameDecoder`]: an incremental, buffer-owning decoder. The driver
//!   feeds received bytes in and pulls completed [`Frame`]s out; `Ok(None)`
//!   means more input is needed. Unknown and reserved (grease) frame types
//!   are skipped without surfacing, per RFC 9114 Section 7.2.8.
//! - [`Frame::encode`]: the corresponding serializer.
//!
//! Parse-level validation follows RFC 9114 Section 7.1: a frame payload
//! must contain exactly the fields identified for its type — extra bytes
//! and truncated fields are `H3_FRAME_ERROR`, as are redundant
//! (non-minimal) variable-length integer encodings (Section 10.8, RFC 9000
//! Section 16). HTTP/2 frame types without an HTTP/3 equivalent
//! (PRIORITY, PING, WINDOW_UPDATE, CONTINUATION) are `H3_FRAME_UNEXPECTED`
//! (Section 7.2.8).
//!
//! What this module deliberately does *not* enforce — it is the driver's
//! (connection-state) job:
//!
//! - stream-type rules (which frame types are legal on which stream, and
//!   that SETTINGS is first on the control stream),
//! - `SETTINGS_MAX_FIELD_SECTION_SIZE` limits on HEADERS/PUSH_PROMISE
//!   payloads,
//! - push ID / stream ID semantics (`H3_ID_ERROR`).
//!
//! A clean end of stream (FIN) with `buffered() != 0` means the last frame
//! was truncated; RFC 9114 Section 7.1 requires that be treated as
//! `H3_FRAME_ERROR` by the driver.

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// The largest value a QUIC variable-length integer can carry.
pub const MAX_VARINT: u64 = (1 << 62) - 1;

/// `DATA` frame type (RFC 9114 Section 7.2.1).
pub const FRAME_DATA: u64 = 0x0;
/// `HEADERS` frame type (RFC 9114 Section 7.2.2).
pub const FRAME_HEADERS: u64 = 0x1;
/// `CANCEL_PUSH` frame type (RFC 9114 Section 7.2.3).
pub const FRAME_CANCEL_PUSH: u64 = 0x3;
/// `SETTINGS` frame type (RFC 9114 Section 7.2.4).
pub const FRAME_SETTINGS: u64 = 0x4;
/// `PUSH_PROMISE` frame type (RFC 9114 Section 7.2.5).
pub const FRAME_PUSH_PROMISE: u64 = 0x5;
/// `GOAWAY` frame type (RFC 9114 Section 7.2.6).
pub const FRAME_GOAWAY: u64 = 0x7;
/// `MAX_PUSH_ID` frame type (RFC 9114 Section 7.2.7).
pub const FRAME_MAX_PUSH_ID: u64 = 0xd;

/// `SETTINGS_QPACK_MAX_TABLE_CAPACITY` (RFC 9204 Section 5).
///
/// Consumed by the control-stream driver when interpreting peer SETTINGS.
#[allow(dead_code)]
pub const SETTINGS_QPACK_MAX_TABLE_CAPACITY: u64 = 0x1;
/// `SETTINGS_MAX_FIELD_SECTION_SIZE` (RFC 9114 Section 7.2.4.1).
///
/// Consumed by the control-stream driver when interpreting peer SETTINGS.
#[allow(dead_code)]
pub const SETTINGS_MAX_FIELD_SECTION_SIZE: u64 = 0x6;
/// `SETTINGS_QPACK_BLOCKED_STREAMS` (RFC 9204 Section 5).
///
/// Consumed by the control-stream driver when interpreting peer SETTINGS.
#[allow(dead_code)]
pub const SETTINGS_QPACK_BLOCKED_STREAMS: u64 = 0x7;
/// `SETTINGS_ENABLE_CONNECT_PROTOCOL` (RFC 9114 Section 7.2.4.1).
///
/// Consumed by the control-stream driver when interpreting peer SETTINGS.
#[allow(dead_code)]
pub const SETTINGS_ENABLE_CONNECT_PROTOCOL: u64 = 0x8;
/// `SETTINGS_H3_DATAGRAM` (RFC 9297 Section 3.1).
///
/// Consumed by the control-stream driver when interpreting peer SETTINGS.
#[allow(dead_code)]
pub const SETTINGS_H3_DATAGRAM: u64 = 0x33;

/// A single SETTINGS parameter: `(identifier, value)`.
pub type Setting = (u64, u64);

/// A parsed `SETTINGS` frame payload (RFC 9114 Section 7.2.4).
///
/// Settings are kept in wire order. Reserved (grease) identifiers are
/// dropped on decode and may be added for encoding; unknown identifiers
/// are preserved and ignored by the driver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    entries: Vec<Setting>,
}

impl Settings {
    /// An empty SETTINGS payload.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a `(identifier, value)` parameter in wire order.
    pub fn insert(&mut self, id: u64, value: u64) {
        self.entries.push((id, value));
    }

    /// The value of the first parameter with `id`, if any.
    pub fn get(&self, id: u64) -> Option<u64> {
        self.entries
            .iter()
            .find_map(|(i, v)| (*i == id).then_some(*v))
    }

    /// The parameters in wire order.
    pub fn iter(&self) -> impl Iterator<Item = Setting> + '_ {
        self.entries.iter().copied()
    }
}

/// An HTTP/3 frame (RFC 9114 Section 7.2).
///
/// `Data` and `Headers` payloads are opaque byte ranges. `Data` is the
/// streamed body chunk (the driver may hand it to the body reader
/// without copying); `Headers` and `PushPromise` field sections are
/// QPACK-encoded and decoded by the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// `DATA`: a chunk of the request or response body.
    Data(Bytes),
    /// `HEADERS`: the QPACK-encoded field section.
    Headers(Bytes),
    /// `SETTINGS`: connection parameters (first frame of a control
    /// stream).
    Settings(Settings),
    /// `CANCEL_PUSH`: push ID whose push the peer should abandon.
    CancelPush(u64),
    /// `PUSH_PROMISE`: push ID plus the promised request's QPACK-encoded
    /// field section.
    PushPromise { push_id: u64, field_section: Bytes },
    /// `GOAWAY`: the highest stream ID (server) or push ID (client) the
    /// sender will process.
    Goaway(u64),
    /// `MAX_PUSH_ID`: the highest push ID the server may use.
    MaxPushId(u64),
}

impl Frame {
    /// Whether this is one of the known HTTP/3 frame types (RFC 9114
    /// Section 7.2).
    ///
    /// The decoder never surfaces unknown or reserved (grease) frame types
    /// — they are consumed and skipped (Section 7.2.8) — so every frame it
    /// returns is by construction a known type. This method documents the
    /// request-stream rule (Section 4.1) that after the trailers only
    /// unknown frames may still appear.
    pub fn is_known(&self) -> bool {
        matches!(
            self,
            Frame::Data(_)
                | Frame::Headers(_)
                | Frame::Settings(_)
                | Frame::CancelPush(_)
                | Frame::PushPromise { .. }
                | Frame::Goaway(_)
                | Frame::MaxPushId(_)
        )
    }

    /// Serializes this frame (type, length, payload) into `dst`.
    pub fn encode(&self, dst: &mut BytesMut) {
        match self {
            Frame::Data(payload) => {
                write_varint(FRAME_DATA, dst);
                write_varint(payload.len() as u64, dst);
                dst.extend_from_slice(payload);
            }
            Frame::Headers(payload) => {
                write_varint(FRAME_HEADERS, dst);
                write_varint(payload.len() as u64, dst);
                dst.extend_from_slice(payload);
            }
            Frame::Settings(settings) => {
                write_varint(FRAME_SETTINGS, dst);
                let len: usize = settings
                    .entries
                    .iter()
                    .map(|(id, value)| varint_size(*id) + varint_size(*value))
                    .sum();
                write_varint(len as u64, dst);
                for (id, value) in &settings.entries {
                    write_varint(*id, dst);
                    write_varint(*value, dst);
                }
            }
            Frame::CancelPush(push_id) => {
                write_varint(FRAME_CANCEL_PUSH, dst);
                write_varint(varint_size(*push_id) as u64, dst);
                write_varint(*push_id, dst);
            }
            Frame::PushPromise {
                push_id,
                field_section,
            } => {
                write_varint(FRAME_PUSH_PROMISE, dst);
                write_varint((varint_size(*push_id) + field_section.len()) as u64, dst);
                write_varint(*push_id, dst);
                dst.extend_from_slice(field_section);
            }
            Frame::Goaway(stream_id) => {
                write_varint(FRAME_GOAWAY, dst);
                write_varint(varint_size(*stream_id) as u64, dst);
                write_varint(*stream_id, dst);
            }
            Frame::MaxPushId(push_id) => {
                write_varint(FRAME_MAX_PUSH_ID, dst);
                write_varint(varint_size(*push_id) as u64, dst);
                write_varint(*push_id, dst);
            }
        }
    }
}

/// Errors raised by [`FrameDecoder::next_frame`].
///
/// Each variant corresponds to a distinct RFC 9114 connection error code
/// (see [`FrameError::h3_code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// `H3_FRAME_ERROR` (0x0106): the frame payload does not exactly match
    /// the fields identified for its type, or a variable-length integer is
    /// encoded non-minimally (RFC 9114 Sections 7.1 and 10.8).
    Frame,
    /// `H3_FRAME_UNEXPECTED` (0x0105): an HTTP/2 frame type with no
    /// HTTP/3 equivalent (PRIORITY, PING, WINDOW_UPDATE, CONTINUATION;
    /// RFC 9114 Section 7.2.8).
    Unexpected(u64),
    /// `H3_SETTINGS_ERROR` (0x0109): a reserved setting identifier
    /// (0x02-0x05) or a duplicate identifier in one SETTINGS frame
    /// (RFC 9114 Section 7.2.4).
    Settings,
}

impl FrameError {
    /// The RFC 9114 connection error code for this error.
    pub const fn h3_code(self) -> u64 {
        use crate::h3::H3Error;
        match self {
            FrameError::Frame => H3Error::FrameError.code(),
            FrameError::Unexpected(_) => H3Error::FrameUnexpected.code(),
            FrameError::Settings => H3Error::Settings.code(),
        }
    }
}

/// Incremental HTTP/3 frame decoder.
///
/// The decoder owns its buffer: the driver calls [`FrameDecoder::extend`]
/// with every received chunk and [`FrameDecoder::next_frame`] once per
/// event-loop turn, draining as many complete frames as are buffered.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: BytesMut,
}

impl FrameDecoder {
    /// A decoder with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends received bytes to the input buffer.
    pub fn extend(&mut self, data: Bytes) {
        self.buf.extend_from_slice(&data);
    }

    /// Bytes buffered but not yet consumed by a frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Pops the next complete frame, if any.
    ///
    /// Returns `Ok(None)` when the buffer does not yet hold a complete
    /// frame; callers must extend the buffer and poll again. Unknown and
    /// reserved frame types are consumed and skipped (RFC 9114 Section
    /// 7.2.8) — a known frame behind them is still returned.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        loop {
            let Some((ty, type_len)) = parse_varint(&self.buf)? else {
                return Ok(None);
            };
            if matches!(ty, 0x02 | 0x06 | 0x08 | 0x09) {
                return Err(FrameError::Unexpected(ty));
            }
            let Some((len, len_len)) = parse_varint(&self.buf[type_len..])? else {
                return Ok(None);
            };
            let header_len = type_len + len_len;
            let Some(total) = header_len.checked_add(len as usize) else {
                return Err(FrameError::Frame);
            };
            if total > self.buf.len() {
                return Ok(None);
            }
            if !is_known_frame_type(ty) {
                self.buf.advance(total);
                continue;
            }
            let mut chunk = self.buf.split_to(total);
            let mut payload = chunk.split_off(header_len);
            let frame = match ty {
                FRAME_DATA => Frame::Data(payload.freeze()),
                FRAME_HEADERS => Frame::Headers(payload.freeze()),
                FRAME_CANCEL_PUSH => Frame::CancelPush(take_varint(&mut payload)?),
                FRAME_SETTINGS => Frame::Settings(parse_settings(&mut payload)?),
                FRAME_PUSH_PROMISE => {
                    let Some((push_id, id_len)) = parse_varint(&payload)? else {
                        return Err(FrameError::Frame);
                    };
                    Frame::PushPromise {
                        push_id,
                        field_section: payload.split_off(id_len).freeze(),
                    }
                }
                FRAME_GOAWAY => Frame::Goaway(take_varint(&mut payload)?),
                FRAME_MAX_PUSH_ID => Frame::MaxPushId(take_varint(&mut payload)?),
                _ => unreachable!("unknown types are skipped above"),
            };
            return Ok(Some(frame));
        }
    }
}

fn is_known_frame_type(ty: u64) -> bool {
    matches!(
        ty,
        FRAME_DATA
            | FRAME_HEADERS
            | FRAME_CANCEL_PUSH
            | FRAME_SETTINGS
            | FRAME_PUSH_PROMISE
            | FRAME_GOAWAY
            | FRAME_MAX_PUSH_ID
    )
}

/// Reserved grease identifiers: `0x1f * N + 0x21` (RFC 9114 Sections 7.2.8
/// and 7.2.4.1) — must be ignored, never interpreted.
fn is_grease(v: u64) -> bool {
    v >= 0x21 && (v - 0x21).is_multiple_of(0x1f)
}

fn is_reserved_setting(id: u64) -> bool {
    (0x02..=0x05).contains(&id)
}

fn parse_settings(payload: &mut BytesMut) -> Result<Settings, FrameError> {
    let mut settings = Settings::new();
    let mut rest = &payload[..];
    while !rest.is_empty() {
        let Some((id, id_len)) = parse_varint(rest)? else {
            return Err(FrameError::Frame);
        };
        rest = &rest[id_len..];
        let Some((value, value_len)) = parse_varint(rest)? else {
            return Err(FrameError::Frame);
        };
        rest = &rest[value_len..];
        if is_reserved_setting(id) {
            return Err(FrameError::Settings);
        }
        if settings.get(id).is_some() {
            return Err(FrameError::Settings);
        }
        if !is_grease(id) {
            settings.insert(id, value);
        }
    }
    Ok(settings)
}

/// Parses the single variable-length integer that must fill `buf` exactly
/// (used for CANCEL_PUSH, GOAWAY, MAX_PUSH_ID, and the PUSH_PROMISE push
/// ID). A truncated or non-minimal integer, or trailing bytes, is
/// `H3_FRAME_ERROR` (RFC 9114 Sections 7.1 and 10.8).
fn take_varint(buf: &mut BytesMut) -> Result<u64, FrameError> {
    let Some((value, n)) = parse_varint(buf)? else {
        return Err(FrameError::Frame);
    };
    if n != buf.len() {
        return Err(FrameError::Frame);
    }
    Ok(value)
}

/// Parses a QUIC variable-length integer (RFC 9000 Section 16) from the
/// front of `buf`.
///
/// Returns `Ok(None)` when `buf` is shorter than the encoding; `Err` when
/// the encoding is non-minimal (a protocol violation, per RFC 9000 Section
/// 16, surfaced as `H3_FRAME_ERROR`).
///
/// The control plane uses this to read uni stream type varints before a
/// stream is assigned its role.
pub(crate) fn parse_varint(buf: &[u8]) -> Result<Option<(u64, usize)>, FrameError> {
    let Some(&first) = buf.first() else {
        return Ok(None);
    };
    let len = 1usize << (first >> 6);
    if buf.len() < len {
        return Ok(None);
    }
    let mut value = u64::from(first & 0x3f);
    for &byte in &buf[1..len] {
        value = (value << 8) | u64::from(byte);
    }
    // Minimal encoding: the value must not fit in the next-smaller
    // encoding. 2-byte values must be >= 2^6, 4-byte >= 2^14, 8-byte >=
    // 2^30 (RFC 9000 Section 16).
    if len > 1 {
        let prev_width = 8 * (len / 2) - 2;
        if value < (1u64 << prev_width) {
            return Err(FrameError::Frame);
        }
    }
    Ok(Some((value, len)))
}

/// The encoded length of `value` as a QUIC variable-length integer.
pub fn varint_size(value: u64) -> usize {
    if value < (1 << 6) {
        1
    } else if value < (1 << 14) {
        2
    } else if value < (1 << 30) {
        4
    } else {
        8
    }
}

/// Encodes `value` as a QUIC variable-length integer (RFC 9000 Section
/// 16). Panics in debug builds if `value` does not fit.
pub fn write_varint(value: u64, dst: &mut BytesMut) {
    debug_assert!(value <= MAX_VARINT, "varint out of range: {value:#x}");
    if value < (1 << 6) {
        dst.put_u8(value as u8);
    } else if value < (1 << 14) {
        dst.put_u16((0b01 << 14) | value as u16);
    } else if value < (1 << 30) {
        dst.put_u32((0b10 << 30) | value as u32);
    } else {
        dst.put_u64((0b11 << 62) | value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(decoder: &mut FrameDecoder) -> Result<Vec<Frame>, FrameError> {
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame()? {
            frames.push(frame);
        }
        Ok(frames)
    }

    fn encode_frames(frames: &[Frame]) -> Bytes {
        let mut buf = BytesMut::new();
        for frame in frames {
            frame.encode(&mut buf);
        }
        buf.freeze()
    }

    #[test]
    fn round_trip_all_frame_types() {
        let mut settings = Settings::new();
        settings.insert(SETTINGS_QPACK_MAX_TABLE_CAPACITY, 4096);
        settings.insert(SETTINGS_MAX_FIELD_SECTION_SIZE, 100);
        settings.insert(SETTINGS_QPACK_BLOCKED_STREAMS, 2);
        settings.insert(0x21, 7); // grease: preserved on encode, ignored on decode

        let frames = [
            Frame::Data(Bytes::from_static(b"hello world")),
            Frame::Headers(Bytes::from_static(b"\x3f\xbd\x01")),
            Frame::Settings(settings),
            Frame::CancelPush(7),
            Frame::PushPromise {
                push_id: 1,
                field_section: Bytes::from_static(b"\x05\x00\x80"),
            },
            Frame::Goaway(2),
            Frame::MaxPushId(0),
            Frame::Data(Bytes::new()),
            Frame::Headers(Bytes::new()),
        ];
        let mut decoder = FrameDecoder::new();
        decoder.extend(encode_frames(&frames));
        let got = decode_all(&mut decoder).expect("all frames parse");

        // Reserved grease setting is dropped, everything else round-trips.
        let mut expected = frames.to_vec();
        expected[2] = Frame::Settings(settings_without_grease());
        assert_eq!(got, expected);
    }

    fn settings_without_grease() -> Settings {
        let mut s = Settings::new();
        s.insert(SETTINGS_QPACK_MAX_TABLE_CAPACITY, 4096);
        s.insert(SETTINGS_MAX_FIELD_SECTION_SIZE, 100);
        s.insert(SETTINGS_QPACK_BLOCKED_STREAMS, 2);
        s
    }

    #[test]
    fn incremental_byte_at_a_time() {
        let wire = encode_frames(&[
            Frame::Headers(Bytes::from_static(b"abc")),
            Frame::Data(Bytes::from_static(b"xy")),
        ]);
        let mut decoder = FrameDecoder::new();
        let mut got = Vec::new();
        for (i, &byte) in wire.iter().enumerate() {
            decoder.extend(Bytes::copy_from_slice(&[byte]));
            while let Some(frame) = decoder.next_frame().unwrap() {
                got.push(frame);
            }
            if i < 4 {
                assert!(got.is_empty(), "frame appeared early at byte {i}");
            }
            if i == 4 {
                // The full HEADERS frame appears exactly when its last
                // byte lands.
                assert_eq!(got, vec![Frame::Headers(Bytes::from_static(b"abc"))]);
            }
        }
        assert_eq!(
            got,
            vec![
                Frame::Headers(Bytes::from_static(b"abc")),
                Frame::Data(Bytes::from_static(b"xy")),
            ]
        );
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn truncated_prefixes_are_incomplete() {
        // Type byte only.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x00]));
        assert_eq!(decoder.next_frame().unwrap(), None);
        // Type + length, no payload yet.
        decoder.extend(Bytes::from_static(&[0x05]));
        assert_eq!(decoder.next_frame().unwrap(), None);
        // Partial payload.
        decoder.extend(Bytes::from_static(b"he"));
        assert_eq!(decoder.next_frame().unwrap(), None);
        // Rest of the payload completes the frame.
        decoder.extend(Bytes::from_static(b"llo"));
        assert_eq!(
            decode_all(&mut decoder).unwrap(),
            vec![Frame::Data(Bytes::from_static(b"hello"))]
        );

        // A frame declaring the maximum varint length stays incomplete
        // without erroring or allocating.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[
            0x01, 0xc0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ]));
        assert_eq!(decoder.next_frame().unwrap(), None);
        assert_eq!(decoder.buffered(), 10);
    }

    #[test]
    fn forbidden_http2_frames() {
        for ty in [0x02u8, 0x06, 0x08, 0x09] {
            let mut decoder = FrameDecoder::new();
            decoder.extend(Bytes::copy_from_slice(&[ty, 0x01, 0x00]));
            let err = decoder.next_frame().unwrap_err();
            assert_eq!(err, FrameError::Unexpected(u64::from(ty)));
            assert_eq!(err.h3_code(), 0x0105);
        }
    }

    #[test]
    fn unknown_and_grease_frames_are_skipped() {
        // Unknown type 0x42 with payload, grease 0x21/0x40/0x5f with
        // arbitrary payload, between two known frames.
        let mut wire = BytesMut::new();
        Frame::Headers(Bytes::from_static(b"first")).encode(&mut wire);
        write_varint(0x42, &mut wire);
        write_varint(3, &mut wire);
        wire.extend_from_slice(b"xyz");
        write_varint(0x21, &mut wire);
        write_varint(2, &mut wire);
        wire.extend_from_slice(&[0xde, 0xad]);
        Frame::Data(Bytes::from_static(b"last")).encode(&mut wire);

        let mut decoder = FrameDecoder::new();
        decoder.extend(wire.freeze());
        let frames = decode_all(&mut decoder).unwrap();
        assert_eq!(
            frames,
            vec![
                Frame::Headers(Bytes::from_static(b"first")),
                Frame::Data(Bytes::from_static(b"last")),
            ]
        );
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn settings_payload_validation() {
        // Empty SETTINGS is legal.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x04, 0x00]));
        assert_eq!(
            decode_all(&mut decoder).unwrap(),
            vec![Frame::Settings(Settings::new())]
        );

        // Duplicate identifier -> H3_SETTINGS_ERROR.
        let mut wire = BytesMut::new();
        Frame::Settings({
            let mut s = Settings::new();
            s.insert(0x06, 1);
            s.insert(0x06, 2);
            s
        })
        .encode(&mut wire);
        let mut decoder = FrameDecoder::new();
        decoder.extend(wire.freeze());
        let err = decoder.next_frame().unwrap_err();
        assert_eq!(err, FrameError::Settings);
        assert_eq!(err.h3_code(), 0x0109);

        // Reserved identifiers 0x02-0x05 -> H3_SETTINGS_ERROR.
        for id in 0x02..=0x05 {
            let mut wire = BytesMut::new();
            Frame::Settings({
                let mut s = Settings::new();
                s.insert(id, 0);
                s
            })
            .encode(&mut wire);
            let mut decoder = FrameDecoder::new();
            decoder.extend(wire.freeze());
            assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Settings);
        }

        // Unknown identifier is preserved (the driver ignores it), known
        // ones surface.
        let mut s = Settings::new();
        s.insert(0x0100, 5);
        s.insert(SETTINGS_ENABLE_CONNECT_PROTOCOL, 1);
        let mut wire = BytesMut::new();
        Frame::Settings(s.clone()).encode(&mut wire);
        let mut decoder = FrameDecoder::new();
        decoder.extend(wire.freeze());
        match decode_all(&mut decoder).unwrap()[0].clone() {
            Frame::Settings(got) => {
                assert_eq!(got.get(0x0100), Some(5));
                assert_eq!(got.get(SETTINGS_ENABLE_CONNECT_PROTOCOL), Some(1));
            }
            other => panic!("expected Settings, got {other:?}"),
        }

        // Odd-length payload (a lone identifier) -> H3_FRAME_ERROR.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x04, 0x01, 0x06]));
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Frame);
    }

    #[test]
    fn fixed_value_frames_reject_bad_payloads() {
        // CANCEL_PUSH with no payload -> H3_FRAME_ERROR.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x03, 0x00]));
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Frame);

        // CANCEL_PUSH with trailing bytes -> H3_FRAME_ERROR.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x03, 0x02, 0x01, 0x00]));
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Frame);

        // CANCEL_PUSH with a redundant 2-byte encoding of 5 -> H3_FRAME_ERROR.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x03, 0x02, 0x40, 0x05]));
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Frame);

        // GOAWAY with a minimal 1-byte value parses.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x07, 0x01, 0x05]));
        assert_eq!(decode_all(&mut decoder).unwrap(), vec![Frame::Goaway(5)]);

        // Non-minimal type encoding (0 encoded in 2 bytes) -> error.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x40, 0x00, 0x00]));
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Frame);

        // Non-minimal length encoding -> error.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x00, 0x40, 0x00]));
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Frame);
    }

    #[test]
    fn push_promise_shapes() {
        // push ID plus field section.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x05, 0x04, 0x01, b'a', b'b', b'c']));
        assert_eq!(
            decode_all(&mut decoder).unwrap(),
            vec![Frame::PushPromise {
                push_id: 1,
                field_section: Bytes::from_static(b"abc"),
            }]
        );

        // Empty field section.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x05, 0x01, 0x01]));
        assert_eq!(
            decode_all(&mut decoder).unwrap(),
            vec![Frame::PushPromise {
                push_id: 1,
                field_section: Bytes::new(),
            }]
        );

        // Missing push ID -> H3_FRAME_ERROR.
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x05, 0x00]));
        assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Frame);
    }

    #[test]
    fn varint_edge_encodings() {
        // Boundaries: 2^6-1 (1 byte), 2^6 (2 bytes), 2^14-1, 2^14, 2^30-1,
        // 2^30, 2^62-1 (max).
        for value in [
            (1 << 6) - 1,
            1 << 6,
            (1 << 14) - 1,
            1 << 14,
            (1 << 30) - 1,
            1 << 30,
            MAX_VARINT,
        ] {
            let mut wire = BytesMut::new();
            write_varint(value, &mut wire);
            assert_eq!(wire.len(), varint_size(value));
            let (got, n) = parse_varint(&wire).unwrap().unwrap();
            assert_eq!(got, value);
            assert_eq!(n, wire.len());
        }

        // Non-minimal encodings are rejected.
        assert_eq!(parse_varint(&[0x40, 0x00]), Err(FrameError::Frame)); // 0 in 2 bytes
        assert_eq!(
            parse_varint(&[0x80, 0x00, 0x00, 0x40]),
            Err(FrameError::Frame)
        ); // 64 in 4 bytes
        assert_eq!(parse_varint(&[0x40, 0x40]), Ok(Some((64, 2))));
        // Truncated.
        assert_eq!(parse_varint(&[0x40]), Ok(None));
        assert_eq!(parse_varint(&[]), Ok(None));
    }

    #[test]
    fn clean_eof_with_truncated_frame_is_detectable() {
        let mut decoder = FrameDecoder::new();
        decoder.extend(Bytes::from_static(&[0x01, 0x05, b'a', b'b']));
        assert_eq!(decoder.next_frame().unwrap(), None);
        // Driver's clean-FIN check: buffered() != 0 -> H3_FRAME_ERROR.
        assert_eq!(decoder.buffered(), 4);
    }
}
