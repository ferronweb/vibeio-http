//! QPACK error codes (RFC 9114 Section 8.1, reused by RFC 9204 Section 6).
//!
//! The QPACK error family is `0x02xx`: these codes are carried in
//! CONNECTION_CLOSE frames by the HTTP/3 layer, which maps them here.

/// Errors raised while QPACK state is inconsistent or a representation is
/// malformed.
///
/// `DecompressionFailed` is produced by field section decoding, the other
/// two by processing of the peer's respective QPACK stream. All three are
/// fatal for the connection: there is no way to resynchronize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QpackError {
    /// `QPACK_DECOMPRESSION_FAILED` (0x0200): a field section could not be
    /// decoded, or references an entry that is evicted or out of range.
    DecompressionFailed,
    /// `QPACK_ENCODER_STREAM_ERROR` (0x0201): an encoder stream instruction
    /// was malformed or violated table constraints.
    EncoderStream,
    /// `QPACK_DECODER_STREAM_ERROR` (0x0202): a decoder stream instruction
    /// was malformed. Kept so [`QpackError::code`] mirrors the full error
    /// family; this implementation only *emits* decoder stream instructions,
    /// it never parses them, so the variant is never constructed. It errors
    /// if the decoder stream of a peer is ever parsed, which is the reminder
    /// to remove this expectation.
    #[expect(dead_code)]
    DecoderStream,
}

impl QpackError {
    /// The HTTP/3 CONNECTION_CLOSE error code (RFC 9204 Section 6).
    ///
    /// Consumed by the HTTP/3 layer, which is why the expectation errors
    /// once that lands.
    #[expect(dead_code)]
    pub(crate) fn code(self) -> u16 {
        match self {
            QpackError::DecompressionFailed => 0x0200,
            QpackError::EncoderStream => 0x0201,
            QpackError::DecoderStream => 0x0202,
        }
    }
}
