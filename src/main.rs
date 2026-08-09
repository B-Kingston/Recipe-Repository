use askama::Template;
use axum::{
    Router,
    extract::{Form, Path, State},
    middleware,
    response::{Html, IntoResponse, Json, Redirect, Response},
    routing::{get, post},
};
use chrono::Utc;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::HashMap, env, io::Write, net::SocketAddr, process::Stdio, str::FromStr, sync::Arc,
};
use thiserror::Error;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{error, info, warn};
use uuid::Uuid;

mod ai;
mod auth;
mod chart;
mod recipes;
use auth::{
    AuthUser, auth_middleware, reset_password, reset_password_page, setup_create, setup_page,
};

use ai::{
    alter_draft, alter_page, alter_recipe, apply_draft, cancel_draft, draft_page,
    fresh_model_catalogue, generate_page, generate_recipe,
};
use recipes::{
    add_block, delete_block, delete_page, delete_recipe, home, move_block, new_recipe, recipe_page,
    update_block, update_recipe,
};

const DRAFT_HOURS: i64 = 24;
const DEFAULT_MODEL: &str = "gpt-5.4-mini";
const DEFAULT_OPENAI_MODEL: &str = "gpt-5.6-luna";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_REASONING_EFFORT: &str = "low";
/// Reasoning efforts offered for the model, matching the worker protocol.
const REASONING_EFFORTS: &[&str] = &["low", "medium", "high"];
/// Built-in fallback for the Settings model list, used only when the live
/// pi.dev catalogue cannot be fetched. Ordered newest first.
const MODEL_OPTIONS: &[&str] = &[
    "gpt-5.6-terra",
    "gpt-5.6-sol",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
];
const RECIPE_PROMPT: &str = r#"You are a recipe assistant. Prefer credible guidance and synthesize practical advice without copying source text. Return one complete recipe, not a patch. Before responding, silently audit the recipe's quantities: choose amounts that are realistic for the stated servings, technique, and intended result; give ingredients useful, unambiguous kitchen measurements; and verify that each step's ingredientUses allocate the listed total of that ingredient exactly once. When an ingredient is divided between steps, use explicit portions that add up to its ingredient-list amount, using consistent units where practical. Do not invent unused ingredients or use vague quantities when a measured amount is needed for a reliable result. Every ingredient must be used exactly through one or more ingredientUses. Each step has a concise chartLabel (2–6 words), a timerSeconds value (0 when untimed; use the midpoint of a range), ingredientUses containing canonical zero-based ingredient indices plus exact amounts used at that step, and inputSteps containing only earlier step indices whose outputs are combined. Give every non-final step at most one consumer; make every step flow into the final step. Ingredient-free preparation steps are allowed. Give heating, simmering, baking, frying, resting, chilling, and reducing steps realistic duration and a clear doneness cue. Keep total timings consistent. Notice when the user's request excludes an ingredient — "egg free", "no dairy", "vegan", "gluten free", "without X" — and do not silently drop it. Judge whether the ingredient is genuinely optional; when it plays a structural role (binding, leavening, emulsifying, thickening, moisture, or browning), substitute a suitable alternative from sound cooking knowledge (such as flax or chia "eggs", aquafaba, plant milks and butters, or xanthan gum) and adjust technique so the recipe still achieves its intended result. Be concise and practical for a home cook."#;
const GROUNDED_RECIPE_PROMPT: &str = r#"You are a meticulous recipe research assistant. Research before generating. Always use the web_search tool. When the user supplies one or more URLs, search for their published title, author, and recipe details. When the user names an existing recipe, cook, book, restaurant, or website, locate the likely original and cross-check it against at least two independent, credible sources when available.

Study ingredient ratios, ingredient roles, preparation, sequencing, temperatures, timings, texture cues, and the intended final result. Recreate the referenced recipe faithfully enough to achieve that result, while resolving omissions or contradictions with sound cooking knowledge. Do not copy expressive prose, headnotes, or distinctive wording; write original, concise instructions. Preserve attribution through citations rather than imitation of the author's voice.

When the user asks to exclude an ingredient — "egg free", "no dairy", "vegan", "gluten free", "without nuts", and similar — do not silently drop it. First judge whether the ingredient is genuinely optional: if it only adds flavour or is truly dispensable, omitting it is fine, but make that a conscious, reasoned decision. If it plays a structural role — binding, leavening, emulsifying, thickening, moisture, browning, or setting — research suitable replacement ingredients and techniques with web_search (for example flax or chia "eggs", aquafaba, mashed banana, plant milks and butters, nut-free butters, xanthan or psyllium gum) and adapt quantities and method so the recipe still achieves its intended result. Keep the recipe true to its original character: the substitution must preserve the outcome, not merely delete the ingredient.

Return one complete recipe, not a patch. Before responding, silently audit the recipe's quantities: choose amounts that are realistic for the stated servings, technique, and intended result; give ingredients useful, unambiguous kitchen measurements; and verify that each step's ingredientUses allocate the listed total of that ingredient exactly once. When an ingredient is divided between steps, use explicit portions that add up to its ingredient-list amount, using consistent units where practical. Do not invent unused ingredients or use vague quantities when a measured amount is needed for a reliable result. Every ingredient must be used exactly through one or more ingredientUses. Each step has a concise chartLabel (2–6 words), a timerSeconds value (0 when untimed; use the midpoint of a range), ingredientUses containing canonical zero-based ingredient indices plus exact amounts used at that step, and inputSteps containing only earlier step indices whose outputs are combined. Give every non-final step at most one consumer; make every step flow into the final step. Ingredient-free preparation steps are allowed. Give heating, simmering, baking, frying, resting, chilling, and reducing steps realistic duration and a clear doneness cue. Keep total timings consistent. Cite every source that materially informed the recipe."#;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    model: String,
    pi_worker_path: String,
    auth_script_path: String,
    search_grounding: bool,
    codex_flows: Arc<Mutex<HashMap<String, CodexFlow>>>,
    model_catalogue: Arc<Mutex<Option<ModelCatalogue>>>,
}

/// Last known Codex model catalogue, refreshed in the background so page
/// renders never wait on a pi.dev refresh.
#[derive(Clone)]
struct ModelCatalogue {
    models: Vec<String>,
    refreshed_at: std::time::Instant,
}

/// In-flight OpenAI Codex device-code authorisation, keyed by flow id.
#[derive(Clone)]
struct CodexFlow {
    user_id: String,
    device_auth_id: String,
    user_code: String,
    expires_at: i64,
}

#[derive(Debug, Error)]
enum AppError {
    #[error("That page was not found.")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Internal(String),
    #[error(
        "The AI service is not configured. Authorise Codex or configure an OpenAI API key from the Settings page."
    )]
    AiNotConfigured,
    #[error("The AI service could not prepare a grounded recipe. Please try again.")]
    Ai,
    #[error("Codex authorisation failed: {0}")]
    CodexAuth(String),
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
    updated_at: String,
}
#[derive(Debug, Clone, FromRow)]
struct Block {
    id: String,
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
#[template(path = "setup.html")]
struct SetupTemplate {
    error: String,
    username: String,
}
#[derive(Template)]
#[template(path = "password_reset.html")]
struct ResetPasswordTemplate {
    error: String,
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
    error: String,
    prompt: String,
}
#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    model_options: Vec<SelectOption>,
    effort_options: Vec<SelectOption>,
    search_enabled: bool,
    codex_authorised: bool,
    codex_auth_url: String,
    openai_mode: bool,
    openai_base_url: String,
}
struct SelectOption {
    value: String,
    selected: bool,
}
#[derive(Template)]
#[template(path = "codex_authorise.html")]
struct CodexAuthoriseTemplate {
    flow_id: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: u64,
    expires_seconds: i64,
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
struct SetupForm {
    username: String,
    password: String,
}
#[derive(Deserialize)]
struct ResetPasswordForm {
    current_password: String,
    new_password: String,
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
struct SettingsForm {
    #[serde(default)]
    model: String, // Codex-only; the OpenAI mode is pinned to DEFAULT_OPENAI_MODEL
    #[serde(default)]
    reasoning_effort: String,
    #[serde(default)]
    use_openai_api: Option<String>, // checkbox present when checked
    #[serde(default)]
    openai_base_url: String,
    #[serde(default)]
    openai_api_key: String,
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
    import_legacy_codex_auth(&db).await?;
    let configured_model = env::var("PI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    set_default_model(&db, &configured_model).await?;
    let state = Arc::new(AppState {
        db,
        model: configured_model,
        pi_worker_path: env::var("PI_WORKER_PATH")
            .unwrap_or_else(|_| "pi/recipe-worker.mjs".into()),
        auth_script_path: env::var("PI_AUTH_SCRIPT_PATH")
            .unwrap_or_else(|_| "pi/codex-auth.mjs".into()),
        search_grounding: env_bool("PI_SEARCH_ENABLED", true),
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(None)),
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
    let protected = Router::new()
        .route("/", get(home))
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
        .route("/ai/drafts/{id}/alter", post(alter_draft))
        .route("/ai/drafts/{id}/apply", post(apply_draft))
        .route("/ai/drafts/{id}/cancel", post(cancel_draft))
        .route("/settings", get(settings_page).post(update_settings))
        .route(
            "/settings/password",
            get(reset_password_page).post(reset_password),
        )
        .route("/settings/authorise-codex", get(authorise_codex_start))
        .route(
            "/settings/authorise-codex/status/{flow_id}",
            get(authorise_codex_status),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/setup", get(setup_page).post(setup_create))
        .nest_service("/static", ServeDir::new("static"))
        .merge(protected)
        .with_state(state)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ingredient {
    name: String,
    quantity: String,
    unit: String,
    /// The model occasionally omits this flag; absence means not optional.
    #[serde(default)]
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
    recipe_id: Option<String>,
    recipe_json: String,
    sources_json: String,
    search_suggestions: String,
    base_updated_at: Option<String>,
}

#[derive(Clone)]
struct FlowStep {
    label: String,
    timer_seconds: i64,
    additions: Vec<String>,
    inputs: Vec<usize>,
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
        "Describe what you want to cook, name a recipe to recreate, or paste one or more recipe URLs. Pi will research the web before preparing a complete draft."
    } else {
        "Describe what you want to cook or name a recipe to recreate. Pi will prepare a complete draft for review."
    }
}

const CODEX_PROVIDER: &str = "openai-codex";
const OPENAI_API_PROVIDER: &str = "openai-compatible";

/// The stored Codex OAuth credential, if any. The database is the source of
/// truth; the Pi worker only ever sees a per-request materialised auth.json.
pub(crate) async fn codex_credential(db: &SqlitePool, user_id: &str) -> Result<Option<Value>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT credential_json FROM pi_credentials WHERE user_id=? AND provider=?")
            .bind(user_id)
            .bind(CODEX_PROVIDER)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(json,)| serde_json::from_str(&json).ok()))
}
/// Persists (or replaces) the Codex OAuth credential for one user.
pub(crate) async fn store_codex_credential(
    db: &SqlitePool,
    user_id: &str,
    credential: &Value,
) -> Result<()> {
    let json = serde_json::to_string(credential)
        .map_err(|_| AppError::CodexAuth("credential could not be serialised".into()))?;
    sqlx::query(
        "INSERT INTO pi_credentials (user_id,provider,credential_json,updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(user_id,provider) DO UPDATE SET
           credential_json = excluded.credential_json,
           updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(CODEX_PROVIDER)
    .bind(json)
    .bind(stamp())
    .execute(db)
    .await?;
    Ok(())
}

/// The active AI provider: "openai" (OpenAI API) or "pi" (Codex).
/// Stored globally in app_settings like the model; the single-user app keeps
/// this app-wide. Anything other than exactly "openai" resolves to "pi".
pub(crate) async fn ai_provider(db: &SqlitePool) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM app_settings WHERE key = 'ai_provider'")
            .fetch_optional(db)
            .await?;
    Ok(row
        .map(|(value,)| value)
        .filter(|value| value == "openai")
        .unwrap_or_else(|| "pi".into()))
}

/// The stored OpenAI API config for one user: (base URL, API key).
pub(crate) async fn openai_api_config(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Option<(String, String)>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT credential_json FROM pi_credentials WHERE user_id=? AND provider=?")
            .bind(user_id)
            .bind(OPENAI_API_PROVIDER)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(json,)| {
        let value: Value = serde_json::from_str(&json).ok()?;
        Some((
            value.get("baseUrl")?.as_str()?.to_string(),
            value.get("apiKey")?.as_str()?.to_string(),
        ))
    }))
}

/// Upserts the OpenAI API config; same SQL shape as store_codex_credential,
/// with provider OPENAI_API_PROVIDER and
/// credential_json = json!({"baseUrl": base_url, "apiKey": api_key}).
pub(crate) async fn store_openai_api_config(
    db: &SqlitePool,
    user_id: &str,
    base_url: &str,
    api_key: &str,
) -> Result<()> {
    let json = serde_json::to_string(&json!({ "baseUrl": base_url, "apiKey": api_key }))
        .map_err(|_| AppError::Internal("OpenAI API config could not be serialised".into()))?;
    sqlx::query(
        "INSERT INTO pi_credentials (user_id,provider,credential_json,updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(user_id,provider) DO UPDATE SET
           credential_json = excluded.credential_json,
           updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(OPENAI_API_PROVIDER)
    .bind(json)
    .bind(stamp())
    .execute(db)
    .await?;
    Ok(())
}

/// One-time migration of a credential previously written by the Pi CLI to
/// `<agent dir>/auth.json` into the database. Only runs before the first user
/// exists; the setup transaction claims the orphan credential for that user.
async fn import_legacy_codex_auth(db: &SqlitePool) -> Result<()> {
    if auth::user_count(db).await? > 0 || codex_credential(db, "").await?.is_some() {
        return Ok(());
    }
    let agent_dir = env::var("PI_CODING_AGENT_DIR").unwrap_or_else(|_| {
        let home = env::var("HOME").unwrap_or_default();
        format!("{home}/.pi/agent")
    });
    let auth_path = std::path::Path::new(&agent_dir).join("auth.json");
    let Ok(text) = std::fs::read_to_string(auth_path) else {
        return Ok(());
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Ok(());
    };
    if let Some(credential) = value.get(CODEX_PROVIDER) {
        store_codex_credential(db, "", credential).await?;
        info!("Imported existing Codex credential into the database");
    }
    Ok(())
}

async fn settings_page(State(s): State<Arc<AppState>>, user: AuthUser) -> Result<Html<String>> {
    let codex_authorised = match codex_credential(&s.db, &user.id).await {
        Ok(Some(credential)) => credential["type"].as_str() == Some("oauth"),
        Ok(None) => false,
        Err(error) => {
            warn!(%error, "Codex credential read failed");
            false
        }
    };
    let openai_mode = crate::ai_provider(&s.db).await? == "openai";
    let openai_base_url = crate::openai_api_config(&s.db, &user.id)
        .await?
        .map(|(url, _)| url)
        .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.into());
    // The model list is Codex-only: the OpenAI mode is pinned to
    // DEFAULT_OPENAI_MODEL, so no catalogue refresh is ever triggered for it.
    let (model, fresh) = if openai_mode {
        (String::new(), Vec::new())
    } else {
        (
            selected_model(&s.db, &s.model).await?,
            fresh_model_catalogue(&s, &user.id).await,
        )
    };
    render(SettingsTemplate {
        model_options: model_options(fresh, &model),
        effort_options: effort_options(&selected_effort(&s.db, DEFAULT_REASONING_EFFORT).await?),
        search_enabled: s.search_grounding,
        codex_authorised,
        codex_auth_url: "/settings/authorise-codex".into(),
        openai_mode,
        openai_base_url,
    })
}

/// Model options for the Settings select: the live catalogue from the Codex
/// worker when available, otherwise the built-in fallback list. The current
/// selection is always included (marked selected) so saving never silently
/// changes the model.
fn model_options(fresh: Vec<String>, current: &str) -> Vec<SelectOption> {
    let mut values = if fresh.is_empty() {
        MODEL_OPTIONS.iter().map(|model| (*model).into()).collect()
    } else {
        fresh
    };
    if !values.iter().any(|value| value == current) {
        values.insert(0, current.to_string());
    }
    values
        .into_iter()
        .map(|value| SelectOption {
            selected: value == current,
            value,
        })
        .collect()
}

/// Effort options for the Settings select: a fixed low/medium/high set (the
/// worker protocol knows no other values), with the current choice selected.
fn effort_options(current: &str) -> Vec<SelectOption> {
    REASONING_EFFORTS
        .iter()
        .map(|value| SelectOption {
            selected: *value == current,
            value: (*value).into(),
        })
        .collect()
}

/// Accepts models from the live catalogue when present, or the built-in
/// fallback list — matching what the Settings page offers.
fn model_supported(model: &str, fresh: &[String]) -> bool {
    fresh.iter().any(|candidate| candidate == model) || MODEL_OPTIONS.contains(&model)
}

async fn update_settings(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<SettingsForm>,
) -> Result<Redirect> {
    let want_openai = form.use_openai_api.is_some();
    // The effort applies to both providers, so it is saved regardless of mode.
    let effort = trim(&form.reasoning_effort);
    let effort = if effort.is_empty() {
        DEFAULT_REASONING_EFFORT.to_string()
    } else {
        effort
    };
    if !REASONING_EFFORTS.contains(&effort.as_str()) {
        return Err(AppError::BadRequest(
            "Choose a supported reasoning effort.".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('reasoning_effort', ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(effort)
    .bind(stamp())
    .execute(&s.db)
    .await?;
    if !want_openai {
        let model = trim(&form.model);
        if model.is_empty() {
            return Err(AppError::BadRequest("Choose a model.".into()));
        }
        let fresh = fresh_model_catalogue(&s, &user.id).await;
        if !model_supported(&model, &fresh) {
            return Err(AppError::BadRequest(
                "Choose a supported Codex model.".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO app_settings (key, value, updated_at) VALUES ('model', ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(&model)
        .bind(stamp())
        .execute(&s.db)
        .await?;
    }
    if want_openai {
        let stored = crate::openai_api_config(&s.db, &user.id).await?;
        let base_url = trim(&form.openai_base_url);
        let base_url = if base_url.is_empty() {
            stored
                .as_ref()
                .map(|(url, _)| url.clone())
                .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.into())
        } else {
            base_url
        };
        let api_key = trim(&form.openai_api_key);
        let api_key = if api_key.is_empty() {
            stored
                .as_ref()
                .map(|(_, key)| key.clone())
                .ok_or_else(|| AppError::BadRequest("Enter the API key.".into()))?
        } else {
            api_key
        };
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err(AppError::BadRequest(
                "API base URL must start with http:// or https://.".into(),
            ));
        }
        let base_url = base_url.trim_end_matches('/').to_string();
        crate::store_openai_api_config(&s.db, &user.id, &base_url, &api_key).await?;
    }
    sqlx::query(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('ai_provider', ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(if want_openai { "openai" } else { "pi" })
    .bind(stamp())
    .execute(&s.db)
    .await?;
    Ok(Redirect::to("/settings"))
}

async fn set_default_model(db: &SqlitePool, model: &str) -> Result<()> {
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT value FROM app_settings WHERE key = 'model'")
            .fetch_optional(db)
            .await?;
    if existing.is_none() {
        sqlx::query("INSERT INTO app_settings (key, value, updated_at) VALUES ('model', ?, ?)")
            .bind(model)
            .bind(stamp())
            .execute(db)
            .await?;
    }
    Ok(())
}

pub(crate) async fn selected_model(db: &SqlitePool, fallback: &str) -> Result<String> {
    let model: Option<(String,)> =
        sqlx::query_as("SELECT value FROM app_settings WHERE key = 'model'")
            .fetch_optional(db)
            .await?;
    Ok(model
        .map(|(model,)| model)
        .unwrap_or_else(|| fallback.into()))
}

pub(crate) async fn selected_effort(db: &SqlitePool, fallback: &str) -> Result<String> {
    let effort: Option<(String,)> =
        sqlx::query_as("SELECT value FROM app_settings WHERE key = 'reasoning_effort'")
            .fetch_optional(db)
            .await?;
    Ok(effort
        .map(|(effort,)| effort)
        .unwrap_or_else(|| fallback.into()))
}

/// Starts an OpenAI Codex device-code flow and shows the code to enter at
/// chatgpt.com/codex/device. The rendered page polls the status route below.
async fn authorise_codex_start(State(s): State<Arc<AppState>>, user: AuthUser) -> Result<Response> {
    let output = run_node_script(&s.auth_script_path, "start", b"")
        .map_err(|_| AppError::CodexAuth("could not start the authorisation flow".into()))?;
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        AppError::CodexAuth("unexpected response from the authorisation helper".into())
    })?;
    if !output.status.success() || value["status"].as_str() != Some("ok") {
        let message = value["message"]
            .as_str()
            .unwrap_or("authorisation could not be started");
        return Err(AppError::CodexAuth(message.to_string()));
    }
    let device_auth_id = value["deviceAuthId"]
        .as_str()
        .ok_or_else(|| AppError::CodexAuth("authorisation helper returned no device id".into()))?;
    let user_code = value["userCode"]
        .as_str()
        .ok_or_else(|| AppError::CodexAuth("authorisation helper returned no user code".into()))?;
    let interval_seconds = value["intervalSeconds"].as_u64().unwrap_or(5).max(1);
    let expires_seconds = value["expiresInSeconds"]
        .as_i64()
        .unwrap_or(15 * 60)
        .max(60);
    let expires_at = Utc::now().timestamp() + expires_seconds;
    let flow_id = Uuid::new_v4().to_string();
    s.codex_flows.lock().insert(
        flow_id.clone(),
        CodexFlow {
            user_id: user.id.clone(),
            device_auth_id: device_auth_id.to_string(),
            user_code: user_code.to_string(),
            expires_at,
        },
    );
    render(CodexAuthoriseTemplate {
        flow_id,
        user_code: user_code.to_string(),
        verification_uri: value["verificationUri"]
            .as_str()
            .unwrap_or("https://auth.openai.com/codex/device")
            .to_string(),
        interval_seconds,
        expires_seconds,
    })
    .map(IntoResponse::into_response)
}

/// One poll attempt for a running device-code flow. On success the helper
/// returns the fresh Codex credential, which is stored in the database, and
/// the flow is dropped.
async fn authorise_codex_status(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(flow_id): Path<String>,
) -> Result<Json<Value>> {
    let flow = s.codex_flows.lock().get(&flow_id).cloned();
    let Some(flow) = flow else {
        return Ok(Json(json!({ "status": "unknown" })));
    };
    if flow.user_id != user.id {
        return Ok(Json(json!({ "status": "unknown" })));
    }
    if Utc::now().timestamp() > flow.expires_at {
        s.codex_flows.lock().remove(&flow_id);
        return Ok(Json(json!({ "status": "expired" })));
    }
    let payload = json!({
        "deviceAuthId": flow.device_auth_id,
        "userCode": flow.user_code,
    });
    let payload = serde_json::to_vec(&payload)
        .map_err(|_| AppError::CodexAuth("invalid flow state".into()))?;
    let output = run_node_script(&s.auth_script_path, "poll", &payload)
        .map_err(|_| AppError::CodexAuth("the authorisation helper could not be run".into()))?;
    let mut value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        json!({ "status": "failed", "message": "unexpected response from the authorisation helper" })
    });
    if value["status"].as_str() == Some("error") {
        value["status"] = json!("failed");
    }
    if value["status"].as_str() == Some("complete") {
        s.codex_flows.lock().remove(&flow_id);
        if let Some(credential) = value.get("credential").cloned() {
            store_codex_credential(&s.db, &user.id, &credential).await?;
        }
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("credential");
    }
    Ok(Json(value))
}

#[cfg(test)]
mod model_tests {
    use super::{MODEL_OPTIONS, SelectOption, effort_options, model_options, model_supported};

    fn values(options: &[SelectOption]) -> Vec<&str> {
        options.iter().map(|option| option.value.as_str()).collect()
    }

    #[test]
    fn fallback_list_is_current() {
        assert!(MODEL_OPTIONS.contains(&"gpt-5.6-sol"));
        assert!(MODEL_OPTIONS.contains(&"gpt-5.6-terra"));
        assert!(MODEL_OPTIONS.contains(&"gpt-5.6-luna"));
        assert!(!MODEL_OPTIONS.contains(&"gpt-5.2-codex"));
    }

    #[test]
    fn fresh_catalogue_supersedes_fallback() {
        let fresh = vec!["gpt-5.6-sol".to_string(), "gpt-5.5".to_string()];
        let options = model_options(fresh.clone(), "gpt-5.6-sol");
        assert_eq!(values(&options), vec!["gpt-5.6-sol", "gpt-5.5"]);
        assert!(options[0].selected && !options[1].selected);
        // Fresh-only, fallback-only, and stale models resolve consistently.
        assert!(model_supported("gpt-5.6-sol", &fresh));
        assert!(model_supported("gpt-5.4", &fresh));
        assert!(!model_supported("gpt-5.2-codex", &fresh));
    }

    #[test]
    fn current_selection_survives_a_catalogue_that_dropped_it() {
        let options = model_options(vec!["gpt-5.6-sol".to_string()], "gpt-5.4");
        assert_eq!(values(&options), vec!["gpt-5.4", "gpt-5.6-sol"]);
        assert!(options[0].selected && !options[1].selected);
    }

    #[test]
    fn empty_fresh_list_falls_back_to_builtin() {
        let options = model_options(Vec::new(), "gpt-5.4-mini");
        assert_eq!(values(&options), MODEL_OPTIONS);
    }

    #[test]
    fn effort_options_offer_all_three_with_current_selected() {
        let options = effort_options("medium");
        assert_eq!(values(&options), vec!["low", "medium", "high"]);
        assert!(!options[0].selected && options[1].selected && !options[2].selected);
        // An unknown stored value never marks an option selected; the worker
        // defaults such requests to low.
        assert!(
            effort_options("extreme")
                .iter()
                .all(|option| !option.selected)
        );
    }
}

fn run_node_script(
    script_path: &str,
    subcommand: &str,
    payload: &[u8],
) -> std::io::Result<std::process::Output> {
    let mut child = std::process::Command::new("node")
        .arg(script_path)
        .arg(subcommand)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .expect("script stdin is piped")
        .write_all(payload)?;
    child.wait_with_output()
}
// Keeps main's error type small without adding an application dependency solely for startup errors.
mod anyhowless {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(test)]
mod tests;
