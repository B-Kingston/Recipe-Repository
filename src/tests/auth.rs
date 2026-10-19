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
    assert_eq!(recipe_owner, user_id);
    assert_eq!(draft_owner, user_id);
    assert_eq!(credential_owner, user_id);

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
    assert_eq!(location(&response), "/settings?notice=password_changed");
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
