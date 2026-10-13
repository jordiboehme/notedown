//! `GET /oauth/authorize` shows the login page; `POST /oauth/authorize` checks the password.

use std::sync::Arc;

use axum::Extension;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use notedown_cloud::api::ClientAddr;
use serde::Deserialize;

use crate::login::{self, Page};
use crate::pending::{CODE_TTL_MS, Code, PENDING_TTL_MS, Pending};
use crate::throttle::Verdict;
use crate::{Oauth, redirect, secret};

const MAX_STATE_LEN: usize = 2048;
const WRONG: &str = "email or password wrong";

#[derive(Deserialize)]
pub(crate) struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    resource: Option<String>,
}

fn found(location: String) -> Response {
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
    )
        .into_response()
}

/// A redirect with `error`, `state` and `iss`.
fn redirect_error(
    o: &Oauth,
    uri: &str,
    state: Option<&str>,
    error: &str,
    description: &str,
) -> Response {
    let mut params = vec![("error", error), ("error_description", description)];
    if let Some(s) = state {
        params.push(("state", s));
    }
    params.push(("iss", &o.base.origin));
    found(redirect::with_params(uri, &params))
}

pub(crate) async fn show(
    State(o): State<Arc<Oauth>>,
    Extension(ClientAddr(ip)): Extension<ClientAddr>,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    if !o.login_limiter.allow(ip, "oauth-authorize", o.now_ms()) {
        let mut r = login::error_page(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many sign-in requests. Wait a minute.",
        );
        r.headers_mut()
            .insert(header::RETRY_AFTER, "60".parse().unwrap());
        return r;
    }
    // Errors about the client or the redirect URI are never redirected.
    let Some(client_id) = q.client_id.as_deref() else {
        return login::error_page(StatusCode::BAD_REQUEST, "The request names no client.");
    };
    let client = match o.clients.get(client_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return login::error_page(
                StatusCode::BAD_REQUEST,
                "This client is not registered here.",
            );
        }
        Err(e) => {
            eprintln!("notedown mcp: client lookup failed: {e}");
            return login::error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "The server cannot read its clients right now.",
            );
        }
    };
    let redirect_uri = match q.redirect_uri.as_deref() {
        Some(u) if client.redirect_uris.iter().any(|r| r == u) => u.to_string(),
        None => {
            return login::error_page(
                StatusCode::BAD_REQUEST,
                "The request names no redirect URI.",
            );
        }
        Some(_) => {
            return login::error_page(
                StatusCode::BAD_REQUEST,
                "The redirect URI is not registered for this client.",
            );
        }
    };
    let state = q.state.as_deref();
    if state.is_some_and(|s| s.len() > MAX_STATE_LEN) {
        return login::error_page(
            StatusCode::BAD_REQUEST,
            "The state is longer than 2048 characters.",
        );
    }
    if q.response_type.as_deref() != Some("code") {
        return redirect_error(
            &o,
            &redirect_uri,
            state,
            "unsupported_response_type",
            "only response_type=code",
        );
    }
    let challenge = match q.code_challenge.as_deref() {
        Some(c) if secret::pkce_text_ok(c) => c.to_string(),
        _ => {
            return redirect_error(
                &o,
                &redirect_uri,
                state,
                "invalid_request",
                "a code_challenge of 43 to 128 characters is required",
            );
        }
    };
    if q.code_challenge_method.as_deref() != Some("S256") {
        return redirect_error(
            &o,
            &redirect_uri,
            state,
            "invalid_request",
            "code_challenge_method must be S256",
        );
    }
    let resource = o.base.resource();
    if q.resource
        .as_deref()
        .is_some_and(|r| !o.base.is_resource(r))
    {
        return redirect_error(
            &o,
            &redirect_uri,
            state,
            "invalid_target",
            "the resource is not this server's MCP endpoint",
        );
    }
    // `scope` is ignored: a grant is always the one scope `notes`.
    let now = o.now_ms();
    let pending = Pending {
        client_id: client.id.clone(),
        client_name: client.name.clone(),
        client_uri: client.uri.clone(),
        redirect_uri,
        state: q.state.clone(),
        challenge,
        resource,
        created_ms: now,
    };
    match o.pending.insert(pending.clone(), now) {
        Ok(id) => login::render(&page_of(&pending, &id, now, None, "")),
        Err(_) => login::error_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many sign-ins are waiting. Try again in a few minutes.",
        ),
    }
}

/// `scheme://host[:port]` of a validated redirect URI, for the CSP `form-action`. Empty when the
/// result holds anything but ASCII letters, digits and `:/.-[]`.
fn origin_of(uri: &str) -> String {
    let Some((scheme, _)) = uri.split_once("://") else {
        return String::new();
    };
    let scheme = scheme.to_ascii_lowercase();
    let authority = redirect::host_of(uri);
    // A CSP host-source cannot hold an IPv6 literal: allow the whole scheme.
    let origin = if authority.starts_with('[') {
        format!("{scheme}:")
    } else {
        format!("{scheme}://{authority}")
    };
    if origin
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ":/.-[]".contains(c))
    {
        origin
    } else {
        String::new()
    }
}

fn page_of(p: &Pending, id: &str, now: u64, error: Option<&str>, email: &str) -> Page {
    let left = PENDING_TTL_MS.saturating_sub(now.saturating_sub(p.created_ms));
    Page {
        client_name: p.client_name.clone(),
        client_uri: p.client_uri.clone(),
        redirect_host: redirect::host_of(&p.redirect_uri).to_string(),
        redirect_origin: origin_of(&p.redirect_uri),
        loopback: redirect::is_loopback(&p.redirect_uri),
        pending_id: id.to_string(),
        minutes_left: left.div_ceil(60_000),
        error: error.map(str::to_string),
        email: email.to_string(),
    }
}

#[derive(Deserialize)]
pub(crate) struct LoginForm {
    pending: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    action: String,
}

pub(crate) async fn submit(
    State(o): State<Arc<Oauth>>,
    Extension(ClientAddr(ip)): Extension<ClientAddr>,
    Form(f): Form<LoginForm>,
) -> Response {
    let now = o.now_ms();
    if !o.login_limiter.allow(ip, "oauth-login", now) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            "too many sign-in attempts, wait a minute\n",
        )
            .into_response();
    }
    let Some(p) = o.pending.get(&f.pending, now) else {
        return login::error_page(
            StatusCode::BAD_REQUEST,
            "This sign-in request is unknown or expired. Start the sign-in again in your client.",
        );
    };
    if f.action == "cancel" {
        if o.pending.take(&f.pending, now).is_none() {
            return login::error_page(
                StatusCode::BAD_REQUEST,
                "This sign-in request is unknown or expired. Start the sign-in again in your client.",
            );
        }
        return redirect_error(
            &o,
            &p.redirect_uri,
            p.state.as_deref(),
            "access_denied",
            "the sign-in was cancelled",
        );
    }
    match o.throttle.attempt(&f.email, now) {
        Verdict::Go => {}
        Verdict::Wait(d) => tokio::time::sleep(d).await,
        Verdict::Refuse { retry_after_s } => {
            let mut r = login::render(&page_of(
                &p,
                &f.pending,
                now,
                Some("Too many wrong passwords. Wait 15 minutes."),
                &f.email,
            ));
            *r.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            r.headers_mut()
                .insert(header::RETRY_AFTER, retry_after_s.into());
            return r;
        }
    }
    let accounts = o.accounts.clone();
    let (email, password) = (f.email.clone(), f.password.clone());
    let checked =
        tokio::task::spawn_blocking(move || login::check_password(&accounts, &email, &password))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r);
    let now = o.now_ms();
    let account = match checked {
        Ok(a) => a,
        Err(e) => {
            eprintln!("notedown mcp: password check failed: {e}");
            return login::error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "The server cannot read its accounts right now.",
            );
        }
    };
    let Some(account) = account else {
        let mut r = login::render(&page_of(&p, &f.pending, now, Some(WRONG), &f.email));
        *r.status_mut() = StatusCode::UNAUTHORIZED;
        return r;
    };
    o.throttle.succeeded(&f.email);
    let Some(p) = o.pending.take(&f.pending, now) else {
        return login::error_page(
            StatusCode::BAD_REQUEST,
            "This sign-in request was already used.",
        );
    };
    let code = o.codes.issue(Code {
        slug: account.slug,
        client_id: p.client_id,
        redirect_uri: p.redirect_uri.clone(),
        challenge: p.challenge,
        resource: p.resource,
        expires_ms: now + CODE_TTL_MS,
    });
    let mut params = vec![("code", code.as_str())];
    if let Some(s) = p.state.as_deref() {
        params.push(("state", s));
    }
    params.push(("iss", &o.base.origin));
    found(redirect::with_params(&p.redirect_uri, &params))
}

#[cfg(test)]
mod tests {
    use super::origin_of;

    #[test]
    fn the_origin_is_lowercased_and_guarded() {
        assert_eq!(
            origin_of("HTTPS://claude.ai/api/mcp/auth_callback"),
            "https://claude.ai"
        );
        assert_eq!(origin_of("http://[::1]:8080/cb"), "http:");
        assert_eq!(origin_of("https://a.example;b/cb"), "");
        assert_eq!(origin_of("nonsense"), "");
    }
}
