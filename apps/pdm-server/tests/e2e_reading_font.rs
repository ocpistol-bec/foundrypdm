//! End-to-end check of the organization default reading font (`/settings/reading-font`).
//!
//! Requires:
//!   PDM_E2E=1
//!   PDM_E2E_BASE   (default http://127.0.0.1:8080)
//!   PDM_E2E_ADMIN_TOKEN
//!
//! Run against a disposable server, never one people use: it creates users and changes the
//! organization's default font (it puts the old value back at the end).

use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;

fn base() -> String {
    std::env::var("PDM_E2E_BASE").unwrap_or_else(|_| "http://127.0.0.1:8080".into())
}

fn admin_token() -> String {
    std::env::var("PDM_E2E_ADMIN_TOKEN").expect("PDM_E2E_ADMIN_TOKEN must be set")
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

/// Send a request; returns the status and the JSON body (`Null` when there is none).
async fn send(
    c: &Client,
    method: Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = c
        .request(method, format!("{}{path}", base()))
        .bearer_auth(token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let r = req.send().await.expect("request");
    let status = r.status();
    let body = r.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// A new user of the admin's organization with the given role, and a token for them.
async fn user_with_role(c: &Client, key: &str, role: &str) -> String {
    let (s, v) = send(
        c,
        Method::POST,
        "/users",
        &admin_token(),
        Some(json!({ "email": format!("{role}-{key}@test.invalid"), "role": role })),
    )
    .await;
    assert!(s.is_success(), "create {role}: {s}");
    let id = v["user"]["id"].as_str().expect("user id").to_string();
    let (s, v) = send(
        c,
        Method::POST,
        &format!("/users/{id}/tokens"),
        &admin_token(),
        Some(json!({ "name": "e2e-reading-font" })),
    )
    .await;
    assert!(s.is_success(), "token for {role}: {s}");
    v["token"].as_str().expect("token").to_string()
}

async fn put_font(c: &Client, token: &str, body: Value) -> (StatusCode, Value) {
    send(c, Method::PUT, "/settings/reading-font", token, Some(body)).await
}

#[tokio::test]
#[ignore]
async fn e2e_admins_set_the_default_reading_font_and_members_read_it() {
    if std::env::var("PDM_E2E").ok().as_deref() != Some("1") {
        eprintln!("PDM_E2E not set; skipping");
        return;
    }
    let c = client();
    let admin = admin_token();
    let key = uuid::Uuid::new_v4().to_string();
    let editor = user_with_role(&c, &key, "editor").await;
    let viewer = user_with_role(&c, &key, "viewer").await;

    // Any member reads it; the answer always names both faces.
    let (s, started) = send(&c, Method::GET, "/settings/reading-font", &viewer, None).await;
    assert_eq!(s, StatusCode::OK, "viewer reads the setting");
    assert!(
        started["ui"].is_string() && started["mono"].is_string(),
        "{started}"
    );

    // Only an admin changes it.
    for token in [&editor, &viewer] {
        let (s, _) = put_font(&c, token, json!({ "ui": "atkinson", "mono": "atkinson" })).await;
        assert_eq!(
            s,
            StatusCode::FORBIDDEN,
            "a non-admin changed the default font"
        );
    }
    // The role is checked before the body is read: a broken body from a non-admin is still 403.
    let r = c
        .put(format!("{}/settings/reading-font", base()))
        .bearer_auth(&editor)
        .header("Content-Type", "application/json")
        .body("{")
        .send()
        .await
        .expect("PUT");
    assert_eq!(
        r.status(),
        StatusCode::FORBIDDEN,
        "non-admin with a broken body"
    );
    let (_, v) = send(&c, Method::GET, "/settings/reading-font", &viewer, None).await;
    assert_eq!(v, started, "refused changes left the default as it was");

    // The admin's value round-trips, and every member reads it.
    let chosen = json!({ "ui": "atkinson", "mono": "system" });
    let (s, v) = put_font(&c, &admin, chosen.clone()).await;
    assert_eq!(s, StatusCode::OK, "admin sets the default font: {v}");
    assert_eq!(v, chosen);
    for token in [&admin, &editor, &viewer] {
        let (s, v) = send(&c, Method::GET, "/settings/reading-font", token, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v, chosen, "members read the stored default");
    }
    // The change is in the audit log with the stored value.
    let (s, v) = send(&c, Method::GET, "/audit?limit=50", &admin, None).await;
    assert_eq!(s, StatusCode::OK);
    let audited = v["events"].as_array().is_some_and(|events| {
        events.iter().any(|e| {
            e["action"] == "org.reading_font_updated"
                && e["object_type"] == "organization"
                && e["detail"] == chosen
        })
    });
    assert!(audited, "no org.reading_font_updated event with {chosen}");
    // A body over the route's limit is refused before it is read.
    let (s, _) = put_font(
        &c,
        &admin,
        json!({ "ui": "system", "mono": "x".repeat(8192) }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::PAYLOAD_TOO_LARGE,
        "an oversized body was read"
    );

    // Faces the portal does not bundle, and malformed bodies, are refused and change nothing.
    for bad in [
        json!({ "ui": "comic", "mono": "system" }),
        json!({ "ui": "atkinson" }),
        json!({ "ui": "system", "mono": "system", "size": 125 }),
    ] {
        let (s, _) = put_font(&c, &admin, bad.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad} was accepted");
    }
    let (_, v) = send(&c, Method::GET, "/settings/reading-font", &viewer, None).await;
    assert_eq!(v, chosen, "a refused change kept the stored default");

    // Put the organization's setting back as it was.
    let (s, v) = put_font(&c, &admin, started.clone()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, started);
}
