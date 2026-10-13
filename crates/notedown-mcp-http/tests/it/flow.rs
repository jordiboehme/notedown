use crate::common::*;

#[tokio::test]
async fn code_exchange_refresh_replay_and_revoke() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let code = sign_in(&s, &id, "anna@example.com", "right").await;
    let t = exchange(&s, &id, &code).await;
    let access = t["access_token"].as_str().unwrap().to_string();
    let refresh1 = t["refresh_token"].as_str().unwrap().to_string();
    assert!(access.starts_with("nda_") && refresh1.starts_with("ndr_"));
    assert_eq!(t["token_type"], "Bearer");
    assert_eq!(t["expires_in"], 3600);

    // The grant file holds only hashes.
    let dir = s.dir.path().join("accounts/anna/oauth");
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(files.len(), 1);
    let text = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
    assert!(!text.contains(&access) && !text.contains(&refresh1));

    // The code is spent.
    let r = s
        .client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", id.as_str()),
            ("code_verifier", VERIFIER),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"], "invalid_grant");

    // Rotation.
    let r = refresh(&s, &id, &refresh1).await;
    assert_eq!(r.status(), 200);
    let t2: serde_json::Value = r.json().await.unwrap();
    let refresh2 = t2["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(refresh1, refresh2);

    // Replay of refresh1 ends the grant: refresh2 dies too.
    let r = refresh(&s, &id, &refresh1).await;
    assert_eq!(r.status(), 400);
    let r = refresh(&s, &id, &refresh2).await;
    assert_eq!(r.status(), 400);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    // A new sign-in, then revoke.
    let code = sign_in(&s, &id, "anna@example.com", "right").await;
    let t = exchange(&s, &id, &code).await;
    let r = s
        .client
        .post(s.url("/oauth/revoke"))
        .form(&[("token", t["refresh_token"].as_str().unwrap())])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = refresh(&s, &id, t["refresh_token"].as_str().unwrap()).await;
    assert_eq!(r.status(), 400);
    let r = s
        .client
        .post(s.url("/oauth/revoke"))
        .form(&[("token", "nonsense")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "revoke of an unknown token is 200 too");
    s.stop().await;
}

async fn error_of(r: reqwest::Response) -> (u16, String) {
    let status = r.status().as_u16();
    let v: serde_json::Value = r.json().await.unwrap();
    (status, v["error"].as_str().unwrap_or("").to_string())
}

#[tokio::test]
async fn revoke_by_access_token_ends_the_grant() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let code = sign_in(&s, &id, "anna@example.com", "right").await;
    let t = exchange(&s, &id, &code).await;
    let r = s
        .client
        .post(s.url("/oauth/revoke"))
        .form(&[("token", t["access_token"].as_str().unwrap())])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = refresh(&s, &id, t["refresh_token"].as_str().unwrap()).await;
    assert_eq!(r.status(), 400);
    s.stop().await;
}

#[tokio::test]
async fn refresh_with_the_wrong_client_or_resource_and_unknown_codes() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let other = register(&s, REDIRECT).await;
    let code = sign_in(&s, &id, "anna@example.com", "right").await;
    let t = exchange(&s, &id, &code).await;
    let rt = t["refresh_token"].as_str().unwrap();

    let r = refresh(&s, &other, rt).await;
    assert_eq!(error_of(r).await, (400, "invalid_grant".to_string()));

    // The grant is for this server's resource; a field naming another one is invalid_target.
    let r = s
        .client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", rt),
            ("client_id", id.as_str()),
            ("resource", "https://x.example/mcp"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(error_of(r).await, (400, "invalid_target".to_string()));
    // The token survived both refusals.
    assert_eq!(refresh(&s, &id, rt).await.status(), 200);

    let r = s
        .client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", "no-such-code"),
            ("redirect_uri", REDIRECT),
            ("client_id", id.as_str()),
            ("code_verifier", VERIFIER),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(error_of(r).await, (400, "invalid_grant".to_string()));
    s.stop().await;
}

#[tokio::test]
async fn a_wrong_resource_still_spends_the_code() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let code = sign_in(&s, &id, "anna@example.com", "right").await;
    let post = |resource: Option<&'static str>| {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", id.as_str()),
            ("code_verifier", VERIFIER),
        ];
        if let Some(r) = resource {
            form.push(("resource", r));
        }
        s.client.post(s.url("/oauth/token")).form(&form).send()
    };
    let r = post(Some("https://x.example/mcp")).await.unwrap();
    assert_eq!(error_of(r).await, (400, "invalid_target".to_string()));
    let r = post(None).await.unwrap();
    assert_eq!(error_of(r).await, (400, "invalid_grant".to_string()));
    s.stop().await;
}

#[tokio::test]
async fn a_body_that_is_no_form_is_rejected_cleanly() {
    let s = start().await;
    let r = s
        .client
        .post(s.url("/oauth/token"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["cache-control"], "no-store");
    assert_eq!(error_of(r).await, (400, "invalid_request".to_string()));
    let r = s
        .client
        .post(s.url("/oauth/revoke"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    s.stop().await;
}

#[tokio::test]
async fn an_mcp_client_sees_only_its_own_account() {
    let s = start().await;
    let (_, anna) = signed_in(&s, "anna").await;
    add_account(&s, "ben", "ben@example.com", "right");
    // The rmcp client speaks ProtocolVersion::LATEST (2026-07-28): the stateless path.
    let client = mcp_client(&s, &anna).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 7);
    let r = client
        .call_tool(rmcp::model::CallToolRequestParams::new("status"))
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true));
    let text = format!("{:?}", r.content);
    assert!(text.contains("anna") && !text.contains("ben"), "{text}");
    let r = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("get_changes").with_arguments(
                serde_json::json!({"consumer": "crystalline", "summary": true})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(r.is_error, Some(true), "{r:?}");
    client.cancel().await.unwrap();
    s.stop().await;
}

/// The log must hold no secret of the sign-in: not the password, tokens, code or verifier,
/// also not from the `Location` of the 302.
#[tokio::test]
async fn the_request_log_holds_no_secret_of_the_sign_in() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "secret-pw-1");
    let id = register(&s, REDIRECT).await;
    let code = sign_in(&s, &id, "anna@example.com", "secret-pw-1").await;
    let t = exchange(&s, &id, &code).await;
    let access = t["access_token"].as_str().unwrap().to_string();
    let refresh_token = t["refresh_token"].as_str().unwrap().to_string();
    let client = mcp_client(&s, &access).await.unwrap();
    client.cancel().await.unwrap();
    let r = refresh(&s, &id, &refresh_token).await;
    assert_eq!(r.status(), 200);
    // Read before `stop`: it drops the temporary data folder. Every line is flushed when written.
    let mut text = String::new();
    for entry in walk(s.state.log.logs_dir()) {
        text.push_str(&String::from_utf8_lossy(&std::fs::read(entry).unwrap()));
    }
    s.stop().await;
    assert!(!text.is_empty(), "the requests were logged");
    for secret in ["secret-pw-1", &access, &refresh_token, &code, VERIFIER] {
        assert!(!text.contains(secret), "the log holds {secret}");
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn passwd_ends_the_grants_of_a_running_server() {
    let s = start().await;
    let (_, anna) = signed_in(&s, "anna").await;
    assert!(mcp_client(&s, &anna).await.is_ok());
    notedown_cloud::auth::Accounts::new(s.dir.path())
        .set_password("anna", "new")
        .unwrap();
    let r = s
        .client
        .post(s.url("/mcp"))
        .header("authorization", format!("Bearer {anna}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    s.stop().await;
}

#[tokio::test]
async fn a_code_issued_before_passwd_is_not_exchanged_after_it() {
    let s = start().await;
    add_account(&s, "anna", "anna@example.com", "right");
    let id = register(&s, REDIRECT).await;
    let code = sign_in(&s, &id, "anna@example.com", "right").await;
    notedown_cloud::auth::Accounts::new(s.dir.path())
        .set_password("anna", "new")
        .unwrap();
    let r = s
        .client
        .post(s.url("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", id.as_str()),
            ("code_verifier", VERIFIER),
        ])
        .send()
        .await
        .unwrap();
    let (status, error) = error_of(r).await;
    assert_eq!((status, error.as_str()), (400, "invalid_grant"));
    s.stop().await;
}
