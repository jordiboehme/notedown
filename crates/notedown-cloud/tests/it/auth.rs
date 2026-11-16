use notedown_cloud::Error;
use notedown_cloud::auth::{
    Accounts, Auth, AuthError, Claims, Devices, LoginCodes, TokenKey, check_login_hash,
    expected_login_hash, md5_hex,
};
use notedown_cloud::journal::{Journal, Origin};

const NOW: u64 = 1_791_270_000_000;

fn data() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn journal(d: &tempfile::TempDir) -> Journal {
    Journal::open(d.path()).unwrap()
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
        Err(Error::EmailInUse { .. })
    ));
    accounts.remove("anna", &journal(&d)).unwrap();
    assert!(!d.path().join("accounts/anna").exists());
    assert!(matches!(
        accounts.remove("anna", &journal(&d)),
        Err(Error::NoAccount(_))
    ));
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

/// A known HS256 vector, computed independently with Python's `hmac` and `hashlib`
/// (key: the bytes 0 to 31).
const VECTOR: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJ1c2VySWQiOjEwMDAwMDEsImVxdWlwbWVudE5vIjoiU04wNzhDMTAwMTIzNDUiLCJnZW5lcmF0aW9uIjozLCJjcmVhdGVUaW1lIjoxNzkxMzAwNjEyfQ.te3Wt1R8KOyL5oGqGohVYDGLCa79dWQprB2dk52pC-A";

fn vector_claims() -> Claims {
    Claims {
        user_id: 1000001,
        equipment_no: "SN078C10012345".into(),
        generation: 3,
        create_time: 1_791_300_612,
    }
}

#[test]
fn token_matches_an_independent_hs256_vector() {
    let key = TokenKey::from_bytes((0u8..32).collect());
    assert_eq!(key.issue(&vector_claims()).unwrap(), VECTOR);
    assert_eq!(key.decode(VECTOR).unwrap(), vector_claims());
}

#[test]
fn token_with_another_alg_or_malformed_is_rejected_without_panic() {
    let key = TokenKey::from_bytes((0u8..32).collect());
    // Correct HMAC-SHA256 signatures over headers that name HS384 and none.
    let hs384 = "eyJhbGciOiJIUzM4NCIsInR5cCI6IkpXVCJ9.eyJ1c2VySWQiOjEwMDAwMDEsImVxdWlwbWVudE5vIjoiU04wNzhDMTAwMTIzNDUiLCJnZW5lcmF0aW9uIjozLCJjcmVhdGVUaW1lIjoxNzkxMzAwNjEyfQ.kxTlHz-rhl1VOv7ILOqlfBBcsQGdgdUdGaGEdsFulXs";
    let none = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJ1c2VySWQiOjEwMDAwMDEsImVxdWlwbWVudE5vIjoiU04wNzhDMTAwMTIzNDUiLCJnZW5lcmF0aW9uIjozLCJjcmVhdGVUaW1lIjoxNzkxMzAwNjEyfQ.sUdf9aRpDd1PHDu4_xaqtW4i8Q6-g5qmwjO9Kq7sPP0";
    assert!(key.decode(hs384).is_err());
    assert!(key.decode(none).is_err());
    let (h, rest) = VECTOR.split_once('.').unwrap();
    let (p, _) = rest.split_once('.').unwrap();
    for bad in [
        "".to_string(),
        ".".into(),
        "..".into(),
        "...".into(),
        "a.b.c".into(),
        "a.b.c.d".into(),
        format!("{h}.{p}"),
        format!("{h}.{p}."),
        format!("{h}.{p}.!!!"),
        format!("{VECTOR}.x"),
        format!("{h}.{p}.te3Wt1R8KOyL5oGqGohVYDGLCa79dWQprB2dk52pC-Q"),
        format!("{h}.{p}.te3Wt1R8KOyL5oGqGohVYDGLCa79dWQprB2dk52pC"),
        format!("x{VECTOR}"),
        "eyJ\u{e9}.\u{1f600}.\u{0}".into(),
    ] {
        assert!(key.decode(&bad).is_err(), "{bad}");
    }
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
    auth.accounts().remove("jordi", &journal(&_d)).unwrap();
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

// The account lock (`accounts/<slug>/.lock`) between the CLI and the server

#[test]
fn concurrent_failed_login_cannot_undo_set_password() {
    let (d, auth) = setup();
    let accounts = Accounts::new(d.path());
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    // The CLI is inside `cloud account passwd` and holds the account lock.
    let held = accounts.lock("jordi").unwrap();
    let login = std::thread::spawn(move || {
        auth.login("jordi@example.org", &hash_for(&code, "wrong"), "SN1", NOW)
            .map_err(|e| e.code())
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!login.is_finished(), "the login must wait for the lock");
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.password_md5 = md5_hex("new");
    a.failed_logins = 0;
    accounts.save(&a).unwrap();
    drop(held);
    assert_eq!(login.join().unwrap().unwrap_err(), "E0019");
    let a = accounts.get("jordi").unwrap().unwrap();
    assert_eq!(a.password_md5, md5_hex("new"), "the new password survives");
    assert_eq!(a.failed_logins, 1, "the failure counts on top of it");
    assert!(d.path().join("accounts/jordi/.lock").is_file());
}

#[test]
fn set_password_waits_for_a_running_login_and_wins() {
    let (d, _auth) = setup();
    let accounts = Accounts::new(d.path());
    // The server is inside a login and holds the account lock.
    let held = accounts.lock("jordi").unwrap();
    let cli = accounts.clone();
    let passwd = std::thread::spawn(move || cli.set_password("jordi", "new"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!passwd.is_finished(), "passwd must wait for the lock");
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.failed_logins = 3;
    accounts.save(&a).unwrap();
    drop(held);
    passwd.join().unwrap().unwrap();
    let a = accounts.get("jordi").unwrap().unwrap();
    assert_eq!(a.password_md5, md5_hex("new"));
    assert_eq!(a.failed_logins, 0);
}

#[test]
fn duplicate_email_is_reported_as_such() {
    let d = data();
    let accounts = Accounts::new(d.path());
    accounts
        .create("jordi", "jordi@example.org", "secret")
        .unwrap();
    let err = accounts
        .create("jordi2", "JORDI@example.org", "secret")
        .unwrap_err();
    assert!(
        matches!(&err, Error::EmailInUse { slug, .. } if slug == "jordi"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "email JORDI@example.org is already used by account jordi"
    );
    assert!(accounts.get("jordi2").unwrap().is_none());
}

#[test]
fn account_remove_waits_for_the_store_lock_first() {
    let dir = tempfile::tempdir().unwrap();
    let accounts = notedown_cloud::auth::Accounts::new(dir.path());
    accounts.create("jordi", "j@example.org", "secret").unwrap();
    let account = accounts.account_dir("jordi");
    std::fs::create_dir_all(account.join("files/NOTE")).unwrap();
    // A commit of the server (or an import) holds the store lock.
    let held = notedown_cloud::store::take_store_lock(&account, None).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let data = dir.path().to_path_buf();
    let remover = std::thread::spawn(move || {
        let r = notedown_cloud::auth::Accounts::new(&data)
            .remove("jordi", &Journal::open(&data).unwrap());
        tx.send(()).unwrap();
        r
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "remove did not wait for the store lock"
    );
    assert!(account.join("files/NOTE").is_dir());
    drop(held);
    rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    remover.join().unwrap().unwrap();
    assert!(!account.exists());
}

#[test]
fn a_device_record_of_a_removed_account_is_no_account_and_creates_nothing() {
    let d = data();
    let accounts = Accounts::new(d.path());
    accounts.create("jordi", "jordi@example.org", "pw").unwrap();
    let devices = Devices::new(d.path());
    assert_eq!(devices.bump("jordi", "SN1", NOW).unwrap(), 1);
    accounts.remove("jordi", &journal(&d)).unwrap();
    assert!(matches!(
        devices.invalidate("jordi", "SN1"),
        Err(Error::NoAccount(_))
    ));
    assert!(matches!(
        devices.bump("jordi", "SN2", NOW),
        Err(Error::NoAccount(_))
    ));
    assert!(!d.path().join("accounts/jordi").exists());
}

#[test]
fn account_remove_deletes_the_pipeline_text_and_create_drops_a_leftover() {
    use notedown_cloud::auth::PIPELINE_DIR;
    let d = data();
    let accounts = Accounts::new(d.path());
    accounts.create("jordi", "jordi@example.org", "pw").unwrap();
    let text = d.path().join(PIPELINE_DIR).join("jordi");
    std::fs::create_dir_all(text.join("notes")).unwrap();
    std::fs::write(text.join("notes/n.json"), "{}").unwrap();
    std::fs::create_dir_all(d.path().join(PIPELINE_DIR).join("anna")).unwrap();
    accounts.remove("jordi", &journal(&d)).unwrap();
    assert!(!text.exists());
    // Nothing else of the pipeline is touched, and no tombstone is left.
    let left: Vec<_> = std::fs::read_dir(d.path().join(PIPELINE_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left, ["anna"]);
    // A text store left by a crash, or recreated by a running writer, is dropped on create.
    std::fs::create_dir_all(text.join("notes")).unwrap();
    accounts.create("jordi", "jordi@example.org", "pw").unwrap();
    assert!(!text.exists());
}

#[test]
fn removing_an_account_journals_its_user_id() {
    let d = data();
    let accounts = Accounts::new(d.path());
    let a = accounts.create("jordi", "jordi@example.org", "pw").unwrap();
    assert_eq!(a.user_id, 1_000_001);
    let journal = journal(&d);
    accounts.remove("jordi", &journal).unwrap();
    let events: Vec<_> = journal.read_from(0).unwrap().map(|e| e.unwrap()).collect();
    let last = events.last().expect("an event");
    assert_eq!(last.body.type_name(), "account.removed");
    assert_eq!(last.account, "jordi");
    assert_eq!(last.origin, Origin::Cli);
    assert_eq!(last.device, None);
    let line: serde_json::Value = serde_json::from_str(&last.to_line()).unwrap();
    assert_eq!(line["type"], "account.removed");
    assert_eq!(line["userId"], 1_000_001);
    assert!(!d.path().join("accounts/jordi").exists());
}

#[test]
fn a_failed_journal_append_removes_nothing() {
    let d = data();
    let accounts = Accounts::new(d.path());
    accounts.create("jordi", "jordi@example.org", "pw").unwrap();
    // No journal/ folder: an append through open_no_create fails.
    let journal = Journal::open_no_create(d.path());
    assert!(accounts.remove("jordi", &journal).is_err());
    assert!(accounts.get("jordi").unwrap().is_some());
    assert!(d.path().join("accounts/jordi").is_dir());
}

// REST tokens

use notedown_cloud::auth::{
    Account, RestKey, RestTokens, Scope, TokenCheck, TokenInfo, list_tokens, revoke_all_tokens,
    revoke_token,
};

fn rest() -> (tempfile::TempDir, Auth, RestTokens) {
    let (d, auth) = setup();
    let tokens = RestTokens::new(d.path(), RestKey::load_or_create(d.path()).unwrap());
    (d, auth, tokens)
}

fn rest_login(auth: &Auth, name: &str) -> (String, TokenInfo, Account) {
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    auth.rest_login(
        "jordi@example.org",
        &hash_for(&code, "secret"),
        name,
        Scope::Read,
        NOW,
    )
    .unwrap()
}

fn removed_by_hand(d: &tempfile::TempDir, slug: &str, user_id: u64) -> u64 {
    use notedown_cloud::journal::{Event, EventBody, new_op_id};
    let mut event = [Event::new(
        slug,
        Origin::Cli,
        None,
        &new_op_id(),
        EventBody::AccountRemoved { user_id },
    )];
    journal(d).append(&mut event).unwrap()[0]
}

fn is_refused(check: TokenCheck) -> bool {
    matches!(check, TokenCheck::Refused)
}

#[test]
fn a_token_checks_as_its_account() {
    let (d, auth, tokens) = rest();
    let (token, info, account) = rest_login(&auth, "mac mini");
    assert!(token.starts_with("ndt1."));
    assert_eq!(account.slug, "jordi");
    assert_eq!(info.name, "mac mini");
    assert_eq!(info.user_id, account.user_id);
    assert_eq!(info.created_ms, NOW);
    assert_eq!(info.scope, Scope::Read);
    assert_eq!(info.id.len(), 8);
    match tokens.check(&token, NOW).unwrap() {
        TokenCheck::Valid(a, Scope::Read) => assert_eq!(a.slug, "jordi"),
        other => panic!("{other:?}"),
    }
    assert_eq!(list_tokens(d.path(), "jordi").unwrap(), [info]);
    assert!(is_refused(tokens.check("ndt1.x.y", NOW).unwrap()));
    assert!(is_refused(tokens.check("", NOW).unwrap()));
}

#[test]
fn a_revoked_token_is_refused() {
    let (d, auth, tokens) = rest();
    let (one, a, _) = rest_login(&auth, "one");
    let (two, b, _) = rest_login(&auth, "two");
    assert_ne!(a.id, b.id);
    assert!(revoke_token(d.path(), "jordi", &a.id).unwrap());
    assert!(!revoke_token(d.path(), "jordi", &a.id).unwrap());
    assert!(revoke_token(d.path(), "jordi", "../x").is_err());
    assert!(is_refused(tokens.check(&one, NOW).unwrap()));
    assert!(matches!(
        tokens.check(&two, NOW).unwrap(),
        TokenCheck::Valid(_, Scope::Read)
    ));
    assert_eq!(revoke_all_tokens(d.path(), "jordi").unwrap(), 1);
    assert!(is_refused(tokens.check(&two, NOW).unwrap()));
    assert!(list_tokens(d.path(), "jordi").unwrap().is_empty());
}

#[test]
fn a_password_change_refuses_every_token() {
    let (_d, auth, tokens) = rest();
    let (one, _, _) = rest_login(&auth, "one");
    let (two, _, _) = rest_login(&auth, "two");
    assert_eq!(auth.accounts().set_password("jordi", "secret").unwrap(), 2);
    assert!(is_refused(tokens.check(&one, NOW).unwrap()));
    assert!(is_refused(tokens.check(&two, NOW).unwrap()));
    assert_eq!(auth.accounts().set_password("jordi", "new").unwrap(), 0);
}

#[test]
fn a_removed_account_token_is_removed_with_its_seq() {
    let (d, auth, tokens) = rest();
    let (token, _, account) = rest_login(&auth, "one");
    let journal = journal(&d);
    auth.accounts().remove("jordi", &journal).unwrap();
    let seq = journal.read_from(0).unwrap().last().unwrap().unwrap().seq;
    match tokens.check(&token, NOW).unwrap() {
        TokenCheck::Removed { user_id, seq: s } => {
            assert_eq!(user_id, account.user_id);
            assert_eq!(s, seq);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn two_removal_records_use_the_last() {
    let (d, auth, tokens) = rest();
    let (token, _, account) = rest_login(&auth, "one");
    auth.accounts().remove("jordi", &journal(&d)).unwrap();
    // Another account's removal and a removal of another user id of this slug in between.
    removed_by_hand(&d, "anna", account.user_id);
    removed_by_hand(&d, "jordi", account.user_id + 7);
    let last = removed_by_hand(&d, "jordi", account.user_id);
    removed_by_hand(&d, "jordi", account.user_id + 8);
    match tokens.check(&token, NOW).unwrap() {
        TokenCheck::Removed { user_id, seq } => {
            assert_eq!(user_id, account.user_id);
            assert_eq!(seq, last);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_refused_token_of_a_live_account_with_a_removal_record_is_refused() {
    let (d, auth, tokens) = rest();
    let (token, info, account) = rest_login(&auth, "one");
    // A crash between the record and the removal leaves a live account with a record.
    removed_by_hand(&d, "jordi", account.user_id);
    assert!(matches!(
        tokens.check(&token, NOW).unwrap(),
        TokenCheck::Valid(_, Scope::Read)
    ));
    assert!(revoke_token(d.path(), "jordi", &info.id).unwrap());
    assert!(is_refused(tokens.check(&token, NOW).unwrap()));
    // A fresh check (no cache) says the same.
    let fresh = RestTokens::new(d.path(), RestKey::load_or_create(d.path()).unwrap());
    assert!(is_refused(fresh.check(&token, NOW).unwrap()));
}

#[test]
fn a_token_of_a_reused_slug_is_removed_not_valid() {
    let (d, auth, tokens) = rest();
    let (token, _, old) = rest_login(&auth, "one");
    auth.accounts().remove("jordi", &journal(&d)).unwrap();
    let new = auth
        .accounts()
        .create("jordi", "jordi@example.org", "secret")
        .unwrap();
    assert_ne!(new.user_id, old.user_id);
    let (fresh, _, _) = rest_login(&auth, "two");
    match tokens.check(&token, NOW).unwrap() {
        TokenCheck::Removed { user_id, .. } => assert_eq!(user_id, old.user_id),
        other => panic!("{other:?}"),
    }
    match tokens.check(&fresh, NOW).unwrap() {
        TokenCheck::Valid(a, Scope::Read) => assert_eq!(a.user_id, new.user_id),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_token_for_an_unknown_user_id_is_refused() {
    let (d, auth, tokens) = rest();
    let (token, _, _) = rest_login(&auth, "one");
    // An older backup restored: the slug holds another user id and the journal no removal.
    let mut a = auth.accounts().get("jordi").unwrap().unwrap();
    a.user_id += 100;
    auth.accounts().save(&a).unwrap();
    assert!(is_refused(tokens.check(&token, NOW).unwrap()));
    // The account folder is gone without a record.
    std::fs::remove_dir_all(d.path().join("accounts/jordi")).unwrap();
    assert!(is_refused(tokens.check(&token, NOW + 61_000).unwrap()));
}

#[test]
fn a_damaged_account_file_is_an_error_not_a_removal() {
    let (d, auth, tokens) = rest();
    let (token, _, account) = rest_login(&auth, "one");
    removed_by_hand(&d, "jordi", account.user_id);
    std::fs::write(d.path().join("accounts/jordi/account.json"), "{").unwrap();
    assert!(tokens.check(&token, NOW).is_err());
}

#[test]
fn issuing_never_stores_the_token() {
    let (d, auth, _tokens) = rest();
    let (token, info, _) = rest_login(&auth, "one");
    let nonce = token.split('.').nth(1).unwrap().to_string();
    let mut stack = vec![d.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(&token), "{}", path.display());
            assert!(!text.contains(&nonce), "{}", path.display());
        }
    }
    let dir = d.path().join("accounts/jordi/tokens");
    let file = dir.join(format!("{}.json", info.id));
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let keys: Vec<_> = stored.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys.len(), 6);
    for k in ["id", "name", "userId", "sha256", "createdMs", "scope"] {
        assert!(stored.get(k).is_some(), "{k}");
    }
    use sha2::Digest;
    assert_eq!(
        stored["sha256"],
        const_hex::encode(sha2::Sha256::digest(token.as_bytes()))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }
}

#[test]
fn rest_login_counts_failures_like_the_device_login() {
    let (_d, auth, _tokens) = rest();
    let failed = |auth: &Auth| auth.accounts().get("jordi").unwrap().unwrap().failed_logins;
    // A missing code does not count.
    let err = auth
        .rest_login(
            "jordi@example.org",
            &hash_for("nocode", "secret"),
            "x",
            Scope::Read,
            NOW,
        )
        .unwrap_err();
    assert_eq!(err.code(), "E0019");
    assert_eq!(failed(&auth), 0);
    let err = auth
        .rest_login("ghost@example.org", "00", "x", Scope::Read, NOW)
        .unwrap_err();
    assert_eq!(err.code(), "E0018");
    for i in 1..=6 {
        let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
        let wrong = hash_for(&code, "bad");
        let err = if i % 2 == 0 {
            auth.rest_login("jordi@example.org", &wrong, "x", Scope::Read, NOW)
                .unwrap_err()
        } else {
            auth.login("jordi@example.org", &wrong, "SN1", NOW)
                .unwrap_err()
        };
        assert_eq!(err.code(), "E0019", "attempt {i}");
        assert_eq!(failed(&auth), i);
    }
    // Locked for both logins, also with the right password.
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let err = auth
        .rest_login(
            "jordi@example.org",
            &hash_for(&code, "secret"),
            "x",
            Scope::Read,
            NOW,
        )
        .unwrap_err();
    assert_eq!(err.code(), "E0045");
    let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
    let err = auth
        .login("jordi@example.org", &hash_for(&code, "secret"), "SN1", NOW)
        .unwrap_err();
    assert_eq!(err.code(), "E0045");
    // After the cooldown a good REST login resets the counter.
    let later = NOW + notedown_cloud::auth::LOCK_COOLDOWN_MS;
    let (code, _) = auth.challenge("jordi@example.org", later).unwrap();
    auth.rest_login(
        "jordi@example.org",
        &hash_for(&code, "secret"),
        "x",
        Scope::Read,
        later,
    )
    .unwrap();
    let a = auth.accounts().get("jordi").unwrap().unwrap();
    assert!(!a.locked);
    assert_eq!(a.failed_logins, 0);
}

#[test]
fn a_locked_account_keeps_its_tokens() {
    let (_d, auth, tokens) = rest();
    let (token, _, _) = rest_login(&auth, "one");
    for _ in 0..6 {
        let (code, _) = auth.challenge("jordi@example.org", NOW).unwrap();
        let _ = auth.rest_login(
            "jordi@example.org",
            &hash_for(&code, "bad"),
            "x",
            Scope::Read,
            NOW,
        );
    }
    assert!(auth.accounts().get("jordi").unwrap().unwrap().locked);
    assert!(matches!(
        tokens.check(&token, NOW).unwrap(),
        TokenCheck::Valid(_, Scope::Read)
    ));
}

#[test]
fn the_removal_lookup_is_cached() {
    let (d, auth, tokens) = rest();
    let (token, _, account) = rest_login(&auth, "one");
    auth.accounts().remove("jordi", &journal(&d)).unwrap();
    let first = tokens.check(&token, NOW).unwrap();
    assert!(matches!(first, TokenCheck::Removed { .. }));
    std::fs::remove_dir_all(d.path().join("journal")).unwrap();
    // Without the journal a fresh lookup finds nothing ...
    let fresh = RestTokens::new(d.path(), RestKey::load_or_create(d.path()).unwrap());
    assert!(is_refused(fresh.check(&token, NOW).unwrap()));
    // ... but a found removal is kept, also much later.
    match tokens.check(&token, NOW + 86_400_000).unwrap() {
        TokenCheck::Removed { user_id, .. } => assert_eq!(user_id, account.user_id),
        other => panic!("{other:?}"),
    }
}

#[test]
fn no_removal_is_cached_for_a_minute() {
    let (d, auth, tokens) = rest();
    let (token, _, account) = rest_login(&auth, "one");
    // The folder goes first, the record comes later (a removal seen half-way).
    std::fs::remove_dir_all(d.path().join("accounts/jordi")).unwrap();
    assert!(is_refused(tokens.check(&token, NOW).unwrap()));
    let seq = removed_by_hand(&d, "jordi", account.user_id);
    assert!(is_refused(tokens.check(&token, NOW + 59_000).unwrap()));
    assert!(matches!(
        tokens.check(&token, NOW + 60_000).unwrap(),
        TokenCheck::Removed { seq: s, .. } if s == seq
    ));
}

#[test]
fn listing_tokens_creates_no_key() {
    let d = data();
    let accounts = Accounts::new(d.path());
    accounts.create("jordi", "jordi@example.org", "pw").unwrap();
    assert!(list_tokens(d.path(), "jordi").unwrap().is_empty());
    assert!(!revoke_token(d.path(), "jordi", "0123abcd").unwrap());
    assert_eq!(revoke_all_tokens(d.path(), "jordi").unwrap(), 0);
    assert_eq!(accounts.set_password("jordi", "new").unwrap(), 0);
    assert!(!d.path().join("keys").exists());
    assert!(matches!(
        list_tokens(d.path(), "ghost"),
        Err(Error::NoAccount(_))
    ));
}

#[test]
fn a_restored_and_removed_again_account_gives_the_new_seq() {
    let (d, auth, tokens) = rest();
    let (token, info, _) = rest_login(&auth, "one");
    let dir = d.path().join("accounts/jordi");
    let token_file = format!("tokens/{}.json", info.id);
    let account_json = std::fs::read(dir.join("account.json")).unwrap();
    let token_json = std::fs::read(dir.join(&token_file)).unwrap();
    let journal = journal(&d);
    auth.accounts().remove("jordi", &journal).unwrap();
    let first = match tokens.check(&token, NOW).unwrap() {
        TokenCheck::Removed { seq, .. } => seq,
        other => panic!("{other:?}"),
    };
    // A backup brings the account back by hand.
    std::fs::create_dir_all(dir.join("tokens")).unwrap();
    std::fs::write(dir.join("account.json"), &account_json).unwrap();
    std::fs::write(dir.join(&token_file), &token_json).unwrap();
    assert!(matches!(
        tokens.check(&token, NOW).unwrap(),
        TokenCheck::Valid(_, Scope::Read)
    ));
    auth.accounts().remove("jordi", &journal).unwrap();
    match tokens.check(&token, NOW).unwrap() {
        TokenCheck::Removed { seq, .. } => assert!(seq > first, "{seq} > {first}"),
        other => panic!("{other:?}"),
    }
}
