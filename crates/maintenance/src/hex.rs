//! Serde for optional bytes as lowercase hex strings.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};

#[expect(
    clippy::ref_option,
    reason = "serde's `with` passes the field by reference"
)]
pub(crate) fn serialize<S: Serializer>(
    bytes: &Option<Vec<u8>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match bytes {
        Some(bytes) => serializer.serialize_some(&encode(bytes)),
        None => serializer.serialize_none(),
    }
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<u8>>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|hex| decode(&hex).ok_or_else(|| D::Error::custom("expected lowercase hex bytes")))
        .transpose()
}

pub(crate) fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

/// Decodes lowercase hex; anything else is `None`, so each value has one
/// spelling and re-serializing a plan reproduces its hash.
pub(crate) fn decode(hex: &str) -> Option<Vec<u8>> {
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let hex = hex.as_bytes();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.chunks_exact(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_has_one_spelling() {
        assert_eq!(encode(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(decode("00abff"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(decode(""), Some(vec![]));
        for invalid in ["0", "AB", "zz", "0g"] {
            assert_eq!(decode(invalid), None, "{invalid}");
        }
    }
}
