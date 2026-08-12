//! HPACK (RFC 7541) header compression.
//!
//! Public surface: [`Decoder`] and [`Encoder`]. Internal modules stay
//! crate-private.

pub(crate) mod decode;
pub(crate) mod encode;
pub(crate) mod huffman;
pub(crate) mod huffman_table;
pub(crate) mod integer;
pub(crate) mod string;
pub(crate) mod table;

pub use decode::Decoder;
pub use encode::Encoder;
pub use table::Header;

/// Errors produced by the HPACK codec. Callers map these to HTTP/2
/// connection errors (`COMPRESSION_ERROR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum HpackError {
    /// An integer representation overflowed or ran out of input.
    InvalidInteger,
    /// A string literal violated framing constraints.
    InvalidString,
    /// A Huffman-encoded string violated RFC 7541 Section 5.2 rules
    /// (EOS symbol in the data, over-long or malformed padding, or
    /// truncation).
    InvalidHuffman,
    /// An indexed header field referenced a non-existent table entry.
    InvalidIndex,
    /// A dynamic table size update exceeded the protocol maximum or
    /// appeared after a header field representation.
    InvalidMaxSize,
    /// The decoded header list exceeded the configured maximum size.
    HeaderListTooLarge,
    /// The first octet of a representation matched no known pattern.
    InvalidRepresentation,
}
