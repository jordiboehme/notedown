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

pub mod base;
mod discovery;
mod redirect;
mod secret;

use base::Base;

/// The one scope: read the notes and move the bookmarks.
pub const SCOPE: &str = "notes";

/// What the OAuth handlers and the gate share.
#[allow(dead_code)] // `data` is used by the tasks that follow
pub(crate) struct Oauth {
    pub(crate) base: Base,
    pub(crate) data: PathBuf,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) trusted: Vec<IpAddr>,
}

impl Oauth {
    #[allow(dead_code)] // used by the tasks that follow
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
    });
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
        .with_state(oauth.clone())
        .layer(middleware::from_fn_with_state(oauth, check_host));
    Ok(Some(routes))
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
