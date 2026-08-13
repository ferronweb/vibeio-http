#![no_main]
//! Fuzz target for the QPACK decoder.
//!
//! QPACK field-section and encoder-stream decoding is a classic source of
//! memory-safety and panic bugs: malformed prefix integers, string-length
//! overruns, dynamic-table index references past the Base or the eviction
//! point. The decoder MUST never panic on arbitrary input — malformed
//! input is part of the protocol and must surface as `QpackError` so the
//! connection can be closed. This target splits the input into encoder
//! stream bytes and one field section, replays them, and asserts the
//! decoder returns `Result`s instead of unwinding; blocked sections are
//! drained via cancellation and expiry so the buffering machinery is
//! exercised too.

use libfuzzer_sys::fuzz_target;
use vibeio_http::qpack::Decoder;

fuzz_target!(|data: &[u8]| {
    let mut decoder = Decoder::new(16384, 100);
    let split = data.len() / 2;
    let (encoder_stream, section) = data.split_at(split);

    let _ = decoder.feed_encoder_stream(encoder_stream);

    // A second, independent decoder feeds only the section bytes: a section
    // may reference dynamic entries that never arrive (blocks), and must
    // not panic while parked.
    let mut lone = Decoder::new(16384, 100);
    let _ = lone.decode_block(section, 1, 0);

    match decoder.decode_block(section, 1, 0) {
        Ok(Some(_headers)) => {}
        Ok(None) => {
            // Parked: unblocking never arrives, but cancellation and expiry
            // must drain the queue without panicking.
            let _ = decoder.stream_cancelled(1);
            let _ = decoder.expire_blocked(u64::MAX, 0);
        }
        Err(_) => {
            // The expected, safe outcome for malformed input.
        }
    }

    // Decoder stream instructions are never inspected by fuzzing, but must
    // be drainable.
    let _ = decoder.take_decoder_stream();
});