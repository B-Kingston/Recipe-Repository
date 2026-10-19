use crate::chart::{build_chart, infer_step_ingredients};
use crate::moments::{self, MomentKind, MomentPool, MomentVector, StepMoment};
use crate::search::{self, Indexed, Outcome};
use crate::thumbs::{self, ThumbCandidate};
use crate::{
    AddRecipeQuery, AddRecipeTemplate, AppError, AppState, AuthUser, Block, BlockForm,
    DeleteTemplate, Draft, HomeQuery, HomeTemplate, Recipe, RecipeForm, RecipeQuery,
    RecipeSnapshot, RecipeTemplate, Result, SnapshotIngredient, SnapshotStep, Source,
    VersionTemplate, ViewBlock, ViewStep, display_timestamp, generate_guidance, number,
    option_number, render, required, stamp, trim,
};
use axum::{
    Form,
    extract::{Multipart, Path, Query, State, rejection::FormRejection},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::Utc;
use sqlx::{FromRow, SqliteConnection, SqlitePool};
use std::{collections::HashMap, os::unix::fs::DirBuilderExt, sync::Arc};
use uuid::Uuid;

/// Version label for a block change, e.g. "Step added" or "Ingredient edited".
fn block_label(section: &str, action: &str) -> String {
    let noun = if section == "step" {
        "Step"
    } else {
        "Ingredient"
    };
    format!("{noun} {action}")
}

/// The library page. Without `?q=` it lists every recipe filed A to Z; with
/// it, the recipes ranked by embedding similarity, or the filed list plus a
/// notice when the embedding service is missing or down.
pub(crate) async fn home(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Query(params): Query<HomeQuery>,
) -> Result<Html<String>> {
    let query = search::normalize_query(params.q.as_deref().unwrap_or(""));
    let cards: Vec<LibraryCard> = sqlx::query_as(
        "SELECT id,title,description,servings,prep_minutes,cook_minutes,
                thumbnail_jpeg IS NOT NULL AS has_thumbnail
         FROM recipes WHERE user_id=? ORDER BY updated_at DESC",
    )
    .bind(&user.id)
    .fetch_all(&state.db)
    .await?;
    let (mut recipes, results, notice) = if query.is_empty() {
        (cards, false, String::new())
    } else {
        match search::search(&state, &user.id, &query).await {
            Outcome::Ranked { ids, indexing } => {
                let mut by_id: HashMap<String, LibraryCard> = cards
                    .into_iter()
                    .map(|card| (card.id.clone(), card))
                    .collect();
                let found = ids.iter().filter_map(|id| by_id.remove(id)).collect();
                let notice = if indexing > 0 {
                    search::indexing_note(indexing)
                } else {
                    String::new()
                };
                (found, true, notice)
            }
            Outcome::NotConfigured => (cards, false, search::NOT_CONFIGURED.to_owned()),
            Outcome::Unavailable => (cards, false, search::UNAVAILABLE.to_owned()),
        }
    };
    let letters = if results {
        Vec::new()
    } else {
        file_by_letter(&mut recipes)
    };
    render(HomeTemplate {
        recipes,
        query,
        results,
        notice,
        letters,
    })
}

/// One library-card row: the fields home.html renders plus whether a dish
/// photo exists. The listing query never loads the JPEG bytes themselves.
#[derive(FromRow)]
pub(crate) struct LibraryCard {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) servings: Option<i64>,
    pub(crate) prep_minutes: Option<i64>,
    pub(crate) cook_minutes: Option<i64>,
    pub(crate) has_thumbnail: bool,
    /// The letter tab this card carries: set by `file_by_letter` on the first
    /// card of each letter, empty on every other card and on search results.
    #[sqlx(skip)]
    pub(crate) letter: String,
}

impl LibraryCard {
    /// "Serves 4", or empty when the recipe has no serving count.
    pub(crate) fn serves(&self) -> String {
        match self.servings {
            Some(servings) if servings > 0 => format!("Serves {servings}"),
            _ => String::new(),
        }
    }

    /// Prep plus cook time, such as "35 min" or "1 h 10 min"; empty when
    /// neither is set.
    pub(crate) fn total_time(&self) -> String {
        let minutes = self.prep_minutes.unwrap_or(0).max(0) + self.cook_minutes.unwrap_or(0).max(0);
        match (minutes / 60, minutes % 60) {
            (0, 0) => String::new(),
            (0, rest) => format!("{rest} min"),
            (hours, 0) => format!("{hours} h"),
            (hours, rest) => format!("{hours} h {rest} min"),
        }
    }

    /// The fragment id of this card's letter tab, such as "B" or "other".
    pub(crate) fn anchor(&self) -> String {
        letter_anchor(self.letter.chars().next().unwrap_or('#'))
    }
}

/// One link in the library's A to Z strip.
pub(crate) struct LetterLink {
    pub(crate) letter: char,
    pub(crate) anchor: String,
    /// A card is filed under this letter, so the link has a target.
    pub(crate) present: bool,
}

/// The letter a title is filed under: its first letter, upper-cased, or '#'
/// when the title starts with a digit or a letter outside A to Z.
pub(crate) fn file_letter(title: &str) -> char {
    match title.chars().find(|c| c.is_alphanumeric()) {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => '#',
    }
}

fn letter_anchor(letter: char) -> String {
    if letter == '#' {
        "other".to_owned()
    } else {
        letter.to_string()
    }
}

/// Files the library like a recipe box: sorts the cards by letter, then
/// title, puts a tab on the first card of each letter, and returns the A to Z
/// strip, led by '#' when some titles start with anything else.
pub(crate) fn file_by_letter(cards: &mut [LibraryCard]) -> Vec<LetterLink> {
    cards.sort_by_cached_key(|card| (file_letter(&card.title), card.title.to_lowercase()));
    let mut present = Vec::new();
    for card in cards.iter_mut() {
        let letter = file_letter(&card.title);
        if present.last() != Some(&letter) {
            card.letter = letter.to_string();
            present.push(letter);
        }
    }
    let other = present.first() == Some(&'#');
    other
        .then_some('#')
        .into_iter()
        .chain('A'..='Z')
        .map(|letter| LetterLink {
            letter,
            anchor: letter_anchor(letter),
            present: present.contains(&letter),
        })
        .collect()
}

/// A recipe with its blocks, loaded to build its search document.
pub(crate) struct IndexSource {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) blocks: Vec<Block>,
}

#[derive(FromRow)]
struct IndexSourceRow {
    id: String,
    title: String,
    description: String,
}

#[derive(FromRow)]
struct IndexBlockRow {
    recipe_id: String,
    #[sqlx(flatten)]
    block: Block,
}

/// Every recipe of `user_id` (or of all users when `None`) with all its
/// blocks, newest edit first. Two queries regardless of library size.
pub(crate) async fn index_sources(
    db: &SqlitePool,
    user_id: Option<&str>,
) -> Result<Vec<IndexSource>> {
    let rows: Vec<IndexSourceRow> = sqlx::query_as(
        "SELECT id,title,description FROM recipes
         WHERE (? IS NULL OR user_id=?) ORDER BY updated_at DESC, id",
    )
    .bind(user_id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    let block_rows: Vec<IndexBlockRow> = sqlx::query_as(
        "SELECT recipe_id,id,section,position,text,quantity,unit,optional FROM recipe_blocks
         WHERE recipe_id IN (SELECT id FROM recipes WHERE (? IS NULL OR user_id=?))
         ORDER BY recipe_id, section, position",
    )
    .bind(user_id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    let mut blocks: HashMap<String, Vec<Block>> = HashMap::new();
    for row in block_rows {
        blocks.entry(row.recipe_id).or_default().push(row.block);
    }
    Ok(rows
        .into_iter()
        .map(|row| IndexSource {
            blocks: blocks.remove(&row.id).unwrap_or_default(),
            id: row.id,
            title: row.title,
            description: row.description,
        })
        .collect())
}

/// The embedded document stored for each recipe of `user_id` (or of all
/// users when `None`), keyed by recipe id.
pub(crate) async fn stored_documents(
    db: &SqlitePool,
    user_id: Option<&str>,
) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT e.recipe_id, e.document FROM recipe_embeddings e
         JOIN recipes r ON r.id = e.recipe_id WHERE (? IS NULL OR r.user_id=?)",
    )
    .bind(user_id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Stores (or replaces) a recipe's document vector. A recipe deleted since
/// its document was built is skipped instead of failing the whole refresh.
pub(crate) async fn store_embedding(
    db: &SqlitePool,
    recipe_id: &str,
    document: &str,
    vector: &[f32],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO recipe_embeddings(recipe_id,document,vector,updated_at)
         SELECT ?1,?2,?3,?4 WHERE EXISTS (SELECT 1 FROM recipes WHERE id=?1)
         ON CONFLICT(recipe_id) DO UPDATE SET
           document=excluded.document, vector=excluded.vector, updated_at=excluded.updated_at",
    )
    .bind(recipe_id)
    .bind(document)
    .bind(crate::embeddings::to_blob(vector))
    .bind(stamp())
    .execute(db)
    .await?;
    Ok(())
}

#[derive(FromRow)]
struct IndexedRow {
    id: String,
    title: String,
    vector: Vec<u8>,
}

/// Id, title and vector of each of `user_id`'s indexed recipes; rows whose
/// stored vector is not a full vector are skipped.
pub(crate) async fn indexed_recipes(db: &SqlitePool, user_id: &str) -> Result<Vec<Indexed>> {
    let rows: Vec<IndexedRow> = sqlx::query_as(
        "SELECT r.id, r.title, e.vector FROM recipe_embeddings e
         JOIN recipes r ON r.id = e.recipe_id WHERE r.user_id=?",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Indexed {
                vector: crate::embeddings::from_blob(&row.vector)?,
                id: row.id,
                title: row.title,
            })
        })
        .collect())
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
    user: AuthUser,
    Query(query): Query<AddRecipeQuery>,
) -> Result<Html<String>> {
    render(AddRecipeTemplate {
        prompt_error: String::new(),
        guidance: generate_guidance(state.search_grounding).into(),
        prompt: String::new(),
        video_error: String::new(),
        url: query.url.unwrap_or_default(),
        notes: String::new(),
        use_description: true,
        use_audio: true,
        use_ocr: true,
        video_mode: query.mode.as_deref() == Some("video"),
        gateway_configured: crate::ai_gateway_credential(&state.db, &user.id)
            .await?
            .is_some(),
    })
}

pub(crate) async fn recipe_page(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Query(query): Query<RecipeQuery>,
) -> Result<Html<String>> {
    let recipe = find_recipe(&state.db, &user.id, &id).await?;
    let (ingredient_blocks, step_blocks): (Vec<Block>, Vec<Block>) = blocks(&state.db, &id)
        .await?
        .into_iter()
        .partition(|block| block.section == "ingredient");
    let edit = query.edit.as_deref();
    let ingredients = ingredient_blocks
        .iter()
        .cloned()
        .map(|block| ViewBlock::from_block(block, edit))
        .collect();
    let mut uses = step_ingredients_by_step(&state.db, &id).await?;
    let recipe_sources = sources(&state.db, &user.id, &id).await?;
    let video_url = social_source_url(&recipe_sources);
    let moments_shown = video_url.is_some() && crate::step_moments_enabled(&state.db).await?;
    let mut step_moments = if moments_shown {
        recipe_step_moments(&state.db, &id).await?
    } else {
        HashMap::new()
    };
    let moments_pending = moments_shown && state.moment_jobs.lock().contains(&id);
    let can_find_moments = moments_shown
        && state.embedder.is_some()
        && !moments_pending
        && !recipe_has_moment_pool(&state.db, &id).await?;
    let still_seconds = if step_moments.is_empty() {
        Vec::new()
    } else {
        recipe_still_seconds(&state.db, &id).await?
    };
    let still_base = format!("/recipes/{id}/stills");
    let steps = step_blocks
        .into_iter()
        .map(|block| {
            let mut used = uses.remove(&block.id).unwrap_or_default();
            if used.is_empty() {
                used = infer_step_ingredients(&block.text, &ingredient_blocks);
            }
            let moment = step_moments
                .remove(&block.id)
                .map(|moment| moments::view(&moment, &still_seconds, &still_base));
            ViewStep {
                ingredients_text: used.join("\n"),
                ingredients: used,
                block: ViewBlock::from_block(block, edit),
                moment,
            }
        })
        .collect::<Vec<_>>();
    let chart_view = query.view.as_deref() == Some("chart");
    let chart = build_chart(&recipe, &ingredient_blocks, &steps, query.step);
    let has_photo: bool = sqlx::query_scalar(
        "SELECT thumbnail_jpeg IS NOT NULL FROM recipes WHERE id=? AND user_id=?",
    )
    .bind(&id)
    .bind(&user.id)
    .fetch_one(&state.db)
    .await?;
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
    let can_pick_video_frames = thumbs::enabled() && video_url.is_some();
    let versions = recipe_versions(&state.db, &user.id, &id).await?;
    render(RecipeTemplate {
        servings_value: option_number(recipe.servings),
        prep_value: option_number(recipe.prep_minutes),
        cook_value: option_number(recipe.cook_minutes),
        recipe,
        ingredients,
        steps,
        sources: recipe_sources,
        can_pick_video_frames,
        moments_pending,
        can_find_moments,
        chart,
        chart_view,
        edit_meta: query.edit_meta.is_some(),
        has_photo,
        thumb_state: thumb_state.to_string(),
        thumb_options,
        versions,
    })
}

pub(crate) async fn update_recipe(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<RecipeForm>,
) -> Result<Response> {
    let recipe = find_recipe(&state.db, &user.id, &id).await?;
    let title = required(&form.title, "Recipe title")?;
    let description = trim(&form.description);
    let servings = number(&form.servings)?;
    let prep_minutes = number(&form.prep_minutes)?;
    let cook_minutes = number(&form.cook_minutes)?;
    if recipe.title == title
        && recipe.description == description
        && recipe.servings == servings
        && recipe.prep_minutes == prep_minutes
        && recipe.cook_minutes == cook_minutes
    {
        return Ok(redirect_recipe(&id));
    }
    let mut tx = state.db.begin().await?;
    ensure_recipe_history(&mut tx, &user.id, &id).await?;
    sqlx::query("UPDATE recipes SET title=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,updated_at=? WHERE user_id=? AND id=?")
        .bind(title)
        .bind(description)
        .bind(servings)
        .bind(prep_minutes)
        .bind(cook_minutes)
        .bind(stamp())
        .bind(&user.id)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    record_version(&mut tx, &user.id, &id, "Details edited").await?;
    tx.commit().await?;
    Ok(redirect_recipe(&id))
}

const SNAPSHOT_SCHEMA_VERSION: u8 = 1;

/// Records the pre-change state of a recipe with no history yet, so the first
/// edit keeps the content it replaces. Call before mutating inside the transaction.
pub(crate) async fn ensure_recipe_history(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    recipe_id: &str,
) -> Result<()> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM recipe_versions WHERE recipe_id=?)")
            .bind(recipe_id)
            .fetch_one(&mut **tx)
            .await?;
    if !exists {
        record_version(tx, user_id, recipe_id, "Original recipe").await?;
    }
    Ok(())
}

/// Stores one post-change content snapshot inside the caller's transaction.
pub(crate) async fn record_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    recipe_id: &str,
    label: &str,
) -> Result<()> {
    let snapshot = recipe_snapshot(tx, user_id, recipe_id).await?;
    let version: i64 = sqlx::query_scalar(
        "INSERT INTO recipe_version_counters(recipe_id,last_version) VALUES(?,1)
         ON CONFLICT(recipe_id) DO UPDATE SET last_version=last_version+1
         RETURNING last_version",
    )
    .bind(recipe_id)
    .fetch_one(&mut **tx)
    .await?;
    let snapshot = serde_json::to_string(&snapshot).map_err(|error| {
        AppError::Internal(format!("Could not encode the recipe version: {error}"))
    })?;
    sqlx::query("INSERT INTO recipe_versions(id,recipe_id,version,label,snapshot,created_at)VALUES(?,?,?,?,?,?)")
        .bind(Uuid::new_v4().to_string())
        .bind(recipe_id)
        .bind(version)
        .bind(label)
        .bind(snapshot)
        .bind(stamp())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Version row for the sidebar list; the snapshot text itself stays out of it.
#[derive(Debug, Clone, FromRow)]
pub(crate) struct VersionListRow {
    pub(crate) version: i64,
    pub(crate) label: String,
    pub(crate) created_at: String,
    #[sqlx(skip)]
    pub(crate) created_display: String,
}

pub(crate) async fn recipe_versions(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Result<Vec<VersionListRow>> {
    let mut rows = sqlx::query_as::<_, VersionListRow>(
        "SELECT v.version,v.label,v.created_at FROM recipe_versions v
         JOIN recipes r ON r.id=v.recipe_id
         WHERE v.recipe_id=? AND r.user_id=?
         ORDER BY v.version DESC",
    )
    .bind(recipe_id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    for row in &mut rows {
        row.created_display = display_timestamp(&row.created_at);
    }
    Ok(rows)
}

#[derive(Debug, Clone, FromRow)]
struct StoredVersionRow {
    version: i64,
    label: String,
    created_at: String,
    snapshot: String,
}

/// One stored version with its JSON contract checked at the repository edge.
#[derive(Debug, Clone)]
pub(crate) struct VersionDetail {
    pub(crate) version: i64,
    pub(crate) label: String,
    pub(crate) created_at: String,
    pub(crate) snapshot: RecipeSnapshot,
}

pub(crate) async fn find_version(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    version: i64,
) -> Result<VersionDetail> {
    let row = sqlx::query_as::<_, StoredVersionRow>(
        "SELECT v.version,v.label,v.created_at,v.snapshot FROM recipe_versions v
         JOIN recipes r ON r.id=v.recipe_id
         WHERE v.recipe_id=? AND v.version=? AND r.user_id=?",
    )
    .bind(recipe_id)
    .bind(version)
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)?;
    let snapshot: RecipeSnapshot = serde_json::from_str(&row.snapshot).map_err(|error| {
        AppError::Internal(format!("Stored recipe version is invalid: {error}"))
    })?;
    if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(AppError::Internal(format!(
            "Stored recipe version uses unsupported schema {}.",
            snapshot.schema_version
        )));
    }
    Ok(VersionDetail {
        version: row.version,
        label: row.label,
        created_at: row.created_at,
        snapshot,
    })
}

pub(crate) async fn delete_version(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, version)): Path<(String, i64)>,
) -> Result<Response> {
    let result = sqlx::query(
        "DELETE FROM recipe_versions
         WHERE recipe_id=? AND version=?
           AND EXISTS (
               SELECT 1 FROM recipes r
               WHERE r.id=recipe_versions.recipe_id AND r.user_id=?
           )",
    )
    .bind(&id)
    .bind(version)
    .bind(&user.id)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(redirect_recipe(&id))
}

/// View one stored version read-only. Renders the saved snapshot JSON, not
/// the live tables, so the page shows exactly what the recipe looked like.
pub(crate) async fn view_version(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, version)): Path<(String, i64)>,
) -> Result<Html<String>> {
    let recipe = find_recipe(&state.db, &user.id, &id).await?;
    let detail = find_version(&state.db, &user.id, &id, version).await?;
    render(VersionTemplate {
        recipe,
        version: detail.version,
        label: detail.label,
        created_display: display_timestamp(&detail.created_at),
        created_at: detail.created_at,
        servings_value: option_number(detail.snapshot.servings),
        prep_value: option_number(detail.snapshot.prep_minutes),
        cook_value: option_number(detail.snapshot.cook_minutes),
        snapshot: detail.snapshot,
    })
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
    ensure_recipe_history(&mut tx, &user.id, &id).await?;
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
        .bind(form.optional.is_some())
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
    record_version(&mut tx, &user.id, &id, &block_label(section, "added")).await?;
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &id);
    Ok(redirect_recipe(&id))
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
    save_block_edit(&mut tx, &user.id, &id, &block, &text, &form).await?;
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &id);
    Ok(redirect_recipe(&id))
}

/// Writes one block's edited fields, its step ingredients, and the matching
/// version. Shared by the edit form and the move buttons, which post the same
/// fields.
async fn save_block_edit(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    recipe_id: &str,
    block: &Block,
    text: &str,
    form: &BlockForm,
) -> Result<()> {
    let is_ingredient = block.section == "ingredient";
    ensure_recipe_history(tx, user_id, recipe_id).await?;
    sqlx::query(
        "UPDATE recipe_blocks SET text=?,quantity=?,unit=?,optional=? WHERE id=? AND recipe_id=?",
    )
    .bind(text)
    .bind(if is_ingredient {
        trim(form.quantity.as_deref().unwrap_or(""))
    } else {
        String::new()
    })
    .bind(if is_ingredient {
        trim(form.unit.as_deref().unwrap_or(""))
    } else {
        String::new()
    })
    .bind(form.optional.is_some())
    .bind(&block.id)
    .bind(recipe_id)
    .execute(&mut **tx)
    .await?;
    if block.section == "step" {
        replace_step_ingredients(
            tx,
            &block.id,
            form.step_ingredients.as_deref().unwrap_or(""),
        )
        .await?;
    }
    invalidate_chart(tx, recipe_id).await?;
    record_version(
        tx,
        user_id,
        recipe_id,
        &block_label(&block.section, "edited"),
    )
    .await
}

/// Whether a submitted block edit would change anything stored. The move buttons
/// post the whole edit form, so an unchanged form must not add a history entry.
async fn block_edit_differs(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    recipe_id: &str,
    block: &Block,
    text: &str,
    form: &BlockForm,
) -> Result<bool> {
    if text != block.text || form.optional.is_some() != block.optional() {
        return Ok(true);
    }
    if block.section == "ingredient"
        && (trim(form.quantity.as_deref().unwrap_or("")) != block.quantity
            || trim(form.unit.as_deref().unwrap_or("")) != block.unit)
    {
        return Ok(true);
    }
    if block.section == "step" {
        let stored = step_ingredients_by_step(&mut **tx, recipe_id)
            .await?
            .remove(&block.id)
            .unwrap_or_default();
        let submitted: Vec<String> = form
            .step_ingredients
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(trim)
            .filter(|line| !line.is_empty())
            .collect();
        return Ok(stored != submitted);
    }
    Ok(false)
}

/// Moves a block one place up or down within its section. The move buttons sit
/// inside the block's edit form, so a changed edit submitted with them is saved
/// first, in the same transaction, instead of being dropped.
pub(crate) async fn move_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, block_id, direction)): Path<(String, String, String)>,
    body: std::result::Result<Form<BlockForm>, FormRejection>,
) -> Result<Response> {
    let block = find_block(&state.db, &user.id, &id, &block_id).await?;
    let delta = match direction.as_str() {
        "up" => -1,
        "down" => 1,
        _ => return Err(AppError::BadRequest("Invalid movement.".into())),
    };
    let target = block.position + delta;
    let edit = body
        .ok()
        .map(|Form(form)| form)
        .filter(|form| !trim(&form.text).is_empty());
    if edit.is_none() && target < 0 {
        return Ok(redirect_recipe(&id));
    }
    let mut tx = state.db.begin().await?;
    if let Some(form) = &edit {
        let text = trim(&form.text);
        if block_edit_differs(&mut tx, &id, &block, &text, form).await? {
            save_block_edit(&mut tx, &user.id, &id, &block, &text, form).await?;
        }
    }
    if target >= 0 {
        let neighbor: Option<String> = sqlx::query_scalar(
            "SELECT id FROM recipe_blocks WHERE recipe_id=? AND section=? AND position=?",
        )
        .bind(&id)
        .bind(&block.section)
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(other) = neighbor {
            ensure_recipe_history(&mut tx, &user.id, &id).await?;
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
            record_version(
                &mut tx,
                &user.id,
                &id,
                &block_label(&block.section, "reordered"),
            )
            .await?;
        }
    }
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &id);
    Ok(redirect_recipe(&id))
}

pub(crate) async fn delete_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, block_id)): Path<(String, String)>,
) -> Result<Response> {
    let block = find_block(&state.db, &user.id, &id, &block_id).await?;
    let mut tx = state.db.begin().await?;
    ensure_recipe_history(&mut tx, &user.id, &id).await?;
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
    record_version(
        &mut tx,
        &user.id,
        &id,
        &block_label(&block.section, "deleted"),
    )
    .await?;
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &id);
    Ok(redirect_recipe(&id))
}

pub(crate) async fn find_recipe(db: &SqlitePool, user_id: &str, id: &str) -> Result<Recipe> {
    sqlx::query_as("SELECT * FROM recipes WHERE user_id=? AND id=?")
        .bind(user_id)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

pub(crate) async fn find_recipe_tx(
    conn: &mut SqliteConnection,
    user_id: &str,
    id: &str,
) -> Result<Recipe> {
    sqlx::query_as("SELECT * FROM recipes WHERE user_id=? AND id=?")
        .bind(user_id)
        .bind(id)
        .fetch_optional(conn)
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

/// Blocks of a recipe in display order. The caller has already checked that
/// the recipe belongs to the signed-in user.
async fn blocks<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    recipe_id: &str,
) -> Result<Vec<Block>> {
    Ok(
        sqlx::query_as("SELECT * FROM recipe_blocks WHERE recipe_id=? ORDER BY section,position")
            .bind(recipe_id)
            .fetch_all(conn)
            .await?,
    )
}

/// Ingredient lines per step block id, loaded in one query for the recipe.
/// Like `blocks`, the caller owns the ownership check.
async fn step_ingredients_by_step<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    recipe_id: &str,
) -> Result<HashMap<String, Vec<String>>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT s.step_id,s.text FROM recipe_step_ingredients s
         JOIN recipe_blocks b ON b.id=s.step_id
         WHERE b.recipe_id=?
         ORDER BY s.step_id,s.position",
    )
    .bind(recipe_id)
    .fetch_all(conn)
    .await?;
    let mut by_step: HashMap<String, Vec<String>> = HashMap::new();
    for (step_id, text) in rows {
        by_step.entry(step_id).or_default().push(text);
    }
    Ok(by_step)
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

pub(crate) async fn sources(db: &SqlitePool, user_id: &str, id: &str) -> Result<Vec<Source>> {
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
    let mut tx = db.begin().await?;
    for (idx, candidate) in candidates.iter().enumerate() {
        sqlx::query(
            "INSERT OR REPLACE INTO draft_thumbnails(draft_id,idx,seconds,jpeg)VALUES(?,?,?,?)",
        )
        .bind(draft_id)
        .bind(idx as i64)
        .bind(candidate.seconds as i64)
        .bind(&candidate.jpeg)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
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

/// Content snapshot of a recipe, read inside the caller's transaction so a
/// version captures the state the write just produced.
pub(crate) async fn recipe_snapshot(
    conn: &mut SqliteConnection,
    user_id: &str,
    recipe_id: &str,
) -> Result<RecipeSnapshot> {
    let recipe = find_recipe_tx(conn, user_id, recipe_id).await?;
    let (ingredients, steps): (Vec<Block>, Vec<Block>) = blocks(&mut *conn, recipe_id)
        .await?
        .into_iter()
        .partition(|block| block.section == "ingredient");
    let mut uses = step_ingredients_by_step(&mut *conn, recipe_id).await?;
    Ok(RecipeSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        title: recipe.title,
        description: recipe.description,
        servings: recipe.servings,
        prep_minutes: recipe.prep_minutes,
        cook_minutes: recipe.cook_minutes,
        ingredients: ingredients
            .into_iter()
            .map(|block| SnapshotIngredient {
                optional: block.optional(),
                name: block.text,
                quantity: block.quantity,
                unit: block.unit,
            })
            .collect(),
        steps: steps
            .into_iter()
            .map(|block| SnapshotStep {
                ingredients: uses.remove(&block.id).unwrap_or_default(),
                text: block.text,
            })
            .collect(),
    })
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
    let recipe = find_recipe(&state.db, &user.id, &id).await?;
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
    if !state.thumbnail_jobs.lock().insert(id.clone()) {
        return Ok(redirect_recipe(&id));
    }
    let db = state.db.clone();
    let jobs = state.thumbnail_jobs.clone();
    let job_recipe_id = id.clone();
    let embedder = state.embedder.clone();
    tokio::spawn(async move {
        if let Ok(candidates) =
            crate::media::extract_fresh_thumbnail_candidates(&video_url, recipe.title, embedder)
                .await
        {
            let _ = store_recipe_thumbnails(&db, &job_recipe_id, &candidates).await;
        }
        // A failed or empty run simply returns the page to its idle state.
        jobs.lock().remove(&job_recipe_id);
    });
    Ok(redirect_recipe(&id))
}

/// Builds the step-moment pool of a recipe saved without one (a recipe from
/// before video imports kept a pool). Runs in the background: it downloads
/// the social video again, stores the pool, and matches the steps. The page
/// shows a pending note until the run ends; a failed run logs a warning and
/// returns the page to its idle state.
pub(crate) async fn start_moment_capture(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    find_recipe(&state.db, &user.id, &id).await?;
    let Some(embedder) = state.embedder.clone() else {
        return Err(AppError::BadRequest(
            "Video step moments need the embedding service (EMBEDDING_URL).".into(),
        ));
    };
    if !crate::step_moments_enabled(&state.db).await? {
        return Err(AppError::BadRequest(
            "Video step moments are turned off in Settings.".into(),
        ));
    }
    let recipe_sources = sources(&state.db, &user.id, &id).await?;
    let video_url = social_source_url(&recipe_sources).ok_or_else(|| {
        AppError::BadRequest(
            "This recipe has no social-video source to find step moments in.".into(),
        )
    })?;
    if !state.moment_jobs.lock().insert(id.clone()) {
        return Ok(redirect_recipe(&id));
    }
    let job_state = state.clone();
    let recipe_id = id.clone();
    tokio::spawn(async move {
        let outcome: Result<usize> = async {
            let pool = crate::media::extract_fresh_moment_pool(&video_url, &embedder).await?;
            if pool.vectors.is_empty() {
                return Ok(0);
            }
            store_recipe_moment_pool(&job_state.db, &recipe_id, &pool).await?;
            moments::refresh_recipe(&job_state, &recipe_id).await?;
            Ok(pool.vectors.len())
        }
        .await;
        match outcome {
            Ok(0) => tracing::warn!(%recipe_id, "The video gave no step-moment vectors"),
            Ok(vectors) => tracing::info!(%recipe_id, vectors, "Step moments captured"),
            Err(error) => tracing::warn!(%recipe_id, %error, "Step moments were not captured"),
        }
        job_state.moment_jobs.lock().remove(&recipe_id);
    });
    Ok(redirect_recipe(&id))
}

// ---- Video step moments ----

/// One stored moment vector row (draft or recipe scope).
#[derive(FromRow)]
struct MomentVectorRow {
    kind: String,
    seconds: f64,
    vector: Vec<u8>,
}

fn moment_vectors(rows: Vec<MomentVectorRow>) -> Vec<MomentVector> {
    rows.into_iter()
        .filter_map(|row| {
            Some(MomentVector {
                kind: MomentKind::parse(&row.kind)?,
                seconds: row.seconds,
                vector: crate::embeddings::from_blob(&row.vector)?,
            })
        })
        .collect()
}

/// Stores a video import's moment pool with its draft.
pub(crate) async fn store_draft_moment_pool(
    db: &SqlitePool,
    draft_id: &str,
    pool: &MomentPool,
) -> Result<()> {
    let mut tx = db.begin().await?;
    for vector in &pool.vectors {
        sqlx::query(
            "INSERT INTO draft_moment_vectors(draft_id,kind,seconds,vector)VALUES(?,?,?,?)",
        )
        .bind(draft_id)
        .bind(vector.kind.as_str())
        .bind(vector.seconds)
        .bind(crate::embeddings::to_blob(&vector.vector))
        .execute(&mut *tx)
        .await?;
    }
    for still in &pool.stills {
        sqlx::query(
            "INSERT OR REPLACE INTO draft_moment_stills(draft_id,seconds,jpeg)VALUES(?,?,?)",
        )
        .bind(draft_id)
        .bind(still.seconds as i64)
        .bind(&still.jpeg)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Carries a draft's moment pool to the next draft of its alteration chain.
pub(crate) async fn copy_draft_moment_pool(
    db: &SqlitePool,
    from_draft_id: &str,
    to_draft_id: &str,
) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO draft_moment_vectors(draft_id,kind,seconds,vector) SELECT ?,kind,seconds,vector FROM draft_moment_vectors WHERE draft_id=?")
        .bind(to_draft_id)
        .bind(from_draft_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT OR REPLACE INTO draft_moment_stills(draft_id,seconds,jpeg) SELECT ?,seconds,jpeg FROM draft_moment_stills WHERE draft_id=?")
        .bind(to_draft_id)
        .bind(from_draft_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The pool a draft's steps are matched against: the draft's own pool (a
/// video import), else the pool of the recipe it alters.
pub(crate) async fn draft_moment_vectors(
    db: &SqlitePool,
    draft_id: &str,
    recipe_id: Option<&str>,
) -> Result<Vec<MomentVector>> {
    let rows: Vec<MomentVectorRow> = sqlx::query_as(
        "SELECT kind,seconds,vector FROM draft_moment_vectors WHERE draft_id=? ORDER BY seconds",
    )
    .bind(draft_id)
    .fetch_all(db)
    .await?;
    if !rows.is_empty() {
        return Ok(moment_vectors(rows));
    }
    match recipe_id {
        Some(recipe_id) => recipe_moment_vectors(db, recipe_id).await,
        None => Ok(Vec::new()),
    }
}

pub(crate) async fn recipe_moment_vectors(
    db: &SqlitePool,
    recipe_id: &str,
) -> Result<Vec<MomentVector>> {
    let rows: Vec<MomentVectorRow> = sqlx::query_as(
        "SELECT kind,seconds,vector FROM recipe_moment_vectors WHERE recipe_id=? ORDER BY seconds",
    )
    .bind(recipe_id)
    .fetch_all(db)
    .await?;
    Ok(moment_vectors(rows))
}

/// True when the recipe has a moment pool to match its steps against.
pub(crate) async fn recipe_has_moment_pool(db: &SqlitePool, recipe_id: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM recipe_moment_vectors WHERE recipe_id=?)")
            .bind(recipe_id)
            .fetch_one(db)
            .await?,
    )
}

/// Replaces a saved recipe's moment pool with a freshly captured one.
pub(crate) async fn store_recipe_moment_pool(
    db: &SqlitePool,
    recipe_id: &str,
    pool: &MomentPool,
) -> Result<()> {
    let mut tx = db.begin().await?;
    for statement in [
        "DELETE FROM recipe_moment_vectors WHERE recipe_id=?",
        "DELETE FROM recipe_moment_stills WHERE recipe_id=?",
    ] {
        sqlx::query(statement)
            .bind(recipe_id)
            .execute(&mut *tx)
            .await?;
    }
    for vector in &pool.vectors {
        sqlx::query(
            "INSERT INTO recipe_moment_vectors(recipe_id,kind,seconds,vector)VALUES(?,?,?,?)",
        )
        .bind(recipe_id)
        .bind(vector.kind.as_str())
        .bind(vector.seconds)
        .bind(crate::embeddings::to_blob(&vector.vector))
        .execute(&mut *tx)
        .await?;
    }
    for still in &pool.stills {
        sqlx::query(
            "INSERT OR REPLACE INTO recipe_moment_stills(recipe_id,seconds,jpeg)VALUES(?,?,?)",
        )
        .bind(recipe_id)
        .bind(still.seconds as i64)
        .bind(&still.jpeg)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Replaces the matched moments of a draft's steps.
pub(crate) async fn replace_draft_step_moments(
    db: &SqlitePool,
    draft_id: &str,
    moments: &[StepMoment],
) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM draft_step_moments WHERE draft_id=?")
        .bind(draft_id)
        .execute(&mut *tx)
        .await?;
    for moment in moments {
        sqlx::query(
            "INSERT INTO draft_step_moments(draft_id,step_index,seconds,source)VALUES(?,?,?,?)",
        )
        .bind(draft_id)
        .bind(moment.step_index as i64)
        .bind(moment.seconds as i64)
        .bind(moment.source.as_str())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn draft_step_moments(db: &SqlitePool, draft_id: &str) -> Result<Vec<StepMoment>> {
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT step_index,seconds,source FROM draft_step_moments WHERE draft_id=? ORDER BY step_index",
    )
    .bind(draft_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(step_index, seconds, source)| {
            Some(StepMoment {
                step_index: usize::try_from(step_index).ok()?,
                seconds: u64::try_from(seconds).ok()?,
                source: MomentKind::parse(&source)?,
            })
        })
        .collect())
}

/// Seconds of the stills a draft can show: its own, else its recipe's.
pub(crate) async fn draft_still_seconds(
    db: &SqlitePool,
    draft_id: &str,
    recipe_id: Option<&str>,
) -> Result<Vec<u64>> {
    let own: Vec<i64> = sqlx::query_scalar(
        "SELECT seconds FROM draft_moment_stills WHERE draft_id=? ORDER BY seconds",
    )
    .bind(draft_id)
    .fetch_all(db)
    .await?;
    if !own.is_empty() {
        return Ok(own.into_iter().map(|s| s.max(0) as u64).collect());
    }
    match recipe_id {
        Some(recipe_id) => recipe_still_seconds(db, recipe_id).await,
        None => Ok(Vec::new()),
    }
}

pub(crate) async fn recipe_still_seconds(db: &SqlitePool, recipe_id: &str) -> Result<Vec<u64>> {
    let rows: Vec<i64> = sqlx::query_scalar(
        "SELECT seconds FROM recipe_moment_stills WHERE recipe_id=? ORDER BY seconds",
    )
    .bind(recipe_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|s| s.max(0) as u64).collect())
}

/// One still for a live draft of this user: from the draft's pool, else from
/// the pool of the recipe the draft alters.
pub(crate) async fn find_draft_still(
    db: &SqlitePool,
    user_id: &str,
    draft_id: &str,
    seconds: i64,
) -> Result<Option<Vec<u8>>> {
    let now = Utc::now().to_rfc3339();
    let own: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT s.jpeg FROM draft_moment_stills s JOIN ai_drafts d ON d.id=s.draft_id
         WHERE d.id=? AND d.user_id=? AND d.expires_at >= ? AND s.seconds=?",
    )
    .bind(draft_id)
    .bind(user_id)
    .bind(&now)
    .bind(seconds)
    .fetch_optional(db)
    .await?;
    if own.is_some() {
        return Ok(own);
    }
    Ok(sqlx::query_scalar(
        "SELECT s.jpeg FROM recipe_moment_stills s JOIN ai_drafts d ON d.recipe_id=s.recipe_id
         WHERE d.id=? AND d.user_id=? AND d.expires_at >= ? AND s.seconds=?",
    )
    .bind(draft_id)
    .bind(user_id)
    .bind(&now)
    .bind(seconds)
    .fetch_optional(db)
    .await?)
}

pub(crate) async fn find_recipe_still(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    seconds: i64,
) -> Result<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar(
        "SELECT s.jpeg FROM recipe_moment_stills s JOIN recipes r ON r.id=s.recipe_id
         WHERE r.id=? AND r.user_id=? AND s.seconds=?",
    )
    .bind(recipe_id)
    .bind(user_id)
    .bind(seconds)
    .fetch_optional(db)
    .await?)
}

/// Inside `apply_draft`'s transaction: a video draft's pool replaces the
/// recipe's pool, and each draft step moment is attached to the step block
/// inserted for it (`step_ids[step_index]`).
pub(crate) async fn apply_draft_moments(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    draft_id: &str,
    recipe_id: &str,
    step_ids: &[String],
) -> Result<()> {
    let has_pool: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM draft_moment_vectors WHERE draft_id=?) OR EXISTS(SELECT 1 FROM draft_moment_stills WHERE draft_id=?)")
            .bind(draft_id)
            .bind(draft_id)
            .fetch_one(&mut **tx)
            .await?;
    if has_pool {
        for statement in [
            "DELETE FROM recipe_moment_vectors WHERE recipe_id=?",
            "DELETE FROM recipe_moment_stills WHERE recipe_id=?",
        ] {
            sqlx::query(statement)
                .bind(recipe_id)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query("INSERT INTO recipe_moment_vectors(recipe_id,kind,seconds,vector) SELECT ?,kind,seconds,vector FROM draft_moment_vectors WHERE draft_id=?")
            .bind(recipe_id)
            .bind(draft_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO recipe_moment_stills(recipe_id,seconds,jpeg) SELECT ?,seconds,jpeg FROM draft_moment_stills WHERE draft_id=?")
            .bind(recipe_id)
            .bind(draft_id)
            .execute(&mut **tx)
            .await?;
    }
    let moments: Vec<(i64, i64, String)> =
        sqlx::query_as("SELECT step_index,seconds,source FROM draft_step_moments WHERE draft_id=?")
            .bind(draft_id)
            .fetch_all(&mut **tx)
            .await?;
    for (step_index, seconds, source) in moments {
        let Some(step_id) = usize::try_from(step_index)
            .ok()
            .and_then(|i| step_ids.get(i))
        else {
            continue;
        };
        sqlx::query(
            "INSERT OR REPLACE INTO recipe_step_moments(step_id,seconds,source)VALUES(?,?,?)",
        )
        .bind(step_id)
        .bind(seconds)
        .bind(source)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// The recipe's step blocks in order: `(block id, text)`.
pub(crate) async fn recipe_step_texts(
    db: &SqlitePool,
    recipe_id: &str,
) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT id,text FROM recipe_blocks WHERE recipe_id=? AND section='step' ORDER BY position",
    )
    .bind(recipe_id)
    .fetch_all(db)
    .await?)
}

/// Replaces the matched moments of every step of a recipe.
pub(crate) async fn replace_recipe_step_moments(
    db: &SqlitePool,
    recipe_id: &str,
    moments: &[(String, StepMoment)],
) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM recipe_step_moments WHERE step_id IN (SELECT id FROM recipe_blocks WHERE recipe_id=?)")
        .bind(recipe_id)
        .execute(&mut *tx)
        .await?;
    for (step_id, moment) in moments {
        sqlx::query("INSERT INTO recipe_step_moments(step_id,seconds,source)VALUES(?,?,?)")
            .bind(step_id)
            .bind(moment.seconds as i64)
            .bind(moment.source.as_str())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Matched moments of a recipe's steps, keyed by step block id.
pub(crate) async fn recipe_step_moments(
    db: &SqlitePool,
    recipe_id: &str,
) -> Result<HashMap<String, StepMoment>> {
    let rows: Vec<(String, i64, i64, String)> = sqlx::query_as(
        "SELECT m.step_id,b.position,m.seconds,m.source FROM recipe_step_moments m
         JOIN recipe_blocks b ON b.id=m.step_id WHERE b.recipe_id=?",
    )
    .bind(recipe_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(step_id, position, seconds, source)| {
            let moment = StepMoment {
                step_index: usize::try_from(position).ok()?,
                seconds: u64::try_from(seconds).ok()?,
                source: MomentKind::parse(&source)?,
            };
            Some((step_id, moment))
        })
        .collect())
}

/// Serves one still of a recipe's moment pool to its owner.
pub(crate) async fn recipe_still(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, seconds)): Path<(String, i64)>,
) -> Result<Response> {
    let jpeg = find_recipe_still(&state.db, &user.id, &id, seconds)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(crate::image_response(jpeg))
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
