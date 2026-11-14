use crate::chart::{build_chart, infer_step_ingredients};
use crate::folders;
use crate::htmx::Flash;
use crate::moments::{self, MomentKind, MomentPool, MomentVector, StepMoment};
use crate::search::{self, Indexed};
use crate::thumbs::{self, ThumbCandidate};
use crate::{
    AddRecipeQuery, AppError, AppState, AuthUser, Block, BlockForm, ChatProposal, DeleteTemplate,
    Draft, HomeQuery, HomeTemplate, Recipe, RecipeForm, RecipeQuery, RecipeSnapshot,
    RecipeTemplate, Result, SnapshotIngredient, SnapshotStep, Source, VersionTemplate, ViewBlock,
    ViewStep, display_timestamp, number, option_number, render, required, stamp, trim,
};
use axum::{
    Form,
    extract::{Multipart, Path, Query, Request, State, rejection::FormRejection},
    http::{HeaderValue, Method, Uri, header},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::Utc;
use parking_lot::Mutex;
use sqlx::{FromRow, SqliteConnection, SqlitePool};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
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

/// The library page. Without `?q=` it lists every recipe filed A to Z, or,
/// in the Folders or Cuisines view, one ticket stack per folder (`?folder=`
/// opens one). With `?q=`, the recipes ranked by embedding similarity or
/// BM25 (the user's choice, and the fallback when the embedding service is
/// missing or down); a query that names a folder ("thai", "desserts") lists
/// that folder's recipes first.
pub(crate) async fn home(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    flash: Flash,
    Query(params): Query<HomeQuery>,
) -> Result<Html<String>> {
    let query = search::normalize_query(params.q.as_deref().unwrap_or(""));
    let view = folders::library_view(&state.db, &user.id).await?;
    let cards: Vec<LibraryCard> = sqlx::query_as(
        "SELECT id,title,description,servings,prep_minutes,cook_minutes,
                thumbnail_jpeg IS NOT NULL AS has_thumbnail,created_at
         FROM recipes WHERE user_id=? ORDER BY updated_at DESC",
    )
    .bind(&user.id)
    .fetch_all(&state.db)
    .await?;
    let total = cards.len();
    let open = params
        .folder
        .as_deref()
        .filter(|_| query.is_empty())
        .and_then(|slug| folders::resolve(slug, view));
    let mut stacks = Vec::new();
    let mut open_folder = None;
    let (mut recipes, results, notice) = if !query.is_empty() {
        let outcome = search::search(&state, &user.id, &query).await?;
        let mut by_id: HashMap<String, LibraryCard> = cards
            .into_iter()
            .map(|card| (card.id.clone(), card))
            .collect();
        let mut found = Vec::new();
        if let Some((shelf, folder)) = folders::match_query(&query) {
            let filed = folders::shelve(&state, &user.id, shelf).await?;
            let mut members: Vec<LibraryCard> = filed
                .iter()
                .filter(|(_, slug)| **slug == folder.slug)
                .filter_map(|(id, _)| by_id.remove(id))
                .collect();
            file_a_to_z(&mut members);
            found = members;
        }
        found.extend(outcome.ids.iter().filter_map(|id| by_id.remove(id)));
        (found, true, outcome.notice)
    } else if let Some((shelf, folder)) = open {
        let filed = folders::shelve(&state, &user.id, shelf).await?;
        let mut members: Vec<LibraryCard> = cards
            .into_iter()
            .filter(|card| filed.get(&card.id).copied() == Some(folder.slug))
            .collect();
        file_a_to_z(&mut members);
        open_folder = Some(folders::OpenFolder::new(shelf, folder));
        (members, false, String::new())
    } else {
        let notice = if flash.is("account_deleted") {
            "Account deleted. This browser switched to your next account.".to_string()
        } else {
            String::new()
        };
        match view {
            Some(shelf) if total > 0 => {
                let filed = folders::shelve(&state, &user.id, shelf).await?;
                stacks = folders::stacks(shelf, &filed, &cards);
                (Vec::new(), false, notice)
            }
            _ => {
                let mut cards = cards;
                file_a_to_z(&mut cards);
                (cards, false, notice)
            }
        }
    };
    attach_ingredients(&state.db, &user.id, &mut recipes).await?;
    stamp_added(&mut recipes);
    render(HomeTemplate {
        recipes,
        query,
        results,
        notice,
        view: view.map_or("az", folders::Shelf::key),
        total,
        stacks,
        open_folder,
    })
}

/// One library ticket: the fields home.html renders plus whether a dish
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
    pub(crate) created_at: String,
    /// The ingredient lines printed on the ticket, in recipe order; filled
    /// by `attach_ingredients`.
    #[sqlx(skip)]
    pub(crate) ingredients: Vec<Block>,
    /// When the recipe was added, as printed at the foot of the ticket;
    /// filled by `stamp_added`.
    #[sqlx(skip)]
    pub(crate) added: String,
}

/// Ingredient lists longer than this print in two columns, so a big recipe
/// makes a wider-packed ticket rather than a very long one.
pub(crate) const TICKET_COLUMNS_AFTER: usize = 8;

/// On a phone a ticket lists this many ingredients, then "… N more"; the
/// cut is CSS (`.ticket-more`), so wider screens keep the whole list.
pub(crate) const TICKET_PHONE_ITEMS: usize = 6;

impl LibraryCard {
    /// True when the ticket's ingredients print in two columns.
    pub(crate) fn packed(&self) -> bool {
        self.ingredients.len() > TICKET_COLUMNS_AFTER
    }

    /// How many ingredients a phone-sized ticket leaves out behind its
    /// "…" line (it shows the first `TICKET_PHONE_ITEMS`).
    pub(crate) fn phone_hidden(&self) -> usize {
        self.ingredients.len().saturating_sub(TICKET_PHONE_ITEMS)
    }

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
        total_time(self.prep_minutes, self.cook_minutes)
    }
}

/// Prep plus cook time, such as "35 min" or "1 h 10 min"; empty when
/// neither is set.
pub(crate) fn total_time(prep_minutes: Option<i64>, cook_minutes: Option<i64>) -> String {
    let minutes = prep_minutes.unwrap_or(0).max(0) + cook_minutes.unwrap_or(0).max(0);
    match (minutes / 60, minutes % 60) {
        (0, 0) => String::new(),
        (0, rest) => format!("{rest} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, rest) => format!("{hours} h {rest} min"),
    }
}

/// The letter the recipe page's plate shows when the recipe has no dish
/// photo (and in e-ink mode, which leaves photos out): the title's first letter or digit,
/// upper-cased, or empty when it has neither.
pub(crate) fn plate_initial(title: &str) -> String {
    title
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default()
}

/// A plate name as stored: whitespace collapsed, at most two words and
/// `PLATE_NAME_MAX` characters, else empty (the plate then shows the
/// title's initial).
pub(crate) fn clean_plate_name(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let name = words.join(" ");
    if words.len() > 2 || name.chars().count() > PLATE_NAME_MAX {
        return String::new();
    }
    name
}

/// The size of a plate name relative to a single initial: a word sized so
/// its longest word spans about the width of the well, capped well below
/// the initial's size so short names do not crowd the rim.
pub(crate) fn plate_name_scale(name: &str) -> f32 {
    let longest = name
        .split_whitespace()
        .map(|word| word.chars().count())
        .max()
        .unwrap_or(0);
    if longest <= 1 {
        return 1.0;
    }
    // The well is about 1.65 initials wide; an upper-case letter of the
    // display face is about 0.68 of its size.
    (1.65 / (0.68 * longest as f32)).clamp(0.24, 0.62)
}

/// The longest plate name that still reads in the plate's well.
pub(crate) const PLATE_NAME_MAX: usize = 16;

/// The letter a title is filed under: its first letter, upper-cased, or '#'
/// when the title starts with a digit or a letter outside A to Z.
pub(crate) fn file_letter(title: &str) -> char {
    match title.chars().find(|c| c.is_alphanumeric()) {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => '#',
    }
}

/// The order the library files a title in: by letter (titles starting with
/// anything else come first), then title.
pub(crate) fn a_to_z_key(title: &str) -> (char, String) {
    (file_letter(title), title.to_lowercase())
}

/// Files the library A to Z (`a_to_z_key`).
pub(crate) fn file_a_to_z(cards: &mut [LibraryCard]) {
    cards.sort_by_cached_key(|card| a_to_z_key(&card.title));
}

/// Stamps each ticket with the UTC date its recipe was added, e.g.
/// "15 Oct 2026", adding the time ("15 Oct 2026, 22:11") when another
/// ticket shares that date.
pub(crate) fn stamp_added(cards: &mut [LibraryCard]) {
    let added: Vec<Option<chrono::DateTime<Utc>>> = cards
        .iter()
        .map(|card| {
            chrono::DateTime::parse_from_rfc3339(&card.created_at)
                .ok()
                .map(|time| time.with_timezone(&Utc))
        })
        .collect();
    let mut per_day: HashMap<chrono::NaiveDate, usize> = HashMap::new();
    for time in added.iter().flatten() {
        *per_day.entry(time.date_naive()).or_default() += 1;
    }
    for (card, time) in cards.iter_mut().zip(added) {
        card.added = match time {
            Some(time) if per_day[&time.date_naive()] > 1 => {
                time.format("%-d %b %Y, %H:%M").to_string()
            }
            Some(time) => time.format("%-d %b %Y").to_string(),
            None => String::new(),
        };
    }
}

/// Loads the ingredient lines of the cards shown, in one query.
async fn attach_ingredients(
    db: &SqlitePool,
    user_id: &str,
    cards: &mut [LibraryCard],
) -> Result<()> {
    if cards.is_empty() {
        return Ok(());
    }
    let ids = vec!["?"; cards.len()].join(",");
    let sql = format!(
        "SELECT recipe_id,id,section,position,text,quantity,unit,optional FROM recipe_blocks
         WHERE section='ingredient' AND recipe_id IN ({ids})
           AND recipe_id IN (SELECT id FROM recipes WHERE user_id=?)
         ORDER BY recipe_id, position"
    );
    let mut query = sqlx::query_as::<_, IndexBlockRow>(&sql);
    for card in cards.iter() {
        query = query.bind(&card.id);
    }
    let rows = query.bind(user_id).fetch_all(db).await?;
    let mut by_recipe: HashMap<String, Vec<Block>> = HashMap::new();
    for row in rows {
        by_recipe.entry(row.recipe_id).or_default().push(row.block);
    }
    for card in cards {
        card.ingredients = by_recipe.remove(&card.id).unwrap_or_default();
    }
    Ok(())
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
    let mut blocks = group_blocks(block_rows);
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

fn group_blocks(rows: Vec<IndexBlockRow>) -> HashMap<String, Vec<Block>> {
    let mut blocks: HashMap<String, Vec<Block>> = HashMap::new();
    for row in rows {
        blocks.entry(row.recipe_id).or_default().push(row.block);
    }
    blocks
}

/// A recipe that may need a new search vector: its search source, the
/// recipe's `updated_at` as read, and the document and `updated_at` its
/// stored vector was built from (`stored` is `None` without a usable vector).
pub(crate) struct IndexCandidate {
    pub(crate) source: IndexSource,
    pub(crate) updated_at: String,
    pub(crate) stored: Option<String>,
    pub(crate) built_from: Option<String>,
}

#[derive(FromRow)]
struct IndexCandidateRow {
    id: String,
    title: String,
    description: String,
    updated_at: String,
    stored: Option<String>,
    built_from: Option<String>,
}

/// Recipes of `user_id` (or of all users when `None`) to check for a new
/// search vector, newest edit first, with their blocks. With `all`, every
/// recipe; otherwise only the ones without a usable vector or edited since
/// it was built (every content edit bumps `recipes.updated_at`, and
/// `recipe_embeddings.updated_at` keeps the value the vector was built from).
pub(crate) async fn index_candidates(
    db: &SqlitePool,
    user_id: Option<&str>,
    all: bool,
) -> Result<Vec<IndexCandidate>> {
    const MATCHING: &str = "FROM recipes r LEFT JOIN recipe_embeddings e ON e.recipe_id=r.id
         WHERE (? IS NULL OR r.user_id=?)
           AND (? OR e.recipe_id IS NULL OR e.updated_at<>r.updated_at OR length(e.vector)<>?)";
    let vector_bytes = (crate::embeddings::DIMENSIONS * 4) as i64;
    let rows: Vec<IndexCandidateRow> = sqlx::query_as(&format!(
        "SELECT r.id,r.title,r.description,r.updated_at,
                CASE WHEN length(e.vector)=? THEN e.document END AS stored,
                e.updated_at AS built_from
         {MATCHING} ORDER BY r.updated_at DESC, r.id"
    ))
    .bind(vector_bytes)
    .bind(user_id)
    .bind(user_id)
    .bind(all)
    .bind(vector_bytes)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let block_rows: Vec<IndexBlockRow> = sqlx::query_as(&format!(
        "SELECT recipe_id,id,section,position,text,quantity,unit,optional FROM recipe_blocks
         WHERE recipe_id IN (SELECT r.id {MATCHING})
         ORDER BY recipe_id, section, position"
    ))
    .bind(user_id)
    .bind(user_id)
    .bind(all)
    .bind(vector_bytes)
    .fetch_all(db)
    .await?;
    let mut blocks = group_blocks(block_rows);
    Ok(rows
        .into_iter()
        .map(|row| IndexCandidate {
            source: IndexSource {
                blocks: blocks.remove(&row.id).unwrap_or_default(),
                id: row.id,
                title: row.title,
                description: row.description,
            },
            updated_at: row.updated_at,
            stored: row.stored,
            built_from: row.built_from,
        })
        .collect())
}

/// Stores (or replaces) a recipe's document vector, built from the recipe
/// as it was at `built_from` (its `updated_at`). A recipe deleted or edited
/// since is skipped: the next refresh builds its new document.
pub(crate) async fn store_embedding(
    db: &SqlitePool,
    recipe_id: &str,
    document: &str,
    vector: &[f32],
    built_from: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO recipe_embeddings(recipe_id,document,vector,updated_at)
         SELECT ?1,?2,?3,?4 WHERE EXISTS (SELECT 1 FROM recipes WHERE id=?1 AND updated_at=?4)
         ON CONFLICT(recipe_id) DO UPDATE SET
           document=excluded.document, vector=excluded.vector, updated_at=excluded.updated_at",
    )
    .bind(recipe_id)
    .bind(document)
    .bind(crate::embeddings::to_blob(vector))
    .bind(built_from)
    .execute(db)
    .await?;
    Ok(())
}

/// Marks a recipe's stored vector as matching the recipe at `built_from`,
/// after an edit that left its document unchanged (servings, say).
pub(crate) async fn confirm_embedding(
    db: &SqlitePool,
    recipe_id: &str,
    document: &str,
    built_from: &str,
) -> Result<()> {
    sqlx::query("UPDATE recipe_embeddings SET updated_at=? WHERE recipe_id=? AND document=?")
        .bind(built_from)
        .bind(recipe_id)
        .bind(document)
        .execute(db)
        .await?;
    Ok(())
}

/// The document and vector of the most recently built search vector.
pub(crate) async fn newest_embedding(db: &SqlitePool) -> Result<Option<(String, Vec<f32>)>> {
    let row: Option<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT document, vector FROM recipe_embeddings WHERE length(vector)=?
         ORDER BY updated_at DESC, recipe_id LIMIT 1",
    )
    .bind((crate::embeddings::DIMENSIONS * 4) as i64)
    .fetch_optional(db)
    .await?;
    Ok(row.and_then(|(document, blob)| Some((document, crate::embeddings::from_blob(&blob)?))))
}

/// Drops every recipe's search vector.
pub(crate) async fn clear_embeddings(db: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM recipe_embeddings")
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

/// A recipe's pinned folder and cuisine (`recipes.folder` / `.cuisine`,
/// empty when filed automatically).
#[derive(FromRow)]
pub(crate) struct RecipePin {
    pub(crate) id: String,
    pub(crate) folder: String,
    pub(crate) cuisine: String,
}

/// The pins of every recipe of the user.
pub(crate) async fn recipe_pins(db: &SqlitePool, user_id: &str) -> Result<Vec<RecipePin>> {
    Ok(
        sqlx::query_as("SELECT id,folder,cuisine FROM recipes WHERE user_id=?")
            .bind(user_id)
            .fetch_all(db)
            .await?,
    )
}

/// One recipe's pins; NotFound when the user has no such recipe.
pub(crate) async fn recipe_pin(db: &SqlitePool, user_id: &str, id: &str) -> Result<RecipePin> {
    sqlx::query_as("SELECT id,folder,cuisine FROM recipes WHERE user_id=? AND id=?")
        .bind(user_id)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

/// Pins a recipe to a folder and a cuisine. Leaves `updated_at` alone: the
/// recipe's content did not change.
pub(crate) async fn set_recipe_pin(
    db: &SqlitePool,
    user_id: &str,
    id: &str,
    folder: &str,
    cuisine: &str,
) -> Result<()> {
    let done = sqlx::query("UPDATE recipes SET folder=?,cuisine=? WHERE user_id=? AND id=?")
        .bind(folder)
        .bind(cuisine)
        .bind(user_id)
        .bind(id)
        .execute(db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// A recipe's stored search vector, if it has one.
pub(crate) async fn recipe_vector(db: &SqlitePool, recipe_id: &str) -> Result<Option<Vec<f32>>> {
    let blob: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT vector FROM recipe_embeddings WHERE recipe_id=?")
            .bind(recipe_id)
            .fetch_optional(db)
            .await?;
    Ok(blob.and_then(|blob| crate::embeddings::from_blob(&blob)))
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
    crate::ai::add_recipe_page(
        &state,
        &user,
        String::new(),
        query.url.unwrap_or_default(),
        crate::media::MediaChannels::default(),
    )
    .await
}

pub(crate) async fn recipe_page(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Query(query): Query<RecipeQuery>,
    flash: Flash,
) -> Result<Html<String>> {
    let recipe = find_recipe(&state.db, &user.id, &id).await?;
    let (ingredient_blocks, step_blocks): (Vec<Block>, Vec<Block>) = blocks(&state.db, &id)
        .await?
        .into_iter()
        .partition(|block| block.section == "ingredient");
    let edit = query.edit.as_deref();
    let scale = crate::scale::Scale::new(recipe.servings, query.serves);
    let ingredients = ingredient_blocks
        .iter()
        .cloned()
        .map(|block| ViewBlock::from_block(block, edit, &scale))
        .collect();
    let mut uses = step_ingredients_by_step(&state.db, &id).await?;
    let recipe_sources = sources(&state.db, &user.id, &id).await?;
    let video_url = social_source_url(&recipe_sources);
    let moments_shown =
        video_url.is_some() && crate::step_moments_enabled(&state.db, &user.id).await?;
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
            let stored = uses.remove(&block.id).unwrap_or_default();
            // A step without saved links shows the ingredients its text
            // names, but its edit form starts empty: saving the guesses
            // would turn them into links the cook never made.
            let shown = if stored.is_empty() {
                infer_step_ingredients(&block.text, &ingredient_blocks)
            } else {
                stored.clone()
            };
            let moment = step_moments
                .remove(&block.id)
                .map(|moment| moments::view(&moment, &still_seconds, &still_base));
            // The edit form keeps the amounts as written; the page shows them
            // scaled to the chosen servings.
            ViewStep {
                ingredients_text: stored.join("\n"),
                ingredients: shown.iter().map(|line| scale.line(line)).collect(),
                block: ViewBlock::from_block(block, edit, &scale),
                moment,
            }
        })
        .collect::<Vec<_>>();
    let chart_view = query.view.as_deref() == Some("chart");
    let chart = build_chart(&recipe, &ingredient_blocks, &steps, query.step, &scale);
    let has_photo = recipe.has_photo;
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
    // The chat drawer's forms and poll come back to this view, step, and
    // servings; the notes (shown on the card only) to the card's servings.
    let back = match (chart_view, query.step) {
        (true, Some(step)) => format!("/recipes/{id}?view=chart&step={step}{}", scale.query),
        (true, None) => format!("/recipes/{id}?view=chart{}", scale.query),
        (false, _) => format!("/recipes/{id}{}", scale.page_query()),
    };
    let chat = crate::chat::panel(&state, &user, &recipe, back, &flash).await?;
    let notes_back = format!("/recipes/{id}{}", scale.page_query());
    let notes =
        crate::notes::panel(&state, &user, &id, &versions, None, notes_back, &flash).await?;
    let filing = folders::filing(&state, &user.id, &id).await?;
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
        active_version: 0,
        chat,
        notes,
        scale,
        filing,
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
    let plate_name = clean_plate_name(&form.plate_name);
    if plate_name.is_empty() && !form.plate_name.trim().is_empty() {
        return Err(AppError::BadRequest(format!(
            "Plate name must be one or two words, at most {PLATE_NAME_MAX} characters."
        )));
    }
    let description = trim(&form.description);
    let servings = number(&form.servings)?;
    let prep_minutes = number(&form.prep_minutes)?;
    let cook_minutes = number(&form.cook_minutes)?;
    if recipe.title == title
        && recipe.plate_name == plate_name
        && recipe.description == description
        && recipe.servings == servings
        && recipe.prep_minutes == prep_minutes
        && recipe.cook_minutes == cook_minutes
    {
        return Ok(redirect_recipe(&id));
    }
    let mut tx = begin_write(&state.db).await?;
    ensure_recipe_history(&mut tx, &user.id, &id).await?;
    sqlx::query("UPDATE recipes SET title=?,plate_name=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,updated_at=? WHERE user_id=? AND id=?")
        .bind(title)
        .bind(&plate_name)
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

/// Makes a saved version the current recipe: its details (plate name
/// included), ingredients, and steps replace the live ones, and the result is
/// recorded as a new version, so the recipe it replaced stays in the history.
/// Sources, the dish photo, and the folder and cuisine pins are not part of a
/// snapshot and are kept.
pub(crate) async fn restore_version(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, version)): Path<(String, i64)>,
) -> Result<Response> {
    let detail = find_version(&state.db, &user.id, &id, version).await?;
    let mut snapshot = detail.snapshot;
    let mut tx = begin_write(&state.db).await?;
    let current = recipe_snapshot(&mut tx, &user.id, &id).await?;
    if snapshot.plate_name.is_none() {
        snapshot.plate_name = if snapshot.title == current.title {
            current.plate_name.clone()
        } else {
            Some(String::new())
        };
    }
    if current == snapshot {
        tx.rollback().await?;
        return Ok(redirect_recipe(&id));
    }
    // The stamp makes chat proposals read before the restore stale.
    sqlx::query("UPDATE recipes SET title=?,plate_name=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,updated_at=? WHERE user_id=? AND id=?")
        .bind(&snapshot.title)
        .bind(snapshot.plate_name.as_deref().unwrap_or_default())
        .bind(&snapshot.description)
        .bind(snapshot.servings)
        .bind(snapshot.prep_minutes)
        .bind(snapshot.cook_minutes)
        .bind(stamp())
        .bind(&user.id)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM recipe_blocks WHERE recipe_id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    insert_blocks(&mut tx, &id, &snapshot.ingredients, &snapshot.steps).await?;
    invalidate_chart(&mut tx, &id).await?;
    record_version(
        &mut tx,
        &user.id,
        &id,
        &format!("Restored version {}", detail.version),
    )
    .await?;
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &user.id, &id);
    Ok(redirect_recipe(&id))
}

/// Deletes one saved version. The newest is refused: it is the recipe as it
/// is now, which notes about "this version" and the chat rely on.
pub(crate) async fn delete_version(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, version)): Path<(String, i64)>,
) -> Result<Response> {
    let newest: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(v.version) FROM recipe_versions v
         JOIN recipes r ON r.id=v.recipe_id
         WHERE v.recipe_id=? AND r.user_id=?",
    )
    .bind(&id)
    .bind(&user.id)
    .fetch_one(&state.db)
    .await?;
    if newest == Some(version) {
        return Err(AppError::BadRequest(
            "The newest version is the recipe as it is now, so it stays in the history.".into(),
        ));
    }
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
    flash: Flash,
) -> Result<Html<String>> {
    let recipe = find_recipe(&state.db, &user.id, &id).await?;
    let detail = find_version(&state.db, &user.id, &id, version).await?;
    let versions = recipe_versions(&state.db, &user.id, &id).await?;
    let notes = crate::notes::panel(
        &state,
        &user,
        &id,
        &versions,
        Some(detail.version),
        format!("/recipes/{id}/versions/{}", detail.version),
        &flash,
    )
    .await?;
    render(VersionTemplate {
        notes,
        versions,
        active_version: detail.version,
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

/// Deletes a recipe. Cascades take its blocks, versions, notes, and chat;
/// its frame candidates have no foreign key, so they go first, in the same
/// transaction.
pub(crate) async fn delete_recipe(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    let mut tx = begin_write(&state.db).await?;
    find_recipe(&mut *tx, &user.id, &id).await?;
    sqlx::query("DELETE FROM recipe_thumbnail_candidates WHERE recipe_id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM recipes WHERE user_id=? AND id=?")
        .bind(&user.id)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
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
    let mut tx = begin_write(&state.db).await?;
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
    touch_recipe(&mut tx, &id).await?;
    record_version(&mut tx, &user.id, &id, &block_label(section, "added")).await?;
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &user.id, &id);
    Ok(redirect_recipe(&id))
}

pub(crate) async fn update_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, block_id)): Path<(String, String)>,
    Form(form): Form<BlockForm>,
) -> Result<Response> {
    let text = required(&form.text, "Block text")?;
    let mut tx = begin_write(&state.db).await?;
    let block = find_block(&mut *tx, &user.id, &id, &block_id).await?;
    // Saving an unchanged form keeps the AI chart and adds no version.
    if !block_edit_differs(&mut tx, &id, &block, &text, &form).await? {
        tx.rollback().await?;
        return Ok(redirect_recipe(&id));
    }
    save_block_edit(&mut tx, &user.id, &id, &block, &text, &form).await?;
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &user.id, &id);
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
    touch_recipe(tx, recipe_id).await?;
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
/// first, in the same transaction, instead of being dropped. The block is read
/// inside the transaction, so a double-clicked move sees where the first one
/// put it.
pub(crate) async fn move_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, block_id, direction)): Path<(String, String, String)>,
    body: std::result::Result<Form<BlockForm>, FormRejection>,
) -> Result<Response> {
    let delta = match direction.as_str() {
        "up" => -1,
        "down" => 1,
        _ => return Err(AppError::BadRequest("Invalid movement.".into())),
    };
    let edit = body
        .ok()
        .map(|Form(form)| form)
        .filter(|form| !trim(&form.text).is_empty());
    let mut tx = begin_write(&state.db).await?;
    let block = find_block(&mut *tx, &user.id, &id, &block_id).await?;
    let target = block.position + delta;
    let mut changed = false;
    if let Some(form) = &edit {
        let text = trim(&form.text);
        if block_edit_differs(&mut tx, &id, &block, &text, form).await? {
            save_block_edit(&mut tx, &user.id, &id, &block, &text, form).await?;
            changed = true;
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
            touch_recipe(&mut tx, &id).await?;
            record_version(
                &mut tx,
                &user.id,
                &id,
                &block_label(&block.section, "reordered"),
            )
            .await?;
            changed = true;
        }
    }
    if !changed {
        tx.rollback().await?;
        return Ok(redirect_recipe(&id));
    }
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &user.id, &id);
    Ok(redirect_recipe(&id))
}

/// Deletes a block and closes the gap it leaves. The block is read inside the
/// transaction, so a repeated delete finds it gone instead of shifting the
/// rest by a stale position.
pub(crate) async fn delete_block(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, block_id)): Path<(String, String)>,
) -> Result<Response> {
    let mut tx = begin_write(&state.db).await?;
    let block = find_block(&mut *tx, &user.id, &id, &block_id).await?;
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
    touch_recipe(&mut tx, &id).await?;
    record_version(
        &mut tx,
        &user.id,
        &id,
        &block_label(&block.section, "deleted"),
    )
    .await?;
    tx.commit().await?;
    moments::spawn_recipe_refresh(&state, &user.id, &id);
    Ok(redirect_recipe(&id))
}

/// Starts a write transaction with `BEGIN IMMEDIATE`, which takes SQLite's
/// write lock up front. A deferred transaction that reads first can fail at
/// its first write with SQLITE_BUSY_SNAPSHOT when a background writer
/// committed in between, and busy_timeout does not retry that; an immediate
/// one waits for the lock instead.
pub(crate) async fn begin_write(
    db: &SqlitePool,
) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
    Ok(db.begin_with("BEGIN IMMEDIATE").await?)
}

/// One of the user's recipes, on the pool or inside a transaction
/// (`&mut *tx`); NotFound for anyone else's. The dish photo is only flagged
/// (`has_photo`), never loaded.
pub(crate) async fn find_recipe<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    user_id: &str,
    id: &str,
) -> Result<Recipe> {
    sqlx::query_as(
        "SELECT id,title,description,servings,prep_minutes,cook_minutes,chart_json,updated_at,
                plate_name,thumbnail_jpeg IS NOT NULL AS has_photo
         FROM recipes WHERE user_id=? AND id=?",
    )
    .bind(user_id)
    .bind(id)
    .fetch_optional(conn)
    .await?
    .ok_or(AppError::NotFound)
}

async fn find_block<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    user_id: &str,
    recipe_id: &str,
    id: &str,
) -> Result<Block> {
    sqlx::query_as(
        "SELECT b.* FROM recipe_blocks b
         JOIN recipes r ON r.id=b.recipe_id
         WHERE b.id=? AND b.recipe_id=? AND r.user_id=?",
    )
    .bind(id)
    .bind(recipe_id)
    .bind(user_id)
    .fetch_optional(conn)
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

/// Inserts a recipe's ingredients and steps, with each step's ingredient
/// lines, in order (the recipe has none yet). Returns the new step block ids
/// in step order.
pub(crate) async fn insert_blocks(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    recipe_id: &str,
    ingredients: &[SnapshotIngredient],
    steps: &[SnapshotStep],
) -> Result<Vec<String>> {
    for (position, ingredient) in ingredients.iter().enumerate() {
        insert_ingredient(tx, recipe_id, position, ingredient).await?;
    }
    let mut step_ids = Vec::with_capacity(steps.len());
    for (position, step) in steps.iter().enumerate() {
        let step_id = insert_step(tx, recipe_id, position, &step.text).await?;
        replace_step_ingredients(tx, &step_id, &step.ingredients.join("\n")).await?;
        step_ids.push(step_id);
    }
    Ok(step_ids)
}

async fn insert_ingredient(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    recipe_id: &str,
    position: usize,
    ingredient: &SnapshotIngredient,
) -> Result<()> {
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit,optional)VALUES(?,?,'ingredient',?,?,?,?,?)")
        .bind(Uuid::new_v4().to_string())
        .bind(recipe_id)
        .bind(position as i64)
        .bind(&ingredient.name)
        .bind(&ingredient.quantity)
        .bind(&ingredient.unit)
        .bind(ingredient.optional)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Inserts one step block, without ingredient lines, and returns its id.
async fn insert_step(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    recipe_id: &str,
    position: usize,
    text: &str,
) -> Result<String> {
    let step_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text)VALUES(?,?,'step',?,?)",
    )
    .bind(&step_id)
    .bind(recipe_id)
    .bind(position as i64)
    .bind(text)
    .execute(&mut **tx)
    .await?;
    Ok(step_id)
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

/// Drops the AI chart (`chart_json`): it was built for the old blocks, so
/// the viewer falls back to a linear flow.
async fn invalidate_chart(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    recipe_id: &str,
) -> Result<()> {
    sqlx::query("UPDATE recipes SET chart_json='' WHERE id=?")
        .bind(recipe_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Stamps the recipe as changed now. Chat proposals (`proposal_base`) and
/// the search index compare this stamp, so a proposal read before the change
/// is refused rather than applied over it, and the recipe is embedded again.
async fn touch_recipe(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, recipe_id: &str) -> Result<()> {
    sqlx::query("UPDATE recipes SET updated_at=? WHERE id=?")
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
    let mut tx = begin_write(db).await?;
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

/// Takes the pool or a transaction (`&mut *tx`), so applying a draft drops
/// its candidates in the same transaction that consumes it.
pub(crate) async fn delete_draft_thumbnails<'e>(
    db: impl sqlx::SqliteExecutor<'e>,
    draft_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM draft_thumbnails WHERE draft_id=?")
        .bind(draft_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Deletes every draft that expired before `now` and returns how many went.
/// Candidate JPEGs go first, in the same transaction, because
/// draft_thumbnails has no foreign key to ai_drafts; the moment pool and the
/// other draft rows cascade with the draft.
pub(crate) async fn delete_expired_drafts(db: &SqlitePool, now: &str) -> Result<u64> {
    let mut tx = begin_write(db).await?;
    sqlx::query("DELETE FROM draft_thumbnails WHERE draft_id IN (SELECT id FROM ai_drafts WHERE expires_at < ?)")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let removed = sqlx::query("DELETE FROM ai_drafts WHERE expires_at < ?")
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(removed)
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
    let recipe = find_recipe(&mut *conn, user_id, recipe_id).await?;
    let (ingredients, steps): (Vec<Block>, Vec<Block>) = blocks(&mut *conn, recipe_id)
        .await?
        .into_iter()
        .partition(|block| block.section == "ingredient");
    let mut uses = step_ingredients_by_step(&mut *conn, recipe_id).await?;
    Ok(RecipeSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        title: recipe.title,
        plate_name: Some(recipe.plate_name),
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

/// Keeps the cook on the view they posted from. The recipe page's forms
/// (blocks, details, photo, step moments, filing) answer with a redirect to
/// the bare `/recipes/{id}`; when that is the page the form was posted from,
/// this puts the page's chart view, step, and servings (`?serves=`) back on
/// it, so an edit does not reset the scaled amounts. The page is htmx's
/// `HX-Current-URL`, or `Referer` for a plain post; without either the
/// redirect stays as it is. Nothing but those three checked values is
/// copied, and only onto the same recipe.
pub(crate) async fn keep_recipe_view(request: Request, next: Next) -> Response {
    let page = (request.method() == Method::POST)
        .then(|| {
            let headers = request.headers();
            headers
                .get("hx-current-url")
                .or_else(|| headers.get(header::REFERER))
        })
        .flatten()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split('#').next())
        .and_then(|value| value.parse::<Uri>().ok());
    let mut response = next.run(request).await;
    let Some(page) = page else {
        return response;
    };
    let target = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .filter(|location| {
            *location == page.path()
                && location
                    .strip_prefix("/recipes/")
                    .is_some_and(|id| !id.is_empty() && !id.contains(['/', '?', '#']))
        })
        .map(|location| {
            format!(
                "{location}{}",
                recipe_view_query(page.query().unwrap_or(""))
            )
        });
    if let Some(target) = target.filter(|target| target.contains('?'))
        && response.status().is_redirection()
        && let Ok(value) = HeaderValue::from_str(&target)
    {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

/// The view parts of a recipe page's query, checked and in a fixed order:
/// `?view=chart&step=N&serves=N` or any part of it, else empty.
fn recipe_view_query(query: &str) -> String {
    let value = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    };
    let mut parts = Vec::new();
    if value("view") == Some("chart") {
        parts.push("view=chart".to_string());
        if let Some(step) = value("step").and_then(|step| step.parse::<usize>().ok()) {
            parts.push(format!("step={step}"));
        }
    }
    if let Some(serves) = value("serves")
        .and_then(|serves| serves.parse::<i64>().ok())
        .filter(|serves| (1..=crate::scale::MAX_SERVINGS).contains(serves))
    {
        parts.push(format!("serves={serves}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

pub(crate) async fn store_recipe_thumbnails(
    db: &SqlitePool,
    recipe_id: &str,
    candidates: &[ThumbCandidate],
) -> Result<()> {
    let mut tx = begin_write(db).await?;
    // The run outlives its recipe when the recipe or its account is deleted
    // meanwhile; the candidates table has no foreign key to stop it.
    let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM recipes WHERE id=?")
        .bind(recipe_id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Ok(());
    }
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

pub(crate) async fn delete_recipe_thumbnails<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    recipe_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM recipe_thumbnail_candidates WHERE recipe_id=?")
        .bind(recipe_id)
        .execute(conn)
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

pub(crate) async fn find_recipe_thumbnail_jpeg<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
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
    .fetch_optional(conn)
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
    let jpeg = normalize_upload_jpeg(&raw, DISH_PHOTO_FILTER, true).await?;
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

/// The dish photo: a square 600 px crop.
const DISH_PHOTO_FILTER: &str = "scale=600:600:force_original_aspect_ratio=increase,crop=600:600";
/// A note photo keeps its shape, at most 1280 px on the longer side (even
/// dimensions for the JPEG encoder).
pub(crate) const NOTE_PHOTO_FILTER: &str = "scale='min(1280,iw)':'min(1280,ih)':force_original_aspect_ratio=decrease,scale=trunc(iw/2)*2:trunc(ih/2)*2";

/// Converts an uploaded image to a JPEG through the local ffmpeg install with
/// `filter`. When ffmpeg produces nothing (not installed, timed out, or the
/// file does not decode), an upload that already is a JPEG is kept as-is if
/// `keep_raw_jpeg` allows it, metadata and all; otherwise it is refused.
pub(crate) async fn normalize_upload_jpeg(
    raw: &[u8],
    filter: &str,
    keep_raw_jpeg: bool,
) -> Result<Vec<u8>> {
    let staged = raw.to_vec();
    let filter = filter.to_string();
    let normalized = tokio::task::spawn_blocking(move || convert_upload(&staged, &filter))
        .await
        .map_err(|_| AppError::Internal("The image converter stopped unexpectedly.".into()))??;
    if let Some(jpeg) = normalized {
        return Ok(jpeg);
    }
    if keep_raw_jpeg && looks_like_jpeg(raw) {
        return Ok(raw.to_vec());
    }
    Err(AppError::BadRequest(
        "That file is not an image this server can process.".into(),
    ))
}

/// A scratch directory that is removed when dropped, whatever happened in it.
struct ScratchDir(std::path::PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Stages an upload in a fresh owner-only directory and runs ffmpeg on it;
/// the directory goes on every outcome. None when ffmpeg is missing or
/// could not read the file.
fn convert_upload(raw: &[u8], filter: &str) -> Result<Option<Vec<u8>>> {
    let staging =
        |error: std::io::Error| AppError::Internal(format!("Could not stage the upload: {error}"));
    let dir = std::env::temp_dir().join(format!("kindle-recipes-upload-{}", Uuid::new_v4()));
    crate::media::create_private_dir(&dir).map_err(staging)?;
    let dir = ScratchDir(dir);
    let source = dir.0.join("in.img");
    let output = dir.0.join("out.jpg");
    std::fs::write(&source, raw).map_err(staging)?;
    let args = vec![
        "-i".into(),
        source.to_string_lossy().to_string(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        filter.to_string(),
        "-q:v".into(),
        "3".into(),
        output.to_string_lossy().to_string(),
    ];
    let converted = crate::media::run_ffmpeg(&args, std::time::Duration::from_secs(60), 64 * 1024);
    Ok(converted
        .ok()
        .and_then(|_| std::fs::read(&output).ok())
        .filter(|bytes| bytes.len() > 2))
}

fn looks_like_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
}

/// Copies the chosen frame candidate onto the recipe and clears the set, in
/// one transaction, so a set a re-pick stores meanwhile is never deleted
/// unseen.
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
    let mut tx = begin_write(&state.db).await?;
    let jpeg = find_recipe_thumbnail_jpeg(&mut *tx, &user.id, &id, idx)
        .await?
        .ok_or(AppError::NotFound)?;
    sqlx::query("UPDATE recipes SET thumbnail_jpeg=? WHERE id=? AND user_id=?")
        .bind(&jpeg)
        .bind(&id)
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    delete_recipe_thumbnails(&mut *tx, &id).await?;
    tx.commit().await?;
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

/// A recipe's place in an in-flight job set (`thumbnail_jobs`,
/// `moment_jobs`), given up when dropped, so a run that ends any way, a
/// panic included, returns the page to idle.
struct JobSlot {
    jobs: Arc<Mutex<HashSet<String>>>,
    recipe_id: String,
}

impl JobSlot {
    /// Takes the recipe's place, or None when a run already holds it.
    fn claim(jobs: &Arc<Mutex<HashSet<String>>>, recipe_id: &str) -> Option<Self> {
        jobs.lock().insert(recipe_id.to_string()).then(|| Self {
            jobs: jobs.clone(),
            recipe_id: recipe_id.to_string(),
        })
    }
}

impl Drop for JobSlot {
    fn drop(&mut self) {
        self.jobs.lock().remove(&self.recipe_id);
    }
}

/// Re-runs only the frame-sampling half of the video pipeline for this
/// recipe's social source. Runs in the background; the dish-photo panel
/// polls (an htmx `every 3s` poller) until the candidate set appears.
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
    let Some(slot) = JobSlot::claim(&state.thumbnail_jobs, &id) else {
        return Ok(redirect_recipe(&id));
    };
    let db = state.db.clone();
    let embedder = state.embedder.clone();
    tokio::spawn(async move {
        let recipe_id = &slot.recipe_id;
        let stored = match crate::media::extract_fresh_thumbnail_candidates(
            &video_url,
            recipe.title,
            embedder,
        )
        .await
        {
            Ok(candidates) => store_recipe_thumbnails(&db, recipe_id, &candidates).await,
            Err(error) => Err(error),
        };
        if let Err(error) = stored {
            tracing::warn!(%recipe_id, %error, "Dish photo frames were not re-picked");
        }
        // A failed or empty run simply returns the page to its idle state.
        drop(slot);
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
    if !crate::step_moments_enabled(&state.db, &user.id).await? {
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
    let Some(slot) = JobSlot::claim(&state.moment_jobs, &id) else {
        return Ok(redirect_recipe(&id));
    };
    let job_state = state.clone();
    let user_id = user.id.clone();
    let recipe_id = id.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let outcome: Result<usize> = async {
            let pool = crate::media::extract_fresh_moment_pool(&video_url, &embedder).await?;
            if pool.vectors.is_empty() {
                return Ok(0);
            }
            store_recipe_moment_pool(&job_state.db, &recipe_id, &pool).await?;
            moments::refresh_recipe(&job_state, &user_id, &recipe_id).await?;
            Ok(pool.vectors.len())
        }
        .await;
        match outcome {
            Ok(0) => tracing::warn!(%recipe_id, "The video gave no step-moment vectors"),
            Ok(vectors) => tracing::info!(%recipe_id, vectors, "Step moments captured"),
            Err(error) => tracing::warn!(%recipe_id, %error, "Step moments were not captured"),
        }
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
    let mut tx = begin_write(db).await?;
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
    let mut tx = begin_write(db).await?;
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

/// The pool a draft's steps are matched against: the moment pool of the
/// video it was imported from.
pub(crate) async fn draft_moment_vectors(
    db: &SqlitePool,
    draft_id: &str,
) -> Result<Vec<MomentVector>> {
    let rows: Vec<MomentVectorRow> = sqlx::query_as(
        "SELECT kind,seconds,vector FROM draft_moment_vectors WHERE draft_id=? ORDER BY seconds",
    )
    .bind(draft_id)
    .fetch_all(db)
    .await?;
    Ok(moment_vectors(rows))
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
    let mut tx = begin_write(db).await?;
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
    let mut tx = begin_write(db).await?;
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

/// Seconds of the stills a draft can show.
pub(crate) async fn draft_still_seconds(db: &SqlitePool, draft_id: &str) -> Result<Vec<u64>> {
    let rows: Vec<i64> = sqlx::query_scalar(
        "SELECT seconds FROM draft_moment_stills WHERE draft_id=? ORDER BY seconds",
    )
    .bind(draft_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|s| s.max(0) as u64).collect())
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

/// One still from the moment pool of a live draft of this user.
pub(crate) async fn find_draft_still(
    db: &SqlitePool,
    user_id: &str,
    draft_id: &str,
    seconds: i64,
) -> Result<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar(
        "SELECT s.jpeg FROM draft_moment_stills s JOIN ai_drafts d ON d.id=s.draft_id
         WHERE d.id=? AND d.user_id=? AND d.expires_at >= ? AND s.seconds=?",
    )
    .bind(draft_id)
    .bind(user_id)
    .bind(Utc::now().to_rfc3339())
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
    let mut tx = begin_write(db).await?;
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

/// One stored recipe-chat row, in thread order.
#[derive(Debug, Clone, FromRow)]
pub(crate) struct ChatMessage {
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) content: String,
    pub(crate) status: String,
    pub(crate) change_label: String,
    pub(crate) change_state: String,
    /// A proposed change (`ChatProposal` JSON), or empty.
    pub(crate) proposal_json: String,
    /// The recipe's `updated_at` when the chat read it for that proposal.
    pub(crate) proposal_base: String,
    pub(crate) created_at: String,
}

/// A user's conversation about one recipe, oldest first. Every chat query is
/// scoped by user id as well as recipe id, so a thread is only ever read by
/// the account that wrote it.
pub(crate) async fn chat_thread(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Result<Vec<ChatMessage>> {
    Ok(sqlx::query_as(
        "SELECT id,role,content,status,change_label,change_state,proposal_json,proposal_base,created_at
         FROM recipe_chat_messages WHERE user_id=? AND recipe_id=? ORDER BY rowid",
    )
    .bind(user_id)
    .bind(recipe_id)
    .fetch_all(db)
    .await?)
}

/// Stores a question and its pending answer together; returns the answer id,
/// or None while another answer in the thread is still running (one answer
/// at a time keeps the thread in order).
pub(crate) async fn start_chat_turn(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    question: &str,
) -> Result<Option<String>> {
    let answer_id = Uuid::new_v4().to_string();
    let now = stamp();
    let mut tx = begin_write(db).await?;
    // The busy check and the write are one statement, so two questions sent
    // at once cannot both start an answer.
    let asked = sqlx::query(
        "INSERT INTO recipe_chat_messages(id,user_id,recipe_id,role,content,created_at)
         SELECT ?1,?2,?3,'user',?4,?5 WHERE NOT EXISTS
           (SELECT 1 FROM recipe_chat_messages
            WHERE user_id=?2 AND recipe_id=?3 AND status='pending')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(recipe_id)
    .bind(question)
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if asked == 0 {
        return Ok(None);
    }
    sqlx::query("INSERT INTO recipe_chat_messages(id,user_id,recipe_id,role,status,created_at)VALUES(?,?,?,'assistant','pending',?)")
        .bind(&answer_id)
        .bind(user_id)
        .bind(recipe_id)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(answer_id))
}

/// A change the chat proposes with an answer: the label, the stored
/// `ChatProposal` JSON, and the recipe's `updated_at` when the chat read it.
pub(crate) struct ProposalRow<'a> {
    pub(crate) label: &'a str,
    pub(crate) json: &'a str,
    pub(crate) base: &'a str,
}

/// What a finished answer did about changing the recipe.
pub(crate) enum AnswerProposal<'a> {
    /// It proposed no change.
    None,
    /// It proposed one that `chat::valid_proposal` refused; the drawer says
    /// the change was not made.
    Refused,
    /// It proposed one that awaits the cook's Accept or Deny.
    Ready(ProposalRow<'a>),
}

/// Fills in a pending answer, with what it did about a change. An answer
/// whose row is gone (the chat was cleared while the model ran) is dropped.
pub(crate) async fn finish_chat_answer(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    answer_id: &str,
    reply: &str,
    proposal: AnswerProposal<'_>,
) -> Result<()> {
    let (label, json, base, change_state) = match &proposal {
        AnswerProposal::Ready(row) => (row.label, row.json, row.base, "ready"),
        AnswerProposal::Refused => ("", "", "", "failed"),
        AnswerProposal::None => ("", "", "", ""),
    };
    sqlx::query(
        "UPDATE recipe_chat_messages SET content=?,status='done',change_label=?,change_state=?,proposal_json=?,proposal_base=?
         WHERE id=? AND user_id=? AND recipe_id=? AND status='pending'",
    )
    .bind(reply)
    .bind(label)
    .bind(change_state)
    .bind(json)
    .bind(base)
    .bind(answer_id)
    .bind(user_id)
    .bind(recipe_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Marks a pending answer as failed with the text to show in its place.
/// `status` is "failed", or "needs_settings" when the fix is in Settings.
pub(crate) async fn fail_chat_answer(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    answer_id: &str,
    status: &str,
    message: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE recipe_chat_messages SET content=?,status=?
         WHERE id=? AND user_id=? AND recipe_id=? AND status='pending'",
    )
    .bind(message)
    .bind(status)
    .bind(answer_id)
    .bind(user_id)
    .bind(recipe_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Deletes the user's whole conversation about a recipe, proposals included.
pub(crate) async fn clear_chat(db: &SqlitePool, user_id: &str, recipe_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM recipe_chat_messages WHERE user_id=? AND recipe_id=?")
        .bind(user_id)
        .bind(recipe_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Answers that were running when the app stopped can never finish; at
/// startup they become failures the cook can ask again about.
pub(crate) async fn fail_interrupted_chats(db: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE recipe_chat_messages SET status='failed',content='The answer was interrupted. Ask again.'
         WHERE status='pending'",
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Denies a proposal that awaits the cook; false when there was none.
pub(crate) async fn decline_chat_proposal(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    answer_id: &str,
) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE recipe_chat_messages SET change_state='discarded'
         WHERE id=? AND user_id=? AND recipe_id=? AND change_state='ready'",
    )
    .bind(answer_id)
    .bind(user_id)
    .bind(recipe_id)
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

/// Moves a section's rows to negative positions, keeping their order, so
/// they can be renumbered without tripping `UNIQUE(recipe_id, section,
/// position)`.
async fn park_section(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    recipe_id: &str,
    section: &str,
) -> Result<()> {
    sqlx::query("UPDATE recipe_blocks SET position=-1-position WHERE recipe_id=? AND section=?")
        .bind(recipe_id)
        .bind(section)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// What accepting a proposal did.
pub(crate) enum AcceptOutcome {
    /// Saved, with its divider and version.
    Applied,
    /// The recipe changed after the chat read it, so the proposal no longer
    /// describes it.
    Stale,
    /// The proposal was decided (or cleared) meanwhile.
    Gone,
}

/// Applies an accepted proposal in one transaction: the details, then each
/// ingredient and step. An item the proposal kept (`from`) keeps its stored
/// row, and with it its step moments; removed items are deleted and new ones
/// inserted. Refuses (Stale) when the recipe changed since the chat read it.
/// The chat's thread gets its divider and the history a `Chat: <label>`
/// version; a changed ingredient list or method drops the AI chart like any
/// other block edit.
pub(crate) async fn accept_chat_proposal(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    answer_id: &str,
) -> Result<AcceptOutcome> {
    let mut tx = begin_write(db).await?;
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT change_label,proposal_json,proposal_base FROM recipe_chat_messages
         WHERE id=? AND user_id=? AND recipe_id=? AND role='assistant'
           AND change_state='ready' AND proposal_json<>''",
    )
    .bind(answer_id)
    .bind(user_id)
    .bind(recipe_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((model_label, json, base)) = row else {
        return Ok(AcceptOutcome::Gone);
    };
    let ChatProposal { before, after } = serde_json::from_str(&json)
        .map_err(|error| AppError::Internal(format!("Could not read a chat proposal: {error}")))?;
    let recipe = find_recipe(&mut *tx, user_id, recipe_id).await?;
    let (ingredients, steps): (Vec<Block>, Vec<Block>) = blocks(&mut *tx, recipe_id)
        .await?
        .into_iter()
        .partition(|block| block.section == "ingredient");
    if recipe.updated_at != base
        || ingredients.len() != before.ingredients.len()
        || steps.len() != before.steps.len()
    {
        return Ok(AcceptOutcome::Stale);
    }
    ensure_recipe_history(&mut tx, user_id, recipe_id).await?;
    // The chat never names the plate: a new title clears the old name, so the
    // plate falls back to the new title's initial.
    let plate_name = if after.title.trim() == recipe.title {
        recipe.plate_name.as_str()
    } else {
        ""
    };
    sqlx::query("UPDATE recipes SET title=?,plate_name=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,updated_at=? WHERE user_id=? AND id=?")
        .bind(after.title.trim())
        .bind(plate_name)
        .bind(after.description.trim())
        .bind(after.servings)
        .bind(after.prep_minutes)
        .bind(after.cook_minutes)
        .bind(stamp())
        .bind(user_id)
        .bind(recipe_id)
        .execute(&mut *tx)
        .await?;
    // Ingredients: keep, update, and reorder the rows the proposal kept.
    let kept: HashSet<usize> = after
        .ingredients
        .iter()
        .filter_map(|item| item.from)
        .collect();
    for (index, block) in ingredients.iter().enumerate() {
        if !kept.contains(&index) {
            sqlx::query("DELETE FROM recipe_blocks WHERE id=?")
                .bind(&block.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    // Positions are unique per section, so the kept rows step aside (to
    // negative positions) before any takes its new place.
    park_section(&mut tx, recipe_id, "ingredient").await?;
    let mut placed = HashSet::new();
    for (position, item) in after.ingredients.iter().enumerate() {
        let existing = item
            .from
            .filter(|from| placed.insert(*from))
            .and_then(|from| ingredients.get(from));
        match existing {
            Some(block) => {
                sqlx::query("UPDATE recipe_blocks SET position=?,text=?,quantity=?,unit=?,optional=? WHERE id=?")
                    .bind(position as i64)
                    .bind(item.name.trim())
                    .bind(item.quantity.trim())
                    .bind(item.unit.trim())
                    .bind(item.optional)
                    .bind(&block.id)
                    .execute(&mut *tx)
                    .await?;
            }
            None => {
                let ingredient = SnapshotIngredient {
                    name: item.name.trim().to_string(),
                    quantity: item.quantity.trim().to_string(),
                    unit: item.unit.trim().to_string(),
                    optional: item.optional,
                };
                insert_ingredient(&mut tx, recipe_id, position, &ingredient).await?;
            }
        }
    }
    // Steps the same way, with their ingredient lines.
    let kept: HashSet<usize> = after.steps.iter().filter_map(|item| item.from).collect();
    for (index, block) in steps.iter().enumerate() {
        if !kept.contains(&index) {
            sqlx::query("DELETE FROM recipe_blocks WHERE id=?")
                .bind(&block.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    // Positions are unique per section, so the kept rows step aside (to
    // negative positions) before any takes its new place.
    park_section(&mut tx, recipe_id, "step").await?;
    let mut placed = HashSet::new();
    for (position, item) in after.steps.iter().enumerate() {
        let existing = item
            .from
            .filter(|from| placed.insert(*from))
            .and_then(|from| steps.get(from));
        let step_id = match existing {
            Some(block) => {
                sqlx::query("UPDATE recipe_blocks SET position=?,text=? WHERE id=?")
                    .bind(position as i64)
                    .bind(item.text.trim())
                    .bind(&block.id)
                    .execute(&mut *tx)
                    .await?;
                block.id.clone()
            }
            None => insert_step(&mut tx, recipe_id, position, item.text.trim()).await?,
        };
        replace_step_ingredients(&mut tx, &step_id, &item.ingredients.join("\n")).await?;
    }
    let proposed = after.snapshot();
    if before.ingredients != proposed.ingredients || before.steps != proposed.steps {
        invalidate_chart(&mut tx, recipe_id).await?;
    }
    let saved = recipe_snapshot(&mut tx, user_id, recipe_id).await?;
    let label = crate::chat::divider_label(&model_label, &before, &saved);
    sqlx::query("INSERT INTO recipe_chat_messages(id,user_id,recipe_id,role,content,created_at)VALUES(?,?,?,'change',?,?)")
        .bind(Uuid::new_v4().to_string())
        .bind(user_id)
        .bind(recipe_id)
        .bind(&label)
        .bind(stamp())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE recipe_chat_messages SET change_state='applied' WHERE id=? AND user_id=? AND recipe_id=?",
    )
    .bind(answer_id)
    .bind(user_id)
    .bind(recipe_id)
    .execute(&mut *tx)
    .await?;
    record_version(&mut tx, user_id, recipe_id, &format!("Chat: {label}")).await?;
    tx.commit().await?;
    Ok(AcceptOutcome::Applied)
}

/// One note on a recipe, without its photo bytes.
#[derive(Debug, Clone, FromRow)]
pub(crate) struct RecipeNote {
    pub(crate) id: String,
    pub(crate) version: Option<i64>,
    pub(crate) body: String,
    pub(crate) has_photo: bool,
    pub(crate) chat_message_id: Option<String>,
    pub(crate) created_at: String,
}

/// The user's notes on a recipe, newest first. Like the chat, every note
/// query is scoped by user id as well as recipe id.
pub(crate) async fn recipe_notes(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Result<Vec<RecipeNote>> {
    Ok(sqlx::query_as(
        "SELECT id,version,body,photo_jpeg IS NOT NULL AS has_photo,chat_message_id,created_at
         FROM recipe_notes WHERE user_id=? AND recipe_id=? ORDER BY created_at DESC, rowid DESC",
    )
    .bind(user_id)
    .bind(recipe_id)
    .fetch_all(db)
    .await?)
}

pub(crate) async fn find_note(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    note_id: &str,
) -> Result<RecipeNote> {
    sqlx::query_as(
        "SELECT id,version,body,photo_jpeg IS NOT NULL AS has_photo,chat_message_id,created_at
         FROM recipe_notes WHERE id=? AND user_id=? AND recipe_id=?",
    )
    .bind(note_id)
    .bind(user_id)
    .bind(recipe_id)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

/// Stores a note. The caller has checked the recipe is the user's and that
/// the note has text or a photo. A chat answer is kept as a note only once:
/// when it already is one, nothing is stored.
pub(crate) async fn insert_note(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    version: Option<i64>,
    body: &str,
    photo: Option<&[u8]>,
    chat_message_id: Option<&str>,
) -> Result<()> {
    // The check and the write are one statement, so keeping an answer twice
    // at once stores one note. A NULL chat_message_id matches no row.
    sqlx::query(
        "INSERT INTO recipe_notes(id,user_id,recipe_id,version,body,photo_jpeg,chat_message_id,created_at,updated_at)
         SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?8 WHERE NOT EXISTS
           (SELECT 1 FROM recipe_notes WHERE user_id=?2 AND recipe_id=?3 AND chat_message_id=?7)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(recipe_id)
    .bind(version)
    .bind(body)
    .bind(photo)
    .bind(chat_message_id)
    .bind(stamp())
    .execute(db)
    .await?;
    Ok(())
}

/// Changes a note's text and the version it is about.
pub(crate) async fn update_note(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    note_id: &str,
    body: &str,
    version: Option<i64>,
) -> Result<()> {
    sqlx::query("UPDATE recipe_notes SET body=?,version=?,updated_at=? WHERE id=? AND user_id=? AND recipe_id=?")
        .bind(body)
        .bind(version)
        .bind(stamp())
        .bind(note_id)
        .bind(user_id)
        .bind(recipe_id)
        .execute(db)
        .await?;
    Ok(())
}

pub(crate) async fn delete_note(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    note_id: &str,
) -> Result<()> {
    let removed = sqlx::query("DELETE FROM recipe_notes WHERE id=? AND user_id=? AND recipe_id=?")
        .bind(note_id)
        .bind(user_id)
        .bind(recipe_id)
        .execute(db)
        .await?
        .rows_affected();
    if removed == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// A note's photo, served only to the note's owner.
pub(crate) async fn note_photo_jpeg(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    note_id: &str,
) -> Result<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar(
        "SELECT photo_jpeg FROM recipe_notes WHERE id=? AND user_id=? AND recipe_id=? AND photo_jpeg IS NOT NULL",
    )
    .bind(note_id)
    .bind(user_id)
    .bind(recipe_id)
    .fetch_optional(db)
    .await?)
}

/// A note as the chat's tools read it, photo bytes included.
#[derive(Debug, Clone, FromRow)]
pub(crate) struct ChatNoteRow {
    pub(crate) version: Option<i64>,
    pub(crate) body: String,
    pub(crate) photo_jpeg: Option<Vec<u8>>,
    pub(crate) created_at: String,
}

/// The newest `limit` notes on a recipe, for the chat.
pub(crate) async fn chat_notes(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    limit: i64,
) -> Result<Vec<ChatNoteRow>> {
    Ok(sqlx::query_as(
        "SELECT version,body,photo_jpeg,created_at FROM recipe_notes
         WHERE user_id=? AND recipe_id=? ORDER BY created_at DESC, rowid DESC LIMIT ?",
    )
    .bind(user_id)
    .bind(recipe_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// The chat answers in a thread that were kept as notes.
pub(crate) async fn kept_chat_answers(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Result<std::collections::HashSet<String>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT chat_message_id FROM recipe_notes
         WHERE user_id=? AND recipe_id=? AND chat_message_id IS NOT NULL",
    )
    .bind(user_id)
    .bind(recipe_id)
    .fetch_all(db)
    .await?;
    Ok(ids.into_iter().collect())
}

/// The version matching the recipe as it is now. Every edit records a
/// version and the newest cannot be deleted (`delete_version`), so that is
/// the newest one; a recipe with no history yet gets its "Original recipe"
/// version first.
pub(crate) async fn current_version(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Result<i64> {
    let mut tx = begin_write(db).await?;
    find_recipe(&mut *tx, user_id, recipe_id).await?;
    ensure_recipe_history(&mut tx, user_id, recipe_id).await?;
    let version: i64 =
        sqlx::query_scalar("SELECT MAX(version) FROM recipe_versions WHERE recipe_id=?")
            .bind(recipe_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(version)
}

/// A finished chat answer's text, if it is the user's and on this recipe.
pub(crate) async fn chat_answer_text(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
    answer_id: &str,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT content FROM recipe_chat_messages
         WHERE id=? AND user_id=? AND recipe_id=? AND role='assistant' AND status='done'",
    )
    .bind(answer_id)
    .bind(user_id)
    .bind(recipe_id)
    .fetch_optional(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_job_that_panics_still_gives_up_its_place() {
        let jobs = Arc::new(Mutex::new(HashSet::new()));
        let slot = JobSlot::claim(&jobs, "r").unwrap();
        assert!(JobSlot::claim(&jobs, "r").is_none(), "one run per recipe");
        let run = tokio::spawn(async move {
            let _slot = slot;
            panic!("the run failed");
        });
        assert!(run.await.is_err());
        assert!(jobs.lock().is_empty(), "the page is back to idle");
    }

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
