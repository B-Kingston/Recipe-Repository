//! What each account keeps in SQLite besides its recipes: app_settings values,
//! subscription credentials, AI endpoints, and the web search key. The queries
//! on those tables live here, apart from account setup and deletion in
//! auth.rs, which claim or drop every per-account row at once; main.rs
//! re-exports these, so callers name them from the crate root.

use crate::{
    AppError, DEFAULT_CLAUDE_MODEL, Endpoint, PROVIDER_CLAUDE, PROVIDER_CODEX, Result,
    WebSearchKey, stamp,
};
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::info;
use uuid::Uuid;

/// The stored subscription OAuth credential for a Pi provider id, if any. The
/// database is the source of truth; the Pi worker only ever sees a per-request
/// materialised auth.json.
pub(crate) async fn pi_credential(
    db: &SqlitePool,
    user_id: &str,
    pi_provider: &str,
) -> Result<Option<Value>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT credential_json FROM pi_credentials WHERE user_id=? AND provider=?")
            .bind(user_id)
            .bind(pi_provider)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(json,)| serde_json::from_str(&json).ok()))
}

/// Persists (or replaces) the credential for one user and provider id.
pub(crate) async fn store_pi_credential<T: Serialize>(
    db: &SqlitePool,
    user_id: &str,
    pi_provider: &str,
    credential: &T,
) -> Result<()> {
    let json = serde_json::to_string(credential)
        .map_err(|_| AppError::Internal("Credential could not be serialised.".into()))?;
    sqlx::query(
        "INSERT INTO pi_credentials (user_id,provider,credential_json,updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(user_id,provider) DO UPDATE SET
           credential_json = excluded.credential_json,
           updated_at = excluded.updated_at,
           auth_failed_at = NULL",
    )
    .bind(user_id)
    .bind(pi_provider)
    .bind(json)
    .bind(stamp())
    .execute(db)
    .await?;
    Ok(())
}

/// Writes back the token a worker run refreshed, but only over the
/// credential the run started from (`started_from`): a login the cook
/// disconnected, reconnected, or deleted with their account during the run
/// stays as they left it. A refresh alone does not prove the login, so an
/// earlier refusal stays flagged until a run signs in (`track_login`).
pub(crate) async fn store_refreshed_pi_credential(
    db: &SqlitePool,
    user_id: &str,
    pi_provider: &str,
    started_from: &Value,
    refreshed: &Value,
) -> Result<()> {
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT credential_json FROM pi_credentials WHERE user_id=? AND provider=?",
    )
    .bind(user_id)
    .bind(pi_provider)
    .fetch_optional(db)
    .await?;
    let Some(stored) = stored.filter(|stored| {
        serde_json::from_str::<Value>(stored).is_ok_and(|stored| stored == *started_from)
    }) else {
        return Ok(());
    };
    let json = serde_json::to_string(refreshed)
        .map_err(|_| AppError::Internal("Credential could not be serialised.".into()))?;
    // Matching the stored text keeps a change made since the read above.
    sqlx::query(
        "UPDATE pi_credentials SET credential_json=?, updated_at=?
         WHERE user_id=? AND provider=? AND credential_json=?",
    )
    .bind(json)
    .bind(stamp())
    .bind(user_id)
    .bind(pi_provider)
    .bind(stored)
    .execute(db)
    .await?;
    Ok(())
}

/// Removes one user's credential for a provider id.
pub(crate) async fn remove_pi_credential(
    db: &SqlitePool,
    user_id: &str,
    pi_provider: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM pi_credentials WHERE user_id=? AND provider=?")
        .bind(user_id)
        .bind(pi_provider)
        .execute(db)
        .await?;
    Ok(())
}

/// A login the worker signs in with: one user's subscription credential or
/// an endpoint's API key (endpoint ids are unique across users).
#[derive(Clone, Copy)]
pub(crate) enum Login<'a> {
    Subscription {
        user_id: &'a str,
        pi_provider: &'a str,
    },
    Endpoint {
        id: &'a str,
    },
}

/// Records whether a provider refused a login: `true` stamps the failure,
/// `false` clears an earlier one (a no-op when none is recorded).
pub(crate) async fn mark_login_refused(
    db: &SqlitePool,
    login: Login<'_>,
    refused: bool,
) -> Result<()> {
    let failed_at = refused.then(stamp);
    let query = match login {
        Login::Subscription {
            user_id,
            pi_provider,
        } => sqlx::query(
            "UPDATE pi_credentials SET auth_failed_at=?
             WHERE user_id=? AND provider=? AND (? OR auth_failed_at IS NOT NULL)",
        )
        .bind(failed_at)
        .bind(user_id)
        .bind(pi_provider),
        Login::Endpoint { id } => sqlx::query(
            "UPDATE ai_endpoints SET auth_failed_at=?
             WHERE id=? AND (? OR auth_failed_at IS NOT NULL)",
        )
        .bind(failed_at)
        .bind(id),
    };
    query.bind(refused).execute(db).await?;
    Ok(())
}

/// Whether the provider refused this subscription's login on its last use.
pub(crate) async fn subscription_login_refused(
    db: &SqlitePool,
    user_id: &str,
    pi_provider: &str,
) -> Result<bool> {
    let row: Option<(bool,)> = sqlx::query_as(
        "SELECT auth_failed_at IS NOT NULL FROM pi_credentials WHERE user_id=? AND provider=?",
    )
    .bind(user_id)
    .bind(pi_provider)
    .fetch_optional(db)
    .await?;
    Ok(row.is_some_and(|(refused,)| refused))
}

/// One of the user's app_settings values, if it has been saved.
pub(crate) async fn app_setting(
    db: &SqlitePool,
    user_id: &str,
    key: &str,
) -> Result<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM app_settings WHERE user_id = ? AND key = ?")
            .bind(user_id)
            .bind(key)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(value,)| value))
}

/// Saves one of the user's app_settings values, replacing any previous one.
pub(crate) async fn set_app_setting(
    db: &SqlitePool,
    user_id: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO app_settings (user_id, key, value, updated_at) VALUES (?, ?, ?, ?)
         ON CONFLICT(user_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(key)
    .bind(value)
    .bind(stamp())
    .execute(db)
    .await?;
    Ok(())
}

/// One of the user's app_settings values; None when unsaved or blank.
async fn non_blank_setting(db: &SqlitePool, user_id: &str, key: &str) -> Result<Option<String>> {
    Ok(app_setting(db, user_id, key)
        .await?
        .filter(|value| !value.trim().is_empty()))
}

/// One of the user's app_settings values when it is one of `choices`, else
/// `default` (unsaved, or a value no longer offered).
async fn choice_setting(
    db: &SqlitePool,
    user_id: &str,
    key: &str,
    choices: &[&'static str],
    default: &'static str,
) -> Result<&'static str> {
    let value = app_setting(db, user_id, key).await?;
    Ok(choices
        .iter()
        .copied()
        .find(|choice| value.as_deref() == Some(*choice))
        .unwrap_or(default))
}

/// The user's active AI provider: "pi" (Codex), "claude" (Claude
/// subscription) or one of their ai_endpoints row ids. Stored per user like
/// the model; reconcile_ai_provider keeps the value valid at startup and the
/// endpoint handlers keep it valid at runtime. Anything absent resolves to
/// "pi".
pub(crate) async fn ai_provider(db: &SqlitePool, user_id: &str) -> Result<String> {
    Ok(app_setting(db, user_id, "ai_provider")
        .await?
        .unwrap_or_else(|| PROVIDER_CODEX.into()))
}

/// app_settings key of an account's AI-login choice: "own" uses the account's
/// own providers, subscriptions, keys, and models; anything else uses the
/// shared account's.
pub(crate) const AI_LOGINS_SETTING: &str = "ai_logins";

/// The account whose AI setup every other account shares by default: the
/// first one created, as (id, username).
pub(crate) async fn shared_ai_account(db: &SqlitePool) -> Result<Option<(String, String)>> {
    Ok(
        sqlx::query_as("SELECT id, username FROM users ORDER BY created_at, id LIMIT 1")
            .fetch_optional(db)
            .await?,
    )
}

/// Whether the user chose their own AI setup over the shared one.
pub(crate) async fn uses_own_ai(db: &SqlitePool, user_id: &str) -> Result<bool> {
    Ok(app_setting(db, user_id, AI_LOGINS_SETTING)
        .await?
        .as_deref()
        == Some("own"))
}

/// The account whose AI provider, subscriptions, API keys, models, and video
/// cleaner serve `user_id`'s generations: the shared account's, unless the
/// user chose their own. The shared account always uses its own. Resolving an
/// AI account again returns it unchanged.
pub(crate) async fn ai_account(db: &SqlitePool, user_id: &str) -> Result<String> {
    if uses_own_ai(db, user_id).await? {
        return Ok(user_id.to_string());
    }
    Ok(shared_ai_account(db)
        .await?
        .map_or_else(|| user_id.to_string(), |(id, _)| id))
}

pub(crate) async fn selected_model(
    db: &SqlitePool,
    user_id: &str,
    fallback: &str,
) -> Result<String> {
    Ok(app_setting(db, user_id, "model")
        .await?
        .unwrap_or_else(|| fallback.into()))
}

/// The Claude subscription model; separate from the Codex `model` setting so
/// switching providers keeps each choice.
pub(crate) async fn selected_claude_model(db: &SqlitePool, user_id: &str) -> Result<String> {
    Ok(app_setting(db, user_id, "claude_model")
        .await?
        .unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.into()))
}

pub(crate) async fn selected_effort(
    db: &SqlitePool,
    user_id: &str,
    fallback: &str,
) -> Result<String> {
    Ok(app_setting(db, user_id, "reasoning_effort")
        .await?
        .unwrap_or_else(|| fallback.into()))
}

/// Heals stale ai_provider settings at startup: the pre-endpoint "openai"
/// value or an id that is not one of the user's endpoints resolves to their
/// first registered endpoint, falling back to Codex ("pi"). Missing settings
/// are left alone (the read path defaults to "pi").
pub(crate) async fn reconcile_ai_provider(db: &SqlitePool) -> Result<()> {
    let settings: Vec<(String, String)> =
        sqlx::query_as("SELECT user_id, value FROM app_settings WHERE key = 'ai_provider'")
            .fetch_all(db)
            .await?;
    for (user_id, current) in settings {
        if current == PROVIDER_CODEX || current == PROVIDER_CLAUDE {
            continue;
        }
        let live: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM ai_endpoints WHERE id=? AND user_id=? LIMIT 1")
                .bind(&current)
                .bind(&user_id)
                .fetch_optional(db)
                .await?;
        if live.is_some() {
            continue;
        }
        let first: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM ai_endpoints WHERE user_id=? ORDER BY created_at LIMIT 1",
        )
        .bind(&user_id)
        .fetch_optional(db)
        .await?;
        let value = first
            .map(|(id,)| id)
            .unwrap_or_else(|| PROVIDER_CODEX.into());
        set_app_setting(db, &user_id, "ai_provider", &value).await?;
        info!(%user_id, from = %current, to = %value, "Reconciled stale AI provider setting");
    }
    Ok(())
}

/// Codex models the pi SDK no longer serves. A stored choice of one of these
/// would fail every generation, so startup moves it to the default model.
const RETIRED_CODEX_MODELS: &[&str] = &["gpt-5.4-mini", "gpt-5.4"];

/// Replaces every user's stored Codex model that the pi SDK no longer serves
/// with the PI_MODEL default. A user without a stored choice already gets the
/// default from `selected_model`.
pub(crate) async fn retire_codex_models(db: &SqlitePool, model: &str) -> Result<()> {
    for retired in RETIRED_CODEX_MODELS {
        sqlx::query("UPDATE app_settings SET value=?, updated_at=? WHERE key='model' AND value=?")
            .bind(model)
            .bind(stamp())
            .bind(retired)
            .execute(db)
            .await?;
    }
    Ok(())
}

pub(crate) const MEDIA_CLEANER_PROVIDER_SETTING: &str = "media_cleaner_provider";
pub(crate) const MEDIA_CLEANER_MODEL_SETTING: &str = "media_cleaner_model";

/// The video cleaner's own provider ("pi", "claude", or an endpoint id);
/// None follows the active AI provider.
pub(crate) async fn media_cleaner_setting(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Option<String>> {
    non_blank_setting(db, user_id, MEDIA_CLEANER_PROVIDER_SETTING).await
}

/// The video cleaner's model; None uses its provider's model.
pub(crate) async fn media_cleaner_model(db: &SqlitePool, user_id: &str) -> Result<Option<String>> {
    non_blank_setting(db, user_id, MEDIA_CLEANER_MODEL_SETTING).await
}

/// app_settings key of the units the user's recipes are generated in:
/// "metric", "imperial", or "source" / absent (whatever suits the recipe).
pub(crate) const MEASUREMENT_SETTING: &str = "measurement_system";
pub(crate) const MEASUREMENT_METRIC: &str = "metric";
pub(crate) const MEASUREMENT_IMPERIAL: &str = "imperial";
pub(crate) const MEASUREMENT_SOURCE: &str = "source";
pub(crate) const MEASUREMENT_SYSTEMS: &[&str] =
    &[MEASUREMENT_METRIC, MEASUREMENT_IMPERIAL, MEASUREMENT_SOURCE];

/// The user's measurement choice: "metric", "imperial" or "source".
pub(crate) async fn measurement_system(db: &SqlitePool, user_id: &str) -> Result<&'static str> {
    choice_setting(
        db,
        user_id,
        MEASUREMENT_SETTING,
        MEASUREMENT_SYSTEMS,
        MEASUREMENT_SOURCE,
    )
    .await
}

/// The prompt line that holds generated and AI-edited recipes to the user's
/// units, or None to leave the units to the recipe.
pub(crate) fn measurement_rule(system: &str) -> Option<&'static str> {
    match system {
        MEASUREMENT_METRIC => Some(
            "Units: write every ingredient quantity, ingredientUses amount, temperature, and size in metric units (g, kg, ml, l, °C, cm). Weigh dry ingredients in grams where a cook would; keep tsp and tbsp for small amounts and counted items as counts. Convert any other units from the source or request, rounding to practical amounts.",
        ),
        MEASUREMENT_IMPERIAL => Some(
            "Units: write every ingredient quantity, ingredientUses amount, temperature, and size in US customary units (cups, tbsp, tsp, oz, lb, fl oz, °F, inches), using fractions such as 1/2 or 1 1/4 rather than decimals. Keep counted items as counts. Convert any metric units from the source or request, rounding to practical amounts.",
        ),
        _ => None,
    }
}

/// app_settings key of the user's "video step moments" switch.
pub(crate) const STEP_MOMENTS_SETTING: &str = "step_moments";

/// Whether the user's video imports capture, and their recipe pages show,
/// step moments. On unless switched off in Settings.
pub(crate) async fn step_moments_enabled(db: &SqlitePool, user_id: &str) -> Result<bool> {
    Ok(app_setting(db, user_id, STEP_MOMENTS_SETTING)
        .await?
        .as_deref()
        != Some("off"))
}

/// app_settings key of the user's library search engine: "embeddings" (search
/// by meaning), "bm25" (word search) or "shared" (absent too), which follows
/// the shared account's choice the way AI logins do.
pub(crate) const SEARCH_ENGINE_SETTING: &str = "search_engine";
pub(crate) const SEARCH_EMBEDDINGS: &str = "embeddings";
pub(crate) const SEARCH_BM25: &str = "bm25";
pub(crate) const SEARCH_SHARED: &str = "shared";
pub(crate) const SEARCH_ENGINES: &[&str] = &[SEARCH_EMBEDDINGS, SEARCH_BM25, SEARCH_SHARED];

/// The user's own saved search choice: "embeddings", "bm25" or "shared".
pub(crate) async fn own_search_engine(db: &SqlitePool, user_id: &str) -> Result<&'static str> {
    choice_setting(
        db,
        user_id,
        SEARCH_ENGINE_SETTING,
        SEARCH_ENGINES,
        SEARCH_SHARED,
    )
    .await
}

/// The engine that runs the user's searches: their own choice, else the
/// shared account's, else search by meaning.
async fn search_engine(db: &SqlitePool, user_id: &str) -> Result<&'static str> {
    let own = own_search_engine(db, user_id).await?;
    if own != SEARCH_SHARED {
        return Ok(own);
    }
    let shared = match shared_ai_account(db).await? {
        Some((id, _)) if id != user_id => own_search_engine(db, &id).await?,
        _ => SEARCH_SHARED,
    };
    Ok(if shared == SEARCH_BM25 {
        SEARCH_BM25
    } else {
        SEARCH_EMBEDDINGS
    })
}

/// Whether the user's library searches use BM25 word search for every query
/// instead of search by meaning (which falls back to BM25 on its own).
pub(crate) async fn keyword_search_selected(db: &SqlitePool, user_id: &str) -> Result<bool> {
    Ok(search_engine(db, user_id).await? == SEARCH_BM25)
}

/// The `Endpoint` columns of an ai_endpoints row.
const ENDPOINT_COLUMNS: &str =
    "id,name,spec,base_url,api_key,model,auth_failed_at IS NOT NULL AS auth_failed";

/// All registered AI endpoints for one user, oldest first.
pub(crate) async fn list_endpoints(db: &SqlitePool, user_id: &str) -> Result<Vec<Endpoint>> {
    let sql =
        format!("SELECT {ENDPOINT_COLUMNS} FROM ai_endpoints WHERE user_id=? ORDER BY created_at");
    Ok(sqlx::query_as(&sql).bind(user_id).fetch_all(db).await?)
}

/// One registered AI endpoint for one user, if it exists.
pub(crate) async fn find_endpoint(
    db: &SqlitePool,
    user_id: &str,
    id: &str,
) -> Result<Option<Endpoint>> {
    let sql = format!("SELECT {ENDPOINT_COLUMNS} FROM ai_endpoints WHERE user_id=? AND id=?");
    Ok(sqlx::query_as(&sql)
        .bind(user_id)
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// Registers a new AI endpoint for one user; returns its id.
pub(crate) async fn insert_endpoint(
    db: &SqlitePool,
    user_id: &str,
    name: &str,
    spec: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let now = stamp();
    sqlx::query("INSERT INTO ai_endpoints(id,user_id,name,spec,base_url,api_key,model,created_at,updated_at)VALUES(?,?,?,?,?,?,?,?,?)")
        .bind(&id)
        .bind(user_id)
        .bind(name)
        .bind(spec)
        .bind(base_url)
        .bind(api_key)
        .bind(model)
        .bind(&now)
        .bind(&now)
        .execute(db)
        .await?;
    Ok(id)
}

/// Removes one of the user's endpoints with its key.
pub(crate) async fn remove_endpoint(db: &SqlitePool, user_id: &str, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM ai_endpoints WHERE user_id=? AND id=?")
        .bind(user_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Replaces the model of one user's endpoint.
pub(crate) async fn update_endpoint_model(
    db: &SqlitePool,
    user_id: &str,
    id: &str,
    model: &str,
) -> Result<()> {
    sqlx::query("UPDATE ai_endpoints SET model=?, updated_at=? WHERE user_id=? AND id=?")
        .bind(model)
        .bind(stamp())
        .bind(user_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// One account's own search API key, if it saved one.
pub(crate) async fn web_search_key(db: &SqlitePool, user_id: &str) -> Result<Option<WebSearchKey>> {
    Ok(
        sqlx::query_as("SELECT provider,api_key FROM web_search_keys WHERE user_id=?")
            .bind(user_id)
            .fetch_optional(db)
            .await?,
    )
}

/// Saves (or replaces) one account's search API key.
pub(crate) async fn store_web_search_key(
    db: &SqlitePool,
    user_id: &str,
    provider: &str,
    api_key: &str,
) -> Result<()> {
    let now = stamp();
    sqlx::query("INSERT INTO web_search_keys(user_id,provider,api_key,created_at,updated_at)VALUES(?,?,?,?,?) ON CONFLICT(user_id) DO UPDATE SET provider=excluded.provider, api_key=excluded.api_key, updated_at=excluded.updated_at")
        .bind(user_id)
        .bind(provider)
        .bind(api_key)
        .bind(&now)
        .bind(&now)
        .execute(db)
        .await?;
    Ok(())
}

/// Removes one account's search API key.
pub(crate) async fn remove_web_search_key(db: &SqlitePool, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM web_search_keys WHERE user_id=?")
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(())
}
