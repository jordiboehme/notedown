use crate::common::*;

#[tokio::test]
async fn register_stores_a_client_and_refuses_bad_metadata() {
    let s = start().await;
    let id = register(&s, REDIRECT).await;
    assert!(id.starts_with("ndc_") && id.len() == 36, "{id}");
    assert!(
        s.dir
            .path()
            .join("oauth/clients")
            .join(format!("{id}.json"))
            .exists()
    );

    for (body, why) in [
        (
            serde_json::json!({"redirect_uris": ["http://claude.ai/cb"]}),
            "http",
        ),
        (serde_json::json!({"redirect_uris": []}), "empty"),
        (
            serde_json::json!({"redirect_uris": [REDIRECT], "token_endpoint_auth_method": "client_secret_basic"}),
            "secret",
        ),
        (
            serde_json::json!({"redirect_uris": [REDIRECT], "grant_types": ["client_credentials"]}),
            "grant",
        ),
        (
            serde_json::json!({"redirect_uris": [REDIRECT], "response_types": ["token"]}),
            "response",
        ),
    ] {
        let r = s
            .client
            .post(s.url("/oauth/register"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{why}");
        let v: serde_json::Value = r.json().await.unwrap();
        assert!(
            v["error"] == "invalid_redirect_uri" || v["error"] == "invalid_client_metadata",
            "{why}: {v}"
        );
    }
    s.stop().await;
}

#[tokio::test]
async fn register_ignores_unknown_fields() {
    let s = start().await;
    let r = s
        .client
        .post(s.url("/oauth/register"))
        .json(
            &serde_json::json!({"redirect_uris": [REDIRECT], "software_id": "x", "scope": "notes"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    s.stop().await;
}

#[tokio::test]
async fn register_is_limited_to_30_per_10_minutes() {
    let s = start().await;
    for _ in 0..30 {
        register(&s, REDIRECT).await;
    }
    let r = s
        .client
        .post(s.url("/oauth/register"))
        .json(&serde_json::json!({"redirect_uris": [REDIRECT]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    s.stop().await;
}
