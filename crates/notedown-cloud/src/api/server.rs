//! Server queries and the token routes (`docs/sync-protocol.md` 2.4, 2.5, 6.7).

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use rand::RngExt;
use serde_json::json;

use super::envelope::Reply;
use crate::auth::{Authenticated, CODE_TTL_MS, MAX_FAILED_LOGINS};
use crate::server::AppState;

/// The capacity constant the device sends in `bindEquipment` (2.4); returned as a harmless value.
const TOTAL_CAPACITY: &str = "25485312";
/// `FILE_TYPE` and `FILE_MAX` as read from Ratta's server (3.3).
const FILE_TYPES: &str = "note,mark,pdf,epub,doc,docx,txt,png,jpg,jpeg,webp,bmp,gif,tif,tga,psd,cbz,fb2,xps,chm,rtf,ppt,pptx,xls,xlsx,zip,rar,gz,tar.gz,zz,apk,snstk,snbak,read,spd,otf,ttf,ttc,eot,dfont,woff";
const FILE_MAX: u64 = 1 << 30;

/// Token from `x-access-token`, checked for signature, account and device generation.
fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<(Authenticated, String), Reply> {
    let token = headers
        .get("x-access-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(Reply::not_logged_in)?;
    match state.auth.authenticate(token) {
        Ok(a) => Ok((a, token.to_string())),
        Err(crate::auth::AuthError::Internal(e)) => {
            eprintln!("notedown cloud: {e}");
            Err(Reply::not_logged_in())
        }
        Err(_) => Err(Reply::not_logged_in()),
    }
}

/// `POST /api/user/query`: the user record (`UserQueryByIdVO`); unknown fields are harmless.
pub async fn user_query(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Reply {
    let (a, _) = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    Reply::ok(json!({
        "userId": a.account.user_id,
        "userName": a.account.email,
        "email": a.account.email,
        "telephone": null,
        "countryCode": null,
        "avatarsUrl": null,
        "sex": null,
        "birthday": null,
        "personalSign": null,
        "hobby": null,
        "education": null,
        "job": null,
        "address": null,
        "totalCapacity": TOTAL_CAPACITY,
        "fileServer": "0",
        "isNormal": "Y",
    }))
    .device(&a.claims.equipment_no)
}

/// `POST /api/equipment/bind/status`: always bound for a logged-in device (2.4).
pub async fn bind_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Reply {
    match authenticate(&state, &headers) {
        Ok((a, _)) => Reply::ok(json!({"bindStatus": true})).device(&a.claims.equipment_no),
        Err(r) => r,
    }
}

/// `POST /api/user/logout`: the device's tokens stop working.
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Reply {
    let (a, _) = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let serial = a.claims.equipment_no;
    match state.auth.logout(&a.account.slug, &serial) {
        Ok(()) => Reply::ok(json!({})).device(&serial),
        Err(e) => {
            eprintln!("notedown cloud: {e}");
            Reply::internal().device(&serial)
        }
    }
}

/// `POST /api/user/query/token` (`QueryTokenVO`): the valid token itself (device tokens do not
/// expire, 2.5).
pub async fn query_token(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Reply {
    match authenticate(&state, &headers) {
        Ok((a, token)) => Reply::ok(json!({"token": token})).device(&a.claims.equipment_no),
        Err(r) => r,
    }
}

/// `GET|POST /api/file/query/server`: reachability check (6.7).
pub async fn query_server() -> Reply {
    Reply::ok(json!({}))
}

/// `POST /api/official/system/base/param` (6.7). Values Ratta's server did not show are
/// harmless guesses.
pub async fn base_param() -> Reply {
    let params = [
        ("COPY_MAX", "100".to_string()),
        ("DOWNLOAD_MAX_NUMBER", "100".to_string()),
        ("EMAIL_CODE_TIME", (CODE_TTL_MS / 60_000).to_string()),
        ("FILE_MAX", FILE_MAX.to_string()),
        ("FILE_TYPE", FILE_TYPES.to_string()),
        ("MAX_ERR_COUNTS", MAX_FAILED_LOGINS.to_string()),
        ("UPLOAD_MAX", "100".to_string()),
    ];
    let list: Vec<_> = params
        .iter()
        .enumerate()
        .map(|(i, (name, value))| json!({"serial": i + 1, "name": name, "value": value}))
        .collect();
    let mut rng = rand::rng();
    let random: String = (0..8)
        .map(|_| char::from(b"0123456789abcdef"[rng.random_range(0..16)]))
        .collect();
    Reply::ok(json!({"paramList": list, "random": random}))
}
