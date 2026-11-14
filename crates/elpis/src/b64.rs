//! Standard base64 (RFC 4648, with padding): for registry logins and signature keys. Small enough not to need a crate.

use anyhow::{Result, bail};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Strict: padded, only the standard alphabet, nothing after the padding.
pub fn decode(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        bail!("not valid base64 (length isn't a multiple of 4)");
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = (i + 1) * 4 == bytes.len();
        let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            bail!("not valid base64 (misplaced padding)");
        }
        let mut n = 0u32;
        for (j, &b) in chunk.iter().enumerate() {
            let v = if j >= 4 - pad {
                0
            } else {
                ALPHABET.iter().position(|&a| a == b).ok_or_else(|| anyhow::anyhow!("not valid base64 (unexpected character)"))? as u32
            };
            n = n << 6 | v;
        }
        let triple = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&triple[..3 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: [(&str, &str); 8] =
        [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy"), ("user:pass", "dXNlcjpwYXNz")];

    #[test]
    fn matches_the_rfc_vectors_both_ways() {
        for (plain, encoded) in VECTORS {
            assert_eq!(encode(plain.as_bytes()), encoded);
            assert_eq!(decode(encoded).unwrap(), plain.as_bytes());
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
    }

    #[test]
    fn malformed_input_is_refused() {
        for bad in ["Zg=", "Zg", "Z===", "Zg==Zg==", "Zm9v!A==", "Zm 9v", "=m9v"] {
            assert!(decode(bad).is_err(), "{bad:?} should be refused");
        }
    }
}
