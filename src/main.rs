use askama::Template;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
};
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::{HashMap, HashSet},
    env,
    io::Read,
    net::SocketAddr,
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
mod chat;
mod display;
mod embeddings;
mod folders;
mod groups;
mod htmx;
mod media;
mod media_worker;
mod moments;
mod notes;
mod recipes;
mod scale;
mod search;
mod settings;
mod thumbs;
use auth::{
    AuthUser, auth_middleware, create_account, delete_account, delete_account_page, login,
    login_page, logout, logout_all, new_account_page, reset_password, reset_password_page,
    setup_create, setup_page, switch_account,
};

use ai::{
    DebugRunMap, ModelSource, alter_draft, apply_draft, cached_model_catalogue, cancel_draft,
    discover_models, draft_page, draft_still, draft_thumbnail, forget_model_catalogue,
    generate_page, generate_recipe, import_events, import_frame, import_run_page,
    media_debug_frame, media_debug_page, media_debug_run_page, media_debug_start,
};
use htmx::{Flash, flash_redirect};
use media::MediaEvidence;
use recipes::{
    add_block, candidate_thumbnail, choose_thumbnail, delete_block, delete_page, delete_recipe,
    delete_version, discard_thumbnail, home, move_block, new_recipe, recipe_page, recipe_still,
    recipe_thumbnail, remove_thumbnail, restore_version, start_moment_capture,
    start_thumbnail_recompute, update_block, update_recipe, upload_thumbnail, view_version,
};
// The settings, credential, endpoint, and search-key accessors keep their
// crate-root names.
pub(crate) use settings::*;

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
    /// Last discovered model catalogue per source: a Pi provider id for a
    /// subscription or an endpoint id, so no two lists overwrite each other.
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
    /// Brute-force limits on sign-in, the password page, and account
    /// creation; in-memory only, so a restart forgets them.
    rate_limits: Arc<auth::RateLimits>,
    /// How group invites reach people: Resend mail, or the link shown to the
    /// inviter when `RESEND_API_KEY` is unset.
    invites: groups::Invites,
}

/// Last successfully discovered model catalogue for one source (a Pi provider
/// or an endpoint); reused until stale (see MODEL_CATALOGUE_TTL in ai.rs).
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

/// An account's search API key, which the recipe chat uses to search the web
/// for models that have no web search of their own.
#[derive(Clone, FromRow)]
pub(crate) struct WebSearchKey {
    /// A `WEB_SEARCH_PROVIDERS` key, e.g. `brave`.
    pub(crate) provider: String,
    pub(crate) api_key: String,
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
    #[error("The AI service could not prepare the recipe. Please try again.")]
    Ai,
    /// The provider refused the login: an expired or revoked subscription
    /// sign-in, or a rejected API key. The text says what to fix in Settings.
    #[error("{0}")]
    AiLoginRefused(String),
    #[error("Codex authorisation failed: {0}")]
    CodexAuth(String),
    #[error("Claude authorisation failed: {0}")]
    ClaudeAuth(String),
    #[error("This form was sent from another site, so it was refused.")]
    CrossOrigin,
    /// A brute-force limit refused the request (`auth::RateLimits`). The
    /// text says what was limited and how long to wait.
    #[error("{0}")]
    TooManyRequests(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Template(#[from] askama::Error),
}
/// Shown in place of any internal detail for server-side failures.
const GENERIC_FAILURE: &str = "Something went wrong on our side. Please try again.";
/// Used only if the error page itself cannot be rendered.
const ERROR_PAGE_FALLBACK: &str = "<!doctype html><title>Recipe Repository</title><p>Something went wrong. <a href=\"/\">Back to library</a></p>";

impl AppError {
    /// What the reader is told: the error's own text, or `GENERIC_FAILURE`
    /// for a server-side failure, whose detail belongs in the log only.
    fn user_message(&self) -> String {
        match self {
            Self::Internal(_)
            | Self::CodexAuth(_)
            | Self::ClaudeAuth(_)
            | Self::Database(_)
            | Self::Template(_) => GENERIC_FAILURE.into(),
            _ => self.to_string(),
        }
    }
}

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
            Self::CrossOrigin => (
                StatusCode::FORBIDDEN,
                "Request refused",
                format!("{self} Open the page here and try again."),
                false,
            ),
            Self::TooManyRequests(message) => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts",
                message.clone(),
                false,
            ),
            Self::AiLoginRefused(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "AI sign-in needs attention",
                message.clone(),
                true,
            ),
            Self::CodexAuth(_) | Self::ClaudeAuth(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Sign-in did not work",
                self.to_string(),
                true,
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                self.user_message(),
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
    /// One or two words for the plate's well; empty shows the initial.
    plate_name: String,
    /// The recipe has a dish photo (`thumbnail_jpeg`, never loaded here).
    has_photo: bool,
    /// Shared with the owner's group (`recipes.shared`).
    shared: bool,
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
    /// The quantity and unit as shown, scaled to the page's servings.
    shown_quantity: String,
}
impl ViewBlock {
    fn from_block(b: Block, edit: Option<&str>, scale: &scale::Scale) -> Self {
        let optional = b.optional();
        let editing = edit == Some(&b.id);
        let shown_quantity = format!("{} {}", scale.quantity(&b.quantity, &b.unit), b.unit)
            .trim()
            .to_string();
        Self {
            shown_quantity,
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

/// Query string of the library page: `?q=` runs a search, `?folder=`
/// opens one folder of the Folders or Cuisines view.
#[derive(Default, Deserialize)]
struct HomeQuery {
    q: Option<String>,
    folder: Option<String>,
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
    /// The sign's toggle: "az", "folders", "cuisines" or "group".
    view: &'static str,
    /// The user is in a group, so the toggle offers the Group view.
    in_group: bool,
    /// Every recipe of the library, whatever the page shows.
    total: usize,
    /// The Folders or Cuisines view's stacks; empty otherwise (and while a
    /// folder is open or a search runs).
    stacks: Vec<folders::Stack>,
    /// The folder whose tickets `recipes` are.
    open_folder: Option<folders::OpenFolder>,
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
    /// A one-time notice above the form (an account was just deleted).
    notice: &'static str,
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
#[template(path = "account_delete.html")]
struct DeleteAccountTemplate {
    username: String,
    /// Usernames of the group members whose generations run on this
    /// account's AI logins now (`groups::ai_dependents`).
    sharing: Vec<String>,
}
/// The page a group invite link opens (outside the sign-in wall).
#[derive(Template)]
#[template(path = "invite.html")]
struct InviteTemplate {
    token: String,
    /// The inviter's username; empty when the invite is gone.
    inviter: String,
    /// One of the `groups::INVITE_*` states.
    state: &'static str,
    /// The browser's active account, when signed in.
    viewer: String,
    /// Why creating an account through the invite failed.
    error: String,
    /// The username typed into the sign-up form.
    username: String,
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
    /// The highlighted version tab; 0 is Current.
    active_version: i64,
    servings_value: String,
    prep_value: String,
    cook_value: String,
    /// The recipe chat drawer; None when the user has the chat off.
    chat: Option<chat::ChatPanel>,
    /// The cook's notes on the recipe; None on a recipe shared by another
    /// member.
    notes: Option<notes::NotesPanel>,
    /// The servings the amounts are shown for (`?serves=`).
    scale: scale::Scale,
    /// The library folder and cuisine the recipe is filed in; None on a
    /// recipe shared by another member (filing is the owner's).
    filing: Option<folders::Filing>,
    /// The owner's username when a group member opens a recipe someone else
    /// shared, which shows read-only; empty on the owner's own page.
    shared_by: String,
    /// The owner's Share-with-group toggle: Some(shared) while the owner is
    /// in a group, None otherwise and for other members.
    share: Option<bool>,
}
/// Titles up to this many characters run around the top of the recipe
/// page's plate rim; longer ones would not fit and leave it plain.
const RIM_TITLE_MAX: usize = 26;

impl RecipeTemplate {
    /// A group member's view of someone else's shared recipe: no edits.
    fn read_only(&self) -> bool {
        !self.shared_by.is_empty()
    }

    /// What the plate's well shows when there is no dish photo: the plate
    /// name, else the title's initial.
    fn plate_text(&self) -> String {
        if self.recipe.plate_name.is_empty() {
            recipes::plate_initial(&self.recipe.title)
        } else {
            self.recipe.plate_name.clone()
        }
    }

    /// The well's text size as a fraction of the initial's (CSS `em`), so the
    /// longest word of a plate name fits across the well.
    fn plate_text_scale(&self) -> String {
        format!("{:.2}", recipes::plate_name_scale(&self.plate_text()))
    }

    /// The title as printed around the top of the plate rim, or empty when
    /// it is too long to fit.
    fn rim_title(&self) -> String {
        let title = self.recipe.title.trim();
        if title.chars().count() > RIM_TITLE_MAX {
            return String::new();
        }
        title.to_uppercase()
    }

    /// Servings and total time, as printed around the bottom of the rim,
    /// e.g. "SERVES 4  ★  35 MIN".
    fn rim_facts(&self) -> String {
        let mut facts = Vec::new();
        if let Some(servings) = self.scale.serves {
            facts.push(format!("SERVES {servings}"));
        }
        let time = recipes::total_time(self.recipe.prep_minutes, self.recipe.cook_minutes);
        if !time.is_empty() {
            facts.push(time.to_uppercase());
        }
        facts.join("  \u{2605}  ")
    }
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
    /// Every saved version, for the tabs above the card.
    versions: Vec<recipes::VersionListRow>,
    active_version: i64,
    /// The notes about this version.
    notes: notes::NotesPanel,
}
impl VersionTemplate {
    /// The version shown is the newest, the recipe as it is now, which
    /// cannot be deleted.
    fn is_newest(&self) -> bool {
        self.versions
            .first()
            .is_some_and(|row| row.version == self.version)
    }
}

/// The editable recipe content captured in version history. Derived chart
/// data and thumbnail bytes are intentionally excluded; they are not shown on
/// the read-only version page and duplicating images per edit would make the
/// history unbounded in size.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecipeSnapshot {
    pub(crate) schema_version: u8,
    pub(crate) title: String,
    /// The name on the plate ("" for none). None in versions stored without
    /// the field: restoring one keeps the recipe's plate name while the title
    /// stays the same, and clears it with another title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plate_name: Option<String>,
    pub(crate) description: String,
    pub(crate) servings: Option<i64>,
    pub(crate) prep_minutes: Option<i64>,
    pub(crate) cook_minutes: Option<i64>,
    pub(crate) ingredients: Vec<SnapshotIngredient>,
    pub(crate) steps: Vec<SnapshotStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SnapshotIngredient {
    pub(crate) name: String,
    pub(crate) quantity: String,
    pub(crate) unit: String,
    pub(crate) optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SnapshotStep {
    pub(crate) text: String,
    pub(crate) ingredients: Vec<String>,
}

/// The recipe as the chat's edit tools left it (the worker's
/// `RecipeProposal`): a snapshot whose ingredients and steps each name the
/// item of the snapshot they came from (`from`), or None when new.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecipeProposal {
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) description: String,
    pub(crate) servings: Option<i64>,
    pub(crate) prep_minutes: Option<i64>,
    pub(crate) cook_minutes: Option<i64>,
    pub(crate) ingredients: Vec<ProposedIngredient>,
    pub(crate) steps: Vec<ProposedStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ProposedIngredient {
    pub(crate) from: Option<usize>,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) quantity: String,
    #[serde(default)]
    pub(crate) unit: String,
    #[serde(default)]
    pub(crate) optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ProposedStep {
    pub(crate) from: Option<usize>,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) ingredients: Vec<String>,
}

impl RecipeProposal {
    /// Whether the proposal changes anything of `before` the cook would see.
    /// The chat never names the plate, so the plate name does not count.
    pub(crate) fn changes(&self, before: &RecipeSnapshot) -> bool {
        let before = RecipeSnapshot {
            plate_name: Some(String::new()),
            ..before.clone()
        };
        self.snapshot() != before
    }

    /// The proposed recipe as a snapshot, the way a saved version records it.
    /// The chat never names the plate, so the plate name is empty (Accept
    /// keeps the recipe's while the title stays).
    pub(crate) fn snapshot(&self) -> RecipeSnapshot {
        RecipeSnapshot {
            schema_version: 1,
            title: self.title.clone(),
            plate_name: Some(String::new()),
            description: self.description.clone(),
            servings: self.servings,
            prep_minutes: self.prep_minutes,
            cook_minutes: self.cook_minutes,
            ingredients: self
                .ingredients
                .iter()
                .map(|ingredient| SnapshotIngredient {
                    name: ingredient.name.clone(),
                    quantity: ingredient.quantity.clone(),
                    unit: ingredient.unit.clone(),
                    optional: ingredient.optional,
                })
                .collect(),
            steps: self
                .steps
                .iter()
                .map(|step| SnapshotStep {
                    text: step.text.clone(),
                    ingredients: step.ingredients.clone(),
                })
                .collect(),
        }
    }
}

/// A stored chat proposal: the recipe the chat saw and what it proposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChatProposal {
    pub(crate) before: RecipeSnapshot,
    pub(crate) after: RecipeProposal,
}

#[derive(Template)]
#[template(path = "delete.html")]
struct DeleteTemplate {
    recipe: Recipe,
}
#[derive(Template)]
#[template(path = "add_recipe.html")]
struct AddRecipeTemplate {
    error: String,
    guidance: String,
    /// Whether page links are researched and cited (worded on their slips).
    search_grounding: bool,
    prompt: String,
    use_description: bool,
    use_audio: bool,
    use_ocr: bool,
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
    /// Username of the group member whose AI setup this account uses; empty
    /// when it uses its own.
    shared_ai_owner: String,
    /// This account shares its AI setup with at least one group member.
    owns_shared_ai: bool,
    /// This account generates with a group member's AI setup (its own has
    /// no connected provider), so its cleaner and search key are not shown.
    uses_shared_ai: bool,
    /// What the shared setup generates with, e.g. "Claude (Pro/Max) · claude-opus-5-5".
    shared_ai_summary: String,
    /// The AI setup this account uses can generate recipes.
    ai_ready: bool,
    model_options: Vec<SelectOption>,
    claude_model_options: Vec<SelectOption>,
    effort_options: Vec<SelectOption>,
    codex_authorised: bool,
    codex_selected: bool,
    claude_authorised: bool,
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
    chat_notice: &'static str,
    web_search_notice: &'static str,
    search_notice: &'static str,
    account_notice: &'static str,
    /// The search APIs a key can be added for, as (key, label).
    web_search_providers: &'static [(&'static str, &'static str)],
    /// The user's own search key: its provider's label and masked tail
    /// (both empty when none is saved). The raw key never reaches the page.
    web_search_label: &'static str,
    web_search_key_masked: String,
    /// The search provider label of the member whose AI setup this account
    /// uses, empty when it has no key.
    shared_web_search: &'static str,
    /// Whether video step moments are captured and shown for this user.
    step_moments: bool,
    /// The units new recipes are generated in: "metric", "imperial" or "source".
    measurement: &'static str,
    units_notice: &'static str,
    /// Whether the user has the recipe chat (the Ask drawer) on.
    recipe_chat: bool,
    /// Whether the embedding sidecar is configured (`EMBEDDING_URL`).
    embeddings_configured: bool,
    /// The user's saved search choice: "embeddings", "bm25" or "shared".
    search_choice: &'static str,
    /// Whether the member whose AI setup this account uses searches with
    /// BM25 (what "shared" follows).
    shared_search_bm25: bool,
    /// The provider refused the stored Codex / Claude sign-in on its last use.
    codex_refused: bool,
    claude_refused: bool,
    /// The login the AI setup in use generates with was refused (shared or own).
    active_login_refused: bool,
    /// The Group block: members, invites, and AI sharing.
    group: groups::GroupPanel,
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
    #[serde(default)]
    plate_name: String,
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
    /// Prefills the username.
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
#[derive(Deserialize, Default)]
struct PromptForm {
    prompt: String,
    /// Sent by the Add recipe card along with its video tick boxes, so a
    /// missing box reads as unticked. A post without it reads every channel.
    #[serde(default)]
    video_options: Option<String>,
    // Tick boxes: present only when ticked.
    #[serde(default)]
    use_description: Option<String>,
    #[serde(default)]
    use_audio: Option<String>,
    #[serde(default)]
    use_ocr: Option<String>,
}

impl PromptForm {
    /// The parts of a linked video to read.
    fn channels(&self) -> media::MediaChannels {
        if self.video_options.is_none() {
            return media::MediaChannels::default();
        }
        media::MediaChannels {
            description: self.use_description.is_some(),
            audio: self.use_audio.is_some(),
            ocr: self.use_ocr.is_some(),
        }
    }
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

/// Shared image response for locally produced JPEG bytes. Every image is the
/// owning user's, so caching stays private. A dish photo, a candidate frame, or
/// a still can change behind the same URL, so the browser revalidates each
/// time: the ETag is a hash of the bytes, and `not_modified` answers 304 while
/// they are unchanged.
pub(crate) fn image_response(jpeg: Vec<u8>) -> Response {
    let digest = Sha256::digest(&jpeg);
    let etag: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (
        [
            (header::CONTENT_TYPE, "image/jpeg".to_string()),
            (header::CACHE_CONTROL, "private, no-cache".to_string()),
            (header::ETAG, format!("\"{etag}\"")),
        ],
        jpeg,
    )
        .into_response()
}

/// Turns a successful response into 304 Not Modified when the request's
/// If-None-Match names the response's ETag, so a revalidated image that has
/// not changed is not sent again. The session cookies the auth middleware set
/// on the response go along: a revalidation can be the request that extends
/// or replaces the session.
async fn not_modified(request: Request, next: Next) -> Response {
    let wanted = request.headers().get(header::IF_NONE_MATCH).cloned();
    let response = next.run(request).await;
    let (Some(wanted), Some(etag)) = (wanted, response.headers().get(header::ETAG)) else {
        return response;
    };
    if response.status() != StatusCode::OK || !etag_matches(&wanted, etag) {
        return response;
    }
    let mut unchanged = StatusCode::NOT_MODIFIED.into_response();
    for name in [header::ETAG, header::CACHE_CONTROL, header::SET_COOKIE] {
        for value in response.headers().get_all(&name) {
            unchanged.headers_mut().append(name.clone(), value.clone());
        }
    }
    unchanged
}

/// Whether an If-None-Match list names this ETag; weak and strong forms
/// compare equal, as RFC 9110 asks for If-None-Match.
fn etag_matches(wanted: &HeaderValue, etag: &HeaderValue) -> bool {
    let (Ok(wanted), Ok(etag)) = (wanted.to_str(), etag.to_str()) else {
        return false;
    };
    let opaque = |tag: &str| tag.trim().trim_start_matches("W/").to_string();
    let etag = opaque(etag);
    wanted
        .split(',')
        .any(|tag| tag.trim() == "*" || opaque(tag) == etag)
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
/// Settings form for the library search engine: "embeddings", "bm25" or "shared".
#[derive(Deserialize)]
struct SearchEngineForm {
    engine: String,
}
#[derive(Deserialize)]
struct MeasurementForm {
    system: String,
}
/// Settings form for an on/off switch: the checkbox is absent when unticked.
#[derive(Deserialize)]
struct SwitchForm {
    #[serde(default)]
    enabled: Option<String>,
}
#[derive(Deserialize)]
struct WebSearchForm {
    provider: String, // a WEB_SEARCH_PROVIDERS key
    #[serde(default)]
    api_key: String,
}
/// A recipe-chat question. `back` is the page the drawer was on.
#[derive(Deserialize)]
struct ChatForm {
    question: String,
    #[serde(default)]
    back: String,
}
/// A note's text and what it is about ("" whole recipe, "current", or a
/// version number), returning to `back`.
#[derive(Deserialize)]
struct NoteForm {
    #[serde(default)]
    body: String,
    #[serde(default)]
    about: String,
    #[serde(default)]
    back: String,
}
/// Opens ("on") or closes the chat drawer, or clears the chat, returning to `back`.
#[derive(Deserialize)]
struct ChatDrawerForm {
    #[serde(default)]
    open: String,
    #[serde(default)]
    back: String,
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
    #[serde(default, deserialize_with = "lenient_number")]
    step: Option<usize>,
    /// Servings to scale the amounts to; absent shows the recipe as written.
    #[serde(default, deserialize_with = "lenient_number")]
    serves: Option<i64>,
}
/// A number from the query string, or None when it is empty or not a
/// number, so a hand-edited `?serves=abc` shows the page as if it were
/// absent rather than failing it.
fn lenient_number<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
{
    let text: Option<String> = Option::deserialize(deserializer)?;
    Ok(text.and_then(|text| text.trim().parse().ok()))
}
#[derive(Deserialize)]
struct AddRecipeQuery {
    /// Prefills the card (the retry link of a stopped import).
    url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhowless::Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|arg| arg == "--healthcheck") {
        return healthcheck().await;
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
    ai::remove_leftover_temp_files(&env::temp_dir());
    let configured_model = env::var("PI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    crate::recipes::fail_interrupted_chats(&db).await?;
    tokio::spawn(prune_expired_drafts(db.clone()));
    let state = Arc::new(AppState {
        db,
        model: configured_model,
        pi_worker_path: "pi/recipe-worker.ts".into(),
        auth_script_path: "pi/codex-auth.ts".into(),
        claude_auth_script_path: "pi/claude-auth.ts".into(),
        search_grounding: env_bool("PI_SEARCH_ENABLED", true),
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        claude_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(HashMap::new())),
        media_debug_runs: Arc::new(Mutex::new(HashMap::new())),
        thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
        moment_jobs: Arc::new(Mutex::new(HashSet::new())),
        embedder: embeddings::Embedder::from_env(),
        rate_limits: Arc::new(auth::RateLimits::new(env_bool(
            "TRUST_PROXY_HEADERS",
            false,
        ))),
        invites: groups::Invites::new(
            env::var("RESEND_API_KEY").ok(),
            env::var("RESEND_FROM").ok(),
            env::var("PUBLIC_BASE_URL").ok(),
        ),
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
    // The peer address feeds the per-address sign-in limits.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
/// Deletes expired drafts now and every hour after, so their dish-photo
/// candidates and moment stills go soon after the draft expires rather than at
/// the next generation or restart. Expired group invites go on the same tick.
async fn prune_expired_drafts(db: SqlitePool) {
    let mut hourly = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
    loop {
        hourly.tick().await;
        match recipes::delete_expired_drafts(&db, &stamp()).await {
            Ok(0) => {}
            Ok(removed) => info!(removed, "Removed expired AI drafts"),
            Err(error) => warn!(%error, "Expired AI drafts could not be removed"),
        }
        match groups::prune_invites(&db).await {
            Ok(0) => {}
            Ok(removed) => info!(removed, "Removed expired group invites"),
            Err(error) => warn!(%error, "Expired group invites could not be removed"),
        }
    }
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
        .route("/library/view", post(folders::update_library_view))
        .route("/recipes/{id}/filing", post(folders::update_filing))
        .route("/recipes/{id}/share", post(groups::update_share))
        .route("/recipes/{id}/copy", post(recipes::copy_recipe))
        .route("/recipes/{id}/thumbnail", get(recipe_thumbnail))
        .route("/recipes/new", get(new_recipe))
        .route("/recipes/{id}", get(recipe_page).post(update_recipe))
        .route("/recipes/{id}/versions/{version}", get(view_version))
        .route(
            "/recipes/{id}/versions/{version}/delete",
            post(delete_version),
        )
        .route(
            "/recipes/{id}/versions/{version}/restore",
            post(restore_version),
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
        .route("/ai/import/{run_id}", get(import_run_page))
        .route("/ai/import/{run_id}/events", get(import_events))
        .route(
            "/ai/import/{run_id}/frames/{url_index}/{file}",
            get(import_frame),
        )
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
        .route("/recipes/{id}/chat", post(chat::ask))
        .route("/recipes/{id}/chat/drawer", post(chat::toggle_drawer))
        .route("/recipes/{id}/chat/clear", post(chat::clear))
        .route(
            "/recipes/{id}/chat/{message_id}/note",
            post(chat::keep_answer),
        )
        .route(
            "/recipes/{id}/chat/{message_id}/accept",
            post(chat::accept_change),
        )
        .route(
            "/recipes/{id}/chat/{message_id}/deny",
            post(chat::decline_change),
        )
        .route(
            "/recipes/{id}/notes",
            post(notes::add_note).layer(DefaultBodyLimit::max(8 * 1024 * 1024)),
        )
        .route("/recipes/{id}/notes/{note_id}", post(notes::update_note))
        .route(
            "/recipes/{id}/notes/{note_id}/delete",
            post(notes::delete_note),
        )
        .route(
            "/recipes/{id}/notes/{note_id}/photo",
            get(notes::note_photo),
        )
        .route("/recipes/{id}/candidate/{idx}", get(candidate_thumbnail))
        .route("/recipes/{id}/stills/{seconds}", get(recipe_still))
        .route("/settings", get(settings_page).post(update_settings))
        .route("/settings/media-cleaner", post(update_media_cleaner))
        .route("/settings/step-moments", post(update_step_moments))
        .route("/settings/measurements", post(update_measurements))
        .route("/settings/recipe-chat", post(update_recipe_chat))
        .route("/settings/web-search", post(add_web_search_key))
        .route("/settings/web-search/delete", post(delete_web_search_key))
        .route("/settings/search-engine", post(update_search_engine))
        .route("/settings/group/invite", post(groups::send_invite))
        .route(
            "/settings/group/invites/{id}/cancel",
            post(groups::cancel_invite),
        )
        .route("/settings/group/leave", post(groups::leave))
        .route("/settings/group/ai-grants", post(groups::update_ai_grants))
        .route("/settings/group/ai-source", post(groups::update_ai_source))
        .route("/invites/{token}/accept", post(groups::accept_invite))
        .route("/settings/endpoints", post(add_endpoint))
        .route("/settings/endpoints/{id}/delete", post(delete_endpoint))
        .route("/settings/endpoints/{id}/check", post(check_endpoint))
        .route("/settings/check/{provider}", post(check_subscription))
        .route(
            "/settings/disconnect/{provider}",
            post(disconnect_subscription),
        )
        .route(
            "/settings/password",
            get(reset_password_page).post(reset_password),
        )
        .route(
            "/settings/delete-account",
            get(delete_account_page).post(delete_account),
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
        .layer(middleware::from_fn(recipes::keep_recipe_view))
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
        .route("/invites/{token}", get(groups::invite_page))
        .route("/invites/{token}/decline", post(groups::decline_invite))
        .route(
            "/invites/{token}/signup",
            post(groups::signup_through_invite),
        )
        .route("/display.css", get(display::display_css))
        .nest_service("/static", ServeDir::new("static"))
        .merge(protected)
        .layer(middleware::from_fn(not_modified))
        .layer(middleware::from_fn(auth::same_origin_only))
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
    /// The dish in one or two words ("Roti", "Pad thai") for the plate on
    /// the recipe page. Older drafts have none.
    #[serde(rename = "plateName", default)]
    plate_name: String,
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
    recipe_json: String,
    sources_json: String,
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
/// "1 step", "3 steps".
fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
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
/// A boolean switch from the environment: 1/true/yes/on or 0/false/no/off in
/// any case. Unset, empty, or anything else keeps the default, so a typo never
/// flips a switch.
fn env_bool(name: &str, default: bool) -> bool {
    let value = env::var(name).unwrap_or_default();
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}
fn generate_guidance(search_grounding: bool) -> &'static str {
    if search_grounding {
        "An idea, a recipe to recreate, recipe links, or an Instagram or Facebook reel. Links are researched, a reel is read from the video, and you review the draft before it is saved."
    } else {
        "An idea, a recipe to recreate, or an Instagram or Facebook reel to read from the video. You review the draft before it is saved."
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

/// Whether an endpoint can run web search: only the worker's native OpenAI
/// and Anthropic paths attach the provider's search tool, and only the
/// provider's own API is sure to serve it. A custom Anthropic-compatible
/// server at another URL (a third-party API or a proxy) writes without
/// research rather than failing every grounded request for want of sources.
pub(crate) fn endpoint_has_web_search(spec: &str, base_url: &str) -> bool {
    (spec == OPENAI_SPEC || spec == ANTHROPIC_SPEC)
        && preset_for_spec(spec)
            .is_some_and(|preset| preset.base_url == base_url.trim_end_matches('/'))
}

/// Maps an `ai_provider` value to its Pi SDK provider id; None for API endpoints.
pub(crate) fn pi_provider_for(provider: &str) -> Option<&'static str> {
    match provider {
        PROVIDER_CODEX => Some(CODEX_PI_PROVIDER),
        PROVIDER_CLAUDE => Some(CLAUDE_PI_PROVIDER),
        _ => None,
    }
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

/// The search APIs a web search key can be for, as (key, label). Keep in step
/// with SEARCH_PROVIDERS in pi/web-search.ts, which holds each one's adapter.
const WEB_SEARCH_PROVIDERS: &[(&str, &str)] = &[
    ("brave", "Brave Search"),
    ("exa", "Exa"),
    ("tavily", "Tavily"),
];

/// The label of a web search provider key.
fn web_search_label(provider: &str) -> &'static str {
    WEB_SEARCH_PROVIDERS
        .iter()
        .find(|(key, _)| *key == provider)
        .map_or("Web search", |(_, label)| label)
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

/// Whether the user has a stored credential for a Pi provider. Any stored
/// credential counts, as it does for generation (`ensure_provider_configured`
/// in ai.rs); one the provider refuses is flagged on its row instead.
async fn pi_authorised(db: &SqlitePool, user_id: &str, pi_provider: &str) -> bool {
    match pi_credential(db, user_id, pi_provider).await {
        Ok(credential) => credential.is_some(),
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
    settings_view(&s, &user, &flash, groups::GroupExtras::default()).await
}

/// The Settings page for the signed-in user. `extras` carries what only the
/// request that made it can show in the Group block (a new invite's link, a
/// refused invite), so the invite form answers with this page directly.
pub(crate) async fn settings_view(
    s: &Arc<AppState>,
    user: &AuthUser,
    flash: &Flash,
    extras: groups::GroupExtras,
) -> Result<Html<String>> {
    // The account whose AI setup serves this one: its own, or a group
    // member's it was granted while its own has no connected provider.
    let shared_id = ai_account(&s.db, &user.id).await?;
    let uses_shared_ai = shared_id != user.id;
    let shared_ai_owner: String = if uses_shared_ai {
        sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
            .bind(&shared_id)
            .fetch_optional(&s.db)
            .await?
            .unwrap_or_default()
    } else {
        String::new()
    };
    let owns_shared_ai = !ai_grantees(&s.db, &user.id).await?.is_empty();
    let shared_ai_summary = if uses_shared_ai {
        ai_summary(s, &shared_id).await?
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
            web_search: endpoint_has_web_search(&endpoint.spec, &endpoint.base_url),
            model_options: cached_model_catalogue(s, &endpoint.id),
            auth_failed: endpoint.auth_failed,
            id: endpoint.id,
            name: endpoint.name,
            model: endpoint.model,
        })
        .collect::<Vec<_>>();
    let codex_refused = subscription_login_refused(&s.db, &user.id, CODEX_PI_PROVIDER).await?;
    let claude_refused = subscription_login_refused(&s.db, &user.id, CLAUDE_PI_PROVIDER).await?;
    let active_login_refused = active_login_refused(&s.db, &shared_id).await?;
    let own_web_search = web_search_key(&s.db, &user.id).await?;
    let shared_web_search = if uses_shared_ai {
        web_search_key(&s.db, &shared_id)
            .await?
            .map_or("", |key| web_search_label(&key.provider))
    } else {
        ""
    };
    let cleaner_configured = ai::media_cleaner_configured(s, &user.id).await;
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
    let cleaner_model_options = select_options(
        cached_model_catalogue(s, pi_provider_for(cleaner_source).unwrap_or(cleaner_source)),
        &cleaner_model,
    );
    // First paint shows the cached catalogues; the page's script then replaces
    // the options from /settings/models with live discovery.
    let model = selected_model(&s.db, &user.id, &s.model).await?;
    let claude_model = selected_claude_model(&s.db, &user.id).await?;
    let codex_models = if codex_authorised {
        cached_model_catalogue(s, CODEX_PI_PROVIDER)
    } else {
        Vec::new()
    };
    let claude_models = if claude_authorised {
        cached_model_catalogue(s, CLAUDE_PI_PROVIDER)
    } else {
        Vec::new()
    };
    render(SettingsTemplate {
        shared_ai_owner,
        owns_shared_ai,
        uses_shared_ai,
        shared_ai_summary,
        ai_ready: ai::ai_configured(s, user).await,
        model_options: model_options(codex_models, &model, MODEL_OPTIONS),
        claude_model_options: model_options(claude_models, &claude_model, CLAUDE_MODEL_OPTIONS),
        effort_options: effort_options(
            &selected_effort(&s.db, &user.id, DEFAULT_REASONING_EFFORT).await?,
        ),
        codex_authorised,
        codex_selected,
        claude_authorised,
        claude_selected,
        endpoints,
        presets: PROVIDER_PRESETS,
        cleaner_configured,
        cleaner_provider,
        ai_provider: provider.clone(),
        cleaner_model,
        cleaner_model_options,
        ai_notice: match flash.0.as_str() {
            "provider_saved" => "Settings saved.",
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
        measurement: measurement_system(&s.db, &user.id).await?,
        units_notice: if flash.is("units_saved") {
            "Units saved."
        } else {
            ""
        },
        moments_notice: if flash.is("moments_saved") {
            "Step moments saved."
        } else {
            ""
        },
        chat_notice: if flash.is("chat_saved") {
            "Recipe chat saved."
        } else {
            ""
        },
        recipe_chat: chat::chat_enabled(&s.db, &user.id).await?,
        web_search_notice: match flash.0.as_str() {
            "web_search_saved" => "Search key saved.",
            "web_search_removed" => "Search key removed.",
            _ => "",
        },
        web_search_providers: WEB_SEARCH_PROVIDERS,
        web_search_label: own_web_search
            .as_ref()
            .map_or("", |key| web_search_label(&key.provider)),
        web_search_key_masked: own_web_search
            .as_ref()
            .map_or_else(String::new, |key| mask_key(&key.api_key)),
        shared_web_search,
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
            SEARCH_SHARED if !uses_shared_ai => SEARCH_EMBEDDINGS,
            choice => choice,
        },
        shared_search_bm25: uses_shared_ai
            && own_search_engine(&s.db, &shared_id).await? == SEARCH_BM25,
        group: groups::panel(s, &user.id, flash, extras).await?,
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
    let values = if fresh.is_empty() {
        fallback.iter().map(|model| (*model).into()).collect()
    } else {
        fresh
    };
    select_options(values, current)
}

/// Model options for a select: `values` with the saved model kept first when
/// it is missing. An empty saved model (the video cleaner's "provider's own
/// model", which the template offers as its first option) adds nothing.
fn select_options(mut values: Vec<String>, current: &str) -> Vec<SelectOption> {
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

/// Saves the units the signed-in user's recipes are generated in. Saved
/// recipes keep their units; the recipe chat can convert one on request.
async fn update_measurements(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<MeasurementForm>,
) -> Result<Response> {
    if !MEASUREMENT_SYSTEMS.contains(&form.system.as_str()) {
        return Err(AppError::BadRequest(
            "Choose metric, imperial, or as written.".into(),
        ));
    }
    set_app_setting(&s.db, &user.id, MEASUREMENT_SETTING, &form.system).await?;
    Ok(flash_redirect("/settings#settings-units", "units_saved"))
}

/// Switches video step moments on or off for the signed-in user. Off stops
/// their new imports from capturing a moment pool and hides moments on their
/// pages; stored moments stay, so switching back on shows them again.
async fn update_step_moments(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<SwitchForm>,
) -> Result<Response> {
    save_switch(&s, &user, STEP_MOMENTS_SETTING, &form).await?;
    Ok(flash_redirect(
        "/settings#settings-moments",
        "moments_saved",
    ))
}

/// Switches the recipe chat on or off for the signed-in user. Off hides the
/// Ask drawer on every recipe; stored conversations stay, so switching back
/// on shows them again.
async fn update_recipe_chat(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<SwitchForm>,
) -> Result<Response> {
    save_switch(&s, &user, chat::CHAT_SETTING, &form).await?;
    Ok(flash_redirect("/settings#settings-chat", "chat_saved"))
}

/// Saves one of the signed-in user's on/off switches as "on" or "off".
async fn save_switch(s: &AppState, user: &AuthUser, key: &str, form: &SwitchForm) -> Result<()> {
    let value = if form.enabled.is_some() { "on" } else { "off" };
    set_app_setting(&s.db, &user.id, key, value).await
}

/// Saves the signed-in user's own search API key, replacing any earlier one.
/// The key is stored in the database and only its masked tail is rendered.
async fn add_web_search_key(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<WebSearchForm>,
) -> Result<Response> {
    let provider = form.provider.trim();
    if !WEB_SEARCH_PROVIDERS.iter().any(|(key, _)| *key == provider) {
        return Err(AppError::BadRequest("Choose a search provider.".into()));
    }
    let api_key = trim(&form.api_key);
    if api_key.is_empty() {
        return Err(AppError::BadRequest("Enter the search API key.".into()));
    }
    if !header_safe_key(&api_key) {
        return Err(AppError::BadRequest(
            "That does not look like a search API key: paste the key alone, without spaces.".into(),
        ));
    }
    store_web_search_key(&s.db, &user.id, provider, &api_key).await?;
    Ok(flash_redirect(
        "/settings#settings-web-search",
        "web_search_saved",
    ))
}

/// Removes the signed-in user's own search API key.
async fn delete_web_search_key(State(s): State<Arc<AppState>>, user: AuthUser) -> Result<Response> {
    remove_web_search_key(&s.db, &user.id).await?;
    Ok(flash_redirect(
        "/settings#settings-web-search",
        "web_search_removed",
    ))
}

/// Saves the signed-in user's library search engine. "shared" follows the
/// choice of the group member whose AI logins the user uses.
async fn update_search_engine(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<SearchEngineForm>,
) -> Result<Response> {
    if !SEARCH_ENGINES.contains(&form.engine.as_str()) {
        return Err(AppError::BadRequest("Unknown search engine.".into()));
    }
    set_app_setting(&s.db, &user.id, SEARCH_ENGINE_SETTING, &form.engine).await?;
    Ok(flash_redirect("/settings#settings-search", "search_saved"))
}

/// A required http(s) base URL with a host, as the URL parser writes it, with
/// any trailing slash removed.
fn normalise_url(raw: &str, name: &str) -> Result<String> {
    let url = required(raw, name)?;
    let parsed = reqwest::Url::parse(&url).ok().filter(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some_and(|host| !host.is_empty())
    });
    let Some(parsed) = parsed else {
        return Err(AppError::BadRequest(format!(
            "{name} must be a web address starting with http:// or https://, such as https://api.example.com/v1."
        )));
    };
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

/// Longest API key accepted, in bytes.
const MAX_API_KEY_BYTES: usize = 4096;

/// Whether a pasted API key can be one: it goes into an HTTP header, so only
/// printable ASCII without spaces is a key at all. Anything else would fail
/// every call, and could be echoed back in the header error.
fn header_safe_key(key: &str) -> bool {
    key.len() <= MAX_API_KEY_BYTES && key.bytes().all(|byte| byte.is_ascii_graphic())
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
    if !header_safe_key(&api_key) {
        return Err(AppError::BadRequest(
            "That does not look like an API key: paste the key alone, without spaces.".into(),
        ));
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

/// Runs an authorisation helper's `start` command and returns its JSON reply.
/// `fail` wraps what went wrong in the provider's own error.
async fn start_auth_helper(script_path: &str, fail: fn(String) -> AppError) -> Result<Value> {
    let output = run_node_script(script_path.to_string(), "start", Vec::new())
        .await
        .map_err(|_| fail("could not start the authorisation flow".into()))?;
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| fail("unexpected response from the authorisation helper".into()))?;
    if !output.status.success() || value["status"].as_str() != Some("ok") {
        let message = value["message"]
            .as_str()
            .unwrap_or("authorisation could not be started");
        return Err(fail(message.to_string()));
    }
    Ok(value)
}

/// Keeps a new Codex device-code flow and returns its id. Expired flows go
/// first: a flow is otherwise dropped only when a poll sees it end, and a
/// closed popup never polls again.
fn remember_codex_flow(flows: &Mutex<HashMap<String, CodexFlow>>, flow: CodexFlow) -> String {
    let flow_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let mut flows = flows.lock();
    flows.retain(|_, flow| now <= flow.expires_at);
    flows.insert(flow_id.clone(), flow);
    flow_id
}

/// Starts an OpenAI Codex device-code flow and shows the code to enter at
/// chatgpt.com/codex/device. The rendered page polls the status route below.
async fn authorise_codex_start(State(s): State<Arc<AppState>>, user: AuthUser) -> Result<Response> {
    let value = start_auth_helper(&s.auth_script_path, AppError::CodexAuth).await?;
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
    let flow_id = remember_codex_flow(
        &s.codex_flows,
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
    let value = start_auth_helper(&s.claude_auth_script_path, AppError::ClaudeAuth).await?;
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
/// is removed only once an exchange succeeds, so an empty paste or a failed
/// exchange leaves it for another paste until it expires.
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

/// Removes one subscription credential (`codex` or `claude`) and leaves the
/// other provider's alone. When the removed provider was active, Codex takes
/// over if it is still connected; otherwise the setting stays and generation
/// reports that AI is not set up.
async fn disconnect_subscription(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(provider): Path<String>,
) -> Result<Response> {
    let (pi_provider, provider) = match provider.as_str() {
        "codex" => (CODEX_PI_PROVIDER, PROVIDER_CODEX),
        "claude" => (CLAUDE_PI_PROVIDER, PROVIDER_CLAUDE),
        _ => return Err(AppError::NotFound),
    };
    remove_pi_credential(&s.db, &user.id, pi_provider).await?;
    if ai_provider(&s.db, &user.id).await? == provider
        && pi_authorised(&s.db, &user.id, CODEX_PI_PROVIDER).await
    {
        set_app_setting(&s.db, &user.id, "ai_provider", PROVIDER_CODEX).await?;
    }
    Ok(flash_redirect("/settings#settings-ai", "disconnected"))
}

#[cfg(test)]
mod model_tests {
    use super::{MODEL_OPTIONS, SelectOption, effort_options, model_options};

    fn values(options: &[SelectOption]) -> Vec<&str> {
        options.iter().map(|option| option.value.as_str()).collect()
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

    #[tokio::test]
    async fn a_failed_sign_in_shows_the_helpers_reason_and_links_to_settings() {
        let (status, body) =
            respond(AppError::CodexAuth("device auth is unavailable".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("device auth is unavailable"), "{body}");
        assert!(body.contains("href=\"/settings\""), "{body}");
        let (_, body) = respond(AppError::ClaudeAuth("no verifier".into())).await;
        assert!(
            body.contains("Claude authorisation failed: no verifier"),
            "{body}"
        );
    }
}

#[cfg(test)]
mod image_tests {
    use super::{image_response, not_modified};
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
        middleware,
        response::Response,
        routing::get,
    };
    use tower::ServiceExt;

    /// Serves `jpeg` at /photo the way the image handlers do, behind the
    /// same revalidation layer as the app.
    async fn fetch(jpeg: &'static [u8], if_none_match: Option<&str>) -> Response {
        let app = Router::new()
            .route(
                "/photo",
                get(move || async move { image_response(jpeg.to_vec()) }),
            )
            .layer(middleware::from_fn(not_modified));
        let mut request = Request::builder().uri("/photo");
        if let Some(tag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        app.oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_unchanged_photo_revalidates_and_a_replaced_one_is_sent_again() {
        let first = fetch(b"old photo", None).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[header::CACHE_CONTROL], "private, no-cache");
        let etag = first.headers()[header::ETAG].to_str().unwrap().to_string();

        let unchanged = fetch(b"old photo", Some(&etag)).await;
        assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(unchanged.headers()[header::ETAG], etag.as_str());
        assert_eq!(
            unchanged.headers()[header::CACHE_CONTROL],
            "private, no-cache"
        );
        assert!(
            to_bytes(unchanged.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );
        let listed = fetch(b"old photo", Some(&format!("\"other\", W/{etag}"))).await;
        assert_eq!(listed.status(), StatusCode::NOT_MODIFIED);

        // A new photo at the same URL no longer matches, so it is sent.
        let replaced = fetch(b"new photo", Some(&etag)).await;
        assert_eq!(replaced.status(), StatusCode::OK);
        assert_ne!(replaced.headers()[header::ETAG], etag.as_str());
        assert_eq!(
            &to_bytes(replaced.into_body(), 1024).await.unwrap()[..],
            b"new photo"
        );
    }

    #[tokio::test]
    async fn a_revalidated_photo_keeps_the_session_cookies_set_on_the_way() {
        let app = Router::new()
            .route(
                "/photo",
                get(|| async {
                    let mut response = image_response(b"photo".to_vec());
                    for cookie in ["recipe_session=new", "recipe_accounts=a.b"] {
                        response
                            .headers_mut()
                            .append(header::SET_COOKIE, cookie.parse().unwrap());
                    }
                    response
                }),
            )
            .layer(middleware::from_fn(not_modified));
        let etag = fetch(b"photo", None).await.headers()[header::ETAG].clone();
        let request = Request::builder()
            .uri("/photo")
            .header(header::IF_NONE_MATCH, etag)
            .body(Body::empty())
            .unwrap();
        let unchanged = app.oneshot(request).await.unwrap();
        assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);
        let cookies: Vec<_> = unchanged
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(cookies, ["recipe_session=new", "recipe_accounts=a.b"]);
    }
}

/// How long a sign-in helper may run. Each call is one request to the
/// provider (start a flow, poll it once, exchange a code), so a helper still
/// running after this is stuck on the network and is killed.
const AUTH_SCRIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Runs a Node sign-in helper on the blocking pool, writing `payload` to its
/// stdin and collecting its output. A failed run's stderr is logged.
async fn run_node_script(
    script_path: String,
    subcommand: &'static str,
    payload: Vec<u8>,
) -> std::io::Result<std::process::Output> {
    tokio::task::spawn_blocking(move || {
        ai::run_node(
            "Sign-in helper",
            &script_path,
            &[subcommand],
            &payload,
            AUTH_SCRIPT_TIMEOUT,
        )
    })
    .await
    .map_err(std::io::Error::other)?
    .inspect_err(|error| warn!(%error, subcommand, "Sign-in helper could not run"))
}
// Keeps main's error type small without adding an application dependency solely for startup errors.
mod anyhowless {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(test)]
mod tests;
