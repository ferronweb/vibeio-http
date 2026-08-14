//! QPACK decoder (RFC 9204 Sections 2.2, 4.3 and 4.5).
//!
//! Consumption: the HTTP/3 layer drives the decoder per connection (it feeds
//! encoder stream data and decoded field sections, and drains the decoder
//! stream); until that lands, the whole module is dead in non-test builds,
//! which is why `dead_code` is expected here. It errors again once the
//! decoder is used, reminding us to remove the expectation.
//!
//! The decoder materializes the shared dynamic table (Section 4.2) from the
//! encoder stream (Section 4.3): every instruction is parsed and mirrored as
//! a table insertion or capacity change. Field sections (Section 4.5) are
//! decoded against the table; a section whose Required Insert Count exceeds
//! the decoder's insert count is buffered as blocked (Section 2.2.1) until a
//! later encoder stream update makes it decodable.
//!
//! The decoder emits decoder stream instructions (Section 4.4): a Section
//! Acknowledgment for every decoded field section with a positive Required
//! Insert Count (Section 2.2.2.1), a Stream Cancellation for abandoned or
//! timed-out blocked streams (Section 2.2.2.2), and coalesced Insert Count
//! Increment instructions (Section 2.2.2.3).
//!
//! Validation is strict: malformed instructions are `QPACK_ENCODER_STREAM_
//! ERROR`, malformed field sections are `QPACK_DECOMPRESSION_FAILED`, a
//! Required Insert Count that does not equal the largest referenced absolute
//! index plus one is rejected (Sections 2.1.2 and 2.2.1), evictions that
//! touch entries with an absolute index at or above the Known Received Count
//! are rejected (Sections 2.1.1 and 3.2.2), and field sections that push a
//! stream's cumulative decoded size over the advertised
//! `SETTINGS_MAX_FIELD_SECTION_SIZE` are rejected (RFC 9114 Section
//! 7.2.4.1).
#![expect(dead_code)]

use std::collections::VecDeque;

use bytes::Bytes;

use crate::h3::qpack::error::QpackError;
use crate::h3::qpack::static_table;
use crate::h3::qpack::table::DynamicTable;
use crate::hpack::{huffman, integer, HpackError};

/// `1` + 7-bit stream ID: Section Acknowledgment (RFC 9204 4.4.1).
const SECTION_ACK: u8 = 0x80;
/// `01` + 6-bit stream ID: Stream Cancellation (RFC 9204 4.4.2).
const STREAM_CANCELLATION: u8 = 0x40;
/// `00` + 6-bit increment: Insert Count Increment (RFC 9204 4.4.3).
const INSERT_COUNT_INCREMENT: u8 = 0x00;

// Encoder instruction patterns (RFC 9204 4.3), mirrored from the encoder.
/// `001` + 5-bit capacity: Set Dynamic Table Capacity (4.3.1).
const SET_CAPACITY: u8 = 0b0010_0000;
/// `1 T` + 6-bit name index: Insert with Name Reference (4.3.2).
const INSERT_WITH_NAME_REF: u8 = 0b1000_0000;
/// `01` + H + 5-bit name length: Insert with Literal Name (4.3.3).
const INSERT_WITH_LITERAL_NAME: u8 = 0b0100_0000;
/// `000` + 5-bit relative index: Duplicate (4.3.4).
const DUPLICATE: u8 = 0b0000_0000;

// Field line patterns (RFC 9204 4.5), mirrored from the encoder.
/// `1 T` + 6-bit index: Indexed Field Line (4.5.2).
const INDEXED: u8 = 0b1000_0000;
/// `0001` + 4-bit post-Base index: Indexed Field Line with Post-Base Index
/// (4.5.3).
const INDEXED_POST_BASE: u8 = 0b0001_0000;
/// `01 N T` + 4-bit name index: Literal Field Line with Name Reference
/// (4.5.4).
const LITERAL_NAME_REF: u8 = 0b0100_0000;
/// `0000 N` + 3-bit post-Base name index: Literal Field Line with Post-Base
/// Name Reference (4.5.5).
const LITERAL_POST_BASE_NAME_REF: u8 = 0b0000_0000;
/// `001 N` + H + 3-bit name length: Literal Field Line with Literal Name
/// (4.5.6).
const LITERAL_LITERAL_NAME: u8 = 0b0010_0000;

/// A field section that was buffered as blocked and has since been decoded
/// after an encoder stream update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnblockedSection {
    /// The stream the encoded field section was received on.
    pub stream_id: u64,
    /// The decoded header list.
    pub headers: Vec<(Bytes, Bytes)>,
}

/// A field section buffered because its Required Insert Count had not been
/// reached yet (RFC 9204 Section 2.2.1).
#[derive(Debug)]
struct BlockedSection {
    stream_id: u64,
    buf: Bytes,
    ric: u64,
    since: u64,
}

/// QPACK decoder: dynamic table mirror and field section decoder.
#[derive(Debug)]
pub struct Decoder {
    dynamic: DynamicTable,
    /// The maximum dynamic table capacity advertised by this decoder in
    /// SETTINGS_QPACK_MAX_TABLE_CAPACITY (RFC 9204 Section 3.2.3).
    max_capacity: u64,
    /// Total insertions and duplications received on the encoder stream.
    /// Part of the field section prefix decoding context.
    ///
    /// The Known Received Count, which rules evictability and the Insert
    /// Count Increment instruction, is tracked separately in
    /// [`Decoder::known_received`] (RFC 9204 Section 2.1.4).
    ///
    /// Invariant: `known_received <= inserted`.
    known_received: u64,
    /// Blocked field sections, in arrival order.
    blocked: VecDeque<BlockedSection>,
    /// Number of blocked sections per stream. Kept separately so admission
    /// does not rescan every buffered section (or allocate a temporary list)
    /// for every field section received while the table is catching up.
    blocked_by_stream: Vec<(u64, usize)>,
    /// Decoded field-section size per stream, summed so the
    /// `SETTINGS_MAX_FIELD_SECTION_SIZE` budget applies across a stream's
    /// field sections (request headers, trailers) like
    /// `SETTINGS_MAX_HEADER_LIST_SIZE` does for HTTP/2 — not per section.
    ///
    /// A stream's budget is only charged when a section is actually
    /// decoded, so a section buffered as blocked is charged when it is
    /// unblocked. Entries are dropped when the stream finishes, is reset,
    /// or is abandoned ([`Decoder::stream_finished`],
    /// [`Decoder::stream_cancelled`], [`Decoder::expire_blocked`]); QUIC
    /// stream IDs are never reused, so a stale entry could not affect
    /// another stream anyway.
    section_size_by_stream: Vec<(u64, usize)>,
    /// The maximum number of streams that may be blocked at once,
    /// SETTINGS_QPACK_BLOCKED_STREAMS (RFC 9204 Section 5).
    max_blocked_streams: usize,
    /// Cap on the total size of a decoded field section (the sum of the
    /// lengths of the names and values of its field lines), the locally
    /// advertised `SETTINGS_MAX_FIELD_SECTION_SIZE` (RFC 9114 Section
    /// 7.2.4.1). Exceeding it is `QPACK_DECOMPRESSION_FAILED`.
    max_field_section_size: usize,
    /// Decoder stream instructions awaiting transmission.
    decoder_stream: Vec<u8>,
}

impl Decoder {
    /// Creates a decoder that advertised `max_capacity` in
    /// SETTINGS_QPACK_MAX_TABLE_CAPACITY and `max_blocked_streams` in
    /// SETTINGS_QPACK_BLOCKED_STREAMS.
    #[inline]
    pub fn new(max_capacity: u64, max_blocked_streams: usize) -> Self {
        Self {
            dynamic: DynamicTable::new(0),
            max_capacity,
            known_received: 0,
            blocked: VecDeque::new(),
            blocked_by_stream: Vec::new(),
            section_size_by_stream: Vec::new(),
            max_blocked_streams,
            max_field_section_size: usize::MAX,
            decoder_stream: Vec::new(),
        }
    }

    /// Sets the maximum size of a decoded field section: the locally
    /// advertised `SETTINGS_MAX_FIELD_SECTION_SIZE` (RFC 9114 Section
    /// 7.2.4.1). The budget is per stream and accumulates across its field
    /// sections (request headers and trailers); a section that pushes a
    /// stream's cumulative name and value octets over this is rejected
    /// with `QPACK_DECOMPRESSION_FAILED` (RFC 9204 Section 4.5).
    #[inline]
    pub fn set_max_field_section_size(&mut self, size: usize) {
        self.max_field_section_size = size;
    }

    /// The total number of insertions and duplications materialized from the
    /// encoder stream so far; the decoder's Insert Count (RFC 9204
    /// Section 2.1.1).
    #[inline]
    pub fn inserted(&self) -> u64 {
        self.dynamic.inserted()
    }

    /// The Known Received Count: insertions and duplications the decoder has
    /// acknowledged or incremented (RFC 9204 Section 2.1.4).
    #[inline]
    pub fn known_received(&self) -> u64 {
        self.known_received
    }

    /// The number of field sections currently buffered as blocked.
    #[inline]
    pub fn pending_blocked(&self) -> usize {
        self.blocked.len()
    }

    /// Takes the accumulated decoder stream instructions.
    #[inline]
    pub fn take_decoder_stream(&mut self) -> Bytes {
        Bytes::from(std::mem::take(&mut self.decoder_stream))
    }

    /// Parses the peer's QPACK decoder stream instructions (RFC 9204
    /// Section 4.4): Section Acknowledgments, Stream Cancellations, and
    /// Insert Count Increments.
    ///
    /// An Insert Count Increment with a zero value is a decoder stream
    /// error (Section 4.4.3); any malformed instruction is too. The
    /// instructions are not otherwise acted upon: this decoder emits its own
    /// decoder-stream instructions and never tracks the peer's
    /// acknowledgements, so it only needs to validate what the peer sends.
    #[inline]
    pub fn feed_decoder_stream(&mut self, buf: &[u8]) -> Result<(), QpackError> {
        let mut off = 0;
        while off < buf.len() {
            let header = buf[off];
            off += 1;
            if header & 0x80 != 0 {
                // `1` + 7-bit stream ID: Section Acknowledgment (4.4.1). The
                // top bit is the instruction type, so this arm must catch
                // every byte with the high bit set, including IDs at or
                // above 64 whose prefix byte is `0xC0`.
                integer::decode(buf, &mut off, 7, header).map_err(dec_stream_err)?;
            } else if header & 0x40 != 0 {
                // `01` + 6-bit stream ID: Stream Cancellation (4.4.2).
                integer::decode(buf, &mut off, 6, header).map_err(dec_stream_err)?;
            } else {
                // `00` + 6-bit increment: Insert Count Increment (4.4.3). A
                // zero increment is forbidden.
                let increment =
                    integer::decode(buf, &mut off, 6, header).map_err(dec_stream_err)?;
                if increment == 0 {
                    return Err(QpackError::DecoderStream);
                }
            }
        }
        Ok(())
    }

    /// Processes the encoder stream instructions in `buf`, materializing
    /// dynamic table updates.
    ///
    /// Returns the field sections that were blocked and can now be decoded,
    /// in arrival order. The Section Acknowledgment for each is queued in
    /// the decoder stream, together with a coalesced Insert Count Increment
    /// when the table grew beyond the acknowledged count.
    #[inline]
    pub fn feed_encoder_stream(&mut self, buf: &[u8]) -> Result<Vec<UnblockedSection>, QpackError> {
        let mut off = 0;
        while off < buf.len() {
            let header = buf[off];
            off += 1;
            match header & 0xC0 {
                // `1 T` + 6-bit name index: Insert with Name Reference
                // (4.3.2). The T bit being set masks to 0xC0, hence the
                // two-arm pattern.
                0x80 | 0xC0 => {
                    // Insert with Name Reference (4.3.2).
                    let index =
                        integer::decode(buf, &mut off, 6, header).map_err(enc_stream_err)?;
                    let value = self
                        .read_value_string(buf, &mut off)
                        .map_err(enc_stream_err)?;
                    let name = if header & 0x40 != 0 {
                        // T=1: static table.
                        let idx = usize::try_from(index).map_err(|_| QpackError::EncoderStream)?;
                        let (name, _) = static_table::get(idx).ok_or(QpackError::EncoderStream)?;
                        Bytes::from_static(name)
                    } else {
                        // T=0: dynamic table, relative index (index 0 is the
                        // most recently inserted entry).
                        let (name, _) = self
                            .dynamic
                            .get_relative_bytes(index)
                            .ok_or(QpackError::EncoderStream)?;
                        name
                    };
                    self.insert_entry(name, value)?;
                }
                0x40 => {
                    // Insert with Literal Name (4.3.3): the name length uses
                    // a 5-bit prefix, so `read_string` receives 6 (it reserves
                    // one bit for the Huffman flag).
                    let name = self
                        .read_string(buf, &mut off, 6, header)
                        .map_err(enc_stream_err)?;
                    let value = self
                        .read_value_string(buf, &mut off)
                        .map_err(enc_stream_err)?;
                    self.insert_entry(name, value)?;
                }
                _ => {
                    if header & 0x20 != 0 {
                        // Set Dynamic Table Capacity (4.3.1).
                        let capacity =
                            integer::decode(buf, &mut off, 5, header).map_err(enc_stream_err)?;
                        if capacity > self.max_capacity {
                            return Err(QpackError::EncoderStream);
                        }
                        let evicted = self.dynamic.evict_for_capacity(capacity);
                        if evicted > self.known_received {
                            return Err(QpackError::EncoderStream);
                        }
                        self.dynamic.set_capacity(capacity);
                    } else {
                        // Duplicate (4.3.4): relative index, 0 being the most
                        // recently inserted entry.
                        let index =
                            integer::decode(buf, &mut off, 5, header).map_err(enc_stream_err)?;
                        let (name, value) = self
                            .dynamic
                            .get_relative_bytes(index)
                            .ok_or(QpackError::EncoderStream)?;
                        self.insert_entry(name, value)?;
                    }
                }
            }
        }

        // Unblock every field section whose Required Insert Count has been
        // reached, in arrival order.
        let mut sections = Vec::new();
        while let Some(front) = self.blocked.front() {
            if front.ric > self.dynamic.inserted() {
                break;
            }
            let front = self.blocked.pop_front().expect("front just inspected");
            self.remove_blocked_section(front.stream_id);
            let headers = self.decode_ready(&front.buf)?;
            let size: usize = headers.iter().map(|(n, v)| n.len() + v.len()).sum();
            if self.account_section(front.stream_id, size) > self.max_field_section_size {
                return Err(QpackError::DecompressionFailed);
            }
            if front.ric > 0 {
                self.acknowledge(front.ric);
                self.emit_section_ack(front.stream_id);
            }
            sections.push(UnblockedSection {
                stream_id: front.stream_id,
                headers,
            });
        }

        // Coalesced Insert Count Increment (2.2.2.3): the encoder may free
        // references as soon as the received entries are acknowledged.
        if self.dynamic.inserted() > self.known_received {
            integer::encode(
                &mut self.decoder_stream,
                self.dynamic.inserted() - self.known_received,
                6,
                INSERT_COUNT_INCREMENT,
            );
            self.known_received = self.dynamic.inserted();
        }
        Ok(sections)
    }

    /// Decodes an encoded field section received on `stream_id`.
    ///
    /// Returns the decoded header list, or `None` when the section was
    /// buffered as blocked (it is returned by a later
    /// [`Decoder::feed_encoder_stream`] call).
    ///
    /// `now` is the caller's monotonic clock (any unit); it is recorded for
    /// [`Decoder::expire_blocked`]. Sections that cannot be processed in
    /// order are never decoded early: a section on a stream with buffered
    /// blocked sections joins the queue even when it could be decoded
    /// already (RFC 9204 Section 2.2.1 requires in-order processing).
    #[inline]
    pub fn decode_block(
        &mut self,
        buf: &[u8],
        stream_id: u64,
        now: u64,
    ) -> Result<Option<Vec<(Bytes, Bytes)>>, QpackError> {
        let (ric, _, _) = self.read_prefix(buf)?;
        let stream_blocked = self.stream_has_blocked(stream_id);
        if ric > self.dynamic.inserted() || stream_blocked {
            if !stream_blocked && self.blocked_by_stream.len() >= self.max_blocked_streams {
                return Err(QpackError::DecompressionFailed);
            }
            self.blocked.push_back(BlockedSection {
                stream_id,
                buf: Bytes::copy_from_slice(buf),
                ric,
                since: now,
            });
            self.add_blocked_section(stream_id);
            return Ok(None);
        }
        let headers = self.decode_ready(buf)?;
        let size: usize = headers.iter().map(|(n, v)| n.len() + v.len()).sum();
        if self.account_section(stream_id, size) > self.max_field_section_size {
            return Err(QpackError::DecompressionFailed);
        }
        if ric > 0 {
            self.acknowledge(ric);
            self.emit_section_ack(stream_id);
        }
        Ok(Some(headers))
    }

    /// Notifies that `stream_id` finished receiving (the peer closed its
    /// send side): no further field sections can arrive on it, so its
    /// field-section size budget is released. Safe to call when the stream
    /// never carried a field section.
    ///
    /// Must only be called when no field section of the stream remains
    /// buffered as blocked: a buffered section is decoded later by
    /// [`Decoder::feed_encoder_stream`], which would then restart the
    /// stream's budget from scratch. The HTTP/3 layer calls this only upon
    /// observing the peer's stream end, which implies every section of the
    /// stream (headers, trailers) has decoded.
    #[inline]
    pub fn stream_finished(&mut self, stream_id: u64) {
        self.section_size_by_stream
            .retain(|(id, _)| *id != stream_id);
    }

    /// Notifies that `stream_id` was reset or abandoned: buffered blocked
    /// sections for it are dropped and a Stream Cancellation instruction is
    /// queued (RFC 9204 Section 2.2.2.2). Returns the instruction.
    #[inline]
    pub fn stream_cancelled(&mut self, stream_id: u64) -> Bytes {
        self.blocked.retain(|b| b.stream_id != stream_id);
        self.blocked_by_stream.retain(|(id, _)| *id != stream_id);
        self.section_size_by_stream
            .retain(|(id, _)| *id != stream_id);
        let mut out = Vec::new();
        integer::encode(&mut out, stream_id, 6, STREAM_CANCELLATION);
        self.decoder_stream.extend_from_slice(&out);
        Bytes::from(out)
    }

    /// Drops blocked sections older than `max_age` in the caller's clock
    /// units, queueing one Stream Cancellation per affected stream. Returns
    /// the instructions.
    #[inline]
    pub fn expire_blocked(&mut self, now: u64, max_age: u64) -> Bytes {
        let mut out = Vec::new();
        let mut cancelled = Vec::new();
        for blocked in &self.blocked {
            if now.saturating_sub(blocked.since) > max_age
                && !cancelled.contains(&blocked.stream_id)
            {
                cancelled.push(blocked.stream_id);
                integer::encode(&mut out, blocked.stream_id, 6, STREAM_CANCELLATION);
            }
        }
        if !cancelled.is_empty() {
            // Cancelling a stream abandons every queued section on it, not
            // merely the one whose timer fired. Keep the queue and its
            // per-stream accounting in lockstep.
            self.blocked
                .retain(|blocked| !cancelled.contains(&blocked.stream_id));
            self.blocked_by_stream
                .retain(|(stream_id, _)| !cancelled.contains(stream_id));
            self.section_size_by_stream
                .retain(|(stream_id, _)| !cancelled.contains(stream_id));
        }
        self.decoder_stream.extend_from_slice(&out);
        Bytes::from(out)
    }

    /// Decodes a field section whose Required Insert Count has been reached
    /// (immediate or retried from the blocked queue), validating that the
    /// announced Required Insert Count equals the largest referenced
    /// absolute index plus one (RFC 9204 Section 2.1.2).
    #[inline]
    fn decode_ready(&self, buf: &[u8]) -> Result<Vec<(Bytes, Bytes)>, QpackError> {
        let (ric, base, mut off) = self.read_prefix(buf)?;
        let mut headers = Vec::new();
        let mut needed = 0u64;
        while off < buf.len() {
            let header = buf[off];
            off += 1;
            if header & 0x80 != 0 {
                // Indexed Field Line (4.5.2).
                let index = integer::decode(buf, &mut off, 6, header).map_err(dec_failed)?;
                if header & 0x40 != 0 {
                    // Static table (T=1).
                    let idx =
                        usize::try_from(index).map_err(|_| QpackError::DecompressionFailed)?;
                    let (name, value) =
                        static_table::get(idx).ok_or(QpackError::DecompressionFailed)?;
                    headers.push((Bytes::from_static(name), Bytes::from_static(value)));
                } else {
                    // Dynamic table (T=0): relative index from the Base.
                    let (name, value) = self
                        .dynamic
                        .get_base_relative_bytes(base, index)
                        .ok_or(QpackError::DecompressionFailed)?;
                    needed = needed.max(base - index);
                    headers.push((name, value));
                }
            } else if header & 0x40 != 0 {
                // Literal Field Line with Name Reference (4.5.4).
                let index = integer::decode(buf, &mut off, 4, header).map_err(dec_failed)?;
                let value = self.read_value_string(buf, &mut off).map_err(dec_failed)?;
                if header & 0x10 != 0 {
                    // Static table (T=1).
                    let idx =
                        usize::try_from(index).map_err(|_| QpackError::DecompressionFailed)?;
                    let (name, _) =
                        static_table::get(idx).ok_or(QpackError::DecompressionFailed)?;
                    headers.push((Bytes::from_static(name), value));
                } else {
                    // Dynamic table (T=0): relative index from the Base.
                    let (name, _) = self
                        .dynamic
                        .get_base_relative_bytes(base, index)
                        .ok_or(QpackError::DecompressionFailed)?;
                    needed = needed.max(base - index);
                    headers.push((name, value));
                }
            } else if header & 0x20 != 0 {
                // Literal Field Line with Literal Name (4.5.6): the N bit is
                // an instruction to peers not to index the line; it does not
                // affect decoding.
                let name = self
                    .read_string(buf, &mut off, 4, header)
                    .map_err(dec_failed)?;
                let value = self.read_value_string(buf, &mut off).map_err(dec_failed)?;
                headers.push((name, value));
            } else if header & 0x10 != 0 {
                // Indexed Field Line with Post-Base Index (4.5.3).
                let index = integer::decode(buf, &mut off, 4, header).map_err(dec_failed)?;
                let (name, value) = self
                    .dynamic
                    .get_post_base_bytes(base, index)
                    .ok_or(QpackError::DecompressionFailed)?;
                needed = needed.max(base + index + 1);
                headers.push((name, value));
            } else {
                // Literal Field Line with Post-Base Name Reference (4.5.5).
                let index = integer::decode(buf, &mut off, 3, header).map_err(dec_failed)?;
                let (name, _) = self
                    .dynamic
                    .get_post_base_bytes(base, index)
                    .ok_or(QpackError::DecompressionFailed)?;
                needed = needed.max(base + index + 1);
                let value = self.read_value_string(buf, &mut off).map_err(dec_failed)?;
                headers.push((name, value));
            }
        }
        if ric != needed {
            return Err(QpackError::DecompressionFailed);
        }
        Ok(headers)
    }

    /// Parses the Encoded Field Section Prefix (RFC 9204 Section 4.5.1):
    /// the Required Insert Count and the Base. Returns both and the number
    /// of octets consumed.
    #[inline]
    fn read_prefix(&self, buf: &[u8]) -> Result<(u64, u64, usize), QpackError> {
        let header = *buf.first().ok_or(QpackError::DecompressionFailed)?;
        let mut off = 1;
        let enc_ric = integer::decode(buf, &mut off, 8, header).map_err(dec_failed)?;
        let max_entries = self.max_capacity / 32;
        let ric = if enc_ric == 0 || max_entries == 0 {
            0
        } else {
            let full_range = 2 * max_entries;
            if enc_ric > full_range {
                return Err(QpackError::DecompressionFailed);
            }
            let max_value = self.dynamic.inserted() + max_entries;
            let max_wrapped = (max_value / full_range) * full_range;
            let mut ric = max_wrapped + enc_ric - 1;
            if ric > max_value {
                if ric <= full_range {
                    return Err(QpackError::DecompressionFailed);
                }
                ric -= full_range;
            }
            if ric == 0 {
                return Err(QpackError::DecompressionFailed);
            }
            ric
        };

        // Base (4.5.1.2): Sign = 0 means Base = Ric + Delta; Sign = 1 means
        // Base = Ric - Delta - 1.
        let header = *buf.get(off).ok_or(QpackError::DecompressionFailed)?;
        off += 1;
        let delta = integer::decode(buf, &mut off, 7, header).map_err(dec_failed)?;
        let base = if header & 0x80 != 0 {
            ric.checked_sub(delta + 1)
                .ok_or(QpackError::DecompressionFailed)?
        } else {
            ric.checked_add(delta)
                .ok_or(QpackError::DecompressionFailed)?
        };
        Ok((ric, base, off))
    }

    /// Reads an N-bit-prefix string literal (RFC 9204 Section 4.1.2) at
    /// `off`, advancing `off` past it. `header` is an already-consumed octet
    /// carrying the Huffman bit (its `prefix_bits - 1` bit) and the length
    /// prefix; for name strings it is the field line's or instruction's
    /// first octet, which pairs with the name length prefix.
    #[inline]
    fn read_string(
        &self,
        buf: &[u8],
        off: &mut usize,
        prefix_bits: u8,
        header: u8,
    ) -> Result<Bytes, HpackError> {
        let huffman = header & (1 << (prefix_bits - 1)) != 0;
        let len = integer::decode(buf, off, prefix_bits - 1, header)?;
        let len = usize::try_from(len).map_err(|_| HpackError::InvalidString)?;
        let end = (*off).checked_add(len).ok_or(HpackError::InvalidString)?;
        let src = buf.get(*off..end).ok_or(HpackError::InvalidString)?;
        *off = end;
        if huffman {
            let mut dst = Vec::with_capacity(len);
            huffman::decode(src, &mut dst)?;
            Ok(Bytes::from(dst))
        } else {
            Ok(Bytes::copy_from_slice(src))
        }
    }

    /// Reads an 8-bit-prefix string literal whose length octet is next in
    /// the buffer, advancing `off` past it. This is the form of every value
    /// string (RFC 9204 Section 4.5).
    #[inline]
    fn read_value_string(&self, buf: &[u8], off: &mut usize) -> Result<Bytes, HpackError> {
        let header = *buf.get(*off).ok_or(HpackError::InvalidString)?;
        *off += 1;
        self.read_string(buf, off, 8, header)
    }

    /// Inserts an entry named `name` with `value`, enforcing the eviction
    /// rules of RFC 9204 Sections 2.1.1 and 3.2.2: entries with an absolute
    /// index at or above the Known Received Count are not evictable, so an
    /// insert that would evict them is an encoder error.
    #[inline]
    fn insert_entry(&mut self, name: Bytes, value: Bytes) -> Result<(), QpackError> {
        let size = DynamicTable::entry_size(&name, &value);
        let evicted = self.dynamic.would_evict(size);
        if evicted > self.known_received {
            return Err(QpackError::EncoderStream);
        }
        self.dynamic
            .insert(name, value)
            .map_err(|_| QpackError::EncoderStream)
    }

    /// Raises the Known Received Count to `ric` (RFC 9204 Section 2.1.4).
    #[inline]
    fn acknowledge(&mut self, ric: u64) {
        self.known_received = self.known_received.max(ric);
    }

    /// Queues a Section Acknowledgment for `stream_id`, unless the maximum
    /// dynamic table capacity is zero (the encoder has no dynamic table
    /// references to free).
    #[inline]
    fn emit_section_ack(&mut self, stream_id: u64) {
        integer::encode(&mut self.decoder_stream, stream_id, 7, SECTION_ACK);
    }

    /// Whether `stream_id` has buffered blocked sections.
    #[inline]
    fn stream_has_blocked(&self, stream_id: u64) -> bool {
        self.blocked_by_stream
            .iter()
            .any(|(id, _)| *id == stream_id)
    }

    /// Records a newly buffered field section without rescanning the queue.
    #[inline]
    fn add_blocked_section(&mut self, stream_id: u64) {
        if let Some((_, count)) = self
            .blocked_by_stream
            .iter_mut()
            .find(|(id, _)| *id == stream_id)
        {
            *count += 1;
        } else {
            self.blocked_by_stream.push((stream_id, 1));
        }
    }

    /// Charges `size` against `stream_id`'s field-section budget and
    /// returns the stream's new cumulative total. Creates the entry on
    /// first use.
    ///
    /// `size` is the decoded name and value octets, not the size of the
    /// encoded block: Huffman encoding and index references make the two
    /// diverge arbitrarily (RFC 9114 Section 7.2.4.1).
    #[inline]
    fn account_section(&mut self, stream_id: u64, size: usize) -> usize {
        if let Some((_, total)) = self
            .section_size_by_stream
            .iter_mut()
            .find(|(id, _)| *id == stream_id)
        {
            *total = total.saturating_add(size);
            *total
        } else {
            self.section_size_by_stream.push((stream_id, size));
            size
        }
    }

    /// Removes one decoded blocked section from the per-stream count.
    #[inline]
    fn remove_blocked_section(&mut self, stream_id: u64) {
        let Some(index) = self
            .blocked_by_stream
            .iter()
            .position(|(id, _)| *id == stream_id)
        else {
            debug_assert!(false, "blocked section missing stream accounting");
            return;
        };
        if self.blocked_by_stream[index].1 == 1 {
            self.blocked_by_stream.swap_remove(index);
        } else {
            self.blocked_by_stream[index].1 -= 1;
        }
    }
}

/// Maps an `HpackError` from an encoder stream instruction to
/// `QPACK_ENCODER_STREAM_ERROR`.
#[inline]
fn enc_stream_err(_: HpackError) -> QpackError {
    QpackError::EncoderStream
}

/// Maps an `HpackError` from a field section to `QPACK_DECOMPRESSION_FAILED`.
#[inline]
fn dec_failed(_: HpackError) -> QpackError {
    QpackError::DecompressionFailed
}

/// Maps an `HpackError` from a decoder stream instruction to
/// `QPACK_DECODER_STREAM_ERROR`.
#[inline]
fn dec_stream_err(_: HpackError) -> QpackError {
    QpackError::DecoderStream
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::h3::qpack::encoder::Encoder;

    #[inline]
    fn hdr(name: &str, value: &str) -> (Bytes, Bytes) {
        (
            Bytes::copy_from_slice(name.as_bytes()),
            Bytes::copy_from_slice(value.as_bytes()),
        )
    }

    #[inline]
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn vector_b1_static_name_ref() {
        // RFC 9204 B.1, decoded: literal field line with a static name
        // reference. The decoder advertised capacity 0, so the dynamic table
        // is unused and nothing is acknowledged.
        let mut dec = Decoder::new(0, 8);
        let headers = dec
            .decode_block(b"\x00\x00\x51\x0b/index.html", 0, 0)
            .unwrap();
        assert_eq!(headers.as_deref(), Some(&[hdr(":path", "/index.html")][..]));
        assert!(dec.take_decoder_stream().is_empty());
        assert_eq!(dec.known_received(), 0);
    }

    #[test]
    fn chromium_style_capacity_round_trip() {
        // Chromium advertises SETTINGS_QPACK_MAX_TABLE_CAPACITY 65536 and
        // blocked streams 100; the server's encoder is then capped at the
        // peer's value (inheritcapacity 65536, Huffman on, as in production).
        let mut enc = Encoder::new(65536, true);
        let mut dec = Decoder::new(65536, 100);

        let response = [
            hdr(":status", "200"),
            hdr("content-type", "text/html"),
            hdr("x-custom-header", "hello-world"),
            hdr("x-another", "value-2"),
        ];
        let s1 = enc.encode_section(0, &response);
        // Reference-before-data: the section arrives first, blocks, then the
        // encoder stream unblocks it.
        assert!(dec.decode_block(&s1.block, 0, 0).unwrap().is_none());
        let unblocked = dec.feed_encoder_stream(&s1.encoder_stream).unwrap();
        assert_eq!(unblocked.len(), 1);
        assert_eq!(unblocked[0].headers.as_slice(), response.as_slice());

        // A second section referencing the entries from the first.
        let s2 = enc.encode_section(
            0,
            &[hdr(":status", "200"), hdr("x-custom-header", "hello-world")],
        );
        let unblocked2 = dec.feed_encoder_stream(&s2.encoder_stream).unwrap();
        assert!(unblocked2.is_empty());
        let out2 = dec.decode_block(&s2.block, 1, 0).unwrap().expect("decodes");
        assert_eq!(
            out2,
            &[hdr(":status", "200"), hdr("x-custom-header", "hello-world")][..]
        );
    }

    #[test]
    fn vector_b2_round_trip() {
        // RFC 9204 B.2: Set Capacity 220, two inserts, then a section
        // referencing them post-Base (Required Insert Count 2, Sign 1).
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(
            &enc.insert_with_name_ref(b":authority", b"www.example.com")
                .unwrap(),
        );
        es.extend_from_slice(&enc.insert_with_name_ref(b":path", b"/sample/path").unwrap());

        let mut dec = Decoder::new(220, 8);
        assert!(dec.feed_encoder_stream(&es).unwrap().is_empty());
        // Insert Count Increment covering both entries.
        assert_eq!(hex(&dec.take_decoder_stream()), "02");
        assert_eq!(dec.known_received(), 2);

        let headers = dec.decode_block(b"\x03\x81\x10\x11", 5, 0).unwrap();
        assert_eq!(
            headers.as_deref(),
            Some(
                &[
                    hdr(":authority", "www.example.com"),
                    hdr(":path", "/sample/path")
                ][..]
            )
        );
        // Section Acknowledgment for stream 5.
        assert_eq!(hex(&dec.take_decoder_stream()), "85");
        assert_eq!(dec.known_received(), 2);
    }

    #[test]
    fn vector_b4_round_trip() {
        // RFC 9204 B.4: relative index after a duplicate.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(
            &enc.insert_with_name_ref(b":authority", b"www.example.com")
                .unwrap(),
        );
        es.extend_from_slice(&enc.insert_with_name_ref(b":path", b"/sample/path").unwrap());
        es.extend_from_slice(&enc.insert_literal(b"custom-key", b"custom-value").unwrap());
        es.extend_from_slice(&enc.duplicate(2).unwrap());
        assert_eq!(
            hex(&es[..]),
            concat!(
                "3fbd01c00f7777772e6578616d706c652e636f6d",
                "c10c2f73616d706c652f706174684a637573746f6d2d6b65790c637573746f6d2d76616c756502"
            )
        );

        let mut dec = Decoder::new(220, 8);
        assert!(dec.feed_encoder_stream(&es).unwrap().is_empty());
        let headers = dec.decode_block(b"\x05\x00\x80", 1, 0).unwrap();
        assert_eq!(
            headers.as_deref(),
            Some(&[hdr(":authority", "www.example.com")][..])
        );
        // The feed queues an Insert Count Increment of 4 before the section's
        // acknowledgment (4 inserts arrived with nothing acknowledged yet).
        assert_eq!(hex(&dec.take_decoder_stream()), "0481");
    }

    #[test]
    fn relative_ref_round_trip() {
        // Entries inserted beforehand are referenced with a relative index.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_with_name_ref(b":path", b"/sample/path").unwrap());
        let block = enc.encode_section(0, &[hdr(":path", "/sample/path")]).block;

        let mut dec = Decoder::new(220, 8);
        assert!(dec.feed_encoder_stream(&es).unwrap().is_empty());
        let headers = dec.decode_block(&block, 2, 0).unwrap();
        assert_eq!(
            headers.as_deref(),
            Some(&[hdr(":path", "/sample/path")][..])
        );
    }

    #[test]
    fn huffman_round_trip() {
        // Huffman-encoded strings on both the encoder stream and in field
        // lines decode back to the original values.
        let mut enc = Encoder::new(220, true);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_with_name_ref(b":path", b"/sample/path").unwrap());
        let block = enc
            .encode_section(
                0,
                &[hdr(":path", "www.example.com/aaaaaaaaaaaaaaaaaaaaaaaaa")],
            )
            .block;

        let mut dec = Decoder::new(220, 8);
        assert!(dec.feed_encoder_stream(&es).unwrap().is_empty());
        let headers = dec.decode_block(&block, 2, 0).unwrap();
        assert_eq!(
            headers.as_deref(),
            Some(&[hdr(":path", "www.example.com/aaaaaaaaaaaaaaaaaaaaaaaaa")][..])
        );
    }

    #[test]
    fn fresh_name_inserted_then_referenced() {
        // The encoder inserts a new name mid-section and references it
        // post-Base; the decoder must materialize it from the encoder stream
        // that accompanies the block.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let out = enc.encode_section(0, &[hdr("custom-key", "custom-value")]);

        let mut dec = Decoder::new(220, 8);
        assert!(dec
            .feed_encoder_stream(&out.encoder_stream)
            .unwrap()
            .is_empty());
        let headers = dec.decode_block(&out.block, 4, 0).unwrap();
        assert_eq!(
            headers.as_deref(),
            Some(&[hdr("custom-key", "custom-value")][..])
        );
        assert_eq!(hex(&dec.take_decoder_stream()), "0184");
    }

    #[test]
    fn blocked_then_unblocked() {
        // The decoder receives a section before the encoder stream data that
        // unblocks it; the section is buffered and returned by the feed.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"custom-key", b"custom-value").unwrap());
        let block = enc
            .encode_section(0, &[hdr("custom-key", "custom-value")])
            .block;

        let mut dec = Decoder::new(220, 8);
        assert!(dec.decode_block(&block, 7, 100).unwrap().is_none());
        assert_eq!(dec.pending_blocked(), 1);
        assert!(dec.take_decoder_stream().is_empty());

        let sections = dec.feed_encoder_stream(&es).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].stream_id, 7);
        assert_eq!(sections[0].headers, vec![hdr("custom-key", "custom-value")]);
        assert_eq!(dec.pending_blocked(), 0);
        // Section Acknowledgment for stream 7; the increment is subsumed by
        // the acknowledgment (Known Received Count reaches the count).
        assert_eq!(hex(&dec.take_decoder_stream()), "87");
        assert_eq!(dec.known_received(), 1);
    }

    #[test]
    fn blocked_stream_limit() {
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"custom-key", b"custom-value").unwrap());
        let block = enc
            .encode_section(0, &[hdr("custom-key", "custom-value")])
            .block;

        let mut dec = Decoder::new(220, 1);
        assert!(dec.decode_block(&block, 1, 0).unwrap().is_none());
        assert_eq!(
            dec.decode_block(&block, 2, 0),
            Err(QpackError::DecompressionFailed),
            "more blocked streams than SETTINGS_QPACK_BLOCKED_STREAMS"
        );
        // The encodings that were not the blocked section are still usable.
        drop(es);
    }

    #[test]
    fn same_stream_sections_stay_ordered() {
        // A second section on a blocked stream is queued behind the first,
        // even when it could be decoded already; both are returned in order
        // by the feed.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(
            &enc.insert_with_name_ref(b":authority", b"www.example.com")
                .unwrap(),
        );
        es.extend_from_slice(&enc.insert_with_name_ref(b":path", b"/sample/path").unwrap());
        let first = enc
            .encode_section(0, &[hdr(":authority", "www.example.com")])
            .block;
        let second = enc.encode_section(0, &[hdr(":method", "GET")]).block;

        let mut dec = Decoder::new(220, 8);
        assert!(dec.decode_block(&first, 3, 0).unwrap().is_none());
        assert!(dec.decode_block(&second, 3, 0).unwrap().is_none());
        assert_eq!(dec.pending_blocked(), 2);

        let sections = dec.feed_encoder_stream(&es).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(
            sections[0].headers,
            vec![hdr(":authority", "www.example.com")]
        );
        assert_eq!(sections[1].headers, vec![hdr(":method", "GET")]);
        // One Section Acknowledgment (the second section has RIC 0), plus an
        // Increment: the ack raised the Known Received Count to 1, but two
        // entries were inserted.
        assert_eq!(hex(&dec.take_decoder_stream()), "8301");
    }

    #[test]
    fn static_only_section_on_populated_table() {
        // A section with no dynamic references announces RIC 0 and decodes
        // immediately even when the decoder has received no inserts yet.
        let mut dec = Decoder::new(220, 8);
        let headers = dec.decode_block(b"\x00\x01\xd1", 0, 0).unwrap();
        assert_eq!(headers.as_deref(), Some(&[hdr(":method", "GET")][..]));
        assert!(dec.take_decoder_stream().is_empty());
    }

    #[test]
    fn literals_with_literal_name() {
        // Literal Field Line with Literal Name, sensistive (N=1) and not.
        let mut dec = Decoder::new(0, 8);
        let headers = dec
            .decode_block(b"\x00\x00\x23foo\x03bar\x33foo\x03baz", 0, 0)
            .unwrap();
        assert_eq!(
            headers.as_deref(),
            Some(&[hdr("foo", "bar"), hdr("foo", "baz")][..])
        );
    }

    #[test]
    fn post_base_literal_name_ref() {
        // Literal Field Line with Post-Base Name Reference (4.5.5): Base 0,
        // post-Base index 0, literal value.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"foo", b"bar").unwrap());

        let mut dec = Decoder::new(220, 8);
        assert!(dec.feed_encoder_stream(&es).unwrap().is_empty());
        let headers = dec.decode_block(b"\x02\x80\x00\x03bar", 1, 0).unwrap();
        assert_eq!(headers.as_deref(), Some(&[hdr("foo", "bar")][..]));
    }

    #[test]
    fn empty_section() {
        let mut dec = Decoder::new(220, 8);
        assert_eq!(dec.decode_block(b"\x00\x00", 0, 0).unwrap(), Some(vec![]));
        assert!(dec.take_decoder_stream().is_empty());
    }

    #[test]
    fn field_section_size_capped() {
        // A field section whose decoded size alone exceeds the advertised
        // SETTINGS_MAX_FIELD_SECTION_SIZE is a decompression failure (RFC
        // 9204 Section 4.5). The cap counts decoded names and values, not
        // the encoded block.
        let mut dec = Decoder::new(220, 8);
        dec.set_max_field_section_size(10);
        // Name "x" (1) + value of 10 octets: 11, one over the cap.
        assert_eq!(
            dec.decode_block(b"\x00\x00\x21x\x0ayyyyyyyyyy", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
        // A section of exactly the cap decodes.
        let headers = dec.decode_block(b"\x00\x00\x21x\tyyyyyyyyy", 1, 0).unwrap();
        assert_eq!(headers.as_deref(), Some(&[hdr("x", "yyyyyyyyy")][..]));
        // Static table lines count too: content-length: 0 is 15 octets.
        assert_eq!(
            dec.decode_block(b"\x00\x00\xc4", 2, 0),
            Err(QpackError::DecompressionFailed)
        );
    }

    #[test]
    fn field_section_size_budget_is_per_stream() {
        // The SETTINGS_MAX_FIELD_SECTION_SIZE budget accumulates across a
        // stream's field sections (request headers and trailers), like
        // SETTINGS_MAX_HEADER_LIST_SIZE does for HTTP/2, rather than
        // capping each section on its own.
        let mut dec = Decoder::new(220, 8);
        dec.set_max_field_section_size(20);
        // :method GET (10) + :authority (10): exactly the budget.
        let headers = dec.decode_block(b"\x00\x01\xd1\xc0", 3, 0).unwrap();
        assert_eq!(
            headers.as_deref(),
            Some(&[hdr(":method", "GET"), hdr(":authority", "")][..])
        );
        // A second section on the same stream, small on its own, pushes the
        // stream over the budget.
        assert_eq!(
            dec.decode_block(b"\x00\x00\x21a\x01b", 3, 0),
            Err(QpackError::DecompressionFailed)
        );
        // Another stream starts with a fresh budget.
        let headers = dec.decode_block(b"\x00\x00\x21a\x01b", 5, 0).unwrap();
        assert_eq!(headers.as_deref(), Some(&[hdr("a", "b")][..]));
    }

    #[test]
    fn stream_finished_releases_budget() {
        let mut dec = Decoder::new(220, 8);
        dec.set_max_field_section_size(10);
        // :method GET (10): exactly the budget.
        assert!(dec.decode_block(b"\x00\x01\xd1", 2, 0).unwrap().is_some());
        assert_eq!(
            dec.decode_block(b"\x00\x00\x21a\x01b", 2, 0),
            Err(QpackError::DecompressionFailed)
        );
        // The HTTP/3 layer reports the stream finished (peer closed its
        // send side): the budget is released.
        dec.stream_finished(2);
        assert!(dec
            .decode_block(b"\x00\x00\x21a\x01b", 2, 0)
            .unwrap()
            .is_some());
    }

    #[test]
    fn stream_cancelled_releases_budget() {
        let mut dec = Decoder::new(220, 8);
        dec.set_max_field_section_size(10);
        assert!(dec.decode_block(b"\x00\x01\xd1", 4, 0).unwrap().is_some());
        assert_eq!(hex(&dec.stream_cancelled(4)), "44");
        // The reset stream's budget is gone; sections fit again.
        assert!(dec
            .decode_block(b"\x00\x00\x21a\x01b", 4, 0)
            .unwrap()
            .is_some());
    }

    #[test]
    fn blocked_section_size_capped_on_unblock() {
        // The cap is enforced when the section is decoded, so a section
        // buffered as blocked is rejected by the feed once the encoder
        // stream unblocks it.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"custom-key", b"custom-value").unwrap());
        let block = enc
            .encode_section(0, &[hdr("custom-key", "custom-value")])
            .block;

        let mut dec = Decoder::new(220, 8);
        dec.set_max_field_section_size(10);
        // custom-key (10) + custom-value (12) = 22, but the section is
        // buffered before its decoded size is known.
        assert!(dec.decode_block(&block, 7, 0).unwrap().is_none());
        assert_eq!(
            dec.feed_encoder_stream(&es),
            Err(QpackError::DecompressionFailed)
        );
    }

    #[test]
    fn blocked_section_charges_the_stream_budget() {
        // A section unblocked later charges the stream's budget: it can
        // push a stream that already used part of its budget over the cap.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"custom-key", b"custom-value").unwrap());
        let block = enc
            .encode_section(0, &[hdr("custom-key", "custom-value")])
            .block;

        let mut dec = Decoder::new(220, 8);
        dec.set_max_field_section_size(30);
        // :method GET (10) on the stream, then a blocked 22-octet section.
        assert!(dec.decode_block(b"\x00\x01\xd1", 7, 0).unwrap().is_some());
        assert!(dec.decode_block(&block, 7, 0).unwrap().is_none());
        // 10 + 22 > 30: the stream is over its budget once unblocked.
        assert_eq!(
            dec.feed_encoder_stream(&es),
            Err(QpackError::DecompressionFailed)
        );

        // The same 22-octet section alone fits on a fresh stream.
        let mut dec = Decoder::new(220, 8);
        dec.set_max_field_section_size(30);
        assert!(dec.decode_block(&block, 9, 0).unwrap().is_none());
        let sections = dec.feed_encoder_stream(&es).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].headers, vec![hdr("custom-key", "custom-value")]);
    }

    #[test]
    fn ric_wrap_round_trip() {
        // 15 inserts at capacity 220 wrap the 8-bit Required Insert Count
        // (2 * MaxEntries = 12): entry 15 encodes as (15 mod 12) + 1 = 4.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es: Vec<Vec<u8>> = Vec::new();
        es.push(enc.set_capacity(220).unwrap().to_vec());
        for i in 0..15 {
            es.push(
                enc.insert_literal(format!("h{i}").as_bytes(), b"v")
                    .unwrap()
                    .to_vec(),
            );
            // Acknowledge each insert like a peer decoder would, so later
            // inserts can turn the table over.
            enc.feed_decoder_stream(&[1]).unwrap();
        }
        let block = enc.encode_section(0, &[hdr("h14", "v")]).block;
        assert_eq!(&block[..2], &[4, 0], "enc_ric 4, Sign 0 Delta 0");

        let mut dec = Decoder::new(220, 8);
        // Chunked like the wire: the decoder acknowledges each batch, so
        // eviction inside a later batch only reaches acknowledged entries.
        for batch in es.chunks(6) {
            let mut joined = Vec::new();
            for part in batch {
                joined.extend_from_slice(part);
            }
            assert!(dec.feed_encoder_stream(&joined).unwrap().is_empty());
        }
        assert_eq!(dec.inserted(), 15);
        assert_eq!(dec.known_received(), 15);
        let headers = dec.decode_block(&block, 9, 0).unwrap();
        assert_eq!(headers.as_deref(), Some(&[hdr("h14", "v")][..]));
    }

    #[test]
    fn increment_coalesced() {
        // Insert Count Increments cover exactly the new entries per batch.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut set = Vec::new();
        set.extend_from_slice(&enc.set_capacity(220).unwrap());
        let a = enc.insert_literal(b"a", b"a").unwrap();
        let b = enc.insert_literal(b"b", b"b").unwrap();
        let c = enc.insert_literal(b"c", b"c").unwrap();

        let mut dec = Decoder::new(220, 8);
        assert!(dec.feed_encoder_stream(&set).unwrap().is_empty());
        assert!(dec.feed_encoder_stream(&a).unwrap().is_empty());
        assert_eq!(hex(&dec.take_decoder_stream()), "01");
        let mut bc = b.to_vec();
        bc.extend_from_slice(&c);
        assert!(dec.feed_encoder_stream(&bc).unwrap().is_empty());
        assert_eq!(hex(&dec.take_decoder_stream()), "02");
        assert_eq!(dec.known_received(), 3);
    }

    #[test]
    fn stream_cancelled_drops_blocked() {
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"custom-key", b"custom-value").unwrap());
        let block = enc
            .encode_section(0, &[hdr("custom-key", "custom-value")])
            .block;

        let mut dec = Decoder::new(220, 8);
        assert!(dec.decode_block(&block, 9, 0).unwrap().is_none());
        assert_eq!(hex(&dec.stream_cancelled(9)), "49");
        assert_eq!(dec.pending_blocked(), 0);
        // The dropped section is gone: no headers surface from the feed.
        assert!(dec.feed_encoder_stream(&es).unwrap().is_empty());
    }

    #[test]
    fn expire_blocked_emits_cancellations() {
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"custom-key", b"custom-value").unwrap());
        let block = enc
            .encode_section(0, &[hdr("custom-key", "custom-value")])
            .block;

        let mut dec = Decoder::new(220, 8);
        assert!(dec.decode_block(&block, 4, 1000).unwrap().is_none());
        assert!(dec.decode_block(&block, 6, 1200).unwrap().is_none());
        // Not expired yet: age is exactly the maximum.
        assert!(dec.expire_blocked(1500, 500).is_empty());
        assert_eq!(dec.pending_blocked(), 2);
        // Stream 4 is older (age 600) and expires; stream 6 (age 400) stays.
        assert_eq!(hex(&dec.expire_blocked(1600, 500)), "44");
        assert_eq!(dec.pending_blocked(), 1);
        // Stream 6 now ages past the maximum.
        assert_eq!(hex(&dec.expire_blocked(1701, 500)), "46");
        assert_eq!(dec.pending_blocked(), 0);
        drop(es);
    }

    #[test]
    fn eviction_into_unacknowledged_rejected() {
        // Evicting an entry with an absolute index at or above the Known
        // Received Count is an encoder stream error (RFC 9204 Section 2.1.1).
        // The encoder refuses to emit such an insert, so this feeds a
        // hostile peer's bytes directly.
        let mut dec = Decoder::new(88, 8);
        // Set Dynamic Table Capacity 88, insert (a,a) and (b,b) — the table
        // holds two 34-byte entries — then insert (c,c), which evicts the
        // never-acknowledged first entry.
        let es = [
            0x3f, 0x39, // Set Dynamic Table Capacity 88
            0x41, b'a', 0x01, b'a', // Insert with Literal Name (a,a)
            0x41, b'b', 0x01, b'b', 0x41, b'c', 0x01, b'c',
        ];
        assert_eq!(dec.feed_encoder_stream(&es), Err(QpackError::EncoderStream));
    }

    #[test]
    fn decoder_stream_section_ack_classifies_high_stream_id() {
        // A Section Acknowledgment for stream ID 64 encodes to `0xC0`; the
        // validation parser must accept it as a Section Acknowledgment
        // (high bit set) rather than misreading it as an Insert Count
        // Increment with a forbidden zero value (RFC 9204 Section 4.4.1).
        let mut dec = Decoder::new(0, 8);
        assert!(dec.feed_decoder_stream(&[0xC0]).is_ok());
        // Stream Cancellation (0x44 = Stream ID 4) and a non-zero Insert
        // Count Increment (0x01) validate too.
        assert!(dec.feed_decoder_stream(&[0x44]).is_ok());
        assert!(dec.feed_decoder_stream(&[0x01]).is_ok());
        // A zero Insert Count Increment is rejected.
        assert_eq!(
            dec.feed_decoder_stream(&[0x00]),
            Err(QpackError::DecoderStream)
        );
    }

    #[test]
    fn capacity_eviction_guard() {
        // A capacity reduction in the same batch as the inserts it evicts is
        // rejected while nothing is acknowledged.
        let mut dec = Decoder::new(220, 8);
        let es = [
            0x3f, 0xbd, 0x01, // Set Dynamic Table Capacity 220
            0x41, b'a', 0x01, b'a', // Insert with Literal Name (a,a)
            0x41, b'b', 0x01, b'b', 0x3f, 0x03, // Set Dynamic Table Capacity 34
        ];
        assert_eq!(dec.feed_encoder_stream(&es), Err(QpackError::EncoderStream));

        // The same reduction is fine once the entries are acknowledged.
        let mut dec = Decoder::new(220, 8);
        let first = [
            0x3f, 0xbd, 0x01, 0x41, b'a', 0x01, b'a', 0x41, b'b', 0x01, b'b',
        ];
        assert!(dec.feed_encoder_stream(&first).unwrap().is_empty());
        assert!(dec.feed_encoder_stream(&[0x3f, 0x03]).unwrap().is_empty());
        assert_eq!(dec.dynamic.len(), 1);
        assert_eq!(
            dec.dynamic.get_absolute(1),
            Some((b"b".as_slice(), b"b".as_slice()))
        );
    }

    #[test]
    fn capacity_above_advertised_rejected() {
        let mut dec = Decoder::new(64, 8);
        assert_eq!(
            dec.feed_encoder_stream(&[0x3f, 0x22]),
            Err(QpackError::EncoderStream),
            "Set Dynamic Table Capacity 65 exceeds the advertised maximum"
        );
    }

    #[test]
    fn duplicate_out_of_range() {
        let mut dec = Decoder::new(220, 8);
        assert_eq!(
            dec.feed_encoder_stream(&[0x00]),
            Err(QpackError::EncoderStream)
        );
    }

    #[test]
    fn encoder_stream_refs_evicted_entry() {
        // Insert with Name Reference to an entry evicted by a capacity
        // reduction is an encoder stream error (RFC 9204 Section 3.2.2).
        // The encoder refuses to emit such an insert, so this feeds a
        // hostile peer's bytes directly.
        let mut dec = Decoder::new(220, 8);
        // Set Dynamic Table Capacity 220, insert (a,a), (b,b), (c,c), then
        // reduce the capacity to 34 so only c survives.
        assert!(dec
            .feed_encoder_stream(&[
                0x3f, 0xbd, 0x01, 0x41, b'a', 0x01, b'a', 0x41, b'b', 0x01, b'b', 0x41, b'c', 0x01,
                b'c',
            ])
            .unwrap()
            .is_empty());
        assert!(dec.feed_encoder_stream(&[0x3f, 0x03]).unwrap().is_empty());
        assert_eq!(dec.dynamic.len(), 1);
        assert_eq!(
            dec.dynamic.get_absolute(2),
            Some((b"c".as_slice(), b"c".as_slice()))
        );
        // An Insert with Name Reference to the evicted entry (relative
        // index 1, the second-newest at insert time) is rejected.
        let mut ref_instr = Vec::new();
        integer::encode(&mut ref_instr, 1, 6, INSERT_WITH_NAME_REF);
        ref_instr.push(1);
        ref_instr.push(b'v');
        assert_eq!(
            dec.feed_encoder_stream(&ref_instr),
            Err(QpackError::EncoderStream)
        );
    }

    #[test]
    fn static_index_out_of_range() {
        let mut dec = Decoder::new(0, 8);
        // Indexed Field Line, static table, index 99: 0xff is the 6-bit
        // prefix in "all ones" form, followed by the continuation 36.
        assert_eq!(
            dec.decode_block(b"\x00\x00\xff\x24", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
    }

    #[test]
    fn relative_index_out_of_range() {
        // Base is 0, so no dynamic relative index is valid.
        let mut dec = Decoder::new(220, 8);
        assert_eq!(
            dec.decode_block(b"\x00\x00\x80", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
    }

    #[test]
    fn post_base_beyond_table() {
        // One insert received, but the section references post-Base index 5.
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"a", b"a").unwrap());
        let mut dec = Decoder::new(220, 8);
        assert!(dec.feed_encoder_stream(&es).unwrap().is_empty());
        assert_eq!(
            dec.decode_block(b"\x02\x80\x15", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
    }

    #[test]
    fn ric_mismatch_rejected() {
        // Announce a Required Insert Count of 2 for a section that references
        // only one entry (RFC 9204 Section 2.1.2: RIC is one larger than the
        // largest referenced absolute index).
        let mut enc = crate::h3::qpack::encoder::Encoder::new(220, false);
        let mut es = Vec::new();
        es.extend_from_slice(&enc.set_capacity(220).unwrap());
        es.extend_from_slice(&enc.insert_literal(b"a", b"a").unwrap());
        es.extend_from_slice(&enc.insert_literal(b"b", b"b").unwrap());
        let mut dec = Decoder::new(220, 8);
        assert!(dec.feed_encoder_stream(&es).unwrap().is_empty());
        assert_eq!(
            dec.decode_block(b"\x03\x00\x81", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
        // And the reverse: RIC 0 announced for a section that references the
        // newly inserted entry.
        assert_eq!(
            dec.decode_block(b"\x00\x01\x80", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
    }

    #[test]
    fn truncated_inputs_do_not_panic() {
        // A string literal that promises more bytes than remain.
        let mut dec = Decoder::new(220, 8);
        assert_eq!(
            dec.feed_encoder_stream(b"\x4aab"),
            Err(QpackError::EncoderStream)
        );
        assert_eq!(
            dec.decode_block(b"\x00\x00\x51\x0aabc", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
        // A section that ends mid-field-line.
        assert_eq!(
            dec.decode_block(b"\x00\x00\x80", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
        // A half a prefix.
        assert_eq!(
            dec.decode_block(b"\x00", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
        // Base underflow: Sign 1 with a delta larger than the insert count.
        assert_eq!(
            dec.decode_block(b"\x02\xff\x00", 0, 0),
            Err(QpackError::DecompressionFailed)
        );
    }

    #[test]
    fn multi_section_round_trip() {
        // Two sections referencing entries inserted by the first, with a
        // static-only section in between.
        let mut enc = Encoder::new(300, false);
        let first = enc.encode_section(
            0,
            &[
                hdr("custom-key", "custom-value"),
                hdr("x-more", "0123456789"),
            ],
        );
        let second = enc.encode_section(0, &[hdr("custom-key", "custom-value")]);
        let third = enc.encode_section(0, &[hdr(":method", "GET")]);

        let mut dec = Decoder::new(300, 8);
        assert!(dec
            .feed_encoder_stream(&first.encoder_stream)
            .unwrap()
            .is_empty());
        let headers = dec.decode_block(&first.block, 1, 0).unwrap();
        assert_eq!(
            headers.as_deref(),
            Some(
                &[
                    hdr("custom-key", "custom-value"),
                    hdr("x-more", "0123456789")
                ][..]
            )
        );
        assert!(dec
            .feed_encoder_stream(&second.encoder_stream)
            .unwrap()
            .is_empty());
        assert_eq!(
            dec.decode_block(&second.block, 1, 0).unwrap().as_deref(),
            Some(&[hdr("custom-key", "custom-value")][..])
        );
        assert_eq!(
            dec.decode_block(&third.block, 1, 0).unwrap().as_deref(),
            Some(&[hdr(":method", "GET")][..])
        );
        // First section acked: stream 1; second too. The increment merged
        // into the first acknowledgment... plus the second.
        assert_eq!(dec.known_received(), 2);
    }
}
