use crate::recipes::{find_draft, find_recipe, recipe_snapshot};
use crate::{
    AppError, AppState, AuthUser, ChartRecipe, DRAFT_HOURS, DraftTemplate, GROUNDED_RECIPE_PROMPT,
    GeneratedRecipe, Ingredient, IngredientUse, ModelCatalogue, PromptForm, RECIPE_PROMPT, Result,
    Source, generate_guidance, render, required, stamp,
};
use axum::{
    Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    process::{Command, Stdio},
    sync::Arc,
    time::Instant,
};
use tracing::{error, info, warn};
use uuid::Uuid;

pub(crate) async fn generate_page() -> Redirect {
    Redirect::to("/recipes/new")
}

pub(crate) async fn generate_recipe(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(f): Form<PromptForm>,
) -> Result<Response> {
    let prompt = required(&f.prompt, "Recipe idea or URL")?;
    match create_draft(&s, &user, None, "generate", &prompt).await {
        Ok(id) => Ok(Redirect::to(&format!("/ai/drafts/{id}")).into_response()),
        Err(e) => render(crate::AiFormTemplate {
            heading: "New Recipe".into(),
            guidance: generate_guidance(s.search_grounding).into(),
            action: "/ai/generate".into(),
            label: "What should this recipe be based on?".into(),
            button: "Research & generate".into(),
            cancel_url: "/".into(),
            error: e.to_string(),
            prompt,
        })
        .map(IntoResponse::into_response),
    }
}

pub(crate) async fn alter_page(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    render(crate::AiFormTemplate {
        heading: "Alter with AI".into(),
        guidance: format!(
            "Tell Pi how to change “{}”. It will return a complete replacement recipe for review.",
            recipe.title
        ),
        action: format!("/recipes/{id}/ai/alter"),
        label: "What should change?".into(),
        button: "Create altered draft".into(),
        cancel_url: format!("/recipes/{id}"),
        error: String::new(),
        prompt: String::new(),
    })
}

pub(crate) async fn alter_recipe(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(f): Form<PromptForm>,
) -> Result<Response> {
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    let prompt = required(&f.prompt, "Comments")?;
    let snapshot = recipe_snapshot(&s.db, &user.id, &recipe).await?;
    let full = format!(
        "User requested changes:\n{}\n\nCurrent recipe JSON:\n{}",
        prompt,
        serde_json::to_string(&snapshot).map_err(|_| AppError::Ai)?
    );
    match create_draft(
        &s,
        &user,
        Some((&recipe.id, &recipe.updated_at)),
        "alter",
        &full,
    )
    .await
    {
        Ok(draft) => Ok(Redirect::to(&format!("/ai/drafts/{draft}")).into_response()),
        Err(e) => render(crate::AiFormTemplate {
            heading: "Alter with AI".into(),
            guidance: "Tell Pi what should change.".into(),
            action: format!("/recipes/{id}/ai/alter"),
            label: "What should change?".into(),
            button: "Create altered draft".into(),
            cancel_url: format!("/recipes/{id}"),
            error: e.to_string(),
            prompt,
        })
        .map(IntoResponse::into_response),
    }
}

pub(crate) async fn draft_page(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    render_draft_page(&s, &user, &id, "", "").await
}

/// Alters the recipe held in a draft, opening the result as a new draft.
/// The alteration chain keeps pointing at the original saved recipe (if any),
/// so applying the final draft still updates that recipe, guarded by the
/// staleness check against the version this chain was based on.
pub(crate) async fn alter_draft(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(f): Form<PromptForm>,
) -> Result<Response> {
    let draft = find_draft(&s.db, &user.id, &id).await?;
    let prompt = required(&f.prompt, "Comments")?;
    let full = format!(
        "User requested changes:\n{}\n\nCurrent recipe JSON:\n{}",
        prompt, draft.recipe_json
    );
    let base = draft.recipe_id.as_deref().map(|recipe_id| {
        (
            recipe_id,
            draft.base_updated_at.as_deref().unwrap_or_default(),
        )
    });
    match create_draft(&s, &user, base, "alter", &full).await {
        Ok(new_id) => Ok(Redirect::to(&format!("/ai/drafts/{new_id}")).into_response()),
        Err(e) => render_draft_page(&s, &user, &id, &e.to_string(), &prompt).await,
    }
}

async fn render_draft_page(
    state: &AppState,
    user: &AuthUser,
    id: &str,
    error: &str,
    prompt: &str,
) -> Result<Response> {
    let draft = find_draft(&state.db, &user.id, id).await?;
    let mut recipe: GeneratedRecipe =
        serde_json::from_str(&draft.recipe_json).map_err(|_| AppError::Ai)?;
    normalize_generated(&mut recipe)?;
    render(DraftTemplate {
        id: id.to_string(),
        recipe,
        sources: serde_json::from_str(&draft.sources_json).map_err(|_| AppError::Ai)?,
        suggestions: draft.search_suggestions,
        error: error.to_string(),
        prompt: prompt.to_string(),
    })
    .map(IntoResponse::into_response)
}

pub(crate) async fn apply_draft(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    let draft = find_draft(&s.db, &user.id, &id).await?;
    let mut recipe: GeneratedRecipe =
        serde_json::from_str(&draft.recipe_json).map_err(|_| AppError::Ai)?;
    normalize_generated(&mut recipe)?;
    let sources: Vec<Source> =
        serde_json::from_str(&draft.sources_json).map_err(|_| AppError::Ai)?;
    let chart_json =
        serde_json::to_string(&ChartRecipe::from_generated(&recipe)).map_err(|_| AppError::Ai)?;
    let mut tx = s.db.begin().await?;
    let now = stamp();
    let recipe_id = if let Some(existing) = draft.recipe_id {
        if draft.base_updated_at.as_deref()
            != Some(&find_recipe(&s.db, &user.id, &existing).await?.updated_at)
        {
            return Err(AppError::BadRequest(
                "This recipe changed after the draft was made. Generate a new alteration.".into(),
            ));
        }
        sqlx::query("UPDATE recipes SET title=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,chart_json=?,updated_at=? WHERE user_id=? AND id=?")
            .bind(&recipe.title)
            .bind(&recipe.description)
            .bind(recipe.servings)
            .bind(recipe.prep_minutes)
            .bind(recipe.cook_minutes)
            .bind(&chart_json)
            .bind(&now)
            .bind(&user.id)
            .bind(&existing)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM recipe_blocks WHERE recipe_id=?")
            .bind(&existing)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM recipe_sources WHERE recipe_id=?")
            .bind(&existing)
            .execute(&mut *tx)
            .await?;
        existing
    } else {
        let recipe_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO recipes(id,title,description,servings,prep_minutes,cook_minutes,chart_json,user_id,created_at,updated_at)VALUES(?,?,?,?,?,?,?,?,?,?)")
            .bind(&recipe_id)
            .bind(&recipe.title)
            .bind(&recipe.description)
            .bind(recipe.servings)
            .bind(recipe.prep_minutes)
            .bind(recipe.cook_minutes)
            .bind(&chart_json)
            .bind(&user.id)
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        recipe_id
    };
    for (position, ingredient) in recipe.ingredients.iter().enumerate() {
        sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit,optional)VALUES(?,?,?,?,?,?,?,?)")
            .bind(Uuid::new_v4().to_string())
            .bind(&recipe_id)
            .bind("ingredient")
            .bind(position as i64)
            .bind(&ingredient.name)
            .bind(&ingredient.quantity)
            .bind(&ingredient.unit)
            .bind(if ingredient.optional { 1 } else { 0 })
            .execute(&mut *tx)
            .await?;
    }
    for (position, step) in recipe.steps.iter().enumerate() {
        let step_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO recipe_blocks(id,recipe_id,section,position,text)VALUES(?,?,?,?,?)",
        )
        .bind(&step_id)
        .bind(&recipe_id)
        .bind("step")
        .bind(position as i64)
        .bind(&step.text)
        .execute(&mut *tx)
        .await?;
        for (ingredient_position, ingredient) in step.ingredients.iter().enumerate() {
            sqlx::query(
                "INSERT INTO recipe_step_ingredients(id,step_id,position,text)VALUES(?,?,?,?)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(&step_id)
            .bind(ingredient_position as i64)
            .bind(ingredient)
            .execute(&mut *tx)
            .await?;
        }
    }
    for (position, source) in sources.iter().enumerate() {
        sqlx::query("INSERT INTO recipe_sources(id,recipe_id,position,title,url)VALUES(?,?,?,?,?)")
            .bind(Uuid::new_v4().to_string())
            .bind(&recipe_id)
            .bind(position as i64)
            .bind(&source.title)
            .bind(&source.url)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM ai_drafts WHERE id=? AND user_id=?")
        .bind(&id)
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{recipe_id}")).into_response())
}

pub(crate) async fn cancel_draft(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    sqlx::query("DELETE FROM ai_drafts WHERE id=? AND user_id=?")
        .bind(id)
        .bind(&user.id)
        .execute(&s.db)
        .await?;
    Ok(Redirect::to("/").into_response())
}

async fn create_draft(
    state: &AppState,
    user: &AuthUser,
    base: Option<(&str, &str)>,
    operation: &str,
    prompt: &str,
) -> Result<String> {
    let (generated_recipe, sources, suggestions) = pi_recipe(state, user, prompt).await?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    sqlx::query("DELETE FROM ai_drafts WHERE expires_at < ?")
        .bind(now.to_rfc3339())
        .execute(&state.db)
        .await?;
    sqlx::query("INSERT INTO ai_drafts(id,recipe_id,operation,recipe_json,sources_json,search_suggestions,base_updated_at,user_id,created_at,expires_at)VALUES(?,?,?,?,?,?,?,?,?,?)")
        .bind(&id)
        .bind(base.map(|(recipe_id, _)| recipe_id))
        .bind(operation)
        .bind(serde_json::to_string(&generated_recipe).map_err(|_| AppError::Ai)?)
        .bind(serde_json::to_string(&sources).map_err(|_| AppError::Ai)?)
        .bind(suggestions)
        .bind(base.map(|(_, base_updated_at)| base_updated_at))
        .bind(&user.id)
        .bind(now.to_rfc3339())
        .bind((now + Duration::hours(DRAFT_HOURS)).to_rfc3339())
        .execute(&state.db)
        .await?;
    Ok(id)
}

async fn pi_recipe(
    state: &AppState,
    user: &AuthUser,
    prompt: &str,
) -> Result<(GeneratedRecipe, Vec<Source>, String)> {
    if crate::ai_provider(&state.db).await? == "openai" {
        return openai_recipe(state, user, prompt).await;
    }
    let mut credential = crate::codex_credential(&state.db, &user.id)
        .await?
        .ok_or(AppError::AiNotConfigured)?;
    let model = crate::selected_model(&state.db, &state.model).await?;
    let effort = crate::selected_effort(&state.db, crate::DEFAULT_REASONING_EFFORT).await?;
    for attempt in 0..2 {
        let input = if attempt == 0 {
            prompt.to_string()
        } else if state.search_grounding {
            format!(
                "{prompt}\n\nImportant: research this thoroughly with web_search before answering. Return the complete recipe and cite only web_search result URLs."
            )
        } else {
            format!(
                "{prompt}\n\nImportant: return a complete recipe as valid JSON matching the requested schema."
            )
        };
        let system_prompt = format!(
            "{}\n\nRecipe JSON schema:\n{}",
            if state.search_grounding {
                GROUNDED_RECIPE_PROMPT
            } else {
                RECIPE_PROMPT
            },
            recipe_schema()
        );
        info!(
            provider = "codex",
            model = %model,
            reasoning_effort = %effort,
            search_enabled = state.search_grounding,
            attempt,
            prompt = %input,
            system_prompt = %system_prompt,
            "LLM request"
        );
        let started = Instant::now();
        let request = json!({
            "prompt": input,
            "systemPrompt": system_prompt,
            "model": model,
            "reasoningEffort": effort,
            "searchEnabled": state.search_grounding,
        });
        let (output, refreshed) =
            run_pi_worker_with_credential(state, &credential, &request).await?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if let Some(refreshed) = refreshed
            && refreshed != credential
        {
            credential = refreshed.clone();
            crate::store_codex_credential(&state.db, &user.id, &refreshed).await?;
        }
        let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| AppError::Ai)?;
        if !output.status.success() {
            let code = value["code"].as_str().unwrap_or("worker");
            let message = value["error"].as_str().unwrap_or("no message");
            if code == "configuration" || attempt == 1 {
                error!(code, message, status = %output.status, elapsed_ms, "Pi worker failed");
                return if code == "configuration" {
                    Err(AppError::AiNotConfigured)
                } else {
                    Err(AppError::Ai)
                };
            }
            warn!(code, message, status = %output.status, elapsed_ms, "Pi worker failed; retrying with research guidance");
            continue;
        }
        info!(
            provider = "codex",
            model = %model,
            reasoning_effort = %effort,
            attempt,
            elapsed_ms,
            response = %value,
            "LLM response"
        );
        match parse_pi_response(&value, state.search_grounding) {
            Ok(result) => return Ok(result),
            Err(error) => {
                if attempt == 1 {
                    error!(attempt, error = %error, "Pi worker output failed validation");
                } else {
                    warn!(attempt, error = %error, "Pi worker output failed validation; retrying");
                }
            }
        }
    }
    Err(AppError::Ai)
}

/// Generates through the OpenAI API Responses path (worker provider "openai"):
/// web_search tool when grounding is enabled, model pinned to gpt-5.6-luna,
/// same two-attempt retry contract as the Codex branch (attempt 1 re-prompted
/// with research guidance) but without credential refresh — the API key never
/// round-trips back from the worker.
async fn openai_recipe(
    state: &AppState,
    user: &AuthUser,
    prompt: &str,
) -> Result<(GeneratedRecipe, Vec<Source>, String)> {
    let Some((base_url, api_key)) = crate::openai_api_config(&state.db, &user.id).await? else {
        return Err(AppError::AiNotConfigured);
    };
    let model = crate::DEFAULT_OPENAI_MODEL;
    let effort = crate::selected_effort(&state.db, crate::DEFAULT_REASONING_EFFORT).await?;
    for attempt in 0..2 {
        let input = if attempt == 0 {
            prompt.to_string()
        } else if state.search_grounding {
            format!(
                "{prompt}\n\nImportant: research this thoroughly with web_search before answering. Return the complete recipe and cite only web_search result URLs."
            )
        } else {
            format!(
                "{prompt}\n\nImportant: return a complete recipe as valid JSON matching the requested schema."
            )
        };
        let system_prompt = format!(
            "{}\n\nRecipe JSON schema:\n{}",
            if state.search_grounding {
                GROUNDED_RECIPE_PROMPT
            } else {
                RECIPE_PROMPT
            },
            recipe_schema()
        );
        info!(
            provider = "openai",
            model = %model,
            reasoning_effort = %effort,
            search_enabled = state.search_grounding,
            attempt,
            prompt = %input,
            system_prompt = %system_prompt,
            "LLM request"
        );
        let started = Instant::now();
        let request = json!({
            "provider": "openai",
            "apiBaseUrl": base_url,
            "apiKey": api_key,
            "prompt": input,
            "systemPrompt": system_prompt,
            "model": model,
            "reasoningEffort": effort,
            "searchEnabled": state.search_grounding,
        });
        let payload = serde_json::to_vec(&request).map_err(|_| AppError::Ai)?;
        let worker_path = state.pi_worker_path.clone();
        let output = tokio::task::spawn_blocking(move || run_pi_worker(&worker_path, &payload))
            .await
            .map_err(|_| AppError::Ai)?
            .map_err(|_| AppError::Ai)?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| AppError::Ai)?;
        if !output.status.success() {
            let code = value["code"].as_str().unwrap_or("worker");
            let message = value["error"].as_str().unwrap_or("no message");
            if code == "configuration" || attempt == 1 {
                error!(code, message, status = %output.status, elapsed_ms, "OpenAI worker failed");
                return if code == "configuration" {
                    Err(AppError::AiNotConfigured)
                } else {
                    Err(AppError::Ai)
                };
            }
            warn!(code, message, status = %output.status, elapsed_ms, "OpenAI worker failed; retrying with research guidance");
            continue;
        }
        info!(
            provider = "openai",
            model = %model,
            reasoning_effort = %effort,
            attempt,
            elapsed_ms,
            response = %value,
            "LLM response"
        );
        match parse_pi_response(&value, state.search_grounding) {
            Ok(result) => return Ok(result),
            Err(error) => {
                if attempt == 1 {
                    error!(attempt, error = %error, "OpenAI worker output failed validation");
                } else {
                    warn!(attempt, error = %error, "OpenAI worker output failed validation; retrying");
                }
            }
        }
    }
    Err(AppError::Ai)
}

fn run_pi_worker(worker_path: &str, payload: &[u8]) -> std::io::Result<std::process::Output> {
    let mut child = Command::new("node")
        .arg(worker_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .expect("Pi worker stdin is piped")
        .write_all(payload)?;
    let output = child.wait_with_output()?;
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        let line = line.trim();
        if !line.is_empty() {
            info!(line, "Pi worker");
        }
    }
    Ok(output)
}

/// Runs the Pi worker with the database credential materialised as a private
/// per-request auth.json. The Pi SDK refreshes tokens in place in that file,
/// so the refreshed credential is read back for the caller to persist. The
/// worker's stdout is returned along with the refresh, if any.
async fn run_pi_worker_with_credential(
    state: &AppState,
    credential: &Value,
    request: &Value,
) -> Result<(std::process::Output, Option<Value>)> {
    let credential_json = serde_json::to_string(credential).map_err(|_| AppError::Ai)?;
    let temp_path =
        std::env::temp_dir().join(format!("kindle-recipes-auth-{}.json", Uuid::new_v4()));
    let mut request = request.clone();
    request["authPath"] = json!(temp_path.to_string_lossy().to_string());
    let payload = serde_json::to_vec(&request).map_err(|_| AppError::Ai)?;
    let worker_path = state.pi_worker_path.clone();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
            .map_err(|_| AppError::Ai)?;
        file.write_all(format!("{{\n  \"openai-codex\": {credential_json}\n}}\n").as_bytes())
            .map_err(|_| AppError::Ai)?;
        let output = run_pi_worker(&worker_path, &payload);
        let refreshed = std::fs::read_to_string(&temp_path).ok().and_then(|text| {
            serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("openai-codex").cloned())
        });
        let _ = std::fs::remove_file(&temp_path);
        output
            .map(|output| (output, refreshed))
            .map_err(|_| AppError::Ai)
    })
    .await
    .map_err(|_| AppError::Ai)?
    .map_err(|_| AppError::Ai)
}

/// Asks the Pi worker to refresh the Codex model catalogue from pi.dev and
/// returns the current model ids (newest first). Returns an empty list when
/// there is no Codex credential or the refresh fails; the Settings page then
/// falls back to the built-in model list.
pub(crate) async fn fetch_codex_models(state: &AppState, user_id: &str) -> Result<Vec<String>> {
    let mut credential = match crate::codex_credential(&state.db, user_id).await? {
        Some(credential) => credential,
        None => return Ok(Vec::new()),
    };
    let (output, refreshed) =
        run_pi_worker_with_credential(state, &credential, &json!({ "command": "listModels" }))
            .await?;
    if let Some(refreshed) = refreshed
        && refreshed != credential
    {
        credential = refreshed;
        crate::store_codex_credential(&state.db, user_id, &credential).await?;
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| AppError::Ai)?;
    if !output.status.success() {
        error!(status = %output.status, "Codex model list refresh failed");
        return Ok(Vec::new());
    }
    Ok(value["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// How long a successfully fetched model catalogue stays usable before the
/// Settings page triggers another background refresh.
const MODEL_CATALOGUE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Returns the live Codex model catalogue when a fresh copy is cached; otherwise
/// starts a background refresh and returns the stale cache (or an empty list on
/// first run). The page render never waits on the Pi worker, so a slow pi.dev
/// refresh cannot stall the Settings page. Only non-empty refreshes replace the
/// cache, so a transient failure retries on the next visit.
pub(crate) async fn fresh_model_catalogue(state: &Arc<AppState>, user_id: &str) -> Vec<String> {
    let cached = (*state.model_catalogue.lock()).clone();
    if cached
        .as_ref()
        .is_some_and(|catalogue| catalogue.refreshed_at.elapsed() < MODEL_CATALOGUE_TTL)
    {
        return cached.map(|catalogue| catalogue.models).unwrap_or_default();
    }
    let state = state.clone();
    let user_id = user_id.to_string();
    tokio::spawn(async move {
        match fetch_codex_models(&state, &user_id).await {
            Ok(models) if !models.is_empty() => {
                (*state.model_catalogue.lock()).replace(ModelCatalogue {
                    models: models.clone(),
                    refreshed_at: Instant::now(),
                });
            }
            Ok(_) => {}
            Err(error) => warn!(%error, "Codex model list refresh failed; keeping cached list"),
        }
    });
    cached.map(|catalogue| catalogue.models).unwrap_or_default()
}

pub(crate) fn parse_pi_response(
    value: &Value,
    require_grounding: bool,
) -> Result<(GeneratedRecipe, Vec<Source>, String)> {
    let mut recipe: GeneratedRecipe =
        serde_json::from_value(value["recipe"].clone()).map_err(|parse_error| {
            warn!(error = %parse_error, "AI response recipe failed schema deserialisation");
            AppError::Ai
        })?;
    let sources: Vec<Source> =
        serde_json::from_value(value["sources"].clone()).map_err(|parse_error| {
            warn!(error = %parse_error, "AI response sources failed deserialisation");
            AppError::Ai
        })?;
    let sources = dedupe_sources(sources);
    if require_grounding && sources.is_empty() {
        warn!("AI response carried no grounded search sources");
        return Err(AppError::Ai);
    }
    normalize_generated(&mut recipe)?;
    Ok((recipe, sources, String::new()))
}

pub(crate) fn recipe_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["title","description","prepMinutes","cookMinutes","servings","ingredients","steps"],"properties":{"title":{"type":"string"},"description":{"type":"string"},"prepMinutes":{"type":"integer"},"cookMinutes":{"type":"integer"},"servings":{"type":"integer"},"ingredients":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["name","quantity","unit","optional"],"properties":{"name":{"type":"string"},"quantity":{"type":"string"},"unit":{"type":"string"},"optional":{"type":"boolean"}}}},"steps":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["text","chartLabel","timerSeconds","ingredientUses","inputSteps"],"properties":{"text":{"type":"string"},"chartLabel":{"type":"string"},"timerSeconds":{"type":"integer","minimum":0},"ingredientUses":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["ingredient","amount"],"properties":{"ingredient":{"type":"integer","minimum":0},"amount":{"type":"string"}}}},"inputSteps":{"type":"array","items":{"type":"integer","minimum":0}}}}}}})
}

pub(crate) fn normalize_generated(recipe: &mut GeneratedRecipe) -> Result<()> {
    if recipe
        .steps
        .iter()
        .all(|step| step.chart_label.trim().is_empty())
    {
        // Old drafts stored only display strings; retain them as a safe linear flow.
        for index in 0..recipe.steps.len() {
            let step = &mut recipe.steps[index];
            step.chart_label = step
                .text
                .split_whitespace()
                .take(5)
                .collect::<Vec<_>>()
                .join(" ");
            step.timer_seconds = 0;
            for display in step.ingredients.clone() {
                if let Some((ingredient_index, ingredient)) = recipe
                    .ingredients
                    .iter()
                    .enumerate()
                    .find(|(_, ingredient)| {
                        display
                            .to_lowercase()
                            .ends_with(&ingredient.name.to_lowercase())
                    })
                {
                    let amount = display[..display.len() - ingredient.name.len()].trim();
                    step.ingredient_uses.push(IngredientUse {
                        ingredient: ingredient_index,
                        amount: if amount.is_empty() {
                            ingredient_display(ingredient)
                        } else {
                            amount.to_string()
                        },
                    });
                }
            }
            if index > 0 {
                step.input_steps.push(index - 1);
            }
        }
        let used: HashSet<usize> = recipe
            .steps
            .iter()
            .flat_map(|step| step.ingredient_uses.iter().map(|use_| use_.ingredient))
            .collect();
        if let Some(last) = recipe.steps.last_mut() {
            for (index, ingredient) in recipe.ingredients.iter().enumerate() {
                if !used.contains(&index) {
                    last.ingredient_uses.push(IngredientUse {
                        ingredient: index,
                        amount: ingredient_display(ingredient),
                    });
                }
            }
        }
    }
    for step in &mut recipe.steps {
        step.ingredients = step
            .ingredient_uses
            .iter()
            .filter_map(|use_| {
                recipe.ingredients.get(use_.ingredient).map(|ingredient| {
                    format!("{} {}", use_.amount.trim(), ingredient.name)
                        .trim()
                        .to_string()
                })
            })
            .collect();
    }
    validate_generated(recipe)
}

fn ingredient_display(ingredient: &Ingredient) -> String {
    let amount = format!("{} {}", ingredient.quantity, ingredient.unit)
        .trim()
        .to_string();
    if amount.is_empty() {
        "as needed".into()
    } else {
        amount
    }
}

pub(crate) fn validate_generated(recipe: &GeneratedRecipe) -> Result<()> {
    if recipe.title.trim().is_empty()
        || recipe.ingredients.is_empty()
        || recipe.steps.len() < 2
        || recipe.prep_minutes < 0
        || recipe.cook_minutes < 0
        || recipe.servings < 0
    {
        warn!(
            title_empty = recipe.title.trim().is_empty(),
            ingredient_count = recipe.ingredients.len(),
            step_count = recipe.steps.len(),
            prep_minutes = recipe.prep_minutes,
            cook_minutes = recipe.cook_minutes,
            servings = recipe.servings,
            "AI recipe failed basic content checks"
        );
        return Err(AppError::Ai);
    }
    let mut used_ingredients = HashSet::new();
    let mut consumers = vec![0usize; recipe.steps.len()];
    for (index, step) in recipe.steps.iter().enumerate() {
        if step.text.trim().is_empty()
            || step.chart_label.trim().is_empty()
            || step.timer_seconds < 0
        {
            warn!(step = index, "AI recipe step failed content checks");
            return Err(AppError::Ai);
        }
        let mut local_ingredients = HashSet::new();
        let mut local_inputs = HashSet::new();
        for use_ in &step.ingredient_uses {
            if use_.ingredient >= recipe.ingredients.len()
                || use_.amount.trim().is_empty()
                || !local_ingredients.insert(use_.ingredient)
            {
                warn!(
                    step = index,
                    ingredient = use_.ingredient,
                    "AI recipe step ingredient use is invalid"
                );
                return Err(AppError::Ai);
            }
            used_ingredients.insert(use_.ingredient);
        }
        for &input in &step.input_steps {
            if input >= index || !local_inputs.insert(input) {
                warn!(
                    step = index,
                    input = input,
                    "AI recipe step input reference is invalid"
                );
                return Err(AppError::Ai);
            }
            consumers[input] += 1;
        }
    }
    if used_ingredients.len() != recipe.ingredients.len() {
        warn!(
            used = used_ingredients.len(),
            total = recipe.ingredients.len(),
            "AI recipe did not allocate every ingredient"
        );
        return Err(AppError::Ai);
    }
    if consumers
        .iter()
        .take(recipe.steps.len() - 1)
        .any(|count| *count != 1)
        || consumers[recipe.steps.len() - 1] != 0
    {
        warn!(consumers = ?consumers, "AI recipe step graph has invalid consumers");
        return Err(AppError::Ai);
    }
    Ok(())
}

pub(crate) fn dedupe_sources(sources: Vec<Source>) -> Vec<Source> {
    let mut seen = HashSet::new();
    sources
        .into_iter()
        .filter(|source| {
            (source.url.starts_with("https://") || source.url.starts_with("http://"))
                && seen.insert(source.url.clone())
        })
        .collect()
}
