//! QPACK encoder (RFC 9204 Sections 4.3 and 4.5).
//!
//! Consumption: the HTTP/3 layer drives the encoder per connection (it feeds
//! field sections and drains the encoder stream); until that lands, the whole
//! module is dead in non-test builds, which is why `dead_code` is expected
//! here. It errors again once the encoder is used, reminding us to remove the
//! expectation.
//!
//! The encoder owns the shared dynamic table (Section 4.2): it alone adds
//! entries, and it emits an unframed sequence of instructions on the encoder
//! stream (Section 4.3) describing every mutation. Field sections are encoded
//! as a Required Insert Count, a Base, and one or more field line
//! representations (Section 4.5).
//!
//! The encoder fixes the Base at the dynamic table insertion count before
//! encoding a section, so every reference to entries that existed before the
//! section is relative, and entries inserted while encoding the section are
//! referenced with post-Base indexes (Section 4.5.1.2 recommends exactly this:
//! Base equal to the Required Insert Count, which makes the Sign bit and the
//! Delta Base zero whenever nothing new is referenced).
//!
//! Inserts are speculative but bounded: an entry is only inserted when it
//! fits and when the eviction it would cause cannot invalidate an index
//! already referenced by the section being encoded, nor remove an entry the
//! decoder still needs. The decoder's acknowledgments (Section 4.4
//! instructions on its decoder stream) raise a Known Received Count and free
//! the references of acknowledged field sections; an entry is evictable only
//! below the lower of the Known Received Count and the smallest reference
//! still outstanding (RFC 9204 Sections 2.1.1 and 2.1.4). The encoder never
//! evicts above that floor, so the peer's decoder never has to reject an
//! insert as a QPACK_ENCODER_STREAM_ERROR.
//!
//! Huffman encoding follows RFC 9204 Section 4.1.2: a string is Huffman
//! encoded when that is shorter, matching HPACK practice.
#![expect(dead_code)]

use std::collections::VecDeque;

use bytes::Bytes;

use crate::h3::qpack::table::DynamicTable;
use crate::h3::qpack::{static_table, QpackError};
use crate::hpack::{huffman, integer};

/// `001` + 5-bit capacity: Set Dynamic Table Capacity (RFC 9204 4.3.1).
const SET_CAPACITY: u8 = 0b0010_0000;
/// `1 T` + 6-bit name index: Insert with Name Reference (RFC 9204 4.3.2).
const INSERT_WITH_NAME_REF: u8 = 0b1000_0000;
/// `01` + H + 5-bit name length: Insert with Literal Name (RFC 9204 4.3.3).
const INSERT_WITH_LITERAL_NAME: u8 = 0b0100_0000;
/// `000` + 5-bit relative index: Duplicate (RFC 9204 4.3.4).
const DUPLICATE: u8 = 0b0000_0000;

/// `1` + 7-bit stream ID: Section Acknowledgment (RFC 9204 4.4.1).
const SECTION_ACK: u8 = 0x80;
/// `01` + 6-bit stream ID: Stream Cancellation (RFC 9204 4.4.2).
const STREAM_CANCELLATION: u8 = 0x40;
/// `00` + 6-bit increment: Insert Count Increment (RFC 9204 4.4.3).
const INSERT_COUNT_INCREMENT: u8 = 0x00;

/// `1 T` + 6-bit index: Indexed Field Line (RFC 9204 4.5.2).
const INDEXED: u8 = 0b1000_0000;
/// `0001` + 4-bit post-Base index: Indexed Field Line with Post-Base Index
/// (RFC 9204 4.5.3).
const INDEXED_POST_BASE: u8 = 0b0001_0000;
/// `01 N T` + 4-bit name index: Literal Field Line with Name Reference
/// (RFC 9204 4.5.4).
const LITERAL_NAME_REF: u8 = 0b0100_0000;
/// `0000 N` + 3-bit post-Base name index: Literal Field Line with Post-Base
/// Name Reference (RFC 9204 4.5.5).
const LITERAL_POST_BASE_NAME_REF: u8 = 0b0000_0000;
/// `001 N` + H + 3-bit name length: Literal Field Line with Literal Name
/// (RFC 9204 4.5.6).
const LITERAL_LITERAL_NAME: u8 = 0b0010_0000;

/// Field names that must never be added to the dynamic table and must always
/// be sent as literal field lines with the N bit set (RFC 9204 Section 7.1).
const NEVER_INDEXED: [&[u8]; 3] = [b"authorization", b"proxy-authorization", b"cookie"];

/// Result of encoding one field section: the encoded field section carried on
/// the request/response stream, and any encoder stream instructions queued
/// while encoding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSection {
    /// Encoded field section prefix plus field lines (RFC 9204 Section 4.5.1).
    pub block: Bytes,
    /// Encoder stream instructions (RFC 9204 Section 4.3).
    pub encoder_stream: Bytes,
}

/// QPACK encoder: dynamic table owner and field section encoder.
#[derive(Debug)]
pub struct Encoder {
    dynamic: DynamicTable,
    /// Upper bound on the dynamic table capacity allowed by the decoder's
    /// SETTINGS_QPACK_MAX_TABLE_CAPACITY (RFC 9204 Section 5).
    max_capacity: u64,
    /// Whether string literals are Huffman encoded when that is shorter.
    huffman: bool,
    /// Number of dynamic table insertions and duplications acknowledged by
    /// the decoder (RFC 9204 Section 2.1.4). Absolute indexes below it are
    /// acknowledged and can be evicted.
    known_received: u64,
    /// Smallest absolute index referenced by each unacknowledged field
    /// section that has dynamic references, in send order (RFC 9204
    /// Sections 2.1.1 and 4.4.1). A section is freed by a Section
    /// Acknowledgment for its stream or by a Stream Cancellation.
    pending_refs: VecDeque<(u64, u64)>,
}

impl Encoder {
    /// Creates an encoder bound to a decoder that advertised the given
    /// `max_capacity` in SETTINGS_QPACK_MAX_TABLE_CAPACITY.
    #[inline]
    pub fn new(max_capacity: u64, huffman: bool) -> Self {
        Self {
            dynamic: DynamicTable::new(0),
            max_capacity,
            huffman,
            known_received: 0,
            pending_refs: VecDeque::new(),
        }
    }

    /// The maximum dynamic table capacity permitted by the decoder.
    #[inline]
    pub fn max_capacity(&self) -> u64 {
        self.max_capacity
    }

    /// The lowest absolute index at or above which entries are NOT
    /// evictable: the lower of the Known Received Count and the smallest
    /// reference of any unacknowledged field section (RFC 9204
    /// Sections 2.1.1 and 2.1.4). An insert whose eviction would reach it
    /// is skipped.
    #[inline]
    fn evictable_floor(&self) -> u64 {
        let pending = self
            .pending_refs
            .iter()
            .map(|(_, min_ref)| *min_ref)
            .min()
            .unwrap_or(u64::MAX);
        self.known_received.min(pending)
    }

    /// Encodes `headers` for the field section on `stream_id` into an
    /// encoded field section plus the encoder stream instructions the
    /// decoder needs to process it.
    ///
    /// The dynamic table is used only when the decoder allows a capacity of
    /// at least one entry (RFC 9204 Section 3.2.3): a maximum capacity below
    /// 32 bytes cannot hold any entry and disables the dynamic table.
    #[inline]
    pub fn encode_section(&mut self, stream_id: u64, headers: &[(Bytes, Bytes)]) -> EncodedSection {
        self.encode_section_with_base(stream_id, headers, self.dynamic.inserted())
    }

    /// Encodes `headers` using the decoder's **acknowledged** insert count
    /// (`known_received`) as the QPACK Base.
    ///
    /// RFC 9204 Section 2.1.2 forbids a relative reference to a dynamic entry
    /// whose absolute index exceeds the Largest Reference the decoder has
    /// acknowledged: such an entry must instead be referenced with a Post-Base
    /// index. The shared encoder's own insert count (`dynamic.inserted()`) can
    /// run far ahead of what the peer has acknowledged — for instance when the
    /// client is busy consuming a large response body and lags on its
    /// decoder-stream acknowledgments. Encoding against the insert count there
    /// would emit relative references above the peer's Largest Reference, which
    /// a strict decoder (e.g. Neqo/Firefox) treats as a QPACK decompression
    /// failure, tearing down the whole connection and dropping every other in-
    /// flight request. Encoding against `known_received` keeps every relative
    /// reference within the acknowledged range and uses Post-Base for anything
    /// newer, so the section decodes correctly even if the peer ACKs late.
    #[inline]
    pub(crate) fn encode_section_with_ack_base(
        &mut self,
        stream_id: u64,
        headers: &[(Bytes, Bytes)],
    ) -> EncodedSection {
        self.encode_section_with_base(stream_id, headers, self.known_received)
    }

    /// The insert count a decoder must have reached to process the most
    /// recently encoded section.
    #[inline]
    pub(crate) fn required_insert_count(&self) -> u64 {
        self.dynamic.inserted()
    }

    #[inline]
    fn encode_section_with_base(
        &mut self,
        stream_id: u64,
        headers: &[(Bytes, Bytes)],
        base: u64,
    ) -> EncodedSection {
        let usable = self.max_capacity >= 32;
        let mut encoder_stream = Vec::new();

        let mut block = Vec::new();
        // Smallest absolute index referenced by the section so far; an insert
        // is skipped when its eviction would reach it.
        let mut min_rel_ref: Option<u64> = None;
        // Required Insert Count: one larger than the largest absolute index
        // of all dynamic table entries referenced by the section, and 0 when
        // none are referenced (RFC 9204 Section 2.1.2). It only grows with
        // dynamic references, so a section that happens to reference only
        // static entries does not inflate it.
        let mut ric = 0u64;

        for (name, value) in headers {
            let sensitive = NEVER_INDEXED.contains(&name.as_ref());

            if sensitive {
                self.encode_literal(name, value, true, &mut block);
                continue;
            }

            // Full match, dynamic table first (newest first), then static.
            // Preserve the corresponding name matches so the literal path
            // below does not repeat either table scan.
            let dynamic_match = usable.then(|| self.dynamic.find_full_or_name(name, value));
            if usable {
                if let Some(abs) = dynamic_match.and_then(|(full, _)| full) {
                    self.encode_indexed(abs, base, &mut block, &mut ric, &mut min_rel_ref);
                    continue;
                }
            }
            let static_match = static_table::find_full_or_name(name, value);
            if let Some(idx) = static_match.0 {
                // Indexed Field Line, static table (T=1).
                integer::encode(&mut block, idx as u64, 6, INDEXED | 0x40);
                continue;
            }

            if usable {
                if let Some(abs) = dynamic_match.and_then(|(_, name)| name) {
                    if abs >= base {
                        // Post-Base name reference (4.5.5). Only reachable on
                        // the vector path, which fixes Base below the insert
                        // count.
                        self.encode_literal_post_base_name_ref(
                            abs,
                            base,
                            value,
                            &mut block,
                            &mut ric,
                            &mut min_rel_ref,
                        );
                    } else {
                        // Literal with Name Reference, dynamic table (T=0).
                        self.encode_literal_with_name_ref(
                            abs,
                            base,
                            value,
                            &mut block,
                            &mut ric,
                            &mut min_rel_ref,
                        );
                    }
                    continue;
                }
            }
            if let Some(idx) = static_match.1 {
                // Literal with Name Reference, static table (T=1).
                self.encode_literal_with_static_name_ref(idx, value, &mut block);
                continue;
            }

            // No name match anywhere: emit a literal name, and insert the
            // entry so later sections can reference it (Section 4.4).
            let size = DynamicTable::entry_size(name, value);
            // An insert only evicts the oldest entries, so it is allowed
            // when the first surviving entry sits at or below every entry
            // the section references and everything the decoder still
            // needs (RFC 9204 Section 2.1.1): evicting below that floor is
            // what the decoder's mirror of this table permits.
            let boundary = min_rel_ref.unwrap_or(u64::MAX).min(self.evictable_floor());
            let safe = self.dynamic.inserted() - self.dynamic.len() as u64
                + self.dynamic.would_evict(size)
                <= boundary;
            if usable && size <= self.max_capacity && safe {
                // Set Dynamic Table Capacity (4.3.1), emitted lazily right
                // before the first insert of this section.
                if self.dynamic.capacity() != self.max_capacity {
                    integer::encode(&mut encoder_stream, self.max_capacity, 5, SET_CAPACITY);
                    self.dynamic.set_capacity(self.max_capacity);
                }
                let abs = self.dynamic.next_absolute();
                // Insert with Literal Name (4.3.3): the name length uses a
                // 5-bit prefix, so `push_string` receives 6 (it reserves one
                // bit for the Huffman flag).
                self.push_string(&mut encoder_stream, name, 6, INSERT_WITH_LITERAL_NAME);
                self.push_string(&mut encoder_stream, value, 8, 0);
                let _ = self.dynamic.insert(name.clone(), value.clone());
                // The fresh entry is referenced with a post-Base index.
                self.encode_indexed(abs, base, &mut block, &mut ric, &mut min_rel_ref);
            } else {
                self.encode_literal(name, value, false, &mut block);
            }
        }

        // A section with dynamic references pins the referenced entries
        // until the decoder acknowledges it (Section 4.4.1).
        if ric > 0 {
            self.pending_refs
                .push_back((stream_id, min_rel_ref.unwrap_or(0)));
        }

        // Encoded Field Section Prefix (RFC 9204 4.5.1).
        let mut prefix = Vec::new();
        self.encode_prefix(&mut prefix, ric, base);
        prefix.reserve(block.len());
        prefix.extend_from_slice(&block);

        EncodedSection {
            block: Bytes::from(prefix),
            encoder_stream: Bytes::from(encoder_stream),
        }
    }

    /// Encodes the Required Insert Count and the Base (RFC 9204 4.5.1).
    #[inline]
    fn encode_prefix(&self, out: &mut Vec<u8>, ric: u64, base: u64) {
        // Required Insert Count (4.5.1.1): 0 stays 0, otherwise it is wrapped
        // modulo 2 * MaxEntries, where MaxEntries = floor(MaxCapacity / 32).
        let max_entries = self.max_capacity / 32;
        let enc_ric = if ric == 0 || max_entries == 0 {
            0
        } else {
            (ric % (2 * max_entries)) + 1
        };
        integer::encode(out, enc_ric, 8, 0);
        // Base (4.5.1.2): Sign=0 when Base >= Ric (Delta = Base - Ric);
        // Sign=1 when Base < Ric (Delta = Ric - Base - 1).
        if base >= ric {
            integer::encode(out, base - ric, 7, 0);
        } else {
            integer::encode(out, ric - base - 1, 7, 0x80);
        }
    }

    /// Encodes an indexed reference to a dynamic table entry.
    #[inline]
    fn encode_indexed(
        &self,
        abs: u64,
        base: u64,
        block: &mut Vec<u8>,
        ric: &mut u64,
        min_rel_ref: &mut Option<u64>,
    ) {
        *ric = (*ric).max(abs + 1);
        if abs >= base {
            // Post-Base index (4.5.3).
            integer::encode(block, abs - base, 4, INDEXED_POST_BASE);
        } else {
            // Relative index (4.5.2): Base - Absolute - 1.
            integer::encode(block, base - abs - 1, 6, INDEXED);
        }
        *min_rel_ref = Some(min_rel_ref.map_or(abs, |m| m.min(abs)));
    }

    /// Encodes a literal field line with a dynamic name reference (T=0).
    #[inline]
    fn encode_literal_with_name_ref(
        &self,
        abs: u64,
        base: u64,
        value: &[u8],
        block: &mut Vec<u8>,
        ric: &mut u64,
        min_rel_ref: &mut Option<u64>,
    ) {
        *ric = (*ric).max(abs + 1);
        let rel = base - abs - 1;
        integer::encode(block, rel, 4, LITERAL_NAME_REF);
        *min_rel_ref = Some(min_rel_ref.map_or(abs, |m| m.min(abs)));
        self.push_string(block, value, 8, 0);
    }

    /// Encodes a literal field line with a post-Base name reference (4.5.5).
    #[inline]
    fn encode_literal_post_base_name_ref(
        &self,
        abs: u64,
        base: u64,
        value: &[u8],
        block: &mut Vec<u8>,
        ric: &mut u64,
        min_rel_ref: &mut Option<u64>,
    ) {
        *ric = (*ric).max(abs + 1);
        integer::encode(block, abs - base, 3, LITERAL_POST_BASE_NAME_REF);
        *min_rel_ref = Some(min_rel_ref.map_or(abs, |m| m.min(abs)));
        self.push_string(block, value, 8, 0);
    }

    /// Encodes a literal field line with a static name reference (T=1).
    #[inline]
    fn encode_literal_with_static_name_ref(&self, idx: usize, value: &[u8], block: &mut Vec<u8>) {
        integer::encode(block, idx as u64, 4, LITERAL_NAME_REF | 0x10);
        self.push_string(block, value, 8, 0);
    }

    /// Encodes a literal field line with a literal name (4.5.6), preferring a
    /// static name reference when one exists.
    #[inline]
    fn encode_literal(&self, name: &[u8], value: &[u8], sensitive: bool, block: &mut Vec<u8>) {
        if let Some(idx) = static_table::find_name(name) {
            integer::encode(
                block,
                idx as u64,
                4,
                LITERAL_NAME_REF | 0x10 | (u8::from(sensitive) << 4),
            );
        } else {
            // `001 N` + H + 3-bit name length.
            self.push_string(
                block,
                name,
                4,
                LITERAL_LITERAL_NAME | (u8::from(sensitive) << 4),
            );
        }
        self.push_string(block, value, 8, 0);
    }

    /// Encodes an N-bit prefix string literal (RFC 9204 Section 4.1.2):
    /// `header` carries the bits preceding the string, the Huffman flag is
    /// set when Huffman encoding is shorter, and the length is encoded with
    /// an (N-1)-bit prefix.
    #[inline]
    fn push_string(&self, out: &mut Vec<u8>, value: &[u8], prefix: u8, header: u8) {
        let (huffman, len) = if self.huffman {
            let huffman_bits = huffman::encoded_len(value);
            if huffman_bits < value.len() * 8 {
                (true, huffman_bits.div_ceil(8) as u64)
            } else {
                (false, value.len() as u64)
            }
        } else {
            (false, value.len() as u64)
        };
        integer::encode(
            out,
            len,
            prefix - 1,
            header | (u8::from(huffman) << (prefix - 1)),
        );
        if huffman {
            huffman::encode_with_len(value, out, len as usize);
        } else {
            out.extend_from_slice(value);
        }
    }

    /// Inserts an entry with a literal name on the encoder stream (4.3.3)
    /// and mirrors it in the local table. Returns the instruction, or `None`
    /// when the entry does not fit in the dynamic table.
    #[inline]
    pub(crate) fn insert_literal(&mut self, name: &[u8], value: &[u8]) -> Option<Bytes> {
        let size = DynamicTable::entry_size(name, value);
        if size > self.dynamic.capacity() || self.insert_would_evict_needed(size) {
            return None;
        }
        let mut out = Vec::new();
        // Name length uses a 5-bit prefix (RFC 9204 4.3.3), so `push_string`
        // receives 6 (it reserves one bit for the Huffman flag).
        self.push_string(&mut out, name, 6, INSERT_WITH_LITERAL_NAME);
        self.push_string(&mut out, value, 8, 0);
        let res = self
            .dynamic
            .insert(Bytes::copy_from_slice(name), Bytes::copy_from_slice(value));
        debug_assert!(res.is_ok(), "insert_literal: entry passed the size check");
        res.ok()?;
        Some(Bytes::from(out))
    }

    /// Inserts an entry with a name reference on the encoder stream (4.3.2),
    /// preferring the dynamic table (T=0) then the static table (T=1), and
    /// mirrors it in the local table. Returns the instruction, or `None`
    /// when the name is not indexed anywhere or the entry does not fit.
    #[inline]
    pub(crate) fn insert_with_name_ref(&mut self, name: &[u8], value: &[u8]) -> Option<Bytes> {
        let size = DynamicTable::entry_size(name, value);
        if size > self.dynamic.capacity() || self.insert_would_evict_needed(size) {
            return None;
        }
        let mut out = Vec::new();
        let (name_idx, pattern) = self
            .dynamic
            .find_name(name)
            .map(|abs| (abs, INSERT_WITH_NAME_REF))
            .or_else(|| {
                static_table::find_name(name).map(|idx| (idx as u64, INSERT_WITH_NAME_REF | 0x40))
            })?;
        if pattern == INSERT_WITH_NAME_REF {
            integer::encode(&mut out, self.dynamic.inserted() - name_idx - 1, 6, pattern);
        } else {
            integer::encode(&mut out, name_idx, 6, pattern);
        }
        self.push_string(&mut out, value, 8, 0);
        let res = self
            .dynamic
            .insert(Bytes::copy_from_slice(name), Bytes::copy_from_slice(value));
        debug_assert!(
            res.is_ok(),
            "insert_with_name_ref: entry passed the size check"
        );
        res.ok()?;
        Some(Bytes::from(out))
    }

    /// Duplicates the entry at the given relative index (4.3.4) and mirrors
    /// it in the local table. Returns the instruction, or `None` when the
    /// index is out of range or the copy does not fit.
    #[inline]
    pub(crate) fn duplicate(&mut self, relative: u64) -> Option<Bytes> {
        if relative >= self.dynamic.len() as u64 {
            return None;
        }
        // The relative index is the deque position: 0 is the most recently
        // inserted entry.
        let (name, value) = self.dynamic.entry_at(relative)?;
        let size = DynamicTable::entry_size(name, value);
        if size > self.dynamic.capacity() || self.insert_would_evict_needed(size) {
            return None;
        }
        let mut out = Vec::new();
        integer::encode(&mut out, relative, 5, DUPLICATE);
        let res = self
            .dynamic
            .insert(Bytes::copy_from_slice(name), Bytes::copy_from_slice(value));
        debug_assert!(res.is_ok(), "duplicate: entry passed the size check");
        res.ok()?;
        Some(Bytes::from(out))
    }

    /// Sets the dynamic table capacity (4.3.1), evicting as needed. Returns
    /// the instruction, or `None` when the capacity is unchanged, exceeds
    /// the decoder's maximum, or the reduction would evict entries the
    /// decoder still needs (RFC 9204 Section 2.1.1).
    #[inline]
    pub fn set_capacity(&mut self, capacity: u64) -> Option<Bytes> {
        if capacity > self.max_capacity || capacity == self.dynamic.capacity() {
            return None;
        }
        if capacity < self.dynamic.capacity() {
            let evicted = self.dynamic.evict_for_capacity(capacity);
            if self.dynamic.inserted() - self.dynamic.len() as u64 + evicted
                > self.evictable_floor()
            {
                return None;
            }
        }
        let mut out = Vec::new();
        integer::encode(&mut out, capacity, 5, SET_CAPACITY);
        self.dynamic.set_capacity(capacity);
        Some(Bytes::from(out))
    }

    /// Whether inserting an entry of `size` bytes would evict an entry the
    /// decoder still needs (RFC 9204 Section 2.1.1): one that is not yet
    /// acknowledged or is referenced by an unacknowledged field section.
    #[inline]
    fn insert_would_evict_needed(&self, size: u64) -> bool {
        self.dynamic.inserted() - self.dynamic.len() as u64 + self.dynamic.would_evict(size)
            > self.evictable_floor()
    }

    /// Processes the decoder stream instructions in `buf`. Section
    /// Acknowledgments (4.4.1) free a field section's references, Stream
    /// Cancellations (4.4.2) free every reference of a cancelled stream,
    /// and Insert Count Increments (4.4.3) raise the Known Received Count.
    ///
    /// Only an Insert Count Increment (4.4.3) is a connection error: a zero
    /// increment, or one that advances past the number of inserts sent
    /// (RFC 9204 Section 2.1.4). A Section Acknowledgment or Stream
    /// Cancellation that matches no outstanding field section is benign —
    /// RFC 9204 Section 4.4 defines no error for it and a peer may send one
    /// for a section already acknowledged or cancelled — so it is ignored
    /// rather than closing the connection.
    #[inline]
    pub fn feed_decoder_stream(&mut self, buf: &[u8]) -> Result<(), QpackError> {
        let mut off = 0;
        while off < buf.len() {
            let header = buf[off];
            off += 1;
            if header & 0x80 != 0 {
                // `1` + 7-bit stream ID: Section Acknowledgment (4.4.1).
                // The top bit is the instruction type; the remaining 7 bits
                // (and any continuation bytes) are the Stream ID, so this
                // arm must catch every byte with the high bit set, including
                // IDs at or above 64 whose prefix byte is `0xC0`. A matching
                // field section is freed; a stray acknowledgment (already
                // freed, or never outstanding) is ignored.
                let stream_id = integer::decode(buf, &mut off, 7, header)
                    .map_err(|_| QpackError::DecoderStream)?;
                if let Some(pos) = self
                    .pending_refs
                    .iter()
                    .position(|(id, _)| *id == stream_id)
                {
                    self.pending_refs.remove(pos);
                }
            } else if header & 0x40 != 0 {
                // `01` + 6-bit stream ID: Stream Cancellation (4.4.2).
                // Every outstanding reference of the stream is dropped; a
                // cancellation for a stream with none is a no-op.
                let stream_id = integer::decode(buf, &mut off, 6, header)
                    .map_err(|_| QpackError::DecoderStream)?;
                self.pending_refs.retain(|(id, _)| *id != stream_id);
            } else {
                // `00` + 6-bit increment: Insert Count Increment (4.4.3).
                // A zero increment is forbidden, and the total may not
                // exceed the number of inserts sent.
                let increment = integer::decode(buf, &mut off, 6, header)
                    .map_err(|_| QpackError::DecoderStream)?;
                if increment == 0
                    || self.known_received.saturating_add(increment) > self.dynamic.inserted()
                {
                    return Err(QpackError::DecoderStream);
                }
                self.known_received += increment;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn decoder_stream_section_ack_classifies_high_stream_id() {
        // A Section Acknowledgment for stream ID 64 encodes to `0xC0`
        // (top bit set, 0x40 bit also set by the 7-bit Stream ID). The
        // instruction is identified by the high bit alone; misclassifying
        // it as an Insert Count Increment (which rejects a zero value)
        // was a latent bug that only surfaced for stream IDs at or above
        // 64 (RFC 9204 Section 4.4.1).
        let mut enc = Encoder::new(256, false);
        enc.encode_section(64, &[hdr("a", "b")]);
        assert!(enc.pending_refs.iter().any(|(id, _)| *id == 64));
        // 0xC0: Section Acknowledgment, Stream ID 64.
        assert!(enc.feed_decoder_stream(&[0xC0]).is_ok());
        assert!(!enc.pending_refs.iter().any(|(id, _)| *id == 64));

        // Stream Cancellation for a low stream ID (0x44 = Stream ID 4)
        // frees its pending reference.
        enc.encode_section(4, &[hdr("a", "b")]);
        assert!(enc.pending_refs.iter().any(|(id, _)| *id == 4));
        assert!(enc.feed_decoder_stream(&[0x44]).is_ok());
        assert!(!enc.pending_refs.iter().any(|(id, _)| *id == 4));

        // Insert Count Increment (0x01 = increment 1) is accepted once the
        // encoder has inserted at least one entry.
        let inserted = enc.dynamic.inserted();
        assert!(inserted >= 1);
        assert!(enc.feed_decoder_stream(&[0x01]).is_ok());
        assert_eq!(enc.known_received, 1);
    }

    #[test]
    fn stray_decoder_stream_instructions_are_benign() {
        // A conformant peer may send a Section Acknowledgment or Stream
        // Cancellation for a field section the encoder has already freed
        // (or never outstanding). RFC 9204 Section 4.4 defines no error for
        // these; treating them as QPACK_DECODER_STREAM_ERROR closed the
        // connection and dropped in-flight requests (the bug this guards).
        let mut enc = Encoder::new(256, false);

        // Section Acknowledgment for a stream with no pending reference.
        assert!(enc.feed_decoder_stream(&[0xC0]).is_ok());
        // Stream Cancellation for a stream with no pending reference.
        assert!(enc.feed_decoder_stream(&[0x44]).is_ok());

        // A duplicate Section Acknowledgment after the only pending
        // reference was already freed is likewise benign.
        enc.encode_section(4, &[hdr("a", "b")]);
        assert!(enc.feed_decoder_stream(&[0x44]).is_ok());
        assert!(enc.feed_decoder_stream(&[0x44]).is_ok());
    }

    #[test]
    fn vector_b1_static_name_ref() {
        // RFC 9204 B.1: literal field line with a static name reference. The
        // decoder advertises capacity 0, so the dynamic table is unused.
        let mut enc = Encoder::new(0, false);
        let out = enc.encode_section(0, &[hdr(":path", "/index.html")]);
        assert_eq!(hex(&out.block), "0000510b2f696e6465782e68746d6c");
        assert!(out.encoder_stream.is_empty());
    }

    #[test]
    fn vector_b2_dynamic_post_base() {
        // RFC 9204 B.2: Set Capacity 220, two inserts with static name
        // references, then a field section referencing them post-Base
        // (Required Insert Count = 2, Sign = 1, Delta Base = 1).
        let mut enc = Encoder::new(220, false);
        assert_eq!(hex(&enc.set_capacity(220).unwrap()), "3fbd01");
        let authority = enc
            .insert_with_name_ref(b":authority", b"www.example.com")
            .unwrap();
        assert_eq!(hex(&authority), "c00f7777772e6578616d706c652e636f6d");
        let path = enc.insert_with_name_ref(b":path", b"/sample/path").unwrap();
        assert_eq!(hex(&path), "c10c2f73616d706c652f70617468");

        // Vector path: Base fixed at 0 (the acknowledged count), so the two
        // entries are referenced post-Base.
        let block = enc.encode_section_with_base(
            0,
            &[
                hdr(":authority", "www.example.com"),
                hdr(":path", "/sample/path"),
            ],
            0,
        );
        assert_eq!(hex(&block.block), "03811011");
    }

    #[test]
    fn vector_b3_insert_literal_name() {
        // RFC 9204 B.3: Insert with Literal Name (custom-key/custom-value).
        let mut enc = Encoder::new(220, false);
        enc.set_capacity(220);
        let ins = enc.insert_literal(b"custom-key", b"custom-value").unwrap();
        assert_eq!(
            hex(&ins),
            "4a637573746f6d2d6b65790c637573746f6d2d76616c7565"
        );
        assert_eq!(enc.dynamic.len(), 1);
        assert_eq!(
            enc.dynamic.get_absolute(0),
            Some((b"custom-key".as_slice(), b"custom-value".as_slice()))
        );
    }

    #[test]
    fn vector_b4_duplicate_and_relative() {
        // RFC 9204 B.4: duplicate entry 2, then an indexed field line
        // relative to Base 4 (Required Insert Count = 4, Sign = 0, Delta
        // Base = 0).
        let mut enc = Encoder::new(220, false);
        enc.set_capacity(220);
        enc.insert_with_name_ref(b":authority", b"www.example.com");
        enc.insert_with_name_ref(b":path", b"/sample/path");
        enc.insert_literal(b"custom-key", b"custom-value");
        assert_eq!(hex(&enc.duplicate(2).unwrap()), "02");
        assert_eq!(enc.dynamic.len(), 4);

        let block = enc.encode_section_with_base(0, &[hdr(":authority", "www.example.com")], 4);
        assert_eq!(hex(&block.block), "050080");
    }

    #[test]
    fn fresh_name_inserted_then_referenced() {
        // A name that matches nothing is inserted with a literal name
        // instruction, then the section references it post-Base.
        let mut enc = Encoder::new(220, false);
        let out = enc.encode_section(0, &[hdr("custom-key", "custom-value")]);
        assert_eq!(
            hex(&out.encoder_stream),
            "3fbd014a637573746f6d2d6b65790c637573746f6d2d76616c7565"
        );
        // Required Insert Count = 1 -> (1 mod 12) + 1 = 2; Base = 0 < Ric ->
        // Sign = 1, Delta = Ric - Base - 1 = 0; post-Base index 0.
        assert_eq!(hex(&out.block), "028010");
        assert_eq!(enc.dynamic.len(), 1);
    }

    #[test]
    fn indexed_static_full_match() {
        let mut enc = Encoder::new(0, false);
        let out = enc.encode_section(0, &[hdr(":method", "GET")]);
        let idx = static_table::find(b":method", b"GET").unwrap() as u8;
        assert_eq!(hex(&out.block), format!("0000{:02x}", 0xc0 | idx));
    }

    #[test]
    fn relative_ref_from_fixed_base() {
        // Entries inserted beforehand are referenced relative to Base = the
        // insert count; nothing new is inserted, so Sign = 0, Delta = 0.
        let mut enc = Encoder::new(220, false);
        enc.set_capacity(220);
        enc.insert_with_name_ref(b":authority", b"www.example.com");
        enc.insert_with_name_ref(b":path", b"/sample/path");
        let out = enc.encode_section(0, &[hdr(":path", "/sample/path")]);
        // Required Insert Count = 2 -> (2 mod 12) + 1 = 3; relative index of
        // absolute 1 from Base 2 is 0.
        assert_eq!(hex(&out.block), "030080");
        assert!(out.encoder_stream.is_empty());
    }

    /// The production encoder must reference dynamic entries against the
    /// decoder's *acknowledged* insert count, not its own (which runs ahead
    /// while a client is busy consuming a large response). An unacknowledged
    /// entry must be referenced with a Post-Base index (RFC 9204 2.1.2); a
    /// relative index above the peer's Largest Reference is a decompression
    /// failure that a strict client (Neqo/Firefox) turns into a connection
    /// close — which is the reported "concurrent requests ignored while a big
    /// download streams" hang.
    #[test]
    fn ack_base_uses_post_base_for_unacknowledged_entry() {
        let mut enc = Encoder::new(220, false);
        enc.set_capacity(220);
        // Insert two entries but never feed the client's decoder stream, so the
        // encoder's known_received stays 0 (nothing acknowledged yet).
        enc.insert_with_name_ref(b":authority", b"www.example.com");
        enc.insert_with_name_ref(b":path", b"/sample/path");

        // A novel header forces a fresh insert (abs 3) that, with base =
        // known_received = 0, can only be referenced post-Base.
        let section = enc.encode_section_with_ack_base(0, &[hdr("x-novel", "value")]);
        // The single dynamic field line is the only instruction after the
        // 2-byte prefix; a Post-Base Indexed Field Line occupies 0x10..=0x1F.
        let body = &section.block[2..];
        assert_eq!(body.len(), 1, "exactly one field line");
        assert_eq!(
            body[0] & 0xF0,
            0x10,
            "unacknowledged entry must be referenced post-Base, not with a relative index"
        );
        assert!(
            !section.encoder_stream.is_empty(),
            "the newly inserted entry must travel on the encoder stream"
        );
    }

    #[test]
    fn ack_base_uses_relative_for_acknowledged_entry() {
        let mut enc = Encoder::new(220, false);
        enc.set_capacity(220);
        enc.insert_with_name_ref(b":authority", b"www.example.com");
        enc.insert_with_name_ref(b":path", b"/sample/path");
        enc.insert_with_name_ref(b"x-foo", b"bar");
        // Client acknowledges the first two inserts (Insert Count Increment 2),
        // raising known_received to 2. Entry `:authority` (abs 1) is now below
        // the Base, so it may be referenced with a relative index.
        enc.feed_decoder_stream(&[0x02]).unwrap();

        let section = enc.encode_section_with_ack_base(0, &[hdr(":authority", "www.example.com")]);
        let body = &section.block[2..];
        assert_eq!(body.len(), 1, "exactly one field line");
        // A relative Indexed Field Line for the dynamic table occupies 0x80..=0xBF.
        assert_eq!(
            body[0] & 0xC0,
            0x80,
            "acknowledged entry must be referenced with a relative index"
        );
        assert!(
            section.encoder_stream.is_empty(),
            "no new insert needed for an already-present entry"
        );
    }

    #[test]
    fn huffman_used_when_shorter() {
        let mut enc = Encoder::new(0, true);
        let out = enc.encode_section(
            0,
            &[hdr(":path", "www.example.com/aaaaaaaaaaaaaaaaaaaaaaaaa")],
        );
        let block = out.block.as_ref();
        assert_eq!(&block[..2], &[0x00, 0x00]);
        // Literal with Name Reference, static :path (0x51).
        assert_eq!(block[2], 0x51);
        // The value length octet has the H bit set and encodes fewer bytes.
        assert_ne!(block[3] & 0x80, 0);
        assert!(block[3] & 0x7f < 42);
        // The block must decode back to the original value.
        let mut dec = Vec::new();
        huffman::decode(&block[4..], &mut dec).unwrap();
        assert_eq!(dec, b"www.example.com/aaaaaaaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn sensitive_never_indexed() {
        let mut enc = Encoder::new(220, false);
        let out = enc.encode_section(0, &[hdr("authorization", "Bearer xyz")]);
        // No encoder stream instruction: the field is never stored.
        assert!(out.encoder_stream.is_empty());
        // Literal with Literal Name, N=1: the N bit (0x10) is set.
        assert_ne!(out.block.as_ref()[2] & 0x10, 0);
        assert_eq!(enc.dynamic.len(), 0);
    }

    #[test]
    fn insert_skipped_when_it_would_evict_referenced_entry() {
        // A 100-byte table holds exactly one 43-byte entry. The section
        // references it, and the second field (71 bytes) does not fit
        // without evicting it, so the insert is skipped and the field is
        // sent as a literal name.
        let mut enc = Encoder::new(100, false);
        enc.set_capacity(100);
        enc.insert_with_name_ref(b":authority", b"www.example.com");
        let out = enc.encode_section(
            0,
            &[
                hdr(":authority", "www.example.com"),
                hdr("x-custom", "0123456789012345678901234567890123456789"),
            ],
        );
        let block = out.block.as_ref();
        // First field line: Indexed, dynamic, relative index 0 (0x80).
        assert_eq!(block[2], 0x80);
        // Second field line: literal with literal name (starts `001`).
        assert_eq!(block[3] & 0xe0, 0x20);
        // No Insert with Literal Name instruction was queued.
        assert!(out.encoder_stream.is_empty());
        assert_eq!(enc.dynamic.len(), 1);
    }

    #[test]
    fn no_duplicate_set_capacity_instruction() {
        let mut enc = Encoder::new(220, false);
        enc.set_capacity(220);
        let out = enc.encode_section(0, &[hdr(":path", "/sample/path")]);
        assert!(out.encoder_stream.is_empty());
    }

    #[test]
    fn empty_section() {
        let mut enc = Encoder::new(220, false);
        let out = enc.encode_section(0, &[]);
        // Prefix only: Required Insert Count 0, Sign 0, Delta Base 0.
        assert_eq!(hex(&out.block), "0000");
        assert!(out.encoder_stream.is_empty());
    }

    #[test]
    fn static_only_section_announces_zero_ric() {
        // A section that references only static entries must announce a
        // Required Insert Count of 0 even when the dynamic table is
        // non-empty (RFC 9204 Section 2.1.2: it is one larger than the
        // largest referenced absolute index, so 0 when none are referenced).
        let mut enc = Encoder::new(220, false);
        enc.set_capacity(220);
        enc.insert_with_name_ref(b":authority", b"www.example.com");
        let out = enc.encode_section(0, &[hdr(":method", "GET")]);
        // Prefix: RIC 0, Base = insert count 1 (Sign 0, Delta 1); then the
        // static index of ":method GET" (17) as an Indexed Field Line.
        assert_eq!(hex(&out.block), "0001d1");
        assert!(out.encoder_stream.is_empty());
    }

    /// Strict, independent decoder for a single QPACK "Insert With Literal
    /// Name" instruction (RFC 9204 Section 4.3.3). Deliberately does NOT use
    /// the project's `push_string`/`read_string`, so it reproduces exactly
    /// what a third-party decoder (e.g. Chromium) does: the name length uses
    /// a 5-bit prefix with the Huffman flag at bit 5.
    fn strict_decode_insert_literal_name(buf: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut off = 0usize;
        let first = *buf.get(off)?;
        // `01` + H + 5-bit name length.
        if first & 0xC0 != 0x40 {
            return None;
        }
        let name_huffman = first & 0x20 != 0;
        let mut name_len = (first & 0x1F) as usize;
        off += 1;
        if name_len == 0x1F {
            let mut shift = 0u32;
            loop {
                let b = *buf.get(off)?;
                off += 1;
                name_len += ((b & 0x7f) as usize) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
        }
        let name_raw = buf.get(off..off + name_len)?;
        off += name_len;
        let name = if name_huffman {
            let mut out = Vec::new();
            crate::hpack::huffman::decode(name_raw, &mut out).ok()?;
            out
        } else {
            name_raw.to_vec()
        };

        let first = *buf.get(off)?;
        // 7-bit value length with the Huffman flag at bit 7.
        let value_huffman = first & 0x80 != 0;
        let mut value_len = (first & 0x7f) as usize;
        off += 1;
        if value_len == 0x7f {
            let mut shift = 0u32;
            loop {
                let b = *buf.get(off)?;
                off += 1;
                value_len += ((b & 0x7f) as usize) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
        }
        let value_raw = buf.get(off..off + value_len)?;
        let value = if value_huffman {
            let mut out = Vec::new();
            crate::hpack::huffman::decode(value_raw, &mut out).ok()?;
            out
        } else {
            value_raw.to_vec()
        };
        Some((name, value))
    }

    #[test]
    fn insert_with_literal_name_survives_strict_decode() {
        // Reproduces the hang Chromium reports: the encoder stream carries an
        // "Insert With Literal Name" instruction whose name length must be a
        // 5-bit prefix with the Huffman flag at bit 5. With Huffman enabled
        // (the production setting), a 4-bit prefix misplaces the flag and
        // corrupts the stream, so a strict decoder cannot recover the name.
        let mut enc = Encoder::new(220, true);
        enc.set_capacity(220);
        let ins = enc
            .insert_literal(b"x-custom-header", b"hello-world")
            .unwrap();

        let (name, value) = strict_decode_insert_literal_name(&ins)
            .expect("strict QPACK decoder must parse the insert");
        assert_eq!(name, b"x-custom-header");
        assert_eq!(value, b"hello-world");
    }

    #[test]
    fn insert_with_literal_name_huffman_flag_at_bit_five() {
        // Byte-level check: with Huffman on, the H flag of an Insert With
        // Literal Name must sit at bit 5 (0x20), not bit 4 (0x10).
        let mut enc = Encoder::new(220, true);
        enc.set_capacity(220);
        let ins = enc.insert_literal(b"x-custom-header", b"hello").unwrap();
        let pos = ins
            .iter()
            .position(|&b| b & 0xC0 == 0x40)
            .expect("insert instruction present");
        assert_eq!(
            ins[pos] & 0x20,
            0x20,
            "Huffman flag must be at bit 5 for an Insert With Literal Name"
        );
    }
}
