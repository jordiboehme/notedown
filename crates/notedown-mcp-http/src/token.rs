//! `POST /oauth/token` (authorization_code with PKCE, refresh_token) and `POST /oauth/revoke`.

use std::sync::Arc;

use axum::Form;
use axum::extract::State;
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

pub(crate) async fn token(State(o): State<Arc<Oauth>>, Form(f): Form<TokenForm>) -> Response {
    let now = o.now_ms();
    let _ = blocking(&o, move |o| {
        o.maybe_sweep(now);
        Ok(())
    })
    .await;
    let resource = o.base.resource();
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
            let (Some(code), Some(verifier)) = (f.code.as_deref(), f.code_verifier.as_deref())
            else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "code and code_verifier are required",
                );
            };
            // Taken before any check: a code is spent by its first use, right or wrong.
            let Some(c) = o.codes.take(code, now) else {
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

pub(crate) async fn revoke(State(o): State<Arc<Oauth>>, Form(f): Form<RevokeForm>) -> Response {
    if let Some(t) = f.token
        && let Err(e) = blocking(&o, move |o| o.grants.revoke(&t)).await
    {
        return unavailable(e);
    }
    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")]).into_response()
}
