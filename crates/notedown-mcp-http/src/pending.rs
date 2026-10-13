//! Pending logins and authorization codes. In memory only: a restart forgets them and the
//! client starts the sign-in again.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::secret;

pub(crate) const PENDING_TTL_MS: u64 = 10 * 60 * 1000;
pub(crate) const CODE_TTL_MS: u64 = 60 * 1000;
const MAX_PER_CLIENT: usize = 4;
const MAX_PENDING: usize = 1000;
const MAX_CODES: usize = 1000;

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub client_id: String,
    pub client_name: String,
    pub client_uri: Option<String>,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub challenge: String,
    pub resource: String,
    pub created_ms: u64,
}

/// No room for another pending login.
#[derive(Debug)]
pub(crate) struct Full;

#[derive(Default)]
pub(crate) struct PendingStore(Mutex<HashMap<String, Pending>>);

impl PendingStore {
    pub(crate) fn insert(&self, p: Pending, now_ms: u64) -> Result<String, Full> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, v| now_ms.saturating_sub(v.created_ms) < PENDING_TTL_MS);
        let mut mine: Vec<(String, u64)> = map
            .iter()
            .filter(|(_, v)| v.client_id == p.client_id)
            .map(|(k, v)| (k.clone(), v.created_ms))
            .collect();
        if mine.len() >= MAX_PER_CLIENT {
            mine.sort_by_key(|(_, at)| *at);
            for (k, _) in &mine[..=mine.len() - MAX_PER_CLIENT] {
                map.remove(k);
            }
        }
        if map.len() >= MAX_PENDING {
            return Err(Full);
        }
        let id = secret::random_hex_32();
        map.insert(id.clone(), p);
        Ok(id)
    }

    pub(crate) fn get(&self, id: &str, now_ms: u64) -> Option<Pending> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(id)
            .filter(|v| now_ms.saturating_sub(v.created_ms) < PENDING_TTL_MS)
            .cloned()
    }

    pub(crate) fn take(&self, id: &str, now_ms: u64) -> Option<Pending> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(id)
            .filter(|v| now_ms.saturating_sub(v.created_ms) < PENDING_TTL_MS)
    }
}

/// Read by the token route (Task 7).
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields are read by the token route of Task 7
pub(crate) struct Code {
    pub slug: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub resource: String,
    pub expires_ms: u64,
}

/// Codes by their SHA-256 hash.
#[derive(Default)]
pub(crate) struct CodeStore(Mutex<HashMap<String, Code>>);

impl CodeStore {
    pub(crate) fn issue(&self, c: Code) -> String {
        let code = secret::random_token("");
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_CODES {
            let now = c.expires_ms.saturating_sub(CODE_TTL_MS);
            map.retain(|_, v| v.expires_ms > now);
        }
        map.insert(secret::sha256_hex(&code), c);
        code
    }

    #[allow(dead_code)] // used by the token route of Task 7
    pub(crate) fn take(&self, code: &str, now_ms: u64) -> Option<Code> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&secret::sha256_hex(code))
            .filter(|c| now_ms < c.expires_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(client: &str, at: u64) -> Pending {
        Pending {
            client_id: client.into(),
            client_name: "c".into(),
            client_uri: None,
            redirect_uri: "https://a.example/cb".into(),
            state: None,
            challenge: "x".into(),
            resource: "r".into(),
            created_ms: at,
        }
    }

    #[test]
    fn pending_expires_caps_per_client_and_is_taken_once() {
        let s = PendingStore::default();
        let ids: Vec<String> = (0..5).map(|i| s.insert(p("c1", i), i).unwrap()).collect();
        assert!(
            s.get(&ids[0], 10).is_none(),
            "the oldest of a client is evicted at 4"
        );
        assert!(s.get(&ids[4], 10).is_some());
        assert!(s.get(&ids[4], PENDING_TTL_MS + 10).is_none(), "expired");
        let id = s.insert(p("c2", 0), 0).unwrap();
        assert!(s.take(&id, 1).is_some());
        assert!(s.take(&id, 1).is_none());
    }

    #[test]
    fn codes_are_single_use_and_expire() {
        let s = CodeStore::default();
        let c = |exp| Code {
            slug: "a".into(),
            client_id: "c".into(),
            redirect_uri: "r".into(),
            challenge: "x".into(),
            resource: "res".into(),
            expires_ms: exp,
        };
        let code = s.issue(c(CODE_TTL_MS));
        assert!(s.take(&code, 0).is_some());
        assert!(s.take(&code, 0).is_none());
        let code = s.issue(c(CODE_TTL_MS));
        assert!(s.take(&code, CODE_TTL_MS + 1).is_none());
    }
}
