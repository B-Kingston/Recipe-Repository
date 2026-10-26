//! Recipe chat: a drawer on the recipe card where the cook asks about the
//! recipe in front of them (before cooking, while cooking, or afterwards for
//! pointers next time), and the one way to change a saved recipe with AI.
//! Each (user, recipe) pair has one private thread.
//!
//! A question stores a pending answer and runs the model on a background
//! task; the page polls until it lands. The model can call tools to read the
//! recipe's notes, their photos, and saved versions, which go to the worker
//! with the request. When the cook asks for a change, the answer carries one
//! instruction, which is drafted through the alteration path and saved from
//! the chat through `apply_draft`, which keeps the version it replaces in the
//! recipe's history. Saving it writes a divider into the thread (see
//! `recipes::record_chat_change`), so the cook can see where the recipe
//! changed when reading back. Any answer can be kept as a note.

use crate::ai;
use crate::htmx::{Flash, flash_redirect};
use crate::recipes::{
    ChatMessage, ChatNoteRow, VersionListRow, chat_answer_text, chat_busy, chat_drafts, chat_notes,
    chat_thread, clear_chat, fail_chat_answer, find_recipe, find_version, finish_chat_answer,
    insert_note, kept_chat_answers, recipe_snapshot, recipe_versions, set_chat_change_state,
    start_chat_turn,
};
use crate::{
    AppError, AppState, AuthUser, ChatDrawerForm, ChatForm, RecipeSnapshot, Result, app_setting,
    display_timestamp, required, set_app_setting,
};
use axum::{
    Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use base64::Engine;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::{collections::BTreeSet, sync::Arc};
use tracing::{error, warn};

/// app_settings key of the user's recipe-chat switch (Settings). On unless
/// switched off.
pub(crate) const CHAT_SETTING: &str = "recipe_chat";
/// app_settings key remembering whether the drawer is open, so it stays open
/// across edits, views, and recipes until the cook closes it.
const DRAWER_SETTING: &str = "recipe_chat_drawer";

pub(crate) const MAX_QUESTION_CHARS: usize = 1000;
const MAX_REPLY_CHARS: usize = 6000;
const MAX_CHANGE_CHARS: usize = 2000;
/// Earlier rows sent with each question; the recipe itself always goes whole.
const HISTORY_ROWS: usize = 16;
const MAX_HISTORY_ROW_CHARS: usize = 1500;
/// A divider label from the model is shown only when it is this short.
const MAX_LABEL_WORDS: usize = 3;
const MAX_LABEL_CHARS: usize = 24;
/// What the chat's tools can read: the newest notes, the newest photos among
/// them, and the newest saved versions (plus any version a note is about).
const CHAT_NOTES: i64 = 40;
const MAX_CHAT_NOTE_CHARS: usize = 2000;
const CHAT_PHOTOS: usize = 6;
const CHAT_VERSIONS: usize = 12;

/// Whether the user has the recipe chat switched on.
pub(crate) async fn chat_enabled(db: &SqlitePool, user_id: &str) -> Result<bool> {
    Ok(app_setting(db, user_id, CHAT_SETTING).await?.as_deref() != Some("off"))
}

async fn ensure_enabled(db: &SqlitePool, user_id: &str) -> Result<()> {
    if chat_enabled(db, user_id).await? {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

/// Opens the drawer, so a page sent to `#recipe-chat` shows the chat.
pub(crate) async fn open_drawer(db: &SqlitePool, user_id: &str) -> Result<()> {
    set_app_setting(db, user_id, DRAWER_SETTING, "on").await
}

async fn drawer_open(db: &SqlitePool, user_id: &str) -> Result<bool> {
    Ok(app_setting(db, user_id, DRAWER_SETTING).await?.as_deref() == Some("on"))
}

/// The chat drawer as the recipe page shows it.
pub(crate) struct ChatPanel {
    pub(crate) open: bool,
    pub(crate) messages: Vec<ChatView>,
    /// Names what is still running (empty when nothing is). The page polls
    /// while it is set and swaps the drawer in once the token changes.
    pub(crate) poll_state: String,
    pub(crate) notice: &'static str,
    /// The page being shown (card or chart, with its step), which the
    /// drawer's forms return to and its poll fetches.
    pub(crate) back: String,
    pub(crate) max_question: usize,
}

/// One row of the thread, ready for the template.
pub(crate) struct ChatView {
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) content: String,
    pub(crate) status: String,
    pub(crate) change_state: String,
    pub(crate) change_label: String,
    /// The newest live draft of the change, or empty (none yet, or expired).
    pub(crate) draft_id: String,
    pub(crate) time: String,
    /// The answer was kept as a note on the recipe.
    pub(crate) kept: bool,
}

/// The drawer for a recipe page, or None when the user has the chat off.
/// The thread is read only while the drawer is open.
pub(crate) async fn panel(
    state: &AppState,
    user: &AuthUser,
    recipe_id: &str,
    back: String,
    flash: &Flash,
) -> Result<Option<ChatPanel>> {
    if !chat_enabled(&state.db, &user.id).await? {
        return Ok(None);
    }
    let open = drawer_open(&state.db, &user.id).await?;
    let (thread, mut drafts, kept) = if open {
        (
            chat_thread(&state.db, &user.id, recipe_id).await?,
            chat_drafts(&state.db, &user.id, recipe_id).await?,
            kept_chat_answers(&state.db, &user.id, recipe_id).await?,
        )
    } else {
        Default::default()
    };
    let poll_state = thread
        .iter()
        .filter(|row| row.status == "pending" || row.change_state == "drafting")
        .map(|row| format!("{}:{}:{}", row.id, row.status, row.change_state))
        .collect::<Vec<_>>()
        .join(",");
    let messages = thread
        .into_iter()
        .map(|row| ChatView {
            draft_id: drafts.remove(&row.id).unwrap_or_default(),
            kept: kept.contains(&row.id),
            time: display_timestamp(&row.created_at),
            id: row.id,
            role: row.role,
            content: row.content,
            status: row.status,
            change_state: row.change_state,
            change_label: row.change_label,
        })
        .collect();
    let notice = if flash.is("chat_busy") {
        "Wait for the answer before asking again."
    } else if flash.is("chat_cleared") {
        "Conversation cleared."
    } else if flash.is("note_kept") {
        "Kept in this recipe's notes."
    } else {
        ""
    };
    Ok(Some(ChatPanel {
        open,
        messages,
        poll_state,
        notice,
        back,
        max_question: MAX_QUESTION_CHARS,
    }))
}

/// `back` when it is this recipe's own page (card or chart, any query), else
/// the recipe page. Forms carry it so the drawer returns where it was opened.
pub(crate) fn return_target(recipe_id: &str, back: &str) -> String {
    let page = format!("/recipes/{recipe_id}");
    let same_page = back == page
        || back
            .strip_prefix(&page)
            .is_some_and(|rest| rest.starts_with('?'));
    if same_page && !back.contains('#') && !back.chars().any(char::is_control) {
        back.to_string()
    } else {
        page
    }
}

/// The old "Alter with AI" page, whose job the chat took over: it opens the
/// drawer on the recipe, so bookmarks and history still land somewhere.
pub(crate) async fn alter_redirect(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    if !chat_enabled(&s.db, &user.id).await? {
        return Ok(Redirect::to(&format!("/recipes/{}", recipe.id)).into_response());
    }
    open_drawer(&s.db, &user.id).await?;
    Ok(Redirect::to(&format!("/recipes/{}#recipe-chat", recipe.id)).into_response())
}

/// Opens or closes the chat drawer. The choice is the user's, not the
/// recipe's, so the drawer stays as it is from recipe to recipe.
pub(crate) async fn toggle_drawer(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<ChatDrawerForm>,
) -> Result<Response> {
    ensure_enabled(&s.db, &user.id).await?;
    find_recipe(&s.db, &user.id, &id).await?;
    let open = form.open == "on";
    set_app_setting(
        &s.db,
        &user.id,
        DRAWER_SETTING,
        if open { "on" } else { "off" },
    )
    .await?;
    let back = return_target(&id, &form.back);
    let to = if open {
        format!("{back}#recipe-chat")
    } else {
        back
    };
    Ok(Redirect::to(&to).into_response())
}

/// Asks the assistant a question about the recipe. The question and a pending
/// answer are stored at once and the model runs in the background, so a
/// closed tab or a slow model never loses the question.
pub(crate) async fn ask(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<ChatForm>,
) -> Result<Response> {
    ensure_enabled(&s.db, &user.id).await?;
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    let question = required(&form.question, "Question")?;
    if question.chars().count() > MAX_QUESTION_CHARS {
        return Err(AppError::BadRequest(format!(
            "Keep the question under {MAX_QUESTION_CHARS} characters."
        )));
    }
    let back = return_target(&recipe.id, &form.back);
    let to = format!("{back}#recipe-chat");
    // One answer at a time keeps the thread in order.
    if chat_busy(&s.db, &user.id, &recipe.id).await? {
        return Ok(flash_redirect(&to, "chat_busy"));
    }
    open_drawer(&s.db, &user.id).await?;
    let answer_id = start_chat_turn(&s.db, &user.id, &recipe.id, &question).await?;
    let state = s.clone();
    let recipe_id = recipe.id.clone();
    tokio::spawn(async move {
        if let Err(error) = answer(&state, &user, &recipe_id, &answer_id).await {
            warn!(%error, "recipe chat answer failed");
            let (status, message) = failure(&error);
            if let Err(error) = fail_chat_answer(&state.db, &answer_id, status, &message).await {
                error!(%error, "could not record the failed chat answer");
            }
        }
    });
    Ok(Redirect::to(&to).into_response())
}

/// Deletes the user's conversation about this recipe.
pub(crate) async fn clear(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<ChatDrawerForm>,
) -> Result<Response> {
    ensure_enabled(&s.db, &user.id).await?;
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    clear_chat(&s.db, &user.id, &recipe.id).await?;
    let back = return_target(&recipe.id, &form.back);
    Ok(flash_redirect(
        &format!("{back}#recipe-chat"),
        "chat_cleared",
    ))
}

/// Keeps a chat answer (pointers for next time, say) as a note on the whole
/// recipe. Keeping it twice adds nothing.
pub(crate) async fn keep_answer(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, message_id)): Path<(String, String)>,
    Form(form): Form<ChatDrawerForm>,
) -> Result<Response> {
    ensure_enabled(&s.db, &user.id).await?;
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    let text = chat_answer_text(&s.db, &user.id, &recipe.id, &message_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if !kept_chat_answers(&s.db, &user.id, &recipe.id)
        .await?
        .contains(&message_id)
    {
        let body = clip(&text, crate::notes::MAX_NOTE_CHARS);
        insert_note(
            &s.db,
            &user.id,
            &recipe.id,
            None,
            &body,
            None,
            Some(&message_id),
        )
        .await?;
    }
    let back = return_target(&recipe.id, &form.back);
    Ok(flash_redirect(&format!("{back}#recipe-chat"), "note_kept"))
}

/// Runs one answer: the model call, then, when the cook asked for a change,
/// the draft of it. A thread cleared meanwhile stops the run quietly.
async fn answer(state: &AppState, user: &AuthUser, recipe_id: &str, answer_id: &str) -> Result<()> {
    let thread = chat_thread(&state.db, &user.id, recipe_id).await?;
    let snapshot = {
        let mut conn = state.db.acquire().await?;
        recipe_snapshot(&mut conn, &user.id, recipe_id).await?
    };
    let recipe_json = serde_json::to_string(&snapshot).map_err(|_| AppError::Ai)?;
    let notes = chat_notes(&state.db, &user.id, recipe_id, CHAT_NOTES).await?;
    let versions = recipe_versions(&state.db, &user.id, recipe_id).await?;
    let library = chat_library(state, user, recipe_id, &notes, &versions).await?;
    let summary = library_summary(&notes, &versions);
    let Some((prompt, question)) = chat_prompt(&recipe_json, &summary, &thread, answer_id) else {
        return Ok(());
    };
    let reply = ai::chat_reply(state, user, &prompt, &library).await?;
    let reply_text = clip(reply.reply.trim(), MAX_REPLY_CHARS);
    if reply_text.is_empty() {
        return Err(AppError::Ai);
    }
    let change = clip(reply.change.trim(), MAX_CHANGE_CHARS);
    let label = if change.is_empty() {
        String::new()
    } else {
        model_label(&reply.label)
    };
    if !finish_chat_answer(&state.db, answer_id, &reply_text, &change, &label).await?
        || change.is_empty()
    {
        return Ok(());
    }
    let request = format!("{change}\n\nWhat the cook said in the recipe chat:\n{question}");
    let next = match ai::draft_chat_change(state, user, recipe_id, answer_id, &request).await {
        Ok(_) => "ready",
        Err(error) => {
            warn!(%error, "recipe chat change could not be drafted");
            "failed"
        }
    };
    set_chat_change_state(&state.db, answer_id, next).await
}

/// What the chat's tools read, as the worker request's `library`: the notes
/// (newest first, the newest photos as base64 JPEGs), the saved versions the
/// tools may open, and the version matching the recipe now. Photo bytes go
/// only to the worker and are never logged.
pub(crate) async fn chat_library(
    state: &AppState,
    user: &AuthUser,
    recipe_id: &str,
    notes: &[ChatNoteRow],
    versions: &[VersionListRow],
) -> Result<Value> {
    let mut photos_left = CHAT_PHOTOS;
    let notes: Vec<Value> = notes
        .iter()
        .map(|note| {
            let photo = note
                .photo_jpeg
                .as_ref()
                .filter(|_| photos_left > 0)
                .map(|jpeg| {
                    photos_left -= 1;
                    base64::engine::general_purpose::STANDARD.encode(jpeg)
                });
            json!({
                "version": note.version,
                "createdAt": display_timestamp(&note.created_at),
                "text": clip(&note.body, MAX_CHAT_NOTE_CHARS),
                "hasPhoto": note.photo_jpeg.is_some(),
                "photo": photo,
            })
        })
        .collect();
    let wanted: BTreeSet<i64> = versions
        .iter()
        .take(CHAT_VERSIONS)
        .map(|row| row.version)
        .chain(notes.iter().filter_map(|note| note["version"].as_i64()))
        .filter(|version| versions.iter().any(|row| row.version == *version))
        .collect();
    let mut saved = Vec::with_capacity(wanted.len());
    for version in wanted.into_iter().rev() {
        let detail = find_version(&state.db, &user.id, recipe_id, version).await?;
        saved.push(json!({
            "version": detail.version,
            "label": detail.label,
            "createdAt": display_timestamp(&detail.created_at),
            "recipe": detail.snapshot,
        }));
    }
    Ok(json!({
        "notes": notes,
        "versions": saved,
        "currentVersion": versions.first().map(|row| row.version),
    }))
}

/// One line telling the model what its tools can find, so it knows when
/// looking is worthwhile.
pub(crate) fn library_summary(notes: &[ChatNoteRow], versions: &[VersionListRow]) -> String {
    let photos = notes
        .iter()
        .filter(|note| note.photo_jpeg.is_some())
        .count();
    let mut line = match (notes.len(), photos) {
        (0, _) => "The cook has not kept any notes on this recipe yet.".to_string(),
        (1, 0) => "The cook has kept 1 note on this recipe.".to_string(),
        (count, 0) => format!("The cook has kept {count} notes on this recipe."),
        (1, _) => "The cook has kept 1 note on this recipe, with a photo.".to_string(),
        (count, 1) => format!("The cook has kept {count} notes on this recipe, 1 with a photo."),
        (count, photos) => {
            format!("The cook has kept {count} notes on this recipe, {photos} with photos.")
        }
    };
    match versions.first() {
        Some(current) => line.push_str(&format!(
            " The recipe has {} saved version{}; the current recipe is version {}.",
            versions.len(),
            if versions.len() == 1 { "" } else { "s" },
            current.version
        )),
        None => line.push_str(" The recipe has no saved versions yet."),
    }
    line
}

/// The prompt for one answer: the recipe as it is now, what the tools can
/// find, the conversation before the question (dividers included, so the
/// model knows the recipe changed since earlier answers), and the question
/// itself. Returns the prompt and the question, or None when the answer is
/// no longer in the thread.
pub(crate) fn chat_prompt(
    recipe_json: &str,
    library_summary: &str,
    thread: &[ChatMessage],
    answer_id: &str,
) -> Option<(String, String)> {
    let position = thread.iter().position(|row| row.id == answer_id)?;
    let question = thread[..position]
        .iter()
        .rev()
        .find(|row| row.role == "user")?
        .content
        .clone();
    let earlier: Vec<String> = thread[..position.saturating_sub(1)]
        .iter()
        .filter_map(|row| match (row.role.as_str(), row.status.as_str()) {
            ("user", _) => Some(format!("Cook: {}", clip(&row.content, MAX_HISTORY_ROW_CHARS))),
            ("assistant", "done") => Some(format!(
                "Assistant: {}",
                clip(&row.content, MAX_HISTORY_ROW_CHARS)
            )),
            ("change", _) => Some(format!(
                "[The cook saved a change to the recipe here: {}. The recipe JSON above already includes it.]",
                row.content
            )),
            _ => None,
        })
        .collect();
    let earlier = &earlier[earlier.len().saturating_sub(HISTORY_ROWS)..];
    let mut prompt = format!("Current recipe JSON:\n{recipe_json}\n\n{library_summary}\n\n");
    if !earlier.is_empty() {
        prompt.push_str("Conversation so far, oldest first:\n");
        prompt.push_str(&earlier.join("\n"));
        prompt.push_str("\n\n");
    }
    prompt.push_str("Latest question from the cook:\n");
    prompt.push_str(&question);
    Some((prompt, question))
}

/// What a failed answer shows, and whether the fix is in Settings.
fn failure(error: &AppError) -> (&'static str, String) {
    match error {
        AppError::AiNotConfigured | AppError::AiLoginRefused(_) => {
            ("needs_settings", error.to_string())
        }
        _ => (
            "failed",
            "The assistant could not answer just now. Ask again in a moment.".into(),
        ),
    }
}

/// The model's name for a change, tidied: one to three words, sentence case,
/// no trailing full stop. Anything longer is dropped, and the divider falls
/// back to `change_summary`.
pub(crate) fn model_label(raw: &str) -> String {
    let words: Vec<&str> = raw
        .trim()
        .trim_end_matches(['.', '!'])
        .split_whitespace()
        .collect();
    let label = words.join(" ");
    if words.is_empty() || words.len() > MAX_LABEL_WORDS || label.chars().count() > MAX_LABEL_CHARS
    {
        return String::new();
    }
    let mut chars = label.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// A short name for what differs between two versions of a recipe, for when
/// the model gave no usable label: "Ingredients changed", "Servings and
/// method changed", or "Recipe changed" when more than two parts moved.
pub(crate) fn change_summary(before: &RecipeSnapshot, after: &RecipeSnapshot) -> String {
    let mut parts = Vec::new();
    if before.title != after.title {
        parts.push("title");
    } else if before.description != after.description {
        parts.push("description");
    }
    if before.servings != after.servings {
        parts.push("servings");
    }
    if before.prep_minutes != after.prep_minutes || before.cook_minutes != after.cook_minutes {
        parts.push("timing");
    }
    if before.ingredients != after.ingredients {
        parts.push("ingredients");
    }
    if before.steps != after.steps {
        parts.push("method");
    }
    let summary = match parts.as_slice() {
        [] => return "Recipe saved".into(),
        [one] => format!("{one} changed"),
        [first, second] => format!("{first} and {second} changed"),
        _ => "recipe changed".into(),
    };
    let mut chars = summary.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The divider's label: the model's own name for the change when it gave a
/// usable one, else a summary of what differs.
pub(crate) fn divider_label(
    model_label_text: &str,
    before: &RecipeSnapshot,
    after: &RecipeSnapshot,
) -> String {
    let label = model_label(model_label_text);
    if label.is_empty() {
        change_summary(before, after)
    } else {
        label
    }
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
