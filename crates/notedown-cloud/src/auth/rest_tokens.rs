//! Per-account tokens of the REST read API.
//!
//! A token is `ndt1.<payload>.<mac>`: `payload` is base64url (no padding) of the JSON
//! `{"slug","userId","id","nonce"}` (`id` 8 lowercase hex, `nonce` 32 random bytes base64url),
//! `mac` is base64url of HMAC-SHA256 of the payload text with the key `keys/rest.key`. The server
//! keeps `accounts/<slug>/tokens/<id>.json` with the SHA-256 of the whole token, never the token.
//! A password change deletes `tokens/`, and the account removal deletes it with the account.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::accounts::{Account, Accounts, validate_slug};
use super::token::load_or_create_key;
use crate::fsutil::{fsync_dir, json_bytes};
use crate::{Error, Result, io_err};

/// `<data>/keys/rest.key`.
pub const REST_KEY_FILE: &str = "rest.key";
/// `accounts/<slug>/tokens`.
pub const TOKENS_DIR: &str = "tokens";
/// The first part of every token.
const PREFIX: &str = "ndt1";
/// A longer text is no token of ours; it is refused before it is decoded.
const MAX_TOKEN_LEN: usize = 1024;
/// A token name is cut to this many characters.
const MAX_NAME_CHARS: usize = 64;
/// "No removal" is believed for this long (ms); a found removal is kept forever.
const NO_REMOVAL_TTL_MS: u64 = 60_000;
/// Journal lines per read of the removal lookup.
const JOURNAL_PAGE: usize = 10_000;
/// New ids tried when the file of an id exists already.
const ID_TRIES: usize = 16;

type HmacSha256 = Hmac<Sha256>;

/// The HMAC key of the REST tokens, `<data>/keys/rest.key`.
#[derive(Clone)]
pub struct RestKey {
    secret: Vec<u8>,
}

impl std::fmt::Debug for RestKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RestKey(<masked>)")
    }
}

impl RestKey {
    /// Load `<data>/keys/rest.key` (64 hex characters); generate it with mode 0600 when missing.
    pub fn load_or_create(data: &Path) -> Result<RestKey> {
        load_or_create_key(data, REST_KEY_FILE).map(|secret| RestKey { secret })
    }

    fn mac(&self) -> HmacSha256 {
        <HmacSha256 as KeyInit>::new_from_slice(&self.secret).expect("hmac accepts any key length")
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac = self.mac();
        mac.update(payload.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    /// The payload of a token with a good MAC (constant-time compare), a valid slug and a
    /// valid id; `None` for anything else. Touches no file.
    fn open(&self, token: &str) -> Option<Payload> {
        if token.len() > MAX_TOKEN_LEN {
            return None;
        }
        let mut parts = token.split('.');
        let (Some(PREFIX), Some(payload), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let mac_bytes = URL_SAFE_NO_PAD.decode(mac).ok()?;
        let mut check = self.mac();
        check.update(payload.as_bytes());
        check.verify_slice(&mac_bytes).ok()?;
        let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let payload: Payload = serde_json::from_slice(&json).ok()?;
        (validate_slug(&payload.slug).is_ok() && valid_id(&payload.id)).then_some(payload)
    }
}

/// What a token carries. Field order is the token's order.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Payload {
    slug: String,
    user_id: u64,
    id: String,
    nonce: String,
}

/// `accounts/<slug>/tokens/<id>.json`. Field order is the file order.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenFile {
    id: String,
    name: String,
    user_id: u64,
    /// Hex SHA-256 of the whole token.
    sha256: String,
    created_ms: u64,
}

/// A stored token as `tokens list` shows it: never the token or its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    pub user_id: u64,
    pub created_ms: u64,
}

impl From<TokenFile> for TokenInfo {
    fn from(f: TokenFile) -> TokenInfo {
        TokenInfo {
            id: f.id,
            name: f.name,
            user_id: f.user_id,
            created_ms: f.created_ms,
        }
    }
}

/// What a token is worth.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // one value per request, never stored in bulk
pub enum TokenCheck {
    /// A token of this live account.
    Valid(Account),
    /// The token's account was removed; `seq` is its last `account.removed` event.
    Removed { user_id: u64, seq: u64 },
    /// Anything else: a bad token, a revoked one, a password change, an unknown user id.
    Refused,
}

/// A cached removal lookup.
#[derive(Debug, Clone, Copy)]
enum Lookup {
    Found(u64),
    NoneAt(u64),
}

/// Checks REST tokens of one data folder.
pub struct RestTokens {
    data: PathBuf,
    accounts: Accounts,
    key: RestKey,
    /// The removal lookup by (slug, user id).
    removals: Mutex<HashMap<(String, u64), Lookup>>,
}

impl RestTokens {
    pub fn new(data: &Path, key: RestKey) -> RestTokens {
        RestTokens {
            data: data.to_path_buf(),
            accounts: Accounts::new(data),
            key,
            removals: Mutex::new(HashMap::new()),
        }
    }

    /// The token check. A bad format, id or MAC is [`TokenCheck::Refused`] before any disk
    /// access. A live account with the token's user id decides alone: [`TokenCheck::Valid`]
    /// when its token file holds the token's hash, else `Refused`, never `Removed`, whatever
    /// the journal holds. Only without such an account the journal is asked for the highest
    /// `account.removed` of that slug and user id. An I/O error or a damaged file is `Err`,
    /// never `Refused`.
    pub fn check(&self, token: &str, now_ms: u64) -> Result<TokenCheck> {
        let Some(payload) = self.key.open(token) else {
            return Ok(TokenCheck::Refused);
        };
        if let Some(account) = self.accounts.get(&payload.slug)?
            && account.user_id == payload.user_id
        {
            // A live account: a removal found before (the account came back from a backup)
            // is stale, so the next removal is looked up again.
            self.removals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(payload.slug.clone(), payload.user_id));
            let path = token_file(&self.accounts.account_dir(&payload.slug), &payload.id);
            let Some(stored) = read_token_file(&path)? else {
                return Ok(TokenCheck::Refused);
            };
            let hash = sha256_hex(token);
            let same: bool = hash.as_bytes().ct_eq(stored.sha256.as_bytes()).into();
            return Ok(
                if same && stored.user_id == payload.user_id && stored.id == payload.id {
                    TokenCheck::Valid(account)
                } else {
                    TokenCheck::Refused
                },
            );
        }
        Ok(
            match self.removal(&payload.slug, payload.user_id, now_ms)? {
                Some(seq) => TokenCheck::Removed {
                    user_id: payload.user_id,
                    seq,
                },
                None => TokenCheck::Refused,
            },
        )
    }

    /// The slug and the id a token carries, when its MAC is good; `None` for anything else.
    /// Touches no file. It says nothing about the token's worth, see [`RestTokens::check`].
    pub(crate) fn slug_and_id(&self, token: &str) -> Option<(String, String)> {
        self.key.open(token).map(|p| (p.slug, p.id))
    }

    /// The highest `seq` of an `account.removed` of `slug` with `user_id`, cached: a found one
    /// forever, none for [`NO_REMOVAL_TTL_MS`].
    fn removal(&self, slug: &str, user_id: u64, now_ms: u64) -> Result<Option<u64>> {
        let key = (slug.to_string(), user_id);
        let cached = self
            .removals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied();
        match cached {
            Some(Lookup::Found(seq)) => return Ok(Some(seq)),
            Some(Lookup::NoneAt(at)) if now_ms.abs_diff(at) < NO_REMOVAL_TTL_MS => return Ok(None),
            _ => {}
        }
        let found = find_removal(&self.data, slug, user_id)?;
        let entry = match found {
            Some(seq) => Lookup::Found(seq),
            None => Lookup::NoneAt(now_ms),
        };
        self.removals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, entry);
        Ok(found)
    }
}

/// Reads the whole journal of `slug`, page by page. A page that asks for more but does not
/// move on is an error: a partial answer must never count (or be cached) as "no removal".
fn find_removal(data: &Path, slug: &str, user_id: u64) -> Result<Option<u64>> {
    let mut from = 0;
    let mut found: Option<u64> = None;
    loop {
        let batch = crate::read::journal(data, slug, from, JOURNAL_PAGE)?;
        for r in batch.removed.iter().filter(|r| r.user_id == user_id) {
            found = Some(found.map_or(r.seq, |f| f.max(r.seq)));
        }
        if !batch.more {
            return Ok(found);
        }
        if batch.next <= from {
            return Err(Error::Io {
                path: data.join(crate::journal::JOURNAL_DIR),
                source: std::io::Error::other("the journal read does not move on"),
            });
        }
        from = batch.next;
    }
}

/// Creates a token for `account` and its file, which holds only the token's hash. The caller
/// holds the account lock. `tokens/` is created with mode 0700, never the account folder.
pub(crate) fn issue(
    accounts: &Accounts,
    key: &RestKey,
    account: &Account,
    name: &str,
    now_ms: u64,
) -> Result<(String, TokenInfo)> {
    let account_dir = accounts.account_dir(&account.slug);
    let dir = account_dir.join(TOKENS_DIR);
    make_private_dir(&account_dir, &dir)?;
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    for _ in 0..ID_TRIES {
        let mut id_bytes = [0u8; 4];
        rand::fill(&mut id_bytes[..]);
        let id = const_hex::encode(id_bytes);
        let mut nonce = [0u8; 32];
        rand::fill(&mut nonce[..]);
        let payload = Payload {
            slug: account.slug.clone(),
            user_id: account.user_id,
            id: id.clone(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
        };
        let json = serde_json::to_vec(&payload).map_err(|e| Error::Invalid {
            what: "token",
            message: e.to_string(),
        })?;
        let payload = URL_SAFE_NO_PAD.encode(json);
        let token = format!("{PREFIX}.{payload}.{}", key.sign(&payload));
        let file = TokenFile {
            id: id.clone(),
            name: name.clone(),
            user_id: account.user_id,
            sha256: sha256_hex(&token),
            created_ms: now_ms,
        };
        let path = token_file(&account_dir, &id);
        match write_new_private(&path, &json_bytes(&file)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => return Err(e),
        }
        fsync_dir(&dir)?;
        return Ok((token, file.into()));
    }
    Err(io_err(&dir)(std::io::Error::other(
        "no free token id, try again",
    )))
}

/// The tokens of an account, sorted by creation time. Needs no key.
pub fn list_tokens(data: &Path, slug: &str) -> Result<Vec<TokenInfo>> {
    let accounts = Accounts::new(data);
    if accounts.get(slug)?.is_none() {
        return Err(Error::NoAccount(slug.to_string()));
    }
    let dir = accounts.account_dir(slug).join(TOKENS_DIR);
    let mut out = Vec::new();
    for id in token_ids(&dir)? {
        if let Some(file) = read_token_file(&token_file(&accounts.account_dir(slug), &id))? {
            out.push(TokenInfo::from(file));
        }
    }
    out.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// Deletes one token; `false` when it did not exist. `id` must be 8 lowercase hex characters.
/// Needs no key.
pub fn revoke_token(data: &Path, slug: &str, id: &str) -> Result<bool> {
    if !valid_id(id) {
        return Err(Error::Invalid {
            what: "token id",
            message: "expected 8 lowercase hex characters".into(),
        });
    }
    let accounts = Accounts::new(data);
    if accounts.get(slug)?.is_none() {
        return Err(Error::NoAccount(slug.to_string()));
    }
    let path = token_file(&accounts.account_dir(slug), id);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            fsync_dir(path.parent().expect("a token file has a folder"))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err(&path)(e)),
    }
}

/// Deletes every token of an account under its lock and returns how many there were. Needs no
/// key.
pub fn revoke_all_tokens(data: &Path, slug: &str) -> Result<usize> {
    let accounts = Accounts::new(data);
    let _lock = accounts.lock(slug)?;
    delete_tokens(&accounts.account_dir(slug))
}

/// Deletes `<account dir>/tokens` and returns the number of tokens in it. The caller holds the
/// account lock. The folder is renamed away first, so every token stops at once, also when the
/// delete fails half-way.
pub(crate) fn delete_tokens(account_dir: &Path) -> Result<usize> {
    let dir = account_dir.join(TOKENS_DIR);
    let count = token_ids(&dir)?.len();
    let mut nonce = [0u8; 8];
    rand::fill(&mut nonce[..]);
    let gone = account_dir.join(format!(
        ".{TOKENS_DIR}-deleted-{}",
        const_hex::encode(nonce)
    ));
    match std::fs::rename(&dir, &gone) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(&dir)(e)),
    }
    fsync_dir(account_dir)?;
    std::fs::remove_dir_all(&gone).map_err(io_err(&gone))?;
    Ok(count)
}

/// The ids of the token files in `dir`; an empty list when it is missing.
fn token_ids(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(dir))?;
        let name = entry.file_name();
        if let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json"))
            && valid_id(id)
        {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}

fn token_file(account_dir: &Path, id: &str) -> PathBuf {
    account_dir.join(TOKENS_DIR).join(format!("{id}.json"))
}

/// `None` when the file does not exist; a file that does not parse is an error.
fn read_token_file(path: &Path) -> Result<Option<TokenFile>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Config {
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path)(e)),
    }
}

/// Creates `dir` (one level below `parent`, which must exist) with mode 0700; an existing
/// `dir` gets mode 0700 too.
fn make_private_dir(parent: &Path, dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let created = match builder.create(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => false,
        Err(e) => return Err(io_err(dir)(e)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(io_err(dir))?;
    }
    if created {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// Writes a new file with mode 0600 (`create_new`) and syncs it; `false` when it exists.
fn write_new_private(path: &Path, bytes: &[u8]) -> Result<bool> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(io_err(path)(e)),
    };
    let mut write = || -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(path);
        return Err(io_err(path)(e));
    }
    Ok(true)
}

fn sha256_hex(token: &str) -> String {
    const_hex::encode(Sha256::digest(token.as_bytes()))
}

/// The slug a token names in its payload, read without the key: `None` when the text is no
/// token of this form. It says nothing about the token's worth (only the server can check the
/// MAC); a client uses it to see whether a token file holds the token of the right account.
pub fn token_slug(token: &str) -> Option<String> {
    token_payload(token).map(|p| p.slug)
}

/// The id a token names in its payload, read without the key (see [`token_slug`]).
pub fn token_id(token: &str) -> Option<String> {
    token_payload(token).map(|p| p.id).filter(|id| valid_id(id))
}

fn token_payload(token: &str) -> Option<Payload> {
    if token.len() > MAX_TOKEN_LEN {
        return None;
    }
    let mut parts = token.split('.');
    let (Some(PREFIX), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let payload: Payload = serde_json::from_slice(&json).ok()?;
    validate_slug(&payload.slug).is_ok().then_some(payload)
}

/// 8 lowercase hex characters.
fn valid_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> RestKey {
        RestKey {
            secret: vec![7u8; 32],
        }
    }

    fn signed(key: &RestKey, json: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(json);
        format!("{PREFIX}.{payload}.{}", key.sign(&payload))
    }

    const GOOD: &str = r#"{"slug":"jordi","userId":1000001,"id":"0123abcd","nonce":"n"}"#;

    #[test]
    fn the_slug_of_a_token_is_read_without_the_key() {
        let token = signed(&key(), GOOD);
        assert_eq!(token_slug(&token).as_deref(), Some("jordi"));
        // Another key: the slug is still read, the worth is the server's to judge.
        let other = RestKey {
            secret: vec![9u8; 32],
        };
        assert_eq!(token_slug(&signed(&other, GOOD)).as_deref(), Some("jordi"));
        assert_eq!(token_slug("ndt1.x.y"), None);
        assert_eq!(token_slug(&token.replacen("ndt1", "ndt2", 1)), None);
        assert_eq!(token_slug(&format!("{token}.z")), None);
        let bad = signed(
            &key(),
            r#"{"slug":"../x","userId":1,"id":"0123abcd","nonce":"n"}"#,
        );
        assert_eq!(token_slug(&bad), None);
    }

    #[test]
    fn a_token_with_a_changed_mac_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = RestTokens::new(dir.path(), key());
        let token = signed(&key(), GOOD);
        assert!(key().open(&token).is_some());
        let (head, mac) = token.rsplit_once('.').unwrap();
        let mut bytes = URL_SAFE_NO_PAD.decode(mac).unwrap();
        bytes[0] ^= 1;
        let changed = format!("{head}.{}", URL_SAFE_NO_PAD.encode(&bytes));
        assert_eq!(tokens.check(&changed, 0).unwrap(), TokenCheck::Refused);
        // Another key, a cut MAC, no MAC, another prefix, a changed payload.
        let other = RestKey {
            secret: vec![8u8; 32],
        };
        assert_eq!(
            tokens.check(&signed(&other, GOOD), 0).unwrap(),
            TokenCheck::Refused
        );
        let cut = &token[..token.len() - 2];
        assert_eq!(tokens.check(cut, 0).unwrap(), TokenCheck::Refused);
        assert_eq!(tokens.check(head, 0).unwrap(), TokenCheck::Refused);
        let renamed = token.replacen(PREFIX, "ndt2", 1);
        assert_eq!(tokens.check(&renamed, 0).unwrap(), TokenCheck::Refused);
        let mac = token.rsplit_once('.').unwrap().1;
        let other_payload = URL_SAFE_NO_PAD.encode(GOOD.replace("1000001", "1000002"));
        let swapped = format!("{PREFIX}.{other_payload}.{mac}");
        assert_eq!(tokens.check(&swapped, 0).unwrap(), TokenCheck::Refused);
    }

    #[test]
    fn a_bad_token_id_is_refused_before_any_disk_access() {
        let dir = tempfile::tempdir().unwrap();
        // `accounts` is a file: any read below it fails with an error, not a refusal.
        std::fs::write(dir.path().join("accounts"), "").unwrap();
        let tokens = RestTokens::new(dir.path(), key());
        assert!(tokens.check(&signed(&key(), GOOD), 0).is_err());
        for bad in [
            GOOD.replace("0123abcd", "0123ABCD"),
            GOOD.replace("0123abcd", "0123abc"),
            GOOD.replace("0123abcd", "../../xy"),
            GOOD.replace("0123abcd", "0123abcde"),
            GOOD.replace("\"jordi\"", "\"../jordi\""),
            GOOD.replace("\"jordi\"", "\"Jordi\""),
            GOOD.replace(",\"nonce\":\"n\"", ""),
            GOOD.replace("}", ",\"more\":1}"),
            "not json".to_string(),
        ] {
            assert_eq!(
                tokens.check(&signed(&key(), &bad), 0).unwrap(),
                TokenCheck::Refused,
                "{bad}"
            );
        }
        let long = format!("{}.{}", signed(&key(), GOOD), "a".repeat(MAX_TOKEN_LEN));
        assert_eq!(tokens.check(&long, 0).unwrap(), TokenCheck::Refused);
    }

    #[test]
    fn ids_are_eight_lowercase_hex() {
        assert!(valid_id("0123abcd"));
        for bad in [
            "",
            "0123abc",
            "0123abcg",
            "0123ABCD",
            "0123abcd0",
            "../x.jso",
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
    }
}
