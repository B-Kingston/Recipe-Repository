//! Groups: invites (with and without mail), membership, recipe sharing, the
//! read-only member view, copies, AI-login grants, and the 0026 migration.

use super::config::ENV_LOCK;
use crate::groups::{Invites, Mailer, OutgoingMail, mask_email, valid_email};
use crate::{AppState, CODEX_PI_PROVIDER, routes, stamp, store_pi_credential};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    sync::Arc,
};
use tower::ServiceExt;

async fn database() -> SqlitePool {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    db
}

fn state_with(db: SqlitePool, invites: Invites) -> Arc<AppState> {
    Arc::new(AppState {
        db,
        model: String::new(),
        pi_worker_path: String::new(),
        auth_script_path: String::new(),
        claude_auth_script_path: String::new(),
        search_grounding: false,
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        claude_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(HashMap::new())),
        media_debug_runs: Arc::new(Mutex::new(HashMap::new())),
        thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
        moment_jobs: Arc::new(Mutex::new(HashSet::new())),
        embedder: None,
        invites,
    })
}

fn state(db: SqlitePool) -> Arc<AppState> {
    state_with(db, Invites::default())
}

/// Puts the accounts in one new group and returns its id.
pub(super) async fn form_group(db: &SqlitePool, members: &[&str]) -> String {
    let group = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO account_groups(id,created_at) VALUES(?,?)")
        .bind(&group)
        .bind(stamp())
        .execute(db)
        .await
        .unwrap();
    for member in members {
        sqlx::query("INSERT INTO group_members(user_id,group_id,joined_at) VALUES(?,?,?)")
            .bind(member)
            .bind(&group)
            .bind(stamp())
            .execute(db)
            .await
            .unwrap();
    }
    group
}

/// `granter` lets `grantee` use its AI logins, granted at `created_at`.
pub(super) async fn grant_ai(
    db: &SqlitePool,
    group: &str,
    granter: &str,
    grantee: &str,
    created_at: &str,
) {
    sqlx::query("INSERT INTO ai_grants(group_id,granter_id,grantee_id,created_at) VALUES(?,?,?,?)")
        .bind(group)
        .bind(granter)
        .bind(grantee)
        .bind(created_at)
        .execute(db)
        .await
        .unwrap();
}

/// Gives the account a Codex sign-in, which its default provider uses.
pub(super) async fn connect_codex(db: &SqlitePool, user_id: &str) {
    let credential = serde_json::json!({"type":"oauth","access":"a","refresh":"r","expires":1});
    store_pi_credential(db, user_id, CODEX_PI_PROVIDER, &credential)
        .await
        .unwrap();
}

/// Creates an account `{name}-id` with a live session and returns the
/// browser's cookie header for it.
async fn account(db: &SqlitePool, name: &str) -> String {
    sqlx::query("INSERT INTO users(id,username,password_hash,created_at) VALUES(?,?,'',?)")
        .bind(format!("{name}-id"))
        .bind(name)
        .bind(stamp())
        .execute(db)
        .await
        .unwrap();
    let token = format!("{name:a<43}");
    sqlx::query("INSERT INTO sessions(token_hash,user_id,created_at,expires_at) VALUES(?,?,?,?)")
        .bind(format!("{:x}", Sha256::digest(token.as_bytes())))
        .bind(format!("{name}-id"))
        .bind(stamp())
        .bind((chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339())
        .execute(db)
        .await
        .unwrap();
    format!("recipe_session={token}")
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(path)
        .header(header::HOST, "recipes.local:3000");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).unwrap()
}

fn post(path: &str, cookie: Option<&str>, form: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, "recipes.local:3000")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(form.to_string())).unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

async fn body(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), 1 << 22).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn location(response: &Response) -> &str {
    response.headers()[header::LOCATION].to_str().unwrap()
}

/// The invite token in a page that shows an invite link for `base`.
fn link_token(page: &str, base: &str) -> String {
    let start = page.find(&format!("{base}/invites/")).expect(page) + base.len() + 9;
    page[start..start + 43].to_string()
}

/// Invites `email` from the account with `cookie` and returns the page.
async fn invite(app: &Router, cookie: &str, email: &str) -> (StatusCode, String) {
    let response = send(
        app,
        post(
            "/settings/group/invite",
            Some(cookie),
            &format!("email={}", email.replace('@', "%40").replace('+', "%2B")),
        ),
    )
    .await;
    (response.status(), body(response).await)
}

async fn group_of(db: &SqlitePool, user_id: &str) -> Option<String> {
    crate::groups::group_of(db, user_id).await.unwrap()
}

async fn count(db: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
}

/// Every text value stored in the database, for checks that a secret or a
/// full address was never stored.
async fn every_stored_text(db: &SqlitePool) -> String {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
    )
    .fetch_all(db)
    .await
    .unwrap();
    let mut all = String::new();
    for table in tables {
        let columns: Vec<String> =
            sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .fetch_all(db)
                .await
                .unwrap();
        for column in columns {
            let values: Vec<Option<String>> =
                sqlx::query_scalar(&format!("SELECT CAST({column} AS TEXT) FROM {table}"))
                    .fetch_all(db)
                    .await
                    .unwrap();
            for value in values.into_iter().flatten() {
                all.push_str(&value);
                all.push('\n');
            }
        }
    }
    all
}

#[test]
fn addresses_are_checked_and_masked() {
    for good in [
        "jane@example.com",
        "jane.doe+recipes@mail.example.co.uk",
        "j@x.io",
        "émile@exemple.fr",
    ] {
        assert!(valid_email(good), "{good}");
    }
    for bad in [
        "",
        "jane",
        "jane@",
        "@example.com",
        "jane@example",
        "jane@@example.com",
        "jane doe@example.com",
        "jane@exa mple.com",
        ".jane@example.com",
        "jane..doe@example.com",
        "jane@-example.com",
        "jane@example..com",
        "jane@example.com\n",
        "jane<script>@example.com",
    ] {
        assert!(!valid_email(bad), "{bad:?}");
    }
    assert!(!valid_email(&format!("{}@example.com", "a".repeat(65))));
    assert_eq!(mask_email("jane.doe@example.com"), "j***@example.com");
    assert_eq!(mask_email("émile@exemple.fr"), "é***@exemple.fr");
    assert_eq!(mask_email("nobody"), "***");
}

#[test]
fn public_base_url_is_trimmed_and_checked() {
    let invites = Invites::new(None, None, Some(" https://reciperepository.app/ ".into()));
    assert_eq!(
        invites.base_url.as_deref(),
        Some("https://reciperepository.app")
    );
    assert!(!invites.mailer.enabled());
    assert!(
        Invites::new(None, None, Some("ftp://x".into()))
            .base_url
            .is_none()
    );
    assert!(
        Invites::new(None, None, Some("  ".into()))
            .base_url
            .is_none()
    );
    assert!(!Invites::new(Some("  ".into()), None, None).mailer.enabled());
}

#[tokio::test]
async fn without_mail_an_invite_shows_its_link_and_stores_only_hashes() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;

    let (status, page) = invite(&app, &alice, "Jane.Doe@Example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("This server sends no email"), "{page}");
    assert!(
        page.contains("Invite for J***@Example.com created."),
        "{page}"
    );
    assert!(
        !page.contains("Jane.Doe"),
        "the full address never reaches the page"
    );
    let token = link_token(&page, "http://recipes.local:3000");

    // The table keeps the token's hash and the masked address only.
    let (token_hash, masked, inviter): (String, String, String) =
        sqlx::query_as("SELECT token_hash, email_masked, inviter_id FROM group_invites")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        token_hash,
        format!("{:x}", Sha256::digest(token.as_bytes()))
    );
    assert_eq!(masked, "J***@Example.com");
    assert_eq!(inviter, "alice-id");
    let stored = every_stored_text(&db).await;
    assert!(!stored.contains(&token), "the token itself is never stored");
    assert!(
        !stored.to_lowercase().contains("jane.doe"),
        "nor the address"
    );

    // The link is shown once: Settings afterwards lists the invite, masked.
    let page = body(send(&app, get("/settings", Some(&alice))).await).await;
    assert!(page.contains("Pending invites"));
    assert!(page.contains("J***@Example.com"));
    assert!(!page.contains(&token));

    // Inviting the same address again replaces the earlier link.
    let (_, page) = invite(&app, &alice, "jane.doe@example.com").await;
    let second = link_token(&page, "http://recipes.local:3000");
    assert_ne!(second, token);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM group_invites").await, 1);
    let page = body(send(&app, get(&format!("/invites/{token}"), None)).await).await;
    assert!(page.contains("This invite has ended"), "{page}");
}

#[tokio::test]
async fn with_mail_the_invite_is_emailed_and_the_page_shows_no_link() {
    let db = database().await;
    let sent = Arc::new(Mutex::new(Vec::<OutgoingMail>::new()));
    let invites = Invites {
        mailer: Mailer::Fake(sent.clone()),
        base_url: Some("https://reciperepository.app".into()),
    };
    let app = routes(state_with(db.clone(), invites));
    let alice = account(&db, "a<l>ice").await;

    let (status, page) = invite(&app, &alice, "jane@example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Invite sent to j***@example.com."), "{page}");
    assert!(!page.contains("reciperepository.app/invites/"), "{page}");
    assert!(!page.contains("jane@example.com"));
    let mail = sent.lock().pop().expect("one message");
    assert_eq!(mail.to, "jane@example.com");
    assert_eq!(mail.subject, "a<l>ice invited you to their recipe group");
    let token = link_token(&mail.text, "https://reciperepository.app");
    assert!(
        mail.html
            .contains(&format!("https://reciperepository.app/invites/{token}"))
    );
    assert!(
        mail.html.contains("a&lt;l&gt;ice invited you"),
        "{}",
        mail.html
    );
    assert!(mail.text.contains("works for 7 days"));
}

#[tokio::test]
async fn a_failed_email_falls_back_to_showing_the_link() {
    let db = database().await;
    let invites = Invites {
        mailer: Mailer::Failing,
        base_url: None,
    };
    let app = routes(state_with(db.clone(), invites));
    let alice = account(&db, "alice").await;
    let (_, page) = invite(&app, &alice, "jane@example.com").await;
    assert!(
        page.contains("The invite email could not be sent."),
        "{page}"
    );
    link_token(&page, "http://recipes.local:3000");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM group_invites").await, 1);
}

#[tokio::test]
async fn bad_addresses_and_too_many_invites_are_refused() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let (status, page) = invite(&app, &alice, "not-an-address").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("Enter an email address like name@example.com."),
        "{page}"
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM group_invites").await, 0);

    for n in 0..crate::groups::MAX_INVITES_PER_DAY {
        let (_, page) = invite(&app, &alice, &format!("cook{n}@example.com")).await;
        assert!(page.contains("created."), "invite {n}: {page}");
    }
    // Cancelling does not give a send back.
    let id: String = sqlx::query_scalar("SELECT id FROM group_invites LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    let response = send(
        &app,
        post(
            &format!("/settings/group/invites/{id}/cancel"),
            Some(&alice),
            "",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (_, page) = invite(&app, &alice, "one-more@example.com").await;
    assert!(page.contains("You have sent 20 invites today."), "{page}");
    assert!(!page.contains("recipes.local:3000/invites/"));
}

#[tokio::test]
async fn accepting_an_invite_makes_a_group_and_uses_the_invite_up() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    let carol = account(&db, "carol").await;
    let (_, page) = invite(&app, &alice, "bob@example.com").await;
    let token = link_token(&page, "http://recipes.local:3000");

    // The inviter sees their own invite, not an Accept button.
    let page = body(send(&app, get(&format!("/invites/{token}"), Some(&alice))).await).await;
    assert!(page.contains("You sent this invite."), "{page}");
    assert!(!page.contains("/accept"));
    let response = send(
        &app,
        post(&format!("/invites/{token}/accept"), Some(&alice), ""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let page = body(send(&app, get(&format!("/invites/{token}"), Some(&bob))).await).await;
    assert!(page.contains("Join alice’s group"), "{page}");
    assert!(page.contains("Join as bob"), "{page}");
    let response = send(
        &app,
        post(&format!("/invites/{token}/accept"), Some(&bob), ""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/settings#settings-group");
    let group = group_of(&db, "alice-id")
        .await
        .expect("alice is in a group");
    assert_eq!(
        group_of(&db, "bob-id").await.as_deref(),
        Some(group.as_str())
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM group_invites").await, 0);

    // The link works once.
    let response = send(
        &app,
        post(&format!("/invites/{token}/accept"), Some(&carol), ""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body(response).await.contains("This invite has ended"));
    assert!(group_of(&db, "carol-id").await.is_none());

    // Any member invites more; the newcomer joins the same group.
    let (_, page) = invite(&app, &bob, "carol@example.com").await;
    let token = link_token(&page, "http://recipes.local:3000");
    send(
        &app,
        post(&format!("/invites/{token}/accept"), Some(&carol), ""),
    )
    .await;
    assert_eq!(group_of(&db, "carol-id").await, Some(group.clone()));
    let page = body(send(&app, get("/settings", Some(&carol))).await).await;
    assert!(page.contains("3 members"), "{page}");
    assert!(
        page.contains("carol <span class=\"muted\">(you)</span>"),
        "{page}"
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM account_groups").await, 1);
}

#[tokio::test]
async fn declining_cancelling_and_expiry_end_an_invite() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;

    // Declining needs no account.
    let (_, page) = invite(&app, &alice, "bob@example.com").await;
    let token = link_token(&page, "http://recipes.local:3000");
    let page = body(send(&app, get(&format!("/invites/{token}"), None)).await).await;
    assert!(page.contains("Create account and join"), "{page}");
    assert!(page.contains(&format!("/login?next=/invites/{token}")));
    let page = body(send(&app, post(&format!("/invites/{token}/decline"), None, "")).await).await;
    assert!(page.contains("Invite declined"), "{page}");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM group_invites").await, 0);
    assert!(group_of(&db, "alice-id").await.is_none());

    // Only the inviter cancels.
    let (_, page) = invite(&app, &alice, "bob@example.com").await;
    let token = link_token(&page, "http://recipes.local:3000");
    let id: String = sqlx::query_scalar("SELECT id FROM group_invites")
        .fetch_one(&db)
        .await
        .unwrap();
    let response = send(
        &app,
        post(
            &format!("/settings/group/invites/{id}/cancel"),
            Some(&bob),
            "",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = send(
        &app,
        post(
            &format!("/settings/group/invites/{id}/cancel"),
            Some(&alice),
            "",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let page = body(send(&app, get(&format!("/invites/{token}"), Some(&bob))).await).await;
    assert!(page.contains("This invite has ended"));

    // An expired invite is gone, and the hourly prune removes it.
    let (_, page) = invite(&app, &alice, "bob@example.com").await;
    let token = link_token(&page, "http://recipes.local:3000");
    sqlx::query("UPDATE group_invites SET expires_at = '2026-01-01T00:00:00+00:00'")
        .execute(&db)
        .await
        .unwrap();
    let response = send(
        &app,
        post(&format!("/invites/{token}/accept"), Some(&bob), ""),
    )
    .await;
    assert!(body(response).await.contains("This invite has ended"));
    assert!(group_of(&db, "bob-id").await.is_none());
    sqlx::query("UPDATE group_invite_sends SET sent_at = '2026-01-01T00:00:00+00:00'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(crate::groups::prune_invites(&db).await.unwrap(), 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM group_invites").await, 0);
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM group_invite_sends").await,
        0
    );
}

#[tokio::test]
async fn a_signed_out_visitor_can_create_an_account_through_an_invite() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let (_, page) = invite(&app, &alice, "dana@example.com").await;
    let token = link_token(&page, "http://recipes.local:3000");

    // A refused form keeps the visitor on the invite with the username.
    let response = send(
        &app,
        post(
            &format!("/invites/{token}/signup"),
            None,
            "username=dana&password=short&confirm_password=short",
        ),
    )
    .await;
    let page = body(response).await;
    assert!(
        page.contains("Password must be at least 8 characters."),
        "{page}"
    );
    assert!(page.contains("value=\"dana\""));
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM users WHERE username='dana'").await,
        0
    );

    let response = send(
        &app,
        post(
            &format!("/invites/{token}/signup"),
            None,
            "username=dana&password=dana-secret&confirm_password=dana-secret",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/settings#settings-group");
    let cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect();
    let session = cookies
        .iter()
        .find_map(|cookie| {
            cookie
                .split(';')
                .next()
                .filter(|c| c.starts_with("recipe_session=") && c.len() > "recipe_session=".len())
        })
        .unwrap_or_else(|| panic!("signed in: {cookies:?}"));
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("recipe_flash=group_joined;"))
    );
    let dana: String = sqlx::query_scalar("SELECT id FROM users WHERE username='dana'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(group_of(&db, &dana).await, group_of(&db, "alice-id").await);
    assert!(group_of(&db, &dana).await.is_some());
    let page = body(send(&app, get("/settings", Some(session))).await).await;
    assert!(page.contains("alice"), "{page}");

    // A used or unknown invite opens no account.
    let response = send(
        &app,
        post(
            &format!("/invites/{token}/signup"),
            None,
            "username=eve&password=eve-secret1&confirm_password=eve-secret1",
        ),
    )
    .await;
    assert!(body(response).await.contains("This invite has ended"));
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM users WHERE username='eve'").await,
        0
    );
}

#[tokio::test]
async fn an_account_in_another_group_must_leave_it_first() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let carol = account(&db, "carol").await;
    account(&db, "dave").await;
    form_group(&db, &["carol-id", "dave-id"]).await;
    let (_, page) = invite(&app, &alice, "carol@example.com").await;
    let token = link_token(&page, "http://recipes.local:3000");

    let page = body(send(&app, get(&format!("/invites/{token}"), Some(&carol))).await).await;
    assert!(page.contains("carol is already in another group"), "{page}");
    assert!(!page.contains("Join as carol"));
    let response = send(
        &app,
        post(&format!("/invites/{token}/accept"), Some(&carol), ""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body(response).await.contains("Leave it in Settings first"));

    // Leaving dissolves the two-member group; then the invite works.
    let response = send(&app, post("/settings/group/leave", Some(&carol), "")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(group_of(&db, "dave-id").await.is_none());
    let response = send(
        &app,
        post(&format!("/invites/{token}/accept"), Some(&carol), ""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        group_of(&db, "carol-id").await,
        group_of(&db, "alice-id").await
    );
}

#[tokio::test]
async fn leaving_ends_grants_and_sharing_and_the_last_two_dissolve_the_group() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    account(&db, "carol").await;
    let group = form_group(&db, &["alice-id", "bob-id", "carol-id"]).await;
    connect_codex(&db, "alice-id").await;
    connect_codex(&db, "bob-id").await;
    grant_ai(
        &db,
        &group,
        "alice-id",
        "carol-id",
        "2026-01-01T00:00:00+00:00",
    )
    .await;
    grant_ai(
        &db,
        &group,
        "bob-id",
        "carol-id",
        "2026-02-01T00:00:00+00:00",
    )
    .await;
    grant_ai(
        &db,
        &group,
        "alice-id",
        "bob-id",
        "2026-01-01T00:00:00+00:00",
    )
    .await;
    seed_recipe(&db, "alice-id", "alice-soup", true).await;
    seed_recipe(&db, "bob-id", "bob-stew", true).await;
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "alice-id"
    );

    // Alice leaves: her grants both ways go, her recipe stops being shared,
    // and carol moves to bob's logins.
    send(&app, post("/settings/group/leave", Some(&alice), "")).await;
    assert!(group_of(&db, "alice-id").await.is_none());
    assert_eq!(crate::ai_account(&db, "carol-id").await.unwrap(), "bob-id");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM ai_grants WHERE granter_id='alice-id' OR grantee_id='alice-id'"
        )
        .await,
        0
    );
    assert_eq!(
        count(&db, "SELECT shared FROM recipes WHERE id='alice-soup'").await,
        0
    );
    assert_eq!(
        count(&db, "SELECT shared FROM recipes WHERE id='bob-stew'").await,
        1
    );

    // Bob leaves: carol is alone, so the group dissolves and carol's grant
    // goes with it.
    let page = body(send(&app, get("/settings", Some(&bob))).await).await;
    assert!(page.contains("Leave group"));
    send(&app, post("/settings/group/leave", Some(&bob), "")).await;
    assert!(group_of(&db, "carol-id").await.is_none());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM account_groups").await, 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM ai_grants").await, 0);
    assert_eq!(
        count(&db, "SELECT shared FROM recipes WHERE id='bob-stew'").await,
        0
    );
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "carol-id"
    );
    let page = body(send(&app, get("/settings", Some(&bob))).await).await;
    assert!(page.contains("Invite the people you cook with."), "{page}");
    assert!(!page.contains("Leave group"));
}

/// A recipe with a photo, an ingredient, a step using it, a source, a
/// chart, a moment still, a note, and a version.
async fn seed_recipe(db: &SqlitePool, user_id: &str, id: &str, shared: bool) {
    let now = "2026-05-01T00:00:00+00:00";
    for statement in [
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at,thumbnail_jpeg,chart_json,plate_name,servings)
         VALUES(?1,'Soup '||?1,?2,?3,?3,x'ffd8ff01',?1||'-chart','Bowl',2)",
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit)
         VALUES(?1||'-salt',?1,'ingredient',0,'salt','1','tsp')",
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES(?1||'-stir',?1,'step',0,'Stir in the salt')",
        "INSERT INTO recipe_step_ingredients(id,step_id,position,text) VALUES(?1||'-use',?1||'-stir',0,'1 tsp salt')",
        "INSERT INTO recipe_sources(id,recipe_id,position,title,url) VALUES(?1||'-src',?1,0,'Blog','https://example.com/'||?1)",
        "INSERT INTO recipe_moment_stills(recipe_id,seconds,jpeg) VALUES(?1,4,x'ffd8ff02')",
        "INSERT INTO recipe_notes(id,user_id,recipe_id,body,created_at,updated_at) VALUES(?1||'-note',?2,?1,'Private note',?3,?3)",
        "INSERT INTO recipe_versions(id,recipe_id,version,snapshot,created_at) VALUES(?1||'-v1',?1,1,'{}',?3)",
    ] {
        sqlx::query(statement)
            .bind(id)
            .bind(user_id)
            .bind(now)
            .execute(db)
            .await
            .unwrap();
    }
    if shared {
        sqlx::query("UPDATE recipes SET shared=1 WHERE id=?")
            .bind(id)
            .execute(db)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn only_the_owner_in_a_group_toggles_sharing_and_it_is_not_an_edit() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    seed_recipe(&db, "alice-id", "soup", false).await;

    // Outside a group there is no toggle, and sharing is refused.
    let page = body(send(&app, get("/recipes/soup", Some(&alice))).await).await;
    assert!(!page.contains("Share with group"), "{page}");
    let response = send(&app, post("/recipes/soup/share", Some(&alice), "shared=on")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    form_group(&db, &["alice-id", "bob-id"]).await;
    let page = body(send(&app, get("/recipes/soup", Some(&alice))).await).await;
    assert!(page.contains("Share with group"), "{page}");
    let response = send(&app, post("/recipes/soup/share", Some(&alice), "shared=on")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/recipes/soup");
    let (shared, updated_at): (bool, String) =
        sqlx::query_as("SELECT shared<>0, updated_at FROM recipes WHERE id='soup'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(shared);
    assert_eq!(
        updated_at, "2026-05-01T00:00:00+00:00",
        "updated_at is kept"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM recipe_versions WHERE recipe_id='soup'"
        )
        .await,
        1,
        "no version recorded"
    );
    let page = body(send(&app, get("/recipes/soup", Some(&alice))).await).await;
    assert!(page.contains("&#10003; Shared with group"), "{page}");

    // Another member cannot change it.
    let response = send(&app, post("/recipes/soup/share", Some(&bob), "shared=off")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = send(
        &app,
        post("/recipes/soup/share", Some(&alice), "shared=off"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        count(&db, "SELECT shared FROM recipes WHERE id='soup'").await,
        0
    );
}

#[tokio::test]
async fn members_read_a_shared_recipe_and_nobody_else_does() {
    let db = database().await;
    let app = routes(state(db.clone()));
    account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    let mallory = account(&db, "mallory").await;
    form_group(&db, &["alice-id", "bob-id"]).await;
    seed_recipe(&db, "alice-id", "soup", true).await;
    seed_recipe(&db, "alice-id", "secret", false).await;

    let response = send(&app, get("/recipes/soup?serves=4", Some(&bob))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    assert!(page.contains("Shared by alice"), "{page}");
    assert!(page.contains("Stir in the salt"));
    assert!(page.contains("2 tsp"), "scaled to 4 servings: {page}");
    assert!(page.contains("Save a copy to my library"));
    for owner_only in [
        "edit_meta=1",
        "/recipes/soup/delete",
        "/recipes/soup/blocks",
        "/recipes/soup/chat",
        "/recipes/soup/notes",
        "/recipes/soup/filing",
        "/recipes/soup/thumbnail/upload",
        "/recipes/soup/share",
        "Private note",
        "version-tabs",
    ] {
        assert!(
            !page.contains(owner_only),
            "{owner_only} on a member's page"
        );
    }
    let chart = send(&app, get("/recipes/soup?view=chart&step=1", Some(&bob))).await;
    assert_eq!(chart.status(), StatusCode::OK);
    for path in ["/recipes/soup/thumbnail", "/recipes/soup/stills/4"] {
        let response = send(&app, get(path, Some(&bob))).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let response = send(&app, get(path, Some(&mallory))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }

    // Owner tools refuse a member.
    for (path, form) in [
        (
            "/recipes/soup",
            "title=Mine&description=&servings=&prep_minutes=&cook_minutes=",
        ),
        ("/recipes/soup/delete", ""),
        ("/recipes/soup/blocks", "section=step&text=More"),
        ("/recipes/soup/blocks/soup-stir/delete", ""),
        ("/recipes/soup/filing", "folder=&cuisine="),
        ("/recipes/soup/thumbnail/remove", ""),
        ("/recipes/soup/versions/1/restore", ""),
        ("/recipes/soup/chat", "question=Hi"),
        ("/recipes/soup/moments/capture", ""),
    ] {
        let response = send(&app, post(path, Some(&bob), form)).await;
        assert!(
            matches!(response.status(), StatusCode::NOT_FOUND),
            "{path} answered {}",
            response.status()
        );
    }
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM recipes WHERE id='soup' AND title='Soup soup'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM recipe_blocks WHERE recipe_id='soup'"
        )
        .await,
        2
    );

    // Unshared recipes, and anyone outside the group, get 404.
    let response = send(&app, get("/recipes/secret", Some(&bob))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = send(&app, get("/recipes/secret/thumbnail", Some(&bob))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = send(&app, get("/recipes/soup", Some(&mallory))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Leaving ends access at once.
    send(&app, post("/settings/group/leave", Some(&bob), "")).await;
    for path in [
        "/recipes/soup",
        "/recipes/soup/thumbnail",
        "/recipes/soup/stills/4",
    ] {
        let response = send(&app, get(path, Some(&bob))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn the_group_view_lists_every_members_shared_recipes() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    seed_recipe(&db, "alice-id", "soup", true).await;
    seed_recipe(&db, "bob-id", "stew", true).await;
    seed_recipe(&db, "bob-id", "hidden", false).await;

    // No Group view outside a group, even when it was saved.
    let page = body(send(&app, get("/", Some(&alice))).await).await;
    assert!(!page.contains("value=\"group\""), "{page}");

    form_group(&db, &["alice-id", "bob-id"]).await;
    let page = body(send(&app, get("/", Some(&alice))).await).await;
    assert!(page.contains("value=\"group\""));
    let response = send(&app, post("/library/view", Some(&alice), "view=group")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let page = body(send(&app, get("/", Some(&alice))).await).await;
    assert!(page.contains("2 tickets shared in your group"), "{page}");
    assert!(page.contains("Soup soup") && page.contains("Soup stew"));
    assert!(page.contains("Shared by alice") && page.contains("Shared by bob"));
    assert!(!page.contains("Soup hidden"));
    assert!(
        page.contains("1 tsp"),
        "ingredients print on shared tickets: {page}"
    );

    // Bob's own library is untouched by it, and search stays his own.
    let page = body(send(&app, get("/?q=soup", Some(&bob))).await).await;
    assert!(!page.contains("/recipes/soup\""), "{page}");

    // After leaving, the saved Group view falls back to A to Z.
    send(&app, post("/settings/group/leave", Some(&alice), "")).await;
    let page = body(send(&app, get("/", Some(&alice))).await).await;
    assert!(page.contains("filed A to Z"), "{page}");
    assert!(!page.contains("Shared by"));
}

#[tokio::test]
async fn a_member_saves_a_copy_of_a_shared_recipe_to_their_library() {
    let db = database().await;
    let app = routes(state(db.clone()));
    account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    let mallory = account(&db, "mallory").await;
    form_group(&db, &["alice-id", "bob-id"]).await;
    seed_recipe(&db, "alice-id", "soup", true).await;

    let response = send(&app, post("/recipes/soup/copy", Some(&mallory), "")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = send(&app, post("/recipes/soup/copy", Some(&bob), "")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let copy = location(&response)
        .strip_prefix("/recipes/")
        .unwrap()
        .to_string();
    assert_ne!(copy, "soup");
    let (owner, title, chart, plate, photo, shared): (String, String, String, String, Vec<u8>, bool) =
        sqlx::query_as(
            "SELECT user_id,title,chart_json,plate_name,thumbnail_jpeg,shared<>0 FROM recipes WHERE id=?",
        )
        .bind(&copy)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(owner, "bob-id");
    assert_eq!(title, "Soup soup");
    assert_eq!(chart, "soup-chart");
    assert_eq!(plate, "Bowl");
    assert_eq!(photo, vec![0xff, 0xd8, 0xff, 0x01]);
    assert!(!shared);
    let uses: Vec<String> = sqlx::query_scalar(
        "SELECT s.text FROM recipe_step_ingredients s JOIN recipe_blocks b ON b.id=s.step_id WHERE b.recipe_id=?",
    )
    .bind(&copy)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(uses, vec!["1 tsp salt"]);
    for (table, expected) in [
        ("recipe_blocks", 2),
        ("recipe_sources", 1),
        ("recipe_notes", 0),
        ("recipe_versions", 0),
        ("recipe_moment_stills", 0),
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE recipe_id=?"))
            .bind(&copy)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, expected, "{table}");
    }
    // The copy is bob's to edit; the original is untouched.
    let page = body(send(&app, get(&format!("/recipes/{copy}"), Some(&bob))).await).await;
    assert!(
        page.contains("edit_meta=1") && !page.contains("Shared by"),
        "{page}"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM recipes WHERE user_id='alice-id'").await,
        1
    );
}

#[tokio::test]
async fn ai_resolution_prefers_own_then_the_chosen_or_earliest_grant() {
    let db = database().await;
    let app = routes(state(db.clone()));
    account(&db, "alice").await;
    account(&db, "bob").await;
    let carol = account(&db, "carol").await;
    let group = form_group(&db, &["alice-id", "bob-id", "carol-id"]).await;
    connect_codex(&db, "alice-id").await;
    connect_codex(&db, "bob-id").await;
    grant_ai(
        &db,
        &group,
        "bob-id",
        "carol-id",
        "2026-02-01T00:00:00+00:00",
    )
    .await;
    grant_ai(
        &db,
        &group,
        "alice-id",
        "carol-id",
        "2026-03-01T00:00:00+00:00",
    )
    .await;

    // The earliest grant is the default.
    assert_eq!(crate::ai_account(&db, "carol-id").await.unwrap(), "bob-id");
    let page = body(send(&app, get("/settings", Some(&carol))).await).await;
    assert!(page.contains("bob's AI logins"), "{page}");
    assert!(
        page.contains("name=\"granter\" value=\"bob-id\" checked"),
        "{page}"
    );

    // Carol picks alice's.
    let response = send(
        &app,
        post(
            "/settings/group/ai-source",
            Some(&carol),
            "granter=alice-id",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "alice-id"
    );
    let response = send(
        &app,
        post(
            "/settings/group/ai-source",
            Some(&carol),
            "granter=carol-id",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A sharer who disconnects is skipped.
    crate::remove_pi_credential(&db, "alice-id", CODEX_PI_PROVIDER)
        .await
        .unwrap();
    assert_eq!(crate::ai_account(&db, "carol-id").await.unwrap(), "bob-id");
    connect_codex(&db, "alice-id").await;

    // Her own setup wins over every grant.
    connect_codex(&db, "carol-id").await;
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "carol-id"
    );
    crate::remove_pi_credential(&db, "carol-id", CODEX_PI_PROVIDER)
        .await
        .unwrap();

    // A grant whose granter left no longer counts, even before its row goes.
    sqlx::query("DELETE FROM group_members WHERE user_id='alice-id'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(crate::ai_account(&db, "carol-id").await.unwrap(), "bob-id");
    sqlx::query("DELETE FROM group_members WHERE user_id='bob-id'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "carol-id"
    );
}

#[tokio::test]
async fn members_choose_who_may_use_their_ai_logins() {
    let db = database().await;
    let app = routes(state(db.clone()));
    let alice = account(&db, "alice").await;
    let bob = account(&db, "bob").await;
    account(&db, "carol").await;
    account(&db, "mallory").await;
    form_group(&db, &["alice-id", "bob-id", "carol-id"]).await;

    // Without an AI login of her own, alice cannot share one.
    let page = body(send(&app, get("/settings", Some(&alice))).await).await;
    assert!(
        page.contains("Connect an AI provider above to share it"),
        "{page}"
    );
    let response = send(
        &app,
        post("/settings/group/ai-grants", Some(&alice), "grantee=bob-id"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    connect_codex(&db, "alice-id").await;
    let page = body(send(&app, get("/settings", Some(&alice))).await).await;
    assert!(page.contains("name=\"grantee\" value=\"bob-id\""), "{page}");
    assert!(!page.contains("value=\"alice-id\""), "not herself");
    let response = send(
        &app,
        post(
            "/settings/group/ai-grants",
            Some(&alice),
            "grantee=bob-id&grantee=carol-id",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(crate::ai_account(&db, "bob-id").await.unwrap(), "alice-id");
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "alice-id"
    );
    let page = body(send(&app, get("/settings", Some(&bob))).await).await;
    assert!(
        page.contains("alice shares their AI logins with you"),
        "{page}"
    );

    // Unticking takes a grant back; outsiders cannot be granted.
    send(
        &app,
        post(
            "/settings/group/ai-grants",
            Some(&alice),
            "grantee=carol-id",
        ),
    )
    .await;
    assert_eq!(crate::ai_account(&db, "bob-id").await.unwrap(), "bob-id");
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "alice-id"
    );
    let response = send(
        &app,
        post(
            "/settings/group/ai-grants",
            Some(&alice),
            "grantee=mallory-id",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = send(&app, post("/settings/group/ai-grants", Some(&bob), "")).await;
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "an empty save is fine"
    );
}

/// Runs every migration before 0026, then `fixtures`, then 0026.
async fn migrate_with(fixtures: &[&str]) -> SqlitePool {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let all = sqlx::migrate!();
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter()
                .filter(|migration| migration.version < 26)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before.run(&db).await.unwrap();
    for statement in fixtures {
        sqlx::query(statement).execute(&db).await.unwrap();
    }
    all.run(&db).await.unwrap();
    db
}

#[tokio::test]
async fn upgrading_a_single_account_makes_no_group() {
    let db = migrate_with(&[
        "INSERT INTO users(id,username,password_hash,created_at) VALUES('u1','alice','x','2026-01-01T00:00:00+00:00')",
        "INSERT INTO recipes(id,title,created_at,updated_at,user_id) VALUES('r1','Soup','t','t','u1')",
        "INSERT INTO app_settings(user_id,key,value,updated_at) VALUES('u1','ai_logins','own','t')",
    ])
    .await;
    assert_eq!(count(&db, "SELECT COUNT(*) FROM account_groups").await, 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM group_members").await, 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM ai_grants").await, 0);
    assert_eq!(count(&db, "SELECT shared FROM recipes").await, 0);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM app_settings WHERE key='ai_logins'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn upgrading_a_household_keeps_its_shared_ai_logins() {
    let db = migrate_with(&[
        "INSERT INTO users(id,username,password_hash,created_at) VALUES('u2','bob','x','2026-02-01T00:00:00+00:00')",
        "INSERT INTO users(id,username,password_hash,created_at) VALUES('u1','alice','x','2026-01-01T00:00:00+00:00')",
        "INSERT INTO users(id,username,password_hash,created_at) VALUES('u3','carol','x','2026-03-01T00:00:00+00:00')",
        "INSERT INTO users(id,username,password_hash,created_at) VALUES('u4','dave','x','2026-04-01T00:00:00+00:00')",
        "INSERT INTO recipes(id,title,created_at,updated_at,user_id) VALUES('r1','Soup','t','t','u2')",
        "INSERT INTO app_settings(user_id,key,value,updated_at) VALUES('u3','ai_logins','own','t')",
        "INSERT INTO app_settings(user_id,key,value,updated_at) VALUES('u4','ai_logins','shared','t')",
        "INSERT INTO pi_credentials(user_id,provider,credential_json,updated_at) VALUES('u1','openai-codex','{}','t')",
    ])
    .await;
    assert_eq!(count(&db, "SELECT COUNT(*) FROM account_groups").await, 1);
    let group: String = sqlx::query_scalar("SELECT id FROM account_groups")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(group.len(), 36, "a uuid-shaped id: {group}");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM group_members WHERE group_id=(SELECT id FROM account_groups)"
        )
        .await,
        4
    );
    let grants: Vec<(String, String)> =
        sqlx::query_as("SELECT granter_id, grantee_id FROM ai_grants ORDER BY grantee_id")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        grants,
        vec![("u1".into(), "u2".into()), ("u1".into(), "u4".into())],
        "the oldest grants everyone but the account that chose its own"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM app_settings WHERE key='ai_logins'"
        )
        .await,
        0
    );
    assert_eq!(count(&db, "SELECT shared FROM recipes").await, 0);
    assert_eq!(crate::ai_account(&db, "u2").await.unwrap(), "u1");
    assert_eq!(crate::ai_account(&db, "u3").await.unwrap(), "u3");
}

/// A `tracing` writer that keeps everything logged in memory.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_api_key_and_full_addresses_never_reach_logs_or_pages() {
    // Resend gets a closed local port, so the send fails fast without
    // leaving the machine. The client reads RESEND_BASE_URL when it is made.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let invites = {
        let _lock = ENV_LOCK.lock();
        unsafe { std::env::set_var("RESEND_BASE_URL", format!("http://127.0.0.1:{port}")) };
        let invites = Invites::new(Some("re_test_SECRET_key_123".into()), None, None);
        unsafe { std::env::remove_var("RESEND_BASE_URL") };
        invites
    };
    assert!(invites.mailer.enabled());

    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let db = database().await;
    let app = routes(state_with(db.clone(), invites));
    let alice = account(&db, "alice").await;
    let (status, page) = invite(&app, &alice, "jane.doe@example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("The invite email could not be sent."),
        "{page}"
    );
    let token = link_token(&page, "http://recipes.local:3000");
    let (_, refused) = invite(&app, &alice, "bad address@example.com").await;
    let settings = body(send(&app, get("/settings", Some(&alice))).await).await;
    let invite_page = body(send(&app, get(&format!("/invites/{token}"), None)).await).await;

    let logged = String::from_utf8(logs.0.lock().clone()).unwrap();
    assert!(logged.contains("Group invite email failed"), "{logged}");
    assert!(logged.contains("j***@example.com"), "{logged}");
    for text in [&logged, &page, &refused, &settings, &invite_page] {
        assert!(!text.contains("SECRET"), "the API key leaked: {text}");
        assert!(!text.contains("jane.doe"), "the address leaked: {text}");
        assert!(!text.contains("bad address"), "the address leaked: {text}");
    }
    assert!(!logged.contains(&token), "the invite token is never logged");
}
