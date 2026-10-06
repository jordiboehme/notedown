//! Login and device binding (`docs/sync-protocol.md` 2.1 to 2.6). None of these needs a token.

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use serde_json::{Value, json};

use super::envelope::{E_BOUND_ELSEWHERE, Reply, field, parse_body};
use super::{ClientAddr, note_firmware};
use crate::auth::{AuthError, Devices};
use crate::server::AppState;

const MSG_NO_ACCOUNT: &str = "Account does not exist";
const MSG_WRONG_PASSWORD: &str = "Wrong account or password";
const MSG_LOCKED: &str = "Account locked, please try again later";
const MSG_BOUND_ELSEWHERE: &str = "This device is bound to another account";
const MSG_LOGIN_FIRST: &str = "Please log in on the device first";

fn auth_error(e: &AuthError) -> Reply {
    match e {
        AuthError::NoAccount => Reply::error("E0018", MSG_NO_ACCOUNT),
        AuthError::WrongPassword => Reply::error("E0019", MSG_WRONG_PASSWORD),
        AuthError::Locked => Reply::error("E0045", MSG_LOCKED),
        AuthError::NotLoggedIn => Reply::not_logged_in(),
        AuthError::Internal(err) => {
            eprintln!("notedown cloud: {err}");
            Reply::internal()
        }
    }
}

fn internal(err: crate::Error) -> Reply {
    auth_error(&AuthError::Internal(err))
}

fn serial(body: &Value) -> Result<String, Reply> {
    let s = field(body, "equipmentNo").ok_or_else(|| Reply::bad_params("equipmentNo"))?;
    if Devices::valid_serial(&s) {
        Ok(s)
    } else {
        Err(Reply::bad_params("equipmentNo"))
    }
}

/// `POST /api/terminal/equipment/unlink {equipmentNo, version}`: the device is no longer bound,
/// and its tokens stop working (2.6). Answers success also for an unknown device.
pub async fn unlink(State(state): State<Arc<AppState>>, body: Bytes) -> Reply {
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let serial = match serial(&body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let version = field(&body, "version");
    let devices = state.auth.devices();
    let owners = match devices.owners(&serial) {
        Ok(o) => o,
        Err(e) => return internal(e),
    };
    for slug in owners {
        let r = note_firmware(&state, &slug, &serial, version.as_deref(), |d| {
            d.bound = false
        })
        .and_then(|_| state.auth.logout(&slug, &serial));
        if let Err(e) = r {
            return internal(e).device(&serial);
        }
    }
    Reply::ok(json!({})).device(&serial)
}

/// `POST /api/official/user/check/exists/server {email, version}` or `{}` (2.4).
pub async fn check_exists(State(state): State<Arc<AppState>>, body: Bytes) -> Reply {
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let user_id = match field(&body, "email") {
        None => Value::Null,
        Some(email) => match state.auth.accounts().find_by_email(&email) {
            Ok(Some(a)) => json!(a.user_id),
            Ok(None) => return auth_error(&AuthError::NoAccount),
            Err(e) => return internal(e),
        },
    };
    Reply::ok(json!({
        "dms": "ALL",
        "userId": user_id,
        "uniqueMachineId": state.auth.machine_id(),
    }))
}

/// `POST /api/official/user/query/random/code {account, countryCode, version}` (2.3). A code is
/// issued for any email (the answer does not reveal accounts).
pub async fn random_code(
    State(state): State<Arc<AppState>>,
    Extension(ClientAddr(ip)): Extension<ClientAddr>,
    body: Bytes,
) -> Reply {
    let now = state.now_ms();
    if !state.limiter.allow(ip, "random_code", now) {
        return Reply::too_many();
    }
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(account) = field(&body, "account") else {
        return Reply::bad_params("account");
    };
    match state.auth.challenge(&account, now) {
        Some((code, ts)) => Reply::ok(json!({"randomCode": code, "timestamp": ts})),
        None => Reply::too_many(),
    }
}

/// `POST /api/official/user/account/login/equipment` (2.4).
pub async fn login_equipment(
    State(state): State<Arc<AppState>>,
    Extension(ClientAddr(ip)): Extension<ClientAddr>,
    body: Bytes,
) -> Reply {
    let now = state.now_ms();
    if !state.limiter.allow(ip, "login_equipment", now) {
        return Reply::too_many();
    }
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let (Some(account), Some(password)) = (field(&body, "account"), field(&body, "password"))
    else {
        return Reply::bad_params("account and password");
    };
    let serial = match serial(&body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let ok = match state.auth.login(&account, &password, &serial, now) {
        Ok(ok) => ok,
        Err(e) => return auth_error(&e).device(&serial),
    };
    let slug = ok.account.slug.clone();
    let this = match note_firmware(
        &state,
        &slug,
        &serial,
        field(&body, "version").as_deref(),
        |_| {},
    ) {
        Ok(d) => d,
        Err(e) => return internal(e).device(&serial),
    };
    let any_bound = match state.auth.devices().list(&slug) {
        Ok(list) => list.iter().any(|d| d.bound),
        Err(e) => return internal(e).device(&serial),
    };
    let yn = |b: bool| if b { "Y" } else { "N" };
    Reply::ok(json!({
        "token": ok.token,
        "counts": ok.failed_logins_before.to_string(),
        // The device trace shows the email here (docs/sync-protocol.md 2.4).
        "userName": ok.account.email,
        "avatarsUrl": null,
        "lastUpdateTime": null,
        "isBind": yn(any_bound),
        "isBindEquipment": yn(this.bound),
        "soldOutCount": 0,
        "mfaRequired": false,
        "mfaToken": null,
    }))
    .device(&serial)
}

/// `POST /api/terminal/user/bindEquipment` (2.4, 2.6): records the binding in
/// `devices/<serial>.json`. Creates nothing under `files/`. The device must have logged in to
/// this account before (the call carries no token).
pub async fn bind_equipment(State(state): State<Arc<AppState>>, body: Bytes) -> Reply {
    let body = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(email) = field(&body, "account") else {
        return Reply::bad_params("account");
    };
    let serial = match serial(&body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let account = match state.auth.accounts().find_by_email(&email) {
        Ok(Some(a)) => a,
        Ok(None) => return auth_error(&AuthError::NoAccount).device(&serial),
        Err(e) => return internal(e).device(&serial),
    };
    let devices = state.auth.devices();
    let owners = match devices.owners(&serial) {
        Ok(o) => o,
        Err(e) => return internal(e).device(&serial),
    };
    for other in owners.iter().filter(|s| **s != account.slug) {
        match devices.get(other, &serial) {
            Ok(Some(d)) if d.bound => {
                return Reply::error(E_BOUND_ELSEWHERE, MSG_BOUND_ELSEWHERE).device(&serial);
            }
            Ok(_) => {}
            Err(e) => return internal(e).device(&serial),
        }
    }
    if !owners.contains(&account.slug) {
        return Reply::error(super::envelope::E_NOT_LOGGED_IN, MSG_LOGIN_FIRST).device(&serial);
    }
    let name = field(&body, "name");
    let capacity = field(&body, "totalCapacity");
    let r = note_firmware(
        &state,
        &account.slug,
        &serial,
        field(&body, "version").as_deref(),
        |d| {
            d.bound = true;
            if name.is_some() {
                d.name = name;
            }
            if capacity.is_some() {
                d.total_capacity = capacity;
            }
        },
    );
    match r {
        Ok(_) => Reply::ok(json!({})).device(&serial),
        Err(e) => internal(e).device(&serial),
    }
}
