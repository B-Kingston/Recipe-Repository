use crate::auth::AuthUser;
use crate::{
    AppError, AppState, SettingsForm, ai_provider, openai_api_config, selected_effort,
    selected_model, store_openai_api_config, update_settings,
};
use axum::extract::{Form, State};
use parking_lot::Mutex;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, sync::Arc};

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

#[tokio::test]
async fn ai_provider_defaults_to_pi() {
    let db = database().await;
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
}

#[tokio::test]
async fn saving_openai_config_switches_provider_and_stores_credential() {
    let db = database().await;
    // The openai mode does not post a model: it is pinned to gpt-5.6-luna.
    let result = save(
        &state(db.clone()),
        SettingsForm {
            model: String::new(),
            reasoning_effort: String::new(),
            use_openai_api: Some("on".into()),
            openai_base_url: "https://api.openai.com/v1/".into(),
            openai_api_key: "sk-test".into(),
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(crate::DEFAULT_OPENAI_MODEL, "gpt-5.6-luna");
    assert_eq!(ai_provider(&db).await.unwrap(), "openai");
    assert_eq!(
        openai_api_config(&db, "u1").await.unwrap(),
        Some((
            "https://api.openai.com/v1".to_string(),
            "sk-test".to_string()
        ))
    );
    // Neither the Codex model nor an openai model key is written.
    for key in ["model", "openai_model"] {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM app_settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&db)
            .await
            .unwrap();
        assert!(row.is_none(), "unexpected app_settings row {key:?}");
    }
}

#[tokio::test]
async fn blank_base_url_falls_back_to_official_default() {
    let db = database().await;
    let result = save(
        &state(db.clone()),
        SettingsForm {
            model: String::new(),
            reasoning_effort: String::new(),
            use_openai_api: Some("on".into()),
            openai_base_url: String::new(),
            openai_api_key: "sk-test".into(),
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(
        openai_api_config(&db, "u1").await.unwrap(),
        Some((
            "https://api.openai.com/v1".to_string(),
            "sk-test".to_string()
        ))
    );
}

#[tokio::test]
async fn unchecking_keeps_credential_and_switches_back() {
    let db = database().await;
    let state = state(db.clone());
    let _ = save(
        &state,
        SettingsForm {
            model: String::new(),
            reasoning_effort: String::new(),
            use_openai_api: Some("on".into()),
            openai_base_url: "https://api.openai.com/v1/".into(),
            openai_api_key: "sk-test".into(),
        },
    )
    .await
    .unwrap();
    let result = save(
        &state,
        SettingsForm {
            model: "gpt-5.4-mini".into(),
            reasoning_effort: String::new(),
            use_openai_api: None,
            openai_base_url: String::new(),
            openai_api_key: String::new(),
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(ai_provider(&db).await.unwrap(), "pi");
    assert_eq!(
        selected_model(&db, "gpt-5.4-mini").await.unwrap(),
        "gpt-5.4-mini"
    );
    assert!(openai_api_config(&db, "u1").await.unwrap().is_some());
}

#[tokio::test]
async fn blank_key_without_stored_credential_is_rejected() {
    let db = database().await;
    let result = save(
        &state(db),
        SettingsForm {
            model: String::new(),
            reasoning_effort: String::new(),
            use_openai_api: Some("on".into()),
            openai_base_url: "https://api.openai.com/v1/".into(),
            openai_api_key: String::new(),
        },
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
}

#[tokio::test]
async fn blank_fields_keep_stored_credential() {
    let db = database().await;
    store_openai_api_config(&db, "u1", "https://api.example.com/v1", "sk-old")
        .await
        .unwrap();
    let result = save(
        &state(db.clone()),
        SettingsForm {
            model: String::new(),
            reasoning_effort: String::new(),
            use_openai_api: Some("on".into()),
            openai_base_url: String::new(),
            openai_api_key: String::new(),
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(
        openai_api_config(&db, "u1").await.unwrap(),
        Some((
            "https://api.example.com/v1".to_string(),
            "sk-old".to_string()
        ))
    );
}

#[tokio::test]
async fn pi_mode_rejects_unsupported_model() {
    let db = database().await;
    let state = state(db.clone());
    let result = save(
        &state,
        SettingsForm {
            model: "not-a-real-model".into(),
            reasoning_effort: String::new(),
            use_openai_api: None,
            openai_base_url: String::new(),
            openai_api_key: String::new(),
        },
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
    let control = save(
        &state,
        SettingsForm {
            model: "gpt-5.4-mini".into(),
            reasoning_effort: String::new(),
            use_openai_api: None,
            openai_base_url: String::new(),
            openai_api_key: String::new(),
        },
    )
    .await;
    assert!(control.is_ok());
    assert_eq!(
        selected_model(&db, "gpt-5.4-mini").await.unwrap(),
        "gpt-5.4-mini"
    );
}

#[tokio::test]
async fn invalid_base_url_is_rejected() {
    let db = database().await;
    let result = save(
        &state(db),
        SettingsForm {
            model: String::new(),
            reasoning_effort: String::new(),
            use_openai_api: Some("on".into()),
            openai_base_url: "api.example.com/v1".into(),
            openai_api_key: "sk-test".into(),
        },
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
}

#[tokio::test]
async fn reasoning_effort_persists_in_pi_mode() {
    let db = database().await;
    let state = state(db.clone());
    let result = save(
        &state,
        SettingsForm {
            model: "gpt-5.4-mini".into(),
            reasoning_effort: "high".into(),
            use_openai_api: None,
            openai_base_url: String::new(),
            openai_api_key: String::new(),
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(selected_effort(&db, "low").await.unwrap(), "high");
}

#[tokio::test]
async fn reasoning_effort_persists_in_openai_mode() {
    let db = database().await;
    let result = save(
        &state(db.clone()),
        SettingsForm {
            model: String::new(),
            reasoning_effort: "medium".into(),
            use_openai_api: Some("on".into()),
            openai_base_url: String::new(),
            openai_api_key: "sk-test".into(),
        },
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(selected_effort(&db, "low").await.unwrap(), "medium");
}

#[tokio::test]
async fn blank_effort_defaults_to_low() {
    let db = database().await;
    let result = save(
        &state(db.clone()),
        SettingsForm {
            model: "gpt-5.4-mini".into(),
            reasoning_effort: String::new(),
            use_openai_api: None,
            openai_base_url: String::new(),
            openai_api_key: String::new(),
        },
    )
    .await;
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
            model: "gpt-5.4-mini".into(),
            reasoning_effort: "extreme".into(),
            use_openai_api: None,
            openai_base_url: String::new(),
            openai_api_key: String::new(),
        },
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
    // The rejected value is not stored.
    assert_eq!(selected_effort(&db, "low").await.unwrap(), "low");
}
