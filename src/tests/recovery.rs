//! Account recovery: the recovery email (Settings, verification links,
//! invites that prove it), "Forgot password?", and reset links.

use super::groups::LogBuffer;
use super::{body_text as body, memory_db as database, test_state_with_mail};
use crate::auth::{Limit, hash_password, token_hash, username_key};
use crate::mail::{Mail, Mailer, OutgoingMail};
use crate::{AppState, routes, stamp};
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
    response::Response,
};
use parking_lot::Mutex;
use sqlx::SqlitePool;
use std::{
    net::SocketAddr,
    sync::{Arc, LazyLock},
};
use tower::ServiceExt;

const BASE: &str = "https://recipes.example";
const PASSWORD: &str = "secret-pass";
static PASSWORD_HASH: LazyLock<String> = LazyLock::new(|| hash_password(PASSWORD).unwrap());

type Outbox = Arc<Mutex<Vec<OutgoingMail>>>;

/// A state that records mail and builds recovery links on BASE.
fn mail_state(db: SqlitePool) -> (Arc<AppState>, Outbox) {
    let sent = Outbox::default();
    let mail = Mail {
        mailer: Mailer::Fake(sent.clone()),
        base_url: Some(BASE.into()),
    };
    (test_state_with_mail(db, mail), sent)
}

/// Creates `{name}-id` with PASSWORD and a live session; returns the
/// browser's cookie header for it.
async fn account(db: &SqlitePool, name: &str) -> String {
    sqlx::query("INSERT INTO users(id,username,password_hash,created_at) VALUES(?,?,?,?)")
        .bind(format!("{name}-id"))
        .bind(name)
        .bind(&*PASSWORD_HASH)
        .bind(stamp())
        .execute(db)
        .await
        .unwrap();
    let token = format!("{name:a<43}");
    sqlx::query("INSERT INTO sessions(token_hash,user_id,created_at,expires_at) VALUES(?,?,?,?)")
        .bind(token_hash(&token))
        .bind(format!("{name}-id"))
        .bind(stamp())
        .bind((chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339())
        .execute(db)
        .await
        .unwrap();
    format!("recipe_session={token}")
}

/// Gives `user_id` a verified recovery address.
async fn verified(db: &SqlitePool, user_id: &str, email: &str) {
    sqlx::query("UPDATE users SET email = ?, email_verified_at = ? WHERE id = ?")
        .bind(email)
        .bind(stamp())
        .bind(user_id)
        .execute(db)
        .await
        .unwrap();
}

/// An invite from `inviter_id` to `email`; returns its token.
async fn invite(db: &SqlitePool, inviter_id: &str, email: &str) -> String {
    let token = crate::auth::new_token();
    sqlx::query(
        "INSERT INTO group_invites (id, token_hash, inviter_id, email_hash, email_masked, created_at, expires_at)
         VALUES (?, ?, ?, ?, 'x***@example.com', ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(token_hash(&token))
    .bind(inviter_id)
    .bind(crate::groups::email_hash(email))
    .bind(stamp())
    .bind((chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339())
    .execute(db)
    .await
    .unwrap();
    token
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).unwrap()
}

fn post(path: &str, cookie: Option<&str>, form: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(form.to_string())).unwrap()
}

fn from_peer(mut request: Request<Body>, peer: &str) -> Request<Body> {
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    request
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

/// Sends the request and returns its status and body.
async fn page(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = send(app, request).await;
    (response.status(), body(response).await)
}

fn location(response: &Response) -> &str {
    response.headers()[header::LOCATION].to_str().unwrap()
}

/// The value the response's `Set-Cookie` gives `name`, if any.
fn set_cookie(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok()?.split(';').next())
        .find_map(|pair| Some(pair.strip_prefix(name)?.strip_prefix('=')?.to_string()))
}

fn encode(value: &str) -> String {
    value.replace('@', "%40").replace('+', "%2B")
}

fn email_form(email: &str, password: &str) -> String {
    format!("email={}&current_password={password}", encode(email))
}

/// The mail sent so far, once `count` messages are in (mail for a reset
/// goes out on a spawned task) or the task had ample turns to send.
async fn outbox(sent: &Outbox, count: usize) -> Vec<OutgoingMail> {
    for _ in 0..100 {
        if sent.lock().len() >= count {
            break;
        }
        tokio::task::yield_now().await;
    }
    sent.lock().clone()
}

/// The token of the `path` link in a message.
fn link_token(mail: &OutgoingMail, path: &str) -> String {
    let marker = format!("{BASE}{path}");
    let start = mail.text.find(&marker).expect(&mail.text) + marker.len();
    mail.text[start..start + 43].to_string()
}

async fn stored_email(db: &SqlitePool, user_id: &str) -> (Option<String>, bool) {
    sqlx::query_as("SELECT email, email_verified_at IS NOT NULL FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn count(db: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
}

#[tokio::test]
async fn without_recovery_mail_there_is_no_forgot_link_or_email_field() {
    // Mail off; then mail on but no PUBLIC_BASE_URL to build links on.
    let with_key = Mail {
        mailer: Mailer::Fake(Outbox::default()),
        base_url: None,
    };
    for mail in [Mail::default(), with_key] {
        let db = database().await;
        let app = routes(test_state_with_mail(db.clone(), mail));
        let alice = account(&db, "alice").await;
        let token = invite(&db, "alice-id", "jane@example.com").await;
        let (_, login) = page(&app, get("/login", None)).await;
        assert!(login.contains("Sign in"), "{login}");
        assert!(!login.contains("/forgot"), "{login}");
        for request in [
            get("/forgot", None),
            post("/forgot", None, "account=alice"),
            get("/settings/email", Some(&alice)),
            post(
                "/settings/email",
                Some(&alice),
                &email_form("a@example.com", PASSWORD),
            ),
        ] {
            assert_eq!(send(&app, request).await.status(), StatusCode::NOT_FOUND);
        }
        let (_, settings) = page(&app, get("/settings", Some(&alice))).await;
        assert!(settings.contains("Reset password"), "{settings}");
        assert!(!settings.contains("Recovery email"), "{settings}");
        let (_, invite_page) = page(&app, get(&format!("/invites/{token}"), None)).await;
        assert!(
            invite_page.contains("Create account and join"),
            "{invite_page}"
        );
        assert!(!invite_page.contains("name=\"email\""), "{invite_page}");
        // An email typed anyway is ignored.
        let response = send(
            &app,
            post(
                &format!("/invites/{token}/signup"),
                None,
                "username=jane&password=jane-secret&confirm_password=jane-secret&email=jane%40example.com",
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM users WHERE email IS NOT NULL").await,
            0
        );
        assert_eq!(count(&db, "SELECT COUNT(*) FROM email_tokens").await, 0);
    }
}

#[tokio::test]
async fn the_recovery_email_is_added_verified_changed_and_removed() {
    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state);
    let alice = account(&db, "alice").await;

    let (_, login) = page(&app, get("/login", None)).await;
    assert!(login.contains("href=\"/forgot\""), "{login}");
    let (_, settings) = page(&app, get("/settings", Some(&alice))).await;
    assert!(settings.contains("Recovery email: none"), "{settings}");
    assert!(settings.contains("href=\"/settings/email\""), "{settings}");

    // The current password is required.
    let (status, refused) = page(
        &app,
        post(
            "/settings/email",
            Some(&alice),
            &email_form("jane@example.com", "wrong-pass"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        refused.contains("Current password is incorrect."),
        "{refused}"
    );
    let (_, refused) = page(
        &app,
        post(
            "/settings/email",
            Some(&alice),
            &email_form("not an address", PASSWORD),
        ),
    )
    .await;
    assert!(refused.contains("Enter an email address"), "{refused}");
    assert!(outbox(&sent, 1).await.is_empty());

    // Saving sends a link; the address waits until it is followed.
    let response = send(
        &app,
        post(
            "/settings/email",
            Some(&alice),
            &email_form("jane@example.com", PASSWORD),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/settings#settings-account");
    let mail = outbox(&sent, 1).await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].to, "jane@example.com");
    assert!(
        mail[0].text.contains("ignore this email"),
        "{}",
        mail[0].text
    );
    let verify = link_token(&mail[0], "/email/verify/");
    assert!(
        mail[0]
            .html
            .contains(&format!("{BASE}/email/verify/{verify}"))
    );
    assert_eq!(stored_email(&db, "alice-id").await, (None, false));
    let (_, settings) = page(&app, get("/settings", Some(&alice))).await;
    assert!(
        settings.contains("jane@example.com is waiting for verification"),
        "{settings}"
    );

    // The link works once, signed out too, and only by its button.
    let (_, confirm) = page(&app, get(&format!("/email/verify/{verify}"), None)).await;
    assert!(confirm.contains("Verify email"), "{confirm}");
    assert_eq!(stored_email(&db, "alice-id").await, (None, false));
    let (_, done) = page(&app, post(&format!("/email/verify/{verify}"), None, "")).await;
    assert!(done.contains("Email verified"), "{done}");
    assert_eq!(
        stored_email(&db, "alice-id").await,
        (Some("jane@example.com".into()), true)
    );
    for request in [
        get(&format!("/email/verify/{verify}"), None),
        post(&format!("/email/verify/{verify}"), None, ""),
    ] {
        let (_, again) = page(&app, request).await;
        assert!(again.contains("This link has expired"), "{again}");
    }
    let (_, settings) = page(&app, get("/settings", Some(&alice))).await;
    assert!(settings.contains("jane@example.com"), "{settings}");
    assert!(settings.contains("(verified)"), "{settings}");

    // A new address leaves the verified one in force until it is verified.
    send(
        &app,
        post(
            "/settings/email",
            Some(&alice),
            &email_form("new@example.com", PASSWORD),
        ),
    )
    .await;
    let mail = outbox(&sent, 2).await;
    assert_eq!(mail[1].to, "new@example.com");
    let pending = link_token(&mail[1], "/email/verify/");
    assert_eq!(
        stored_email(&db, "alice-id").await,
        (Some("jane@example.com".into()), true)
    );
    let (_, page_now) = page(&app, get("/settings/email", Some(&alice))).await;
    assert!(page_now.contains("Now: jane@example.com"), "{page_now}");
    assert!(
        page_now.contains("Waiting for verification: new@example.com"),
        "{page_now}"
    );

    // Saving a blank address removes both, and the pending link dies.
    let response = send(
        &app,
        post("/settings/email", Some(&alice), &email_form("", PASSWORD)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(stored_email(&db, "alice-id").await, (None, false));
    let (_, gone) = page(&app, post(&format!("/email/verify/{pending}"), None, "")).await;
    assert!(gone.contains("This link has expired"), "{gone}");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM email_tokens").await, 0);
}

#[tokio::test]
async fn the_email_form_limits_wrong_passwords_and_mail_per_account() {
    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state.clone());
    let alice = account(&db, "alice").await;

    for n in 0..Limit::AccountMail.allowance() {
        let response = send(
            &app,
            post(
                "/settings/email",
                Some(&alice),
                &email_form(&format!("a{n}@example.com"), PASSWORD),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }
    let (status, limited) = page(
        &app,
        post(
            "/settings/email",
            Some(&alice),
            &email_form("more@example.com", PASSWORD),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.contains("Too many emails"), "{limited}");
    assert_eq!(outbox(&sent, 6).await.len(), Limit::AccountMail.allowance());
    // Only the newest link is live.
    assert_eq!(count(&db, "SELECT COUNT(*) FROM email_tokens").await, 1);

    let bob = account(&db, "bob").await;
    for _ in 0..Limit::Password.allowance() {
        let (status, _) = page(
            &app,
            post(
                "/settings/email",
                Some(&bob),
                &email_form("bob@example.com", "wrong-pass"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    // Past the allowance even the right password is turned away.
    let (status, limited) = page(
        &app,
        post(
            "/settings/email",
            Some(&bob),
            &email_form("bob@example.com", PASSWORD),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.contains("Too many password attempts"), "{limited}");
    assert!(limited.contains("value=\"bob@example.com\""), "{limited}");
}

#[tokio::test]
async fn verification_links_expire_and_a_verified_address_belongs_to_one_account() {
    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state);
    let alice = account(&db, "alice").await;
    account(&db, "bob").await;
    verified(&db, "bob-id", "jane@example.com").await;

    send(
        &app,
        post(
            "/settings/email",
            Some(&alice),
            &email_form("Jane@Example.com", PASSWORD),
        ),
    )
    .await;
    let token = link_token(&outbox(&sent, 1).await[0], "/email/verify/");
    let (_, taken) = page(&app, post(&format!("/email/verify/{token}"), None, "")).await;
    assert!(
        taken.contains("Another account already uses this email address."),
        "{taken}"
    );
    assert_eq!(stored_email(&db, "alice-id").await, (None, false));
    // The database refuses a second verified copy too, letter case ignored.
    let duplicate = sqlx::query(
        "UPDATE users SET email = 'JANE@example.com', email_verified_at = 'x' WHERE id = 'alice-id'",
    )
    .execute(&db)
    .await;
    assert!(duplicate.is_err());

    // Once bob lets the address go, the same link works; then it expires.
    sqlx::query("UPDATE users SET email = NULL, email_verified_at = NULL WHERE id = 'bob-id'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE email_tokens SET expires_at = ?")
        .bind((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339())
        .execute(&db)
        .await
        .unwrap();
    let (_, expired) = page(&app, get(&format!("/email/verify/{token}"), None)).await;
    assert!(expired.contains("This link has expired"), "{expired}");
    let (_, expired) = page(&app, post(&format!("/email/verify/{token}"), None, "")).await;
    assert!(expired.contains("This link has expired"), "{expired}");
    assert_eq!(stored_email(&db, "alice-id").await, (None, false));
    assert_eq!(crate::recovery::prune_tokens(&db).await.unwrap(), 1);
}

#[tokio::test]
async fn an_invite_proves_its_own_address_and_another_gets_a_link() {
    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state);
    account(&db, "alice").await;

    // Signing up with the invited address (any letter case): verified, no mail.
    let token = invite(&db, "alice-id", "jane@example.com").await;
    let (_, invite_page) = page(&app, get(&format!("/invites/{token}"), None)).await;
    assert!(
        invite_page.contains("Email for password recovery"),
        "{invite_page}"
    );
    let response = send(
        &app,
        post(
            &format!("/invites/{token}/signup"),
            None,
            "username=jane&password=jane-secret&confirm_password=jane-secret&email=Jane%40Example.com",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let jane: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'jane'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        stored_email(&db, &jane).await,
        (Some("Jane@Example.com".into()), true)
    );
    assert!(outbox(&sent, 1).await.is_empty());

    // Accepting with another address: a verification link, nothing verified.
    let bob = account(&db, "bob").await;
    let token = invite(&db, "alice-id", "bob@example.com").await;
    let response = send(
        &app,
        post(
            &format!("/invites/{token}/accept"),
            Some(&bob),
            "email=robert%40example.com",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(stored_email(&db, "bob-id").await, (None, false));
    let mail = outbox(&sent, 1).await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].to, "robert@example.com");
    link_token(&mail[0], "/email/verify/");

    // A bad address refuses the sign-up form before any account is made.
    let token = invite(&db, "alice-id", "carol@example.com").await;
    let (status, refused) = page(
        &app,
        post(
            &format!("/invites/{token}/signup"),
            None,
            "username=carol&password=carol-secret&confirm_password=carol-secret&email=carol",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(refused.contains("Enter an email address"), "{refused}");
    assert!(refused.contains("value=\"carol\""), "{refused}");
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM users WHERE username = 'carol'").await,
        0
    );
}

#[tokio::test]
async fn forgot_answers_the_same_whatever_matched_and_mails_only_verified_addresses() {
    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state);
    account(&db, "alice").await;
    verified(&db, "alice-id", "jane@example.com").await;
    let carol = account(&db, "carol").await;
    // carol's address is only pending.
    send(
        &app,
        post(
            "/settings/email",
            Some(&carol),
            &email_form("carol@example.com", PASSWORD),
        ),
    )
    .await;
    assert_eq!(outbox(&sent, 1).await.len(), 1);

    let (status, form) = page(&app, get("/forgot", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("Username or email"), "{form}");
    let mut answers = Vec::new();
    for account in [
        "alice",
        "nobody",
        "nobody%40example.com",
        "carol",
        "carol%40example.com",
        "JANE%40example.com",
    ] {
        answers.push(page(&app, post("/forgot", None, &format!("account={account}"))).await);
    }
    for answer in &answers {
        assert_eq!(answer, &answers[0]);
    }
    assert_eq!(answers[0].0, StatusCode::OK);
    assert!(
        answers[0]
            .1
            .contains("If an account matches and has a verified email, we've sent a link."),
        "{}",
        answers[0].1
    );
    let mail = outbox(&sent, 3).await;
    let resets: Vec<&OutgoingMail> = mail.iter().filter(|m| m.text.contains("/reset/")).collect();
    assert_eq!(resets.len(), 2, "{mail:?}");
    assert!(resets.iter().all(|m| m.to == "jane@example.com"));
    assert!(resets[0].text.contains("alice"), "{}", resets[0].text);
    assert!(resets[0].text.contains("ignore this email"));
    // The newer link replaced the older one.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM email_tokens WHERE purpose = 'reset'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn forgot_is_limited_per_client_address_and_per_account() {
    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state);
    account(&db, "alice").await;
    verified(&db, "alice-id", "jane@example.com").await;

    // Each address may ask so often; past that it is told to wait.
    for _ in 0..Limit::ForgotAddress.allowance() {
        let request = from_peer(post("/forgot", None, "account=nobody"), "203.0.113.5:1000");
        assert_eq!(send(&app, request).await.status(), StatusCode::OK);
    }
    let request = from_peer(post("/forgot", None, "account=nobody"), "203.0.113.5:1001");
    let (status, limited) = page(&app, request).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.contains("Too many requests"), "{limited}");

    // Each account gets so many mails, whoever asks, and the page never says.
    let neutral = page(&app, post("/forgot", None, "account=nobody")).await;
    for n in 0..=Limit::AccountMail.allowance() {
        let request = from_peer(
            post("/forgot", None, "account=alice"),
            &format!("198.51.100.{n}:1"),
        );
        assert_eq!(page(&app, request).await, neutral);
    }
    assert_eq!(
        outbox(&sent, usize::MAX).await.len(),
        Limit::AccountMail.allowance()
    );
}

/// Asks for a reset of alice's password and returns the link's token.
async fn reset_link(app: &Router, sent: &Outbox) -> String {
    let before = sent.lock().len();
    send(app, post("/forgot", None, "account=alice")).await;
    let mail = outbox(sent, before + 1).await;
    link_token(mail.last().unwrap(), "/reset/")
}

#[tokio::test]
async fn a_reset_link_works_once_expires_and_gives_way_to_a_newer_one() {
    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state);
    account(&db, "alice").await;
    verified(&db, "alice-id", "jane@example.com").await;

    let older = reset_link(&app, &sent).await;
    let newer = reset_link(&app, &sent).await;
    assert_ne!(older, newer);
    let (_, expired) = page(&app, get(&format!("/reset/{older}"), None)).await;
    assert!(expired.contains("This link has expired"), "{expired}");
    assert!(expired.contains("href=\"/forgot\""), "{expired}");
    let (_, form) = page(&app, get(&format!("/reset/{newer}"), None)).await;
    assert!(form.contains("Set a new password"), "{form}");

    // The usual password rules apply, and a refusal keeps the link.
    let (_, refused) = page(
        &app,
        post(
            &format!("/reset/{newer}"),
            None,
            "new_password=new-secret-1&confirm_password=other-secret",
        ),
    )
    .await;
    assert!(refused.contains("Passwords do not match."), "{refused}");
    let (_, refused) = page(
        &app,
        post(
            &format!("/reset/{newer}"),
            None,
            "new_password=short&confirm_password=short",
        ),
    )
    .await;
    assert!(refused.contains("at least 8 characters"), "{refused}");

    let response = send(
        &app,
        post(
            &format!("/reset/{newer}"),
            None,
            "new_password=new-secret-1&confirm_password=new-secret-1",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (_, used) = page(
        &app,
        post(
            &format!("/reset/{newer}"),
            None,
            "new_password=new-secret-2&confirm_password=new-secret-2",
        ),
    )
    .await;
    assert!(used.contains("This link has expired"), "{used}");

    // An hour on, a link no longer works.
    let late = reset_link(&app, &sent).await;
    sqlx::query("UPDATE email_tokens SET expires_at = ?")
        .bind((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339())
        .execute(&db)
        .await
        .unwrap();
    let (_, expired) = page(
        &app,
        post(
            &format!("/reset/{late}"),
            None,
            "new_password=new-secret-3&confirm_password=new-secret-3",
        ),
    )
    .await;
    assert!(expired.contains("This link has expired"), "{expired}");
}

#[tokio::test]
async fn a_reset_ends_every_session_and_signs_this_browser_in() {
    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state.clone());
    let alice_elsewhere = account(&db, "alice").await;
    verified(&db, "alice-id", "jane@example.com").await;
    let bob = account(&db, "bob").await;
    let bob_token = bob.trim_start_matches("recipe_session=").to_string();
    for _ in 0..Limit::LoginUsername.allowance() {
        state.rate_limits.attempt(&[username_key("alice")]).unwrap();
    }
    assert!(state.rate_limits.blocked(&username_key("Alice")).is_some());

    // This browser is signed in to bob when alice's link is followed.
    let token = reset_link(&app, &sent).await;
    let response = send(
        &app,
        post(
            &format!("/reset/{token}"),
            Some(&bob),
            "new_password=new-secret-1&confirm_password=new-secret-1",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/settings#settings-account");
    let session = set_cookie(&response, "recipe_session").unwrap();
    assert_eq!(
        set_cookie(&response, "recipe_accounts").as_deref(),
        Some(bob_token.as_str()),
        "bob stays signed in behind alice"
    );
    assert!(state.rate_limits.blocked(&username_key("alice")).is_none());

    // alice's other sessions ended; this browser's new one works.
    let response = send(&app, get("/", Some(&alice_elsewhere))).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(location(&response).starts_with("/login"));
    let (status, settings) = page(
        &app,
        get("/settings", Some(&format!("recipe_session={session}"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        settings.contains("Recovery email: jane@example.com"),
        "{settings}"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM sessions WHERE user_id = 'alice-id'"
        )
        .await,
        1
    );

    // The new password signs in; the old one no longer does.
    let response = send(
        &app,
        post("/login", None, "username=alice&password=new-secret-1"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (_, refused) = page(
        &app,
        post(
            "/login",
            None,
            &format!("username=alice&password={PASSWORD}"),
        ),
    )
    .await;
    assert!(
        refused.contains("Username or password is incorrect."),
        "{refused}"
    );
}

#[tokio::test]
async fn links_are_stored_hashed_and_neither_tokens_nor_addresses_are_logged() {
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let db = database().await;
    let (state, sent) = mail_state(db.clone());
    let app = routes(state);
    let alice = account(&db, "alice").await;
    send(
        &app,
        post(
            "/settings/email",
            Some(&alice),
            &email_form("jane.doe@example.com", PASSWORD),
        ),
    )
    .await;
    let verify = link_token(&outbox(&sent, 1).await[0], "/email/verify/");
    let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM email_tokens")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(stored, vec![token_hash(&verify)]);
    send(&app, post(&format!("/email/verify/{verify}"), None, "")).await;
    let reset = reset_link(&app, &sent).await;
    let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM email_tokens")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(stored, vec![token_hash(&reset)]);
    send(
        &app,
        post(
            &format!("/reset/{reset}"),
            None,
            "new_password=new-secret-1&confirm_password=new-secret-1",
        ),
    )
    .await;

    // A provider outage is logged masked, and the page stays the same.
    let failing = test_state_with_mail(
        db.clone(),
        Mail {
            mailer: Mailer::Failing,
            base_url: Some(BASE.into()),
        },
    );
    let failing_app = routes(failing);
    let (status, answer) = page(&failing_app, post("/forgot", None, "account=alice")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(answer.contains("we've sent a link"), "{answer}");
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }

    let logged = String::from_utf8(logs.0.lock().clone()).unwrap();
    // Only this test fails a send; lines other tests also reach may be
    // cached as disabled by a test running without a subscriber.
    assert!(logged.contains("Password reset email failed"), "{logged}");
    assert!(logged.contains("j***@example.com"), "{logged}");
    assert!(
        !logged.contains("jane.doe"),
        "an address was logged: {logged}"
    );
    for token in [&verify, &reset] {
        assert!(!logged.contains(token.as_str()), "a token was logged");
    }
}
