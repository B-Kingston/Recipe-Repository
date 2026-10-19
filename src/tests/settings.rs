use crate::auth::AuthUser;
use crate::{
    AiGatewayForm, AppError, AppState, CLAUDE_PI_PROVIDER, CODEX_PI_PROVIDER, ClaudeExchangeForm,
    ClaudeFlow, EndpointForm, SettingsForm, add_endpoint, ai_gateway_credential, ai_provider,
    authorise_claude_exchange, delete_endpoint, disconnect_claude, disconnect_codex, find_endpoint,
    insert_endpoint, list_endpoints, mask_key, pi_credential, reconcile_ai_provider,
    selected_claude_model, selected_effort, selected_model, stamp, store_pi_credential,
    update_ai_gateway, update_settings,
};
use axum::{
    extract::{Form, Path, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use parking_lot::Mutex;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

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

async fn save(
    state: &Arc<AppState>,
    form: SettingsForm,
) -> Result<axum::response::Redirect, AppError> {
    update_settings(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Form(form),
    )
    .await
}

fn settings_form(provider: &str) -> SettingsForm {
    SettingsForm {
        model: "gpt-5.5".into(),
        claude_model: String::new(),
        endpoint_model: String::new(),
        endpoint_model_for: String::new(),
        reasoning_effort: String::new(),
        provider: provider.into(),
    }
}

fn claude_form(model: &str) -> SettingsForm {
    SettingsForm {
        model: String::new(),
        claude_model: model.into(),
        endpoint_model: String::new(),
        endpoint_model_for: String::new(),
        reasoning_effort: String::new(),
        provider: "claude".into(),
    }
}

async fn add(db: &SqlitePool, name: &str, spec: &str, base_url: &str, key: &str) -> String {
    insert_endpoint(db, "u1", name, spec, base_url, key, "")
        .await
        .unwrap()
}

async fn set_provider(db: &SqlitePool, value: &str) {
    sqlx::query(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('ai_provider', ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(value)
    .bind(stamp())
    .execute(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn gateway_credentials_are_saved_per_user() {
    let db = database().await;
    let _ = update_ai_gateway(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Form(AiGatewayForm {
            base_url: "https://ai-gateway.vercel.sh/v1/".into(),
            api_key: "vg-secret-abcdefgh".into(),
        }),
    )
    .await
    .unwrap();
    let credential = ai_gateway_credential(&db, "u1").await.unwrap().unwrap();
    assert_eq!(credential.api_key, "vg-secret-abcdefgh");
    assert_eq!(credential.base_url, "https://ai-gateway.vercel.sh/v1");
    assert!(ai_gateway_credential(&db, "u2").await.unwrap().is_none());
}

#[tokio::test]
async fn blank_gateway_key_preserves_the_saved_secret() {
    let db = database().await;
    for (base_url, api_key) in [
        ("https://first.example/v1", "vg-secret"),
        ("https://second.example/v1", ""),
    ] {
        let _ = update_ai_gateway(
            State(state(db.clone())),
            AuthUser { id: "u1".into() },
            Form(AiGatewayForm {
                base_url: base_url.into(),
                api_key: api_key.into(),
            }),
        )
        .await
        .unwrap();
    }
    let credential = ai_gateway_credential(&db, "u1").await.unwrap().unwrap();
    assert_eq!(credential.api_key, "vg-secret");
    assert_eq!(credential.base_url, "https://second.example/v1");
}

#[tokio::test]
async fn gateway_requires_a_key_and_valid_url() {
    let db = database().await;
    for form in [
        AiGatewayForm {
            base_url: "https://ai-gateway.vercel.sh/v1".into(),
            api_key: String::new(),
        },
        AiGatewayForm {
            base_url: "ai-gateway.vercel.sh/v1".into(),
            api_key: "vg-secret".into(),
        },
    ] {
        let result = update_ai_gateway(
            State(state(db.clone())),
            AuthUser { id: "u1".into() },
            Form(form),
        )
        .await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }
}

#[tokio::test]
async fn ai_provider_defaults_to_codex() {
    let db = database().await;
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
}

#[tokio::test]
async fn saving_codex_mode_stores_model_and_provider() {
    let db = database().await;
    let state = state(db.clone());
    let result = save(&state, settings_form("pi")).await;
    assert!(result.is_ok());
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
    assert_eq!(selected_model(&db, "gpt-5.5").await.unwrap(), "gpt-5.5");
}

#[tokio::test]
async fn saving_an_endpoint_switches_the_provider() {
    let db = database().await;
    let id = add(
        &db,
        "Claude",
        "anthropic",
        "https://api.anthropic.com",
        "sk-ant-test",
    )
    .await;
    let state = state(db.clone());
    let result = save(
        &state,
        SettingsForm {
            model: String::new(),
            claude_model: String::new(),
            endpoint_model: String::new(),
            endpoint_model_for: String::new(),
            reasoning_effort: String::new(),
            provider: id.clone(),
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(ai_provider(&db).await.unwrap(), id);
    // Endpoint mode never writes or requires a Codex model.
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM app_settings WHERE key = 'model'")
            .fetch_optional(&db)
            .await
            .unwrap();
    assert!(row.is_none());
}

#[tokio::test]
async fn saving_an_endpoint_stores_its_model_only_from_its_own_field() {
    let db = database().await;
    let shown = add(
        &db,
        "Shown",
        "anthropic",
        "https://api.anthropic.com",
        "sk-ant-a",
    )
    .await;
    let picked = add(
        &db,
        "Picked",
        "openai",
        "https://api.example.com/v1",
        "sk-b",
    )
    .await;
    let state = state(db.clone());
    let form = |provider: &str, model: &str, model_for: &str| SettingsForm {
        model: String::new(),
        claude_model: String::new(),
        endpoint_model: model.into(),
        endpoint_model_for: model_for.into(),
        reasoning_effort: String::new(),
        provider: provider.into(),
    };
    // Without the page script, the field of the endpoint that was shown is
    // submitted while another endpoint is picked; it must not move across.
    assert!(
        save(&state, form(&picked, "claude-opus-5-5", &shown))
            .await
            .is_ok()
    );
    assert_eq!(
        find_endpoint(&db, "u1", &picked)
            .await
            .unwrap()
            .unwrap()
            .model,
        ""
    );
    assert_eq!(
        find_endpoint(&db, "u1", &shown)
            .await
            .unwrap()
            .unwrap()
            .model,
        ""
    );
    assert!(
        save(&state, form(&shown, " claude-opus-5-5 ", &shown))
            .await
            .is_ok()
    );
    assert_eq!(
        find_endpoint(&db, "u1", &shown)
            .await
            .unwrap()
            .unwrap()
            .model,
        "claude-opus-5-5"
    );
    // A blank field keeps the stored model.
    assert!(save(&state, form(&shown, "", &shown)).await.is_ok());
    assert_eq!(
        find_endpoint(&db, "u1", &shown)
            .await
            .unwrap()
            .unwrap()
            .model,
        "claude-opus-5-5"
    );
}

#[tokio::test]
async fn switching_back_to_codex_keeps_the_endpoint_row() {
    let db = database().await;
    let id = add(
        &db,
        "Claude",
        "anthropic",
        "https://api.anthropic.com",
        "sk-ant-test",
    )
    .await;
    set_provider(&db, &id).await;
    let state = state(db.clone());
    let result = save(&state, settings_form("pi")).await;
    assert!(result.is_ok());
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
    assert!(find_endpoint(&db, "u1", &id).await.unwrap().is_some());
}

#[tokio::test]
async fn unknown_or_foreign_provider_is_rejected() {
    let db = database().await;
    let foreign = add(&db, "Other", "openai", "https://api.example.com/v1", "sk-x").await;
    sqlx::query("UPDATE ai_endpoints SET user_id='u2' WHERE id=?")
        .bind(&foreign)
        .execute(&db)
        .await
        .unwrap();
    for provider in ["missing-id", foreign.as_str()] {
        let result = save(&state(db.clone()), settings_form(provider)).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))), "{provider}");
    }
}

#[tokio::test]
async fn add_endpoint_registers_the_combo_with_its_key() {
    let db = database().await;
    let state = state(db.clone());
    let result = add_endpoint(
        State(state),
        AuthUser { id: "u1".into() },
        Form(EndpointForm {
            name: "Local".into(),
            spec: "openai".into(),
            base_url: "http://127.0.0.1:8080/v1/".into(),
            api_key: "sk-local".into(),
            model: "gpt-5.5".into(),
        }),
    )
    .await;
    assert!(result.is_ok());
    let endpoint = list_endpoints(&db, "u1").await.unwrap().pop().unwrap();
    assert_eq!(endpoint.name, "Local");
    assert_eq!(endpoint.spec, "openai");
    assert_eq!(endpoint.base_url, "http://127.0.0.1:8080/v1");
    assert_eq!(endpoint.api_key, "sk-local");
    assert_eq!(endpoint.model, "gpt-5.5");
    // Registering a combo never activates it.
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
}

#[tokio::test]
async fn add_endpoint_rejects_blank_fields_bad_spec_and_bad_url() {
    let db = database().await;
    let state = state(db.clone());
    let cases = [
        EndpointForm {
            name: String::new(),
            spec: "openai".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
            model: String::new(),
        },
        EndpointForm {
            name: "X".into(),
            spec: "google".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
            model: String::new(),
        },
        EndpointForm {
            name: "X".into(),
            spec: "openai".into(),
            base_url: "api.example.com/v1".into(),
            api_key: "sk-test".into(),
            model: String::new(),
        },
        EndpointForm {
            name: "X".into(),
            spec: "openai".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: String::new(),
            model: String::new(),
        },
    ];
    for form in cases {
        let result = add_endpoint(
            State(state.clone()),
            AuthUser { id: "u1".into() },
            Form(form),
        )
        .await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }
    assert!(list_endpoints(&db, "u1").await.unwrap().is_empty());
}

#[tokio::test]
async fn deleting_the_active_endpoint_falls_back_to_codex() {
    let db = database().await;
    let id = add(
        &db,
        "Claude",
        "anthropic",
        "https://api.anthropic.com",
        "sk-ant-test",
    )
    .await;
    set_provider(&db, &id).await;
    let state = state(db.clone());
    let result =
        delete_endpoint(State(state), AuthUser { id: "u1".into() }, Path(id.clone())).await;
    assert!(result.is_ok());
    assert!(find_endpoint(&db, "u1", &id).await.unwrap().is_none());
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
}

#[tokio::test]
async fn deleting_an_inactive_endpoint_keeps_the_provider() {
    let db = database().await;
    let active = add(
        &db,
        "Claude",
        "anthropic",
        "https://api.anthropic.com",
        "sk-ant-test",
    )
    .await;
    let inactive = add(&db, "Other", "openai", "https://api.example.com/v1", "sk-x").await;
    set_provider(&db, &active).await;
    let state = state(db.clone());
    let _ = delete_endpoint(State(state), AuthUser { id: "u1".into() }, Path(inactive))
        .await
        .unwrap();
    assert_eq!(ai_provider(&db).await.unwrap(), active);
}

#[tokio::test]
async fn deleting_another_users_endpoint_does_nothing() {
    let db = database().await;
    let id = add(
        &db,
        "Claude",
        "anthropic",
        "https://api.anthropic.com",
        "sk-ant-test",
    )
    .await;
    sqlx::query("UPDATE ai_endpoints SET user_id='u2' WHERE id=?")
        .bind(&id)
        .execute(&db)
        .await
        .unwrap();
    let _ = delete_endpoint(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(id.clone()),
    )
    .await
    .unwrap();
    assert!(find_endpoint(&db, "u2", &id).await.unwrap().is_some());
}

#[tokio::test]
async fn reconcile_resolves_a_dangling_endpoint_to_codex() {
    let db = database().await;
    set_provider(&db, "deleted-endpoint-id").await;
    reconcile_ai_provider(&db).await.unwrap();
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
}

#[tokio::test]
async fn reconcile_resolves_a_dangling_endpoint_to_the_first_registered() {
    let db = database().await;
    let first = add(&db, "A", "openai", "https://api.example.com/v1", "sk-1").await;
    add(&db, "B", "anthropic", "https://api.anthropic.com", "sk-2").await;
    set_provider(&db, "openai").await;
    reconcile_ai_provider(&db).await.unwrap();
    assert_eq!(ai_provider(&db).await.unwrap(), first);
}

#[tokio::test]
async fn reconcile_leaves_pi_and_live_endpoints_alone() {
    let db = database().await;
    let id = add(&db, "A", "openai", "https://api.example.com/v1", "sk-1").await;
    set_provider(&db, "pi").await;
    reconcile_ai_provider(&db).await.unwrap();
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
    set_provider(&db, &id).await;
    reconcile_ai_provider(&db).await.unwrap();
    assert_eq!(ai_provider(&db).await.unwrap(), id);
}

#[test]
fn mask_key_never_exposes_the_full_key() {
    assert_eq!(mask_key("sk-ant-test-abcdefgh"), "••••efgh");
    assert_eq!(mask_key("abc"), "••••");
    assert_eq!(mask_key(""), "••••");
    assert!(!mask_key("sk-ant-test-abcdefgh").contains("sk-ant"));
}

#[tokio::test]
async fn reasoning_effort_persists_in_codex_mode() {
    let db = database().await;
    let state = state(db.clone());
    for effort in ["high", "max"] {
        let result = save(
            &state,
            SettingsForm {
                model: "gpt-5.5".into(),
                claude_model: String::new(),
                endpoint_model: String::new(),
                endpoint_model_for: String::new(),
                reasoning_effort: effort.into(),
                provider: "pi".into(),
            },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(selected_effort(&db, "low").await.unwrap(), effort);
    }
}

#[tokio::test]
async fn reasoning_effort_persists_in_endpoint_mode() {
    let db = database().await;
    let id = add(
        &db,
        "Claude",
        "anthropic",
        "https://api.anthropic.com",
        "sk-ant-test",
    )
    .await;
    let result = save(
        &state(db.clone()),
        SettingsForm {
            model: String::new(),
            claude_model: String::new(),
            endpoint_model: String::new(),
            endpoint_model_for: String::new(),
            reasoning_effort: "medium".into(),
            provider: id,
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(selected_effort(&db, "low").await.unwrap(), "medium");
}

#[tokio::test]
async fn blank_effort_defaults_to_low() {
    let db = database().await;
    let result = save(&state(db.clone()), settings_form("pi")).await;
    assert!(result.is_ok());
    assert_eq!(selected_effort(&db, "low").await.unwrap(), "low");
}

#[tokio::test]
async fn unsupported_effort_is_rejected() {
    let db = database().await;
    let state = state(db.clone());
    let result = save(
        &state,
        SettingsForm {
            model: "gpt-5.5".into(),
            claude_model: String::new(),
            endpoint_model: String::new(),
            endpoint_model_for: String::new(),
            reasoning_effort: "extreme".into(),
            provider: "pi".into(),
        },
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
    // The rejected value is not stored.
    assert_eq!(selected_effort(&db, "low").await.unwrap(), "low");
}

#[tokio::test]
async fn pi_mode_rejects_unsupported_model_and_saves_nothing() {
    let db = database().await;
    let state = state(db.clone());
    let result = save(
        &state,
        SettingsForm {
            model: "not-a-real-model".into(),
            claude_model: String::new(),
            endpoint_model: String::new(),
            endpoint_model_for: String::new(),
            reasoning_effort: "max".into(),
            provider: "pi".into(),
        },
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
    assert_eq!(selected_effort(&db, "low").await.unwrap(), "low");
    let control = save(&state, settings_form("pi")).await;
    assert!(control.is_ok());
    assert_eq!(selected_model(&db, "gpt-5.5").await.unwrap(), "gpt-5.5");
}

#[tokio::test]
async fn claude_provider_requires_a_claude_credential() {
    let db = database().await;
    let state = state(db.clone());
    let result = save(&state, claude_form("claude-sonnet-5-5")).await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");

    // A Codex credential does not authorise Claude.
    let credential = serde_json::json!({"type":"oauth","access":"a","refresh":"r","expires":1});
    store_pi_credential(&db, "u1", CODEX_PI_PROVIDER, &credential)
        .await
        .unwrap();
    let result = save(&state, claude_form("claude-sonnet-5-5")).await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
}

#[tokio::test]
async fn credentials_are_stored_per_provider() {
    let db = database().await;
    let codex = serde_json::json!({"type":"oauth","access":"codex","refresh":"r","expires":1});
    let claude =
        serde_json::json!({"type":"oauth","access":"sk-ant-oat-x","refresh":"r","expires":1});
    store_pi_credential(&db, "u1", CODEX_PI_PROVIDER, &codex)
        .await
        .unwrap();
    store_pi_credential(&db, "u1", CLAUDE_PI_PROVIDER, &claude)
        .await
        .unwrap();
    assert_eq!(
        pi_credential(&db, "u1", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .unwrap()["access"],
        "codex"
    );
    assert_eq!(
        pi_credential(&db, "u1", CLAUDE_PI_PROVIDER)
            .await
            .unwrap()
            .unwrap()["access"],
        "sk-ant-oat-x"
    );
}

#[tokio::test]
async fn claude_model_setting_is_independent_of_the_codex_model() {
    let db = database().await;
    let state = state(db.clone());
    let credential = serde_json::json!({"type":"oauth","access":"a","refresh":"r","expires":1});
    store_pi_credential(&db, "u1", CLAUDE_PI_PROVIDER, &credential)
        .await
        .unwrap();
    assert_eq!(
        selected_claude_model(&db).await.unwrap(),
        "claude-sonnet-5-5"
    );
    assert!(save(&state, settings_form("pi")).await.is_ok());
    assert!(save(&state, claude_form("claude-opus-5-5")).await.is_ok());
    assert_eq!(ai_provider(&db).await.unwrap(), "claude");
    assert_eq!(selected_claude_model(&db).await.unwrap(), "claude-opus-5-5");
    assert_eq!(selected_model(&db, "x").await.unwrap(), "gpt-5.5");
    // A Codex model is not a valid Claude model.
    let result = save(&state, claude_form("gpt-5.5")).await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
}

#[tokio::test]
async fn failed_claude_exchange_keeps_the_flow_for_another_paste() {
    let db = database().await;
    let state = state(db);
    state.claude_flows.lock().insert(
        "flow".into(),
        ClaudeFlow {
            user_id: "u1".into(),
            url: "https://claude.example/authorize".into(),
            verifier: "verifier".into(),
            created: Instant::now(),
        },
    );
    // The test state has no helper script, so the exchange fails.
    let response = authorise_claude_exchange(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Form(ClaudeExchangeForm {
            flow_id: "flow".into(),
            code: "not-a-real-code".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("name=\"flow_id\" value=\"flow\""), "{body}");
    assert!(body.contains("class=\"error\""), "{body}");
    assert!(body.contains("https://claude.example/authorize"), "{body}");
    assert!(state.claude_flows.lock().contains_key("flow"));
}

#[tokio::test]
async fn disconnecting_claude_keeps_codex_and_falls_back_to_it() {
    let db = database().await;
    let state = state(db.clone());
    let codex = serde_json::json!({"type":"oauth","access":"codex","refresh":"r","expires":1});
    let claude = serde_json::json!({"type":"oauth","access":"claude","refresh":"r","expires":1});
    store_pi_credential(&db, "u1", CODEX_PI_PROVIDER, &codex)
        .await
        .unwrap();
    store_pi_credential(&db, "u1", CLAUDE_PI_PROVIDER, &claude)
        .await
        .unwrap();
    set_provider(&db, "claude").await;

    let redirect = disconnect_claude(State(state.clone()), AuthUser { id: "u1".into() })
        .await
        .unwrap();
    assert_eq!(
        redirect.into_response().headers()[header::LOCATION],
        "/settings?notice=disconnected"
    );
    assert!(
        pi_credential(&db, "u1", CLAUDE_PI_PROVIDER)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        pi_credential(&db, "u1", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .unwrap()["access"],
        "codex"
    );
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
}

#[tokio::test]
async fn disconnecting_active_codex_without_a_fallback_leaves_the_setting() {
    let db = database().await;
    let state = state(db.clone());
    let codex = serde_json::json!({"type":"oauth","access":"codex","refresh":"r","expires":1});
    let claude = serde_json::json!({"type":"oauth","access":"claude","refresh":"r","expires":1});
    store_pi_credential(&db, "u1", CODEX_PI_PROVIDER, &codex)
        .await
        .unwrap();
    store_pi_credential(&db, "u1", CLAUDE_PI_PROVIDER, &claude)
        .await
        .unwrap();
    set_provider(&db, "pi").await;

    let redirect = disconnect_codex(State(state.clone()), AuthUser { id: "u1".into() })
        .await
        .unwrap();
    assert_eq!(
        redirect.into_response().headers()[header::LOCATION],
        "/settings?notice=disconnected"
    );
    assert!(
        pi_credential(&db, "u1", CODEX_PI_PROVIDER)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        pi_credential(&db, "u1", CLAUDE_PI_PROVIDER)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
}

#[tokio::test]
async fn empty_claude_code_keeps_the_flow_for_a_retry() {
    let db = database().await;
    let state = state(db);
    state.claude_flows.lock().insert(
        "flow".into(),
        ClaudeFlow {
            user_id: "u1".into(),
            url: String::new(),
            verifier: "verifier".into(),
            created: Instant::now(),
        },
    );
    for code in ["", "   "] {
        let response = authorise_claude_exchange(
            State(state.clone()),
            AuthUser { id: "u1".into() },
            Form(ClaudeExchangeForm {
                flow_id: "flow".into(),
                code: code.into(),
            }),
        )
        .await
        .unwrap()
        .into_response();
        let location = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.contains("claude_error=Paste"), "{location}");
    }
    assert!(state.claude_flows.lock().contains_key("flow"));
}
