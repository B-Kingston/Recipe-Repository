use crate::auth::AuthUser;
use crate::{
    AppError, AppState, CLAUDE_PI_PROVIDER, CODEX_PI_PROVIDER, ClaudeExchangeForm, ClaudeFlow,
    EndpointForm, MediaCleanerForm, SettingsForm, add_endpoint, ai_provider,
    authorise_claude_exchange, delete_endpoint, disconnect_claude, disconnect_codex, find_endpoint,
    insert_endpoint, list_endpoints, mask_key, media_cleaner_model, media_cleaner_setting,
    pi_credential, reconcile_ai_provider, selected_claude_model, selected_effort, selected_model,
    stamp, store_pi_credential, update_media_cleaner, update_settings,
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
) -> Result<axum::response::Response, AppError> {
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
        "INSERT INTO app_settings (user_id, key, value, updated_at) VALUES ('u1', 'ai_provider', ?, ?)
         ON CONFLICT(user_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(value)
    .bind(stamp())
    .execute(db)
    .await
    .unwrap();
}

fn cleaner_form(provider: &str, model: &str) -> MediaCleanerForm {
    MediaCleanerForm {
        provider: provider.into(),
        model: model.into(),
    }
}

#[tokio::test]
async fn the_media_cleaner_follows_the_ai_provider_until_one_is_chosen() {
    let db = database().await;
    let state = state(db.clone());
    assert_eq!(media_cleaner_setting(&db, "u1").await.unwrap(), None);
    let groq = add(&db, "Groq", "groq", "", "gsk-key").await;
    let _ = update_media_cleaner(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Form(cleaner_form(&groq, " llama-3.1-8b-instant ")),
    )
    .await
    .unwrap();
    assert_eq!(
        media_cleaner_setting(&db, "u1").await.unwrap(),
        Some(groq.clone())
    );
    assert_eq!(
        media_cleaner_model(&db, "u1").await.unwrap().as_deref(),
        Some("llama-3.1-8b-instant")
    );
    // Back to following the AI provider, with the provider's own model.
    let _ = update_media_cleaner(
        State(state),
        AuthUser { id: "u1".into() },
        Form(cleaner_form("", "")),
    )
    .await
    .unwrap();
    assert_eq!(media_cleaner_setting(&db, "u1").await.unwrap(), None);
    assert_eq!(media_cleaner_model(&db, "u1").await.unwrap(), None);
}

#[tokio::test]
async fn the_media_cleaner_rejects_unknown_and_foreign_providers() {
    let db = database().await;
    let state = state(db.clone());
    let foreign = add(&db, "Groq", "groq", "", "gsk-key").await;
    sqlx::query("UPDATE ai_endpoints SET user_id='u2' WHERE id=?")
        .bind(&foreign)
        .execute(&db)
        .await
        .unwrap();
    for provider in ["missing-id", foreign.as_str()] {
        let result = update_media_cleaner(
            State(state.clone()),
            AuthUser { id: "u1".into() },
            Form(cleaner_form(provider, "")),
        )
        .await;
        assert!(matches!(result, Err(AppError::BadRequest(_))), "{provider}");
    }
    assert_eq!(media_cleaner_setting(&db, "u1").await.unwrap(), None);
}

#[tokio::test]
async fn deleting_the_cleaners_endpoint_returns_it_to_the_ai_provider() {
    let db = database().await;
    let state = state(db.clone());
    let id = add(&db, "Groq", "groq", "", "gsk-key").await;
    let _ = update_media_cleaner(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Form(cleaner_form(&id, "llama-3.1-8b-instant")),
    )
    .await
    .unwrap();
    let _ = delete_endpoint(State(state), AuthUser { id: "u1".into() }, Path(id)).await;
    assert_eq!(media_cleaner_setting(&db, "u1").await.unwrap(), None);
    assert_eq!(media_cleaner_model(&db, "u1").await.unwrap(), None);
}

#[tokio::test]
async fn a_saved_gateway_key_migrates_to_the_cleaners_own_endpoint() {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().in_memory(true))
        .await
        .unwrap();
    let mut before = sqlx::migrate!();
    before.migrations = before
        .migrations
        .iter()
        .filter(|migration| migration.version < 16)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    before.run(&db).await.unwrap();
    // The 0016 cleaner setting was app-wide; 0017 hands it to the account.
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at) VALUES ('u1', 'alice', '', ?)",
    )
    .bind(stamp())
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO pi_credentials (user_id, provider, credential_json, updated_at) VALUES ('u1', 'vercel-ai-gateway', ?, ?)")
        .bind(r#"{"apiKey":"vck-secret","baseUrl":"https://ai-gateway.vercel.sh/v1"}"#)
        .bind(stamp())
        .execute(&db)
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    let endpoint = list_endpoints(&db, "u1").await.unwrap().pop().unwrap();
    assert_eq!(endpoint.name, "Vercel AI Gateway");
    assert_eq!(endpoint.spec, "openai-compatible");
    assert_eq!(endpoint.base_url, "https://ai-gateway.vercel.sh/v1");
    assert_eq!(endpoint.api_key, "vck-secret");
    assert_eq!(endpoint.model, "poolside/laguna-s-2.1-free");
    assert_eq!(
        media_cleaner_setting(&db, "u1").await.unwrap(),
        Some(endpoint.id)
    );
    assert!(
        pi_credential(&db, "u1", "vercel-ai-gateway")
            .await
            .unwrap()
            .is_none()
    );
    // The AI provider is untouched.
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
}

#[tokio::test]
async fn ai_provider_defaults_to_codex() {
    let db = database().await;
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
}

#[tokio::test]
async fn saving_codex_mode_stores_model_and_provider() {
    let db = database().await;
    let state = state(db.clone());
    let result = save(&state, settings_form("pi")).await;
    assert!(result.is_ok());
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
    assert_eq!(
        selected_model(&db, "u1", "gpt-5.5").await.unwrap(),
        "gpt-5.5"
    );
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
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), id);
    // Endpoint mode never writes or requires a Codex model.
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM app_settings WHERE user_id = 'u1' AND key = 'model'")
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
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
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

fn endpoint_form(provider: &str, base_url: &str, api_key: &str, model: &str) -> EndpointForm {
    EndpointForm {
        provider: provider.into(),
        base_url: base_url.into(),
        api_key: api_key.into(),
        model: model.into(),
    }
}

async fn add_from_form(state: &Arc<AppState>, form: EndpointForm) -> Result<(), AppError> {
    add_endpoint(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Form(form),
    )
    .await
    .map(|_| ())
}

#[tokio::test]
async fn adding_a_named_provider_needs_only_its_key_and_makes_it_active() {
    let db = database().await;
    let state = state(db.clone());
    // A stray URL or model from the hidden custom fields is ignored.
    add_from_form(
        &state,
        endpoint_form("google", "https://evil.example", "AIza-key", "x"),
    )
    .await
    .unwrap();
    let endpoint = list_endpoints(&db, "u1").await.unwrap().pop().unwrap();
    assert_eq!(endpoint.name, "Google Gemini");
    assert_eq!(endpoint.spec, "google");
    // pi-ai knows the provider's own base URL.
    assert_eq!(endpoint.base_url, "");
    assert_eq!(endpoint.api_key, "AIza-key");
    assert_eq!(endpoint.model, "");
    assert_eq!(crate::endpoint_model(&endpoint), "gemini-3.8-flash");
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), endpoint.id);
}

#[tokio::test]
async fn adding_native_providers_stores_their_base_url_and_numbers_repeats() {
    let db = database().await;
    let state = state(db.clone());
    add_from_form(&state, endpoint_form("anthropic", "", "sk-ant-1", ""))
        .await
        .unwrap();
    add_from_form(&state, endpoint_form("anthropic", "", "sk-ant-2", ""))
        .await
        .unwrap();
    let endpoints = list_endpoints(&db, "u1").await.unwrap();
    let names: Vec<_> = endpoints
        .iter()
        .map(|endpoint| endpoint.name.as_str())
        .collect();
    assert_eq!(names, ["Anthropic", "Anthropic 2"]);
    assert!(
        endpoints
            .iter()
            .all(|endpoint| endpoint.base_url == "https://api.anthropic.com")
    );
}

#[tokio::test]
async fn adding_a_custom_server_takes_its_url_and_model_and_allows_no_key() {
    let db = database().await;
    let state = state(db.clone());
    add_from_form(
        &state,
        endpoint_form("custom-openai", "http://ollama:11434/v1/", "", "llama3.3"),
    )
    .await
    .unwrap();
    let endpoint = list_endpoints(&db, "u1").await.unwrap().pop().unwrap();
    assert_eq!(endpoint.name, "ollama:11434");
    assert_eq!(endpoint.spec, "openai-compatible");
    assert_eq!(endpoint.base_url, "http://ollama:11434/v1");
    assert_eq!(endpoint.api_key, "");
    assert_eq!(endpoint.model, "llama3.3");
}

#[tokio::test]
async fn adding_a_provider_rejects_unknown_choices_missing_keys_and_bad_urls() {
    let db = database().await;
    let state = state(db.clone());
    let cases = [
        endpoint_form("", "", "sk-test", ""),
        endpoint_form("not-a-provider", "", "sk-test", ""),
        endpoint_form("openai", "", "", ""),
        endpoint_form("custom-anthropic", "", "sk-test", ""),
        endpoint_form("custom-anthropic", "api.example.com/v1", "sk-test", ""),
        endpoint_form("custom-anthropic", "https://api.example.com", "", ""),
    ];
    for form in cases {
        let result = add_from_form(&state, form).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }
    assert!(list_endpoints(&db, "u1").await.unwrap().is_empty());
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
}

#[test]
fn only_native_specs_keep_grounded_search() {
    use crate::ai::{SearchMode, endpoint_search_mode};
    assert!(endpoint_search_mode("openai", SearchMode::Grounded) == SearchMode::Grounded);
    assert!(endpoint_search_mode("anthropic", SearchMode::Grounded) == SearchMode::Grounded);
    assert!(endpoint_search_mode("google", SearchMode::Grounded) == SearchMode::Off);
    assert!(endpoint_search_mode("openai-compatible", SearchMode::GapFill) == SearchMode::GapFill);
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
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
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
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), active);
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
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
}

#[tokio::test]
async fn reconcile_resolves_a_dangling_endpoint_to_the_first_registered() {
    let db = database().await;
    let first = add(&db, "A", "openai", "https://api.example.com/v1", "sk-1").await;
    add(&db, "B", "anthropic", "https://api.anthropic.com", "sk-2").await;
    set_provider(&db, "openai").await;
    reconcile_ai_provider(&db).await.unwrap();
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), first);
}

#[tokio::test]
async fn reconcile_leaves_pi_and_live_endpoints_alone() {
    let db = database().await;
    let id = add(&db, "A", "openai", "https://api.example.com/v1", "sk-1").await;
    set_provider(&db, "pi").await;
    reconcile_ai_provider(&db).await.unwrap();
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
    set_provider(&db, &id).await;
    reconcile_ai_provider(&db).await.unwrap();
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), id);
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
        assert_eq!(selected_effort(&db, "u1", "low").await.unwrap(), effort);
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
    assert_eq!(selected_effort(&db, "u1", "low").await.unwrap(), "medium");
}

#[tokio::test]
async fn blank_effort_defaults_to_low() {
    let db = database().await;
    let result = save(&state(db.clone()), settings_form("pi")).await;
    assert!(result.is_ok());
    assert_eq!(selected_effort(&db, "u1", "low").await.unwrap(), "low");
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
    assert_eq!(selected_effort(&db, "u1", "low").await.unwrap(), "low");
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
    assert_eq!(selected_effort(&db, "u1", "low").await.unwrap(), "low");
    let control = save(&state, settings_form("pi")).await;
    assert!(control.is_ok());
    assert_eq!(
        selected_model(&db, "u1", "gpt-5.5").await.unwrap(),
        "gpt-5.5"
    );
}

#[tokio::test]
async fn claude_provider_requires_a_claude_credential() {
    let db = database().await;
    let state = state(db.clone());
    let result = save(&state, claude_form("claude-sonnet-5-5")).await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");

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
        selected_claude_model(&db, "u1").await.unwrap(),
        "claude-sonnet-5-5"
    );
    assert!(save(&state, settings_form("pi")).await.is_ok());
    assert!(save(&state, claude_form("claude-opus-5-5")).await.is_ok());
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "claude");
    assert_eq!(
        selected_claude_model(&db, "u1").await.unwrap(),
        "claude-opus-5-5"
    );
    assert_eq!(selected_model(&db, "u1", "x").await.unwrap(), "gpt-5.5");
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
        axum::http::HeaderMap::new(),
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
    assert_flash(&redirect, "/settings#settings-ai", "disconnected");
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
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
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
    assert_flash(&redirect, "/settings#settings-ai", "disconnected");
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
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "pi");
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
            axum::http::HeaderMap::new(),
            Form(ClaudeExchangeForm {
                flow_id: "flow".into(),
                code: code.into(),
            }),
        )
        .await
        .unwrap()
        .into_response();
        assert_flash(&response, "/settings#settings-ai", "claude_code_missing");
    }
    assert!(state.claude_flows.lock().contains_key("flow"));
}

#[tokio::test]
async fn settings_belong_to_each_user() {
    let db = database().await;
    let state = state(db.clone());
    let id = add(&db, "A", "openai", "https://api.example.com/v1", "sk-1").await;
    assert!(save(&state, settings_form(&id)).await.is_ok());
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), id);
    assert_eq!(ai_provider(&db, "u2").await.unwrap(), "pi");
    assert_eq!(
        selected_effort(&db, "u2", "low").await.unwrap(),
        "low",
        "another user's saved effort does not leak"
    );

    // A provider value naming another user's endpoint is stale for this user.
    sqlx::query(
        "INSERT INTO app_settings (user_id, key, value, updated_at) VALUES ('u2', 'ai_provider', ?, ?)",
    )
    .bind(&id)
    .bind(stamp())
    .execute(&db)
    .await
    .unwrap();
    reconcile_ai_provider(&db).await.unwrap();
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), id);
    assert_eq!(ai_provider(&db, "u2").await.unwrap(), "pi");
}

async fn add_user(db: &SqlitePool, id: &str, username: &str, created_at: &str) {
    sqlx::query("INSERT INTO users (id, username, password_hash, created_at) VALUES (?, ?, '', ?)")
        .bind(id)
        .bind(username)
        .bind(created_at)
        .execute(db)
        .await
        .unwrap();
}

async fn settings_body(state: &Arc<AppState>, user_id: &str) -> String {
    settings_body_with(state, user_id, "").await
}

async fn settings_body_with(state: &Arc<AppState>, user_id: &str, flash: &str) -> String {
    let html = crate::settings_page(
        State(state.clone()),
        AuthUser { id: user_id.into() },
        crate::htmx::Flash(flash.into()),
    )
    .await
    .unwrap();
    html.0
}

/// A 303 to `location` that sets the one-time `flash` notice.
fn assert_flash(response: &axum::response::Response, location: &str, flash: &str) {
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], location);
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cookie.starts_with(&format!("recipe_flash={flash};")),
        "{cookie}"
    );
}

async fn choose_ai_logins(state: &Arc<AppState>, user_id: &str, mode: &str) {
    let result = crate::update_ai_logins(
        State(state.clone()),
        AuthUser { id: user_id.into() },
        Form(crate::AiLoginsForm { mode: mode.into() }),
    )
    .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn other_accounts_share_the_first_accounts_ai_logins_until_they_override() {
    let db = database().await;
    let state = state(db.clone());
    // Without accounts every user is their own AI account.
    assert_eq!(crate::ai_account(&db, "u1").await.unwrap(), "u1");
    add_user(&db, "u1", "alice", "2026-01-01T00:00:00+00:00").await;
    add_user(&db, "u2", "bob", "2026-02-01T00:00:00+00:00").await;
    assert_eq!(crate::ai_account(&db, "u1").await.unwrap(), "u1");
    assert_eq!(crate::ai_account(&db, "u2").await.unwrap(), "u1");

    // Alice connects Claude: bob can generate with it and sees what it is.
    let bob = AuthUser { id: "u2".into() };
    assert!(!crate::ai::ai_configured(&state, &bob).await);
    let claude =
        serde_json::json!({"type":"oauth","access":"sk-ant-oat-x","refresh":"r","expires":1});
    store_pi_credential(&db, "u1", CLAUDE_PI_PROVIDER, &claude)
        .await
        .unwrap();
    assert!(save(&state, claude_form("claude-opus-5-5")).await.is_ok());
    assert!(crate::ai::ai_configured(&state, &bob).await);
    let body = settings_body(&state, "u2").await;
    assert!(body.contains("alice's AI logins"), "{body}");
    assert!(body.contains("Claude (Pro/Max) · claude-opus-5-5"));
    assert!(body.contains("Use my own AI logins"));
    assert!(!body.contains("Add a provider with an API key"));
    assert!(
        !body.contains("sk-ant"),
        "the shared credential never reaches the page"
    );
    let body = settings_body(&state, "u1").await;
    assert!(body.contains("other accounts generate recipes with these logins"));
    assert!(body.contains("Add a provider with an API key"));

    // Bob overrides: his own (empty) setup applies and alice's stays put.
    choose_ai_logins(&state, "u2", "own").await;
    assert_eq!(crate::ai_account(&db, "u2").await.unwrap(), "u2");
    assert!(!crate::ai::ai_configured(&state, &bob).await);
    let body = settings_body(&state, "u2").await;
    assert!(body.contains("Use alice's shared logins"));
    assert!(body.contains("Add a provider with an API key"));
    assert_eq!(ai_provider(&db, "u1").await.unwrap(), "claude");

    // Going back to the shared logins keeps bob's own settings for later.
    let id = insert_endpoint(
        &db,
        "u2",
        "OpenAI",
        "openai",
        "https://api.openai.com/v1",
        "sk-b",
        "",
    )
    .await
    .unwrap();
    crate::set_app_setting(&db, "u2", "ai_provider", &id)
        .await
        .unwrap();
    assert!(crate::ai::ai_configured(&state, &bob).await);
    choose_ai_logins(&state, "u2", "shared").await;
    assert_eq!(crate::ai_account(&db, "u2").await.unwrap(), "u1");
    assert_eq!(ai_provider(&db, "u2").await.unwrap(), id);

    let result = crate::update_ai_logins(
        State(state.clone()),
        bob,
        Form(crate::AiLoginsForm {
            mode: "theirs".into(),
        }),
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
}

async fn choose_search(state: &Arc<AppState>, user_id: &str, engine: &str) -> Result<(), AppError> {
    crate::update_search_engine(
        State(state.clone()),
        AuthUser { id: user_id.into() },
        Form(crate::SearchEngineForm {
            engine: engine.into(),
        }),
    )
    .await
    .map(|_| ())
}

#[tokio::test]
async fn other_accounts_share_the_first_accounts_search_engine_until_they_override() {
    let db = database().await;
    let state = state(db.clone());
    add_user(&db, "u1", "alice", "2026-01-01T00:00:00+00:00").await;
    add_user(&db, "u2", "bob", "2026-02-01T00:00:00+00:00").await;
    // Search by meaning by default, for everyone.
    assert!(!crate::keyword_search_selected(&db, "u1").await.unwrap());
    assert!(!crate::keyword_search_selected(&db, "u2").await.unwrap());

    // The shared account's choice carries over to accounts left on "shared".
    choose_search(&state, "u1", "bm25").await.unwrap();
    assert!(crate::keyword_search_selected(&db, "u1").await.unwrap());
    assert!(crate::keyword_search_selected(&db, "u2").await.unwrap());
    assert!(settings_body(&state, "u2").await.contains(
        r#"value="shared" checked><span>Same as alice</span><span class="muted">Word search</span>"#
    ));

    // An account's own choice wins until it returns to the shared one.
    choose_search(&state, "u2", "embeddings").await.unwrap();
    assert!(!crate::keyword_search_selected(&db, "u2").await.unwrap());
    choose_search(&state, "u2", "shared").await.unwrap();
    assert!(crate::keyword_search_selected(&db, "u2").await.unwrap());

    // The shared account has no "shared" option of its own.
    let page = settings_body(&state, "u1").await;
    assert!(!page.contains(r#"value="shared""#));
    assert!(page.contains(r#"value="bm25" checked"#));

    assert!(matches!(
        choose_search(&state, "u1", "vectors").await,
        Err(AppError::BadRequest(_))
    ));
}

fn worker_output(code: i32, stdout: &str) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt;
    std::process::Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

const REFUSED_LOGIN: &str = r#"{"error":"OAuth refresh failed for anthropic: invalid_grant","code":"auth","retryable":false}"#;

#[tokio::test]
async fn a_refused_subscription_login_is_flagged_until_it_works_again() {
    let db = database().await;
    let state = state(db.clone());
    add_user(&db, "u1", "alice", "2026-01-01T00:00:00+00:00").await;
    store_pi_credential(
        &db,
        "u1",
        CLAUDE_PI_PROVIDER,
        &serde_json::json!({"type": "oauth"}),
    )
    .await
    .unwrap();
    set_provider(&db, "claude").await;
    let login = crate::Login::Subscription {
        user_id: "u1",
        pi_provider: CLAUDE_PI_PROVIDER,
    };
    let refused = || crate::subscription_login_refused(&db, "u1", CLAUDE_PI_PROVIDER);

    // Any other failure passes through for the caller's own handling.
    let other = crate::ai::track_login(
        &state,
        login,
        "Claude",
        true,
        worker_output(1, r#"{"error":"HTTP 500","code":"worker"}"#),
    )
    .await;
    assert!(other.is_ok_and(|output| !output.status.success()));
    assert!(!refused().await.unwrap());

    match crate::ai::track_login(
        &state,
        login,
        "Claude",
        true,
        worker_output(1, REFUSED_LOGIN),
    )
    .await
    {
        Err(AppError::AiLoginRefused(message)) => {
            assert!(
                message.contains("Reconnect Claude in Settings"),
                "{message}"
            )
        }
        other => panic!("expected a refused login, got {:?}", other.map(|_| ())),
    }
    assert!(refused().await.unwrap());
    let body = settings_body(&state, "u1").await;
    assert!(body.contains("Sign-in expired"), "{body}");
    assert!(body.contains("refused its sign-in"), "{body}");
    assert!(body.contains("action=\"/settings/check/claude\""), "{body}");

    // A model listing does not sign in, so it leaves the flag alone.
    crate::ai::track_login(&state, login, "Claude", false, worker_output(0, "{}"))
        .await
        .unwrap();
    assert!(refused().await.unwrap());
    // A call that signed in clears it.
    crate::ai::track_login(&state, login, "Claude", true, worker_output(0, "{}"))
        .await
        .unwrap();
    assert!(!refused().await.unwrap());

    // Reconnecting (or a stored token refresh) clears it as well.
    crate::ai::track_login(
        &state,
        login,
        "Claude",
        true,
        worker_output(1, REFUSED_LOGIN),
    )
    .await
    .unwrap_err();
    store_pi_credential(
        &db,
        "u1",
        CLAUDE_PI_PROVIDER,
        &serde_json::json!({"type": "oauth", "access": "new"}),
    )
    .await
    .unwrap();
    assert!(!refused().await.unwrap());
    assert!(
        !settings_body(&state, "u1")
            .await
            .contains("Sign-in expired")
    );
}

#[tokio::test]
async fn a_rejected_endpoint_key_is_flagged_on_its_row_and_for_sharing_accounts() {
    let db = database().await;
    let state = state(db.clone());
    add_user(&db, "u1", "alice", "2026-01-01T00:00:00+00:00").await;
    add_user(&db, "u2", "bob", "2026-02-01T00:00:00+00:00").await;
    let id = add(&db, "Groq", "groq", "", "gsk-test-1234").await;
    set_provider(&db, &id).await;
    let login = crate::Login::Endpoint { id: &id };

    match crate::ai::track_login(&state, login, "Groq", true, worker_output(1, REFUSED_LOGIN)).await
    {
        Err(AppError::AiLoginRefused(message)) => {
            assert!(
                message.starts_with("Groq rejected its API key"),
                "{message}"
            )
        }
        other => panic!("expected a refused login, got {:?}", other.map(|_| ())),
    }
    assert!(
        find_endpoint(&db, "u1", &id)
            .await
            .unwrap()
            .unwrap()
            .auth_failed
    );
    let body = settings_body(&state, "u1").await;
    assert!(body.contains("Key rejected"), "{body}");
    assert!(
        body.contains(&format!("action=\"/settings/endpoints/{id}/check\"")),
        "{body}"
    );
    // An account sharing u1's logins is told who must reconnect.
    let shared = settings_body(&state, "u2").await;
    assert!(shared.contains("alice needs to reconnect it"), "{shared}");

    crate::ai::track_login(&state, login, "Groq", true, worker_output(0, "{}"))
        .await
        .unwrap();
    assert!(
        !find_endpoint(&db, "u1", &id)
            .await
            .unwrap()
            .unwrap()
            .auth_failed
    );
    assert!(
        !settings_body(&state, "u2")
            .await
            .contains("needs to reconnect")
    );
}

#[tokio::test]
async fn login_checks_flash_their_outcome_in_the_ai_block() {
    let db = database().await;
    let state = state(db.clone());
    add_user(&db, "u1", "alice", "2026-01-01T00:00:00+00:00").await;
    for (check, works, flash) in [
        (
            crate::ai::LoginCheck::Works,
            "claude_login_works",
            "claude_login_works",
        ),
        (
            crate::ai::LoginCheck::Refused,
            "claude_login_works",
            "login_refused",
        ),
        (
            crate::ai::LoginCheck::Failed,
            "endpoint_key_works",
            "login_check_failed",
        ),
    ] {
        assert_flash(
            &crate::login_check_redirect(check, works),
            "/settings#settings-ai",
            flash,
        );
    }
    for (flash, text) in [
        ("codex_login_works", "ChatGPT (Codex) sign-in works."),
        ("claude_login_works", "Claude sign-in works."),
        ("endpoint_key_works", "The API key works."),
        ("login_refused", "The provider refused that sign-in"),
        ("login_check_failed", "could not be checked right now"),
    ] {
        let body = settings_body_with(&state, "u1", flash).await;
        assert!(body.contains(text), "{flash}: {body}");
    }
}

#[tokio::test]
async fn checking_an_unknown_login_is_not_found() {
    let db = database().await;
    let state = state(db.clone());
    let unknown = crate::check_subscription(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Path("gemini".into()),
    )
    .await;
    assert!(matches!(unknown, Err(AppError::NotFound)));
    let foreign = crate::check_endpoint(
        State(state.clone()),
        AuthUser { id: "u2".into() },
        Path(add(&db, "Groq", "groq", "", "gsk-test-1234").await),
    )
    .await;
    assert!(matches!(foreign, Err(AppError::NotFound)));
    // Without a stored sign-in there is nothing to check.
    let missing = crate::check_subscription(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Path("claude".into()),
    )
    .await
    .unwrap();
    assert_eq!(missing.headers()[header::LOCATION], "/settings#settings-ai");
}

#[tokio::test]
async fn the_cleaner_model_is_a_select_that_keeps_the_saved_model() {
    let db = database().await;
    let state = state(db.clone());
    // The first account owns the AI setup, so its page shows the cleaner.
    add_user(&db, "u1", "alice", "2026-01-01T00:00:00+00:00").await;
    crate::set_app_setting(&db, "u1", "media_cleaner_model", "my-small-model")
        .await
        .unwrap();
    let html = settings_body(&state, "u1").await;
    assert!(
        html.contains(r#"<select id="cleaner_model" name="model" data-ai-provider="pi">"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<option value="">Provider's model</option>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<option value="my-small-model" selected>my-small-model</option>"#),
        "{html}"
    );
}

async fn save_search_key(
    state: &Arc<AppState>,
    user_id: &str,
    provider: &str,
    api_key: &str,
) -> Result<axum::response::Response, AppError> {
    crate::add_web_search_key(
        State(state.clone()),
        AuthUser { id: user_id.into() },
        Form(crate::WebSearchForm {
            provider: provider.into(),
            api_key: api_key.into(),
        }),
    )
    .await
}

#[tokio::test]
async fn a_search_key_is_saved_masked_replaced_and_removed() {
    let db = database().await;
    let state = state(db.clone());
    add_user(&db, "u1", "alice", "2026-01-01T00:00:00+00:00").await;
    for (provider, key) in [
        ("bing", "key-123456"),
        ("exa", "  "),
        ("", "key-123456"),
        ("exa", "two words"),
        ("exa", "key\u{7}bell"),
    ] {
        let result = save_search_key(&state, "u1", provider, key).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))), "{provider}");
    }
    assert!(crate::web_search_key(&db, "u1").await.unwrap().is_none());
    assert!(settings_body(&state, "u1").await.contains("Built-in only"));

    let response = save_search_key(&state, "u1", "exa", " exa-secret-key-9876 ")
        .await
        .unwrap();
    assert_flash(
        &response,
        "/settings#settings-web-search",
        "web_search_saved",
    );
    let html = settings_body_with(&state, "u1", "web_search_saved").await;
    assert!(html.contains("Search key saved."), "{html}");
    assert!(html.contains("&#10003; Exa"), "{html}");
    assert!(html.contains("key ••••9876"), "{html}");
    assert!(
        !html.contains("exa-secret-key"),
        "the key never reaches the page"
    );

    save_search_key(&state, "u1", "brave", "brave-key-1111")
        .await
        .unwrap();
    let key = crate::web_search_key(&db, "u1").await.unwrap().unwrap();
    assert_eq!(
        (key.provider.as_str(), key.api_key.as_str()),
        ("brave", "brave-key-1111")
    );

    let response = crate::delete_web_search_key(State(state.clone()), AuthUser { id: "u1".into() })
        .await
        .unwrap();
    assert_flash(
        &response,
        "/settings#settings-web-search",
        "web_search_removed",
    );
    assert!(crate::web_search_key(&db, "u1").await.unwrap().is_none());
}

#[tokio::test]
async fn an_account_sharing_ai_logins_sees_the_shared_search_key_but_not_its_form() {
    let db = database().await;
    let state = state(db.clone());
    add_user(&db, "u1", "alice", "2026-01-01T00:00:00+00:00").await;
    add_user(&db, "u2", "bob", "2026-02-01T00:00:00+00:00").await;
    let html = settings_body(&state, "u2").await;
    assert!(html.contains("alice has no search key"), "{html}");
    save_search_key(&state, "u1", "tavily", "tvly-shared-4321")
        .await
        .unwrap();
    let html = settings_body(&state, "u2").await;
    assert!(html.contains("with alice's Tavily key"), "{html}");
    assert!(!html.contains("settings-web-search"), "{html}");
    assert!(!html.contains("4321"), "{html}");
    assert!(
        settings_body(&state, "u1")
            .await
            .contains("Shared: accounts using these AI logins use this key too.")
    );
}

#[test]
fn the_chat_searches_only_when_enabled_and_sends_the_key_it_has() {
    use crate::ai::{SearchMode, chat_web_search};
    let key = crate::WebSearchKey {
        provider: "exa".into(),
        api_key: "exa-key".into(),
    };
    let (mode, sent) = chat_web_search(false, Some(&key));
    assert!(mode == SearchMode::Off);
    assert!(sent.is_null());
    let (mode, sent) = chat_web_search(true, None);
    assert!(mode == SearchMode::GapFill);
    assert!(sent.is_null());
    let (mode, sent) = chat_web_search(true, Some(&key));
    assert!(mode == SearchMode::GapFill);
    assert_eq!(
        sent,
        serde_json::json!({ "provider": "exa", "apiKey": "exa-key" })
    );
}
