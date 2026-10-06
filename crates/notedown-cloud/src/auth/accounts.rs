//! `accounts/<slug>/account.json`: one folder per account, written only by the CLI and the server.

use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::fsutil::{json_bytes, write_atomic};
use crate::{Error, Result, io_err};

pub const ACCOUNT_FILE: &str = "account.json";
/// Failed logins after which an account is locked (`E0045`).
pub const MAX_FAILED_LOGINS: u32 = 6;
/// A lock clears by itself after this time (ms); `cloud account passwd` clears it at once.
pub const LOCK_COOLDOWN_MS: u64 = 15 * 60 * 1000;
const NEXT_ID_FILE: &str = ".next_user_id";
const FIRST_USER_ID: u64 = 1_000_001;

/// Field order is the file order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub slug: String,
    pub email: String,
    pub user_id: u64,
    pub user_name: String,
    /// `md5_hex(password)`: the protocol requires it (`docs/sync-protocol.md` 2.3). Never log it.
    pub password_md5: String,
    pub created: String,
    pub locked: bool,
    /// When the lock was set (ms since the epoch); `None` while unlocked. Files written before
    /// this field existed read as `None`.
    #[serde(default)]
    pub locked_at_ms: Option<u64>,
    pub failed_logins: u32,
}

/// `md5_hex(password)`, 32 lowercase hex characters.
pub fn md5_hex(password: &str) -> String {
    const_hex::encode(Md5::digest(password.as_bytes()))
}

/// Slugs are folder names: lowercase letters, digits, `-` and `_`, up to 64 characters.
pub fn validate_slug(slug: &str) -> Result<()> {
    let ok = !slug.is_empty()
        && slug.len() <= 64
        && slug.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid {
            what: "account slug",
            message: "use 1 to 64 of a-z, 0-9, - and _, starting with a letter or digit".into(),
        })
    }
}

fn validate_email(email: &str) -> Result<()> {
    let ok = !email.is_empty()
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.contains('@');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid {
            what: "email",
            message: "expected an address like name@example.org".into(),
        })
    }
}

fn validate_password(password: &str) -> Result<()> {
    if password.is_empty() {
        return Err(Error::Invalid {
            what: "password",
            message: "must not be empty".into(),
        });
    }
    Ok(())
}

/// The accounts of one data folder.
#[derive(Debug, Clone)]
pub struct Accounts {
    root: PathBuf,
}

impl Accounts {
    /// `data_dir` is the data folder; accounts live in `<data>/accounts`.
    pub fn new(data_dir: &Path) -> Accounts {
        Accounts {
            root: data_dir.join("accounts"),
        }
    }

    pub fn account_dir(&self, slug: &str) -> PathBuf {
        self.root.join(slug)
    }

    fn file(&self, slug: &str) -> PathBuf {
        self.account_dir(slug).join(ACCOUNT_FILE)
    }

    fn next_user_id(&self) -> Result<u64> {
        let path = self.root.join(NEXT_ID_FILE);
        let from_file = match std::fs::read_to_string(&path) {
            Ok(t) => t.trim().parse::<u64>().ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_err(&path)(e)),
        };
        let max_used = self.list()?.iter().map(|a| a.user_id + 1).max();
        let id = from_file
            .into_iter()
            .chain(max_used)
            .max()
            .unwrap_or(FIRST_USER_ID)
            .max(FIRST_USER_ID);
        write_atomic(&path, format!("{}\n", id + 1).as_bytes(), false)?;
        Ok(id)
    }

    /// Create an account. The stored password is `md5_hex(password)`. User ids are never reused.
    pub fn create(&self, slug: &str, email: &str, password: &str) -> Result<Account> {
        validate_slug(slug)?;
        validate_email(email)?;
        validate_password(password)?;
        if self.get(slug)?.is_some() || self.find_by_email(email)?.is_some() {
            return Err(Error::AccountExists(slug.to_string()));
        }
        let account = Account {
            slug: slug.to_string(),
            email: email.to_string(),
            user_id: self.next_user_id()?,
            user_name: slug.to_string(),
            password_md5: md5_hex(password),
            created: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .expect("rfc3339 formatting"),
            locked: false,
            locked_at_ms: None,
            failed_logins: 0,
        };
        self.save(&account)?;
        Ok(account)
    }

    /// Atomic write with mode 0600.
    pub fn save(&self, account: &Account) -> Result<()> {
        validate_slug(&account.slug)?;
        write_atomic(&self.file(&account.slug), &json_bytes(account), true)
    }

    pub fn get(&self, slug: &str) -> Result<Option<Account>> {
        validate_slug(slug)?;
        let path = self.file(slug);
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

    /// Sets a new password and clears the lock and the failed-login counter.
    pub fn set_password(&self, slug: &str, password: &str) -> Result<()> {
        validate_password(password)?;
        let mut account = self
            .get(slug)?
            .ok_or_else(|| Error::NoAccount(slug.to_string()))?;
        account.password_md5 = md5_hex(password);
        account.locked = false;
        account.locked_at_ms = None;
        account.failed_logins = 0;
        self.save(&account)
    }

    /// All accounts, sorted by slug. Folders without an `account.json` are skipped.
    pub fn list(&self) -> Result<Vec<Account>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&self.root)(e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_err(&self.root))?;
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            if validate_slug(&name).is_err() {
                continue;
            }
            if let Some(a) = self.get(&name)? {
                out.push(a);
            }
        }
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(out)
    }

    /// Case-insensitive match on the email.
    pub fn find_by_email(&self, email: &str) -> Result<Option<Account>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|a| a.email.eq_ignore_ascii_case(email)))
    }

    pub fn find_by_user_id(&self, user_id: u64) -> Result<Option<Account>> {
        Ok(self.list()?.into_iter().find(|a| a.user_id == user_id))
    }

    /// Delete the account folder with everything in it (files, digests, devices).
    pub fn remove(&self, slug: &str) -> Result<()> {
        validate_slug(slug)?;
        if self.get(slug)?.is_none() {
            return Err(Error::NoAccount(slug.to_string()));
        }
        let dir = self.account_dir(slug);
        std::fs::remove_dir_all(&dir).map_err(io_err(&dir))
    }
}
