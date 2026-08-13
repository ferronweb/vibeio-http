//! HPACK (RFC 7541) header compression.
//!
//! Public surface: [`Decoder`], [`Encoder`], and [`Header`], re-exported at
//! [`crate::hpack`]. The shared primitives (Huffman coding, prefix integers)
//! live in the crate-level [`crate::hpack`] module, which QPACK also consumes.

pub(crate) use crate::hpack::{huffman, integer};

pub(crate) mod decode;
pub(crate) mod encode;
pub(crate) mod string;
pub(crate) mod table;

pub use crate::hpack::HpackError;
pub use decode::Decoder;
pub use encode::Encoder;
pub use table::Header;
