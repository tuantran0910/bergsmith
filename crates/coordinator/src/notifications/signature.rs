//! Webhook signatures (BS-029.T1, ADR 0060).
//!
//! A webhook request carries `X-Bergsmith-Signature: t=<unix>,v1=<hex>`, where
//! the MAC is HMAC-SHA256 over `<t>.<body>`. The timestamp is part of the
//! signed string, so a receiver can reject an old request; the format mirrors
//! the widely implemented Stripe scheme so existing verification libraries
//! work unchanged.

use ring::hmac;

/// The header field carrying the signature.
///
/// Also re-exported from [`bergsmith_common::notifications`] so a reader of the
/// client contract does not need this crate.
pub const SIGNATURE_TIMESTAMP: &str = "t";

const VERSION: &str = "v1";

/// Computes the lowercase-hex HMAC-SHA256 of `<timestamp>.<body>`.
#[must_use]
pub fn mac_hex(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut context = hmac::Context::with_key(&key);
    context.update(timestamp.to_string().as_bytes());
    context.update(b".");
    context.update(body);
    to_hex(context.sign().as_ref())
}

/// Formats the value of the signature header.
#[must_use]
pub fn header_value(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    format!(
        "{SIGNATURE_TIMESTAMP}={timestamp},{VERSION}={}",
        mac_hex(secret, timestamp, body)
    )
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The expected value is the standard HMAC-SHA256 of
    /// `1700000000.<body>` under `whsec_test`, computed independently.
    const BODY: &[u8] = br#"{"id":"018f0000-0000-7000-8000-000000000000"}"#;

    #[test]
    fn the_mac_covers_the_timestamp_and_the_body() {
        assert_eq!(
            mac_hex(b"whsec_test", 1_700_000_000, BODY),
            "a0a387571e417a7a9815f8add790979ae6ac74492419b3cdf7dfcd0e00b5674b"
        );
        assert_ne!(
            mac_hex(b"whsec_test", 1_700_000_001, BODY),
            mac_hex(b"whsec_test", 1_700_000_000, BODY)
        );
        assert_ne!(
            mac_hex(b"whsec_test", 1_700_000_000, br#"{"id":"other"}"#),
            mac_hex(b"whsec_test", 1_700_000_000, BODY)
        );
    }

    #[test]
    fn the_header_carries_both_fields() {
        let header = header_value(b"whsec_test", 1_700_000_000, BODY);
        assert_eq!(
            header,
            "t=1700000000,v1=a0a387571e417a7a9815f8add790979ae6ac74492419b3cdf7dfcd0e00b5674b"
        );
    }
}
