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

/// Whether one account's provider value can run: a connected subscription,
/// or one of its endpoints with a key (a custom OpenAI-compatible server may
/// have none). A subscription counts once any credential is stored; one the
/// provider refused is flagged on its row instead.
pub(crate) async fn provider_configured(
    db: &SqlitePool,
    user_id: &str,
    provider: &str,
) -> Result<bool> {
    if let Some(pi_provider) = crate::pi_provider_for(provider) {
        return Ok(pi_credential(db, user_id, pi_provider).await?.is_some());
    }
    Ok(find_endpoint(db, user_id, provider)
        .await?
        .is_some_and(|endpoint| {
            !endpoint.api_key.trim().is_empty() || endpoint.spec == crate::OPENAI_COMPATIBLE_SPEC
        }))
}

/// Whether the account has an AI setup of its own: its active provider is a
/// connected subscription or one of its endpoints.
pub(crate) async fn own_ai_configured(db: &SqlitePool, user_id: &str) -> Result<bool> {
    provider_configured(db, user_id, &ai_provider(db, user_id).await?).await
}

/// app_settings key of the group member whose AI logins a user picked when
/// several members grant theirs (a user id; absent means the earliest grant).
pub(crate) const AI_SOURCE_SETTING: &str = "ai_source";

/// The members who let `user_id` use their AI logins, as (id, username),
/// earliest grant first. Only grants between two current members of the
/// grant's group count, so leaving or dissolving a group ends them even
/// before their rows go.
pub(crate) async fn ai_grants_received(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT g.granter_id, u.username FROM ai_grants g
         JOIN group_members me ON me.user_id = g.grantee_id AND me.group_id = g.group_id
         JOIN group_members them ON them.user_id = g.granter_id AND them.group_id = g.group_id
         JOIN users u ON u.id = g.granter_id
         WHERE g.grantee_id = ?
         ORDER BY g.created_at, g.rowid",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// The members `granter_id` lets use its AI logins, by user id, under the
/// same current-membership rule as `ai_grants_received`.
pub(crate) async fn ai_grantees(db: &SqlitePool, granter_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT g.grantee_id FROM ai_grants g
         JOIN group_members me ON me.user_id = g.granter_id AND me.group_id = g.group_id
         JOIN group_members them ON them.user_id = g.grantee_id AND them.group_id = g.group_id
         WHERE g.granter_id = ?
         ORDER BY g.created_at, g.rowid",
    )
    .bind(granter_id)
    .fetch_all(db)
    .await?)
}

/// Replaces the members `granter_id` lets use its AI logins with `grantees`
/// (members of `group_id`; the caller checks). A grant kept keeps its date,
/// so the grantees' default choice does not move.
pub(crate) async fn set_ai_grants(
    db: &SqlitePool,
    group_id: &str,
    granter_id: &str,
    grantees: &[String],
) -> Result<()> {
    let mut tx = crate::recipes::begin_write(db).await?;
    let existing: Vec<String> =
        sqlx::query_scalar("SELECT grantee_id FROM ai_grants WHERE granter_id = ?")
            .bind(granter_id)
            .fetch_all(&mut *tx)
            .await?;
    for grantee in existing.iter().filter(|id| !grantees.contains(id)) {
        sqlx::query("DELETE FROM ai_grants WHERE granter_id = ? AND grantee_id = ?")
            .bind(granter_id)
            .bind(grantee)
            .execute(&mut *tx)
            .await?;
    }
    for grantee in grantees.iter().filter(|id| !existing.contains(id)) {
        sqlx::query(
            "INSERT INTO ai_grants (group_id, granter_id, grantee_id, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(group_id)
        .bind(granter_id)
        .bind(grantee)
        .bind(stamp())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The account whose AI provider, subscriptions, API keys, models, effort,
/// video cleaner, and search key serve `user_id`: its own when it has an AI
/// setup of its own; else that of a group member who granted it theirs (the
/// one it picked in Settings, else the earliest grant), skipping members
/// with no AI setup either; else its own, which then fails as not
/// configured. Resolving an AI account again returns it unchanged, since a
/// member only serves others while its own setup is configured.
pub(crate) async fn ai_account(db: &SqlitePool, user_id: &str) -> Result<String> {
    if own_ai_configured(db, user_id).await? {
        return Ok(user_id.to_string());
    }
    let mut grants = ai_grants_received(db, user_id).await?;
    if let Some(picked) = app_setting(db, user_id, AI_SOURCE_SETTING).await?
        && let Some(index) = grants.iter().position(|(id, _)| *id == picked)
    {
        let chosen = grants.remove(index);
        grants.insert(0, chosen);
    }
    for (granter, _) in grants {
        if own_ai_configured(db, &granter).await? {
            return Ok(granter);
        }
    }
    Ok(user_id.to_string())
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
/// the choice of the account whose AI logins the user uses (`ai_account`).
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

/// The engine that runs the user's searches: their own choice, else that of
/// the account whose AI logins they use, else search by meaning.
async fn search_engine(db: &SqlitePool, user_id: &str) -> Result<&'static str> {
    let own = own_search_engine(db, user_id).await?;
    if own != SEARCH_SHARED {
        return Ok(own);
    }
    let ai_user = ai_account(db, user_id).await?;
    let followed = if ai_user == user_id {
        SEARCH_SHARED
    } else {
        own_search_engine(db, &ai_user).await?
    };
    Ok(if followed == SEARCH_BM25 {
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
