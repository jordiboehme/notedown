//! Accounts, login codes, the password check and tokens.

mod accounts;
mod codes;
mod token;

use std::sync::Mutex;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub use accounts::{
    ACCOUNT_FILE, Account, AccountLock, Accounts, AppDataImported, DEFAULT_QUOTA_BYTES, Imported,
    LOCK_COOLDOWN_MS, LOCK_FILE, MAX_FAILED_LOGINS, OAUTH_DIR, PIPELINE_DIR, md5_hex,
    validate_slug,
};
pub use codes::{CODE_LEN, CODE_TTL_MS, LoginCodes, MAX_PENDING_CODES};
pub use token::{Claims, Device, Devices, KEY_FILE, TokenKey};

use crate::Error;

/// `sha256_hex(password_md5 + random_code)`: what the device sends as its password
/// (`docs/sync-protocol.md` 2.3).
pub fn expected_login_hash(password_md5: &str, random_code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password_md5.as_bytes());
    hasher.update(random_code.as_bytes());
    const_hex::encode(hasher.finalize())
}

/// Constant-time comparison of the supplied hash with the expected one.
pub fn check_login_hash(password_md5: &str, random_code: &str, supplied: &str) -> bool {
    let expected = expected_login_hash(password_md5, random_code);
    let supplied = supplied.trim().to_ascii_lowercase();
    expected.as_bytes().ct_eq(supplied.as_bytes()).into()
}

/// Why a login or a token check failed. Messages never contain secrets.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// `E0018`
    #[error("account not found")]
    NoAccount,
    /// `E0019`
    #[error("wrong password")]
    WrongPassword,
    /// `E0045`
    #[error("account locked")]
    Locked,
    /// `E0712`
    #[error("not logged in or login expired")]
    NotLoggedIn,
    #[error(transparent)]
    Internal(#[from] Error),
}

impl AuthError {
    /// The error code of the protocol (`docs/sync-protocol.md` 7.2); `None` for internal errors.
    pub fn code_opt(&self) -> Option<&'static str> {
        match self {
            AuthError::NoAccount => Some("E0018"),
            AuthError::WrongPassword => Some("E0019"),
            AuthError::Locked => Some("E0045"),
            AuthError::NotLoggedIn => Some("E0712"),
            AuthError::Internal(_) => None,
        }
    }

    /// Like [`code_opt`](Self::code_opt); internal errors show as `E0712` so a device never sees
    /// a success.
    pub fn code(&self) -> &'static str {
        self.code_opt().unwrap_or("E0712")
    }
}

#[derive(Debug)]
pub struct LoginOk {
    pub token: String,
    pub account: Account,
    /// The counter before this login reset it (the device shows it as `counts`).
    pub failed_logins_before: u32,
}

#[derive(Debug)]
pub struct Authenticated {
    pub account: Account,
    pub claims: Claims,
}

/// Login and token checks of one data folder.
pub struct Auth {
    accounts: Accounts,
    devices: Devices,
    codes: LoginCodes,
    key: TokenKey,
    /// Serializes logins in this process (codes and counter); the account lock file serializes
    /// them with the CLI.
    login_lock: Mutex<()>,
}

impl Auth {
    /// Opens the data folder's accounts and creates `keys/token.key` on the first start.
    pub fn open(data_dir: &std::path::Path) -> crate::Result<Auth> {
        Ok(Auth {
            accounts: Accounts::new(data_dir),
            devices: Devices::new(data_dir),
            codes: LoginCodes::new(),
            key: TokenKey::load_or_create(data_dir)?,
            login_lock: Mutex::new(()),
        })
    }

    pub fn accounts(&self) -> &Accounts {
        &self.accounts
    }

    pub fn devices(&self) -> &Devices {
        &self.devices
    }

    /// The server instance id the device sees as `uniqueMachineId`: stable across restarts.
    pub fn machine_id(&self) -> String {
        self.key.derive_id("unique-machine-id")
    }

    /// `query/random/code`: issues a code for any email, so the answer does not reveal accounts.
    /// `None` when too many codes are pending.
    pub fn challenge(&self, email: &str, now_ms: u64) -> Option<(String, u64)> {
        self.codes.issue(email, now_ms)
    }

    /// `login/equipment`. A lock older than [`LOCK_COOLDOWN_MS`] is cleared first (counter back
    /// to zero); a lock without a stored time (older file) gets the current time and stays.
    /// Order: unknown account `E0018`, locked `E0045`, then the code is
    /// consumed (single use). A missing, used or expired code gives `E0019` without counting
    /// (no password guess was possible); a wrong hash gives `E0019` and counts, the sixth failure
    /// locks the account. Success resets the counter, raises the device's token generation and
    /// returns a token for it.
    pub fn login(
        &self,
        email: &str,
        password_hash: &str,
        equipment_no: &str,
        now_ms: u64,
    ) -> Result<LoginOk, AuthError> {
        let _guard = self.login_lock.lock().unwrap_or_else(|e| e.into_inner());
        let code = self.codes.consume(email, now_ms);
        let slug = self
            .accounts
            .find_by_email(email)?
            .ok_or(AuthError::NoAccount)?
            .slug;
        // Read the account again under the account lock: a `cloud account passwd` of the CLI
        // in between is not overwritten by the counter update below.
        let _account_lock = lock_account(&self.accounts, &slug)?;
        let mut account = self.accounts.get(&slug)?.ok_or(AuthError::NoAccount)?;
        if account.locked {
            match account.locked_at_ms {
                Some(at) if now_ms.saturating_sub(at) >= LOCK_COOLDOWN_MS => {
                    // Cooldown over: a fresh start, so one more failure does not lock again.
                    account.locked = false;
                    account.locked_at_ms = None;
                    account.failed_logins = 0;
                    self.accounts.save(&account)?;
                }
                Some(_) => return Err(AuthError::Locked),
                None => {
                    // A lock without a time (older file): the cooldown starts now.
                    account.locked_at_ms = Some(now_ms);
                    self.accounts.save(&account)?;
                    return Err(AuthError::Locked);
                }
            }
        }
        let Some(code) = code else {
            return Err(AuthError::WrongPassword);
        };
        if !check_login_hash(&account.password_md5, &code, password_hash) {
            account.failed_logins += 1;
            if account.failed_logins >= MAX_FAILED_LOGINS {
                account.locked = true;
                account.locked_at_ms = Some(now_ms);
            }
            self.accounts.save(&account)?;
            return Err(AuthError::WrongPassword);
        }
        let failed_logins_before = account.failed_logins;
        if failed_logins_before != 0 {
            account.failed_logins = 0;
            self.accounts.save(&account)?;
        }
        let generation = self.devices.bump(&account.slug, equipment_no, now_ms)?;
        let token = self.key.issue(&Claims {
            user_id: account.user_id,
            equipment_no: equipment_no.to_string(),
            generation,
            create_time: now_ms / 1000,
        })?;
        Ok(LoginOk {
            token,
            account,
            failed_logins_before,
        })
    }

    /// Signature, account and generation. Every failure is `NotLoggedIn` (`E0712`).
    pub fn authenticate(&self, token: &str) -> Result<Authenticated, AuthError> {
        let claims = self.key.decode(token).map_err(|_| AuthError::NotLoggedIn)?;
        let account = self
            .accounts
            .find_by_user_id(claims.user_id)?
            .ok_or(AuthError::NotLoggedIn)?;
        match self.devices.generation(&account.slug, &claims.equipment_no) {
            Ok(Some(g)) if g == claims.generation => Ok(Authenticated { account, claims }),
            Ok(_) | Err(Error::Invalid { .. }) => Err(AuthError::NotLoggedIn),
            Err(e) => Err(e.into()),
        }
    }

    /// Logout or unbinding: older tokens of this device stop working.
    pub fn logout(&self, slug: &str, equipment_no: &str) -> crate::Result<()> {
        self.devices.invalidate(slug, equipment_no).map(|_| ())
    }
}

/// [`Accounts::lock`] for a login: an account removed since it was looked up is
/// [`AuthError::NoAccount`] (E0018), not an internal error.
fn lock_account(accounts: &Accounts, slug: &str) -> Result<AccountLock, AuthError> {
    match accounts.lock(slug) {
        Ok(lock) => Ok(lock),
        Err(Error::NoAccount(_)) => Err(AuthError::NoAccount),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vanished_account_at_the_lock_is_no_account() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path());
        assert!(matches!(
            lock_account(&accounts, "anna"),
            Err(AuthError::NoAccount)
        ));
    }
}
