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
    assert_eq!(r.headers()["retry-after"], "600");
    s.stop().await;
}

#[tokio::test]
async fn register_refuses_odd_client_uris() {
    let s = start().await;
    for uri in [
        "https://a b.example",
        "https://a.example/\nx",
        "https://user@a.example/",
        "http://a.example/",
    ] {
        let r = s
            .client
            .post(s.url("/oauth/register"))
            .json(&serde_json::json!({"redirect_uris": [REDIRECT], "client_uri": uri}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{uri:?}");
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["error"], "invalid_client_metadata", "{uri:?}");
    }
    s.stop().await;
}

#[tokio::test]
async fn register_strips_invisible_characters_from_the_name() {
    let s = start().await;
    let r = s
        .client
        .post(s.url("/oauth/register"))
        .json(&serde_json::json!({"redirect_uris": [REDIRECT], "client_name": "Cl\u{202E}au\u{0007}de\u{200B}\n"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["client_name"], "Claude");
    s.stop().await;
}

#[tokio::test]
async fn a_corrupt_client_file_does_not_break_registration() {
    let s = start().await;
    register(&s, REDIRECT).await;
    std::fs::write(
        s.dir
            .path()
            .join("oauth/clients")
            .join(format!("ndc_{}.json", "e".repeat(32))),
        b"garbage",
    )
    .unwrap();
    register(&s, REDIRECT).await;
    s.stop().await;
}
