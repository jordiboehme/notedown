//! Random ids and tokens, hashes and the PKCE check.

// Used from Task 4 on; removed in Task 10.
#![allow(dead_code)]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub(crate) fn random_hex_32() -> String {
    let mut b = [0u8; 16];
    rand::fill(&mut b[..]);
    const_hex::encode(b)
}

pub(crate) fn random_token(prefix: &str) -> String {
    let mut b = [0u8; 32];
    rand::fill(&mut b[..]);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(b))
}

pub(crate) fn sha256_hex(text: &str) -> String {
    const_hex::encode(Sha256::digest(text.as_bytes()))
}

fn unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~')
}

pub(crate) fn pkce_text_ok(text: &str) -> bool {
    (43..=128).contains(&text.len()) && text.bytes().all(unreserved)
}

pub(crate) fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    computed.as_bytes().ct_eq(challenge.as_bytes()).into()
}

pub(crate) fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_example_verifier_matches_its_challenge() {
        let verifier = "dBjftJeZ4CVP-mJ92K28uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "jDBroimTx0Ag-gB-3YHpo5TS8niR-0pvTfRtAXTl1p8";
        assert!(pkce_matches(verifier, challenge));
        assert!(!pkce_matches(
            verifier,
            "jDBroimTx0Ag-gB-3YHpo5TS8niR-0pvTfRtAXTl1p9"
        ));
        assert!(!pkce_matches(verifier, ""));
        assert!(!pkce_matches(verifier, &format!("{challenge}x")));
        assert!(pkce_text_ok(verifier));
        assert!(!pkce_text_ok("short"));
        assert!(!pkce_text_ok(&"a".repeat(129)));
        assert!(!pkce_text_ok(&format!("{}+", "a".repeat(43))));
    }

    #[test]
    fn tokens_have_their_prefix_and_length() {
        let t = random_token("nda_");
        assert!(t.starts_with("nda_"));
        assert_eq!(t.len(), 4 + 43);
        assert_ne!(t, random_token("nda_"));
        assert_eq!(random_hex_32().len(), 32);
        assert_eq!(sha256_hex("abc").len(), 64);
    }

    #[test]
    fn percent_encode_keeps_only_unreserved() {
        assert_eq!(
            percent_encode("a b&c=d/é~._-"),
            "a%20b%26c%3Dd%2F%C3%A9~._-"
        );
    }
}
