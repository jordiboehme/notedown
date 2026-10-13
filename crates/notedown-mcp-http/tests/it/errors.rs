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

async fn register_from(s: &Server, ip: &str) -> reqwest::Response {
    s.client
        .post(s.url("/oauth/register"))
        .header("x-forwarded-for", ip)
        .json(&serde_json::json!({"redirect_uris": [REDIRECT]}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn register_is_limited_to_10_per_minute_per_ip() {
    let s = start().await;
    for _ in 0..10 {
        register(&s, REDIRECT).await;
    }
    let r = register_from(&s, "127.0.0.1").await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "60");
    // Another address is still served.
    assert_eq!(register_from(&s, "192.0.2.9").await.status(), 201);
    s.stop().await;
}

#[tokio::test]
async fn register_is_limited_to_30_per_10_minutes() {
    let s = start().await;
    // 127.0.0.1 is a trusted proxy here, so X-Forwarded-For names the caller: three addresses
    // stay under the per-address limit and fill the global one.
    for ip in ["192.0.2.1", "192.0.2.2", "192.0.2.3"] {
        for _ in 0..10 {
            assert_eq!(register_from(&s, ip).await.status(), 201);
        }
    }
    let r = register_from(&s, "192.0.2.4").await;
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

fn fill_clients(s: &Server, age_ms: u64) {
    let dir = s.dir.path().join("oauth/clients");
    std::fs::create_dir_all(&dir).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let at = now_ms - age_ms;
    for i in 0..1000 {
        let id = format!("ndc_{i:032x}");
        let c = serde_json::json!({
            "id": id, "name": "x", "redirect_uris": ["https://a.example/cb"],
            "created_ms": at, "used_ms": at,
        });
        std::fs::write(dir.join(format!("{id}.json")), c.to_string()).unwrap();
    }
}

/// A full store of fresh clients that never got a grant: nothing can be pruned.
#[tokio::test]
async fn a_full_client_store_answers_503() {
    let s = start().await;
    fill_clients(&s, 0);
    let r = s
        .client
        .post(s.url("/oauth/register"))
        .json(&serde_json::json!({"redirect_uris": [REDIRECT]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"], "temporarily_unavailable");
    s.stop().await;
}

#[tokio::test]
async fn a_full_store_of_never_used_clients_older_than_10_minutes_takes_a_new_one() {
    let s = start().await;
    fill_clients(&s, 11 * 60 * 1000);
    let r = s
        .client
        .post(s.url("/oauth/register"))
        .json(&serde_json::json!({"redirect_uris": [REDIRECT]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    s.stop().await;
}

/// At the cap, a never-used client in the middle of a sign-in (a pending login) is kept.
#[tokio::test]
async fn at_the_cap_a_client_with_a_pending_login_is_not_pruned() {
    let s = start().await;
    fill_clients(&s, 11 * 60 * 1000);
    let file = s
        .dir
        .path()
        .join(format!("oauth/clients/ndc_{:032x}.json", 0));
    let mut c: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    c["redirect_uris"] = serde_json::json!([REDIRECT]);
    std::fs::write(&file, c.to_string()).unwrap();
    let id = c["id"].as_str().unwrap();
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(id, &[]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    pending_id_of(&r.text().await.unwrap());
    let r = s
        .client
        .post(s.url("/oauth/register"))
        .json(&serde_json::json!({"redirect_uris": [REDIRECT]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert!(file.exists(), "the client in a sign-in was pruned");
    s.stop().await;
}

#[tokio::test]
async fn authorize_refuses_without_redirecting_for_an_unknown_client_or_redirect() {
    let s = start().await;
    let id = register(&s, REDIRECT).await;
    for q in [
        authorize_query("ndc_00000000000000000000000000000000", &[]),
        authorize_query(
            &id,
            &[("redirect_uri", "https://claude.ai/api/mcp/auth_callback/")],
        ),
        authorize_query(&id, &[("redirect_uri", "https://evil.example/cb")]),
    ] {
        let r = s
            .client
            .get(s.url("/oauth/authorize"))
            .query(&q)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert!(r.headers().get("location").is_none());
    }
    s.stop().await;
}

#[tokio::test]
async fn authorize_redirects_other_errors_with_state_and_iss() {
    let s = start().await;
    let id = register(&s, REDIRECT).await;
    for (extra, error) in [
        (
            vec![("response_type", "token")],
            "unsupported_response_type",
        ),
        (vec![("code_challenge", "")], "invalid_request"),
        (vec![("code_challenge_method", "plain")], "invalid_request"),
        (
            vec![("resource", "https://other.example/mcp")],
            "invalid_target",
        ),
    ] {
        let r = s
            .client
            .get(s.url("/oauth/authorize"))
            .query(&authorize_query(&id, &extra))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302, "{error}");
        let p = location_params(&r);
        assert_eq!(p["error"], error);
        assert_eq!(p["state"], "st-1");
        assert_eq!(p["iss"], BASE);
    }
    s.stop().await;
}

#[tokio::test]
async fn a_wrong_password_shows_the_form_again_and_cancel_denies() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(&id, &[]))
        .send()
        .await
        .unwrap();
    let pending = pending_id_of(&r.text().await.unwrap());
    let r = s
        .client
        .post(s.url("/oauth/authorize"))
        .form(&[
            ("pending", pending.as_str()),
            ("email", "anna@example.com"),
            ("password", "wrong"),
            ("action", "login"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let html = r.text().await.unwrap();
    assert!(html.contains("email or password wrong"));
    assert_eq!(
        pending_id_of(&html),
        pending,
        "the same pending login stays"
    );
    let r = s
        .client
        .post(s.url("/oauth/authorize"))
        .form(&[("pending", pending.as_str()), ("action", "cancel")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(location_params(&r)["error"], "access_denied");
    // Used once: the pending id is gone.
    let r = s
        .client
        .post(s.url("/oauth/authorize"))
        .form(&[
            ("pending", pending.as_str()),
            ("email", "anna@example.com"),
            ("password", "right"),
            ("action", "login"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    s.stop().await;
}

#[tokio::test]
async fn the_web_throttle_never_touches_the_device_lockout() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(&id, &[]))
        .send()
        .await
        .unwrap();
    let pending = pending_id_of(&r.text().await.unwrap());
    let mut statuses = Vec::new();
    // 3 free failures, then the first delayed one (1 s) is enough to see the counter untouched.
    for _ in 0..4 {
        let r = s
            .client
            .post(s.url("/oauth/authorize"))
            .form(&[
                ("pending", pending.as_str()),
                ("email", "anna@example.com"),
                ("password", "wrong"),
                ("action", "login"),
            ])
            .send()
            .await
            .unwrap();
        statuses.push(r.status().as_u16());
    }
    assert_eq!(statuses, [401, 401, 401, 401]);
    let a = notedown_cloud::auth::Accounts::new(s.dir.path())
        .get("anna")
        .unwrap()
        .unwrap();
    assert_eq!(a.failed_logins, 0);
    assert!(!a.locked);
    s.stop().await;
}

#[tokio::test]
async fn login_posts_are_limited_per_ip() {
    let s = start().await;
    for i in 1..=11 {
        let r = s
            .client
            .post(s.url("/oauth/authorize"))
            .form(&[
                ("pending", "nope"),
                ("email", "x@example.com"),
                ("password", "p"),
                ("action", "login"),
            ])
            .send()
            .await
            .unwrap();
        if i <= 10 {
            assert_ne!(r.status(), 429, "post {i}");
        } else {
            assert_eq!(r.status(), 429);
        }
    }
    s.stop().await;
}

#[tokio::test]
async fn authorize_pages_are_limited_per_ip() {
    let s = start().await;
    let id = register(&s, REDIRECT).await;
    for i in 1..=11 {
        let r = s
            .client
            .get(s.url("/oauth/authorize"))
            .query(&authorize_query(&id, &[]))
            .send()
            .await
            .unwrap();
        if i <= 10 {
            assert_eq!(r.status(), 200, "get {i}");
        } else {
            assert_eq!(r.status(), 429);
            assert_eq!(r.headers()["retry-after"], "60");
            assert!(
                r.text()
                    .await
                    .unwrap()
                    .contains("Too many sign-in requests. Wait a minute.")
            );
        }
    }
    s.stop().await;
}

#[tokio::test]
async fn authorize_requires_the_redirect_uri() {
    let s = start().await;
    let id = register(&s, REDIRECT).await;
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(&id, &[("redirect_uri", "")]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.headers().get("location").is_none());
    assert!(r.text().await.unwrap().contains("names no redirect URI"));
    s.stop().await;
}

#[tokio::test]
async fn the_right_password_redirects_with_a_code_once() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(&id, &[]))
        .send()
        .await
        .unwrap();
    let pending = pending_id_of(&r.text().await.unwrap());
    let form = [
        ("pending", pending.as_str()),
        ("email", "anna@example.com"),
        ("password", "right"),
        ("action", "login"),
    ];
    let r = s
        .client
        .post(s.url("/oauth/authorize"))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    assert!(
        r.headers()["location"]
            .to_str()
            .unwrap()
            .starts_with(REDIRECT)
    );
    let p = location_params(&r);
    assert!(!p["code"].is_empty());
    assert_eq!(p["state"], "st-1");
    assert_eq!(p["iss"], BASE);
    let r = s
        .client
        .post(s.url("/oauth/authorize"))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    // The helper walks the same path.
    assert!(
        !sign_in(&s, &id, "anna@example.com", "right")
            .await
            .is_empty()
    );
    s.stop().await;
}

#[tokio::test]
async fn the_eighth_attempt_is_refused_with_retry_after() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(&id, &[]))
        .send()
        .await
        .unwrap();
    let pending = pending_id_of(&r.text().await.unwrap());
    // 3 free, then waits of 1, 2, 4, 8 s: about 15 s in all.
    let mut statuses = Vec::new();
    for _ in 0..7 {
        let r = s
            .client
            .post(s.url("/oauth/authorize"))
            .form(&[
                ("pending", pending.as_str()),
                ("email", "anna@example.com"),
                ("password", "wrong"),
                ("action", "login"),
            ])
            .send()
            .await
            .unwrap();
        statuses.push(r.status().as_u16());
    }
    assert_eq!(statuses, [401; 7]);
    let r = s
        .client
        .post(s.url("/oauth/authorize"))
        .form(&[
            ("pending", pending.as_str()),
            ("email", "anna@example.com"),
            ("password", "right"),
            ("action", "login"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    let secs: u64 = r.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(secs > 0 && secs <= 900);
    let html = r.text().await.unwrap();
    assert!(html.contains("Too many wrong passwords. Wait 15 minutes."));
    s.stop().await;
}

#[tokio::test]
async fn every_bad_code_exchange_is_the_same_invalid_grant() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let other = register(&s, REDIRECT).await;
    // (client, redirect_uri, verifier); None leaves redirect_uri out.
    for (client, redirect, verifier) in [
        (other.as_str(), Some(REDIRECT), VERIFIER),
        (id.as_str(), Some("https://claude.ai/other"), VERIFIER),
        (id.as_str(), None, VERIFIER),
        (
            id.as_str(),
            Some(REDIRECT),
            "dBjftJeZ4CVP-mJ92K28uhbUJU1p1r_wW1gFWFOEjXX",
        ),
    ] {
        let code = sign_in(&s, &id, "anna@example.com", "right").await;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", client),
            ("code_verifier", verifier),
        ];
        if let Some(r) = redirect {
            form.push(("redirect_uri", r));
        }
        let r = s
            .client
            .post(s.url("/oauth/token"))
            .form(&form)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["error"], "invalid_grant");
    }
    s.stop().await;
}

#[tokio::test]
async fn a_wrong_resource_at_the_token_endpoint_is_invalid_target() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let code = sign_in(&s, &id, "anna@example.com", "right").await;
    let r = s
        .client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", id.as_str()),
            ("code_verifier", VERIFIER),
            ("resource", "https://x.example/mcp"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"], "invalid_target");
    s.stop().await;
}

#[tokio::test]
async fn mcp_without_a_good_token_is_401_with_resource_metadata() {
    let s = start().await;
    for auth in [None, Some("Bearer nda_nope"), Some("Basic abc")] {
        let mut r = s
            .client
            .post(s.url("/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        if let Some(a) = auth {
            r = r.header("authorization", a);
        }
        let r = r.send().await.unwrap();
        assert_eq!(r.status(), 401);
        assert_eq!(
            r.headers()["www-authenticate"],
            "Bearer resource_metadata=\"https://notedown.example/.well-known/oauth-protected-resource/mcp\""
        );
    }
    s.stop().await;
}

#[tokio::test]
async fn stateless_requests_run_on_the_account_of_each_token() {
    let s = start().await;
    let (_, anna) = signed_in(&s, "anna").await;
    let (_, ben) = signed_in(&s, "ben").await;
    for (token, me, other) in [(&anna, "anna", "ben"), (&ben, "ben", "anna")] {
        let r = s
            .client
            .post(s.url("/mcp"))
            .header("authorization", format!("Bearer {token}"))
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/call")
            .header("mcp-name", "status")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            // A stateless request carries its method and name as headers (SEP-2243), and its
            // protocol version and client capabilities in `_meta`.
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#)
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body = r.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(me) && !body.contains(other), "{body}");
    }
    s.stop().await;
}

#[tokio::test]
async fn a_legacy_session_works_for_its_account_and_is_403_for_another() {
    let s = start().await;
    let (_, anna) = signed_in(&s, "anna").await;
    let (_, ben) = signed_in(&s, "ben").await;
    // Open a session as anna with a raw initialize to read the session id.
    let r = s
        .client
        .post(s.url("/mcp"))
        .header("authorization", format!("Bearer {anna}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let session = r.headers()["mcp-session-id"].to_str().unwrap().to_string();
    let post = |token: &str, body: &'static str| {
        s.client
            .post(s.url("/mcp"))
            .header("authorization", format!("Bearer {token}"))
            .header("mcp-session-id", &session)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body)
    };
    // Anna's own legacy session works.
    let r = post(
        &anna,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .send()
    .await
    .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let r = post(
        &anna,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("anna"), "{body}");
    // Ben's token on Anna's session is refused.
    let r = post(
        &ben,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 403);
    s.stop().await;
}

#[tokio::test]
async fn any_scope_is_ignored_at_authorize() {
    let s = start().await;
    let id = register(&s, REDIRECT).await;
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(
            &id,
            &[("scope", "claudeai offline_access")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("name=\"pending\""));
    s.stop().await;
}

#[tokio::test]
async fn mcp_with_a_trailing_slash_is_the_same_gate() {
    let s = start().await;
    let r = s
        .client
        .post(s.url("/mcp/"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.headers()["www-authenticate"],
        "Bearer resource_metadata=\"https://notedown.example/.well-known/oauth-protected-resource/mcp\""
    );
    s.stop().await;
}

#[tokio::test]
async fn unknown_oauth_and_well_known_paths_are_404() {
    let s = start().await;
    for path in [
        "/.well-known/openid-configuration",
        "/oauth/x",
        "/mcp/x",
        "/mcp/a/b",
    ] {
        for post in [false, true] {
            let r = if post {
                s.client.post(s.url(path))
            } else {
                s.client.get(s.url(path))
            }
            .send()
            .await
            .unwrap();
            assert_eq!(r.status(), 404, "{path}");
            assert_eq!(r.headers()["cache-control"], "no-store", "{path}");
            assert_eq!(r.text().await.unwrap(), "not found\n", "{path}");
        }
    }
    // Still behind the Host check.
    for path in ["/oauth/x", "/mcp/x"] {
        let r = s
            .client
            .get(s.url(path))
            .header("x-forwarded-host", "other.example")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 421, "{path}");
    }
    s.stop().await;
}

#[tokio::test]
async fn a_resource_with_one_trailing_slash_is_the_endpoint() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let slash = "https://notedown.example/mcp/";
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(&id, &[("resource", slash)]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = s
        .client
        .get(s.url("/oauth/authorize"))
        .query(&authorize_query(
            &id,
            &[("resource", "https://notedown.example/mcp//")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(location_params(&r)["error"], "invalid_target");
    let code = sign_in(&s, &id, "anna@example.com", "right").await;
    let r = s
        .client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", id.as_str()),
            ("code_verifier", VERIFIER),
            ("resource", slash),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    let refresh_token = v["refresh_token"].as_str().unwrap();
    let r = s
        .client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", id.as_str()),
            ("resource", slash),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    s.stop().await;
}
