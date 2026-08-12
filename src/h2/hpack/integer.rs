//! HPACK integer representation (RFC 7541 Section 5.1).
//!
//! Integers are encoded with an N-bit prefix (5, 6, 7, or 8 bits) followed
//! by zero or more continuation octets carrying 7 bits each.

use super::HpackError;

/// Encodes `value` with an `prefix_bits`-bit prefix, appending to `out`.
///
/// The low `prefix_bits` bits of `header` (which carries the representation's
/// other bits) are replaced with the encoded value.
#[inline]
pub(crate) fn encode(out: &mut Vec<u8>, value: u32, prefix_bits: u8, header: u8) {
    let mask = (1u32 << prefix_bits) - 1;
    if value < mask {
        out.push((u32::from(header) & !mask | value) as u8);
        return;
    }
    out.push((u32::from(header) & !mask | mask) as u8);
    let mut v = value - mask;
    while v >= 128 {
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Decodes an `prefix_bits`-bit-prefixed integer.
///
/// `header` is the already-consumed first octet; `off` is updated past the
/// consumed continuation octets of `buf`. Values that would overflow 32 bits
/// are treated as a decoding error (RFC 7541 Section 5.1).
#[inline]
pub(crate) fn decode(
    buf: &[u8],
    off: &mut usize,
    prefix_bits: u8,
    header: u8,
) -> Result<u32, HpackError> {
    let mask = (1u32 << prefix_bits) - 1;
    let mut value = u32::from(header) & mask;
    if value < mask {
        return Ok(value);
    }

    let mut shift: u32 = 0;
    loop {
        if shift > 28 {
            return Err(HpackError::InvalidInteger);
        }
        let b = *buf.get(*off).ok_or(HpackError::InvalidInteger)?;
        *off += 1;
        value = value
            .checked_add((u32::from(b & 0x7f)) << shift)
            .ok_or(HpackError::InvalidInteger)?;
        if b & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_with_prefix() {
        // RFC 7541 C.1.1: 10 with a 5-bit prefix.
        let mut out = Vec::new();
        encode(&mut out, 10, 5, 0);
        assert_eq!(out, [0x0a]);

        // C.1.2: 1337 with a 5-bit prefix.
        out.clear();
        encode(&mut out, 1337, 5, 0);
        assert_eq!(out, [0x1f, 0x9a, 0x0a]);

        // C.1.3: 42 starting at octet boundary with an 8-bit prefix.
        out.clear();
        encode(&mut out, 42, 8, 0);
        assert_eq!(out, [0x2a]);
    }

    #[test]
    fn preserves_header_bits() {
        // The representation bits (0x80: Huffman flag) survive encoding.
        let mut out = Vec::new();
        encode(&mut out, 5, 7, 0x80);
        assert_eq!(out, [0x85]);
        out.clear();
        encode(&mut out, 0, 7, 0x80);
        assert_eq!(out, [0x80]);
        out.clear();
        encode(&mut out, 127, 7, 0x80);
        assert_eq!(out, [0xff, 0x00]);
    }

    #[test]
    fn round_trip() {
        for &(bits, value) in &[
            (5, 0u32),
            (5, 30),
            (5, 31),
            (5, 1337),
            (5, u32::MAX - 1),
            (6, 62),
            (6, 63),
            (6, 10_000),
            (7, 127),
            (7, 128),
            (7, 1 << 24),
            (8, 255),
            (8, 256),
        ] {
            let mut out = Vec::new();
            encode(&mut out, value, bits, 0);
            let mut off = 1;
            assert_eq!(decode(&out, &mut off, bits, out[0]).unwrap(), value);
            assert_eq!(off, out.len(), "value {value}");
        }
    }

    #[test]
    fn rejects_truncated() {
        let mut out = Vec::new();
        encode(&mut out, 1337, 5, 0);
        // Chop the continuation octets.
        let mut off = 1;
        let header = 0x1f;
        assert!(decode(&out[..1], &mut off, 5, header).is_err());
    }

    #[test]
    fn rejects_overflow() {
        // Continuation octets that would overflow u32: 0x80 repeated five
        // times with a 5-bit prefix.
        let buf = [0x1f, 0xff, 0xff, 0xff, 0xff, 0x7f];
        let mut off = 1;
        assert_eq!(
            decode(&buf, &mut off, 5, buf[0]),
            Err(HpackError::InvalidInteger)
        );
    }
}
