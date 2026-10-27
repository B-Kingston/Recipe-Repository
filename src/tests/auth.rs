use super::config::ENV_LOCK;
use crate::auth::{
    AuthUser, create_user, hash_password, reset_password, setup_create, verify_password,
};
use crate::{
    AppState, CODEX_PI_PROVIDER, ResetPasswordForm, SetupForm, import_legacy_codex_auth,
    pi_credential, routes, stamp, store_pi_credential,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Form, State},
    http::{Request, StatusCode, header},
    response::Response,
};
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde_json::json;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, collections::HashSet, fs, sync::Arc};
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

fn state(db: SqlitePool) -> Arc<AppState> {
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
    })
}

fn credential(access: &str, account_id: &str) -> serde_json::Value {
    json!({
        "type": "oauth",
        "access": access,
        "refresh": "refresh_1",
        "expires": 4_102_444_800_000_i64,
        "accountId": account_id,
    })
}

fn request(path: &str, cookie: Option<&str>) -> Request<Body> {
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

fn location(response: &Response) -> &str {
    response.headers()[header::LOCATION].to_str().unwrap()
}

/// The `name=value` part of the cookie a response sets, if any.
fn set_cookie(response: &Response) -> Option<String> {
    let value = response.headers().get(header::SET_COOKIE)?.to_str().ok()?;
    Some(value.split(';').next()?.to_string())
}

/// Creates the alice account and returns the session cookie setup gives.
async fn setup_alice(state: &Arc<AppState>, password: &str) -> String {
    let response = setup_create(
        State(state.clone()),
        Form(SetupForm {
            username: "alice".into(),
            password: password.into(),
            confirm_password: password.into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
    set_cookie(&response).expect("setup signs the browser in")
}

async fn sign_in(app: &Router, username: &str, password: &str) -> String {
    let response = app
        .clone()
        .oneshot(post(
            "/login",
            None,
            &format!("username={username}&password={password}"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    set_cookie(&response).expect("login sets the session cookie")
}

#[tokio::test]
async fn credential_round_trips_through_the_database() {
    let db = database().await;
    assert!(
        pi_credential(&db, "u1", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .is_none()
    );

    store_pi_credential(
        &db,
        "u1",
        CODEX_PI_PROVIDER,
        &credential("access_1", "acct_1"),
    )
    .await
    .unwrap();
    assert!(
        pi_credential(&db, "u2", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .is_none()
    );
    let stored = pi_credential(&db, "u1", CODEX_PI_PROVIDER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored["type"], "oauth");
    assert_eq!(stored["accountId"], "acct_1");

    // A refresh replaces the stored access token rather than appending.
    store_pi_credential(
        &db,
        "u1",
        CODEX_PI_PROVIDER,
        &credential("access_2", "acct_1"),
    )
    .await
    .unwrap();
    assert_eq!(
        pi_credential(&db, "u1", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .unwrap()["access"],
        "access_2"
    );
}

// The env lock spans awaits on purpose: the runtime is current-thread, so the
// guard never crosses threads and serialises env access against other tests.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn legacy_auth_json_is_imported_once_and_never_overrides_the_database() {
    let _lock = ENV_LOCK.lock();
    let agent_dir =
        std::env::temp_dir().join(format!("kindle-recipes-auth-test-{}", std::process::id()));
    fs::create_dir_all(&agent_dir).unwrap();
    unsafe {
        std::env::set_var("PI_CODING_AGENT_DIR", &agent_dir);
    }
    let db = database().await;

    // No file yet: nothing to import.
    import_legacy_codex_auth(&db).await.unwrap();
    assert!(
        pi_credential(&db, "", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .is_none()
    );

    // A legacy Pi CLI credential is imported once.
    fs::write(
        agent_dir.join("auth.json"),
        r#"{"openai-codex":{"type":"oauth","access":"a","refresh":"r","expires":1,"accountId":"x"}}"#,
    )
    .unwrap();
    import_legacy_codex_auth(&db).await.unwrap();
    assert_eq!(
        pi_credential(&db, "", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .unwrap()["accountId"],
        "x"
    );

    // Later changes to the file never overwrite the database.
    fs::write(
        agent_dir.join("auth.json"),
        r#"{"openai-codex":{"type":"oauth","access":"b","refresh":"r","expires":1,"accountId":"y"}}"#,
    )
    .unwrap();
    import_legacy_codex_auth(&db).await.unwrap();
    assert_eq!(
        pi_credential(&db, "", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .unwrap()["accountId"],
        "x"
    );

    unsafe {
        std::env::remove_var("PI_CODING_AGENT_DIR");
    }
    fs::remove_dir_all(&agent_dir).ok();
}

#[test]
fn password_hashes_verify_and_reject() {
    let hash = hash_password("hunter2").unwrap();
    assert!(verify_password("hunter2", &hash));
    assert!(!verify_password("nope", &hash));
    assert!(!verify_password("hunter2", "not a password hash"));
}

#[tokio::test]
async fn whitespace_only_passwords_are_rejected_and_stored_exactly() {
    let db = database().await;
    assert!(matches!(
        create_user(&db, "alice", "   ", "   ").await,
        Err(crate::AppError::BadRequest(_))
    ));
    create_user(&db, "alice", " spaced ", " spaced ")
        .await
        .unwrap();
    let stored: String =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE username='alice'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(verify_password(" spaced ", &stored));
    assert!(!verify_password("spaced", &stored));
}

#[tokio::test]
async fn first_setup_creates_user_and_claims_orphans() {
    let db = database().await;
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,title,created_at,updated_at) VALUES('orphan','Orphan',?,?)",
    )
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ai_drafts(id,operation,recipe_json,sources_json,created_at,expires_at)
         VALUES('draft','generate','{}','[]',?,?)",
    )
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pi_credentials(user_id,provider,credential_json,updated_at)
         VALUES('','openai-codex','{}',?)",
    )
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO app_settings(user_id,key,value,updated_at) VALUES('','ai_provider','claude',?)",
    )
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();

    let response = setup_create(
        State(state(db.clone())),
        Form(SetupForm {
            username: "alice".into(),
            password: "password-1".into(),
            confirm_password: "password-1".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username='alice'")
        .fetch_one(&db)
        .await
        .unwrap();
    let recipe_owner: String = sqlx::query_scalar("SELECT user_id FROM recipes WHERE id='orphan'")
        .fetch_one(&db)
        .await
        .unwrap();
    let draft_owner: String = sqlx::query_scalar("SELECT user_id FROM ai_drafts WHERE id='draft'")
        .fetch_one(&db)
        .await
        .unwrap();
    let credential_owner: String =
        sqlx::query_scalar("SELECT user_id FROM pi_credentials WHERE provider='openai-codex'")
            .fetch_one(&db)
            .await
            .unwrap();
    let setting_owner: String =
        sqlx::query_scalar("SELECT user_id FROM app_settings WHERE key='ai_provider'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(recipe_owner, user_id);
    assert_eq!(draft_owner, user_id);
    assert_eq!(credential_owner, user_id);
    assert_eq!(setting_owner, user_id);

    setup_create(
        State(state(db.clone())),
        Form(SetupForm {
            username: "bob".into(),
            password: "anything".into(),
            confirm_password: "anything".into(),
        }),
    )
    .await
    .unwrap();
    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(user_count, 1);
}

#[tokio::test]
async fn setup_requires_matching_passwords_of_at_least_eight_characters() {
    let db = database().await;
    let state = state(db.clone());
    let cases = [
        (
            "alice",
            "password-1",
            "password-2",
            "Passwords do not match.",
        ),
        (
            "alice",
            "short",
            "short",
            "Password must be at least 8 characters.",
        ),
        ("alice", "      ", "      ", "Password is required."),
    ];
    for (username, password, confirm, expected) in cases {
        let response = setup_create(
            State(state.clone()),
            Form(SetupForm {
                username: username.into(),
                password: password.into(),
                confirm_password: confirm.into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response_body(response).await.contains(expected));
    }
    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(user_count, 0);
}

#[tokio::test]
async fn signed_out_visitors_go_to_setup_then_to_the_login_page() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());

    for path in ["/", "/login"] {
        let response = app.clone().oneshot(request(path, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
        assert_eq!(location(&response), "/setup", "{path}");
    }

    setup_alice(&state, "secret-pass").await;
    for (req, expected) in [
        (request("/", None), "/login"),
        (
            request("/recipes/r1?view=chart&step=2", None),
            "/login?next=/recipes/r1%3Fview%3Dchart%26step%3D2",
        ),
        // A signed-out form post cannot resume after sign-in.
        (post("/settings/step-moments", None, "enabled=on"), "/login"),
        (request("/", Some("recipe_session=not-a-session")), "/login"),
    ] {
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), expected);
    }

    let response = app
        .clone()
        .oneshot(request("/healthz", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn login_checks_credentials_and_returns_to_the_requested_page() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());
    setup_alice(&state, "secret-pass").await;

    for (path, next) in [
        (
            "/login?next=/recipes/r1%3Fview%3Dchart",
            "/recipes/r1?view=chart",
        ),
        ("/login?next=//evil.example", "/"),
    ] {
        let response = app.clone().oneshot(request(path, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_body(response).await;
        assert!(
            body.contains(&format!(r#"name="next" value="{next}""#)),
            "{body}"
        );
    }

    for form in [
        "username=alice&password=wrong-pass",
        "username=mallory&password=secret-pass",
    ] {
        let response = app
            .clone()
            .oneshot(post("/login", None, form))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{form}");
        assert!(set_cookie(&response).is_none(), "{form}");
        assert!(
            response_body(response)
                .await
                .contains("Username or password is incorrect."),
            "{form}"
        );
    }

    for (next, expected) in [
        ("%2Frecipes%2Fr1%3Fview%3Dchart", "/recipes/r1?view=chart"),
        ("", "/"),
        ("%2F%2Fevil.example", "/"),
        ("%2F%5Cevil.example", "/"),
        ("https%3A%2F%2Fevil.example", "/"),
    ] {
        // The username is trimmed, as setup stores it.
        let response = app
            .clone()
            .oneshot(post(
                "/login",
                None,
                &format!("username=+alice+&password=secret-pass&next={next}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{next}");
        assert_eq!(location(&response), expected, "{next}");
        let header = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(
            header.contains("HttpOnly") && header.contains("SameSite=Lax"),
            "{header}"
        );
        let cookie = set_cookie(&response).unwrap();
        let token = cookie.strip_prefix("recipe_session=").unwrap();
        let stored_raw: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE token_hash = ?")
                .bind(token)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stored_raw, 0, "the table keeps only token hashes");
        let response = app
            .clone()
            .oneshot(request("/", Some(&format!("theme=dark; {cookie}"))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn sessions_scope_items_and_end_on_logout_or_expiry() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());
    let alice = setup_alice(&state, "secret-pass").await;
    let alice_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username='alice'")
        .fetch_one(&db)
        .await
        .unwrap();
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at)
         VALUES('alice-recipe','Alice recipe',?,?,?)",
    )
    .bind(&alice_id)
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    let bob_hash = hash_password("bob-secret").unwrap();
    sqlx::query("INSERT INTO users(id,username,password_hash,created_at) VALUES('bob','bob',?,?)")
        .bind(bob_hash)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();
    let bob = sign_in(&app, "bob", "bob-secret").await;

    let response = app
        .clone()
        .oneshot(request("/recipes/alice-recipe", Some(&alice)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(request("/recipes/alice-recipe", Some(&bob)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // A session not extended for a day gets a full new lifetime, and the
    // response sends the cookie again with it.
    sqlx::query("UPDATE sessions SET expires_at = ? WHERE user_id = 'bob'")
        .bind((Utc::now() + Duration::days(2)).to_rfc3339())
        .execute(&db)
        .await
        .unwrap();
    let response = app.clone().oneshot(request("/", Some(&bob))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(set_cookie(&response).as_deref(), Some(bob.as_str()));
    let header = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(header.contains("Max-Age=2592000"), "{header}");
    let expires: String =
        sqlx::query_scalar("SELECT expires_at FROM sessions WHERE user_id = 'bob'")
            .fetch_one(&db)
            .await
            .unwrap();
    let expires = DateTime::parse_from_rfc3339(&expires).unwrap();
    assert!(expires > Utc::now() + Duration::days(29));
    // Within a day of that extension the session is only read.
    let response = app.clone().oneshot(request("/", Some(&bob))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(set_cookie(&response).is_none());

    sqlx::query("UPDATE sessions SET expires_at = ? WHERE user_id = 'bob'")
        .bind((Utc::now() - Duration::minutes(1)).to_rfc3339())
        .execute(&db)
        .await
        .unwrap();
    let response = app.clone().oneshot(request("/", Some(&bob))).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");

    let response = app
        .clone()
        .oneshot(post("/logout", Some(&alice), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    let header = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        header.starts_with("recipe_session=;") && header.contains("Max-Age=0"),
        "{header}"
    );
    let response = app
        .clone()
        .oneshot(request("/", Some(&alice)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
}

async fn response_body(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn reset_password_requires_current_password_and_updates_login() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());

    let setup_cookie = setup_alice(&state, "old-secret").await;
    let other_device = sign_in(&app, "alice", "old-secret").await;
    let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username='alice'")
        .fetch_one(&db)
        .await
        .unwrap();
    let user = AuthUser {
        id: user_id.clone(),
    };

    // The page renders through the router for a signed-in user.
    let response = app
        .clone()
        .oneshot(request("/settings/password", Some(&setup_cookie)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response_body(response).await.contains("Reset password"));

    // Wrong current password: form re-rendered with an error, hash untouched.
    let response = reset_password(
        State(state.clone()),
        user.clone(),
        Form(ResetPasswordForm {
            current_password: "wrong".into(),
            new_password: "reset-secret".into(),
            confirm_password: "reset-secret".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response_body(response)
            .await
            .contains("Current password is incorrect.")
    );
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(&user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(verify_password("old-secret", &stored));

    // Blank new password: form re-rendered with an error, hash untouched.
    let response = reset_password(
        State(state.clone()),
        user.clone(),
        Form(ResetPasswordForm {
            current_password: "old-secret".into(),
            new_password: "   ".into(),
            confirm_password: "   ".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response_body(response)
            .await
            .contains("New password is required.")
    );

    // Mismatched confirmation and a short password are both refused.
    for (new_password, confirm_password, expected) in [
        ("reset-secret", "reset-secres", "Passwords do not match."),
        ("short", "short", "Password must be at least 8 characters."),
    ] {
        let response = reset_password(
            State(state.clone()),
            user.clone(),
            Form(ResetPasswordForm {
                current_password: "old-secret".into(),
                new_password: new_password.into(),
                confirm_password: confirm_password.into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response_body(response).await.contains(expected));
    }
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(&user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(verify_password("old-secret", &stored));

    // Correct current password: hash replaced and the redirect lands on Settings.
    let response = reset_password(
        State(state.clone()),
        user.clone(),
        Form(ResetPasswordForm {
            current_password: "old-secret".into(),
            new_password: "reset-secret".into(),
            confirm_password: "reset-secret".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/settings#settings-account");
    assert!(
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|value| value
                .as_bytes()
                .starts_with(b"recipe_flash=password_changed;")),
        "the notice rides in the flash cookie"
    );
    let new_cookie = set_cookie(&response).expect("this browser gets a new session");
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(&user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(verify_password("reset-secret", &stored));
    assert!(!verify_password("old-secret", &stored));

    // Every earlier session ends; the new cookie keeps this browser signed in.
    for cookie in [&setup_cookie, &other_device] {
        let response = app
            .clone()
            .oneshot(request("/", Some(cookie)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/login");
    }
    let response = app
        .clone()
        .oneshot(request("/", Some(&new_cookie)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The login page now accepts the reset password and rejects the old one.
    let response = app
        .clone()
        .oneshot(post("/login", None, "username=alice&password=old-secret"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(set_cookie(&response).is_none());
    sign_in(&app, "alice", "reset-secret").await;
}

/// Every `name=value` the response sets, in header order.
fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok()?.split(';').next())
        .map(str::to_string)
        .collect()
}

/// The value of the named cookie among those a response sets.
fn cookie_set(response: &Response, name: &str) -> Option<String> {
    set_cookies(response)
        .into_iter()
        .find_map(|cookie| cookie.strip_prefix(&format!("{name}=")).map(str::to_string))
}

/// The browser's `Cookie` header for an active session and remembered ones.
fn browser(active: &str, remembered: &[&str]) -> String {
    format!(
        "recipe_session={active}; recipe_accounts={}",
        remembered.join(".")
    )
}

fn token(cookie: &str) -> &str {
    cookie.strip_prefix("recipe_session=").unwrap()
}

async fn user_id(db: &SqlitePool, username: &str) -> String {
    sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(username)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn add_account(db: &SqlitePool, username: &str, password: &str) {
    sqlx::query("INSERT INTO users(id,username,password_hash,created_at) VALUES(?,?,?,?)")
        .bind(format!("{username}-id"))
        .bind(username)
        .bind(hash_password(password).unwrap())
        .bind(stamp())
        .execute(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_account_menu_creates_accounts_and_switches_between_them() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());
    let alice = token(&setup_alice(&state, "secret-pass").await).to_string();
    let alice_id = user_id(&db, "alice").await;
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at)
         VALUES('alice-recipe','Alice recipe',?,?,?)",
    )
    .bind(&alice_id)
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();

    // Every signed-in page carries the menu for the active account.
    let response = app
        .clone()
        .oneshot(request("/", Some(&browser(&alice, &[]))))
        .await
        .unwrap();
    let body = response_body(response).await;
    assert!(body.contains("data-account-menu"), "{body}");
    assert!(
        body.contains(
            "<p class=\"ticket-bar ticket-head\"><span class=\"account-name\">alice</span>"
        )
    );
    assert!(body.contains("href=\"/accounts/new\""));
    assert!(!body.contains("Sign out of all accounts"));

    let response = app
        .clone()
        .oneshot(request("/accounts/new", Some(&browser(&alice, &[]))))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Usernames are unique regardless of case, and the password rules apply.
    for (form, expected) in [
        (
            "username=Alice&password=bob-secret&confirm_password=bob-secret",
            "That username is taken.",
        ),
        (
            "username=bob&password=short&confirm_password=short",
            "Password must be at least 8 characters.",
        ),
        (
            "username=+&password=bob-secret&confirm_password=bob-secret",
            "Username is required.",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(post("/accounts/new", Some(&browser(&alice, &[])), form))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{form}");
        assert!(set_cookies(&response).is_empty(), "{form}");
        assert!(response_body(response).await.contains(expected), "{form}");
    }

    // Creating bob switches the browser to him; alice stays signed in behind.
    let response = app
        .clone()
        .oneshot(post(
            "/accounts/new",
            Some(&browser(&alice, &[])),
            "username=+bob+&password=bob-secret&confirm_password=bob-secret",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
    let bob = cookie_set(&response, "recipe_session").unwrap();
    assert_ne!(bob, alice);
    assert_eq!(
        cookie_set(&response, "recipe_accounts").as_deref(),
        Some(alice.as_str())
    );
    let bob_id = user_id(&db, "bob").await;

    // Bob has his own empty library and his own settings.
    let response = app
        .clone()
        .oneshot(request(
            "/recipes/alice-recipe",
            Some(&browser(&bob, &[&alice])),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app
        .clone()
        .oneshot(request("/", Some(&browser(&bob, &[&alice]))))
        .await
        .unwrap();
    assert!(set_cookies(&response).is_empty(), "nothing changed");
    let body = response_body(response).await;
    assert!(
        body.contains(
            "<p class=\"ticket-bar ticket-head\"><span class=\"account-name\">bob</span>"
        )
    );
    assert!(body.contains(&format!("name=\"user_id\" value=\"{alice_id}\"")));
    assert!(body.contains("Sign out of all accounts"));

    // Switching makes alice active again and remembers bob.
    let response = app
        .clone()
        .oneshot(post(
            "/accounts/switch",
            Some(&browser(&bob, &[&alice])),
            &format!("user_id={alice_id}"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
    assert_eq!(
        cookie_set(&response, "recipe_session").as_deref(),
        Some(alice.as_str())
    );
    assert_eq!(
        cookie_set(&response, "recipe_accounts").as_deref(),
        Some(bob.as_str())
    );
    let response = app
        .clone()
        .oneshot(request(
            "/recipes/alice-recipe",
            Some(&browser(&alice, &[&bob])),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // An account this browser is not signed in to needs its password, and
    // an unknown account is not found.
    add_account(&db, "carol", "carol-secret").await;
    let response = app
        .clone()
        .oneshot(post(
            "/accounts/switch",
            Some(&browser(&alice, &[&bob])),
            "user_id=carol-id",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login?username=carol");
    assert!(set_cookies(&response).is_empty());
    let response = app
        .clone()
        .oneshot(post(
            "/accounts/switch",
            Some(&browser(&alice, &[&bob])),
            "user_id=nobody",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app
        .clone()
        .oneshot(request("/", Some(&browser(&alice, &[&bob]))))
        .await
        .unwrap();
    let body = response_body(response).await;
    assert!(body.contains("href=\"/login?username=carol\""), "{body}");
    assert!(body.contains(&format!("name=\"user_id\" value=\"{bob_id}\"")));
}

#[tokio::test]
async fn signing_in_adds_an_account_and_signing_out_falls_back_to_the_next() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());
    let alice = token(&setup_alice(&state, "secret-pass").await).to_string();
    add_account(&db, "carol", "carol-secret").await;

    // With a session, the sign-in page adds an account.
    let response = app
        .clone()
        .oneshot(request(
            "/login?username=carol",
            Some(&browser(&alice, &[])),
        ))
        .await
        .unwrap();
    let body = response_body(response).await;
    assert!(body.contains("Add an account"), "{body}");
    assert!(body.contains(r#"name="username" type="text" value="carol""#));
    let response = app.clone().oneshot(request("/login", None)).await.unwrap();
    let body = response_body(response).await;
    assert!(!body.contains("Add an account"));
    assert!(!body.contains("data-account-menu"));

    let response = app
        .clone()
        .oneshot(post(
            "/login",
            Some(&browser(&alice, &[])),
            "username=carol&password=carol-secret",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let carol = cookie_set(&response, "recipe_session").unwrap();
    assert_eq!(
        cookie_set(&response, "recipe_accounts").as_deref(),
        Some(alice.as_str())
    );

    // Signing in to the active account again replaces its session.
    let response = app
        .clone()
        .oneshot(post(
            "/login",
            Some(&browser(&carol, &[&alice])),
            "username=carol&password=carol-secret",
        ))
        .await
        .unwrap();
    let carol_again = cookie_set(&response, "recipe_session").unwrap();
    assert_ne!(carol_again, carol);
    assert_eq!(
        cookie_set(&response, "recipe_accounts").as_deref(),
        Some(alice.as_str())
    );
    let sessions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id='carol-id'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(sessions, 1);
    let carol = carol_again;

    // Signing out of carol hands the browser back to alice.
    let response = app
        .clone()
        .oneshot(post("/logout", Some(&browser(&carol, &[&alice])), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
    assert_eq!(
        set_cookies(&response),
        vec![
            format!("recipe_session={alice}"),
            "recipe_accounts=".to_string()
        ]
    );
    let response = app
        .clone()
        .oneshot(request("/", Some(&browser(&carol, &[]))))
        .await
        .unwrap();
    assert_eq!(location(&response), "/login");

    // Signing out of the last account clears both cookies.
    let response = app
        .clone()
        .oneshot(post("/logout", Some(&browser(&alice, &[])), ""))
        .await
        .unwrap();
    assert_eq!(location(&response), "/login");
    assert_eq!(
        set_cookies(&response),
        vec![
            "recipe_session=".to_string(),
            "recipe_accounts=".to_string()
        ]
    );
}

#[tokio::test]
async fn an_ended_session_hands_over_to_a_remembered_account() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());
    let alice = token(&setup_alice(&state, "secret-pass").await).to_string();
    add_account(&db, "carol", "carol-secret").await;
    add_account(&db, "dave", "dave-secret").await;
    let carol = token(&sign_in(&app, "carol", "carol-secret").await).to_string();
    let dave = token(&sign_in(&app, "dave", "dave-secret").await).to_string();

    // Carol's session ends: alice takes over, and the accounts cookie drops
    // carol's token and anything malformed.
    sqlx::query("UPDATE sessions SET expires_at = ? WHERE user_id = 'carol-id'")
        .bind((Utc::now() - Duration::minutes(1)).to_rfc3339())
        .execute(&db)
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(request(
            "/",
            Some(&browser(&carol, &[&alice, "junk", &dave, &carol])),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        set_cookies(&response),
        vec![
            format!("recipe_session={alice}"),
            format!("recipe_accounts={dave}")
        ]
    );
    let body = response_body(response).await;
    assert!(
        body.contains(
            "<p class=\"ticket-bar ticket-head\"><span class=\"account-name\">alice</span>"
        )
    );

    // Signing out of all accounts ends every session the browser holds.
    let response = app
        .clone()
        .oneshot(post("/logout/all", Some(&browser(&alice, &[&dave])), ""))
        .await
        .unwrap();
    assert_eq!(location(&response), "/login");
    assert_eq!(
        set_cookies(&response),
        vec![
            "recipe_session=".to_string(),
            "recipe_accounts=".to_string()
        ]
    );
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id <> 'carol-id'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// Every table that holds an account's data, with the column whose value
/// starts with the owner's user id once `seed_account` has filled it.
const OWNED_TABLES: &[(&str, &str)] = &[
    ("recipes", "id"),
    ("recipe_blocks", "id"),
    ("recipe_step_ingredients", "id"),
    ("recipe_sources", "id"),
    ("recipe_versions", "id"),
    ("recipe_version_counters", "recipe_id"),
    ("recipe_embeddings", "recipe_id"),
    ("recipe_thumbnail_candidates", "recipe_id"),
    ("recipe_moment_vectors", "recipe_id"),
    ("recipe_moment_stills", "recipe_id"),
    ("recipe_step_moments", "step_id"),
    ("recipe_chat_messages", "id"),
    ("recipe_notes", "id"),
    ("ai_drafts", "id"),
    ("draft_thumbnails", "draft_id"),
    ("draft_moment_vectors", "draft_id"),
    ("draft_moment_stills", "draft_id"),
    ("draft_step_moments", "draft_id"),
    ("app_settings", "user_id"),
    ("ai_endpoints", "id"),
    ("pi_credentials", "user_id"),
    ("sessions", "user_id"),
    ("users", "id"),
];

/// Seeds a row in every per-account table for `user_id`, each keyed by an id
/// that starts with the user id: a recipe with its blocks, sources, history,
/// search vector, photos, moments, chat, and note; an alteration draft and a
/// new-recipe draft with thumbnails and moments; and settings and AI logins.
async fn seed_account(db: &SqlitePool, user_id: &str) {
    for statement in [
        "INSERT INTO recipes(id,title,created_at,updated_at,user_id,thumbnail_jpeg)
         VALUES(?1||'-recipe','Soup',?2,?2,?1,x'ffd8')",
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text)
         VALUES(?1||'-ingredient',?1||'-recipe','ingredient',0,'salt')",
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text)
         VALUES(?1||'-step',?1||'-recipe','step',0,'stir')",
        "INSERT INTO recipe_step_ingredients(id,step_id,position,text)
         VALUES(?1||'-use',?1||'-step',0,'salt')",
        "INSERT INTO recipe_sources(id,recipe_id,position,url)
         VALUES(?1||'-source',?1||'-recipe',0,'https://example.com/'||?1)",
        "INSERT INTO recipe_versions(id,recipe_id,version,snapshot,created_at)
         VALUES(?1||'-version',?1||'-recipe',1,'{}',?2)",
        "INSERT INTO recipe_version_counters(recipe_id,last_version) VALUES(?1||'-recipe',1)",
        "INSERT INTO recipe_embeddings(recipe_id,document,vector,updated_at)
         VALUES(?1||'-recipe','Soup',x'00',?2)",
        "INSERT INTO recipe_thumbnail_candidates(recipe_id,idx,seconds,jpeg)
         VALUES(?1||'-recipe',0,1,x'ffd8')",
        "INSERT INTO recipe_moment_vectors(recipe_id,kind,seconds,vector)
         VALUES(?1||'-recipe','frame',1.0,x'00')",
        "INSERT INTO recipe_moment_stills(recipe_id,seconds,jpeg) VALUES(?1||'-recipe',1,x'ffd8')",
        "INSERT INTO recipe_step_moments(step_id,seconds,source) VALUES(?1||'-step',1,'frame')",
        "INSERT INTO recipe_chat_messages(id,user_id,recipe_id,role,content,created_at)
         VALUES(?1||'-chat',?1,?1||'-recipe','user','More salt?',?2)",
        "INSERT INTO recipe_notes(id,user_id,recipe_id,body,photo_jpeg,chat_message_id,created_at,updated_at)
         VALUES(?1||'-note',?1,?1||'-recipe','Good',x'ffd8',?1||'-chat',?2,?2)",
        "INSERT INTO ai_drafts(id,recipe_id,operation,recipe_json,sources_json,created_at,expires_at,user_id,chat_message_id)
         VALUES(?1||'-alter',?1||'-recipe','alter','{}','[]',?2,?2,?1,?1||'-chat')",
        "INSERT INTO ai_drafts(id,recipe_id,operation,recipe_json,sources_json,created_at,expires_at,user_id)
         VALUES(?1||'-draft',NULL,'generate','{}','[]',?2,?2,?1)",
        "INSERT INTO draft_thumbnails(draft_id,idx,seconds,jpeg) VALUES(?1||'-draft',0,1,x'ffd8')",
        "INSERT INTO draft_thumbnails(draft_id,idx,seconds,jpeg) VALUES(?1||'-alter',0,1,x'ffd8')",
        "INSERT INTO draft_moment_vectors(draft_id,kind,seconds,vector)
         VALUES(?1||'-draft','audio',1.0,x'00')",
        "INSERT INTO draft_moment_stills(draft_id,seconds,jpeg) VALUES(?1||'-draft',1,x'ffd8')",
        "INSERT INTO draft_step_moments(draft_id,step_index,seconds,source)
         VALUES(?1||'-draft',0,1,'audio')",
        "INSERT INTO app_settings(user_id,key,value,updated_at) VALUES(?1,'recipe_chat','on',?2)",
        "INSERT INTO ai_endpoints(id,user_id,name,spec,base_url,api_key,created_at,updated_at)
         VALUES(?1||'-endpoint',?1,'OpenAI API','openai','https://api.openai.com/v1','sk-test',?2,?2)",
        "INSERT INTO pi_credentials(user_id,provider,credential_json,updated_at)
         VALUES(?1,'openai-codex','{}',?2)",
    ] {
        let mut query = sqlx::query(statement).bind(user_id);
        if statement.contains("?2") {
            query = query.bind(stamp());
        }
        query.execute(db).await.unwrap();
    }
}

/// Rows per table that belong to `user_id`.
async fn owned_rows(db: &SqlitePool, user_id: &str) -> Vec<(&'static str, i64)> {
    let mut counts = Vec::new();
    for (table, column) in OWNED_TABLES {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE {column} LIKE ?1 || '%'"
        ))
        .bind(user_id)
        .fetch_one(db)
        .await
        .unwrap();
        counts.push((*table, count));
    }
    counts
}

#[tokio::test]
async fn deleting_an_account_removes_everything_it_owns_and_nothing_else() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());
    let alice = token(&setup_alice(&state, "secret-pass").await).to_string();
    let alice_id = user_id(&db, "alice").await;
    add_account(&db, "bob", "bob-secret").await;
    let bob = token(&sign_in(&app, "bob", "bob-secret").await).to_string();
    let alice_elsewhere = sign_in(&app, "alice", "secret-pass").await;
    seed_account(&db, &alice_id).await;
    seed_account(&db, "bob-id").await;
    state
        .thumbnail_jobs
        .lock()
        .insert(format!("{alice_id}-recipe"));
    state
        .moment_jobs
        .lock()
        .insert(format!("{alice_id}-recipe"));
    state.codex_flows.lock().insert(
        "flow".into(),
        crate::CodexFlow {
            user_id: alice_id.clone(),
            device_auth_id: String::new(),
            user_code: String::new(),
            expires_at: i64::MAX,
        },
    );
    let alice_before = owned_rows(&db, &alice_id).await;
    let bob_before = owned_rows(&db, "bob-id").await;
    for (table, count) in alice_before.iter().chain(&bob_before) {
        assert!(*count > 0, "{table} was not seeded");
    }

    // The confirmation page alone deletes nothing.
    let response = app
        .clone()
        .oneshot(request(
            "/settings/delete-account",
            Some(&browser(&alice, &[&bob])),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    assert!(body.contains("Delete account alice</button>"), "{body}");
    assert!(body.contains(r#"action="/settings/delete-account" hx-boost="false" data-confirm="#));
    assert!(body.contains(r#"href="/settings">Cancel</a>"#));
    assert!(!body.contains("current_password"));
    assert_eq!(owned_rows(&db, &alice_id).await, alice_before);

    // Deleting signs this browser over to the remembered account.
    let response = app
        .clone()
        .oneshot(post(
            "/settings/delete-account",
            Some(&browser(&alice, &[&bob])),
            "",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
    assert_eq!(
        cookie_set(&response, "recipe_session").as_deref(),
        Some(bob.as_str())
    );
    assert_eq!(
        cookie_set(&response, "recipe_accounts").as_deref(),
        Some("")
    );
    assert_eq!(
        cookie_set(&response, "recipe_flash").as_deref(),
        Some("account_deleted")
    );
    for (table, count) in owned_rows(&db, &alice_id).await {
        assert_eq!(count, 0, "{table} still holds the deleted account's rows");
    }
    let orphans: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM recipe_blocks WHERE recipe_id NOT IN (SELECT id FROM recipes))
              + (SELECT COUNT(*) FROM draft_thumbnails WHERE draft_id NOT IN (SELECT id FROM ai_drafts))
              + (SELECT COUNT(*) FROM recipe_thumbnail_candidates WHERE recipe_id NOT IN (SELECT id FROM recipes))",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(orphans, 0);
    assert_eq!(owned_rows(&db, "bob-id").await, bob_before);
    assert!(state.thumbnail_jobs.lock().is_empty());
    assert!(state.moment_jobs.lock().is_empty());
    assert!(state.codex_flows.lock().is_empty());

    // The library opens as bob with the notice; alice's other browser is out.
    let response = app
        .clone()
        .oneshot(request(
            "/",
            Some(&format!(
                "recipe_session={bob}; recipe_flash=account_deleted"
            )),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    assert!(body.contains("Account deleted."), "{body}");
    assert!(!body.contains("alice"), "the menu no longer lists alice");
    let response = app
        .clone()
        .oneshot(request("/", Some(&alice_elsewhere)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    let response = app
        .clone()
        .oneshot(post("/login", None, "username=alice&password=secret-pass"))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "alice can no longer sign in"
    );
}

#[tokio::test]
async fn deleting_the_shared_account_warns_and_hands_ai_logins_to_the_next_oldest() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());
    let alice = token(&setup_alice(&state, "secret-pass").await).to_string();
    let alice_id = user_id(&db, "alice").await;
    add_account(&db, "carol", "carol-secret").await;
    add_account(&db, "bob", "bob-secret").await;
    for (id, created_at) in [
        (alice_id.as_str(), "2026-01-01T00:00:00+00:00"),
        ("bob-id", "2026-02-01T00:00:00+00:00"),
        ("carol-id", "2026-03-01T00:00:00+00:00"),
    ] {
        sqlx::query("UPDATE users SET created_at = ? WHERE id = ?")
            .bind(created_at)
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO app_settings(user_id,key,value,updated_at) VALUES('carol-id','ai_logins','own',?)")
        .bind(stamp())
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(crate::ai_account(&db, "bob-id").await.unwrap(), alice_id);
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "carol-id"
    );

    // An account that is not the shared one gets no warning.
    let bob = token(&sign_in(&app, "bob", "bob-secret").await).to_string();
    let response = app
        .clone()
        .oneshot(request(
            "/settings/delete-account",
            Some(&browser(&bob, &[])),
        ))
        .await
        .unwrap();
    let body = response_body(response).await;
    assert!(!body.contains("AI logins are shared"), "{body}");

    let response = app
        .clone()
        .oneshot(request(
            "/settings/delete-account",
            Some(&browser(&alice, &[])),
        ))
        .await
        .unwrap();
    let body = response_body(response).await;
    assert!(
        body.contains("This account’s AI logins are shared."),
        "{body}"
    );
    assert!(
        body.contains("bob uses them."),
        "carol uses her own: {body}"
    );
    assert!(
        body.contains("AI setup of bob, the next oldest account"),
        "{body}"
    );
    assert!(body.contains("Delete account alice</button>"));

    let response = app
        .clone()
        .oneshot(post(
            "/settings/delete-account",
            Some(&browser(&alice, &[])),
            "",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    let response = app
        .clone()
        .oneshot(request("/login", Some("recipe_flash=account_deleted")))
        .await
        .unwrap();
    let body = response_body(response).await;
    assert!(body.contains("Account deleted."), "{body}");
    assert_eq!(
        crate::shared_ai_account(&db).await.unwrap(),
        Some(("bob-id".to_string(), "bob".to_string()))
    );
    assert_eq!(crate::ai_account(&db, "bob-id").await.unwrap(), "bob-id");
    assert_eq!(
        crate::ai_account(&db, "carol-id").await.unwrap(),
        "carol-id"
    );
    sqlx::query("DELETE FROM app_settings WHERE user_id='carol-id'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(crate::ai_account(&db, "carol-id").await.unwrap(), "bob-id");
}

#[tokio::test]
async fn deleting_the_last_account_signs_out_and_leads_to_setup() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());
    let alice = token(&setup_alice(&state, "secret-pass").await).to_string();
    let alice_id = user_id(&db, "alice").await;
    seed_account(&db, &alice_id).await;

    let response = app
        .clone()
        .oneshot(post(
            "/settings/delete-account",
            Some(&browser(&alice, &[])),
            "",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/login");
    assert_eq!(cookie_set(&response, "recipe_session").as_deref(), Some(""));
    assert_eq!(
        cookie_set(&response, "recipe_accounts").as_deref(),
        Some("")
    );
    for (table, count) in owned_rows(&db, &alice_id).await {
        assert_eq!(count, 0, "{table} still holds the deleted account's rows");
    }
    let response = app
        .clone()
        .oneshot(request("/login", Some("recipe_flash=account_deleted")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/setup");
    let response = app
        .clone()
        .oneshot(request("/", Some(&browser(&alice, &[]))))
        .await
        .unwrap();
    assert_eq!(location(&response), "/setup");
}

/// `POST /logout` with the session cookie and the given extra headers.
fn logout_with(cookie: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = post("/logout", Some(cookie), "");
    for (name, value) in headers {
        request.headers_mut().insert(
            header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            header::HeaderValue::from_str(value).unwrap(),
        );
    }
    request
}

#[tokio::test]
async fn forms_sent_from_another_origin_are_refused() {
    let state = state(database().await);
    let cookie = setup_alice(&state, "correct horse").await;
    let app = routes(state);

    for headers in [
        vec![("sec-fetch-site", "cross-site")],
        // Another port on the same host is the same site, and SameSite=Lax
        // sends the cookie along: only the origin check stops it.
        vec![("sec-fetch-site", "same-site")],
        vec![
            ("origin", "http://minibuntu:8082"),
            ("host", "minibuntu:3000"),
        ],
        vec![("origin", "null"), ("host", "minibuntu:3000")],
        vec![("origin", "http://minibuntu:3000")],
    ] {
        let response = app
            .clone()
            .oneshot(logout_with(&cookie, &headers))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{headers:?}");
        assert!(set_cookie(&response).is_none(), "{headers:?}");
        let body = response_body(response).await;
        assert!(body.contains("sent from another site"), "{body}");
    }
    // The session survived every refused sign-out.
    let response = app
        .clone()
        .oneshot(request("/", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Signing in from another site is refused too (login CSRF).
    let mut login = post("/login", None, "username=alice&password=correct horse");
    login
        .headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    let response = app.clone().oneshot(login).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(set_cookie(&response).is_none());

    // Links from elsewhere still open pages.
    let mut page = request("/", Some(&cookie));
    page.headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(page).await.unwrap().status(),
        StatusCode::OK
    );

    for headers in [
        vec![("sec-fetch-site", "same-origin")],
        vec![("sec-fetch-site", "none")],
        vec![
            ("origin", "http://minibuntu:3000"),
            ("host", "minibuntu:3000"),
        ],
        // Behind a TLS proxy the app sees plain HTTP and the browser's Host.
        vec![
            ("origin", "https://Recipes.example"),
            ("host", "recipes.example"),
        ],
        // No header says where it came from: an old browser or a script.
        vec![],
    ] {
        let session = sign_in(&app, "alice", "correct horse").await;
        let response = app
            .clone()
            .oneshot(logout_with(&session, &headers))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{headers:?}");
    }
}
