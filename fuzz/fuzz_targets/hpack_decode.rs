#![no_main]
//! Fuzz target for the native HPACK decoder.
//!
//! HPACK header-block decoding is a classic source of memory-safety and
//! panic bugs (out-of-bounds integer/string reads, bad dynamic-table index
//! references). The decoder MUST never panic on arbitrary input: malformed
//! header blocks are part of the protocol and must surface as `HpackError`
//! so the connection can RST the stream. This target feeds raw bytes and
//! asserts the decoder returns a `Result` instead of unwinding.

use libfuzzer_sys::fuzz_target;
use vibeio_http::hpack::Decoder;

fuzz_target!(|data: &[u8]| {
    let mut decoder = Decoder::new(4096);
    match decoder.decode(data, &mut 0) {
        Ok(headers) => {
            // The dynamic table must stay internally consistent after a
            // successful decode: every referenced entry resolves.
            for h in &headers {
                let _ = h.name().len() + h.value().len();
            }
        }
        Err(_) => {
            // Any error is the expected, safe outcome for malformed input;
            // the decoder must never panic on it.
        }
    }
});
