//! Server key, tokens and the per-device token generation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::fsutil::{fsync_dir, json_bytes, write_atomic, write_atomic_in};
use crate::{Error, Result, io_err};

pub const KEY_FILE: &str = "token.key";

/// Load the key `<data>/keys/<name>` (hex, at least 32 bytes); generate 32 random bytes with
/// mode 0600 when it is missing.
pub(crate) fn load_or_create_key(data_dir: &Path, name: &str) -> Result<Vec<u8>> {
    let path = data_dir.join("keys").join(name);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let secret = const_hex::decode(text.trim()).map_err(|_| Error::Config {
                path: path.clone(),
                message: "not a hex key".into(),
            })?;
            if secret.len() < 32 {
                return Err(Error::Config {
                    path,
                    message: "key shorter than 32 bytes".into(),
                });
            }
            Ok(secret)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut secret = vec![0u8; 32];
            rand::fill(&mut secret[..]);
            let text = format!("{}\n", const_hex::encode(&secret));
            write_atomic(&path, text.as_bytes(), true)?;
            Ok(secret)
        }
        Err(e) => Err(io_err(&path)(e)),
    }
}

/// The header of every issued token, base64url of `{"alg":"HS256","typ":"JWT"}`.
const HEADER: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";

type HmacSha256 = Hmac<Sha256>;

/// The token header; only `alg` is checked.
#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
}

/// Claims of a device token. Device tokens carry no `exp` (`docs/sync-protocol.md` 2.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claims {
    pub user_id: u64,
    pub equipment_no: String,
    pub generation: u64,
    /// Seconds since the epoch.
    pub create_time: u64,
}

/// The HS256 secret from `keys/token.key`.
#[derive(Clone)]
pub struct TokenKey {
    secret: Vec<u8>,
}

impl std::fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenKey(<masked>)")
    }
}

impl TokenKey {
    pub fn from_bytes(secret: Vec<u8>) -> TokenKey {
        TokenKey { secret }
    }

    /// Load `<data>/keys/token.key` (64 hex characters); generate it with mode 0600 when missing.
    pub fn load_or_create(data_dir: &Path) -> Result<TokenKey> {
        load_or_create_key(data_dir, KEY_FILE).map(|secret| TokenKey { secret })
    }

    /// A stable public id derived from the key (domain-separated hash, the key cannot be
    /// recovered from it): 32 hex characters, the same across restarts.
    pub fn derive_id(&self, label: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"notedown-cloud/");
        h.update(label.as_bytes());
        h.update([0u8]);
        h.update(&self.secret);
        const_hex::encode(&h.finalize()[..16])
    }

    fn mac(&self) -> HmacSha256 {
        // HMAC takes a key of any length.
        <HmacSha256 as KeyInit>::new_from_slice(&self.secret).expect("hmac accepts any key length")
    }

    /// A standard HS256 JWT: header `{"alg":"HS256","typ":"JWT"}`, the claims, base64url without
    /// padding. No `exp`.
    pub fn issue(&self, claims: &Claims) -> Result<String> {
        let payload = serde_json::to_vec(claims).map_err(|e| Error::Invalid {
            what: "token",
            message: e.to_string(),
        })?;
        let signing_input = format!("{HEADER}.{}", URL_SAFE_NO_PAD.encode(payload));
        let mut mac = self.mac();
        mac.update(signing_input.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Ok(format!("{signing_input}.{sig}"))
    }

    /// Checks the signature only (constant time); the generation is checked against the device
    /// record. Any `alg` other than `HS256` and any malformed token are rejected.
    pub fn decode(&self, token: &str) -> Result<Claims> {
        let bad = || Error::Invalid {
            what: "token",
            message: "bad signature or claims".into(),
        };
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| bad())?;
        let mut mac = self.mac();
        mac.update(header.as_bytes());
        mac.update(b".");
        mac.update(payload.as_bytes());
        mac.verify_slice(&sig).map_err(|_| bad())?;
        let header: JwtHeader = URL_SAFE_NO_PAD
            .decode(header)
            .ok()
            .and_then(|h| serde_json::from_slice(&h).ok())
            .ok_or_else(bad)?;
        if header.alg != "HS256" {
            return Err(bad());
        }
        let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| bad())?;
        serde_json::from_slice(&payload).map_err(|_| bad())
    }
}

/// `accounts/<slug>/devices/<serial>.json`. Field order is the file order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub serial: String,
    /// Device name from `bindEquipment`.
    #[serde(default)]
    pub name: Option<String>,
    /// Bound to this account (`bindEquipment`); `unlink` clears it.
    #[serde(default)]
    pub bound: bool,
    /// The `version` the device sent last (login, bind, unlink).
    #[serde(default)]
    pub firmware: Option<String>,
    /// `totalCapacity` from `bindEquipment`, as sent (a string).
    #[serde(default)]
    pub total_capacity: Option<String>,
    pub token_generation: u64,
    /// Milliseconds since the epoch of the last login.
    pub last_login_ms: u64,
}

impl Device {
    fn new(serial: &str) -> Device {
        Device {
            serial: serial.to_string(),
            name: None,
            bound: false,
            firmware: None,
            total_capacity: None,
            token_generation: 0,
            last_login_ms: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Devices {
    data_dir: PathBuf,
    /// Serializes every read-modify-write of device files in this process.
    write_lock: Arc<Mutex<()>>,
}

fn valid_serial(serial: &str) -> bool {
    !serial.is_empty()
        && serial.len() <= 64
        && serial
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Devices {
    pub fn new(data_dir: &Path) -> Devices {
        Devices {
            data_dir: data_dir.to_path_buf(),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Whether `serial` is acceptable as a file name (1 to 64 of letters, digits, `-`, `_`).
    pub fn valid_serial(serial: &str) -> bool {
        valid_serial(serial)
    }

    /// Read, change and write one device record under the write lock; creates it when missing.
    /// Returns the record before (`None` when new) and after the change. [`Error::NoAccount`]
    /// when the account has no `account.json` or is removed meanwhile: only `devices/` is
    /// created, inside the existing account folder, so a removed account never comes back.
    pub fn update(
        &self,
        slug: &str,
        serial: &str,
        change: impl FnOnce(&mut Device),
    ) -> Result<(Option<Device>, Device)> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.file(slug, serial)?;
        let account_dir = self.data_dir.join("accounts").join(slug);
        let no_account = || Error::NoAccount(slug.to_string());
        if !account_dir.join(super::accounts::ACCOUNT_FILE).is_file() {
            return Err(no_account());
        }
        let before = self.get(slug, serial)?;
        let mut device = before.clone().unwrap_or_else(|| Device::new(serial));
        change(&mut device);
        if before.as_ref() == Some(&device) {
            return Ok((before, device));
        }
        let dir = path.parent().unwrap_or(&account_dir);
        match std::fs::create_dir(dir) {
            Ok(()) => fsync_dir(&account_dir)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(no_account()),
            Err(e) => return Err(io_err(dir)(e)),
        }
        match write_atomic_in(&path, &json_bytes(&device), false) {
            Ok(()) => Ok((before, device)),
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(no_account())
            }
            Err(e) => Err(e),
        }
    }

    /// All devices of one account, sorted by serial.
    pub fn list(&self, slug: &str) -> Result<Vec<Device>> {
        super::accounts::validate_slug(slug)?;
        let dir = self.data_dir.join("accounts").join(slug).join("devices");
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&dir)(e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_err(&dir))?;
            let name = entry.file_name();
            let Some(serial) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            if !valid_serial(serial) {
                continue;
            }
            if let Some(d) = self.get(slug, serial)? {
                out.push(d);
            }
        }
        out.sort_by(|a, b| a.serial.cmp(&b.serial));
        Ok(out)
    }

    /// Slugs of the accounts that have a record of this device, sorted.
    pub fn owners(&self, serial: &str) -> Result<Vec<String>> {
        if !valid_serial(serial) {
            return Ok(Vec::new());
        }
        let root = self.data_dir.join("accounts");
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&root)(e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_err(&root))?;
            let Some(slug) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            if super::accounts::validate_slug(&slug).is_ok()
                && entry
                    .path()
                    .join("devices")
                    .join(format!("{serial}.json"))
                    .is_file()
            {
                out.push(slug);
            }
        }
        out.sort();
        Ok(out)
    }

    fn file(&self, slug: &str, serial: &str) -> Result<PathBuf> {
        super::accounts::validate_slug(slug)?;
        if !valid_serial(serial) {
            return Err(Error::Invalid {
                what: "device serial",
                message: "use 1 to 64 of letters, digits, - and _".into(),
            });
        }
        Ok(self
            .data_dir
            .join("accounts")
            .join(slug)
            .join("devices")
            .join(format!("{serial}.json")))
    }

    pub fn get(&self, slug: &str, serial: &str) -> Result<Option<Device>> {
        let path = self.file(slug, serial)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| Error::Config {
                    path,
                    message: e.to_string(),
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    pub fn generation(&self, slug: &str, serial: &str) -> Result<Option<u64>> {
        Ok(self.get(slug, serial)?.map(|d| d.token_generation))
    }

    /// Raise the token generation (new login, logout, unbinding); older tokens stop working.
    /// Creates the record on the first call. Returns the new generation.
    pub fn bump(&self, slug: &str, serial: &str, now_ms: u64) -> Result<u64> {
        self.bump_inner(slug, serial, Some(now_ms))
    }

    /// Like [`bump`](Self::bump) for logout and unbinding: `last_login_ms` stays as it was.
    pub fn invalidate(&self, slug: &str, serial: &str) -> Result<u64> {
        self.bump_inner(slug, serial, None)
    }

    fn bump_inner(&self, slug: &str, serial: &str, login_ms: Option<u64>) -> Result<u64> {
        let (_, device) = self.update(slug, serial, |d| {
            d.token_generation += 1;
            if let Some(ms) = login_ms {
                d.last_login_ms = ms;
            }
        })?;
        Ok(device.token_generation)
    }
}
