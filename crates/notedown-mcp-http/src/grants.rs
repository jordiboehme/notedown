//! Grants: one file per sign-in in `<data>/accounts/<slug>/oauth/<id>.json` (mode 0600), with
//! only SHA-256 hashes of its tokens. An in-memory index maps token hashes to grants; the file
//! is the truth, so a grant deleted by the CLI (`passwd`, `account oauth --revoke`) ends at once.
//!
//! Writers (`issue`, the rewrite of `refresh`) hold the account lock of notedown-cloud, so a
//! grant that `passwd`, `account remove` or `revoke_grants` deleted is never written back.
//! Lock order: the account lock is never waited for while the index mutex is held.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use notedown_cloud::auth::{Accounts, OAUTH_DIR, validate_slug};
use serde::{Deserialize, Serialize};

use crate::secret;

pub(crate) const ACCESS_TTL_MS: u64 = 60 * 60 * 1000;
pub(crate) const REFRESH_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Hard cap of replaced refresh tokens kept per grant for replay detection.
const KEEP_REPLACED: usize = 1000;

/// A rotated refresh token: its hash and the refresh expiry it had.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Replaced {
    hash: String,
    expires_ms: u64,
}

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
    replaced: Vec<Replaced>,
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

fn path_of(accounts: &Accounts, slug: &str, id: &str) -> PathBuf {
    dir_of(accounts, slug).join(format!("{id}.json"))
}

fn read(accounts: &Accounts, slug: &str, id: &str) -> anyhow::Result<Option<Grant>> {
    if !valid_id(id) {
        return Ok(None);
    }
    match std::fs::read(path_of(accounts, slug, id)) {
        Ok(b) => {
            let g: Grant = serde_json::from_slice(&b)?;
            Ok((g.id == id).then_some(g))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write(accounts: &Accounts, slug: &str, g: &Grant) -> anyhow::Result<()> {
    let path = path_of(accounts, slug, &g.id);
    notedown_cloud::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(g)?, true)?;
    Ok(())
}

fn delete(accounts: &Accounts, slug: &str, id: &str) -> anyhow::Result<bool> {
    if !valid_id(id) {
        return Ok(false);
    }
    match std::fs::remove_file(path_of(accounts, slug, id)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Every grant of every account: (slug, grant). An account whose grant folder cannot be listed
/// is skipped with a log line, and so is a bad grant file; only a failing account listing fails.
fn all(accounts: &Accounts) -> anyhow::Result<Vec<(String, Grant)>> {
    let mut out = Vec::new();
    for a in accounts.list()? {
        match of_account(accounts, &a.slug) {
            Ok(grants) => out.extend(grants.into_iter().map(|(_, g)| (a.slug.clone(), g))),
            Err(e) => eprintln!(
                "notedown mcp: skipping the grants of account {}: {e}",
                a.slug
            ),
        }
    }
    Ok(out)
}

/// The ids (file stems) of the grant files of an account, without reading them.
fn ids_of(accounts: &Accounts, slug: &str) -> anyhow::Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir_of(accounts, slug)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json"))
            && valid_id(id)
        {
            out.push(id.to_string());
        }
    }
    Ok(out)
}

/// The readable grants of an account, oldest first. A bad file is logged (path only) and skipped.
fn of_account(accounts: &Accounts, slug: &str) -> anyhow::Result<Vec<(String, Grant)>> {
    let mut out = Vec::new();
    for id in ids_of(accounts, slug)? {
        match read(accounts, slug, &id) {
            Ok(Some(g)) => out.push((id, g)),
            Ok(None) => {}
            Err(_) => eprintln!(
                "notedown mcp: skipping unreadable grant file {}",
                path_of(accounts, slug, &id).display()
            ),
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

/// Deletes the grant files of an account (`None`: all), by file name without parsing them, so a
/// corrupt grant can be revoked too. Holds the account lock.
pub fn revoke_grants(data: &Path, slug: &str, id: Option<&str>) -> anyhow::Result<usize> {
    validate_slug(slug)?;
    let accounts = Accounts::new(data);
    if !accounts.account_dir(slug).is_dir() {
        return Ok(0);
    }
    let _lock = accounts.lock(slug)?;
    let mut n = 0;
    for gid in ids_of(&accounts, slug)? {
        if id.is_none_or(|want| want == gid) && delete(&accounts, slug, &gid)? {
            n += 1;
        }
    }
    Ok(n)
}

/// Drops the expired replaced tokens, remembers the current refresh token as replaced and keeps
/// at most `cap` entries (the oldest go first).
fn remember_replaced(g: &mut Grant, now_ms: u64, cap: usize) {
    g.replaced.retain(|r| r.expires_ms > now_ms);
    g.replaced.push(Replaced {
        hash: g.refresh_hash.clone(),
        expires_ms: g.refresh_expires_ms,
    });
    if g.replaced.len() > cap {
        let excess = g.replaced.len() - cap;
        g.replaced.drain(..excess);
    }
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

    fn index_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn index_grant(index: &mut HashMap<String, Entry>, slug: &str, g: &Grant) {
        let e = |kind| Entry {
            slug: slug.to_string(),
            grant: g.id.clone(),
            kind,
        };
        index.insert(g.access_hash.clone(), e(Kind::Access));
        index.insert(g.refresh_hash.clone(), e(Kind::Refresh));
        for r in &g.replaced {
            index.insert(r.hash.clone(), e(Kind::Replaced));
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

    /// Takes the account lock; `None` when the account folder is gone (nothing is created).
    fn lock(&self, slug: &str) -> anyhow::Result<Option<notedown_cloud::auth::AccountLock>> {
        if !self.accounts.account_dir(slug).is_dir() {
            return Ok(None);
        }
        match self.accounts.lock(slug) {
            Ok(l) => Ok(Some(l)),
            // The folder can vanish between the check and the open.
            Err(_) if !self.accounts.account_dir(slug).is_dir() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn issue(
        &self,
        slug: &str,
        client_id: &str,
        client_name: &str,
        resource: &str,
        now_ms: u64,
    ) -> anyhow::Result<Option<Issued>> {
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
        // `None`: the account is gone, nothing is written.
        let Some(_lock) = self.lock(slug)? else {
            return Ok(None);
        };
        if self.accounts.get(slug)?.is_none() {
            return Ok(None);
        }
        write(&self.accounts, slug, &g)?;
        Self::index_grant(&mut self.index_lock(), slug, &g);
        Ok(Some(issued))
    }

    /// The account of a valid access token for `resource`, else `None`.
    pub(crate) fn access(
        &self,
        token: &str,
        resource: &str,
        now_ms: u64,
    ) -> anyhow::Result<Option<String>> {
        let hash = secret::sha256_hex(token);
        let entry = self.index_lock().get(&hash).cloned();
        let Some(entry) = entry.filter(|e| e.kind == Kind::Access) else {
            return Ok(None);
        };
        let Some(g) = read(&self.accounts, &entry.slug, &entry.grant)? else {
            Self::unindex(&mut self.index_lock(), &entry.grant);
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
        let entry = self.index_lock().get(&hash).cloned();
        let Some(entry) = entry else {
            return Ok(Err(Refused::Grant));
        };
        if entry.kind == Kind::Replaced {
            // A replay: the token was rotated already. End the whole grant.
            delete(&self.accounts, &entry.slug, &entry.grant)?;
            Self::unindex(&mut self.index_lock(), &entry.grant);
            return Ok(Err(Refused::Grant));
        }
        if entry.kind != Kind::Refresh {
            return Ok(Err(Refused::Grant));
        }
        self.refresh_locked(&entry, &hash, client_id, resource, now_ms)
    }

    /// The part of `refresh` after the index lookup: under the account lock, against the file.
    fn refresh_locked(
        &self,
        entry: &Entry,
        hash: &str,
        client_id: &str,
        resource: &str,
        now_ms: u64,
    ) -> anyhow::Result<Result<Issued, Refused>> {
        // The account lock, never waited for under the index mutex.
        let Some(_lock) = self.lock(&entry.slug)? else {
            Self::unindex(&mut self.index_lock(), &entry.grant);
            return Ok(Err(Refused::Grant));
        };
        if self.accounts.get(&entry.slug)?.is_none() {
            Self::unindex(&mut self.index_lock(), &entry.grant);
            return Ok(Err(Refused::Grant));
        }
        let Some(mut g) = read(&self.accounts, &entry.slug, &entry.grant)? else {
            Self::unindex(&mut self.index_lock(), &entry.grant);
            return Ok(Err(Refused::Grant));
        };
        if g.replaced.iter().any(|r| r.hash == hash) {
            // A replay that raced a rotation: end the whole grant.
            delete(&self.accounts, &entry.slug, &g.id)?;
            Self::unindex(&mut self.index_lock(), &g.id);
            return Ok(Err(Refused::Grant));
        }
        if g.refresh_hash != hash || g.client_id != client_id || now_ms >= g.refresh_expires_ms {
            return Ok(Err(Refused::Grant));
        }
        if g.resource != resource {
            return Ok(Err(Refused::Target));
        }
        remember_replaced(&mut g, now_ms, KEEP_REPLACED);
        let issued = Self::new_tokens(&mut g, now_ms);
        write(&self.accounts, &entry.slug, &g)?;
        let mut index = self.index_lock();
        let still = index
            .get(hash)
            .is_some_and(|e| e.grant == g.id && e.kind == Kind::Refresh);
        if !still {
            // Revoked or replayed while we rotated: do not leave the grant behind.
            drop(index);
            delete(&self.accounts, &entry.slug, &g.id)?;
            return Ok(Err(Refused::Grant));
        }
        Self::unindex(&mut index, &g.id);
        Self::index_grant(&mut index, &entry.slug, &g);
        Ok(Ok(issued))
    }

    /// Ends the grant of an access or refresh token; an unknown token is ignored.
    pub(crate) fn revoke(&self, token: &str) -> anyhow::Result<()> {
        let hash = secret::sha256_hex(token);
        let mut index = self.index_lock();
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

    fn live_grants(&self) -> HashSet<(String, String)> {
        self.index_lock()
            .values()
            .map(|e| (e.slug.clone(), e.grant.clone()))
            .collect()
    }

    pub(crate) fn client_ids(&self) -> HashSet<String> {
        self.live_grants()
            .into_iter()
            .filter_map(|(slug, id)| match read(&self.accounts, &slug, &id) {
                Ok(g) => g,
                Err(_) => {
                    eprintln!(
                        "notedown mcp: skipping unreadable grant file {}",
                        path_of(&self.accounts, &slug, &id).display()
                    );
                    None
                }
            })
            .map(|g| g.client_id)
            .collect()
    }

    /// Deletes the grants whose refresh token expired, and forgets index entries of grants that
    /// are gone (deleted by the CLI). An unreadable file is logged and skipped.
    pub(crate) fn sweep(&self, now_ms: u64) -> anyhow::Result<usize> {
        let mut n = 0;
        for (slug, id) in self.live_grants() {
            match read(&self.accounts, &slug, &id) {
                Ok(Some(g)) if now_ms >= g.refresh_expires_ms => {
                    if delete(&self.accounts, &slug, &id)? {
                        n += 1;
                    }
                    Self::unindex(&mut self.index_lock(), &id);
                }
                Ok(Some(_)) => {}
                Ok(None) => Self::unindex(&mut self.index_lock(), &id),
                Err(_) => eprintln!(
                    "notedown mcp: skipping unreadable grant file {}",
                    path_of(&self.accounts, &slug, &id).display()
                ),
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
            .unwrap()
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
        store
            .issue("anna", "ndc_1", "Claude", "r", 0)
            .unwrap()
            .unwrap();
        assert_eq!(store.sweep(REFRESH_TTL_MS - 1).unwrap(), 0);
        assert_eq!(store.sweep(REFRESH_TTL_MS).unwrap(), 1);
        assert!(store.client_ids().is_empty());
    }

    #[test]
    fn a_grant_deleted_by_hand_is_not_written_back_by_refresh() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        let t = store
            .issue("anna", "ndc_1", "Claude", "r", 0)
            .unwrap()
            .unwrap();
        let oauth = dir.path().join("accounts/anna/oauth");
        for f in std::fs::read_dir(&oauth).unwrap() {
            std::fs::remove_file(f.unwrap().path()).unwrap();
        }
        let r = store.refresh(&t.refresh, "ndc_1", "r", 1).unwrap();
        assert_eq!(r.err(), Some(Refused::Grant));
        assert_eq!(std::fs::read_dir(&oauth).unwrap().count(), 0);
    }

    #[test]
    fn refresh_and_issue_never_recreate_a_removed_account() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = notedown_cloud::auth::Accounts::new(dir.path());
        accounts.create("anna", "a@x", "p").unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        let t = store
            .issue("anna", "ndc_1", "Claude", "r", 0)
            .unwrap()
            .unwrap();
        accounts.remove("anna").unwrap();
        assert_eq!(
            store.refresh(&t.refresh, "ndc_1", "r", 1).unwrap().err(),
            Some(Refused::Grant)
        );
        assert!(
            store
                .issue("anna", "ndc_1", "Claude", "r", 1)
                .unwrap()
                .is_none()
        );
        assert!(!dir.path().join("accounts/anna").exists());
    }

    #[test]
    fn set_password_ends_the_grants() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = notedown_cloud::auth::Accounts::new(dir.path());
        accounts.create("anna", "a@x", "p").unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        let t = store
            .issue("anna", "ndc_1", "Claude", "r", 0)
            .unwrap()
            .unwrap();
        assert!(store.access(&t.access, "r", 1).unwrap().is_some());
        accounts.set_password("anna", "a-new-password").unwrap();
        assert_eq!(store.access(&t.access, "r", 1).unwrap(), None);
    }

    #[test]
    fn a_corrupt_file_stops_nothing_and_can_be_revoked() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        store
            .issue("anna", "ndc_1", "Claude", "r", 0)
            .unwrap()
            .unwrap();
        let oauth = dir.path().join("accounts/anna/oauth");
        std::fs::write(oauth.join(format!("{}.json", "a".repeat(32))), "{ not json").unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        assert_eq!(list_grants(dir.path(), "anna").unwrap().len(), 1);
        assert_eq!(store.sweep(1).unwrap(), 0);
        assert_eq!(store.client_ids().len(), 1);
        assert_eq!(revoke_grants(dir.path(), "anna", None).unwrap(), 2);
        assert_eq!(std::fs::read_dir(&oauth).unwrap().count(), 0);
    }

    #[test]
    fn a_grant_whose_id_differs_from_its_file_name_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        store
            .issue("anna", "ndc_1", "Claude", "r", 0)
            .unwrap()
            .unwrap();
        let oauth = dir.path().join("accounts/anna/oauth");
        let f = std::fs::read_dir(&oauth)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::copy(&f, oauth.join(format!("{}.json", "b".repeat(32)))).unwrap();
        assert_eq!(list_grants(dir.path(), "anna").unwrap().len(), 1);
    }

    #[test]
    fn rotation_refuses_the_old_access_token_and_keeps_the_replay_memory() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        let t1 = store
            .issue("anna", "ndc_1", "Claude", "r", 0)
            .unwrap()
            .unwrap();
        let mut last = t1.refresh.clone();
        for i in 1..=60u64 {
            let t = store.refresh(&last, "ndc_1", "r", i).unwrap().ok().unwrap();
            if i == 1 {
                assert_eq!(store.access(&t1.access, "r", 2).unwrap(), None);
                assert!(store.access(&t.access, "r", 2).unwrap().is_some());
            }
            last = t.refresh;
        }
        // The very first refresh token is still remembered after 60 rotations.
        assert_eq!(
            store.refresh(&t1.refresh, "ndc_1", "r", 61).unwrap().err(),
            Some(Refused::Grant)
        );
        assert_eq!(
            list_grants(dir.path(), "anna").unwrap().len(),
            0,
            "replay ended the grant"
        );
    }

    #[test]
    fn revoke_grants_with_an_id_removes_only_that_grant() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        store.issue("anna", "ndc_1", "A", "r", 0).unwrap().unwrap();
        store.issue("anna", "ndc_2", "B", "r", 1).unwrap().unwrap();
        let list = list_grants(dir.path(), "anna").unwrap();
        assert_eq!(
            revoke_grants(dir.path(), "anna", Some(&list[0].id)).unwrap(),
            1
        );
        let left = list_grants(dir.path(), "anna").unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, list[1].id);
    }

    #[test]
    fn refresh_checks_client_and_resource() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        let t = store.issue("anna", "ndc_1", "A", "r", 0).unwrap().unwrap();
        assert_eq!(
            store.refresh(&t.refresh, "ndc_2", "r", 1).unwrap().err(),
            Some(Refused::Grant)
        );
        assert_eq!(
            store
                .refresh(&t.refresh, "ndc_1", "other", 1)
                .unwrap()
                .err(),
            Some(Refused::Target)
        );
        assert!(store.refresh(&t.refresh, "ndc_1", "r", 1).unwrap().is_ok());
    }

    #[test]
    fn a_replay_that_raced_a_rotation_ends_the_grant() {
        let dir = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(dir.path())
            .create("anna", "a@x", "p")
            .unwrap();
        let store = GrantStore::open(dir.path()).unwrap();
        let t = store
            .issue("anna", "ndc_1", "Claude", "r", 0)
            .unwrap()
            .unwrap();
        // Both requests did their index lookup before the rotation.
        let hash = secret::sha256_hex(&t.refresh);
        let entry = store.index_lock().get(&hash).cloned().unwrap();
        assert!(store.refresh(&t.refresh, "ndc_1", "r", 1).unwrap().is_ok());
        // The second one continues after the lookup with the old token.
        let r = store
            .refresh_locked(&entry, &hash, "ndc_1", "r", 2)
            .unwrap();
        assert_eq!(r.err(), Some(Refused::Grant));
        assert_eq!(
            list_grants(dir.path(), "anna").unwrap().len(),
            0,
            "grant ended"
        );
        assert!(store.index_lock().is_empty());
    }

    #[test]
    fn replaced_entries_expire_and_are_capped() {
        let mut g = Grant {
            id: "x".into(),
            client_id: String::new(),
            client_name: String::new(),
            resource: String::new(),
            scope: String::new(),
            created_ms: 0,
            used_ms: 0,
            access_hash: String::new(),
            access_expires_ms: 0,
            refresh_hash: "cur".into(),
            refresh_expires_ms: 500,
            replaced: vec![
                Replaced {
                    hash: "old".into(),
                    expires_ms: 10,
                },
                Replaced {
                    hash: "live".into(),
                    expires_ms: 100,
                },
            ],
        };
        remember_replaced(&mut g, 50, 3);
        let hashes: Vec<_> = g.replaced.iter().map(|r| r.hash.as_str()).collect();
        assert_eq!(hashes, ["live", "cur"], "the expired entry is dropped");
        g.refresh_hash = "n1".into();
        remember_replaced(&mut g, 50, 3);
        g.refresh_hash = "n2".into();
        remember_replaced(&mut g, 50, 3);
        let hashes: Vec<_> = g.replaced.iter().map(|r| r.hash.as_str()).collect();
        assert_eq!(hashes, ["cur", "n1", "n2"], "the oldest goes at the cap");
        assert_eq!(KEEP_REPLACED, 1000);
    }
}
