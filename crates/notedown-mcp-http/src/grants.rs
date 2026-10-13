//! Grants: one file per sign-in in `<data>/accounts/<slug>/oauth/<id>.json` (mode 0600), with
//! only SHA-256 hashes of its tokens. An in-memory index maps token hashes to grants; the file
//! is the truth, so a grant deleted by the CLI (`passwd`, `account oauth --revoke`) ends at once.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use notedown_cloud::auth::{Accounts, OAUTH_DIR};
use serde::{Deserialize, Serialize};

use crate::secret;

pub(crate) const ACCESS_TTL_MS: u64 = 60 * 60 * 1000;
pub(crate) const REFRESH_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Replaced refresh hashes kept per grant for replay detection.
const KEEP_REPLACED: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Grant {
    id: String,
    client_id: String,
    client_name: String,
    resource: String,
    scope: String,
    created_ms: u64,
    used_ms: u64,
    access_hash: String,
    access_expires_ms: u64,
    refresh_hash: String,
    refresh_expires_ms: u64,
    #[serde(default)]
    replaced: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Access,
    Refresh,
    Replaced,
}

#[derive(Debug, Clone)]
struct Entry {
    slug: String,
    grant: String,
    kind: Kind,
}

pub(crate) struct Issued {
    pub access: String,
    pub refresh: String,
}

/// Why a refresh was refused; the answer is always `invalid_grant` or `invalid_target`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Grant,
    Target,
}

pub struct GrantSummary {
    pub id: String,
    pub client_name: String,
    pub created_ms: u64,
    pub used_ms: u64,
}

pub(crate) struct GrantStore {
    accounts: Accounts,
    index: Mutex<HashMap<String, Entry>>,
}

fn dir_of(accounts: &Accounts, slug: &str) -> PathBuf {
    accounts.account_dir(slug).join(OAUTH_DIR)
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read(accounts: &Accounts, slug: &str, id: &str) -> anyhow::Result<Option<Grant>> {
    if !valid_id(id) {
        return Ok(None);
    }
    match std::fs::read(dir_of(accounts, slug).join(format!("{id}.json"))) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write(accounts: &Accounts, slug: &str, g: &Grant) -> anyhow::Result<()> {
    let path = dir_of(accounts, slug).join(format!("{}.json", g.id));
    notedown_cloud::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(g)?, true)?;
    Ok(())
}

fn delete(accounts: &Accounts, slug: &str, id: &str) -> anyhow::Result<bool> {
    match std::fs::remove_file(dir_of(accounts, slug).join(format!("{id}.json"))) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Every grant of every account: (slug, grant).
fn all(accounts: &Accounts) -> anyhow::Result<Vec<(String, Grant)>> {
    let mut out = Vec::new();
    for a in accounts.list()? {
        for (_, g) in of_account(accounts, &a.slug)? {
            out.push((a.slug.clone(), g));
        }
    }
    Ok(out)
}

fn of_account(accounts: &Accounts, slug: &str) -> anyhow::Result<Vec<(String, Grant)>> {
    let dir = dir_of(accounts, slug);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if let Some(g) = read(accounts, slug, id)? {
            out.push((id.to_string(), g));
        }
    }
    out.sort_by_key(|(_, g)| g.created_ms);
    Ok(out)
}

pub fn list_grants(data: &Path, slug: &str) -> anyhow::Result<Vec<GrantSummary>> {
    Ok(of_account(&Accounts::new(data), slug)?
        .into_iter()
        .map(|(id, g)| GrantSummary {
            id,
            client_name: g.client_name,
            created_ms: g.created_ms,
            used_ms: g.used_ms,
        })
        .collect())
}

pub fn revoke_grants(data: &Path, slug: &str, id: Option<&str>) -> anyhow::Result<usize> {
    let accounts = Accounts::new(data);
    let mut n = 0;
    for (gid, _) in of_account(&accounts, slug)? {
        if id.is_none_or(|want| want == gid) && delete(&accounts, slug, &gid)? {
            n += 1;
        }
    }
    Ok(n)
}

impl GrantStore {
    pub(crate) fn open(data: &Path) -> anyhow::Result<GrantStore> {
        let accounts = Accounts::new(data);
        let mut index = HashMap::new();
        for (slug, g) in all(&accounts)? {
            Self::index_grant(&mut index, &slug, &g);
        }
        Ok(GrantStore {
            accounts,
            index: Mutex::new(index),
        })
    }

    fn index_grant(index: &mut HashMap<String, Entry>, slug: &str, g: &Grant) {
        let e = |kind| Entry {
            slug: slug.to_string(),
            grant: g.id.clone(),
            kind,
        };
        index.insert(g.access_hash.clone(), e(Kind::Access));
        index.insert(g.refresh_hash.clone(), e(Kind::Refresh));
        for h in &g.replaced {
            index.insert(h.clone(), e(Kind::Replaced));
        }
    }

    fn unindex(index: &mut HashMap<String, Entry>, grant: &str) {
        index.retain(|_, e| e.grant != grant);
    }

    fn new_tokens(g: &mut Grant, now_ms: u64) -> Issued {
        let access = secret::random_token("nda_");
        let refresh = secret::random_token("ndr_");
        g.access_hash = secret::sha256_hex(&access);
        g.access_expires_ms = now_ms + ACCESS_TTL_MS;
        g.refresh_hash = secret::sha256_hex(&refresh);
        g.refresh_expires_ms = now_ms + REFRESH_TTL_MS;
        g.used_ms = now_ms;
        Issued { access, refresh }
    }

    pub(crate) fn issue(
        &self,
        slug: &str,
        client_id: &str,
        client_name: &str,
        resource: &str,
        now_ms: u64,
    ) -> anyhow::Result<Issued> {
        let mut g = Grant {
            id: secret::random_hex_32(),
            client_id: client_id.into(),
            client_name: client_name.into(),
            resource: resource.into(),
            scope: crate::SCOPE.into(),
            created_ms: now_ms,
            used_ms: now_ms,
            access_hash: String::new(),
            access_expires_ms: 0,
            refresh_hash: String::new(),
            refresh_expires_ms: 0,
            replaced: Vec::new(),
        };
        let issued = Self::new_tokens(&mut g, now_ms);
        let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        write(&self.accounts, slug, &g)?;
        Self::index_grant(&mut index, slug, &g);
        Ok(issued)
    }

    /// The account of a valid access token for `resource`, else `None`.
    #[allow(dead_code)] // used by the MCP gate of the tasks that follow
    pub(crate) fn access(
        &self,
        token: &str,
        resource: &str,
        now_ms: u64,
    ) -> anyhow::Result<Option<String>> {
        let hash = secret::sha256_hex(token);
        let entry = {
            let index = self.index.lock().unwrap_or_else(|e| e.into_inner());
            index.get(&hash).cloned()
        };
        let Some(entry) = entry.filter(|e| e.kind == Kind::Access) else {
            return Ok(None);
        };
        let Some(g) = read(&self.accounts, &entry.slug, &entry.grant)? else {
            let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
            Self::unindex(&mut index, &entry.grant);
            return Ok(None);
        };
        let ok = g.access_hash == hash && now_ms < g.access_expires_ms && g.resource == resource;
        Ok(ok.then_some(entry.slug))
    }

    pub(crate) fn refresh(
        &self,
        token: &str,
        client_id: &str,
        resource: &str,
        now_ms: u64,
    ) -> anyhow::Result<Result<Issued, Refused>> {
        let hash = secret::sha256_hex(token);
        let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = index.get(&hash).cloned() else {
            return Ok(Err(Refused::Grant));
        };
        if entry.kind == Kind::Replaced {
            // A replay: the token was rotated already. End the whole grant.
            delete(&self.accounts, &entry.slug, &entry.grant)?;
            Self::unindex(&mut index, &entry.grant);
            return Ok(Err(Refused::Grant));
        }
        if entry.kind != Kind::Refresh {
            return Ok(Err(Refused::Grant));
        }
        let Some(mut g) = read(&self.accounts, &entry.slug, &entry.grant)? else {
            Self::unindex(&mut index, &entry.grant);
            return Ok(Err(Refused::Grant));
        };
        if g.refresh_hash != hash || g.client_id != client_id || now_ms >= g.refresh_expires_ms {
            return Ok(Err(Refused::Grant));
        }
        if g.resource != resource {
            return Ok(Err(Refused::Target));
        }
        g.replaced.push(g.refresh_hash.clone());
        if g.replaced.len() > KEEP_REPLACED {
            g.replaced.remove(0);
        }
        let issued = Self::new_tokens(&mut g, now_ms);
        write(&self.accounts, &entry.slug, &g)?;
        Self::unindex(&mut index, &g.id);
        Self::index_grant(&mut index, &entry.slug, &g);
        Ok(Ok(issued))
    }

    /// Ends the grant of an access or refresh token; an unknown token is ignored.
    pub(crate) fn revoke(&self, token: &str) -> anyhow::Result<()> {
        let hash = secret::sha256_hex(token);
        let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = index
            .get(&hash)
            .cloned()
            .filter(|e| e.kind != Kind::Replaced)
        {
            delete(&self.accounts, &entry.slug, &entry.grant)?;
            Self::unindex(&mut index, &entry.grant);
        }
        Ok(())
    }

    pub(crate) fn client_ids(&self) -> HashSet<String> {
        let index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        let grants: HashSet<(String, String)> = index
            .values()
            .map(|e| (e.slug.clone(), e.grant.clone()))
            .collect();
        drop(index);
        grants
            .into_iter()
            .filter_map(|(slug, id)| read(&self.accounts, &slug, &id).ok().flatten())
            .map(|g| g.client_id)
            .collect()
    }

    /// Deletes the grants whose refresh token expired, and forgets index entries of grants that
    /// are gone (deleted by the CLI).
    pub(crate) fn sweep(&self, now_ms: u64) -> anyhow::Result<usize> {
        let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        let grants: HashSet<(String, String)> = index
            .values()
            .map(|e| (e.slug.clone(), e.grant.clone()))
            .collect();
        let mut n = 0;
        for (slug, id) in grants {
            match read(&self.accounts, &slug, &id)? {
                Some(g) if now_ms >= g.refresh_expires_ms => {
                    delete(&self.accounts, &slug, &id)?;
                    Self::unindex(&mut index, &id);
                    n += 1;
                }
                Some(_) => {}
                None => Self::unindex(&mut index, &id),
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_rebuilt_from_the_files_and_access_checks_resource_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        let t = store
            .issue("anna", "ndc_1", "Claude", "https://n/mcp", 0)
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        assert_eq!(
            store
                .access(&t.access, "https://n/mcp", 1)
                .unwrap()
                .as_deref(),
            Some("anna")
        );
        assert_eq!(
            store.access(&t.access, "https://other/mcp", 1).unwrap(),
            None
        );
        assert_eq!(
            store
                .access(&t.access, "https://n/mcp", ACCESS_TTL_MS)
                .unwrap(),
            None
        );
        assert_eq!(store.access("nda_nope", "https://n/mcp", 1).unwrap(), None);
        assert_eq!(list_grants(dir.path(), "anna").unwrap().len(), 1);
        assert_eq!(revoke_grants(dir.path(), "anna", None).unwrap(), 1);
        assert_eq!(
            store.access(&t.access, "https://n/mcp", 1).unwrap(),
            None,
            "file gone"
        );
    }

    #[test]
    fn sweep_deletes_grants_past_their_refresh_expiry() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        store.issue("anna", "ndc_1", "Claude", "r", 0).unwrap();
        assert_eq!(store.sweep(REFRESH_TTL_MS - 1).unwrap(), 0);
        assert_eq!(store.sweep(REFRESH_TTL_MS).unwrap(), 1);
        assert!(store.client_ids().is_empty());
    }
}
