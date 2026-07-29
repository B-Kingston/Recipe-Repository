use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{Duration, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::HashSet, env, net::SocketAddr, str::FromStr, sync::Arc,
    time::Duration as StdDuration,
};
use thiserror::Error;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{error, info};
use uuid::Uuid;

const DRAFT_HOURS: i64 = 24;
const RECIPE_PROMPT: &str = r#"You are a recipe assistant. Prefer credible guidance and synthesize practical advice without copying source text. Return one complete recipe, not a patch. Every ingredient must be used exactly through one or more ingredientUses. Each step has a concise chartLabel (2–6 words), a timerSeconds value (0 when untimed; use the midpoint of a range), ingredientUses containing canonical zero-based ingredient indices plus exact amounts used at that step, and inputSteps containing only earlier step indices whose outputs are combined. Give every non-final step at most one consumer; make every step flow into the final step. Ingredient-free preparation steps are allowed. Give heating, simmering, baking, frying, resting, chilling, and reducing steps realistic duration and a clear doneness cue. Keep total timings consistent. Be concise and practical for a home cook."#;
const GROUNDED_RECIPE_PROMPT: &str = r#"You are a meticulous recipe research assistant. Research before generating. Always use Google Search. When the user supplies one or more URLs, also use URL Context to read every accessible page directly. When the user names an existing recipe, cook, book, restaurant, or website, locate the likely original and cross-check it against at least two independent, credible sources when available.

Study ingredient ratios, ingredient roles, preparation, sequencing, temperatures, timings, texture cues, and the intended final result. Recreate the referenced recipe faithfully enough to achieve that result, while resolving omissions or contradictions with sound cooking knowledge. Do not copy expressive prose, headnotes, or distinctive wording; write original, concise instructions. Preserve attribution through citations rather than imitation of the author's voice.

Return one complete recipe, not a patch. Every ingredient must be used exactly through one or more ingredientUses. Each step has a concise chartLabel (2–6 words), a timerSeconds value (0 when untimed; use the midpoint of a range), ingredientUses containing canonical zero-based ingredient indices plus exact amounts used at that step, and inputSteps containing only earlier step indices whose outputs are combined. Give every non-final step at most one consumer; make every step flow into the final step. Ingredient-free preparation steps are allowed. Give heating, simmering, baking, frying, resting, chilling, and reducing steps realistic duration and a clear doneness cue. Keep total timings consistent. Cite every source that materially informed the recipe."#;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    http: reqwest::Client,
    api_key: String,
    model: String,
    gemini_base_url: String,
    search_grounding: bool,
}

#[derive(Debug, Error)]
enum AppError {
    #[error("That page was not found.")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("The AI service is not configured. Add GEMINI_API_KEY to .env.")]
    AiNotConfigured,
    #[error("The AI service could not prepare a grounded recipe. Please try again.")]
    Ai,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Template(#[from] askama::Error),
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        error!(error = %self, "request failed");
        (status, Html(format!("<!doctype html><main style=\"font:18px Georgia;margin:2rem\"><h1>Something needs attention</h1><p>{}</p><p><a href=\"/\">Back to recipes</a></p></main>", html_escape(&self.to_string())))).into_response()
    }
}
type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Clone, FromRow)]
struct Recipe {
    id: String,
    title: String,
    description: String,
    servings: Option<i64>,
    prep_minutes: Option<i64>,
    cook_minutes: Option<i64>,
    chart_json: String,
    created_at: String,
    updated_at: String,
}
#[derive(Debug, Clone, FromRow)]
struct Block {
    id: String,
    recipe_id: String,
    section: String,
    position: i64,
    text: String,
    quantity: String,
    unit: String,
    optional: i64,
}
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
struct Source {
    id: Option<String>,
    recipe_id: Option<String>,
    position: Option<i64>,
    title: String,
    url: String,
}
impl Block {
    fn optional(&self) -> bool {
        self.optional != 0
    }
}

#[derive(Clone)]
struct ViewBlock {
    id: String,
    position: i64,
    text: String,
    quantity: String,
    unit: String,
    optional: bool,
    editing: bool,
}
impl ViewBlock {
    fn from_block(b: Block, edit: Option<&str>) -> Self {
        let optional = b.optional();
        let editing = edit == Some(&b.id);
        Self {
            id: b.id,
            position: b.position,
            text: b.text,
            quantity: b.quantity,
            unit: b.unit,
            optional,
            editing,
        }
    }
}
#[derive(Clone)]
struct ViewStep {
    block: ViewBlock,
    ingredients: Vec<String>,
    ingredients_text: String,
}
#[derive(Clone)]
struct ChartCell {
    step: usize,
    label: String,
    row: usize,
    span: usize,
    active: bool,
    dimmed: bool,
    href: String,
}
#[derive(Clone)]
struct ChartLeaf {
    label: String,
    row: usize,
    active: bool,
    dimmed: bool,
}
#[derive(Clone)]
struct ChartDetail {
    step: usize,
    text: String,
    additions: Vec<String>,
    timer_seconds: i64,
    previous_href: String,
    next_href: String,
    has_previous: bool,
    has_next: bool,
}
#[derive(Clone)]
struct ChartView {
    cells: Vec<ChartCell>,
    leaves: Vec<ChartLeaf>,
    unlinked: Vec<String>,
    detail: Option<ChartDetail>,
    step_count: usize,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    recipes: Vec<Recipe>,
}
#[derive(Template)]
#[template(path = "recipe.html")]
struct RecipeTemplate {
    recipe: Recipe,
    ingredients: Vec<ViewBlock>,
    steps: Vec<ViewStep>,
    sources: Vec<Source>,
    chart: ChartView,
    chart_view: bool,
    edit_meta: bool,
    servings_value: String,
    prep_value: String,
    cook_value: String,
    servings_display: String,
    prep_display: String,
    cook_display: String,
}
#[derive(Template)]
#[template(path = "delete.html")]
struct DeleteTemplate {
    recipe: Recipe,
}
#[derive(Template)]
#[template(path = "ai_form.html")]
struct AiFormTemplate {
    heading: String,
    guidance: String,
    action: String,
    label: String,
    button: String,
    cancel_url: String,
    error: String,
    prompt: String,
}
#[derive(Template)]
#[template(path = "draft.html")]
struct DraftTemplate {
    id: String,
    recipe: GeneratedRecipe,
    sources: Vec<Source>,
    suggestions: String,
}
fn render(t: impl Template) -> Result<Html<String>> {
    Ok(Html(t.render()?))
}

#[derive(Deserialize)]
struct RecipeForm {
    title: String,
    description: String,
    servings: String,
    prep_minutes: String,
    cook_minutes: String,
}
#[derive(Deserialize)]
struct BlockForm {
    section: Option<String>,
    text: String,
    quantity: Option<String>,
    unit: Option<String>,
    optional: Option<String>,
    step_ingredients: Option<String>,
}
#[derive(Deserialize)]
struct PromptForm {
    prompt: String,
}
#[derive(Deserialize)]
struct RecipeQuery {
    edit: Option<String>,
    edit_meta: Option<String>,
    view: Option<String>,
    step: Option<usize>,
}

#[tokio::main]
async fn main() -> anyhowless::Result<()> {
    dotenvy::dotenv().ok();
    if env::args().any(|arg| arg == "--healthcheck") {
        return healthcheck().await;
    }
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "kindle_recipes=info".into()))
        .init();
    let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:recipes.sqlite3".into());
    let options = SqliteConnectOptions::from_str(&database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal);
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&db).await?;
    let state = Arc::new(AppState {
        db,
        http: reqwest::Client::builder()
            .timeout(StdDuration::from_secs(55))
            .build()?,
        api_key: env::var("GEMINI_API_KEY").unwrap_or_default(),
        model: env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-3.6-flash".into()),
        gemini_base_url: env::var("GEMINI_BASE_URL").unwrap_or_else(|_| {
            "https://generativelanguage.googleapis.com/v1beta/interactions".into()
        }),
        search_grounding: env_bool("GEMINI_SEARCH_GROUNDING", true),
    });
    let app = routes(state).layer(TraceLayer::new_for_http());
    let addr: SocketAddr = env::var("APP_BIND")
        .unwrap_or_else(|_| "0.0.0.0:3000".into())
        .parse()?;
    info!(%addr, "Kindle Recipes listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
async fn healthcheck() -> anyhowless::Result<()> {
    let bind = env::var("APP_BIND").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let port = bind
        .rsplit_once(':')
        .map(|(_, port)| port)
        .unwrap_or("3000");
    let response = reqwest::get(format!("http://127.0.0.1:{port}/healthz")).await?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(std::io::Error::other("recipe server health check failed").into())
    }
}
fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/healthz", get(|| async { "ok" }))
        .route("/recipes/new", get(new_recipe))
        .route("/recipes/{id}", get(recipe_page).post(update_recipe))
        .route("/recipes/{id}/delete", get(delete_page).post(delete_recipe))
        .route("/recipes/{id}/blocks", post(add_block))
        .route("/recipes/{id}/blocks/{block_id}", post(update_block))
        .route(
            "/recipes/{id}/blocks/{block_id}/move/{direction}",
            post(move_block),
        )
        .route("/recipes/{id}/blocks/{block_id}/delete", post(delete_block))
        .route("/ai/generate", get(generate_page).post(generate_recipe))
        .route("/recipes/{id}/ai/alter", get(alter_page).post(alter_recipe))
        .route("/ai/drafts/{id}", get(draft_page))
        .route("/ai/drafts/{id}/apply", post(apply_draft))
        .route("/ai/drafts/{id}/cancel", post(cancel_draft))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state)
}

async fn home(State(s): State<Arc<AppState>>) -> Result<Html<String>> {
    render(HomeTemplate {
        recipes: sqlx::query_as("SELECT * FROM recipes ORDER BY updated_at DESC")
            .fetch_all(&s.db)
            .await?,
    })
}
async fn new_recipe(State(s): State<Arc<AppState>>) -> Result<Html<String>> {
    render(ai_form(
        "New Recipe",
        generate_guidance(s.search_grounding),
        "/ai/generate",
        "What should this recipe be based on?",
        "Research & generate",
        "/",
        "",
        "",
    ))
}
async fn recipe_page(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<RecipeQuery>,
) -> Result<Html<String>> {
    let recipe = find_recipe(&s.db, &id).await?;
    let blocks = blocks(&s.db, &id).await?;
    let edit = q.edit.as_deref();
    let ingredient_blocks: Vec<Block> = blocks
        .iter()
        .filter(|b| b.section == "ingredient")
        .cloned()
        .collect();
    let ingredients = ingredient_blocks
        .iter()
        .cloned()
        .map(|b| ViewBlock::from_block(b, edit))
        .collect();
    let mut steps = Vec::new();
    for block in blocks.iter().filter(|b| b.section == "step").cloned() {
        let mut used = step_ingredients(&s.db, &block.id).await?;
        if used.is_empty() {
            used = infer_step_ingredients(&block.text, &ingredient_blocks);
        }
        steps.push(ViewStep {
            block: ViewBlock::from_block(block, edit),
            ingredients_text: used.join("\n"),
            ingredients: used,
        });
    }
    let chart_view = q.view.as_deref() == Some("chart");
    let chart = build_chart(&recipe, &ingredient_blocks, &steps, q.step);
    render(RecipeTemplate {
        servings_value: option_number(recipe.servings),
        prep_value: option_number(recipe.prep_minutes),
        cook_value: option_number(recipe.cook_minutes),
        servings_display: option_number(recipe.servings),
        prep_display: option_number(recipe.prep_minutes),
        cook_display: option_number(recipe.cook_minutes),
        recipe,
        ingredients,
        steps,
        sources: sources(&s.db, &id).await?,
        chart,
        chart_view,
        edit_meta: q.edit_meta.is_some(),
    })
}
async fn update_recipe(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Form(f): Form<RecipeForm>,
) -> Result<Response> {
    find_recipe(&s.db, &id).await?;
    sqlx::query("UPDATE recipes SET title=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,updated_at=? WHERE id=?").bind(required(&f.title, "Recipe title")?).bind(trim(&f.description)).bind(number(&f.servings)?).bind(number(&f.prep_minutes)?).bind(number(&f.cook_minutes)?).bind(stamp()).bind(&id).execute(&s.db).await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}
async fn delete_page(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    render(DeleteTemplate {
        recipe: find_recipe(&s.db, &id).await?,
    })
}
async fn delete_recipe(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Result<Response> {
    let r = sqlx::query("DELETE FROM recipes WHERE id=?")
        .bind(id)
        .execute(&s.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to("/").into_response())
}

async fn add_block(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Form(f): Form<BlockForm>,
) -> Result<Response> {
    find_recipe(&s.db, &id).await?;
    let section = f
        .section
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("Block section is required.".into()))?;
    if !matches!(section, "ingredient" | "step") {
        return Err(AppError::BadRequest("Invalid block section.".into()));
    };
    let text = required(&f.text, "Block text")?;
    let mut tx = s.db.begin().await?;
    let position: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position)+1,0) FROM recipe_blocks WHERE recipe_id=? AND section=?",
    )
    .bind(&id)
    .bind(section)
    .fetch_one(&mut *tx)
    .await?;
    let block_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit,optional) VALUES(?,?,?,?,?,?,?,?)").bind(&block_id).bind(&id).bind(section).bind(position).bind(text).bind(trim(f.quantity.as_deref().unwrap_or(""))).bind(trim(f.unit.as_deref().unwrap_or(""))).bind(if f.optional.is_some(){1}else{0}).execute(&mut *tx).await?;
    if section == "step" {
        replace_step_ingredients(
            &mut tx,
            &block_id,
            f.step_ingredients.as_deref().unwrap_or(""),
        )
        .await?;
    }
    clear_chart(&mut tx, &id).await?;
    touch(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}
async fn update_block(
    State(s): State<Arc<AppState>>,
    Path((id, block_id)): Path<(String, String)>,
    Form(f): Form<BlockForm>,
) -> Result<Response> {
    let b = find_block(&s.db, &id, &block_id).await?;
    let text = required(&f.text, "Block text")?;
    let mut tx = s.db.begin().await?;
    sqlx::query(
        "UPDATE recipe_blocks SET text=?,quantity=?,unit=?,optional=? WHERE id=? AND recipe_id=?",
    )
    .bind(text)
    .bind(if b.section == "ingredient" {
        trim(f.quantity.as_deref().unwrap_or(""))
    } else {
        String::new()
    })
    .bind(if b.section == "ingredient" {
        trim(f.unit.as_deref().unwrap_or(""))
    } else {
        String::new()
    })
    .bind(if f.optional.is_some() { 1 } else { 0 })
    .bind(&block_id)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if b.section == "step" {
        replace_step_ingredients(
            &mut tx,
            &block_id,
            f.step_ingredients.as_deref().unwrap_or(""),
        )
        .await?;
    }
    clear_chart(&mut tx, &id).await?;
    touch(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}
async fn move_block(
    State(s): State<Arc<AppState>>,
    Path((id, block_id, direction)): Path<(String, String, String)>,
) -> Result<Response> {
    let b = find_block(&s.db, &id, &block_id).await?;
    let delta = if direction == "up" {
        -1
    } else if direction == "down" {
        1
    } else {
        return Err(AppError::BadRequest("Invalid movement.".into()));
    };
    let target = b.position + delta;
    if target < 0 {
        return Ok(Redirect::to(&format!("/recipes/{id}")).into_response());
    };
    let mut tx = s.db.begin().await?;
    let neighbor: Option<String> = sqlx::query_scalar(
        "SELECT id FROM recipe_blocks WHERE recipe_id=? AND section=? AND position=?",
    )
    .bind(&id)
    .bind(&b.section)
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(other) = neighbor {
        sqlx::query("UPDATE recipe_blocks SET position=-1 WHERE id=?")
            .bind(&block_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE recipe_blocks SET position=? WHERE id=?")
            .bind(b.position)
            .bind(other)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE recipe_blocks SET position=? WHERE id=?")
            .bind(target)
            .bind(&block_id)
            .execute(&mut *tx)
            .await?;
        clear_chart(&mut tx, &id).await?;
        touch(&mut tx, &id).await?;
    }
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}
async fn delete_block(
    State(s): State<Arc<AppState>>,
    Path((id, block_id)): Path<(String, String)>,
) -> Result<Response> {
    let b = find_block(&s.db, &id, &block_id).await?;
    let mut tx = s.db.begin().await?;
    sqlx::query("DELETE FROM recipe_blocks WHERE id=?")
        .bind(&block_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE recipe_blocks SET position=position-1 WHERE recipe_id=? AND section=? AND position>? ").bind(&id).bind(&b.section).bind(b.position).execute(&mut *tx).await?;
    clear_chart(&mut tx, &id).await?;
    touch(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}

async fn generate_page() -> Redirect {
    Redirect::to("/recipes/new")
}
async fn generate_recipe(
    State(s): State<Arc<AppState>>,
    Form(f): Form<PromptForm>,
) -> Result<Response> {
    let prompt = required(&f.prompt, "Recipe idea or URL")?;
    match create_draft(&s, None, "generate", &prompt).await {
        Ok(id) => Ok(Redirect::to(&format!("/ai/drafts/{id}")).into_response()),
        Err(e) => render(ai_form(
            "New Recipe",
            generate_guidance(s.search_grounding),
            "/ai/generate",
            "What should this recipe be based on?",
            "Research & generate",
            "/",
            &e.to_string(),
            &prompt,
        ))
        .map(IntoResponse::into_response),
    }
}
async fn alter_page(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    let r = find_recipe(&s.db, &id).await?;
    render(ai_form(
        "Alter with AI",
        &format!(
            "Tell Gemini how to change “{}”. It will return a complete replacement recipe for review.",
            r.title
        ),
        &format!("/recipes/{id}/ai/alter"),
        "What should change?",
        "Create altered draft",
        &format!("/recipes/{id}"),
        "",
        "",
    ))
}
async fn alter_recipe(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Form(f): Form<PromptForm>,
) -> Result<Response> {
    let r = find_recipe(&s.db, &id).await?;
    let prompt = required(&f.prompt, "Comments")?;
    let snapshot = recipe_snapshot(&s.db, &r).await?;
    let full = format!(
        "User requested changes:\n{}\n\nCurrent recipe JSON:\n{}",
        prompt,
        serde_json::to_string(&snapshot).unwrap()
    );
    match create_draft(&s, Some(&r), "alter", &full).await {
        Ok(draft) => Ok(Redirect::to(&format!("/ai/drafts/{draft}")).into_response()),
        Err(e) => render(ai_form(
            "Alter with AI",
            "Tell Gemini what should change.",
            &format!("/recipes/{id}/ai/alter"),
            "What should change?",
            "Create altered draft",
            &format!("/recipes/{id}"),
            &e.to_string(),
            &prompt,
        ))
        .map(IntoResponse::into_response),
    }
}

async fn draft_page(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    let d = find_draft(&s.db, &id).await?;
    let mut recipe: GeneratedRecipe =
        serde_json::from_str(&d.recipe_json).map_err(|_| AppError::Ai)?;
    normalize_generated(&mut recipe)?;
    render(DraftTemplate {
        id,
        recipe,
        sources: serde_json::from_str(&d.sources_json).map_err(|_| AppError::Ai)?,
        suggestions: d.search_suggestions,
    })
}
async fn apply_draft(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Result<Response> {
    let d = find_draft(&s.db, &id).await?;
    let mut g: GeneratedRecipe = serde_json::from_str(&d.recipe_json).map_err(|_| AppError::Ai)?;
    normalize_generated(&mut g)?;
    let ss: Vec<Source> = serde_json::from_str(&d.sources_json).map_err(|_| AppError::Ai)?;
    let chart_json =
        serde_json::to_string(&ChartRecipe::from_generated(&g)).map_err(|_| AppError::Ai)?;
    let mut tx = s.db.begin().await?;
    let now = stamp();
    let recipe_id = if let Some(existing) = d.recipe_id {
        if d.base_updated_at.as_deref() != Some(&find_recipe(&s.db, &existing).await?.updated_at) {
            return Err(AppError::BadRequest(
                "This recipe changed after the draft was made. Generate a new alteration.".into(),
            ));
        };
        sqlx::query("UPDATE recipes SET title=?,description=?,servings=?,prep_minutes=?,cook_minutes=?,chart_json=?,updated_at=? WHERE id=?").bind(&g.title).bind(&g.description).bind(g.servings).bind(g.prep_minutes).bind(g.cook_minutes).bind(&chart_json).bind(&now).bind(&existing).execute(&mut *tx).await?;
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
        let rid = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO recipes(id,title,description,servings,prep_minutes,cook_minutes,chart_json,created_at,updated_at)VALUES(?,?,?,?,?,?,?,?,?)").bind(&rid).bind(&g.title).bind(&g.description).bind(g.servings).bind(g.prep_minutes).bind(g.cook_minutes).bind(&chart_json).bind(&now).bind(&now).execute(&mut *tx).await?;
        rid
    };
    for (position, i) in g.ingredients.iter().enumerate() {
        sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit,optional)VALUES(?,?,?,?,?,?,?,?)").bind(Uuid::new_v4().to_string()).bind(&recipe_id).bind("ingredient").bind(position as i64).bind(&i.name).bind(&i.quantity).bind(&i.unit).bind(if i.optional{1}else{0}).execute(&mut *tx).await?;
    }
    for (position, step) in g.steps.iter().enumerate() {
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
    for (position, source) in ss.iter().enumerate() {
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
async fn cancel_draft(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Result<Response> {
    sqlx::query("DELETE FROM ai_drafts WHERE id=?")
        .bind(id)
        .execute(&s.db)
        .await?;
    Ok(Redirect::to("/").into_response())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ingredient {
    name: String,
    quantity: String,
    unit: String,
    optional: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IngredientUse {
    ingredient: usize,
    amount: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeneratedStep {
    text: String,
    #[serde(rename = "chartLabel", default)]
    chart_label: String,
    #[serde(rename = "timerSeconds", default)]
    timer_seconds: i64,
    #[serde(rename = "ingredientUses", default)]
    ingredient_uses: Vec<IngredientUse>,
    #[serde(rename = "inputSteps", default)]
    input_steps: Vec<usize>,
    #[serde(default)]
    ingredients: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeneratedRecipe {
    title: String,
    description: String,
    #[serde(rename = "prepMinutes")]
    prep_minutes: i64,
    #[serde(rename = "cookMinutes")]
    cook_minutes: i64,
    servings: i64,
    ingredients: Vec<Ingredient>,
    steps: Vec<GeneratedStep>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChartRecipe {
    version: u8,
    steps: Vec<ChartStep>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChartStep {
    chart_label: String,
    timer_seconds: i64,
    ingredient_uses: Vec<IngredientUse>,
    input_steps: Vec<usize>,
}
impl ChartRecipe {
    fn from_generated(g: &GeneratedRecipe) -> Self {
        Self {
            version: 1,
            steps: g
                .steps
                .iter()
                .map(|s| ChartStep {
                    chart_label: s.chart_label.clone(),
                    timer_seconds: s.timer_seconds,
                    ingredient_uses: s.ingredient_uses.clone(),
                    input_steps: s.input_steps.clone(),
                })
                .collect(),
        }
    }
}
#[derive(FromRow)]
struct Draft {
    id: String,
    recipe_id: Option<String>,
    operation: String,
    recipe_json: String,
    sources_json: String,
    search_suggestions: String,
    base_updated_at: Option<String>,
    created_at: String,
    expires_at: String,
}

async fn create_draft(
    s: &AppState,
    recipe: Option<&Recipe>,
    operation: &str,
    prompt: &str,
) -> Result<String> {
    if s.api_key.is_empty() {
        return Err(AppError::AiNotConfigured);
    }
    let (g, sources, suggestions) = gemini(s, prompt).await?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    sqlx::query("DELETE FROM ai_drafts WHERE expires_at < ?")
        .bind(now.to_rfc3339())
        .execute(&s.db)
        .await?;
    sqlx::query("INSERT INTO ai_drafts(id,recipe_id,operation,recipe_json,sources_json,search_suggestions,base_updated_at,created_at,expires_at)VALUES(?,?,?,?,?,?,?,?,?)").bind(&id).bind(recipe.map(|x|x.id.as_str())).bind(operation).bind(serde_json::to_string(&g).unwrap()).bind(serde_json::to_string(&sources).unwrap()).bind(suggestions).bind(recipe.map(|x|x.updated_at.as_str())).bind(now.to_rfc3339()).bind((now+Duration::hours(DRAFT_HOURS)).to_rfc3339()).execute(&s.db).await?;
    Ok(id)
}
async fn gemini(s: &AppState, prompt: &str) -> Result<(GeneratedRecipe, Vec<Source>, String)> {
    for attempt in 0..2 {
        let input = if attempt == 0 {
            prompt.to_string()
        } else if s.search_grounding {
            format!(
                "{prompt}\n\nImportant: research this thoroughly before answering. Use Google Search, read any supplied URLs with URL Context, and return a complete recipe whose output has URL citations."
            )
        } else {
            format!(
                "{prompt}\n\nImportant: return a complete recipe as valid JSON matching the requested schema."
            )
        };
        let mut body = json!({"model":s.model,"input":input,"system_instruction":if s.search_grounding { GROUNDED_RECIPE_PROMPT } else { RECIPE_PROMPT },"store":false,"response_format":{"type":"text","mime_type":"application/json","schema":recipe_schema()}});
        if s.search_grounding {
            body["tools"] = json!([{"type":"google_search"},{"type":"url_context"}]);
        }
        let resp = s
            .http
            .post(&s.gemini_base_url)
            .header("x-goog-api-key", &s.api_key)
            .header("Api-Revision", "2026-05-20")
            .json(&body)
            .send()
            .await
            .map_err(|_| AppError::Ai)?;
        if !resp.status().is_success() {
            error!(status=%resp.status(),"Gemini request failed");
            return Err(AppError::Ai);
        }
        let value: Value = resp.json().await.map_err(|_| AppError::Ai)?;
        if let Ok(result) = parse_response(&value, s.search_grounding) {
            return Ok(result);
        }
    }
    Err(AppError::Ai)
}
fn parse_response(
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
fn recipe_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["title","description","prepMinutes","cookMinutes","servings","ingredients","steps"],"properties":{"title":{"type":"string"},"description":{"type":"string"},"prepMinutes":{"type":"integer"},"cookMinutes":{"type":"integer"},"servings":{"type":"integer"},"ingredients":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["name","quantity","unit","optional"],"properties":{"name":{"type":"string"},"quantity":{"type":"string"},"unit":{"type":"string"},"optional":{"type":"boolean"}}}},"steps":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["text","chartLabel","timerSeconds","ingredientUses","inputSteps"],"properties":{"text":{"type":"string"},"chartLabel":{"type":"string"},"timerSeconds":{"type":"integer","minimum":0},"ingredientUses":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["ingredient","amount"],"properties":{"ingredient":{"type":"integer","minimum":0},"amount":{"type":"string"}}}},"inputSteps":{"type":"array","items":{"type":"integer","minimum":0}}}}}}})
}
fn normalize_generated(g: &mut GeneratedRecipe) -> Result<()> {
    if g.steps
        .iter()
        .all(|step| step.chart_label.trim().is_empty())
    {
        // A short-lived old draft has only display strings. Convert it to a safe
        // linear flow so it remains savable while new AI output is strictly rich.
        for i in 0..g.steps.len() {
            let step = &mut g.steps[i];
            step.chart_label = step
                .text
                .split_whitespace()
                .take(5)
                .collect::<Vec<_>>()
                .join(" ");
            step.timer_seconds = 0;
            for display in step.ingredients.clone() {
                if let Some((index, ingredient)) = g
                    .ingredients
                    .iter()
                    .enumerate()
                    .find(|(_, x)| display.to_lowercase().ends_with(&x.name.to_lowercase()))
                {
                    let amount = display[..display.len() - ingredient.name.len()].trim();
                    step.ingredient_uses.push(IngredientUse {
                        ingredient: index,
                        amount: if amount.is_empty() {
                            ingredient_display(ingredient)
                        } else {
                            amount.to_string()
                        },
                    });
                }
            }
            if i > 0 {
                step.input_steps.push(i - 1);
            }
        }
        let used: HashSet<usize> = g
            .steps
            .iter()
            .flat_map(|x| x.ingredient_uses.iter().map(|u| u.ingredient))
            .collect();
        if let Some(last) = g.steps.last_mut() {
            for (i, ingredient) in g.ingredients.iter().enumerate() {
                if !used.contains(&i) {
                    last.ingredient_uses.push(IngredientUse {
                        ingredient: i,
                        amount: ingredient_display(ingredient),
                    });
                }
            }
        }
    }
    for step in &mut g.steps {
        step.ingredients = step
            .ingredient_uses
            .iter()
            .filter_map(|use_| {
                g.ingredients.get(use_.ingredient).map(|ingredient| {
                    format!("{} {}", use_.amount.trim(), ingredient.name)
                        .trim()
                        .to_string()
                })
            })
            .collect();
    }
    validate_generated(g)
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
fn validate_generated(g: &GeneratedRecipe) -> Result<()> {
    if g.title.trim().is_empty()
        || g.ingredients.is_empty()
        || g.steps.len() < 2
        || g.prep_minutes < 0
        || g.cook_minutes < 0
        || g.servings < 0
    {
        return Err(AppError::Ai);
    }
    let mut used_ingredients = HashSet::new();
    let mut consumers = vec![0usize; g.steps.len()];
    for (index, step) in g.steps.iter().enumerate() {
        if step.text.trim().is_empty()
            || step.chart_label.trim().is_empty()
            || step.timer_seconds < 0
        {
            return Err(AppError::Ai);
        }
        let mut local_ingredients = HashSet::new();
        let mut local_inputs = HashSet::new();
        for use_ in &step.ingredient_uses {
            if use_.ingredient >= g.ingredients.len()
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
    if used_ingredients.len() != g.ingredients.len()
        || consumers
            .iter()
            .take(g.steps.len() - 1)
            .any(|count| *count != 1)
        || consumers[g.steps.len() - 1] != 0
    {
        return Err(AppError::Ai);
    }
    Ok(())
}
fn dedupe_sources(s: Vec<Source>) -> Vec<Source> {
    let mut seen = HashSet::new();
    s.into_iter()
        .filter(|x| {
            (x.url.starts_with("https://") || x.url.starts_with("http://"))
                && seen.insert(x.url.clone())
        })
        .collect()
}
fn infer_step_ingredients(step: &str, ingredients: &[Block]) -> Vec<String> {
    let lower = step.to_lowercase();
    let words: HashSet<String> = lower
        .split(|c: char| !c.is_alphanumeric())
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
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| word.len() > 2)
                .next_back()
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

#[derive(Clone)]
struct FlowStep {
    label: String,
    timer_seconds: i64,
    additions: Vec<String>,
    inputs: Vec<usize>,
}
fn selected_chart_step(requested: Option<usize>, step_count: usize) -> Option<usize> {
    if step_count == 0 {
        None
    } else {
        requested.map(|one_based| one_based.saturating_sub(1).min(step_count - 1))
    }
}
fn build_chart(
    recipe: &Recipe,
    ingredients: &[Block],
    steps: &[ViewStep],
    requested: Option<usize>,
) -> ChartView {
    let rich = (!recipe.chart_json.trim().is_empty())
        .then_some(recipe.chart_json.as_str())
        .and_then(|raw| serde_json::from_str::<ChartRecipe>(raw).ok())
        .filter(|chart| chart.version == 1 && chart.steps.len() == steps.len())
        .filter(|chart| {
            let candidate = GeneratedRecipe {
                title: recipe.title.clone(),
                description: String::new(),
                prep_minutes: 0,
                cook_minutes: 0,
                servings: 0,
                ingredients: ingredients
                    .iter()
                    .map(|b| Ingredient {
                        name: b.text.clone(),
                        quantity: b.quantity.clone(),
                        unit: b.unit.clone(),
                        optional: b.optional(),
                    })
                    .collect(),
                steps: chart
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| GeneratedStep {
                        text: steps[i].block.text.clone(),
                        chart_label: s.chart_label.clone(),
                        timer_seconds: s.timer_seconds,
                        ingredient_uses: s.ingredient_uses.clone(),
                        input_steps: s.input_steps.clone(),
                        ingredients: Vec::new(),
                    })
                    .collect(),
            };
            validate_generated(&candidate).is_ok()
        });
    let using_rich = rich.is_some();
    let flow: Vec<FlowStep> = if let Some(chart) = rich {
        chart
            .steps
            .into_iter()
            .map(|step| FlowStep {
                label: step.chart_label,
                timer_seconds: step.timer_seconds,
                additions: step
                    .ingredient_uses
                    .into_iter()
                    .filter_map(|u| {
                        ingredients.get(u.ingredient).map(|ingredient| {
                            format!("{} {}", u.amount, ingredient.text)
                                .trim()
                                .to_string()
                        })
                    })
                    .collect(),
                inputs: step.input_steps,
            })
            .collect()
    } else {
        steps
            .iter()
            .enumerate()
            .map(|(i, step)| FlowStep {
                label: step
                    .block
                    .text
                    .split_whitespace()
                    .take(5)
                    .collect::<Vec<_>>()
                    .join(" "),
                timer_seconds: 0,
                additions: step.ingredients.clone(),
                inputs: if i == 0 { Vec::new() } else { vec![i - 1] },
            })
            .collect()
    };
    let unlinked = if using_rich {
        Vec::new()
    } else {
        ingredients
            .iter()
            .filter(|ingredient| {
                !steps
                    .iter()
                    .flat_map(|step| step.ingredients.iter())
                    .any(|used| {
                        used.to_lowercase()
                            .contains(&ingredient.text.to_lowercase())
                    })
            })
            .map(|ingredient| {
                format!(
                    "{} {} {}",
                    ingredient.quantity, ingredient.unit, ingredient.text
                )
                .trim()
                .to_string()
            })
            .collect()
    };
    let selected = selected_chart_step(requested, flow.len());
    let mut leaves = Vec::<(String, usize)>::new();
    let mut ranges = vec![(0usize, 0usize); flow.len()];
    fn layout(
        i: usize,
        flow: &[FlowStep],
        leaves: &mut Vec<(String, usize)>,
        ranges: &mut [(usize, usize)],
    ) {
        let start = leaves.len();
        for &input in &flow[i].inputs {
            layout(input, flow, leaves, ranges);
        }
        for item in &flow[i].additions {
            leaves.push((item.clone(), i));
        }
        if flow[i].inputs.is_empty() && flow[i].additions.is_empty() {
            leaves.push(("Preparation".into(), i));
        }
        let end = leaves.len().max(start + 1);
        ranges[i] = (start, end);
    }
    if !flow.is_empty() {
        layout(flow.len() - 1, &flow, &mut leaves, &mut ranges);
    }
    let mut active_steps = HashSet::new();
    fn ancestors(i: usize, flow: &[FlowStep], out: &mut HashSet<usize>) {
        if out.insert(i) {
            for &input in &flow[i].inputs {
                ancestors(input, flow, out);
            }
        }
    }
    if let Some(current) = selected {
        ancestors(current, &flow, &mut active_steps);
    }
    let is_selected = selected.is_some();
    let chart_leaves = leaves
        .into_iter()
        .enumerate()
        .map(|(row, (label, source))| ChartLeaf {
            label,
            row,
            active: active_steps.contains(&source),
            dimmed: is_selected && !active_steps.contains(&source),
        })
        .collect();
    let cells = flow
        .iter()
        .enumerate()
        .map(|(step, item)| {
            let (row, end) = ranges[step];
            ChartCell {
                step: step + 1,
                label: item.label.clone(),
                row,
                span: (end - row).max(1),
                active: selected == Some(step),
                dimmed: is_selected && !active_steps.contains(&step),
                href: format!("/recipes/{}?view=chart&step={}", recipe.id, step + 1),
            }
        })
        .collect();
    let detail = selected.map(|step| ChartDetail {
        step: step + 1,
        text: steps[step].block.text.clone(),
        additions: flow[step].additions.clone(),
        timer_seconds: flow[step].timer_seconds,
        previous_href: format!("/recipes/{}?view=chart&step={}", recipe.id, step),
        next_href: format!("/recipes/{}?view=chart&step={}", recipe.id, step + 2),
        has_previous: step > 0,
        has_next: step + 1 < flow.len(),
    });
    ChartView {
        cells,
        leaves: chart_leaves,
        unlinked,
        detail,
        step_count: flow.len(),
    }
}

async fn find_recipe(db: &SqlitePool, id: &str) -> Result<Recipe> {
    sqlx::query_as("SELECT * FROM recipes WHERE id=?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}
async fn find_block(db: &SqlitePool, recipe_id: &str, id: &str) -> Result<Block> {
    sqlx::query_as("SELECT * FROM recipe_blocks WHERE id=? AND recipe_id=?")
        .bind(id)
        .bind(recipe_id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}
async fn blocks(db: &SqlitePool, id: &str) -> Result<Vec<Block>> {
    Ok(
        sqlx::query_as("SELECT * FROM recipe_blocks WHERE recipe_id=? ORDER BY section,position")
            .bind(id)
            .fetch_all(db)
            .await?,
    )
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
async fn clear_chart(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, recipe_id: &str) -> Result<()> {
    sqlx::query("UPDATE recipes SET chart_json='' WHERE id=?")
        .bind(recipe_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn sources(db: &SqlitePool, id: &str) -> Result<Vec<Source>> {
    Ok(sqlx::query_as("SELECT id,recipe_id,position,title,url FROM recipe_sources WHERE recipe_id=? ORDER BY position").bind(id).fetch_all(db).await?)
}
async fn find_draft(db: &SqlitePool, id: &str) -> Result<Draft> {
    sqlx::query_as("SELECT * FROM ai_drafts WHERE id=? AND expires_at >= ?")
        .bind(id)
        .bind(Utc::now().to_rfc3339())
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}
async fn touch(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, id: &str) -> Result<()> {
    sqlx::query("UPDATE recipes SET updated_at=? WHERE id=?")
        .bind(stamp())
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn recipe_snapshot(db: &SqlitePool, r: &Recipe) -> Result<Value> {
    let b = blocks(db, &r.id).await?;
    let mut steps = Vec::new();
    for step in b.iter().filter(|x| x.section == "step") {
        steps.push(json!({"text":step.text,"ingredients":step_ingredients(db,&step.id).await?}));
    }
    Ok(
        json!({"title":r.title,"description":r.description,"servings":r.servings,"prepMinutes":r.prep_minutes,"cookMinutes":r.cook_minutes,"ingredients":b.iter().filter(|x|x.section=="ingredient").map(|x|json!({"name":x.text,"quantity":x.quantity,"unit":x.unit,"optional":x.optional()})).collect::<Vec<_>>(),"steps":steps}),
    )
}
fn required(s: &str, name: &str) -> Result<String> {
    let v = trim(s);
    if v.is_empty() {
        Err(AppError::BadRequest(format!("{name} is required.")))
    } else {
        Ok(v)
    }
}
fn trim(s: &str) -> String {
    s.trim().to_string()
}
fn number(s: &str) -> Result<Option<i64>> {
    let s = trim(s);
    if s.is_empty() {
        return Ok(None);
    }
    let n = s
        .parse::<i64>()
        .map_err(|_| AppError::BadRequest("Times and servings must be whole numbers.".into()))?;
    if n < 0 {
        return Err(AppError::BadRequest(
            "Times and servings cannot be negative.".into(),
        ));
    }
    Ok(Some(n))
}
fn option_number(n: Option<i64>) -> String {
    n.map(|x| x.to_string()).unwrap_or_default()
}
fn stamp() -> String {
    Utc::now().to_rfc3339()
}
fn ai_form(
    heading: &str,
    guidance: &str,
    action: &str,
    label: &str,
    button: &str,
    cancel: &str,
    error: &str,
    prompt: &str,
) -> AiFormTemplate {
    AiFormTemplate {
        heading: heading.into(),
        guidance: guidance.into(),
        action: action.into(),
        label: label.into(),
        button: button.into(),
        cancel_url: cancel.into(),
        error: error.into(),
        prompt: prompt.into(),
    }
}
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn env_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}
fn generate_guidance(search_grounding: bool) -> &'static str {
    if search_grounding {
        "Describe what you want to cook, name a recipe to recreate, or paste one or more recipe URLs. Gemini will read, compare, and research before preparing a complete draft."
    } else {
        "Describe what you want to cook or name a recipe to recreate. Gemini will prepare a complete draft for review."
    }
}

// Keeps main's error type small without adding an application dependency solely for startup errors.
mod anyhowless {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_schema_requires_block_shape() {
        let schema = recipe_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&Value::String("ingredients".into())));
        assert!(required.contains(&Value::String("steps".into())));
        assert_eq!(
            schema["properties"]["ingredients"]["items"]["properties"]["optional"]["type"],
            "boolean"
        );
    }

    #[test]
    fn generated_recipe_needs_real_content() {
        let blank = GeneratedRecipe {
            title: " ".into(),
            description: String::new(),
            prep_minutes: 0,
            cook_minutes: 0,
            servings: 2,
            ingredients: vec![],
            steps: vec![],
        };
        assert!(validate_generated(&blank).is_err());
        let valid = GeneratedRecipe {
            title: "Toast".into(),
            description: String::new(),
            prep_minutes: 1,
            cook_minutes: 3,
            servings: 1,
            ingredients: vec![Ingredient {
                name: "bread".into(),
                quantity: "1".into(),
                unit: "slice".into(),
                optional: false,
            }],
            steps: vec![
                GeneratedStep {
                    text: "Heat a pan.".into(),
                    chart_label: "heat pan".into(),
                    timer_seconds: 0,
                    ingredient_uses: vec![],
                    input_steps: vec![],
                    ingredients: vec![],
                },
                GeneratedStep {
                    text: "Toast until golden.".into(),
                    chart_label: "toast until golden".into(),
                    timer_seconds: 180,
                    ingredient_uses: vec![IngredientUse {
                        ingredient: 0,
                        amount: "1 slice".into(),
                    }],
                    input_steps: vec![0],
                    ingredients: vec!["1 slice bread".into()],
                },
            ],
        };
        assert!(validate_generated(&valid).is_ok());
    }

    #[test]
    fn legacy_draft_supports_unmeasured_ingredients() {
        let mut legacy = GeneratedRecipe {
            title: "Seasoned Toast".into(),
            description: String::new(),
            prep_minutes: 1,
            cook_minutes: 3,
            servings: 1,
            ingredients: vec![Ingredient {
                name: "salt".into(),
                quantity: String::new(),
                unit: String::new(),
                optional: false,
            }],
            steps: vec![
                GeneratedStep {
                    text: "Heat a pan.".into(),
                    chart_label: String::new(),
                    timer_seconds: 0,
                    ingredient_uses: vec![],
                    input_steps: vec![],
                    ingredients: vec![],
                },
                GeneratedStep {
                    text: "Season and serve.".into(),
                    chart_label: String::new(),
                    timer_seconds: 0,
                    ingredient_uses: vec![],
                    input_steps: vec![],
                    ingredients: vec!["salt".into()],
                },
            ],
        };

        assert!(normalize_generated(&mut legacy).is_ok());
        assert_eq!(legacy.steps[1].ingredient_uses[0].amount, "as needed");
        assert_eq!(legacy.steps[1].ingredients[0], "as needed salt");
    }

    #[test]
    fn chart_step_query_is_clamped_to_valid_bounds() {
        assert_eq!(selected_chart_step(None, 5), None);
        assert_eq!(selected_chart_step(Some(0), 5), Some(0));
        assert_eq!(selected_chart_step(Some(3), 5), Some(2));
        assert_eq!(selected_chart_step(Some(999), 5), Some(4));
        assert_eq!(selected_chart_step(Some(1), 0), None);
    }

    #[test]
    fn chart_flow_accepts_branch_and_merge_and_rejects_bad_references() {
        let ingredient = |name: &str| Ingredient {
            name: name.into(),
            quantity: "1".into(),
            unit: "cup".into(),
            optional: false,
        };
        let step = |label: &str, uses: Vec<IngredientUse>, inputs: Vec<usize>| GeneratedStep {
            text: label.into(),
            chart_label: label.into(),
            timer_seconds: 0,
            ingredient_uses: uses,
            input_steps: inputs,
            ingredients: vec![],
        };
        let recipe = GeneratedRecipe {
            title: "Eggnog".into(),
            description: String::new(),
            prep_minutes: 0,
            cook_minutes: 0,
            servings: 1,
            ingredients: vec![ingredient("eggs"), ingredient("milk")],
            steps: vec![
                step(
                    "beat",
                    vec![IngredientUse {
                        ingredient: 0,
                        amount: "1 cup".into(),
                    }],
                    vec![],
                ),
                step(
                    "warm",
                    vec![IngredientUse {
                        ingredient: 1,
                        amount: "1 cup".into(),
                    }],
                    vec![],
                ),
                step("combine", vec![], vec![0, 1]),
            ],
        };
        assert!(validate_generated(&recipe).is_ok());
        let mut invalid = recipe.clone();
        invalid.steps[1].input_steps = vec![2];
        assert!(validate_generated(&invalid).is_err());
        let mut split = recipe.clone();
        split.steps[2].input_steps = vec![0];
        assert!(validate_generated(&split).is_err());
    }

    #[test]
    fn chart_layout_merges_eggnog_fixture() {
        let recipe = Recipe {
            id: "eggnog".into(),
            title: "Cooked Egg Nog".into(),
            description: String::new(),
            servings: None,
            prep_minutes: None,
            cook_minutes: None,
            chart_json: serde_json::to_string(&ChartRecipe {
                version: 1,
                steps: vec![
                    ChartStep {
                        chart_label: "wash".into(),
                        timer_seconds: 0,
                        ingredient_uses: vec![IngredientUse {
                            ingredient: 0,
                            amount: "6 large".into(),
                        }],
                        input_steps: vec![],
                    },
                    ChartStep {
                        chart_label: "beat".into(),
                        timer_seconds: 0,
                        ingredient_uses: vec![IngredientUse {
                            ingredient: 1,
                            amount: "1/4 cup".into(),
                        }],
                        input_steps: vec![0],
                    },
                    ChartStep {
                        chart_label: "cook gently".into(),
                        timer_seconds: 300,
                        ingredient_uses: vec![IngredientUse {
                            ingredient: 2,
                            amount: "2 cups".into(),
                        }],
                        input_steps: vec![1],
                    },
                    ChartStep {
                        chart_label: "rest".into(),
                        timer_seconds: 300,
                        ingredient_uses: vec![],
                        input_steps: vec![2],
                    },
                ],
            })
            .unwrap(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let block = |position: i64, text: &str| Block {
            id: String::new(),
            recipe_id: "eggnog".into(),
            section: "ingredient".into(),
            position,
            text: text.into(),
            quantity: String::new(),
            unit: String::new(),
            optional: 0,
        };
        let chart = build_chart(
            &recipe,
            &vec![block(0, "eggs"), block(1, "sugar"), block(2, "milk")],
            &vec![
                ViewStep {
                    block: ViewBlock {
                        id: "s0".into(),
                        position: 0,
                        text: "Wash the eggs.".into(),
                        quantity: String::new(),
                        unit: String::new(),
                        optional: false,
                        editing: false,
                    },
                    ingredients: vec![],
                    ingredients_text: String::new(),
                },
                ViewStep {
                    block: ViewBlock {
                        id: "s1".into(),
                        position: 1,
                        text: "Beat the eggs and sugar.".into(),
                        quantity: String::new(),
                        unit: String::new(),
                        optional: false,
                        editing: false,
                    },
                    ingredients: vec![],
                    ingredients_text: String::new(),
                },
                ViewStep {
                    block: ViewBlock {
                        id: "s2".into(),
                        position: 2,
                        text: "Cook over low heat.".into(),
                        quantity: String::new(),
                        unit: String::new(),
                        optional: false,
                        editing: false,
                    },
                    ingredients: vec![],
                    ingredients_text: String::new(),
                },
                ViewStep {
                    block: ViewBlock {
                        id: "s3".into(),
                        position: 3,
                        text: "Rest five minutes.".into(),
                        quantity: String::new(),
                        unit: String::new(),
                        optional: false,
                        editing: false,
                    },
                    ingredients: vec![],
                    ingredients_text: String::new(),
                },
            ],
            Some(4),
        );
        assert_eq!(chart.leaves.len(), 3);
        assert_eq!(chart.cells[3].span, 3);
        assert!(chart.cells[3].active);
    }

    #[test]
    fn old_steps_infer_explicitly_measured_ingredients() {
        let ingredient = |text: &str, quantity: &str, unit: &str| Block {
            id: String::new(),
            recipe_id: String::new(),
            section: "ingredient".into(),
            position: 0,
            text: text.into(),
            quantity: quantity.into(),
            unit: unit.into(),
            optional: 0,
        };
        let ingredients = vec![
            ingredient("olive oil", "2", "tbsp"),
            ingredient("salt", "1", "tsp"),
        ];
        assert_eq!(
            infer_step_ingredients("Heat 2 tbsp olive oil until shimmering.", &ingredients),
            vec!["2 tbsp olive oil"]
        );
    }

    #[test]
    fn duplicate_citation_urls_are_removed() {
        let source = |url: &str| Source {
            id: None,
            recipe_id: None,
            position: None,
            title: "A".into(),
            url: url.into(),
        };
        assert_eq!(
            dedupe_sources(vec![
                source("https://example.com"),
                source("https://example.com"),
                source("javascript:alert(1)")
            ])
            .len(),
            1
        );
    }

    #[test]
    fn grounded_response_extracts_recipe_and_citations() {
        let recipe_json = json!({
            "title": "Toast", "description": "Quick", "prepMinutes": 1,
            "cookMinutes": 3, "servings": 1,
            "ingredients": [{"name":"bread","quantity":"1","unit":"slice","optional":false}],
            "steps": [{"text":"Heat pan.","ingredients":[]},{"text":"Toast until golden.","ingredients":["1 slice bread"]}]
        }).to_string();
        let response = json!({"steps":[
            {"type":"google_search_call","arguments":{"queries":["toast recipe"]}},
            {"type":"google_search_result","result":[{"search_suggestions":"<div>Search</div>"}]},
            {"type":"model_output","content":[{"type":"text","text":recipe_json,"annotations":[{"type":"url_citation","url":"https://example.com/toast","title":"Toast source"}]}]}
        ]});
        let (recipe, sources, suggestions) = parse_response(&response, true).unwrap();
        assert_eq!(recipe.title, "Toast");
        assert_eq!(sources[0].url, "https://example.com/toast");
        assert!(suggestions.contains("Search"));
    }

    #[test]
    fn url_context_response_counts_as_grounded_research() {
        let recipe_json = json!({
            "title": "Referenced Toast", "description": "Researched from a supplied URL",
            "prepMinutes": 1, "cookMinutes": 3, "servings": 1,
            "ingredients": [{"name":"bread","quantity":"1","unit":"slice","optional":false}],
            "steps": [{"text":"Heat pan.","ingredients":[]},{"text":"Toast until golden.","ingredients":["1 slice bread"]}]
        }).to_string();
        let response = json!({"steps":[
            {"type":"url_context_call","arguments":{"urls":["https://example.com/toast"]}},
            {"type":"url_context_result","result":[{"url":"https://example.com/toast","status":"success"}]},
            {"type":"model_output","content":[{"type":"text","text":recipe_json,"annotations":[
                {"type":"url_citation","url":"https://example.com/toast","title":"Toast recipe"}
            ]}]}
        ]});
        let (recipe, sources, _) = parse_response(&response, true).unwrap();
        assert_eq!(recipe.title, "Referenced Toast");
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn grounding_without_a_search_call_is_rejected() {
        let response = json!({"steps":[{"type":"model_output","content":[{"type":"text","text":"{}","annotations":[{"type":"url_citation","url":"https://example.com"}]}]}]});
        assert!(parse_response(&response, true).is_err());
    }

    #[test]
    fn ungrounded_response_is_accepted_without_search_or_citations() {
        let recipe_json = json!({
            "title": "Toast", "description": "Quick", "prepMinutes": 1,
            "cookMinutes": 3, "servings": 1,
            "ingredients": [{"name":"bread","quantity":"1","unit":"slice","optional":false}],
            "steps": [{"text":"Heat pan.","ingredients":[]},{"text":"Toast until golden.","ingredients":["1 slice bread"]}]
        }).to_string();
        let response = json!({"steps":[
            {"type":"model_output","content":[{"type":"text","text":recipe_json}]}
        ]});
        let (recipe, sources, suggestions) = parse_response(&response, false).unwrap();
        assert_eq!(recipe.title, "Toast");
        assert!(sources.is_empty());
        assert!(suggestions.is_empty());
    }

    #[test]
    fn env_bool_accepts_true_and_false_and_uses_default_for_invalid_values() {
        unsafe { env::set_var("KINDLE_RECIPES_TEST_BOOL", "false") };
        assert!(!env_bool("KINDLE_RECIPES_TEST_BOOL", true));
        unsafe { env::set_var("KINDLE_RECIPES_TEST_BOOL", "true") };
        assert!(env_bool("KINDLE_RECIPES_TEST_BOOL", false));
        unsafe { env::set_var("KINDLE_RECIPES_TEST_BOOL", "yes") };
        assert!(env_bool("KINDLE_RECIPES_TEST_BOOL", true));
        unsafe { env::remove_var("KINDLE_RECIPES_TEST_BOOL") };
    }

    #[tokio::test]
    async fn moving_a_block_swaps_positions_without_changing_ids() {
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
        let now = stamp();
        sqlx::query("INSERT INTO recipes(id,title,created_at,updated_at) VALUES('r','Test',?,?)")
            .bind(&now)
            .bind(&now)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('first','r','step',0,'First'),('second','r','step',1,'Second')")
            .execute(&db).await.unwrap();
        let state = Arc::new(AppState {
            db: db.clone(),
            http: reqwest::Client::new(),
            api_key: String::new(),
            model: String::new(),
            gemini_base_url: String::new(),
            search_grounding: false,
        });
        move_block(
            State(state),
            Path(("r".into(), "first".into(), "down".into())),
        )
        .await
        .unwrap();
        let ordered: Vec<(String, i64)> =
            sqlx::query_as("SELECT id,position FROM recipe_blocks ORDER BY position")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(ordered, vec![("second".into(), 0), ("first".into(), 1)]);
    }

    #[tokio::test]
    async fn deleting_a_block_compacts_the_remaining_positions() {
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
        let now = stamp();
        sqlx::query("INSERT INTO recipes(id,title,created_at,updated_at) VALUES('r','Test',?,?)")
            .bind(&now)
            .bind(&now)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('a','r','ingredient',0,'A'),('b','r','ingredient',1,'B'),('c','r','ingredient',2,'C')")
            .execute(&db).await.unwrap();
        let state = Arc::new(AppState {
            db: db.clone(),
            http: reqwest::Client::new(),
            api_key: String::new(),
            model: String::new(),
            gemini_base_url: String::new(),
            search_grounding: false,
        });
        delete_block(State(state), Path(("r".into(), "b".into())))
            .await
            .unwrap();
        let ordered: Vec<(String, i64)> =
            sqlx::query_as("SELECT id,position FROM recipe_blocks ORDER BY position")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(ordered, vec![("a".into(), 0), ("c".into(), 1)]);
    }
}
