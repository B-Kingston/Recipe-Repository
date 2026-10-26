use askama::Template;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
};
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::{HashMap, HashSet},
    env,
    io::{Read, Write},
    net::SocketAddr,
    process::Stdio,
    str::FromStr,
    sync::Arc,
};
use thiserror::Error;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{error, info, warn};
use uuid::Uuid;

mod ai;
mod auth;
mod chart;
mod display;
mod embeddings;
mod htmx;
mod media;
mod media_worker;
mod moments;
mod recipes;
mod search;
mod thumbs;
use auth::{
    AuthUser, auth_middleware, create_account, login, login_page, logout, logout_all,
    new_account_page, reset_password, reset_password_page, setup_create, setup_page,
    switch_account,
};

use ai::{
    DebugRunMap, ModelSource, alter_draft, alter_page, alter_recipe, apply_draft,
    cached_model_catalogue, cancel_draft, discover_models, draft_page, draft_still,
    draft_thumbnail, forget_model_catalogue, generate_page, generate_recipe, import_events,
    import_frame, import_page, import_recipe, import_run_page, media_debug_frame, media_debug_page,
    media_debug_run_page, media_debug_start,
};
use htmx::{Flash, flash_redirect};
use media::MediaEvidence;
use recipes::{
    add_block, candidate_thumbnail, choose_thumbnail, delete_block, delete_page, delete_recipe,
    delete_version, discard_thumbnail, home, move_block, new_recipe, recipe_page, recipe_still,
    recipe_thumbnail, remove_thumbnail, start_moment_capture, start_thumbnail_recompute,
    update_block, update_recipe, upload_thumbnail, view_version,
};

const DRAFT_HOURS: i64 = 24;
const DEFAULT_MODEL: &str = "gpt-6-luna";
const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-5-5";
const DEFAULT_OPENAI_MODEL: &str = "gpt-5.6-luna";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5-5";
const DEFAULT_REASONING_EFFORT: &str = "low";
/// Reasoning efforts offered for the model, matching the worker protocol.
/// Newer Codex models also support `xhigh` and `max`; Anthropic endpoints
/// cap those values at their highest available thinking budget in the worker.
const REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// Fallback for the Codex model select, used only when the worker's live
/// `listModels` discovery is unavailable (no credential or refresh failure).
const MODEL_OPTIONS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-6-astra",
    "gpt-5.6-terra",
    "gpt-5.6-sol",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.3-codex-spark",
];
/// Fallback for the Claude model select, used only when live discovery is
/// unavailable. Newest first.
const CLAUDE_MODEL_OPTIONS: &[&str] = &[
    "claude-sonnet-5-5",
    "claude-opus-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
    "claude-sonnet-5",
    "claude-opus-5",
];
const RECIPE_PROMPT: &str = r#"You are a recipe assistant. Prefer credible guidance and synthesize practical advice without copying source text. Return one complete recipe, not a patch. Before responding, silently audit the recipe's quantities: choose amounts that are realistic for the stated servings, technique, and intended result; give ingredients useful, unambiguous kitchen measurements; and verify that each step's ingredientUses allocate the listed total of that ingredient exactly once. When an ingredient is divided between steps, use explicit portions that add up to its ingredient-list amount, using consistent units where practical. Do not invent unused ingredients or use vague quantities when a measured amount is needed for a reliable result. Every ingredient must be used exactly through one or more ingredientUses. Each step has a concise chartLabel (2–6 words), a timerSeconds value (0 when untimed; use the midpoint of a range), ingredientUses containing canonical zero-based ingredient indices plus exact amounts used at that step, and inputSteps containing only earlier step indices whose outputs are combined. Give every non-final step at most one consumer; make every step flow into the final step. Ingredient-free preparation steps are allowed. Give heating, simmering, baking, frying, resting, chilling, and reducing steps realistic duration and a clear doneness cue. Keep total timings consistent. Notice when the user's request excludes an ingredient — "egg free", "no dairy", "vegan", "gluten free", "without X" — and do not silently drop it. Judge whether the ingredient is genuinely optional; when it plays a structural role (binding, leavening, emulsifying, thickening, moisture, or browning), substitute a suitable alternative from sound cooking knowledge (such as flax or chia "eggs", aquafaba, plant milks and butters, or xanthan gum) and adjust technique so the recipe still achieves its intended result. Be concise and practical for a home cook."#;
const GROUNDED_RECIPE_PROMPT: &str = r#"You are a meticulous recipe research assistant. Research before generating. Always use the web_search tool. When the user supplies one or more URLs, search for their published title, author, and recipe details. When the user names an existing recipe, cook, book, restaurant, or website, locate the likely original and cross-check it against at least two independent, credible sources when available.

Study ingredient ratios, ingredient roles, preparation, sequencing, temperatures, timings, texture cues, and the intended final result. Recreate the referenced recipe faithfully enough to achieve that result, while resolving omissions or contradictions with sound cooking knowledge. Do not copy expressive prose, headnotes, or distinctive wording; write original, concise instructions. Preserve attribution through citations rather than imitation of the author's voice.

When the user asks to exclude an ingredient — "egg free", "no dairy", "vegan", "gluten free", "without nuts", and similar — do not silently drop it. First judge whether the ingredient is genuinely optional: if it only adds flavour or is truly dispensable, omitting it is fine, but make that a conscious, reasoned decision. If it plays a structural role — binding, leavening, emulsifying, thickening, moisture, browning, or setting — research suitable replacement ingredients and techniques with web_search (for example flax or chia "eggs", aquafaba, mashed banana, plant milks and butters, nut-free butters, xanthan or psyllium gum) and adapt quantities and method so the recipe still achieves its intended result. Keep the recipe true to its original character: the substitution must preserve the outcome, not merely delete the ingredient.

Return one complete recipe, not a patch. Before responding, silently audit the recipe's quantities: choose amounts that are realistic for the stated servings, technique, and intended result; give ingredients useful, unambiguous kitchen measurements; and verify that each step's ingredientUses allocate the listed total of that ingredient exactly once. When an ingredient is divided between steps, use explicit portions that add up to its ingredient-list amount, using consistent units where practical. Do not invent unused ingredients or use vague quantities when a measured amount is needed for a reliable result. Every ingredient must be used exactly through one or more ingredientUses. Each step has a concise chartLabel (2–6 words), a timerSeconds value (0 when untimed; use the midpoint of a range), ingredientUses containing canonical zero-based ingredient indices plus exact amounts used at that step, and inputSteps containing only earlier step indices whose outputs are combined. Give every non-final step at most one consumer; make every step flow into the final step. Ingredient-free preparation steps are allowed. Give heating, simmering, baking, frying, resting, chilling, and reducing steps realistic duration and a clear doneness cue. Keep total timings consistent. Cite every source that materially informed the recipe."#;

/// System prompt for drafts built from imported social-video evidence (the
/// From Video importer and its alteration chain). The web_search tool is
/// attached but optional: research exists to fill gaps the evidence leaves,
/// so an answer drawn purely from sufficient evidence stays valid.
const EVIDENCE_RECIPE_PROMPT: &str = r#"You are a meticulous recipe assistant working from imported social-video evidence. The evidence was distilled automatically from a social video's caption, spoken audio, and on-screen text, so it can be incomplete, garbled, or one-sided. Treat it as the best available record of the original creator's intent, not as verified fact.

Build the recipe from the supplied evidence first. Where it leaves the recipe incomplete or implausible — missing quantities, servings, timings, temperatures, or steps that do not hang together — judge what the dish needs: reason from sound cooking knowledge, and use the web_search tool to study similar published recipes for the same dish when knowledge alone is not enough. Fill every such gap so the finished recipe works end to end, and keep it faithful to the ingredients, character, and method shown in the video. Do not copy expressive prose from researched sources; write original, concise instructions.

Return one complete recipe, not a patch. Before responding, silently audit the recipe's quantities: choose amounts that are realistic for the stated servings, technique, and intended result; give ingredients useful, unambiguous kitchen measurements; and verify that each step's ingredientUses allocate the listed total of that ingredient exactly once. When an ingredient is divided between steps, use explicit portions that add up to its ingredient-list amount, using consistent units where practical. Do not invent unused ingredients or use vague quantities when a measured amount is needed for a reliable result. Every ingredient must be used exactly through one or more ingredientUses. Each step has a concise chartLabel (2–6 words), a timerSeconds value (0 when untimed; use the midpoint of a range), ingredientUses containing canonical zero-based ingredient indices plus exact amounts used at that step, and inputSteps containing only earlier step indices whose outputs are combined. Give every non-final step at most one consumer; make every step flow into the final step. Ingredient-free preparation steps are allowed. Give heating, simmering, baking, frying, resting, chilling, and reducing steps realistic duration and a clear doneness cue. Keep total timings consistent. When web_search filled a gap, list those pages in sources; when the evidence alone was sufficient, return an empty sources array."#;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    model: String,
    pi_worker_path: String,
    auth_script_path: String,
    claude_auth_script_path: String,
    search_grounding: bool,
    codex_flows: Arc<Mutex<HashMap<String, CodexFlow>>>,
    claude_flows: Arc<Mutex<HashMap<String, ClaudeFlow>>>,
    /// Last discovered model catalogue per Pi provider id, so Codex and
    /// Claude lists never overwrite each other.
    model_catalogue: Arc<Mutex<HashMap<String, ModelCatalogue>>>,
    media_debug_runs: DebugRunMap,
    /// Recipe ids with a dish-photo frame run in flight; in-memory only, so
    /// a restart simply drops any "pending" state back to idle.
    thumbnail_jobs: Arc<Mutex<HashSet<String>>>,
    /// Recipe ids with a step-moment capture in flight; in-memory only, like
    /// `thumbnail_jobs`.
    moment_jobs: Arc<Mutex<HashSet<String>>>,
    /// EmbeddingGemma 2 sidecar client (`EMBEDDING_URL`); `None` turns off
    /// library search, video step moments, and dish-photo ranking.
    embedder: Option<embeddings::Embedder>,
}

/// Last successfully discovered model catalogue for one Pi provider; reused
/// until stale (see MODEL_CATALOGUE_TTL in ai.rs).
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

/// In-flight Claude "copy code" authorisation, keyed by flow id. Holds the
/// PKCE verifier server-side; it expires after CLAUDE_FLOW_TTL.
#[derive(Clone)]
struct ClaudeFlow {
    user_id: String,
    url: String,
    verifier: String,
    created: std::time::Instant,
}
const CLAUDE_FLOW_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// A registered AI endpoint (OpenAI-spec or Anthropic-spec) with its API key,
/// as stored in ai_endpoints. Shared into the AI pipeline; the key is sent to
/// the worker per request and never rendered into templates.
/// Not `Debug`: the key must never reach a log line through `{:?}`.
#[derive(Clone, FromRow)]
pub(crate) struct Endpoint {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) spec: String,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
    /// The endpoint rejected its key on the last call that used it.
    pub(crate) auth_failed: bool,
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
        "The AI service is not configured. Authorise Codex, connect your Claude subscription, or add an API endpoint from the Settings page."
    )]
    AiNotConfigured,
    #[error(
        "The video cleaner's AI provider is not connected. Choose one in Settings before importing a video."
    )]
    MediaCleanerNotConfigured,
    #[error("The AI service could not prepare a grounded recipe. Please try again.")]
    Ai,
    /// The provider refused the login: an expired or revoked subscription
    /// sign-in, or a rejected API key. The text says what to fix in Settings.
    #[error("{0}")]
    AiLoginRefused(String),
    #[error("Codex authorisation failed: {0}")]
    CodexAuth(String),
    #[error("Claude authorisation failed: {0}")]
    ClaudeAuth(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Template(#[from] askama::Error),
}
/// Shown in place of any internal detail for server-side failures.
const GENERIC_FAILURE: &str = "Something went wrong on our side. Please try again.";
/// Used only if the error page itself cannot be rendered.
const ERROR_PAGE_FALLBACK: &str = "<!doctype html><title>Recipe Repository</title><p>Something went wrong. <a href=\"/\">Back to library</a></p>";

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!(error = %self, "request failed");
        let (status, heading, message, settings_link) = match &self {
            Self::NotFound => (StatusCode::NOT_FOUND, "Not found", self.to_string(), false),
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                "Something needs attention",
                message.clone(),
                false,
            ),
            Self::AiNotConfigured => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "AI is not set up",
                self.to_string(),
                true,
            ),
            Self::MediaCleanerNotConfigured => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Video cleaner not set up",
                self.to_string(),
                true,
            ),
            Self::Ai => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Recipe not created",
                self.to_string(),
                false,
            ),
            Self::AiLoginRefused(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "AI sign-in needs attention",
                message.clone(),
                true,
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                GENERIC_FAILURE.into(),
                false,
            ),
        };
        let page = ErrorTemplate {
            heading: heading.into(),
            message,
            settings_link,
        }
        .render();
        match page {
            Ok(html) => (status, Html(html)).into_response(),
            Err(_) => (status, Html(ERROR_PAGE_FALLBACK)).into_response(),
        }
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
    /// Where the source video shows or talks about this step.
    moment: Option<MomentView>,
}
/// One step's matched moment in the source video, shown beside the step.
#[derive(Clone)]
struct MomentView {
    /// "m:ss" start of the moment.
    label: String,
    /// How the moment was matched, for the reader.
    source: String,
    /// Still-image URLs around the moment.
    stills: Vec<String>,
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
    moment: Option<MomentView>,
}
#[derive(Clone)]
struct ChartView {
    cells: Vec<ChartCell>,
    leaves: Vec<ChartLeaf>,
    unlinked: Vec<String>,
    detail: Option<ChartDetail>,
    step_count: usize,
}

/// Query string of the library page: `?q=` runs a search.
#[derive(Default, Deserialize)]
struct HomeQuery {
    q: Option<String>,
}
#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    recipes: Vec<crate::recipes::LibraryCard>,
    /// The trimmed search text (prefills the form); empty when not searching.
    query: String,
    /// True when `recipes` are ranked search results rather than the whole library.
    results: bool,
    /// Search status shown above the recipes (indexing progress, service problems).
    notice: String,
}
#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    error: String,
    username: String,
}
#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: String,
    username: String,
    /// Local path to return to after sign-in.
    next: String,
    /// The browser already has a session, so this sign-in adds an account.
    adding: bool,
}
#[derive(Template)]
#[template(path = "account_new.html")]
struct NewAccountTemplate {
    error: String,
    username: String,
}
#[derive(Template)]
#[template(path = "password_reset.html")]
struct ResetPasswordTemplate {
    error: String,
}
#[derive(Template)]
#[template(path = "display.html")]
struct DisplayTemplate {
    /// The mode saved in this browser: "auto", "colour" or "eink".
    mode: &'static str,
    /// The request comes from a Kindle browser, so Automatic means e-ink here.
    kindle: bool,
    saved: bool,
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
    has_photo: bool,
    thumb_state: String,
    can_pick_video_frames: bool,
    /// A step-moment capture for this recipe is running.
    moments_pending: bool,
    /// The recipe has a video source but no moment pool yet.
    can_find_moments: bool,
    thumb_options: Vec<DraftThumbView>,
    versions: Vec<recipes::VersionListRow>,
    servings_value: String,
    prep_value: String,
    cook_value: String,
}
#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    heading: String,
    message: String,
    settings_link: bool,
}
#[derive(Template)]
#[template(path = "draft_expired.html")]
struct DraftExpiredTemplate {}
#[derive(Template)]
#[template(path = "version.html")]
struct VersionTemplate {
    recipe: Recipe,
    version: i64,
    label: String,
    created_at: String,
    created_display: String,
    snapshot: RecipeSnapshot,
    servings_value: String,
    prep_value: String,
    cook_value: String,
}

/// The editable recipe content captured in version history. Derived chart
/// data and thumbnail bytes are intentionally excluded; they are not shown on
/// the read-only version page and duplicating images per edit would make the
/// history unbounded in size.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecipeSnapshot {
    pub(crate) schema_version: u8,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) servings: Option<i64>,
    pub(crate) prep_minutes: Option<i64>,
    pub(crate) cook_minutes: Option<i64>,
    pub(crate) ingredients: Vec<SnapshotIngredient>,
    pub(crate) steps: Vec<SnapshotStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SnapshotIngredient {
    pub(crate) name: String,
    pub(crate) quantity: String,
    pub(crate) unit: String,
    pub(crate) optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SnapshotStep {
    pub(crate) text: String,
    pub(crate) ingredients: Vec<String>,
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
    field_label: String,
    placeholder: String,
    show_tips: bool,
    button: String,
    cancel_url: String,
    error: String,
    prompt: String,
}
#[derive(Template)]
#[template(path = "add_recipe.html")]
struct AddRecipeTemplate {
    prompt_error: String,
    guidance: String,
    prompt: String,
    video_error: String,
    url: String,
    notes: String,
    use_description: bool,
    use_audio: bool,
    use_ocr: bool,
    video_mode: bool,
    cleaner_configured: bool,
}
#[derive(Template)]
#[template(path = "media_import_run.html")]
struct MediaImportRunTemplate {
    run_id: String,
    finished: bool,
    draft_url: Option<String>,
    use_description: bool,
    use_audio: bool,
    use_ocr: bool,
    evidence: DebugUrlView,
    /// What each evidence source found so far, in plain words.
    sources: Vec<ImportSourceView>,
    /// The add page with this URL and its sources filled in, for a retry.
    retry_url: String,
}
/// One evidence source on the import progress page.
pub(crate) struct ImportSourceView {
    /// Event channel name the page script updates: description, audio, ocr.
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    /// waiting, found, none, or skipped.
    pub(crate) state: &'static str,
    pub(crate) detail: String,
}
#[derive(Template)]
#[template(path = "draft.html")]
struct DraftTemplate {
    id: String,
    recipe: GeneratedRecipe,
    sources: Vec<Source>,
    suggestions: String,
    evidence: Option<MediaEvidence>,
    thumbs: Vec<DraftThumbView>,
    /// The matched video moment of each step, in step order.
    step_moments: Vec<Option<MomentView>>,
    error: String,
    prompt: String,
}
#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    /// Username of the account whose AI setup the others share.
    shared_ai_owner: String,
    /// This account is the one whose AI setup is shared.
    owns_shared_ai: bool,
    /// This account uses the shared AI setup, so its own is not shown.
    uses_shared_ai: bool,
    /// What the shared setup generates with, e.g. "Claude (Pro/Max) · claude-opus-5-5".
    shared_ai_summary: String,
    /// The AI setup this account uses can generate recipes.
    ai_ready: bool,
    model_options: Vec<SelectOption>,
    claude_model_options: Vec<SelectOption>,
    effort_options: Vec<SelectOption>,
    codex_authorised: bool,
    codex_auth_url: String,
    codex_selected: bool,
    claude_authorised: bool,
    claude_auth_url: String,
    claude_selected: bool,
    endpoints: Vec<EndpointRow>,
    /// The "Add a provider" choices.
    presets: &'static [ProviderPreset],
    cleaner_configured: bool,
    /// The cleaner's provider value; empty follows the active AI provider.
    cleaner_provider: String,
    /// The active AI provider, which an empty cleaner provider follows.
    ai_provider: String,
    cleaner_model: String,
    /// Cached models for the cleaner's provider (the blank "provider's model"
    /// choice is in the template); the script replaces them live.
    cleaner_model_options: Vec<SelectOption>,
    /// One-time notices from the last save, each shown in its own block
    /// (empty for none); `ai_error` is the AI block's failure notice.
    ai_notice: &'static str,
    ai_error: &'static str,
    cleaner_notice: &'static str,
    moments_notice: &'static str,
    search_notice: &'static str,
    account_notice: &'static str,
    /// Whether video step moments are captured and shown (global setting).
    step_moments: bool,
    /// Whether the embedding sidecar is configured (`EMBEDDING_URL`).
    embeddings_configured: bool,
    /// The user's saved search choice: "embeddings", "bm25" or "shared".
    search_choice: &'static str,
    /// Whether the shared account searches with BM25 (what "shared" follows).
    shared_search_bm25: bool,
    /// The provider refused the stored Codex / Claude sign-in on its last use.
    codex_refused: bool,
    claude_refused: bool,
    /// The login the AI setup in use generates with was refused (shared or own).
    active_login_refused: bool,
}
struct SelectOption {
    value: String,
    selected: bool,
}
/// One saved AI endpoint as shown in Settings: name, masked key, and model.
/// The raw key never reaches the template.
struct EndpointRow {
    id: String,
    name: String,
    key_masked: String,
    /// Whether recipes from this endpoint can be researched on the web.
    web_search: bool,
    model: String,
    /// The spec default the endpoint runs when no model is stored.
    default_model: String,
    /// The last discovered models; the page script refreshes them live.
    model_options: Vec<String>,
    selected: bool,
    /// The endpoint rejected its key on the last call that used it.
    auth_failed: bool,
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
#[derive(Template)]
#[template(path = "claude_authorise.html")]
struct ClaudeAuthoriseTemplate {
    flow_id: String,
    url: String,
    expires_minutes: u64,
    error: String,
}
#[derive(Deserialize)]
struct ClaudeExchangeForm {
    flow_id: String,
    #[serde(default)]
    code: String,
}
#[derive(Deserialize)]
struct ModelsQuery {
    #[serde(default)]
    provider: String,
}
#[derive(Template)]
#[template(path = "media_debug.html")]
struct MediaDebugTemplate {
    error: String,
    urls_value: String,
    runs: Vec<MediaDebugRunRow>,
}
/// One recent debugger run as listed on the launcher page.
pub(crate) struct MediaDebugRunRow {
    pub(crate) id: String,
    pub(crate) url_count: usize,
    pub(crate) finished: bool,
    pub(crate) age_minutes: i64,
}
#[derive(Template)]
#[template(path = "media_debug_run.html")]
struct MediaDebugRunTemplate {
    run_id: String,
    finished: bool,
    urls: Vec<DebugUrlView>,
}
#[derive(Deserialize)]
struct MediaDebugForm {
    urls: String,
}
/// One URL's full three-phase review snapshot for the run page: description
/// captures, audio analysis, and OCR captures with their retained frames.
pub(crate) struct DebugUrlView {
    pub(crate) source_url: String,
    pub(crate) status: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) duration_seconds: Option<u64>,
    pub(crate) transcript: String,
    pub(crate) cleaned_recipe_text: String,
    pub(crate) warnings: Vec<String>,
    pub(crate) error_message: String,
    pub(crate) error_settings: bool,
    pub(crate) captures: Vec<MediaDebugCaptureView>,
    pub(crate) cards: Vec<MediaDebugCardView>,
}
pub(crate) struct MediaDebugCaptureView {
    pub(crate) seconds: u64,
    pub(crate) image_url: Option<String>,
    pub(crate) raw: String,
    pub(crate) cleaned: Option<String>,
    pub(crate) card: Option<usize>,
}
pub(crate) struct MediaDebugCardView {
    pub(crate) seconds: u64,
    pub(crate) text: String,
    pub(crate) kept: bool,
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
pub(crate) struct SetupForm {
    username: String,
    password: String,
    #[serde(default)]
    confirm_password: String,
}
#[derive(Deserialize)]
struct ResetPasswordForm {
    current_password: String,
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}
#[derive(Deserialize)]
pub(crate) struct LoginForm {
    username: String,
    password: String,
    #[serde(default)]
    next: String,
}
#[derive(Deserialize)]
struct LoginQuery {
    next: Option<String>,
    /// Prefills the username, e.g. from the account menu.
    username: Option<String>,
}
#[derive(Deserialize)]
pub(crate) struct SwitchAccountForm {
    user_id: String,
}
#[derive(Default, Deserialize)]
struct BlockForm {
    section: Option<String>,
    #[serde(default)]
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
/// Which dish-photo candidate (if any) the user chose on the draft preview.
#[derive(Deserialize)]
pub(crate) struct ApplyDraftForm {
    pub(crate) thumbnail: Option<String>,
}

/// Which frame candidate the user picked for an existing recipe.
#[derive(Deserialize)]
pub(crate) struct ThumbnailPickForm {
    pub(crate) choice: String,
}

/// One candidate offered on the draft preview; `index` is the storage slot.
pub(crate) struct DraftThumbView {
    pub(crate) index: usize,
    pub(crate) seconds: u64,
}

/// Shared image response for locally produced JPEG bytes. Cache headers stay
/// private: every thumbnail is behind the single-user sign-in.
pub(crate) fn image_response(jpeg: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "private, max-age=300"),
        ],
        jpeg,
    )
        .into_response()
}

#[derive(Deserialize)]
struct MediaImportForm {
    url: String,
    #[serde(default)]
    notes: String,
    // Tick boxes: present only when ticked, so None means "skip this channel".
    #[serde(default)]
    use_description: Option<String>,
    #[serde(default)]
    use_audio: Option<String>,
    #[serde(default)]
    use_ocr: Option<String>,
}
#[derive(Deserialize)]
struct SettingsForm {
    #[serde(default)]
    model: String, // Codex model
    #[serde(default)]
    claude_model: String, // Claude subscription model
    #[serde(default)]
    endpoint_model: String, // model for the endpoint named by endpoint_model_for
    #[serde(default)]
    endpoint_model_for: String, // the endpoint id whose model field was submitted
    #[serde(default)]
    reasoning_effort: String,
    #[serde(default)]
    provider: String, // "pi" (Codex), "claude" (Claude subscription) or an ai_endpoints row id
}
#[derive(Deserialize)]
struct EndpointForm {
    provider: String, // a PROVIDER_PRESETS key
    #[serde(default)]
    base_url: String, // custom servers only
    #[serde(default)]
    api_key: String, // optional only for a custom OpenAI-compatible server
    #[serde(default)]
    model: String, // custom servers only; blank uses the preset default
}
/// Settings form for the video step-moments switch: the checkbox is absent
/// when unticked.
/// Settings form for the AI-login choice: "own" or "shared".
#[derive(Deserialize)]
struct AiLoginsForm {
    mode: String,
}
/// Settings form for the library search engine: "embeddings", "bm25" or "shared".
#[derive(Deserialize)]
struct SearchEngineForm {
    engine: String,
}
#[derive(Deserialize)]
struct StepMomentsForm {
    #[serde(default)]
    enabled: Option<String>,
}
/// Settings form for the video cleaner: a provider value ("" follows the
/// active AI provider) and an optional model.
#[derive(Deserialize)]
struct MediaCleanerForm {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
}
#[derive(Deserialize)]
struct RecipeQuery {
    edit: Option<String>,
    edit_meta: Option<String>,
    view: Option<String>,
    step: Option<usize>,
}
#[derive(Deserialize)]
struct AddRecipeQuery {
    mode: Option<String>,
    /// Prefills the video URL (the retry link of a stopped import).
    url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhowless::Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|arg| arg == "--healthcheck") {
        return healthcheck().await;
    }
    if let Some(index) = args
        .iter()
        .position(|arg| arg == "--extract-media-evidence")
    {
        let url = args.get(index + 1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--extract-media-evidence requires a social-media URL",
            )
        })?;
        let evidence = media::extract_social_evidence(url, media::MediaChannels::default()).await?;
        println!("{}", serde_json::to_string_pretty(&evidence)?);
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--clean-ocr") {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        let value: Value = serde_json::from_str(&input)?;
        let snippets = media::clean_ocr_batch(&value, 0.5);
        println!("{}", serde_json::to_string_pretty(&snippets)?);
        return Ok(());
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
    crate::recipes::delete_expired_draft_thumbnails(&db, &stamp()).await?;
    let configured_model = env::var("PI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    retire_codex_models(&db, &configured_model).await?;
    reconcile_ai_provider(&db).await?;
    let removed_expired_drafts = sqlx::query("DELETE FROM ai_drafts WHERE expires_at < ?")
        .bind(stamp())
        .execute(&db)
        .await?
        .rows_affected();
    if removed_expired_drafts > 0 {
        info!(removed_expired_drafts, "Removed expired AI drafts");
    }
    let state = Arc::new(AppState {
        db,
        model: configured_model,
        pi_worker_path: env::var("PI_WORKER_PATH").unwrap_or_else(|_| "pi/recipe-worker.ts".into()),
        auth_script_path: env::var("PI_AUTH_SCRIPT_PATH")
            .unwrap_or_else(|_| "pi/codex-auth.ts".into()),
        claude_auth_script_path: env::var("PI_CLAUDE_AUTH_SCRIPT_PATH")
            .unwrap_or_else(|_| "pi/claude-auth.ts".into()),
        search_grounding: env_bool("PI_SEARCH_ENABLED", true),
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        claude_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(HashMap::new())),
        media_debug_runs: Arc::new(Mutex::new(HashMap::new())),
        thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
        moment_jobs: Arc::new(Mutex::new(HashSet::new())),
        embedder: embeddings::Embedder::from_env(),
    });
    if state.embedder.is_some() {
        tokio::spawn(search::backfill(state.clone()));
    }
    let app = routes(state).layer(TraceLayer::new_for_http());
    let addr: SocketAddr = env::var("APP_BIND")
        .unwrap_or_else(|_| "0.0.0.0:3000".into())
        .parse()?;
    info!(%addr, "Recipe Repository listening");
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
        .route("/recipes/{id}/thumbnail", get(recipe_thumbnail))
        .route("/recipes/new", get(new_recipe))
        .route("/recipes/{id}", get(recipe_page).post(update_recipe))
        .route(
            "/recipes/{id}/versions/{version}",
            get(view_version).delete(delete_version),
        )
        .route(
            "/recipes/{id}/versions/{version}/delete",
            post(delete_version),
        )
        .route("/recipes/{id}/delete", get(delete_page).post(delete_recipe))
        .route("/recipes/{id}/blocks", post(add_block))
        .route("/recipes/{id}/blocks/{block_id}", post(update_block))
        .route(
            "/recipes/{id}/blocks/{block_id}/move/{direction}",
            post(move_block),
        )
        .route("/recipes/{id}/blocks/{block_id}/delete", post(delete_block))
        .route("/ai/generate", get(generate_page).post(generate_recipe))
        .route(
            "/ai/import",
            get(import_page)
                .post(import_recipe)
                .layer(DefaultBodyLimit::max(128 * 1024)),
        )
        .route("/ai/import/{run_id}", get(import_run_page))
        .route("/ai/import/{run_id}/events", get(import_events))
        .route(
            "/ai/import/{run_id}/frames/{url_index}/{file}",
            get(import_frame),
        )
        .route("/recipes/{id}/ai/alter", get(alter_page).post(alter_recipe))
        .route("/ai/drafts/{id}", get(draft_page))
        .route("/ai/drafts/{id}/alter", post(alter_draft))
        .route("/ai/drafts/{id}/apply", post(apply_draft))
        .route("/ai/drafts/{id}/cancel", post(cancel_draft))
        .route("/ai/drafts/{id}/thumb/{idx}", get(draft_thumbnail))
        .route("/ai/drafts/{id}/stills/{seconds}", get(draft_still))
        .route(
            "/recipes/{id}/thumbnail/upload",
            post(upload_thumbnail).layer(DefaultBodyLimit::max(8 * 1024 * 1024)),
        )
        .route("/recipes/{id}/thumbnail/choose", post(choose_thumbnail))
        .route("/recipes/{id}/thumbnail/discard", post(discard_thumbnail))
        .route("/recipes/{id}/thumbnail/remove", post(remove_thumbnail))
        .route(
            "/recipes/{id}/thumbnail/recompute",
            post(start_thumbnail_recompute),
        )
        .route("/recipes/{id}/moments/capture", post(start_moment_capture))
        .route("/recipes/{id}/candidate/{idx}", get(candidate_thumbnail))
        .route("/recipes/{id}/stills/{seconds}", get(recipe_still))
        .route("/settings", get(settings_page).post(update_settings))
        .route("/settings/media-cleaner", post(update_media_cleaner))
        .route("/settings/step-moments", post(update_step_moments))
        .route("/settings/search-engine", post(update_search_engine))
        .route("/settings/ai-logins", post(update_ai_logins))
        .route("/settings/endpoints", post(add_endpoint))
        .route("/settings/endpoints/{id}/delete", post(delete_endpoint))
        .route("/settings/endpoints/{id}/check", post(check_endpoint))
        .route("/settings/check/{provider}", post(check_subscription))
        .route("/settings/disconnect/codex", post(disconnect_codex))
        .route("/settings/disconnect/claude", post(disconnect_claude))
        .route(
            "/settings/password",
            get(reset_password_page).post(reset_password),
        )
        .route("/accounts/new", get(new_account_page).post(create_account))
        .route("/accounts/switch", post(switch_account))
        .route(
            "/settings/display",
            get(display::display_page).post(display::update_display),
        )
        .route("/settings/authorise-codex", get(authorise_codex_start))
        .route(
            "/settings/authorise-codex/status/{flow_id}",
            get(authorise_codex_status),
        )
        .route("/settings/models", get(settings_models))
        .route(
            "/settings/authorise-claude",
            get(authorise_claude_start).post(authorise_claude_exchange),
        )
        .route(
            "/settings/media-debug",
            get(media_debug_page).post(media_debug_start),
        )
        .route("/settings/media-debug/{run_id}", get(media_debug_run_page))
        .route(
            "/settings/media-debug/{run_id}/frames/{url_index}/{file}",
            get(media_debug_frame),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/setup", get(setup_page).post(setup_create))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/logout/all", post(logout_all))
        .route("/display.css", get(display::display_css))
        .nest_service("/static", ServeDir::new("static"))
        .merge(protected)
        .layer(middleware::from_fn(htmx::htmx_layer))
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
    evidence_json: String,
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
/// A stored RFC3339 timestamp as a short UTC date, e.g. "15 Oct 2026, 22:11".
fn display_timestamp(stored: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(stored)
        .map(|time| {
            time.with_timezone(&Utc)
                .format("%-d %b %Y, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| stored.to_string())
}
fn env_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}
fn generate_guidance(search_grounding: bool) -> &'static str {
    if search_grounding {
        "Describe what you want to cook, name a recipe to recreate, or paste one or more recipe URLs. The assistant will research the web before preparing a complete draft."
    } else {
        "Describe what you want to cook or name a recipe to recreate. The assistant will prepare a complete draft for review."
    }
}

/// Provider value stored in `ai_provider` for the Codex (ChatGPT) subscription.
pub(crate) const PROVIDER_CODEX: &str = "pi";
/// Provider value stored in `ai_provider` for the Claude (Pro/Max) subscription.
pub(crate) const PROVIDER_CLAUDE: &str = "claude";
/// Pi SDK provider ids; also the `pi_credentials.provider` key and the
/// `piProvider` field sent to the worker.
pub(crate) const CODEX_PI_PROVIDER: &str = "openai-codex";
pub(crate) const CLAUDE_PI_PROVIDER: &str = "anthropic";
const OPENAI_SPEC: &str = "openai";
const ANTHROPIC_SPEC: &str = "anthropic";
/// A custom server speaking OpenAI Chat Completions, run through pi-ai.
pub(crate) const OPENAI_COMPATIBLE_SPEC: &str = "openai-compatible";

/// One choice in the Settings "Add a provider" picker. `spec` is what the
/// worker dispatches on: "openai" and "anthropic" run its native Responses and
/// Messages code (with web search); every other spec is a pi-ai provider id,
/// or "openai-compatible" for a custom server. pi-ai providers keep their own
/// base URL, so only the native and custom choices store one.
pub(crate) struct ProviderPreset {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) spec: &'static str,
    /// The native base URL; empty when pi-ai knows it or the user enters it.
    pub(crate) base_url: &'static str,
    /// Empty when the user must name the model.
    pub(crate) default_model: &'static str,
    /// Whether the user enters the base URL (and may enter a model).
    pub(crate) custom: bool,
}

const fn preset(
    key: &'static str,
    label: &'static str,
    default_model: &'static str,
) -> ProviderPreset {
    ProviderPreset {
        key,
        label,
        spec: key,
        base_url: "",
        default_model,
        custom: false,
    }
}

pub(crate) const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        key: OPENAI_SPEC,
        label: "OpenAI",
        spec: OPENAI_SPEC,
        base_url: "https://api.openai.com/v1",
        default_model: DEFAULT_OPENAI_MODEL,
        custom: false,
    },
    ProviderPreset {
        key: ANTHROPIC_SPEC,
        label: "Anthropic",
        spec: ANTHROPIC_SPEC,
        base_url: "https://api.anthropic.com",
        default_model: DEFAULT_ANTHROPIC_MODEL,
        custom: false,
    },
    preset("google", "Google Gemini", "gemini-3.8-flash"),
    preset("mistral", "Mistral", "mistral-large-latest"),
    preset("xai", "xAI (Grok)", "grok-4.7"),
    preset("deepseek", "DeepSeek", "deepseek-v4-pro"),
    preset("openrouter", "OpenRouter", "anthropic/claude-sonnet-5.5"),
    preset("groq", "Groq", "openai/gpt-oss-120b"),
    preset(
        "together",
        "Together AI",
        "deepseek-ai/DeepSeek-V4-Pro-0813",
    ),
    preset(
        "fireworks",
        "Fireworks",
        "accounts/fireworks/models/kimi-k3",
    ),
    preset("cerebras", "Cerebras", "gpt-oss-120b"),
    ProviderPreset {
        key: "custom-openai",
        label: "Other server (OpenAI-compatible)",
        spec: OPENAI_COMPATIBLE_SPEC,
        base_url: "",
        default_model: "",
        custom: true,
    },
    ProviderPreset {
        key: "custom-anthropic",
        label: "Other server (Anthropic-compatible)",
        spec: ANTHROPIC_SPEC,
        base_url: "",
        default_model: DEFAULT_ANTHROPIC_MODEL,
        custom: true,
    },
];

/// The preset a stored spec belongs to; the native preset wins over the
/// custom one that shares its spec.
fn preset_for_spec(spec: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|preset| preset.spec == spec)
}

/// Whether endpoints of this spec can run web search: only the worker's
/// native OpenAI and Anthropic paths attach the provider's search tool.
pub(crate) fn spec_has_web_search(spec: &str) -> bool {
    spec == OPENAI_SPEC || spec == ANTHROPIC_SPEC
}

/// Maps an `ai_provider` value to its Pi SDK provider id; None for API endpoints.
pub(crate) fn pi_provider_for(provider: &str) -> Option<&'static str> {
    match provider {
        PROVIDER_CODEX => Some(CODEX_PI_PROVIDER),
        PROVIDER_CLAUDE => Some(CLAUDE_PI_PROVIDER),
        _ => None,
    }
}

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
/// Persists (or replaces) the credential for one user and provider id. The
/// Vercel AI Gateway key shares this table under its own provider id.
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

const MEDIA_CLEANER_PROVIDER_SETTING: &str = "media_cleaner_provider";
const MEDIA_CLEANER_MODEL_SETTING: &str = "media_cleaner_model";

/// The video cleaner's own provider ("pi", "claude", or an endpoint id);
/// None follows the active AI provider.
pub(crate) async fn media_cleaner_setting(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Option<String>> {
    Ok(app_setting(db, user_id, MEDIA_CLEANER_PROVIDER_SETTING)
        .await?
        .filter(|value| !value.trim().is_empty()))
}

/// The video cleaner's model; None uses its provider's model.
pub(crate) async fn media_cleaner_model(db: &SqlitePool, user_id: &str) -> Result<Option<String>> {
    Ok(app_setting(db, user_id, MEDIA_CLEANER_MODEL_SETTING)
        .await?
        .filter(|value| !value.trim().is_empty()))
}

/// One of the user's app_settings values, if it has been saved.
async fn app_setting(db: &SqlitePool, user_id: &str, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM app_settings WHERE user_id = ? AND key = ?")
            .bind(user_id)
            .bind(key)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(value,)| value))
}

/// Saves one of the user's app_settings values, replacing any previous one.
async fn set_app_setting(db: &SqlitePool, user_id: &str, key: &str, value: &str) -> Result<()> {
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

/// app_settings key of the user's "video step moments" switch.
const STEP_MOMENTS_SETTING: &str = "step_moments";

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
const SEARCH_ENGINE_SETTING: &str = "search_engine";
const SEARCH_EMBEDDINGS: &str = "embeddings";
const SEARCH_BM25: &str = "bm25";
const SEARCH_SHARED: &str = "shared";

/// The user's own saved search choice: "embeddings", "bm25" or "shared".
async fn own_search_engine(db: &SqlitePool, user_id: &str) -> Result<&'static str> {
    Ok(
        match app_setting(db, user_id, SEARCH_ENGINE_SETTING)
            .await?
            .as_deref()
        {
            Some(SEARCH_EMBEDDINGS) => SEARCH_EMBEDDINGS,
            Some(SEARCH_BM25) => SEARCH_BM25,
            _ => SEARCH_SHARED,
        },
    )
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
const AI_LOGINS_SETTING: &str = "ai_logins";

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
async fn uses_own_ai(db: &SqlitePool, user_id: &str) -> Result<bool> {
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

/// All registered AI endpoints for one user, oldest first.
pub(crate) async fn list_endpoints(db: &SqlitePool, user_id: &str) -> Result<Vec<Endpoint>> {
    Ok(sqlx::query_as(
        "SELECT id,name,spec,base_url,api_key,model,auth_failed_at IS NOT NULL AS auth_failed FROM ai_endpoints WHERE user_id=? ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// The model an endpoint spec runs when the endpoint stores none; empty for a
/// custom OpenAI-compatible server, which has no sensible default.
fn spec_default_model(spec: &str) -> &'static str {
    preset_for_spec(spec).map_or("", |preset| preset.default_model)
}

/// The model an endpoint runs: its stored model, or the spec default when
/// none is stored.
pub(crate) fn endpoint_model(endpoint: &Endpoint) -> String {
    let model = endpoint.model.trim();
    if model.is_empty() {
        spec_default_model(&endpoint.spec).to_string()
    } else {
        model.to_string()
    }
}

/// One registered AI endpoint for one user, if it exists.
pub(crate) async fn find_endpoint(
    db: &SqlitePool,
    user_id: &str,
    id: &str,
) -> Result<Option<Endpoint>> {
    Ok(sqlx::query_as(
        "SELECT id,name,spec,base_url,api_key,model,auth_failed_at IS NOT NULL AS auth_failed FROM ai_endpoints WHERE user_id=? AND id=?",
    )
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

/// The tail of a stored API key, for display only; the full key is never
/// rendered into a template. Keys shorter than 8 characters show no tail at
/// all, so a short key can never leak in full.
fn mask_key(key: &str) -> String {
    const MASK: &str = "••••";
    if key.chars().count() < 8 {
        return MASK.into();
    }
    let tail: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{MASK}{tail}")
}

/// Heals stale ai_provider settings at startup: the pre-endpoint "openai"
/// value or an id that is not one of the user's endpoints resolves to their
/// first registered endpoint, falling back to Codex ("pi"). Missing settings
/// are left alone (the read path defaults to "pi").
async fn reconcile_ai_provider(db: &SqlitePool) -> Result<()> {
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
        if value != current {
            info!(%user_id, from = %current, to = %value, "Reconciled stale AI provider setting");
        }
    }
    Ok(())
}

/// One-time migration of a credential previously written by the Pi CLI to
/// `<agent dir>/auth.json` into the database. Only runs before the first user
/// exists; the setup transaction claims the orphan credential for that user.
async fn import_legacy_codex_auth(db: &SqlitePool) -> Result<()> {
    if auth::user_count(db).await? > 0 || pi_credential(db, "", CODEX_PI_PROVIDER).await?.is_some()
    {
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
    if let Some(credential) = value.get(CODEX_PI_PROVIDER) {
        store_pi_credential(db, "", CODEX_PI_PROVIDER, credential).await?;
        info!("Imported existing Codex credential into the database");
    }
    Ok(())
}

/// Whether the user has a stored OAuth credential for a Pi provider.
async fn pi_authorised(db: &SqlitePool, user_id: &str, pi_provider: &str) -> bool {
    match pi_credential(db, user_id, pi_provider).await {
        Ok(Some(credential)) => credential["type"].as_str() == Some("oauth"),
        Ok(None) => false,
        Err(error) => {
            warn!(%error, pi_provider, "Subscription credential read failed");
            false
        }
    }
}

async fn settings_page(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    flash: Flash,
) -> Result<Html<String>> {
    let (shared_id, shared_ai_owner) = shared_ai_account(&s.db).await?.unwrap_or_default();
    let owns_shared_ai = shared_id == user.id;
    let uses_shared_ai = !owns_shared_ai && !uses_own_ai(&s.db, &user.id).await?;
    let shared_ai_summary = if uses_shared_ai {
        ai_summary(&s, &shared_id).await?
    } else {
        String::new()
    };
    let codex_authorised = pi_authorised(&s.db, &user.id, CODEX_PI_PROVIDER).await;
    let claude_authorised = pi_authorised(&s.db, &user.id, CLAUDE_PI_PROVIDER).await;
    let provider = crate::ai_provider(&s.db, &user.id).await?;
    let codex_selected = provider == PROVIDER_CODEX;
    let claude_selected = provider == PROVIDER_CLAUDE;
    let endpoints = crate::list_endpoints(&s.db, &user.id)
        .await?
        .into_iter()
        .map(|endpoint| EndpointRow {
            selected: endpoint.id == provider,
            key_masked: mask_key(&endpoint.api_key),
            default_model: spec_default_model(&endpoint.spec).to_string(),
            web_search: spec_has_web_search(&endpoint.spec),
            model_options: cached_model_catalogue(&s, &endpoint.id),
            auth_failed: endpoint.auth_failed,
            id: endpoint.id,
            name: endpoint.name,
            model: endpoint.model,
        })
        .collect::<Vec<_>>();
    let codex_refused = subscription_login_refused(&s.db, &user.id, CODEX_PI_PROVIDER).await?;
    let claude_refused = subscription_login_refused(&s.db, &user.id, CLAUDE_PI_PROVIDER).await?;
    let ai_user = if uses_shared_ai {
        shared_id.as_str()
    } else {
        user.id.as_str()
    };
    let active_login_refused = active_login_refused(&s.db, ai_user).await?;
    let cleaner_configured = ai::media_cleaner_configured(&s, &user.id).await;
    let cleaner_provider = media_cleaner_setting(&s.db, &user.id)
        .await?
        .unwrap_or_default();
    let cleaner_model = media_cleaner_model(&s.db, &user.id)
        .await?
        .unwrap_or_default();
    let cleaner_source = if cleaner_provider.is_empty() {
        provider.as_str()
    } else {
        cleaner_provider.as_str()
    };
    let cleaner_model_options = cleaner_model_options(
        cached_model_catalogue(
            &s,
            pi_provider_for(cleaner_source).unwrap_or(cleaner_source),
        ),
        &cleaner_model,
    );
    // First paint shows the cached catalogues; the page's script then replaces
    // the options from /settings/models with live discovery.
    let model = selected_model(&s.db, &user.id, &s.model).await?;
    let claude_model = selected_claude_model(&s.db, &user.id).await?;
    let codex_models = if codex_authorised {
        cached_model_catalogue(&s, CODEX_PI_PROVIDER)
    } else {
        Vec::new()
    };
    let claude_models = if claude_authorised {
        cached_model_catalogue(&s, CLAUDE_PI_PROVIDER)
    } else {
        Vec::new()
    };
    render(SettingsTemplate {
        shared_ai_owner,
        owns_shared_ai,
        uses_shared_ai,
        shared_ai_summary,
        ai_ready: ai::ai_configured(&s, &user).await,
        model_options: model_options(codex_models, &model, MODEL_OPTIONS),
        claude_model_options: model_options(claude_models, &claude_model, CLAUDE_MODEL_OPTIONS),
        effort_options: effort_options(
            &selected_effort(&s.db, &user.id, DEFAULT_REASONING_EFFORT).await?,
        ),
        codex_authorised,
        codex_auth_url: "/settings/authorise-codex".into(),
        codex_selected,
        claude_authorised,
        claude_auth_url: "/settings/authorise-claude".into(),
        claude_selected,
        endpoints,
        presets: PROVIDER_PRESETS,
        cleaner_configured,
        cleaner_provider,
        ai_provider: provider.clone(),
        cleaner_model,
        cleaner_model_options,
        ai_notice: match flash.0.as_str() {
            "provider_saved" | "logins_saved" => "Settings saved.",
            "provider_added" => "Provider added.",
            "provider_removed" => "Provider removed.",
            "disconnected" => "Subscription disconnected.",
            "claude_connected" => "Claude connected.",
            "codex_login_works" => "ChatGPT (Codex) sign-in works.",
            "claude_login_works" => "Claude sign-in works.",
            "endpoint_key_works" => "The API key works.",
            _ => "",
        },
        ai_error: match flash.0.as_str() {
            "claude_code_missing" => "Paste the code Claude showed you.",
            "claude_expired" => "This sign-in expired. Start the authorisation again.",
            "login_refused" => {
                "The provider refused that sign-in: it has expired or was revoked. Reconnect it (or replace its key) below."
            }
            "login_check_failed" => {
                "That sign-in could not be checked right now. It may still work; check again later."
            }
            _ => "",
        },
        cleaner_notice: if flash.is("cleaner_saved") {
            "Video cleaner saved."
        } else {
            ""
        },
        moments_notice: if flash.is("moments_saved") {
            "Step moments saved."
        } else {
            ""
        },
        search_notice: if flash.is("search_saved") {
            "Search saved."
        } else {
            ""
        },
        account_notice: if flash.is("password_changed") {
            "Password changed."
        } else {
            ""
        },
        step_moments: step_moments_enabled(&s.db, &user.id).await?,
        embeddings_configured: s.embedder.is_some(),
        search_choice: match own_search_engine(&s.db, &user.id).await? {
            SEARCH_SHARED if owns_shared_ai => SEARCH_EMBEDDINGS,
            choice => choice,
        },
        shared_search_bm25: !owns_shared_ai
            && own_search_engine(&s.db, &shared_id).await? == SEARCH_BM25,
        codex_refused,
        claude_refused,
        active_login_refused,
    })
}

/// Whether the provider refused the login an account's AI setup generates
/// with (its active subscription or endpoint).
async fn active_login_refused(db: &SqlitePool, user_id: &str) -> Result<bool> {
    let provider = ai_provider(db, user_id).await?;
    match pi_provider_for(&provider) {
        Some(pi_provider) => subscription_login_refused(db, user_id, pi_provider).await,
        None => Ok(find_endpoint(db, user_id, &provider)
            .await?
            .is_some_and(|endpoint| endpoint.auth_failed)),
    }
}

/// Refreshes the user's own Codex or Claude sign-in now, so an expired or
/// revoked login is found (and flagged) before a generation needs it.
async fn check_subscription(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(provider): Path<String>,
) -> Result<Response> {
    let (pi_provider, works) = match provider.as_str() {
        "codex" => (CODEX_PI_PROVIDER, "codex_login_works"),
        "claude" => (CLAUDE_PI_PROVIDER, "claude_login_works"),
        _ => return Err(AppError::NotFound),
    };
    let check = match ai::check_subscription_login(&s, &user.id, pi_provider).await {
        Ok(check) => check,
        Err(AppError::AiNotConfigured) => {
            return Ok(axum::response::Redirect::to("/settings#settings-ai").into_response());
        }
        Err(error) => {
            warn!(%error, pi_provider, "Login check could not run");
            ai::LoginCheck::Failed
        }
    };
    Ok(login_check_redirect(check, works))
}

/// Tests one of the user's endpoint keys by listing the endpoint's models.
async fn check_endpoint(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    let endpoint = find_endpoint(&s.db, &user.id, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let check = match ai::check_endpoint_login(&s, &endpoint).await {
        Ok(check) => check,
        Err(error) => {
            warn!(%error, endpoint = %endpoint.name, "Login check could not run");
            ai::LoginCheck::Failed
        }
    };
    Ok(login_check_redirect(check, "endpoint_key_works"))
}

/// Back to the AI block with the check's notice. A refused login also shows
/// on its own row, which names it.
fn login_check_redirect(check: ai::LoginCheck, works: &str) -> Response {
    let key = match check {
        ai::LoginCheck::Works => works,
        ai::LoginCheck::Refused => "login_refused",
        ai::LoginCheck::Failed => "login_check_failed",
    };
    flash_redirect("/settings#settings-ai", key)
}

/// Model options for the Settings select: the discovered catalogue when
/// available, otherwise the provider's fallback list. The current selection is
/// always included (marked selected) so saving never silently changes the model.
fn model_options(fresh: Vec<String>, current: &str, fallback: &[&str]) -> Vec<SelectOption> {
    let mut values = if fresh.is_empty() {
        fallback.iter().map(|model| (*model).into()).collect()
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

/// Model options for the video cleaner select: the cached catalogue with the
/// saved model kept first when it is missing. Empty means the provider's own
/// model, which the template offers as its first option.
fn cleaner_model_options(mut values: Vec<String>, current: &str) -> Vec<SelectOption> {
    if !current.is_empty() && !values.iter().any(|value| value == current) {
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

/// Effort options for the Settings select, with the current choice selected.
fn effort_options(current: &str) -> Vec<SelectOption> {
    REASONING_EFFORTS
        .iter()
        .map(|value| SelectOption {
            selected: *value == current,
            value: (*value).into(),
        })
        .collect()
}

/// Models for one subscription provider: the live catalogue when discovery
/// yields one, otherwise the provider's built-in fallback list.
async fn provider_models(s: &Arc<AppState>, user_id: &str, pi_provider: &str) -> Vec<String> {
    let source = ModelSource::Subscription {
        user_id,
        pi_provider,
    };
    let discovered = discover_models(s, &source).await;
    if !discovered.is_empty() {
        return discovered;
    }
    let fallback = if pi_provider == CLAUDE_PI_PROVIDER {
        CLAUDE_MODEL_OPTIONS
    } else {
        MODEL_OPTIONS
    };
    fallback.iter().map(|model| (*model).to_string()).collect()
}

/// JSON model list for the Settings model fields, awaiting live discovery.
/// `provider` is "pi", "claude" or one of the user's endpoint ids. An endpoint
/// has no fallback list: its field accepts any model id, so a failed
/// discovery returns no suggestions.
async fn settings_models(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Query(query): Query<ModelsQuery>,
) -> Result<Json<Value>> {
    let (models, selected) = match pi_provider_for(&query.provider) {
        Some(pi_provider) => {
            let models = provider_models(&s, &user.id, pi_provider).await;
            let selected = if pi_provider == CLAUDE_PI_PROVIDER {
                selected_claude_model(&s.db, &user.id).await?
            } else {
                selected_model(&s.db, &user.id, &s.model).await?
            };
            (models, selected)
        }
        None => {
            let endpoint = find_endpoint(&s.db, &user.id, &query.provider)
                .await?
                .ok_or_else(|| AppError::BadRequest("Unknown model provider.".into()))?;
            let models = discover_models(&s, &ModelSource::Endpoint(&endpoint)).await;
            (models, endpoint_model(&endpoint))
        }
    };
    Ok(Json(json!({ "models": models, "selected": selected })))
}

async fn update_settings(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<SettingsForm>,
) -> Result<Response> {
    let provider = trim(&form.provider);
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
    if provider == PROVIDER_CODEX {
        save_subscription_model(&s, &user, CODEX_PI_PROVIDER, "model", &form.model, "Codex")
            .await?;
    } else if provider == PROVIDER_CLAUDE {
        if !pi_authorised(&s.db, &user.id, CLAUDE_PI_PROVIDER).await {
            return Err(AppError::BadRequest(
                "Connect your Claude subscription before selecting it.".into(),
            ));
        }
        save_subscription_model(
            &s,
            &user,
            CLAUDE_PI_PROVIDER,
            "claude_model",
            &form.claude_model,
            "Claude",
        )
        .await?;
    } else {
        crate::find_endpoint(&s.db, &user.id, &provider)
            .await?
            .filter(|endpoint| preset_for_spec(&endpoint.spec).is_some())
            .ok_or_else(|| AppError::BadRequest("Choose a saved API endpoint.".into()))?;
        // The model field belongs to the endpoint that was shown; without the
        // page script a different radio may have been picked meanwhile. Any
        // model id is accepted: an endpoint may serve models it does not list.
        let model = trim(&form.endpoint_model);
        if !model.is_empty() && trim(&form.endpoint_model_for) == provider {
            update_endpoint_model(&s.db, &user.id, &provider, &model).await?;
        }
    }
    // Saved only once the provider and model are valid, so a rejected form
    // leaves the previous settings untouched.
    set_app_setting(&s.db, &user.id, "reasoning_effort", &effort).await?;
    set_app_setting(&s.db, &user.id, "ai_provider", &provider).await?;
    Ok(flash_redirect("/settings#settings-ai", "provider_saved"))
}

/// Validates the model against the provider's discovered list (fallback list
/// when discovery is unavailable) and stores it under the provider's own
/// app_settings key, so each provider keeps its own choice. A blank value keeps
/// the stored model.
async fn save_subscription_model(
    s: &Arc<AppState>,
    user: &AuthUser,
    pi_provider: &str,
    setting_key: &str,
    submitted: &str,
    label: &str,
) -> Result<()> {
    let model = trim(submitted);
    if model.is_empty() {
        return Ok(());
    }
    if !provider_models(s, &user.id, pi_provider)
        .await
        .contains(&model)
    {
        return Err(AppError::BadRequest(format!(
            "Choose a supported {label} model."
        )));
    }
    set_app_setting(&s.db, &user.id, setting_key, &model).await
}

/// Chooses the video cleaner's provider and model. A blank provider follows
/// the active AI provider; a blank model uses the provider's own model.
async fn update_media_cleaner(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<MediaCleanerForm>,
) -> Result<Response> {
    let provider = trim(&form.provider);
    let known = provider.is_empty()
        || pi_provider_for(&provider).is_some()
        || find_endpoint(&s.db, &user.id, &provider).await?.is_some();
    if !known {
        return Err(AppError::BadRequest(
            "Choose a provider for the video cleaner.".into(),
        ));
    }
    set_app_setting(&s.db, &user.id, MEDIA_CLEANER_PROVIDER_SETTING, &provider).await?;
    set_app_setting(
        &s.db,
        &user.id,
        MEDIA_CLEANER_MODEL_SETTING,
        &trim(&form.model),
    )
    .await?;
    Ok(flash_redirect(
        "/settings#settings-cleaner",
        "cleaner_saved",
    ))
}

/// What an account's AI setup generates with: the provider's label and its
/// model, e.g. "Claude (Pro/Max) · claude-opus-5-5".
async fn ai_summary(s: &AppState, user_id: &str) -> Result<String> {
    let provider = ai_provider(&s.db, user_id).await?;
    let (label, model) = match provider.as_str() {
        PROVIDER_CODEX => (
            "Codex (ChatGPT)".to_string(),
            selected_model(&s.db, user_id, &s.model).await?,
        ),
        PROVIDER_CLAUDE => (
            "Claude (Pro/Max)".to_string(),
            selected_claude_model(&s.db, user_id).await?,
        ),
        id => match find_endpoint(&s.db, user_id, id).await? {
            Some(endpoint) => (endpoint.name.clone(), endpoint_model(&endpoint)),
            None => ("Codex (ChatGPT)".to_string(), String::new()),
        },
    };
    Ok(if model.trim().is_empty() {
        label
    } else {
        format!("{label} · {model}")
    })
}

/// Chooses between the shared AI setup and the user's own. The user's own
/// providers, keys, and models are kept either way, so switching back and
/// forth loses nothing.
async fn update_ai_logins(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<AiLoginsForm>,
) -> Result<Response> {
    let value = match form.mode.trim() {
        "own" => "own",
        "shared" => "shared",
        _ => {
            return Err(AppError::BadRequest(
                "Choose shared or own AI logins.".into(),
            ));
        }
    };
    set_app_setting(&s.db, &user.id, AI_LOGINS_SETTING, value).await?;
    Ok(flash_redirect("/settings#settings-ai", "logins_saved"))
}

/// Switches video step moments on or off for the signed-in user. Off stops
/// their new imports from capturing a moment pool and hides moments on their
/// pages; stored moments stay, so switching back on shows them again.
async fn update_step_moments(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<StepMomentsForm>,
) -> Result<Response> {
    let value = if form.enabled.is_some() { "on" } else { "off" };
    set_app_setting(&s.db, &user.id, STEP_MOMENTS_SETTING, value).await?;
    Ok(flash_redirect(
        "/settings#settings-moments",
        "moments_saved",
    ))
}

/// Saves the signed-in user's library search engine. "shared" follows the
/// shared account's choice; the shared account itself picks one of the two.
async fn update_search_engine(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<SearchEngineForm>,
) -> Result<Response> {
    let value = match form.engine.as_str() {
        SEARCH_EMBEDDINGS | SEARCH_BM25 | SEARCH_SHARED => form.engine.as_str(),
        _ => return Err(AppError::BadRequest("Unknown search engine.".into())),
    };
    set_app_setting(&s.db, &user.id, SEARCH_ENGINE_SETTING, value).await?;
    Ok(flash_redirect("/settings#settings-search", "search_saved"))
}

/// A required http(s) base URL with any trailing slash removed.
fn normalise_url(raw: &str, name: &str) -> Result<String> {
    let url = required(raw, name)?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(AppError::BadRequest(format!(
            "{name} must start with http:// or https://."
        )));
    }
    Ok(url.trim_end_matches('/').to_string())
}

/// Adds an AI provider from the Settings picker and makes it the active one,
/// so adding a key is all it takes to start generating with it. A named
/// provider needs only its key; a custom server also needs its base URL. The
/// key is stored in the database, never in a config file, and only its masked
/// tail is ever rendered back to the page.
async fn add_endpoint(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<EndpointForm>,
) -> Result<Response> {
    let preset = PROVIDER_PRESETS
        .iter()
        .find(|preset| preset.key == form.provider.trim())
        .ok_or_else(|| AppError::BadRequest("Choose an AI provider.".into()))?;
    let (base_url, model) = if preset.custom {
        (
            normalise_url(&form.base_url, "Server URL")?,
            trim(&form.model),
        )
    } else {
        (preset.base_url.to_string(), String::new())
    };
    let api_key = trim(&form.api_key);
    if api_key.is_empty() && preset.spec != OPENAI_COMPATIBLE_SPEC {
        return Err(AppError::BadRequest("Enter the API key.".into()));
    }
    let name = endpoint_name(&s.db, &user.id, preset, &base_url).await?;
    let id = crate::insert_endpoint(
        &s.db,
        &user.id,
        &name,
        preset.spec,
        &base_url,
        &api_key,
        &model,
    )
    .await?;
    set_app_setting(&s.db, &user.id, "ai_provider", &id).await?;
    Ok(flash_redirect("/settings#settings-ai", "provider_added"))
}

/// A display name for a new endpoint: the provider's label, or a custom
/// server's host, numbered when the user already has one by that name.
async fn endpoint_name(
    db: &SqlitePool,
    user_id: &str,
    preset: &ProviderPreset,
    base_url: &str,
) -> Result<String> {
    let base = if preset.custom {
        let host = base_url.split("://").nth(1).unwrap_or(base_url);
        host.split('/').next().unwrap_or(host).to_string()
    } else {
        preset.label.to_string()
    };
    let taken: Vec<String> = list_endpoints(db, user_id)
        .await?
        .into_iter()
        .map(|endpoint| endpoint.name)
        .collect();
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("{base} {n}");
        n += 1;
    }
    Ok(name)
}

/// Removes a saved endpoint. If it was the active provider, generation falls
/// back to Codex ("pi") so the setting never dangles.
async fn delete_endpoint(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    crate::remove_endpoint(&s.db, &user.id, &id).await?;
    forget_model_catalogue(&s, &id);
    if ai_provider(&s.db, &user.id).await? == id {
        set_app_setting(&s.db, &user.id, "ai_provider", PROVIDER_CODEX).await?;
    }
    if media_cleaner_setting(&s.db, &user.id).await?.as_deref() == Some(id.as_str()) {
        set_app_setting(&s.db, &user.id, MEDIA_CLEANER_PROVIDER_SETTING, "").await?;
        set_app_setting(&s.db, &user.id, MEDIA_CLEANER_MODEL_SETTING, "").await?;
    }
    Ok(flash_redirect("/settings#settings-ai", "provider_removed"))
}

/// Codex models the pi SDK no longer serves. A stored choice of one of these
/// would fail every generation, so startup moves it to the default model.
const RETIRED_CODEX_MODELS: &[&str] = &["gpt-5.4-mini", "gpt-5.4"];

/// Replaces every user's stored Codex model that the pi SDK no longer serves
/// with the PI_MODEL default. A user without a stored choice already gets the
/// default from `selected_model`.
async fn retire_codex_models(db: &SqlitePool, model: &str) -> Result<()> {
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

/// Starts an OpenAI Codex device-code flow and shows the code to enter at
/// chatgpt.com/codex/device. The rendered page polls the status route below.
async fn authorise_codex_start(State(s): State<Arc<AppState>>, user: AuthUser) -> Result<Response> {
    let output = run_node_script(s.auth_script_path.clone(), "start", Vec::new())
        .await
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
    let output = run_node_script(s.auth_script_path.clone(), "poll", payload)
        .await
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
            store_pi_credential(&s.db, &user.id, CODEX_PI_PROVIDER, &credential).await?;
        }
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("credential");
    }
    Ok(Json(value))
}

/// Starts a Claude "copy code" flow: the helper builds the sign-in URL and
/// PKCE verifier; the verifier stays server-side until the user pastes the code.
async fn authorise_claude_start(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Response> {
    let output = run_node_script(s.claude_auth_script_path.clone(), "start", Vec::new())
        .await
        .map_err(|_| AppError::ClaudeAuth("could not start the authorisation flow".into()))?;
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        AppError::ClaudeAuth("unexpected response from the authorisation helper".into())
    })?;
    if !output.status.success() || value["status"].as_str() != Some("ok") {
        let message = value["message"]
            .as_str()
            .unwrap_or("authorisation could not be started");
        return Err(AppError::ClaudeAuth(message.to_string()));
    }
    let url = value["url"].as_str().ok_or_else(|| {
        AppError::ClaudeAuth("authorisation helper returned no sign-in URL".into())
    })?;
    let verifier = value["verifier"]
        .as_str()
        .ok_or_else(|| AppError::ClaudeAuth("authorisation helper returned no verifier".into()))?;
    let flow_id = Uuid::new_v4().to_string();
    {
        let mut flows = s.claude_flows.lock();
        flows.retain(|_, flow| flow.created.elapsed() < CLAUDE_FLOW_TTL);
        flows.insert(
            flow_id.clone(),
            ClaudeFlow {
                user_id: user.id.clone(),
                url: url.to_string(),
                verifier: verifier.to_string(),
                created: std::time::Instant::now(),
            },
        );
    }
    render(ClaudeAuthoriseTemplate {
        flow_id,
        url: url.to_string(),
        expires_minutes: CLAUDE_FLOW_TTL.as_secs() / 60,
        error: String::new(),
    })
    .map(IntoResponse::into_response)
}

/// Sends the user back to Settings with a Claude error notice, named by its
/// flash key (`settings_page` words it).
fn claude_failure_redirect(headers: &HeaderMap, key: &str) -> Response {
    htmx::finish_popup(headers, "/settings#settings-ai", key)
}

/// Completes a Claude flow: the pasted code (or redirect URL) is exchanged by
/// the helper using the stored verifier, and the credential is saved. The flow
/// is single-use once an exchange is attempted, because authorisation codes
/// are; an empty paste leaves the flow intact for a retry.
async fn authorise_claude_exchange(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    headers: HeaderMap,
    Form(form): Form<ClaudeExchangeForm>,
) -> Result<Response> {
    let input = trim(&form.code);
    if input.is_empty() {
        return Ok(claude_failure_redirect(&headers, "claude_code_missing"));
    }
    // The flow is only read here and removed once the exchange succeeds, so a
    // failed attempt leaves the sign-in page usable for another paste.
    let flow = {
        let mut flows = s.claude_flows.lock();
        flows.retain(|_, flow| flow.created.elapsed() < CLAUDE_FLOW_TTL);
        flows
            .get(&form.flow_id)
            .filter(|flow| flow.user_id == user.id)
            .cloned()
    };
    let Some(flow) = flow else {
        return Ok(claude_failure_redirect(&headers, "claude_expired"));
    };
    let payload = serde_json::to_vec(&json!({ "input": input, "verifier": flow.verifier }))
        .map_err(|_| AppError::ClaudeAuth("invalid flow state".into()))?;
    let value: Value =
        match run_node_script(s.claude_auth_script_path.clone(), "exchange", payload).await {
            Ok(output) => serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
                json!({ "status": "failed", "message": "unexpected response from the authorisation helper" })
            }),
            Err(error) => {
                warn!(%error, "Claude authorisation helper could not be run");
                json!({ "status": "failed" })
            }
        };
    if value["status"].as_str() == Some("complete")
        && let Some(credential) = value.get("credential")
    {
        store_pi_credential(&s.db, &user.id, CLAUDE_PI_PROVIDER, credential).await?;
        s.claude_flows.lock().remove(&form.flow_id);
        return Ok(htmx::finish_popup(
            &headers,
            "/settings#settings-ai",
            "claude_connected",
        ));
    }
    let message = value["message"]
        .as_str()
        .filter(|message| !message.is_empty())
        .unwrap_or("Claude authorisation could not be completed. Check the code and try again.");
    warn!(message, "Claude authorisation failed");
    let remaining = CLAUDE_FLOW_TTL.saturating_sub(flow.created.elapsed());
    render(ClaudeAuthoriseTemplate {
        flow_id: form.flow_id,
        url: flow.url,
        expires_minutes: (remaining.as_secs() / 60).max(1),
        error: message.to_string(),
    })
    .map(IntoResponse::into_response)
}

/// Removes one subscription credential and leaves the other provider's alone.
/// When the removed provider was active, Codex takes over if it is still
/// connected; otherwise the setting stays and generation reports that AI is
/// not set up.
async fn disconnect_subscription(
    s: &Arc<AppState>,
    user: &AuthUser,
    pi_provider: &str,
    provider: &str,
) -> Result<Response> {
    sqlx::query("DELETE FROM pi_credentials WHERE user_id=? AND provider=?")
        .bind(&user.id)
        .bind(pi_provider)
        .execute(&s.db)
        .await?;
    if ai_provider(&s.db, &user.id).await? == provider
        && pi_authorised(&s.db, &user.id, CODEX_PI_PROVIDER).await
    {
        set_app_setting(&s.db, &user.id, "ai_provider", PROVIDER_CODEX).await?;
    }
    Ok(flash_redirect("/settings#settings-ai", "disconnected"))
}

async fn disconnect_codex(State(s): State<Arc<AppState>>, user: AuthUser) -> Result<Response> {
    disconnect_subscription(&s, &user, CODEX_PI_PROVIDER, PROVIDER_CODEX).await
}

async fn disconnect_claude(State(s): State<Arc<AppState>>, user: AuthUser) -> Result<Response> {
    disconnect_subscription(&s, &user, CLAUDE_PI_PROVIDER, PROVIDER_CLAUDE).await
}

#[cfg(test)]
mod model_tests {
    use super::{CLAUDE_MODEL_OPTIONS, MODEL_OPTIONS, SelectOption, effort_options, model_options};

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
        let options = model_options(fresh, "gpt-5.6-sol", MODEL_OPTIONS);
        assert_eq!(values(&options), vec!["gpt-5.6-sol", "gpt-5.5"]);
        assert!(options[0].selected && !options[1].selected);
    }

    #[test]
    fn current_selection_survives_a_catalogue_that_dropped_it() {
        let options = model_options(vec!["gpt-5.6-sol".to_string()], "gpt-5.5", MODEL_OPTIONS);
        assert_eq!(values(&options), vec!["gpt-5.5", "gpt-5.6-sol"]);
        assert!(options[0].selected && !options[1].selected);
    }

    #[test]
    fn empty_fresh_list_falls_back_to_builtin() {
        let options = model_options(Vec::new(), "gpt-5.5", MODEL_OPTIONS);
        assert_eq!(values(&options), MODEL_OPTIONS);
    }

    #[test]
    fn claude_fallback_uses_claude_ids_only() {
        let options = model_options(Vec::new(), "claude-sonnet-5-5", CLAUDE_MODEL_OPTIONS);
        assert_eq!(values(&options), CLAUDE_MODEL_OPTIONS);
        assert!(!CLAUDE_MODEL_OPTIONS.contains(&"gpt-5.5"));
    }

    #[test]
    fn effort_options_offer_all_five_with_current_selected() {
        let options = effort_options("max");
        assert_eq!(
            values(&options),
            vec!["low", "medium", "high", "xhigh", "max"]
        );
        assert!(options[4].selected);
        assert!(options[..4].iter().all(|option| !option.selected));
        // An unknown stored value never marks an option selected.
        assert!(
            effort_options("extreme")
                .iter()
                .all(|option| !option.selected)
        );
    }
}

#[cfg(test)]
mod error_tests {
    use super::AppError;
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};

    async fn respond(error: AppError) -> (StatusCode, String) {
        let response = error.into_response();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn statuses_follow_the_error_mapping() {
        assert_eq!(respond(AppError::NotFound).await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            respond(AppError::BadRequest("Bad input.".into())).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            respond(AppError::Ai).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn server_failures_hide_internal_detail_inside_the_layout() {
        let (status, body) = respond(AppError::Internal("sqlite exploded at row 7".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body.contains("sqlite exploded"), "{body}");
        assert!(body.contains("Something went wrong on our side"), "{body}");
        assert!(body.contains("Back to library"), "{body}");
        assert!(body.contains("/static/app.css"), "{body}");
    }

    #[tokio::test]
    async fn missing_ai_setup_links_to_settings() {
        let (status, body) = respond(AppError::AiNotConfigured).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("href=\"/settings\""), "{body}");
    }
}

/// Runs a Node helper script on the blocking pool, writing `payload` to its
/// stdin and collecting stdout and stderr.
async fn run_node_script(
    script_path: String,
    subcommand: &'static str,
    payload: Vec<u8>,
) -> std::io::Result<std::process::Output> {
    tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new("node")
            .arg(&script_path)
            .arg(subcommand)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .expect("script stdin is piped")
            .write_all(&payload)?;
        child.wait_with_output()
    })
    .await
    .map_err(std::io::Error::other)?
}
// Keeps main's error type small without adding an application dependency solely for startup errors.
mod anyhowless {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(test)]
mod tests;
