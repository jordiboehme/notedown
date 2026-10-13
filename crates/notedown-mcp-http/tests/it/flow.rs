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
