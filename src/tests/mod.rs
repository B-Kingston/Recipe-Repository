mod ai;
mod auth;
mod chart;
mod chat;
mod config;
mod display;
mod folders;
mod groups;
mod htmx;
mod media;
mod moments;
mod notes;
mod recipes;
mod scale;
mod search;
mod settings;
mod thumbs;

use crate::AppState;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::sync::Arc;

/// A fresh in-memory database on one connection, every migration applied.
pub(crate) async fn memory_db() -> SqlitePool {
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

/// The state handlers run against in tests: no AI helpers, no embedding
/// sidecar, mail off, and empty in-memory maps.
pub(crate) fn test_state(db: SqlitePool) -> Arc<AppState> {
    Arc::new(AppState {
        db,
        model: String::new(),
        pi_worker_path: String::new(),
        auth_script_path: String::new(),
        claude_auth_script_path: String::new(),
        search_grounding: false,
        codex_flows: Default::default(),
        claude_flows: Default::default(),
        model_catalogue: Default::default(),
        media_debug_runs: Default::default(),
        thumbnail_jobs: Default::default(),
        moment_jobs: Default::default(),
        embedder: None,
        rate_limits: Default::default(),
        invites: Default::default(),
    })
}

pub(crate) fn user(id: &str) -> crate::AuthUser {
    crate::AuthUser { id: id.into() }
}

/// A response's body as text.
pub(crate) async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}
