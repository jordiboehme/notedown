//! Registered clients (RFC 7591, public clients only): one file per client in
//! `<data>/oauth/clients/`.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use notedown_cloud::api::ClientAddr;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Oauth, oauth_error, redirect, secret};

pub(crate) const MAX_CLIENTS: usize = 1000;
const MAX_REDIRECT_URIS: usize = 10;
const MAX_NAME_CHARS: usize = 100;
const UNUSED_MS: u64 = 30 * 24 * 3600 * 1000;
/// At the cap, a client that never got a grant is pruned once it is this old.
const NEVER_USED_MS: u64 = 10 * 60 * 1000;
pub(crate) const REGISTRATION_BURST: usize = 30;
pub(crate) const REGISTRATION_WINDOW_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Client {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    pub redirect_uris: Vec<String>,
    pub created_ms: u64,
    pub used_ms: u64,
}

pub(crate) struct ClientStore {
    dir: PathBuf,
    write: Mutex<()>,
}

fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.starts_with("ndc_")
        && id[4..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl ClientStore {
    pub(crate) fn new(data: &Path) -> ClientStore {
        ClientStore {
            dir: data.join("oauth").join("clients"),
            write: Mutex::new(()),
        }
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub(crate) fn get(&self, id: &str) -> anyhow::Result<Option<Client>> {
        if !valid_id(id) {
            return Ok(None);
        }
        match std::fs::read(self.file(id)) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn create(&self, c: &Client) -> anyhow::Result<()> {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        self.write_file(c)
    }

    fn write_file(&self, c: &Client) -> anyhow::Result<()> {
        notedown_cloud::fsutil::write_atomic(
            &self.file(&c.id),
            &serde_json::to_vec_pretty(c)?,
            false,
        )?;
        Ok(())
    }

    /// Records a use; a failure only loses the stamp.
    pub(crate) fn touch(&self, id: &str, now_ms: u64) {
        if let Ok(Some(mut c)) = self.get(id) {
            c.used_ms = now_ms;
            let _ = self.create(&c);
        }
    }

    fn all(&self) -> anyhow::Result<Vec<Client>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            match self.get(id) {
                Ok(Some(c)) => out.push(c),
                Ok(None) => {}
                Err(_) => eprintln!("notedown mcp: skipping unreadable client file {name:?}"),
            }
        }
        Ok(out)
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> anyhow::Result<usize> {
        Ok(self.all()?.len())
    }

    /// Deletes the clients without a grant (`in_use`) not used for 30 days.
    pub(crate) fn prune(&self, now_ms: u64, in_use: &HashSet<String>) -> anyhow::Result<usize> {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        self.prune_locked(now_ms, in_use)
    }

    fn prune_locked(&self, now_ms: u64, in_use: &HashSet<String>) -> anyhow::Result<usize> {
        self.remove_where(in_use, |c| {
            now_ms.saturating_sub(c.used_ms.max(c.created_ms)) > UNUSED_MS
        })
    }

    /// Deletes the clients that never got a grant (`used_ms == created_ms`, not in `in_use`) and
    /// are older than [`NEVER_USED_MS`].
    fn prune_never_used_locked(
        &self,
        now_ms: u64,
        in_use: &HashSet<String>,
    ) -> anyhow::Result<usize> {
        self.remove_where(in_use, |c| {
            c.used_ms == c.created_ms && now_ms.saturating_sub(c.created_ms) > NEVER_USED_MS
        })
    }

    /// Deletes the clients not in `in_use` for which `old` holds.
    fn remove_where(
        &self,
        in_use: &HashSet<String>,
        old: impl Fn(&Client) -> bool,
    ) -> anyhow::Result<usize> {
        let mut n = 0;
        for c in self.all()? {
            if !in_use.contains(&c.id) && old(&c) {
                match std::fs::remove_file(self.file(&c.id)) {
                    Ok(()) => n += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(n)
    }

    /// Stores `c` unless the store holds [`MAX_CLIENTS`] after pruning; `Ok(false)` at the cap.
    /// At the cap it first prunes the clients that never got a grant and are older than
    /// [`NEVER_USED_MS`], then the ones unused for 30 days. Count, prune and write happen under
    /// one hold of the write lock.
    pub(crate) fn create_capped(
        &self,
        c: &Client,
        now_ms: u64,
        in_use: &HashSet<String>,
    ) -> anyhow::Result<bool> {
        let _w = self.write.lock().unwrap_or_else(|e| e.into_inner());
        if self.all()?.len() >= MAX_CLIENTS {
            self.prune_never_used_locked(now_ms, in_use)?;
        }
        if self.all()?.len() >= MAX_CLIENTS {
            self.prune_locked(now_ms, in_use)?;
            if self.all()?.len() >= MAX_CLIENTS {
                return Ok(false);
            }
        }
        self.write_file(c)?;
        Ok(true)
    }
}

/// At most [`REGISTRATION_BURST`] registrations per [`REGISTRATION_WINDOW_MS`], for all callers.
#[derive(Default)]
pub(crate) struct RegistrationLimiter(Mutex<VecDeque<u64>>);

impl RegistrationLimiter {
    pub(crate) fn allow(&self, now_ms: u64) -> bool {
        let mut q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        while q
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= REGISTRATION_WINDOW_MS)
        {
            q.pop_front();
        }
        if q.len() >= REGISTRATION_BURST {
            return false;
        }
        q.push_back(now_ms);
        true
    }
}

/// Unknown fields are ignored (RFC 7591).
#[derive(Deserialize)]
pub(crate) struct RegisterBody {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    client_uri: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

/// Characters that are invisible or reorder text, which could spoof the login page.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2028}' | '\u{2029}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
}

fn metadata_error(description: &str) -> Response {
    oauth_error(
        StatusCode::BAD_REQUEST,
        "invalid_client_metadata",
        description,
    )
}

pub(crate) async fn register(
    State(o): State<Arc<Oauth>>,
    Extension(ClientAddr(ip)): Extension<ClientAddr>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > crate::MAX_REGISTER_BYTES {
        return metadata_error("the registration is larger than 64 KiB");
    }
    let Ok(b) = serde_json::from_slice::<RegisterBody>(&body) else {
        return metadata_error("the registration is not a JSON object of client metadata");
    };
    if b.redirect_uris.is_empty() || b.redirect_uris.len() > MAX_REDIRECT_URIS {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "give 1 to 10 redirect_uris",
        );
    }
    for uri in &b.redirect_uris {
        if let Some(why) = redirect::problem(uri) {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                &format!("redirect URI {why}"),
            );
        }
    }
    if b.token_endpoint_auth_method
        .as_deref()
        .is_some_and(|m| m != "none")
    {
        return metadata_error("only public clients: token_endpoint_auth_method must be none");
    }
    if b.grant_types.as_ref().is_some_and(|g| {
        g.iter()
            .any(|g| g != "authorization_code" && g != "refresh_token")
    }) {
        return metadata_error("grant_types may hold only authorization_code and refresh_token");
    }
    if b.response_types
        .as_ref()
        .is_some_and(|r| r.iter().any(|r| r != "code"))
    {
        return metadata_error("response_types may hold only code");
    }
    if b.client_uri
        .as_deref()
        .is_some_and(|u| !u.starts_with("https://") || redirect::problem(u).is_some())
    {
        return metadata_error("client_uri must be an https URL");
    }
    let now = o.now_ms();
    // Per address first: the global limiter counts only what it allows, so one address that
    // is over its own limit never uses up the global slots.
    if !o.login_limiter.allow(ip, "oauth-register", now) {
        let mut r = oauth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "invalid_client_metadata",
            "too many registrations from this address, try again in a minute",
        );
        r.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, "60".parse().unwrap());
        return r;
    }
    if !o.registrations.allow(now) {
        let mut r = oauth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "invalid_client_metadata",
            "too many registrations, try again in 10 minutes",
        );
        r.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, "600".parse().unwrap());
        return r;
    }
    let name: String = b
        .client_name
        .as_deref()
        .unwrap_or("an MCP client")
        .trim()
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .take(MAX_NAME_CHARS)
        .collect();
    let client = Client {
        id: format!("ndc_{}", secret::random_hex_32()),
        name: if name.is_empty() {
            "an MCP client".into()
        } else {
            name
        },
        uri: b.client_uri,
        redirect_uris: b.redirect_uris,
        created_ms: now,
        used_ms: now,
    };
    let stored = {
        let (o, client) = (o.clone(), client.clone());
        tokio::task::spawn_blocking(move || {
            o.clients.create_capped(&client, now, &o.grants_in_use())
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("store task failed: {e}")))
    };
    match stored {
        Ok(true) => (
            StatusCode::CREATED,
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Json(json!({
                "client_id": client.id,
                "client_id_issued_at": now / 1000,
                "client_name": client.name,
                "client_uri": client.uri,
                "redirect_uris": client.redirect_uris,
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
            })),
        )
            .into_response(),
        Ok(false) => oauth_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "this server holds the most clients it keeps",
        ),
        Err(e) => {
            eprintln!("notedown mcp: client registration failed: {e}");
            oauth_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                "the client could not be stored",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_removes_old_unused_clients_without_a_grant() {
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::new(dir.path());
        let id = |c: char| format!("ndc_{}", c.to_string().repeat(32));
        let (old, kept, new) = (id('a'), id('b'), id('c'));
        let day = 24 * 3600 * 1000;
        let mk = |id: &str, used| Client {
            id: id.into(),
            name: "x".into(),
            uri: None,
            redirect_uris: vec!["https://a.example/cb".into()],
            created_ms: 0,
            used_ms: used,
        };
        store.create(&mk(&old, 0)).unwrap();
        store.create(&mk(&kept, 0)).unwrap();
        store.create(&mk(&new, 29 * day)).unwrap();
        let in_use: HashSet<String> = [kept.clone()].into();
        assert_eq!(store.prune(31 * day, &in_use).unwrap(), 1);
        assert!(store.get(&old).unwrap().is_none());
        assert!(store.get(&kept).unwrap().is_some());
        assert!(store.get(&new).unwrap().is_some());
    }

    fn client(id: String, used: u64) -> Client {
        Client {
            id,
            name: "x".into(),
            uri: None,
            redirect_uris: vec!["https://a.example/cb".into()],
            created_ms: used,
            used_ms: used,
        }
    }

    fn fill(store: &ClientStore, n: usize, used: u64) {
        for i in 0..n {
            let c = client(format!("ndc_{i:032x}"), used);
            std::fs::create_dir_all(&store.dir).unwrap();
            std::fs::write(store.file(&c.id), serde_json::to_vec(&c).unwrap()).unwrap();
        }
    }

    #[test]
    fn the_cap_refuses_when_nothing_is_prunable_and_prunes_when_it_can() {
        let day = 24 * 3600 * 1000;
        let none = HashSet::new();
        let new = client(format!("ndc_{}", "f".repeat(32)), 100 * day);

        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::new(dir.path());
        fill(&store, MAX_CLIENTS, 100 * day);
        assert!(!store.create_capped(&new, 100 * day, &none).unwrap());
        assert_eq!(store.count().unwrap(), MAX_CLIENTS);

        // Used once, then not for 30 days.
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::new(dir.path());
        fill(&store, MAX_CLIENTS, 0);
        for i in 0..MAX_CLIENTS {
            store.touch(&format!("ndc_{i:032x}"), 1);
        }
        assert!(store.create_capped(&new, 100 * day, &none).unwrap());
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn at_the_cap_never_used_clients_older_than_10_minutes_go_first() {
        let min = 60 * 1000;
        let none = HashSet::new();
        let new = client(format!("ndc_{}", "f".repeat(32)), 11 * min);

        // Never used and 11 minutes old: pruned.
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::new(dir.path());
        fill(&store, MAX_CLIENTS, 0);
        assert!(store.create_capped(&new, 11 * min, &none).unwrap());
        assert_eq!(store.count().unwrap(), 1);

        // Never used but only 9 minutes old: kept.
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::new(dir.path());
        fill(&store, MAX_CLIENTS, 2 * min);
        assert!(!store.create_capped(&new, 11 * min, &none).unwrap());

        // Used once (a code was exchanged), or holding a grant: kept.
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::new(dir.path());
        fill(&store, MAX_CLIENTS - 1, 0);
        for i in 0..MAX_CLIENTS - 1 {
            store.touch(&format!("ndc_{i:032x}"), 1);
        }
        let granted = format!("ndc_{}", "e".repeat(32));
        store.create(&client(granted.clone(), 0)).unwrap();
        let in_use: HashSet<String> = [granted].into();
        assert!(!store.create_capped(&new, 11 * min, &in_use).unwrap());
        assert_eq!(store.count().unwrap(), MAX_CLIENTS);
    }

    #[test]
    fn corrupt_files_are_skipped_and_odd_ids_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = ClientStore::new(dir.path());
        fill(&store, 1, 0);
        std::fs::write(
            dir.path()
                .join("oauth/clients")
                .join(format!("ndc_{}.json", "e".repeat(32))),
            b"{not json",
        )
        .unwrap();
        assert_eq!(store.count().unwrap(), 1);
        assert!(store.get("../x").unwrap().is_none());
        assert!(store.get("ndc_../../..").unwrap().is_none());
        assert!(
            store
                .get(&format!("ndc_{}", "A".repeat(32)))
                .unwrap()
                .is_none()
        );
    }
}
