use crate::common::*;

#[tokio::test]
async fn discovery_documents_name_the_base() {
    let s = start().await;
    let r = s
        .client
        .get(s.url("/.well-known/oauth-protected-resource/mcp"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["resource"], format!("{BASE}/mcp"));
    assert_eq!(v["authorization_servers"][0], BASE);
    let r = s
        .client
        .get(s.url("/.well-known/oauth-protected-resource"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = s
        .client
        .get(s.url("/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap();
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["issuer"], BASE);
    assert_eq!(v["token_endpoint"], format!("{BASE}/oauth/token"));
    assert_eq!(v["code_challenge_methods_supported"][0], "S256");
    assert!(r_has_no_cors(&s).await);
    s.stop().await;
}

async fn r_has_no_cors(s: &Server) -> bool {
    let r = s
        .client
        .get(s.url("/.well-known/oauth-authorization-server"))
        .header("origin", "https://claude.ai")
        .send()
        .await
        .unwrap();
    r.headers().get("access-control-allow-origin").is_none()
}

#[tokio::test]
async fn a_wrong_host_gets_421() {
    let s = start().await;
    let r = s
        .client
        .get(s.url("/.well-known/oauth-authorization-server"))
        .header("x-forwarded-host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 421);
    s.stop().await;
}

#[tokio::test]
async fn without_https_nothing_new_is_served() {
    for edit in [
        (|c: &mut notedown_cloud::Config| c.external_base_url = Some("http://n.example".into()))
            as fn(&mut notedown_cloud::Config),
        |c| c.external_base_url = None,
        |c| c.mcp.enabled = false,
    ] {
        let s = start_with(edit).await;
        for path in [
            "/mcp",
            "/oauth/register",
            "/.well-known/oauth-authorization-server",
        ] {
            let r = s.client.get(s.url(path)).send().await.unwrap();
            // The device catch-all answers unknown paths; it is never a discovery document.
            let text = r.text().await.unwrap();
            assert!(!text.contains("authorization_endpoint"), "{path}: {text}");
            assert!(!text.contains("resource_metadata"), "{path}: {text}");
        }
        let r = s
            .client
            .get(s.url("/api/file/query/server"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        s.stop().await;
    }
}
