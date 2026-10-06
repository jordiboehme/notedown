//! Login codes: in memory, single use, five minutes.

use std::collections::HashMap;
use std::sync::Mutex;

use rand::RngExt;

pub const CODE_LEN: usize = 8;
pub const CODE_TTL_MS: u64 = 5 * 60 * 1000;
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// One pending code per email. A restart drops them all, which only forces a new challenge.
#[derive(Debug, Default)]
pub struct LoginCodes {
    pending: Mutex<HashMap<String, (String, u64)>>,
}

impl LoginCodes {
    pub fn new() -> LoginCodes {
        LoginCodes::default()
    }

    /// New random code and its timestamp (ms). Replaces an earlier code of the same email.
    pub fn issue(&self, email: &str, now_ms: u64) -> (String, u64) {
        let mut rng = rand::rng();
        let code: String = (0..CODE_LEN)
            .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
            .collect();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, (_, ts)| now_ms.saturating_sub(*ts) <= CODE_TTL_MS);
        pending.insert(key(email), (code.clone(), now_ms));
        (code, now_ms)
    }

    /// Take the code of this email: it is gone afterwards, whatever the caller does with it.
    /// `None` when there is none or it is older than five minutes.
    pub fn consume(&self, email: &str, now_ms: u64) -> Option<String> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let (code, ts) = pending.remove(&key(email))?;
        (now_ms.saturating_sub(ts) <= CODE_TTL_MS).then_some(code)
    }
}

fn key(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}
