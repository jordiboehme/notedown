//! The MCP tools of `notedown mcp` over HTTP at `<external_base_url>/mcp`, protected by OAuth
//! 2.1 and signed in with a notedown account (spec
//! `docs/superpowers/specs/2026-10-13-notedown-mcp-http-oauth-design.md`).

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use notedown_cloud::Clock;
use notedown_cloud::server::AppState;

mod authorize;
pub mod base;
mod clients;
mod discovery;
mod gate;
mod grants;
mod login;
mod pending;
mod redirect;
mod secret;
mod session;
mod throttle;
mod token;

use base::Base;
pub use gate::McpAccount;
pub use grants::{GrantSummary, list_grants, revoke_grants};

/// The one scope: read the notes and move the bookmarks.
pub const SCOPE: &str = "notes";

pub(crate) const MAX_REGISTER_BYTES: usize = 64 * 1024;
pub(crate) const MAX_FORM_BYTES: usize = 8 * 1024;

/// What the OAuth handlers and the gate share.
pub(crate) struct Oauth {
    pub(crate) base: Base,
    pub(crate) data: PathBuf,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) trusted: Vec<IpAddr>,
    pub(crate) clients: clients::ClientStore,
    pub(crate) registrations: clients::RegistrationLimiter,
    pub(crate) pending: pending::PendingStore,
    pub(crate) codes: pending::CodeStore,
    pub(crate) throttle: throttle::Throttle,
    pub(crate) login_limiter: notedown_cloud::api::limit::RateLimiter,
    pub(crate) accounts: notedown_cloud::auth::Accounts,
    pub(crate) grants: grants::GrantStore,
    last_sweep: std::sync::atomic::AtomicU64,
}

impl Oauth {
    /// The client ids that hold a grant (pruning keeps them).
    pub(crate) fn grants_in_use(&self) -> std::collections::HashSet<String> {
        self.grants.client_ids()
    }

    /// True at most once an hour: a sweep of expired grants and old unused clients is due.
    pub(crate) fn sweep_due(&self, now_ms: u64) -> bool {
        use std::sync::atomic::Ordering;
        let last = self.last_sweep.load(Ordering::Relaxed);
        now_ms.saturating_sub(last) >= 3_600_000
            && self
                .last_sweep
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }

    /// Deletes expired grants and old unused clients (blocking file work).
    pub(crate) fn sweep(&self, now_ms: u64) {
        if let Err(e) = self.grants.sweep(now_ms) {
            eprintln!("notedown mcp: grant sweep failed: {e}");
        }
        if let Err(e) = self.clients.prune(now_ms, &self.grants_in_use()) {
            eprintln!("notedown mcp: client prune failed: {e}");
        }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        let ns = self.clock.now().unix_timestamp_nanos();
        u64::try_from(ns / 1_000_000).unwrap_or(0)
    }
}

/// The router of the MCP endpoint and its OAuth routes, or `None` when they are not served:
/// `external_base_url` is not `https://`, or `[mcp] enabled = false`.
pub fn router(state: &Arc<AppState>) -> anyhow::Result<Option<Router>> {
    let config = &state.config;
    if !config.mcp.enabled {
        return Ok(None);
    }
    let Some(base) = config.external_base_url.as_deref().and_then(Base::parse) else {
        return Ok(None);
    };
    let oauth = Arc::new(Oauth {
        base,
        data: state.data_dir.clone(),
        clock: state.clock.clone(),
        trusted: config.trusted_proxies.clone(),
        clients: clients::ClientStore::new(&state.data_dir),
        registrations: Default::default(),
        pending: Default::default(),
        codes: Default::default(),
        throttle: Default::default(),
        login_limiter: Default::default(),
        accounts: notedown_cloud::auth::Accounts::new(&state.data_dir),
        grants: grants::GrantStore::open(&state.data_dir)?,
        last_sweep: Default::default(),
    });
    let mcp_routes = mcp_routes(&oauth);
    let routes = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(discovery::protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(discovery::protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(discovery::authorization_server),
        )
        .route(
            "/oauth/register",
            axum::routing::post(clients::register)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_REGISTER_BYTES)),
        )
        .route(
            "/oauth/authorize",
            get(authorize::show)
                .post(authorize::submit)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_FORM_BYTES)),
        )
        .route(
            "/oauth/token",
            axum::routing::post(token::token)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_FORM_BYTES)),
        )
        .route(
            "/oauth/revoke",
            axum::routing::post(token::revoke)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_FORM_BYTES)),
        )
        // Nothing else under these prefixes falls through to the device routes.
        .route("/oauth/{*rest}", axum::routing::any(not_found))
        .route("/.well-known/{*rest}", axum::routing::any(not_found))
        .with_state(oauth.clone())
        .merge(mcp_routes)
        .layer(middleware::from_fn_with_state(oauth, check_host));
    Ok(Some(routes))
}

/// `/mcp`: rmcp's streamable HTTP service behind the bearer gate.
fn mcp_routes(oauth: &Arc<Oauth>) -> Router {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    let mut manager = LocalSessionManager::default();
    // No SSE priming frame (strict SSE parsers refuse it).
    manager.session_config.sse_retry = None;
    let manager = Arc::new(manager);
    let data = oauth.data.clone();
    let servers = session::Servers::default();
    let mcp = StreamableHttpService::new(
        move || Ok(session::AccountSession::new(data.clone(), servers.clone())),
        manager.clone(),
        StreamableHttpServerConfig::default()
            .with_sse_retry(None)
            .with_legacy_session_mode(true)
            // The Host check of this router replaces rmcp's loopback-only default.
            .disable_allowed_hosts(),
    );
    let gate = Arc::new(gate::Gate {
        oauth: oauth.clone(),
        owners: Default::default(),
        sessions: manager,
    });
    Router::new()
        .route_service("/mcp", mcp.clone())
        .route_service("/mcp/", mcp)
        .layer(middleware::from_fn_with_state(gate, gate::gate))
}

/// A plain 404 for an unknown path under `/oauth/` or `/.well-known/`.
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        "not found\n",
    )
        .into_response()
}

/// 421 for a request not addressed to the base (a wrong `Host`).
async fn check_host(State(o): State<Arc<Oauth>>, request: Request, next: Next) -> Response {
    if !base::host_ok(&o.base, &request, &o.trusted) {
        return (
            StatusCode::MISDIRECTED_REQUEST,
            "this server answers only for its own name\n",
        )
            .into_response();
    }
    next.run(request).await
}

/// An OAuth error answer (RFC 6749 5.2): JSON `error` and `error_description`, never cached.
pub(crate) fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        axum::Json(serde_json::json!({"error": error, "error_description": description})),
    )
        .into_response()
}
