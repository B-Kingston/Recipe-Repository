use crate::auth::{AuthUser, hash_password, reset_password, setup_create, verify_password};
use crate::{
    AppState, ResetPasswordForm, SetupForm, codex_credential, import_legacy_codex_auth, routes,
    stamp, store_codex_credential,
};
use axum::{
    body::{Body, to_bytes},
    extract::{Form, State},
    http::{Request, StatusCode, header},
};
use base64::Engine;
use parking_lot::Mutex;
use serde_json::json;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, collections::HashSet, fs, sync::Arc};
use tower::ServiceExt;

static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        search_grounding: false,
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(None)),
        media_debug_runs: Arc::new(Mutex::new(HashMap::new())),
        thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
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

fn basic_header(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

fn request(path: &str, authorization: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(authorization) = authorization {
        builder = builder.header(header::AUTHORIZATION, authorization);
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn credential_round_trips_through_the_database() {
    let db = database().await;
    assert!(codex_credential(&db, "u1").await.unwrap().is_none());

    store_codex_credential(&db, "u1", &credential("access_1", "acct_1"))
        .await
        .unwrap();
    assert!(codex_credential(&db, "u2").await.unwrap().is_none());
    let stored = codex_credential(&db, "u1").await.unwrap().unwrap();
    assert_eq!(stored["type"], "oauth");
    assert_eq!(stored["accountId"], "acct_1");

    // A refresh replaces the stored access token rather than appending.
    store_codex_credential(&db, "u1", &credential("access_2", "acct_1"))
        .await
        .unwrap();
    assert_eq!(
        codex_credential(&db, "u1").await.unwrap().unwrap()["access"],
        "access_2"
    );
}

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
    assert!(codex_credential(&db, "").await.unwrap().is_none());

    // A legacy Pi CLI credential is imported once.
    fs::write(
        agent_dir.join("auth.json"),
        r#"{"openai-codex":{"type":"oauth","access":"a","refresh":"r","expires":1,"accountId":"x"}}"#,
    )
    .unwrap();
    import_legacy_codex_auth(&db).await.unwrap();
    assert_eq!(
        codex_credential(&db, "").await.unwrap().unwrap()["accountId"],
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
        codex_credential(&db, "").await.unwrap().unwrap()["accountId"],
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
            password: "x".into(),
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
async fn protected_routes_require_basic_auth_and_scope_items() {
    let db = database().await;
    let state = state(db.clone());
    let app = routes(state.clone());

    let response = app.clone().oneshot(request("/", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/setup");

    setup_create(
        State(state.clone()),
        Form(SetupForm {
            username: "alice".into(),
            password: "secret".into(),
        }),
    )
    .await
    .unwrap();
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

    let response = app.clone().oneshot(request("/", None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get(header::WWW_AUTHENTICATE).unwrap(),
        "Basic realm=\"Kindle Recipes\""
    );

    let alice_auth = basic_header("alice", "secret");
    let response = app
        .clone()
        .oneshot(request("/", Some(&alice_auth)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let wrong_auth = basic_header("alice", "wrong");
    let response = app
        .clone()
        .oneshot(request("/", Some(&wrong_auth)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .clone()
        .oneshot(request("/healthz", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bob_hash = hash_password("bob-secret").unwrap();
    sqlx::query("INSERT INTO users(id,username,password_hash,created_at) VALUES('bob','bob',?,?)")
        .bind(bob_hash)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();
    let alice_response = app
        .clone()
        .oneshot(request("/recipes/alice-recipe", Some(&alice_auth)))
        .await
        .unwrap();
    assert_eq!(alice_response.status(), StatusCode::OK);

    let bob_auth = basic_header("bob", "bob-secret");
    let bob_response = app
        .clone()
        .oneshot(request("/recipes/alice-recipe", Some(&bob_auth)))
        .await
        .unwrap();
    assert_eq!(bob_response.status(), StatusCode::NOT_FOUND);
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

    setup_create(
        State(state.clone()),
        Form(SetupForm {
            username: "alice".into(),
            password: "old-secret".into(),
        }),
    )
    .await
    .unwrap();
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
        .oneshot(request(
            "/settings/password",
            Some(&basic_header("alice", "old-secret")),
        ))
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
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get(header::LOCATION).unwrap(),
        "/settings"
    );
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(&user_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(verify_password("reset-secret", &stored));
    assert!(!verify_password("old-secret", &stored));

    // The router now accepts the reset password and rejects the old one.
    let response = app
        .clone()
        .oneshot(request("/", Some(&basic_header("alice", "reset-secret"))))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(request("/", Some(&basic_header("alice", "old-secret"))))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
