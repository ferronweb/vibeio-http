//! QUIC transport abstraction for the HTTP/3 implementation.
//!
//! The HTTP/3 layer (RFC 9114) is written against these traits instead of a
//! concrete QUIC stack, so any QUIC implementation can be adapted. The
//! `quinn` adapter behind the `h3-quinn` feature and the request stream
//! handling consume these traits exclusively.
//!
//! See `CUSTOM_HTTP3_IMPL.md` (local, uncommitted) for the trait shape;
//! this module is populated in the QUIC transport step.
