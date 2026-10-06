use notedown_cloud::Error;
use notedown_cloud::auth::{
    Accounts, Auth, AuthError, Claims, Devices, LoginCodes, TokenKey, check_login_hash,
    expected_login_hash, md5_hex,
};

const NOW: u64 = 1_791_270_000_000;

fn data() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// accounts

#[test]
fn create_list_find_remove() {
    let d = data();
    let accounts = Accounts::new(d.path());
    assert!(accounts.list().unwrap().is_empty());
    let a = accounts
        .create("jordi", "Jordi@Example.org", "secret")
        .unwrap();
    let b = accounts.create("anna", "anna@example.org", "pw").unwrap();
    assert_ne!(a.user_id, b.user_id);
    let slugs: Vec<_> = accounts
        .list()
        .unwrap()
        .into_iter()
        .map(|a| a.slug)
        .collect();
    assert_eq!(slugs, ["anna", "jordi"]);
    assert_eq!(
        accounts
            .find_by_email("jordi@example.ORG")
            .unwrap()
            .unwrap()
            .slug,
        "jordi"
    );
    assert!(
        accounts
            .find_by_email("nobody@example.org")
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        accounts.create("jordi", "x@example.org", "pw"),
        Err(Error::AccountExists(_))
    ));
    assert!(matches!(
        accounts.create("other", "anna@example.org", "pw"),
        Err(Error::AccountExists(_))
    ));
    accounts.remove("anna").unwrap();
    assert!(!d.path().join("accounts/anna").exists());
    assert!(matches!(accounts.remove("anna"), Err(Error::NoAccount(_))));
    // ids are never reused
    let c = accounts.create("carl", "carl@example.org", "pw").unwrap();
    assert!(c.user_id > b.user_id);
}

#[test]
fn slug_is_validated_against_path_tricks() {
    let d = data();
    let accounts = Accounts::new(d.path());
    for bad in ["", "../x", "A", "a/b", ".hidden", "a b"] {
        assert!(
            accounts.create(bad, "x@example.org", "pw").is_err(),
            "{bad}"
        );
    }
    assert!(accounts.create("ok", "no-at-sign", "pw").is_err());
    assert!(accounts.create("ok", "x@example.org", "").is_err());
}

#[test]
fn password_is_stored_as_md5_and_file_is_private_and_stable() {
    let d = data();
    let accounts = Accounts::new(d.path());
    accounts
        .create("jordi", "jordi@example.org", "secret")
        .unwrap();
    assert_eq!(md5_hex("secret"), "5ebe2294ecd0e0f08eab7690d2a6ee69");
    let path = d.path().join("accounts/jordi/account.json");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"password_md5\": \"5ebe2294ecd0e0f08eab7690d2a6ee69\""));
    assert!(!text.contains("secret"));
    assert!(text.ends_with("}\n"));
    let keys: Vec<_> = text
        .lines()
        .filter_map(|l| l.strip_prefix("  \""))
        .map(|l| l.split('"').next().unwrap())
        .collect();
    assert_eq!(
        keys,
        [
            "slug",
            "email",
            "user_id",
            "user_name",
            "password_md5",
            "created",
            "locked",
            "locked_at_ms",
            "failed_logins"
        ]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let again = accounts.get("jordi").unwrap().unwrap();
    assert_eq!(
        again.user_id,
        accounts
            .find_by_email("jordi@example.org")
            .unwrap()
            .unwrap()
            .user_id
    );
}

#[test]
fn set_password_replaces_hash_and_unlocks() {
    let d = data();
    let accounts = Accounts::new(d.path());
    let mut a = accounts
        .create("jordi", "jordi@example.org", "old")
        .unwrap();
    a.locked = true;
    a.failed_logins = 6;
    accounts.save(&a).unwrap();
    accounts.set_password("jordi", "new").unwrap();
    let a = accounts.get("jordi").unwrap().unwrap();
    assert_eq!(a.password_md5, md5_hex("new"));
    assert!(!a.locked);
    assert_eq!(a.failed_logins, 0);
    assert!(matches!(
        accounts.set_password("ghost", "x"),
        Err(Error::NoAccount(_))
    ));
}

// codes

#[test]
fn code_is_8_alphanumeric_chars_single_use() {
    let codes = LoginCodes::new();
    let (code, ts) = codes.issue("Jordi@example.org", NOW).unwrap();
    assert_eq!(code.len(), 8);
    assert!(code.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_eq!(ts, NOW);
    assert_eq!(codes.consume("jordi@example.org", NOW + 1000), Some(code));
    assert_eq!(codes.consume("jordi@example.org", NOW + 1000), None);
}

#[test]
fn code_expires_after_five_minutes() {
    let codes = LoginCodes::new();
    let (code, _) = codes.issue("a@example.org", NOW).unwrap();
    assert_eq!(codes.consume("a@example.org", NOW + 300_001), None);
    let (code2, _) = codes.issue("a@example.org", NOW).unwrap();
    assert_eq!(codes.consume("a@example.org", NOW + 300_000), Some(code2));
    let _ = code;
}

#[test]
fn new_challenge_replaces_the_old_code() {
    let codes = LoginCodes::new();
    let (first, _) = codes.issue("a@example.org", NOW).unwrap();
    let (second, _) = codes.issue("a@example.org", NOW + 1).unwrap();
    // astronomically unlikely to collide; consume yields the newest
    let got = codes.consume("a@example.org", NOW + 2).unwrap();
    assert_eq!(got, second);
    let _ = first;
}

// password check

#[test]
fn hash_known_vector() {
    // sha256_hex(md5("secret") + "a1B2c3D4")
    let md5 = md5_hex("secret");
    let expected = expected_login_hash(&md5, "a1B2c3D4");
    assert_eq!(expected.len(), 64);
    let again = {
        use sha2::{Digest, Sha256};
        const_hex::encode(Sha256::digest(b"5ebe2294ecd0e0f08eab7690d2a6ee69a1B2c3D4"))
    };
    assert_eq!(expected, again);
    assert!(check_login_hash(&md5, "a1B2c3D4", &expected));
    assert!(check_login_hash(&md5, "a1B2c3D4", &expected.to_uppercase()));
    assert!(!check_login_hash(&md5, "a1B2c3D5", &expected));
    assert!(!check_login_hash(&md5, "a1B2c3D4", "short"));
    assert!(!check_login_hash(&md5, "a1B2c3D4", ""));
}

// Auth: login, lockout

fn setup() -> (tempfile::TempDir, Auth) {
    let d = data();
    Accounts::new(d.path())
        .create("jordi", "jordi@example.org", "secret")
        .unwrap();
    let auth = Auth::open(d.path()).unwrap();
    (d, auth)
}

fn hash_for(code: &str, password: &str) -> String {
    expected_login_hash(&md5_hex(password), code)
}

#[test]
fn login_ok_returns_token_and_resets_counter() {
    let (_d, auth) = setup();
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    // one earlier failure
    let (bad, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let err = auth
        .login("jordi@example.org", &hash_for(&bad, "wrong"), "SN1", NOW)
        .unwrap_err();
    assert_eq!(err.code(), "E0019");
    assert_eq!(
        auth.accounts().get("jordi").unwrap().unwrap().failed_logins,
        1
    );
    let _ = code;
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let ok = auth
        .login("jordi@example.org", &hash_for(&code, "secret"), "SN1", NOW)
        .unwrap();
    assert_eq!(ok.account.slug, "jordi");
    assert_eq!(ok.failed_logins_before, 1);
    assert_eq!(
        auth.accounts().get("jordi").unwrap().unwrap().failed_logins,
        0
    );
    let who = auth.authenticate(&ok.token).unwrap();
    assert_eq!(who.account.slug, "jordi");
    assert_eq!(who.claims.equipment_no, "SN1");
}

#[test]
fn unknown_account_is_e0018() {
    let (_d, auth) = setup();
    let (code, _) = auth.challenge("ghost@example.org", NOW).unwrap();
    let err = auth
        .login("ghost@example.org", &hash_for(&code, "secret"), "SN1", NOW)
        .unwrap_err();
    assert_eq!(err.code(), "E0018");
}

#[test]
fn reused_or_missing_code_fails_with_e0019_and_does_not_count() {
    let (_d, auth) = setup();
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let h = hash_for(&code, "secret");
    auth.login("jordi@example.org", &h, "SN1", NOW).unwrap();
    let err = auth.login("jordi@example.org", &h, "SN1", NOW).unwrap_err();
    assert_eq!(err.code(), "E0019");
    let err = auth
        .login("jordi@example.org", &h, "SN1", NOW + 400_000)
        .unwrap_err();
    assert_eq!(err.code(), "E0019");
    assert_eq!(
        auth.accounts().get("jordi").unwrap().unwrap().failed_logins,
        0
    );
}

#[test]
fn expired_code_is_refused() {
    let (_d, auth) = setup();
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let err = auth
        .login(
            "jordi@example.org",
            &hash_for(&code, "secret"),
            "SN1",
            NOW + 300_001,
        )
        .unwrap_err();
    assert_eq!(err.code(), "E0019");
}

#[test]
fn six_failures_lock_the_account() {
    let (_d, auth) = setup();
    for i in 1..=6 {
        let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
        let err = auth
            .login("jordi@example.org", &hash_for(&code, "bad"), "SN1", NOW)
            .unwrap_err();
        assert_eq!(err.code(), "E0019", "attempt {i}");
    }
    let a = auth.accounts().get("jordi").unwrap().unwrap();
    assert!(a.locked);
    assert_eq!(a.failed_logins, 6);
    // even the right password is refused now
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let err = auth
        .login("jordi@example.org", &hash_for(&code, "secret"), "SN1", NOW)
        .unwrap_err();
    assert_eq!(err.code(), "E0045");
    // passwd unlocks
    auth.accounts().set_password("jordi", "secret").unwrap();
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    auth.login("jordi@example.org", &hash_for(&code, "secret"), "SN1", NOW)
        .unwrap();
}

#[test]
fn errors_never_show_secrets() {
    let (_d, auth) = setup();
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let h = hash_for(&code, "wrong");
    let err = auth.login("jordi@example.org", &h, "SN1", NOW).unwrap_err();
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains(&h) && !shown.contains(&code));
}

// tokens

#[test]
fn key_is_generated_private_and_reloaded() {
    let d = data();
    let k1 = TokenKey::load_or_create(d.path()).unwrap();
    let path = d.path().join("keys/token.key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let k2 = TokenKey::load_or_create(d.path()).unwrap();
    let claims = Claims {
        user_id: 7,
        equipment_no: "SN1".into(),
        generation: 3,
        create_time: 1,
    };
    assert_eq!(k2.decode(&k1.issue(&claims).unwrap()).unwrap(), claims);
}

#[test]
fn token_round_trip_claims_and_rejections() {
    let d = data();
    let key = TokenKey::load_or_create(d.path()).unwrap();
    let claims = Claims {
        user_id: 1000001,
        equipment_no: "SN078C".into(),
        generation: 2,
        create_time: 1_791_270_000,
    };
    let token = key.issue(&claims).unwrap();
    assert_eq!(key.decode(&token).unwrap(), claims);
    // payload uses the documented claim names and no exp
    let payload = token.split('.').nth(1).unwrap();
    use base64::Engine;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(v["userId"], 1000001);
    assert_eq!(v["equipmentNo"], "SN078C");
    assert_eq!(v["generation"], 2);
    assert_eq!(v["createTime"], 1_791_270_000);
    assert!(v.get("exp").is_none());
    // another key, and garbage
    let other = tempfile::tempdir().unwrap();
    let other_key = TokenKey::load_or_create(other.path()).unwrap();
    assert!(other_key.decode(&token).is_err());
    assert!(key.decode("garbage").is_err());
    let mut tampered = token.clone();
    tampered.push('x');
    assert!(key.decode(&tampered).is_err());
}

#[test]
fn new_login_and_logout_invalidate_older_tokens() {
    let (_d, auth) = setup();
    let login = |now: u64| {
        let (code, _) = auth.challenge("jordi@example.org", now).unwrap();
        auth.login("jordi@example.org", &hash_for(&code, "secret"), "SN1", now)
            .unwrap()
    };
    let first = login(NOW);
    assert!(auth.authenticate(&first.token).is_ok());
    let second = login(NOW + 1000);
    assert!(matches!(
        auth.authenticate(&first.token),
        Err(AuthError::NotLoggedIn)
    ));
    assert_eq!(
        auth.authenticate(&second.token).unwrap().claims.generation,
        2
    );
    assert_eq!(auth.authenticate(&first.token).unwrap_err().code(), "E0712");
    auth.logout("jordi", "SN1").unwrap();
    assert!(auth.authenticate(&second.token).is_err());
    // another device is independent
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let other = auth
        .login("jordi@example.org", &hash_for(&code, "secret"), "SN2", NOW)
        .unwrap();
    assert!(auth.authenticate(&other.token).is_ok());
}

#[test]
fn token_of_removed_account_is_refused() {
    let (_d, auth) = setup();
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let ok = auth
        .login("jordi@example.org", &hash_for(&code, "secret"), "SN1", NOW)
        .unwrap();
    auth.accounts().remove("jordi").unwrap();
    assert!(auth.authenticate(&ok.token).is_err());
}

#[test]
fn device_record_file() {
    let d = data();
    let accounts = Accounts::new(d.path());
    accounts.create("jordi", "jordi@example.org", "pw").unwrap();
    let devices = Devices::new(d.path());
    assert_eq!(devices.generation("jordi", "SN1").unwrap(), None);
    assert_eq!(devices.bump("jordi", "SN1", NOW).unwrap(), 1);
    assert_eq!(devices.bump("jordi", "SN1", NOW).unwrap(), 2);
    assert_eq!(devices.generation("jordi", "SN1").unwrap(), Some(2));
    let text = std::fs::read_to_string(d.path().join("accounts/jordi/devices/SN1.json")).unwrap();
    assert!(text.starts_with("{\n  \"serial\": \"SN1\""));
    assert!(text.ends_with("}\n"));
    assert!(devices.bump("jordi", "../evil", NOW).is_err());
}

// limits added with the server (code cap, lock cooldown)

#[test]
fn pending_codes_are_capped() {
    use notedown_cloud::auth::MAX_PENDING_CODES;
    let codes = LoginCodes::new();
    for i in 0..MAX_PENDING_CODES {
        assert!(codes.issue(&format!("u{i}@example.org"), NOW).is_some());
    }
    // a new email is refused, whatever it is
    assert!(codes.issue("new@example.org", NOW).is_none());
    assert!(codes.issue("jordi@example.org", NOW).is_none());
    // replacing the code of a pending email does not grow the map
    assert!(codes.issue("u0@example.org", NOW).is_some());
    // expired codes make room again
    assert!(
        codes
            .issue(
                "new@example.org",
                NOW + notedown_cloud::auth::CODE_TTL_MS + 1
            )
            .is_some()
    );
}

#[test]
fn lock_clears_after_the_cooldown() {
    use notedown_cloud::auth::LOCK_COOLDOWN_MS;
    let (_d, auth) = setup();
    for _ in 0..6 {
        let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
        auth.login("jordi@example.org", &hash_for(&code, "bad"), "SN1", NOW)
            .unwrap_err();
    }
    let a = auth.accounts().get("jordi").unwrap().unwrap();
    assert!(a.locked);
    assert_eq!(a.locked_at_ms, Some(NOW));
    // still locked just before the end of the cooldown
    let t = NOW + LOCK_COOLDOWN_MS - 1;
    let (code, _) = auth.challenge("jordi@example.org", t).unwrap();
    let err = auth
        .login("jordi@example.org", &hash_for(&code, "secret"), "SN1", t)
        .unwrap_err();
    assert_eq!(err.code(), "E0045");
    // after it the right password works and the counter starts again from zero
    let t = NOW + LOCK_COOLDOWN_MS;
    let (code, _) = auth.challenge("jordi@example.org", t).unwrap();
    let ok = auth
        .login("jordi@example.org", &hash_for(&code, "secret"), "SN1", t)
        .unwrap();
    assert_eq!(ok.failed_logins_before, 0);
    let a = auth.accounts().get("jordi").unwrap().unwrap();
    assert!(!a.locked);
    assert_eq!(a.locked_at_ms, None);
    assert_eq!(a.failed_logins, 0);
}

#[test]
fn legacy_lock_without_time_starts_the_cooldown_now() {
    use notedown_cloud::auth::LOCK_COOLDOWN_MS;
    let (d, auth) = setup();
    // an account.json written before the lock time existed
    let path = d.path().join("accounts/jordi/account.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v.as_object_mut().unwrap().remove("locked_at_ms");
    v["locked"] = true.into();
    v["failed_logins"] = 6.into();
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let err = auth
        .login("jordi@example.org", &hash_for(&code, "secret"), "SN1", NOW)
        .unwrap_err();
    assert_eq!(err.code(), "E0045");
    assert_eq!(
        auth.accounts().get("jordi").unwrap().unwrap().locked_at_ms,
        Some(NOW)
    );
    let t = NOW + LOCK_COOLDOWN_MS;
    let (code, _) = auth.challenge("jordi@example.org", t).unwrap();
    auth.login("jordi@example.org", &hash_for(&code, "secret"), "SN1", t)
        .unwrap();
}

#[test]
fn set_password_clears_the_lock_time() {
    let d = data();
    let accounts = Accounts::new(d.path());
    let mut a = accounts
        .create("jordi", "jordi@example.org", "old")
        .unwrap();
    a.locked = true;
    a.locked_at_ms = Some(NOW);
    accounts.save(&a).unwrap();
    accounts.set_password("jordi", "new").unwrap();
    assert_eq!(accounts.get("jordi").unwrap().unwrap().locked_at_ms, None);
}
