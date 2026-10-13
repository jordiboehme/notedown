//! `POST /oauth/token` (authorization_code with PKCE, refresh_token) and `POST /oauth/revoke`.

use std::sync::Arc;

use axum::Form;
use axum::extract::State;
use axum::extract::rejection::FormRejection;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::grants::{ACCESS_TTL_MS, Issued, Refused};
use crate::{Oauth, oauth_error, secret};

const BAD_CODE: &str = "this code cannot be exchanged: it is unknown, used, expired, or was issued for another client, redirect URI or verifier";
const BAD_REFRESH: &str = "this refresh token cannot be used: it is unknown, expired, revoked, or was issued to another client";

#[derive(Deserialize)]
pub(crate) struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}

fn granted(i: Issued) -> Response {
    (
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        axum::Json(json!({
            "access_token": i.access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_MS / 1000,
            "refresh_token": i.refresh,
            "scope": crate::SCOPE,
        })),
    )
        .into_response()
}

/// Runs blocking store work off the async threads; a failed task is a store error too.
async fn blocking<T: Send + 'static>(
    o: &Arc<Oauth>,
    work: impl FnOnce(&Oauth) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let o = o.clone();
    tokio::task::spawn_blocking(move || work(&o))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("store task failed: {e}")))
}

fn unavailable(e: anyhow::Error) -> Response {
    eprintln!("notedown mcp: token store failed: {e}");
    oauth_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "temporarily_unavailable",
        "the grants cannot be read or written right now",
    )
}

pub(crate) async fn token(
    State(o): State<Arc<Oauth>>,
    form: Result<Form<TokenForm>, FormRejection>,
) -> Response {
    let Ok(Form(f)) = form else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the request body must be a form",
        );
    };
    let now = o.now_ms();
    if o.sweep_due(now) {
        let o = o.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || o.sweep(now)).await {
            eprintln!("notedown mcp: sweep task failed: {e}");
        }
    }
    let resource = o.base.resource();
    // A code is spent by its first use, so it is taken before any check that can fail.
    let taken = match (f.grant_type.as_deref(), f.code.as_deref()) {
        (Some("authorization_code"), Some(code)) => Some(o.codes.take(code, now)),
        _ => None,
    };
    if f.resource.as_deref().is_some_and(|r| r != resource) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "the resource is not this server's MCP endpoint",
        );
    }
    let Some(client_id) = f.client_id.clone() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "client_id is required",
        );
    };
    match f.grant_type.as_deref() {
        Some("authorization_code") => {
            let (Some(taken), Some(verifier)) = (taken, f.code_verifier.as_deref()) else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "code and code_verifier are required",
                );
            };
            let Some(c) = taken else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", BAD_CODE);
            };
            let ok = c.client_id == client_id
                && f.redirect_uri.as_deref() == Some(c.redirect_uri.as_str())
                && secret::pkce_text_ok(verifier)
                && secret::pkce_matches(verifier, &c.challenge)
                && c.resource == resource;
            if !ok {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", BAD_CODE);
            }
            let result = blocking(&o, move |o| {
                let Some(cl) = o.clients.get(&client_id)? else {
                    return Ok(None);
                };
                o.clients.touch(&client_id, now);
                o.grants
                    .issue(&c.slug, &client_id, &cl.name, &resource, now)
                    .map(Some)
            })
            .await;
            match result {
                Ok(Some(i)) => granted(i),
                Ok(None) => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", BAD_CODE),
                Err(e) => unavailable(e),
            }
        }
        Some("refresh_token") => {
            let Some(token) = f.refresh_token.clone() else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "refresh_token is required",
                );
            };
            let result = blocking(&o, move |o| {
                let r = o.grants.refresh(&token, &client_id, &resource, now)?;
                if r.is_ok() {
                    o.clients.touch(&client_id, now);
                }
                Ok(r)
            })
            .await;
            match result {
                Ok(Ok(i)) => granted(i),
                Ok(Err(Refused::Grant)) => {
                    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", BAD_REFRESH)
                }
                Ok(Err(Refused::Target)) => oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_target",
                    "this grant is for another resource",
                ),
                Err(e) => unavailable(e),
            }
        }
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type must be authorization_code or refresh_token",
        ),
    }
}

#[derive(Deserialize)]
pub(crate) struct RevokeForm {
    token: Option<String>,
}

pub(crate) async fn revoke(
    State(o): State<Arc<Oauth>>,
    form: Result<Form<RevokeForm>, FormRejection>,
) -> Response {
    // RFC 7009: the answer is 200 whatever the request held.
    let token = form.ok().and_then(|Form(f)| f.token);
    if let Some(t) = token
        && let Err(e) = blocking(&o, move |o| o.grants.revoke(&t)).await
    {
        return unavailable(e);
    }
    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")]).into_response()
}
