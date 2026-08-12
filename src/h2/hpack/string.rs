//! HPACK string literals (RFC 7541 Section 5.2).

use super::{huffman, integer, HpackError};

/// The Huffman-flag bit in the string literal length octet.
const HUFFMAN_FLAG: u8 = 0x80;

/// Whether Huffman encoding shortens `value` enough to be worthwhile.
pub(crate) fn should_huffman(value: &[u8]) -> bool {
    !value.is_empty() && huffman::encoded_len(value) < value.len() * 8
}

/// Encodes a string literal (raw or Huffman-coded) into `out`.
pub(crate) fn encode(out: &mut Vec<u8>, value: &[u8], huffman: bool) {
    if huffman {
        integer::encode(
            out,
            huffman::encoded_len(value).div_ceil(8) as u32,
            7,
            HUFFMAN_FLAG,
        );
        huffman::encode(value, out);
    } else {
        integer::encode(out, value.len() as u32, 7, 0);
        out.extend_from_slice(value);
    }
}

/// Decodes a string literal from `buf` at `off`, enforcing a maximum
/// decoded length. Returns whether the literal was Huffman-coded.
pub(crate) fn decode(
    buf: &[u8],
    off: &mut usize,
    max_length: usize,
) -> Result<(bool, Vec<u8>), HpackError> {
    let first = *buf.get(*off).ok_or(HpackError::InvalidString)?;
    *off += 1;
    let len = integer::decode(buf, off, 7, first)? as usize;
    if len > max_length {
        return Err(HpackError::InvalidString);
    }
    let end = off.checked_add(len).ok_or(HpackError::InvalidString)?;
    let slice = buf.get(*off..end).ok_or(HpackError::InvalidString)?;
    *off = end;

    if first & HUFFMAN_FLAG != 0 {
        let mut value = Vec::with_capacity(len / 2 + 1);
        huffman::decode(slice, &mut value)?;
        if value.len() > max_length {
            return Err(HpackError::InvalidString);
        }
        Ok((true, value))
    } else {
        Ok((false, slice.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for (value, huffman) in [
            (b"".as_slice(), false),
            (b"", true),
            (b"custom-key", false),
            (b"custom-key", true),
            (b"custom-huffman", true),
            (b"x", true),
            (&[0u8; 3], false),
            (&[0u8; 3], true),
        ] {
            let mut out = Vec::new();
            encode(&mut out, value, huffman);
            let mut off = 0;
            let (was_huffman, decoded) = decode(&out, &mut off, 1024).unwrap();
            assert_eq!(was_huffman, huffman);
            assert_eq!(decoded, value);
            assert_eq!(off, out.len());
        }
    }

    #[test]
    fn empty_huffman_flag_keeps_flag() {
        let mut out = Vec::new();
        encode(&mut out, b"", true);
        assert_eq!(out, [0x80]);
        let mut off = 0;
        assert_eq!(decode(&out, &mut off, 16).unwrap(), (true, vec![]));
    }

    #[test]
    fn decode_enforces_max_length() {
        let mut out = Vec::new();
        encode(&mut out, b"toolong", false);
        let mut off = 0;
        assert!(matches!(
            decode(&out, &mut off, 3),
            Err(HpackError::InvalidString)
        ));

        // Huffman expansion is checked too: "ASDF" compresses to fewer
        // bits than 4 bytes? No — pick a value that expands: use a string
        // of 5-bit symbols which packs >4 chars into 4 bytes.
        let mut out = Vec::new();
        encode(&mut out, b"0123456789", true);
        let mut off = 0;
        // Wire length < 10 but decoded length is 10.
        assert!(decode(&out, &mut off, 9).is_err());
    }

    #[test]
    fn huffman_choice() {
        assert!(!should_huffman(b""));
        assert!(should_huffman(b"www.example.com"));
        assert!(!should_huffman(&[0xff; 10]));
    }
}
