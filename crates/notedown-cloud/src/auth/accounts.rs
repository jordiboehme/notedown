//! `accounts/<slug>/account.json`: one folder per account, written only by the CLI and the server.

use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::fsutil::{is_same_file, json_bytes, rename_durable, write_atomic};
use crate::{Error, Result, io_err};

/// The folder of an account's OAuth grants (`notedown-mcp-http`). A new password deletes it.
pub const OAUTH_DIR: &str = "oauth";

pub const ACCOUNT_FILE: &str = "account.json";
/// `accounts/<slug>/.lock`: an advisory lock that the CLI and the server take around every
/// read-modify-write of `account.json`, so neither can overwrite a change of the other.
pub const LOCK_FILE: &str = ".lock";
/// Failed logins after which an account is locked (`E0045`).
pub const MAX_FAILED_LOGINS: u32 = 6;
/// A lock clears by itself after this time (ms); `cloud account passwd` clears it at once.
pub const LOCK_COOLDOWN_MS: u64 = 15 * 60 * 1000;
const NEXT_ID_FILE: &str = ".next_user_id";
/// The name start of a removed account folder before it is deleted (`.removed-<slug>-<hex>`).
const TOMBSTONE_PREFIX: &str = ".removed-";
/// How often [`Accounts::lock`] takes the lock again when its `.lock` file was replaced.
const LOCK_TRIES: usize = 5;
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
    /// Set by `notedown cloud import` when a device backup was imported and verified. In accept
    /// mode with `protect = true`, `synchronous/start` answers `E0079` until it is set. Absent in
    /// files written before it existed and in accounts without an import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<Imported>,
    /// Set by `notedown cloud import-app-data` when the digests of a device backup were
    /// imported. With `protect = true` every digest and to-do route answers `E0079` until it is
    /// set. Absent in files written before it existed and in accounts without that import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_data_imported: Option<AppDataImported>,
    /// When the lock was set (ms since the epoch); `None` while unlocked. Files written before
    /// this field existed read as `None`.
    #[serde(default)]
    pub locked_at_ms: Option<u64>,
    pub failed_logins: u32,
    /// Storage quota in bytes (`totalCapacity`); absent means the default
    /// ([`DEFAULT_QUOTA_BYTES`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_bytes: Option<u64>,
}

/// The default storage quota when `account.json` has no `quota_bytes`: 64 GiB.
pub const DEFAULT_QUOTA_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// The import marker in `account.json`. Field order is the file order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Imported {
    /// When the import finished, RFC 3339 (like `created`).
    pub at: String,
    /// The imported backup folder, as given on the command line.
    pub source: String,
    /// Number of imported files.
    pub files: u64,
    /// Total bytes of the imported files.
    pub bytes: u64,
}

/// The app data import marker in `account.json`. Field order is the file order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppDataImported {
    /// When the import finished, RFC 3339 (like `created`).
    pub at: String,
    /// The imported backup (`.snbak`), as given on the command line; empty for an import of
    /// nothing (`--empty`).
    pub source: String,
    /// Number of imported digests.
    pub digests: u64,
    /// Number of imported digest groups.
    pub groups: u64,
    /// Number of imported handwritten comments (`.mark` files).
    pub marks: u64,
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

/// The held account lock; dropping it closes the file and releases the lock.
#[derive(Debug)]
pub struct AccountLock {
    _file: std::fs::File,
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
        if self.get(slug)?.is_some() {
            return Err(Error::AccountExists(slug.to_string()));
        }
        if let Some(other) = self.find_by_email(email)? {
            return Err(Error::EmailInUse {
                email: email.to_string(),
                slug: other.slug,
            });
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
            imported: None,
            app_data_imported: None,
            locked_at_ms: None,
            failed_logins: 0,
            quota_bytes: None,
        };
        let _lock = self.lock_for_create(slug)?;
        if self.get(slug)?.is_some() {
            return Err(Error::AccountExists(slug.to_string()));
        }
        // A folder without `account.json` is reused. OAuth grants left in it belong to a
        // removed account: delete them, so its tokens never work for the new one.
        let oauth = self.account_dir(slug).join(OAUTH_DIR);
        match std::fs::remove_dir_all(&oauth) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&oauth)(e)),
        }
        self.save(&account)?;
        Ok(account)
    }

    /// Take the account lock (`accounts/<slug>/.lock`, exclusive, blocking). It is held until
    /// the guard is dropped. Every read-modify-write of `account.json` runs under it, in the CLI
    /// and in the server. [`Error::NoAccount`] when the account has no `account.json` or was
    /// removed while this call waited; nothing is created then.
    pub fn lock(&self, slug: &str) -> Result<AccountLock> {
        self.lock_with(slug, false)
    }

    /// [`Accounts::lock`] for [`Accounts::create`]: it creates the account folder and does not
    /// need an `account.json`.
    fn lock_for_create(&self, slug: &str) -> Result<AccountLock> {
        self.lock_with(slug, true)
    }

    fn lock_with(&self, slug: &str, create: bool) -> Result<AccountLock> {
        validate_slug(slug)?;
        let dir = self.account_dir(slug);
        let path = dir.join(LOCK_FILE);
        let no_account = || Error::NoAccount(slug.to_string());
        for _ in 0..LOCK_TRIES {
            if create {
                std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
            } else if !self.file(slug).is_file() {
                return Err(no_account());
            }
            let file = match std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
            {
                Ok(f) => f,
                // The folder is gone: removed before or while we got here.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => {
                    return Err(no_account());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err(&path)(e)),
            };
            file.lock().map_err(io_err(&path))?;
            // `remove` deletes the folder, `.lock` too, while it holds the lock. A waiter then
            // holds a lock on an unlinked file that excludes nobody: drop it and look again.
            match is_same_file(&file, &path).map_err(io_err(&path))? {
                Some(true) => {}
                Some(false) => continue,
                None if create => continue,
                None => return Err(no_account()),
            }
            if !create && !self.file(slug).is_file() {
                return Err(no_account());
            }
            return Ok(AccountLock { _file: file });
        }
        Err(io_err(&path)(std::io::Error::other(
            "the lock file keeps changing, try again",
        )))
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
        if self.get(slug)?.is_none() {
            return Err(Error::NoAccount(slug.to_string()));
        }
        let _lock = self.lock(slug)?;
        let mut account = self
            .get(slug)?
            .ok_or_else(|| Error::NoAccount(slug.to_string()))?;
        account.password_md5 = md5_hex(password);
        account.locked = false;
        account.locked_at_ms = None;
        account.failed_logins = 0;
        self.save(&account)?;
        // A new password ends every OAuth sign-in of the account.
        let oauth = self.account_dir(slug).join(OAUTH_DIR);
        match std::fs::remove_dir_all(&oauth) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&oauth)(e)),
        }
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

    /// Delete the account folder with everything in it (files, digests, devices), under the
    /// store lock and the account lock.
    pub fn remove(&self, slug: &str) -> Result<()> {
        validate_slug(slug)?;
        self.sweep_tombstones();
        if self.get(slug)?.is_none() {
            return Err(Error::NoAccount(slug.to_string()));
        }
        let dir = self.account_dir(slug);
        // Wait for a running change of the account to finish: the store lock first (a commit
        // or an import of the server or another command), then the account lock, as the
        // global lock order says. The raw lock works on a damaged store too.
        let _store = crate::store::take_store_lock(&dir, Some(slug))?;
        let _lock = self.lock(slug)?;
        self.remove_locked(slug)
    }

    /// The part of [`Accounts::remove`] under both locks. The folder is first renamed to a
    /// tombstone (`.removed-<slug>-<hex>`), so the account is gone at once and as a whole:
    /// every waiter on a lock of it finds its path gone and gets [`Error::NoAccount`], and
    /// nothing can recreate a part of it. Then the tombstone is deleted. A tombstone left by a
    /// crash or a failed delete is swept by the next `remove` and at server start.
    fn remove_locked(&self, slug: &str) -> Result<()> {
        let dir = self.account_dir(slug);
        let mut raw = [0u8; 8];
        rand::fill(&mut raw[..]);
        let tomb = self.root.join(format!(
            "{TOMBSTONE_PREFIX}{slug}-{}",
            const_hex::encode(raw)
        ));
        rename_durable(&dir, &tomb)?;
        if let Err(e) = std::fs::remove_dir_all(&tomb) {
            eprintln!(
                "notedown cloud: account {slug} is removed, but {} could not be deleted: {e}; \
                 it is deleted at the next start",
                tomb.display()
            );
        }
        Ok(())
    }

    /// Delete the tombstones of removed accounts (`accounts/.removed-*`) that a crash or a
    /// failed delete left. Errors are only logged. Returns how many were deleted.
    pub fn sweep_tombstones(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        let mut n = 0;
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(TOMBSTONE_PREFIX))
            {
                continue;
            }
            let path = entry.path();
            match std::fs::remove_dir_all(&path) {
                Ok(()) => n += 1,
                Err(e) => eprintln!(
                    "notedown cloud: cannot delete the removed account folder {}: {e}",
                    path.display()
                ),
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn accounts() -> (tempfile::TempDir, Accounts) {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path());
        accounts.create("anna", "anna@example.org", "pw").unwrap();
        (dir, accounts)
    }

    #[test]
    fn lock_on_a_removed_account_is_no_account_and_creates_nothing() {
        let (_dir, accounts) = accounts();
        accounts.remove("anna").unwrap();
        assert!(matches!(accounts.lock("anna"), Err(Error::NoAccount(s)) if s == "anna"));
        assert!(!accounts.account_dir("anna").exists());
    }

    #[test]
    fn lock_on_a_folder_without_account_json_is_no_account_and_creates_no_lock_file() {
        let (_dir, accounts) = accounts();
        let dir = accounts.account_dir("anna");
        std::fs::remove_file(dir.join(LOCK_FILE)).ok();
        std::fs::remove_file(dir.join(ACCOUNT_FILE)).unwrap();
        assert!(matches!(accounts.lock("anna"), Err(Error::NoAccount(_))));
        assert!(!dir.join(LOCK_FILE).exists());
    }

    #[test]
    fn a_waiter_on_an_account_removed_meanwhile_gets_no_account() {
        let (_dir, accounts) = accounts();
        let held = accounts.lock("anna").unwrap();
        let waiter = {
            let accounts = accounts.clone();
            std::thread::spawn(move || accounts.lock("anna").map(|_| ()))
        };
        // Let the waiter open the lock file and block on it.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished());
        std::fs::remove_dir_all(accounts.account_dir("anna")).unwrap();
        drop(held);
        let got = waiter.join().unwrap();
        assert!(matches!(got, Err(Error::NoAccount(_))), "{got:?}");
        assert!(!accounts.account_dir("anna").exists());
    }

    #[test]
    fn a_waiter_never_holds_a_lock_file_that_was_replaced() {
        let (_dir, accounts) = accounts();
        let path = accounts.account_dir("anna").join(LOCK_FILE);
        let held = accounts.lock("anna").unwrap();
        let waiter = {
            let accounts = accounts.clone();
            std::thread::spawn(move || accounts.lock("anna"))
        };
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished());
        // The waiter blocks on the old inode; a new `.lock` takes its place.
        std::fs::remove_file(&path).unwrap();
        std::fs::File::create(&path).unwrap();
        drop(held);
        let got = waiter.join().unwrap().unwrap();
        let other = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(
            matches!(other.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "the waiter holds a lock on an unlinked file"
        );
        drop(got);
        other.try_lock().unwrap();
    }

    #[test]
    fn create_over_a_leftover_folder_drops_its_oauth_grants() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path());
        let oauth = accounts.account_dir("anna").join(OAUTH_DIR);
        std::fs::create_dir_all(&oauth).unwrap();
        std::fs::write(oauth.join("0123.json"), "{}").unwrap();
        accounts.create("anna", "anna@example.org", "pw").unwrap();
        assert!(!oauth.exists());
        assert!(accounts.get("anna").unwrap().is_some());
        // The account lock works for the new account.
        drop(accounts.lock("anna").unwrap());
    }

    fn tombstones(accounts: &Accounts) -> Vec<String> {
        std::fs::read_dir(&accounts.root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(TOMBSTONE_PREFIX))
            .collect()
    }

    #[test]
    fn remove_with_a_waiter_in_lock_gives_no_account_and_leaves_no_folder() {
        let (_dir, accounts) = accounts();
        let dir = accounts.account_dir("anna");
        let _store = crate::store::take_store_lock(&dir, None).unwrap();
        let held = accounts.lock("anna").unwrap();
        let waiter = {
            let accounts = accounts.clone();
            std::thread::spawn(move || accounts.lock("anna").map(|_| ()))
        };
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished());
        accounts.remove_locked("anna").unwrap();
        drop(held);
        let got = waiter.join().unwrap();
        assert!(matches!(got, Err(Error::NoAccount(_))), "{got:?}");
        assert!(!dir.exists());
        assert!(tombstones(&accounts).is_empty());
        assert!(accounts.get("anna").unwrap().is_none());
    }

    #[test]
    fn a_store_lock_waiter_never_recreates_a_removed_account() {
        let (_dir, accounts) = accounts();
        let data = accounts.root.parent().unwrap().to_path_buf();
        let dir = accounts.account_dir("anna");
        let store = crate::store::Store::open(&data, "anna").unwrap();
        let held = crate::store::take_store_lock(&dir, None).unwrap();
        let waiter = std::thread::spawn(move || store.lock().map(|_| ()));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished());
        let tomb = accounts.root.join(format!("{TOMBSTONE_PREFIX}anna-test"));
        std::fs::rename(&dir, &tomb).unwrap();
        std::fs::remove_dir_all(&tomb).unwrap();
        drop(held);
        let got = waiter.join().unwrap();
        assert!(matches!(got, Err(Error::NoAccount(_))), "{got:?}");
        assert!(!dir.exists(), "the waiter recreated the account folder");
    }

    #[test]
    fn a_leftover_tombstone_is_swept_by_remove_and_by_the_sweep() {
        let (_dir, accounts) = accounts();
        accounts.create("bert", "bert@example.org", "pw").unwrap();
        let tomb = accounts.root.join(format!("{TOMBSTONE_PREFIX}carl-0123"));
        std::fs::create_dir_all(tomb.join("devices")).unwrap();
        std::fs::write(tomb.join("devices/SN1.json"), "{}").unwrap();
        accounts.remove("bert").unwrap();
        assert!(tombstones(&accounts).is_empty());

        std::fs::create_dir_all(tomb.join("files")).unwrap();
        assert_eq!(accounts.sweep_tombstones(), 1);
        assert!(!tomb.exists());
        assert_eq!(accounts.sweep_tombstones(), 0);
        assert!(accounts.get("anna").unwrap().is_some());
    }

    #[test]
    fn a_tombstone_never_counts_as_an_owner_or_for_the_staging_sweep() {
        let (dir, accounts) = accounts();
        let tomb = accounts.root.join(format!("{TOMBSTONE_PREFIX}carl-0123"));
        std::fs::create_dir_all(tomb.join("devices")).unwrap();
        std::fs::write(tomb.join("devices/SN1.json"), "{}").unwrap();
        let devices = crate::auth::Devices::new(dir.path());
        assert!(devices.owners("SN1").unwrap().is_empty());
        let staged = tomb.join(crate::sync::staging::STAGING_DIR).join("u1");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("part"), "x").unwrap();
        assert_eq!(
            crate::sync::staging::sweep(dir.path(), u64::MAX / 2).unwrap(),
            0
        );
        assert!(staged.exists());
    }

    #[test]
    fn the_server_start_sweeps_tombstones() {
        let (dir, accounts) = accounts();
        let tomb = accounts.root.join(format!("{TOMBSTONE_PREFIX}carl-0123"));
        std::fs::create_dir_all(tomb.join("files")).unwrap();
        crate::server::AppState::open(crate::Config::default(), dir.path()).unwrap();
        assert!(!tomb.exists());
        assert!(accounts.get("anna").unwrap().is_some());
    }
}
