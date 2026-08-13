use crate::chart::build_chart;
use crate::{
    AiFormTemplate, AppError, AppState, AuthUser, Block, BlockForm, DeleteTemplate, Draft,
    HomeTemplate, Recipe, RecipeForm, RecipeQuery, RecipeTemplate, Result, Source, ViewBlock,
    ViewStep, generate_guidance, number, option_number, render, required, stamp, trim,
};
use axum::{
    Form,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::{collections::HashSet, sync::Arc};
use uuid::Uuid;

pub(crate) async fn home(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Html<String>> {
    render(HomeTemplate {
        recipes: sqlx::query_as("SELECT * FROM recipes WHERE user_id=? ORDER BY updated_at DESC")
            .bind(&user.id)
            .fetch_all(&state.db)
            .await?,
    })
}

pub(crate) async fn new_recipe(State(state): State<Arc<AppState>>) -> Result<Html<String>> {
    render(AiFormTemplate {
        heading: "New Recipe".into(),
        guidance: generate_guidance(state.search_grounding).into(),
        action: "/ai/generate".into(),
        label: "What should this recipe be based on?".into(),
        button: "Research & generate".into(),
        cancel_url: "/".into(),
        error: String::new(),
        prompt: String::new(),
        pairwise_critique: false,
    })
}

pub(crate) async fn recipe_page(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Query(query): Query<RecipeQuery>,
) -> Result<Html<String>> {
    let recipe = find_recipe(&state.db, &user.id, &id).await?;
    let blocks = blocks(&state.db, &user.id, &id).await?;
    let edit = query.edit.as_deref();
    let ingredient_blocks: Vec<Block> = blocks
        .iter()
        .filter(|block| block.section == "ingredient")
        .cloned()
        .collect();
    let ingredients = ingredient_blocks
        .iter()
        .cloned()
        .map(|block| ViewBlock::from_block(block, edit))
        .collect();
    let mut steps = Vec::new();
    for block in blocks
        .iter()
        .filter(|block| block.section == "step")
        .cloned()
    {
        let mut used = step_ingredients(&state.db, &block.id).await?;
        if used.is_empty() {
            used = infer_step_ingredients(&block.text, &ingredient_blocks);
        }
        steps.push(ViewStep {
            block: ViewBlock::from_block(block, edit),
            ingredients_text: used.join("\n"),
            ingredients: used,
        });
    }
    let chart_view = query.view.as_deref() == Some("chart");
    let chart = build_chart(&recipe, &ingredient_blocks, &steps, query.step);
    render(RecipeTemplate {
        servings_value: option_number(recipe.servings),
        prep_value: option_number(recipe.prep_minutes),
        cook_value: option_number(recipe.cook_minutes),
        recipe,
        ingredients,
        steps,
        sources: sources(&state.db, &user.id, &id).await?,
        chart,
        chart_view,
        edit_meta: query.edit_meta.is_some(),
    })
}

pub(crate) async fn update_recipe(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<RecipeForm>,
) -> Result<Response> {
    find_recipe(&state.db, &user.id, &id).await?;
    sqlx::query("UPDATE recipes SET title=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,updated_at=? WHERE user_id=? AND id=?")
        .bind(required(&form.title, "Recipe title")?)
        .bind(trim(&form.description))
        .bind(number(&form.servings)?)
        .bind(number(&form.prep_minutes)?)
        .bind(number(&form.cook_minutes)?)
        .bind(stamp())
        .bind(&user.id)
        .bind(&id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}

pub(crate) async fn delete_page(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    render(DeleteTemplate {
        recipe: find_recipe(&state.db, &user.id, &id).await?,
    })
}

pub(crate) async fn delete_recipe(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    let result = sqlx::query("DELETE FROM recipes WHERE user_id=? AND id=?")
        .bind(&user.id)
        .bind(id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to("/").into_response())
}

pub(crate) async fn add_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<BlockForm>,
) -> Result<Response> {
    find_recipe(&state.db, &user.id, &id).await?;
    let section = form
        .section
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("Block section is required.".into()))?;
    if !matches!(section, "ingredient" | "step") {
        return Err(AppError::BadRequest("Invalid block section.".into()));
    }
    let text = required(&form.text, "Block text")?;
    let mut tx = state.db.begin().await?;
    let position: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position)+1,0) FROM recipe_blocks WHERE recipe_id=? AND section=?",
    )
    .bind(&id)
    .bind(section)
    .fetch_one(&mut *tx)
    .await?;
    let block_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit,optional) VALUES(?,?,?,?,?,?,?,?)")
        .bind(&block_id)
        .bind(&id)
        .bind(section)
        .bind(position)
        .bind(text)
        .bind(trim(form.quantity.as_deref().unwrap_or("")))
        .bind(trim(form.unit.as_deref().unwrap_or("")))
        .bind(if form.optional.is_some() { 1 } else { 0 })
        .execute(&mut *tx)
        .await?;
    if section == "step" {
        replace_step_ingredients(
            &mut tx,
            &block_id,
            form.step_ingredients.as_deref().unwrap_or(""),
        )
        .await?;
    }
    invalidate_chart(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}

pub(crate) async fn update_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, block_id)): Path<(String, String)>,
    Form(form): Form<BlockForm>,
) -> Result<Response> {
    let block = find_block(&state.db, &user.id, &id, &block_id).await?;
    let text = required(&form.text, "Block text")?;
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE recipe_blocks SET text=?,quantity=?,unit=?,optional=? WHERE id=? AND recipe_id=?",
    )
    .bind(text)
    .bind(if block.section == "ingredient" {
        trim(form.quantity.as_deref().unwrap_or(""))
    } else {
        String::new()
    })
    .bind(if block.section == "ingredient" {
        trim(form.unit.as_deref().unwrap_or(""))
    } else {
        String::new()
    })
    .bind(if form.optional.is_some() { 1 } else { 0 })
    .bind(&block_id)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if block.section == "step" {
        replace_step_ingredients(
            &mut tx,
            &block_id,
            form.step_ingredients.as_deref().unwrap_or(""),
        )
        .await?;
    }
    invalidate_chart(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}

pub(crate) async fn move_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, block_id, direction)): Path<(String, String, String)>,
) -> Result<Response> {
    let block = find_block(&state.db, &user.id, &id, &block_id).await?;
    let delta = match direction.as_str() {
        "up" => -1,
        "down" => 1,
        _ => return Err(AppError::BadRequest("Invalid movement.".into())),
    };
    let target = block.position + delta;
    if target < 0 {
        return Ok(Redirect::to(&format!("/recipes/{id}")).into_response());
    }
    let mut tx = state.db.begin().await?;
    let neighbor: Option<String> = sqlx::query_scalar(
        "SELECT id FROM recipe_blocks WHERE recipe_id=? AND section=? AND position=?",
    )
    .bind(&id)
    .bind(&block.section)
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(other) = neighbor {
        sqlx::query("UPDATE recipe_blocks SET position=-1 WHERE id=?")
            .bind(&block_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE recipe_blocks SET position=? WHERE id=?")
            .bind(block.position)
            .bind(other)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE recipe_blocks SET position=? WHERE id=?")
            .bind(target)
            .bind(&block_id)
            .execute(&mut *tx)
            .await?;
        invalidate_chart(&mut tx, &id).await?;
    }
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}

pub(crate) async fn delete_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, block_id)): Path<(String, String)>,
) -> Result<Response> {
    let block = find_block(&state.db, &user.id, &id, &block_id).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM recipe_blocks WHERE id=?")
        .bind(&block_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE recipe_blocks SET position=position-1 WHERE recipe_id=? AND section=? AND position>?")
        .bind(&id)
        .bind(&block.section)
        .bind(block.position)
        .execute(&mut *tx)
        .await?;
    invalidate_chart(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}

pub(crate) async fn find_recipe(db: &SqlitePool, user_id: &str, id: &str) -> Result<Recipe> {
    sqlx::query_as("SELECT * FROM recipes WHERE user_id=? AND id=?")
        .bind(user_id)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

async fn find_block(db: &SqlitePool, user_id: &str, recipe_id: &str, id: &str) -> Result<Block> {
    sqlx::query_as(
        "SELECT b.* FROM recipe_blocks b
         JOIN recipes r ON r.id=b.recipe_id
         WHERE b.id=? AND b.recipe_id=? AND r.user_id=?",
    )
    .bind(id)
    .bind(recipe_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

async fn blocks(db: &SqlitePool, user_id: &str, id: &str) -> Result<Vec<Block>> {
    Ok(sqlx::query_as(
        "SELECT b.* FROM recipe_blocks b
             JOIN recipes r ON r.id=b.recipe_id
             WHERE b.recipe_id=? AND r.user_id=?
             ORDER BY b.section,b.position",
    )
    .bind(id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

async fn step_ingredients(db: &SqlitePool, step_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT text FROM recipe_step_ingredients WHERE step_id=? ORDER BY position",
    )
    .bind(step_id)
    .fetch_all(db)
    .await?)
}

async fn replace_step_ingredients(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    step_id: &str,
    value: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM recipe_step_ingredients WHERE step_id=?")
        .bind(step_id)
        .execute(&mut **tx)
        .await?;
    for (position, text) in value
        .lines()
        .map(trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        sqlx::query("INSERT INTO recipe_step_ingredients(id,step_id,position,text)VALUES(?,?,?,?)")
            .bind(Uuid::new_v4().to_string())
            .bind(step_id)
            .bind(position as i64)
            .bind(text)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn invalidate_chart(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    recipe_id: &str,
) -> Result<()> {
    sqlx::query("UPDATE recipes SET chart_json='',updated_at=? WHERE id=?")
        .bind(stamp())
        .bind(recipe_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn sources(db: &SqlitePool, user_id: &str, id: &str) -> Result<Vec<Source>> {
    Ok(sqlx::query_as(
        "SELECT s.id,s.recipe_id,s.position,s.title,s.url
         FROM recipe_sources s
         JOIN recipes r ON r.id=s.recipe_id
         WHERE s.recipe_id=? AND r.user_id=?
         ORDER BY s.position",
    )
    .bind(id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

pub(crate) async fn find_draft(db: &SqlitePool, user_id: &str, id: &str) -> Result<Draft> {
    sqlx::query_as("SELECT * FROM ai_drafts WHERE id=? AND user_id=? AND expires_at >= ?")
        .bind(id)
        .bind(user_id)
        .bind(Utc::now().to_rfc3339())
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub(crate) async fn recipe_snapshot(
    db: &SqlitePool,
    user_id: &str,
    recipe: &Recipe,
) -> Result<Value> {
    let blocks = blocks(db, user_id, &recipe.id).await?;
    let mut steps = Vec::new();
    for step in blocks.iter().filter(|block| block.section == "step") {
        steps.push(json!({"text":step.text,"ingredients":step_ingredients(db,&step.id).await?}));
    }
    Ok(
        json!({"title":recipe.title,"description":recipe.description,"servings":recipe.servings,"prepMinutes":recipe.prep_minutes,"cookMinutes":recipe.cook_minutes,"ingredients":blocks.iter().filter(|block|block.section=="ingredient").map(|block|json!({"name":block.text,"quantity":block.quantity,"unit":block.unit,"optional":block.optional()})).collect::<Vec<_>>(),"steps":steps}),
    )
}

pub(crate) fn infer_step_ingredients(step: &str, ingredients: &[Block]) -> Vec<String> {
    let lower = step.to_lowercase();
    let words: HashSet<String> = lower
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    ingredients
        .iter()
        .filter(|ingredient| {
            if ingredient.quantity.is_empty() || ingredient.unit.is_empty() {
                return false;
            }
            let unit = ingredient.unit.to_lowercase();
            let keyword = ingredient
                .text
                .to_lowercase()
                .split(|character: char| !character.is_alphanumeric())
                .rfind(|word| word.len() > 2)
                .unwrap_or_default()
                .to_string();
            lower.contains(&ingredient.quantity.to_lowercase())
                && words.contains(&unit)
                && words.contains(&keyword)
        })
        .map(|ingredient| {
            format!(
                "{} {} {}",
                ingredient.quantity, ingredient.unit, ingredient.text
            )
        })
        .collect()
}
