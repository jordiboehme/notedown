//! The bearer gate in front of `/mcp`: a valid access token for this resource, and a session
//! bound to the account that opened it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use notedown_cloud::api::envelope::Streamed;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use crate::Oauth;

/// The account of the bearer token, for the session handler.
#[derive(Debug, Clone)]
pub struct McpAccount(pub String);

const SESSION: &str = "mcp-session-id";
const MAX_SESSIONS: usize = 1000;

/// Session id to the account that opened it.
#[derive(Default)]
pub(crate) struct SessionOwners(Mutex<HashMap<String, String>>);

pub(crate) struct Gate {
    pub(crate) oauth: Arc<Oauth>,
    pub(crate) owners: SessionOwners,
    pub(crate) sessions: Arc<LocalSessionManager>,
}

fn bearer(request: &Request) -> Option<&str> {
    let v = request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

fn unauthorized(o: &Oauth) -> Response {
    let challenge = format!("Bearer resource_metadata=\"{}\"", o.base.metadata_url());
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_str(&challenge).unwrap_or(HeaderValue::from_static("Bearer")),
        )],
        axum::Json(serde_json::json!({
            "error": "invalid_token",
            "error_description": "sign in to use this MCP server",
        })),
    )
        .into_response()
}

fn unavailable(e: impl std::fmt::Display) -> Response {
    eprintln!("notedown mcp: token check failed: {e}");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "the grants cannot be read right now\n",
    )
        .into_response()
}

pub(crate) async fn gate(State(g): State<Arc<Gate>>, mut request: Request, next: Next) -> Response {
    let o = &g.oauth;
    let Some(token) = bearer(&request).map(str::to_string) else {
        return unauthorized(o);
    };
    let (oauth, resource, now) = (o.clone(), o.base.resource(), o.now_ms());
    let checked =
        tokio::task::spawn_blocking(move || oauth.grants.access(&token, &resource, now)).await;
    let slug = match checked {
        Ok(Ok(Some(slug))) => slug,
        Ok(Ok(None)) => return unauthorized(o),
        Ok(Err(e)) => return unavailable(e),
        Err(e) => return unavailable(e),
    };
    let session = request
        .headers()
        .get(SESSION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(id) = &session {
        let owner = g
            .owners
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        if owner.is_some_and(|owner| owner != slug) {
            return (
                StatusCode::FORBIDDEN,
                "this session belongs to another account; start a new session\n",
            )
                .into_response();
        }
    }
    request.extensions_mut().insert(McpAccount(slug.clone()));
    let mut response = next.run(request).await;
    if session.is_none()
        && let Some(id) = response
            .headers()
            .get(SESSION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    {
        g.claim(id, slug).await;
    }
    // SSE answers stream; the request log must not buffer them.
    response.extensions_mut().insert(Streamed);
    response
}

impl Gate {
    /// Binds session `id` to account `slug`. At the cap, the ids of closed sessions are dropped.
    async fn claim(&self, id: String, slug: String) {
        use rmcp::transport::streamable_http_server::session::SessionManager;
        let full = self
            .owners
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            >= MAX_SESSIONS;
        if full {
            let ids: Vec<String> = self
                .owners
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .cloned()
                .collect();
            let mut gone = Vec::new();
            for sid in ids {
                if !self
                    .sessions
                    .has_session(&sid.clone().into())
                    .await
                    .unwrap_or(false)
                {
                    gone.push(sid);
                }
            }
            let mut map = self.owners.0.lock().unwrap_or_else(|e| e.into_inner());
            for sid in gone {
                map.remove(&sid);
            }
        }
        self.owners
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, slug);
    }
}
