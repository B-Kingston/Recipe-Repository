use crate::recipes::{find_draft, find_recipe, recipe_snapshot};
use crate::{
    AppError, AppState, ChartRecipe, DRAFT_HOURS, DraftTemplate, GROUNDED_RECIPE_PROMPT,
    GeneratedRecipe, Ingredient, IngredientUse, PromptForm, RECIPE_PROMPT, Recipe, Result, Source,
    generate_guidance, render, required, stamp,
};
use axum::{
    Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};
use tracing::error;
use uuid::Uuid;

pub(crate) async fn generate_page() -> Redirect {
    Redirect::to("/recipes/new")
}

pub(crate) async fn generate_recipe(
    State(s): State<Arc<AppState>>,
    Form(f): Form<PromptForm>,
) -> Result<Response> {
    let prompt = required(&f.prompt, "Recipe idea or URL")?;
    match create_draft(&s, None, "generate", &prompt).await {
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
    Path(id): Path<String>,
) -> Result<Html<String>> {
    let recipe = find_recipe(&s.db, &id).await?;
    render(crate::AiFormTemplate {
        heading: "Alter with AI".into(),
        guidance: format!(
            "Tell Gemini how to change “{}”. It will return a complete replacement recipe for review.",
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
    Path(id): Path<String>,
    Form(f): Form<PromptForm>,
) -> Result<Response> {
    let recipe = find_recipe(&s.db, &id).await?;
    let prompt = required(&f.prompt, "Comments")?;
    let snapshot = recipe_snapshot(&s.db, &recipe).await?;
    let full = format!(
        "User requested changes:\n{}\n\nCurrent recipe JSON:\n{}",
        prompt,
        serde_json::to_string(&snapshot).map_err(|_| AppError::Ai)?
    );
    match create_draft(&s, Some(&recipe), "alter", &full).await {
        Ok(draft) => Ok(Redirect::to(&format!("/ai/drafts/{draft}")).into_response()),
        Err(e) => render(crate::AiFormTemplate {
            heading: "Alter with AI".into(),
            guidance: "Tell Gemini what should change.".into(),
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
    Path(id): Path<String>,
) -> Result<Html<String>> {
    let draft = find_draft(&s.db, &id).await?;
    let mut recipe: GeneratedRecipe =
        serde_json::from_str(&draft.recipe_json).map_err(|_| AppError::Ai)?;
    normalize_generated(&mut recipe)?;
    render(DraftTemplate {
        id,
        recipe,
        sources: serde_json::from_str(&draft.sources_json).map_err(|_| AppError::Ai)?,
        suggestions: draft.search_suggestions,
    })
}

pub(crate) async fn apply_draft(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response> {
    let draft = find_draft(&s.db, &id).await?;
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
            != Some(&find_recipe(&s.db, &existing).await?.updated_at)
        {
            return Err(AppError::BadRequest(
                "This recipe changed after the draft was made. Generate a new alteration.".into(),
            ));
        }
        sqlx::query("UPDATE recipes SET title=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,chart_json=?,updated_at=? WHERE id=?")
            .bind(&recipe.title)
            .bind(&recipe.description)
            .bind(recipe.servings)
            .bind(recipe.prep_minutes)
            .bind(recipe.cook_minutes)
            .bind(&chart_json)
            .bind(&now)
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
        sqlx::query("INSERT INTO recipes(id,title,description,servings,prep_minutes,cook_minutes,chart_json,created_at,updated_at)VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(&recipe_id)
            .bind(&recipe.title)
            .bind(&recipe.description)
            .bind(recipe.servings)
            .bind(recipe.prep_minutes)
            .bind(recipe.cook_minutes)
            .bind(&chart_json)
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
    sqlx::query("DELETE FROM ai_drafts WHERE id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{recipe_id}")).into_response())
}

pub(crate) async fn cancel_draft(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response> {
    sqlx::query("DELETE FROM ai_drafts WHERE id=?")
        .bind(id)
        .execute(&s.db)
        .await?;
    Ok(Redirect::to("/").into_response())
}

async fn create_draft(
    state: &AppState,
    base_recipe: Option<&Recipe>,
    operation: &str,
    prompt: &str,
) -> Result<String> {
    if state.api_key.is_empty() {
        return Err(AppError::AiNotConfigured);
    }
    let (generated_recipe, sources, suggestions) = gemini(state, prompt).await?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    sqlx::query("DELETE FROM ai_drafts WHERE expires_at < ?")
        .bind(now.to_rfc3339())
        .execute(&state.db)
        .await?;
    sqlx::query("INSERT INTO ai_drafts(id,recipe_id,operation,recipe_json,sources_json,search_suggestions,base_updated_at,created_at,expires_at)VALUES(?,?,?,?,?,?,?,?,?)")
        .bind(&id)
        .bind(base_recipe.map(|recipe| recipe.id.as_str()))
        .bind(operation)
        .bind(serde_json::to_string(&generated_recipe).map_err(|_| AppError::Ai)?)
        .bind(serde_json::to_string(&sources).map_err(|_| AppError::Ai)?)
        .bind(suggestions)
        .bind(base_recipe.map(|recipe| recipe.updated_at.as_str()))
        .bind(now.to_rfc3339())
        .bind((now + Duration::hours(DRAFT_HOURS)).to_rfc3339())
        .execute(&state.db)
        .await?;
    Ok(id)
}

async fn gemini(state: &AppState, prompt: &str) -> Result<(GeneratedRecipe, Vec<Source>, String)> {
    for attempt in 0..2 {
        let input = if attempt == 0 {
            prompt.to_string()
        } else if state.search_grounding {
            format!(
                "{prompt}\n\nImportant: research this thoroughly before answering. Use Google Search, read any supplied URLs with URL Context, and return a complete recipe whose output has URL citations."
            )
        } else {
            format!(
                "{prompt}\n\nImportant: return a complete recipe as valid JSON matching the requested schema."
            )
        };
        let mut body = json!({"model":state.model,"input":input,"system_instruction":if state.search_grounding { GROUNDED_RECIPE_PROMPT } else { RECIPE_PROMPT },"store":false,"response_format":{"type":"text","mime_type":"application/json","schema":recipe_schema()}});
        if state.search_grounding {
            body["tools"] = json!([{"type":"google_search"},{"type":"url_context"}]);
        }
        let response = state
            .http
            .post(&state.gemini_base_url)
            .header("x-goog-api-key", &state.api_key)
            .header("Api-Revision", "2026-05-20")
            .json(&body)
            .send()
            .await
            .map_err(|_| AppError::Ai)?;
        if !response.status().is_success() {
            error!(status=%response.status(), "Gemini request failed");
            return Err(AppError::Ai);
        }
        let value: Value = response.json().await.map_err(|_| AppError::Ai)?;
        if let Ok(result) = parse_response(&value, state.search_grounding) {
            return Ok(result);
        }
    }
    Err(AppError::Ai)
}

pub(crate) fn parse_response(
    value: &Value,
    require_grounding: bool,
) -> Result<(GeneratedRecipe, Vec<Source>, String)> {
    let researched = value["steps"].as_array().is_some_and(|steps| {
        steps.iter().any(|step| {
            matches!(
                step["type"].as_str(),
                Some("google_search_call" | "url_context_call")
            )
        })
    });
    let mut text = String::new();
    let mut citations = Vec::new();
    let mut suggestions = String::new();
    if let Some(steps) = value["steps"].as_array() {
        for step in steps {
            if step["type"].as_str() == Some("google_search_result") {
                if let Some(results) = step["result"].as_array() {
                    for result in results {
                        if suggestions.is_empty() {
                            suggestions = result["search_suggestions"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string();
                        }
                    }
                }
            }
            if step["type"].as_str() != Some("model_output") {
                continue;
            }
            let Some(content) = step["content"].as_array() else {
                continue;
            };
            for block in content {
                if block["type"].as_str() != Some("text") {
                    continue;
                }
                text.push_str(block["text"].as_str().unwrap_or_default());
                if let Some(annotations) = block["annotations"].as_array() {
                    for annotation in annotations {
                        if annotation["type"].as_str() != Some("url_citation") {
                            continue;
                        }
                        let url = annotation["url"].as_str().unwrap_or_default();
                        if !url.is_empty() {
                            citations.push(Source {
                                id: None,
                                recipe_id: None,
                                position: None,
                                title: annotation["title"].as_str().unwrap_or_default().to_string(),
                                url: url.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }
    let citations = dedupe_sources(citations);
    if require_grounding && (!researched || citations.is_empty()) {
        return Err(AppError::Ai);
    }
    let mut recipe: GeneratedRecipe = serde_json::from_str(&text).map_err(|_| AppError::Ai)?;
    normalize_generated(&mut recipe)?;
    Ok((recipe, citations, suggestions))
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
        return Err(AppError::Ai);
    }
    let mut used_ingredients = HashSet::new();
    let mut consumers = vec![0usize; recipe.steps.len()];
    for (index, step) in recipe.steps.iter().enumerate() {
        if step.text.trim().is_empty()
            || step.chart_label.trim().is_empty()
            || step.timer_seconds < 0
        {
            return Err(AppError::Ai);
        }
        let mut local_ingredients = HashSet::new();
        let mut local_inputs = HashSet::new();
        for use_ in &step.ingredient_uses {
            if use_.ingredient >= recipe.ingredients.len()
                || use_.amount.trim().is_empty()
                || !local_ingredients.insert(use_.ingredient)
            {
                return Err(AppError::Ai);
            }
            used_ingredients.insert(use_.ingredient);
        }
        for &input in &step.input_steps {
            if input >= index || !local_inputs.insert(input) {
                return Err(AppError::Ai);
            }
            consumers[input] += 1;
        }
    }
    if used_ingredients.len() != recipe.ingredients.len()
        || consumers
            .iter()
            .take(recipe.steps.len() - 1)
            .any(|count| *count != 1)
        || consumers[recipe.steps.len() - 1] != 0
    {
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
