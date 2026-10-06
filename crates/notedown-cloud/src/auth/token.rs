//! Server key, tokens and the per-device token generation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::fsutil::{json_bytes, write_atomic};
use crate::{Error, Result, io_err};

pub const KEY_FILE: &str = "token.key";

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
        let path = data_dir.join("keys").join(KEY_FILE);
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
                Ok(TokenKey { secret })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut secret = vec![0u8; 32];
                rand::fill(&mut secret[..]);
                let text = format!("{}\n", const_hex::encode(&secret));
                write_atomic(&path, text.as_bytes(), true)?;
                Ok(TokenKey { secret })
            }
            Err(e) => Err(io_err(&path)(e)),
        }
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

    pub fn issue(&self, claims: &Claims) -> Result<String> {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(&self.secret),
        )
        .map_err(|e| Error::Invalid {
            what: "token",
            message: e.to_string(),
        })
    }

    /// Checks the signature only; the generation is checked against the device record.
    pub fn decode(&self, token: &str) -> Result<Claims> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.required_spec_claims.clear();
        validation.validate_exp = false;
        jsonwebtoken::decode::<Claims>(token, &DecodingKey::from_secret(&self.secret), &validation)
            .map(|d| d.claims)
            .map_err(|_| Error::Invalid {
                what: "token",
                message: "bad signature or claims".into(),
            })
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
    /// Returns the record before (`None` when new) and after the change.
    pub fn update(
        &self,
        slug: &str,
        serial: &str,
        change: impl FnOnce(&mut Device),
    ) -> Result<(Option<Device>, Device)> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.file(slug, serial)?;
        let before = self.get(slug, serial)?;
        let mut device = before.clone().unwrap_or_else(|| Device::new(serial));
        change(&mut device);
        if before.as_ref() != Some(&device) {
            write_atomic(&path, &json_bytes(&device), false)?;
        }
        Ok((before, device))
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
