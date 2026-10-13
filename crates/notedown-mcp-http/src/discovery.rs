//! RFC 9728 and RFC 8414 documents.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::Oauth;

pub async fn protected_resource(State(o): State<Arc<Oauth>>) -> Json<Value> {
    Json(json!({
        "resource": o.base.resource(),
        "authorization_servers": [o.base.origin],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [crate::SCOPE],
        "resource_name": "notedown",
    }))
}

pub async fn authorization_server(State(o): State<Arc<Oauth>>) -> Json<Value> {
    let b = &o.base;
    Json(json!({
        "issuer": b.origin,
        "authorization_endpoint": b.url("/oauth/authorize"),
        "token_endpoint": b.url("/oauth/token"),
        "registration_endpoint": b.url("/oauth/register"),
        "revocation_endpoint": b.url("/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [crate::SCOPE],
        "authorization_response_iss_parameter_supported": true,
    }))
}
