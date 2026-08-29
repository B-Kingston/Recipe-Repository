use crate::chart::build_chart;
use crate::thumbs::{self, ThumbCandidate};
use crate::{
    AddRecipeQuery, AddRecipeTemplate, AppError, AppState, AuthUser, Block, BlockForm,
    DeleteTemplate, Draft, HomeTemplate, Recipe, RecipeForm, RecipeQuery, RecipeTemplate, Result,
    Source, ViewBlock, ViewStep, generate_guidance, number, option_number, render, required, stamp,
    trim,
};
use axum::{
    Form,
    extract::{Multipart, Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::{FromRow, SqlitePool};
use std::{collections::HashSet, os::unix::fs::DirBuilderExt, sync::Arc};
use uuid::Uuid;

pub(crate) async fn home(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Html<String>> {
    render(HomeTemplate {
        recipes: sqlx::query_as(
            "SELECT id,title,description,thumbnail_jpeg IS NOT NULL AS has_thumbnail
             FROM recipes WHERE user_id=? ORDER BY updated_at DESC",
        )
        .bind(&user.id)
        .fetch_all(&state.db)
        .await?,
    })
}

/// One library-card row: the fields home.html renders plus whether a dish
/// photo exists. The listing query never loads the JPEG bytes themselves.
#[derive(FromRow)]
pub(crate) struct LibraryCard {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) has_thumbnail: bool,
}

/// Streams a recipe's chosen dish photo to its owning user only.
pub(crate) async fn recipe_thumbnail(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    let jpeg = find_recipe_thumbnail(&state.db, &user.id, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(crate::image_response(jpeg))
}

pub(crate) async fn new_recipe(
    State(state): State<Arc<AppState>>,
    Query(query): Query<AddRecipeQuery>,
) -> Result<Html<String>> {
    render(AddRecipeTemplate {
        prompt_error: String::new(),
        guidance: generate_guidance(state.search_grounding).into(),
        prompt: String::new(),
        video_error: String::new(),
        url: String::new(),
        notes: String::new(),
        use_description: true,
        use_audio: true,
        use_ocr: true,
        video_mode: query.mode.as_deref() == Some("video"),
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
    let has_photo = sqlx::query_scalar::<_, i64>(
        "SELECT thumbnail_jpeg IS NOT NULL FROM recipes WHERE id=? AND user_id=?",
    )
    .bind(&id)
    .bind(&user.id)
    .fetch_one(&state.db)
    .await?
        == 1;
    let pending_frames = state.thumbnail_jobs.lock().contains(&id);
    let thumb_options: Vec<crate::DraftThumbView> =
        find_recipe_thumbnails(&state.db, &user.id, &id)
            .await?
            .into_iter()
            .map(|row| crate::DraftThumbView {
                index: row.idx.max(0) as usize,
                seconds: row.seconds.max(0) as u64,
            })
            .collect();
    let thumb_state = if pending_frames {
        "pending"
    } else if !thumb_options.is_empty() {
        "choosing"
    } else {
        "idle"
    };
    let recipe_sources = sources(&state.db, &user.id, &id).await?;
    let can_pick_video_frames = thumbs::enabled() && social_source_url(&recipe_sources).is_some();
    render(RecipeTemplate {
        servings_value: option_number(recipe.servings),
        prep_value: option_number(recipe.prep_minutes),
        cook_value: option_number(recipe.cook_minutes),
        recipe,
        ingredients,
        steps,
        sources: recipe_sources,
        can_pick_video_frames,
        chart,
        chart_view,
        edit_meta: query.edit_meta.is_some(),
        has_photo,
        thumb_state: thumb_state.to_string(),
        thumb_options,
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

/// One stored frame candidate row (draft or recipe scope); `idx` orders the
/// candidates by quality and the preview offers every row for selection.
#[derive(Debug, Clone, FromRow)]
pub(crate) struct ThumbRow {
    pub(crate) idx: i64,
    pub(crate) seconds: i64,
}

pub(crate) async fn store_draft_thumbnails(
    db: &SqlitePool,
    draft_id: &str,
    candidates: &[ThumbCandidate],
) -> Result<()> {
    for (idx, candidate) in candidates.iter().enumerate() {
        sqlx::query(
            "INSERT OR REPLACE INTO draft_thumbnails(draft_id,idx,seconds,jpeg)VALUES(?,?,?,?)",
        )
        .bind(draft_id)
        .bind(idx as i64)
        .bind(candidate.seconds as i64)
        .bind(&candidate.jpeg)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Carries candidates over to an altered draft so the picker survives the
/// alteration chain until the recipe is applied.
pub(crate) async fn copy_draft_thumbnails(
    db: &SqlitePool,
    from_draft_id: &str,
    to_draft_id: &str,
) -> Result<()> {
    sqlx::query("INSERT OR REPLACE INTO draft_thumbnails(draft_id,idx,seconds,jpeg) SELECT ?,idx,seconds,jpeg FROM draft_thumbnails WHERE draft_id=?")
        .bind(to_draft_id)
        .bind(from_draft_id)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn delete_draft_thumbnails(db: &SqlitePool, draft_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM draft_thumbnails WHERE draft_id=?")
        .bind(draft_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Runs wherever expired drafts are pruned so candidate JPEG bytes never
/// outlive the draft they belong to.
pub(crate) async fn delete_expired_draft_thumbnails(db: &SqlitePool, now: &str) -> Result<()> {
    sqlx::query("DELETE FROM draft_thumbnails WHERE draft_id IN (SELECT id FROM ai_drafts WHERE expires_at < ?)")
        .bind(now)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn find_draft_thumbnails(
    db: &SqlitePool,
    user_id: &str,
    draft_id: &str,
) -> Result<Vec<ThumbRow>> {
    Ok(sqlx::query_as(
        "SELECT t.idx,t.seconds FROM draft_thumbnails t
         JOIN ai_drafts d ON d.id=t.draft_id
         WHERE d.id=? AND d.user_id=? AND d.expires_at >= ?
         ORDER BY t.idx",
    )
    .bind(draft_id)
    .bind(user_id)
    .bind(Utc::now().to_rfc3339())
    .fetch_all(db)
    .await?)
}

pub(crate) async fn find_draft_thumbnail_jpeg(
    db: &SqlitePool,
    user_id: &str,
    draft_id: &str,
    idx: i64,
) -> Result<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar(
        "SELECT t.jpeg FROM draft_thumbnails t
         JOIN ai_drafts d ON d.id=t.draft_id
         WHERE d.id=? AND d.user_id=? AND d.expires_at >= ? AND t.idx=?",
    )
    .bind(draft_id)
    .bind(user_id)
    .bind(Utc::now().to_rfc3339())
    .bind(idx)
    .fetch_optional(db)
    .await?)
}

pub(crate) async fn find_recipe_thumbnail(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Result<Option<Vec<u8>>> {
    Ok(
        sqlx::query_scalar("SELECT thumbnail_jpeg FROM recipes WHERE id=? AND user_id=?")
            .bind(recipe_id)
            .bind(user_id)
            .fetch_optional(db)
            .await?,
    )
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

/// First source URL that is a supported social-video post, if any. This is
/// both the "came from a video" signal and the fetch target for re-picking
/// frames.
pub(crate) fn social_source_url(sources: &[Source]) -> Option<String> {
    sources
        .iter()
        .filter_map(|source| crate::media::canonical_social_url(&source.url).ok())
        .next()
}

fn redirect_recipe(recipe_id: &str) -> Response {
    Redirect::to(&format!("/recipes/{recipe_id}")).into_response()
}

pub(crate) async fn store_recipe_thumbnails(
    db: &SqlitePool,
    recipe_id: &str,
    candidates: &[ThumbCandidate],
) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM recipe_thumbnail_candidates WHERE recipe_id=?")
        .bind(recipe_id)
        .execute(&mut *tx)
        .await?;
    for (idx, candidate) in candidates.iter().enumerate() {
        sqlx::query(
            "INSERT INTO recipe_thumbnail_candidates(recipe_id,idx,seconds,jpeg)VALUES(?,?,?,?)",
        )
        .bind(recipe_id)
        .bind(idx as i64)
        .bind(candidate.seconds as i64)
        .bind(&candidate.jpeg)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn delete_recipe_thumbnails(db: &SqlitePool, recipe_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM recipe_thumbnail_candidates WHERE recipe_id=?")
        .bind(recipe_id)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn find_recipe_thumbnails(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Result<Vec<ThumbRow>> {
    Ok(sqlx::query_as(
        "SELECT c.idx,c.seconds FROM recipe_thumbnail_candidates c
         JOIN recipes r ON r.id=c.recipe_id
         WHERE r.id=? AND r.user_id=?
         ORDER BY c.idx",
    )
    .bind(recipe_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

pub(crate) async fn find_recipe_thumbnail_jpeg(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    idx: i64,
) -> Result<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar(
        "SELECT c.jpeg FROM recipe_thumbnail_candidates c
         JOIN recipes r ON r.id=c.recipe_id
         WHERE r.id=? AND r.user_id=? AND c.idx=?",
    )
    .bind(recipe_id)
    .bind(user_id)
    .bind(idx)
    .fetch_optional(db)
    .await?)
}

/// Serves one pending frame candidate for a saved recipe.
pub(crate) async fn candidate_thumbnail(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, idx)): Path<(String, usize)>,
) -> Result<Response> {
    let jpeg = find_recipe_thumbnail_jpeg(&state.db, &user.id, &id, idx as i64)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(crate::image_response(jpeg))
}

/// Stores an uploaded photo as the dish photo. The bytes are normalised to a
/// square 600 px JPEG through the local ffmpeg install; when ffmpeg is not
/// installed but the upload already is a JPEG, it is kept as-is.
pub(crate) async fn upload_thumbnail(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<Response> {
    find_recipe(&state.db, &user.id, &id).await?;
    let mut uploaded: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::BadRequest("The uploaded image could not be read.".into()))?
    {
        if field.name() == Some("image") {
            let bytes = field
                .bytes()
                .await
                .map_err(|_| AppError::BadRequest("Images are limited to 8 MiB.".into()))?;
            uploaded = Some(bytes.to_vec());
            break;
        }
    }
    let raw = uploaded
        .filter(|bytes| !bytes.is_empty())
        .ok_or_else(|| AppError::BadRequest("Choose an image file to upload.".into()))?;
    let jpeg = normalize_upload_jpeg(&raw).await?;
    // A manual upload replaces the photo and any pending frame picks.
    delete_recipe_thumbnails(&state.db, &id).await?;
    sqlx::query("UPDATE recipes SET thumbnail_jpeg=? WHERE id=? AND user_id=?")
        .bind(&jpeg)
        .bind(&id)
        .bind(&user.id)
        .execute(&state.db)
        .await?;
    Ok(redirect_recipe(&id))
}

async fn normalize_upload_jpeg(raw: &[u8]) -> Result<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("kindle-recipes-upload-{}", Uuid::new_v4()));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| AppError::Internal(format!("Could not stage the upload: {error}")))?;
    let source = dir.join("in.img");
    let output = dir.join("out.jpg");
    tokio::fs::write(&source, raw)
        .await
        .map_err(|error| AppError::Internal(format!("Could not stage the upload: {error}")))?;
    let args = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        source.to_string_lossy().to_string(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        "scale=600:600:force_original_aspect_ratio=increase,crop=600:600".into(),
        "-q:v".into(),
        "3".into(),
        output.to_string_lossy().to_string(),
    ];
    let converted = tokio::task::spawn_blocking(move || {
        crate::media::run_tool(
            &crate::media::env_path("MEDIA_FFMPEG_PATH", "ffmpeg"),
            &args,
            std::time::Duration::from_secs(60),
            64 * 1024,
        )
    })
    .await
    .map_err(|_| AppError::Internal("The image converter stopped unexpectedly.".into()))?;
    let normalized = match converted {
        Ok(_) => tokio::fs::read(&output)
            .await
            .ok()
            .filter(|bytes| bytes.len() > 2),
        Err(_) => None,
    };
    let _ = tokio::fs::remove_dir_all(&dir).await;
    if let Some(jpeg) = normalized {
        return Ok(jpeg);
    }
    if looks_like_jpeg(raw) {
        return Ok(raw.to_vec());
    }
    Err(AppError::BadRequest(
        "That file is not an image this server can process.".into(),
    ))
}

fn looks_like_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
}

/// Copies the chosen frame candidate onto the recipe and clears the set.
pub(crate) async fn choose_thumbnail(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<crate::ThumbnailPickForm>,
) -> Result<Response> {
    let idx: i64 =
        form.choice.trim().parse().map_err(|_| {
            AppError::BadRequest("The selected dish photo was not recognised.".into())
        })?;
    let jpeg = find_recipe_thumbnail_jpeg(&state.db, &user.id, &id, idx)
        .await?
        .ok_or(AppError::NotFound)?;
    sqlx::query("UPDATE recipes SET thumbnail_jpeg=? WHERE id=? AND user_id=?")
        .bind(&jpeg)
        .bind(&id)
        .bind(&user.id)
        .execute(&state.db)
        .await?;
    delete_recipe_thumbnails(&state.db, &id).await?;
    Ok(redirect_recipe(&id))
}

/// Drops pending frame candidates without touching the current photo.
pub(crate) async fn discard_thumbnail(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    find_recipe(&state.db, &user.id, &id).await?;
    delete_recipe_thumbnails(&state.db, &id).await?;
    Ok(redirect_recipe(&id))
}

/// Clears the photo and any pending frame picks.
pub(crate) async fn remove_thumbnail(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    find_recipe(&state.db, &user.id, &id).await?;
    delete_recipe_thumbnails(&state.db, &id).await?;
    sqlx::query("UPDATE recipes SET thumbnail_jpeg=NULL WHERE id=? AND user_id=?")
        .bind(&id)
        .bind(&user.id)
        .execute(&state.db)
        .await?;
    Ok(redirect_recipe(&id))
}

/// Re-runs only the frame-sampling half of the video pipeline for this
/// recipe's social source. Runs in the background; the page polls itself
/// with a meta refresh until the candidate set appears.
pub(crate) async fn start_thumbnail_recompute(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    find_recipe(&state.db, &user.id, &id).await?;
    if !thumbs::enabled() {
        return Err(AppError::BadRequest(
            "Dish-photo frame picking is disabled on this server.".into(),
        ));
    }
    let recipe_sources = sources(&state.db, &user.id, &id).await?;
    let video_url = social_source_url(&recipe_sources).ok_or_else(|| {
        AppError::BadRequest(
            "This recipe has no social-video source to re-pick frames from.".into(),
        )
    })?;
    if state.thumbnail_jobs.lock().contains(&id) {
        return Ok(redirect_recipe(&id));
    }
    state.thumbnail_jobs.lock().insert(id.clone());
    let db = state.db.clone();
    let jobs = state.thumbnail_jobs.clone();
    let job_recipe_id = id.clone();
    tokio::spawn(async move {
        let outcome = crate::media::extract_fresh_thumbnail_candidates(&video_url).await;
        if let Ok(candidates) = outcome {
            let _ = store_recipe_thumbnails(&db, &job_recipe_id, &candidates).await;
        }
        // A failed or empty run simply returns the page to its idle state.
        jobs.lock().remove(&job_recipe_id);
    });
    Ok(redirect_recipe(&id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_sniffing_accepts_only_soi_prefixed_bytes() {
        assert!(looks_like_jpeg(&[0xFF, 0xD8, 0xFF, 0xE0, 1, 2]));
        assert!(!looks_like_jpeg(&[0x89, b'P', b'N', b'G']));
        assert!(!looks_like_jpeg(&[0xFF, 0xD8]));
        assert!(!looks_like_jpeg(b""));
    }

    #[test]
    fn social_sources_are_detected_while_other_urls_are_ignored() {
        let social = Source {
            id: None,
            recipe_id: None,
            position: None,
            title: "Reel".into(),
            url: "https://www.instagram.com/reel/AbCdEf123/".into(),
        };
        let blog = Source {
            url: "https://example.com/recipe".into(),
            ..social.clone()
        };
        assert!(social_source_url(std::slice::from_ref(&social)).is_some());
        assert!(social_source_url(std::slice::from_ref(&blog)).is_none());
        assert_eq!(
            social_source_url(&[blog, social]),
            Some("https://www.instagram.com/reel/AbCdEf123/".into())
        );
    }
}
