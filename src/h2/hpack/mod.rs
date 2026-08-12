//! HPACK (RFC 7541) header compression.
//!
//! Only the dynamic table (H2) consumes these primitives yet; the encoder
//! and decoder are wired up in H3/H4. Until then the module is exercised
//! through its unit tests.
#![allow(dead_code)]

pub(crate) mod huffman;
pub(crate) mod integer;
pub(crate) mod string;

/// Errors produced by the HPACK codec. Callers map these to HTTP/2
/// connection errors (`COMPRESSION_ERROR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum HpackError {
    /// An integer representation overflowed or ran out of input.
    InvalidInteger,
    /// A string literal violated framing constraints.
    InvalidString,
    /// A Huffman-encoded string violated RFC 7541 Section 5.2 rules
    /// (EOS symbol in the data, over-long or malformed padding, or
    /// truncation).
    InvalidHuffman,
}
