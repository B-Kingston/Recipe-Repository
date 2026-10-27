//! Recipe chat: a drawer on the recipe card where the cook asks about the
//! recipe in front of them (before cooking, while cooking, or afterwards for
//! pointers next time), and the one way to change a saved recipe with AI.
//! Each (user, recipe) pair has one private thread.
//!
//! A question stores a pending answer and runs the model on a background
//! task; the page polls until it lands. The model can call tools to read the
//! recipe's notes, their photos, and saved versions, which go to the worker
//! with the request, and to search the web. When the cook asks for a change,
//! the model edits a copy of the recipe with the edit tools, and the answer
//! carries the result as a proposal: the drawer lists every change (details,
//! ingredients, and steps, with the changed words marked) under Accept and
//! Deny. Accepting writes it in one transaction (`recipes::accept_chat_proposal`),
//! keeps the version it replaces in the recipe's history, and writes a
//! divider into the thread, so the cook can see where the recipe changed
//! when reading back. Any answer can be kept as a note.

use crate::ai;
use crate::htmx::{Flash, flash_redirect};
use crate::recipes::{
    AcceptOutcome, ChatMessage, ChatNoteRow, ProposalRow, VersionListRow, accept_chat_proposal,
    chat_answer_text, chat_busy, chat_notes, chat_thread, clear_chat, decline_chat_proposal,
    fail_chat_answer, find_recipe, find_recipe_tx, find_version, finish_chat_answer, insert_note,
    kept_chat_answers, recipe_snapshot, recipe_versions, start_chat_turn,
};
use crate::{
    AppError, AppState, AuthUser, ChatDrawerForm, ChatForm, ChatProposal, RecipeProposal,
    RecipeSnapshot, Result, app_setting, display_timestamp, moments, required, set_app_setting,
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
/// The most ingredients or steps a proposal may have, and the longest text
/// it may give one; past these it is refused rather than stored.
const MAX_PROPOSAL_ITEMS: usize = 200;
const MAX_PROPOSAL_TEXT_CHARS: usize = 4000;
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
    /// The proposed change, listed for the cook; None without one.
    pub(crate) proposal: Option<ProposalView>,
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
    let (thread, kept) = if open {
        (
            chat_thread(&state.db, &user.id, recipe_id).await?,
            kept_chat_answers(&state.db, &user.id, recipe_id).await?,
        )
    } else {
        Default::default()
    };
    let poll_state = thread
        .iter()
        .filter(|row| row.status == "pending")
        .map(|row| format!("{}:{}", row.id, row.status))
        .collect::<Vec<_>>()
        .join(",");
    let updated_at = if thread.iter().any(|row| row.change_state == "ready") {
        find_recipe(&state.db, &user.id, recipe_id)
            .await?
            .updated_at
    } else {
        String::new()
    };
    let messages = thread
        .into_iter()
        .map(|row| ChatView {
            proposal: (row.change_state == "ready")
                .then(|| serde_json::from_str::<ChatProposal>(&row.proposal_json).ok())
                .flatten()
                .map(|proposal| proposal_view(&proposal, row.proposal_base != updated_at)),
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
    } else if flash.is("change_applied") {
        "Change saved to the recipe."
    } else if flash.is("change_declined") {
        "Change denied. The recipe is as it was."
    } else if flash.is("change_stale") {
        "The recipe changed since this was proposed, so it was not saved. Ask again for a fresh proposal."
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

/// Runs one answer: the model call, with the recipe the edit tools work on.
/// A proposal that passes `valid_proposal` is stored with the answer for the
/// cook to accept or deny. A thread cleared meanwhile stops the run quietly.
async fn answer(state: &AppState, user: &AuthUser, recipe_id: &str, answer_id: &str) -> Result<()> {
    let thread = chat_thread(&state.db, &user.id, recipe_id).await?;
    let (snapshot, base) = {
        let mut conn = state.db.acquire().await?;
        let recipe = find_recipe_tx(&mut conn, &user.id, recipe_id).await?;
        (
            recipe_snapshot(&mut conn, &user.id, recipe_id).await?,
            recipe.updated_at,
        )
    };
    let recipe_json = serde_json::to_string(&snapshot).map_err(|_| AppError::Ai)?;
    let notes = chat_notes(&state.db, &user.id, recipe_id, CHAT_NOTES).await?;
    let versions = recipe_versions(&state.db, &user.id, recipe_id).await?;
    let library = chat_library(state, user, recipe_id, &notes, &versions).await?;
    let summary = library_summary(&notes, &versions);
    let Some((prompt, _question)) = chat_prompt(&recipe_json, &summary, &thread, answer_id) else {
        return Ok(());
    };
    let reply = ai::chat_reply(state, user, &prompt, &library, &snapshot).await?;
    let reply_text = clip(reply.reply.trim(), MAX_REPLY_CHARS);
    if reply_text.is_empty() {
        return Err(AppError::Ai);
    }
    let proposal = reply.proposal.filter(|proposal| {
        let valid = valid_proposal(&snapshot, proposal);
        if !valid {
            warn!("recipe chat proposal was refused as invalid");
        }
        valid
    });
    let label = model_label(&reply.label);
    let json = match &proposal {
        Some(after) => serde_json::to_string(&ChatProposal {
            before: snapshot,
            after: after.clone(),
        })
        .map_err(|_| AppError::Ai)?,
        None => String::new(),
    };
    finish_chat_answer(
        &state.db,
        answer_id,
        &reply_text,
        proposal.as_ref().map(|_| ProposalRow {
            label: &label,
            json: &json,
            base: &base,
        }),
    )
    .await?;
    Ok(())
}

/// Whether a proposal can be stored: a title, named ingredients, worded
/// steps, sane numbers and sizes, every `from` naming a distinct item of the
/// recipe the chat saw, and some change the cook would see.
pub(crate) fn valid_proposal(before: &RecipeSnapshot, proposal: &RecipeProposal) -> bool {
    let fits = |text: &str| text.chars().count() <= MAX_PROPOSAL_TEXT_CHARS;
    let distinct = |froms: Vec<Option<usize>>, len: usize| {
        let mut seen = BTreeSet::new();
        froms
            .into_iter()
            .flatten()
            .all(|from| from < len && seen.insert(from))
    };
    !proposal.title.trim().is_empty()
        && fits(&proposal.title)
        && fits(&proposal.description)
        && [
            proposal.servings,
            proposal.prep_minutes,
            proposal.cook_minutes,
        ]
        .iter()
        .all(|value| value.is_none_or(|value| (0..=100_000).contains(&value)))
        && proposal.ingredients.len() <= MAX_PROPOSAL_ITEMS
        && proposal.steps.len() <= MAX_PROPOSAL_ITEMS
        && proposal.ingredients.iter().all(|item| {
            !item.name.trim().is_empty()
                && fits(&item.name)
                && fits(&item.quantity)
                && fits(&item.unit)
        })
        && proposal.steps.iter().all(|step| {
            !step.text.trim().is_empty()
                && fits(&step.text)
                && step.ingredients.len() <= MAX_PROPOSAL_ITEMS
                && step.ingredients.iter().all(|line| fits(line))
        })
        && distinct(
            proposal.ingredients.iter().map(|item| item.from).collect(),
            before.ingredients.len(),
        )
        && distinct(
            proposal.steps.iter().map(|step| step.from).collect(),
            before.steps.len(),
        )
        && proposal.snapshot() != *before
}

/// Accepts the change an answer proposed. The card itself changes, so the
/// whole page reloads, with the drawer showing the divider.
pub(crate) async fn accept_change(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, message_id)): Path<(String, String)>,
    Form(form): Form<ChatDrawerForm>,
) -> Result<Response> {
    ensure_enabled(&s.db, &user.id).await?;
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    let to = format!("{}#recipe-chat", return_target(&recipe.id, &form.back));
    match accept_chat_proposal(&s.db, &user.id, &recipe.id, &message_id).await? {
        AcceptOutcome::Applied => {
            moments::spawn_recipe_refresh(&s, &user.id, &recipe.id);
            Ok(flash_redirect(&to, "change_applied"))
        }
        AcceptOutcome::Stale => Ok(flash_redirect(&to, "change_stale")),
        AcceptOutcome::Gone => Ok(Redirect::to(&to).into_response()),
    }
}

/// Denies the change an answer proposed; the recipe stays as it is.
pub(crate) async fn decline_change(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, message_id)): Path<(String, String)>,
    Form(form): Form<ChatDrawerForm>,
) -> Result<Response> {
    ensure_enabled(&s.db, &user.id).await?;
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    let to = format!("{}#recipe-chat", return_target(&recipe.id, &form.back));
    if decline_chat_proposal(&s.db, &user.id, &recipe.id, &message_id).await? {
        Ok(flash_redirect(&to, "change_declined"))
    } else {
        Ok(Redirect::to(&to).into_response())
    }
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
                "Assistant: {}{}",
                clip(&row.content, MAX_HISTORY_ROW_CHARS),
                match row.change_state.as_str() {
                    "ready" => " [The changes proposed here await the cook's Accept or Deny; they are not in the recipe JSON above.]",
                    "discarded" => " [The cook denied the changes proposed here; they were not saved and are not in the recipe JSON above.]",
                    _ => "",
                }
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

/// A proposed change as the drawer lists it: only what differs, grouped
/// into details, ingredients, and method.
pub(crate) struct ProposalView {
    pub(crate) details: Vec<DetailChange>,
    pub(crate) ingredients: Vec<ItemChange>,
    pub(crate) steps: Vec<ItemChange>,
    /// e.g. "2 ingredients · 1 step"; what the change touches, in a line.
    pub(crate) summary: String,
    /// The recipe changed after the chat read it, so Accept would refuse.
    pub(crate) stale: bool,
}

/// One changed detail, e.g. Servings 4 → 6.
pub(crate) struct DetailChange {
    pub(crate) name: &'static str,
    pub(crate) before: String,
    pub(crate) after: String,
}

/// One added, removed, changed, or moved ingredient or step.
pub(crate) struct ItemChange {
    /// "added", "removed", "changed", or "moved".
    pub(crate) kind: &'static str,
    /// "Step 3" for steps (the old number when removed); empty for ingredients.
    pub(crate) place: String,
    /// The text with the changed words marked: inserted words for an added
    /// item, deleted ones for a removed item, both for a changed one.
    pub(crate) words: Vec<Segment>,
    /// A step's ingredient lines, marked the same way; empty when they did
    /// not change.
    pub(crate) uses: Vec<Segment>,
    /// "was step 5" when the item also moved.
    pub(crate) moved_from: String,
}

/// A run of words that stayed, were inserted, or were deleted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    /// "same", "ins", or "del".
    pub(crate) kind: &'static str,
    pub(crate) text: String,
}

fn segment(kind: &'static str, text: impl Into<String>) -> Segment {
    Segment {
        kind,
        text: text.into(),
    }
}

/// Past this many words a side, a change is shown as old text then new text
/// rather than word by word.
const MAX_DIFF_WORDS: usize = 400;

/// The words of `before` and `after` marked as kept, deleted, or inserted
/// (a longest-common-subsequence diff), with neighbouring words of the same
/// kind joined into one run.
pub(crate) fn word_diff(before: &str, after: &str) -> Vec<Segment> {
    let old: Vec<&str> = before.split_whitespace().collect();
    let new: Vec<&str> = after.split_whitespace().collect();
    let tokens = diff_tokens(&old, &new);
    let mut runs: Vec<Segment> = Vec::new();
    for (kind, word) in tokens {
        match runs.last_mut() {
            Some(run) if run.kind == kind => {
                run.text.push(' ');
                run.text.push_str(word);
            }
            _ => runs.push(segment(kind, word)),
        }
    }
    runs
}

/// Each item of `old` and `new` marked kept, deleted, or inserted, by a
/// longest-common-subsequence diff, deletions before the insertions that
/// replace them. Past MAX_DIFF_WORDS a side, everything old is deleted and
/// everything new inserted.
fn diff_tokens<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(&'static str, &'a str)> {
    let mut tokens = Vec::new();
    if old.len() > MAX_DIFF_WORDS || new.len() > MAX_DIFF_WORDS {
        tokens.extend(old.iter().map(|word| ("del", *word)));
        tokens.extend(new.iter().map(|word| ("ins", *word)));
        return tokens;
    }
    // lengths[i][j]: the longest common run of old[i..] and new[j..].
    let mut lengths = vec![vec![0u16; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lengths[i][j] = if old[i] == new[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            tokens.push(("same", old[i]));
            i += 1;
            j += 1;
        } else if i < old.len() && (j == new.len() || lengths[i + 1][j] >= lengths[i][j + 1]) {
            tokens.push(("del", old[i]));
            i += 1;
        } else {
            tokens.push(("ins", new[j]));
            j += 1;
        }
    }
    tokens
}

/// A step's ingredient lines, each marked kept, deleted, or inserted (a
/// repeated line counts once per copy); empty when the lines are the same.
fn uses_diff(before: &[String], after: &[String]) -> Vec<Segment> {
    if before == after {
        return Vec::new();
    }
    let old: Vec<&str> = before.iter().map(String::as_str).collect();
    let new: Vec<&str> = after.iter().map(String::as_str).collect();
    diff_tokens(&old, &new)
        .into_iter()
        .map(|(kind, line)| segment(kind, line))
        .collect()
}

fn ingredient_line(name: &str, quantity: &str, unit: &str, optional: bool) -> String {
    let line = [quantity, unit, name]
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if optional {
        format!("{line} (optional)")
    } else {
        line
    }
}

/// The positions (into `froms`) of the kept items that keep their order: the
/// longest increasing run of origins. Every other kept item moved.
fn in_order(froms: &[Option<usize>]) -> BTreeSet<usize> {
    let kept: Vec<(usize, usize)> = froms
        .iter()
        .enumerate()
        .filter_map(|(at, from)| from.map(|from| (at, from)))
        .collect();
    // best[k]: the longest increasing run ending at kept[k], and its previous.
    let mut best: Vec<(usize, Option<usize>)> = Vec::with_capacity(kept.len());
    for k in 0..kept.len() {
        let mut entry = (1, None);
        for p in 0..k {
            if kept[p].1 < kept[k].1 && best[p].0 + 1 > entry.0 {
                entry = (best[p].0 + 1, Some(p));
            }
        }
        best.push(entry);
    }
    let mut ordered = BTreeSet::new();
    // On a tie the run ending latest wins, so the item that jumped furthest
    // forward is the one shown as moved.
    let mut at = (0..best.len()).max_by_key(|&k| best[k].0);
    while let Some(k) = at {
        ordered.insert(kept[k].0);
        at = best[k].1;
    }
    ordered
}

/// One section's changes in reading order: each proposed item that is new,
/// changed, or moved, with each removed item where it used to be.
fn section_changes<T>(
    before: &[T],
    after: &[(Option<usize>, T)],
    numbered: Option<&'static str>,
    text: impl Fn(&T) -> String,
    uses: impl Fn(&T, &T) -> Vec<Segment>,
) -> Vec<ItemChange>
where
    T: PartialEq,
{
    let froms: Vec<Option<usize>> = after.iter().map(|(from, _)| *from).collect();
    let ordered = in_order(&froms);
    let kept: BTreeSet<usize> = froms.iter().flatten().copied().collect();
    let place =
        |number: usize| numbered.map_or_else(String::new, |word| format!("{word} {number}"));
    // A removed item has no new number, so it is named by its old one.
    let removed_place = |number: usize| {
        numbered.map_or_else(String::new, |word| {
            format!("Was {} {number}", word.to_lowercase())
        })
    };
    let mut removed = (0..before.len())
        .filter(|index| !kept.contains(index))
        .peekable();
    let mut changes = Vec::new();
    for (at, (from, item)) in after.iter().enumerate() {
        // Removed items that sat before this one's origin go first; only items
        // still in order say where that is.
        if let Some(origin) = from.filter(|_| ordered.contains(&at)) {
            while let Some(gone) = removed.next_if(|gone| gone < &origin) {
                changes.push(removed_change(removed_place(gone + 1), text(&before[gone])));
            }
        }
        match from {
            None => changes.push(ItemChange {
                kind: "added",
                place: place(at + 1),
                words: vec![segment("ins", text(item))],
                uses: Vec::new(),
                moved_from: String::new(),
            }),
            Some(origin) => {
                let original = &before[*origin];
                let moved = !ordered.contains(&at);
                if original == item && !moved {
                    continue;
                }
                let edited = original != item;
                changes.push(ItemChange {
                    kind: if edited { "changed" } else { "moved" },
                    place: place(at + 1),
                    words: if edited {
                        word_diff(&text(original), &text(item))
                    } else {
                        vec![segment("same", text(item))]
                    },
                    uses: uses(original, item),
                    moved_from: if moved {
                        numbered.map_or_else(
                            || "moved".to_string(),
                            |word| format!("was {} {}", word.to_lowercase(), origin + 1),
                        )
                    } else {
                        String::new()
                    },
                });
            }
        }
    }
    for gone in removed {
        changes.push(removed_change(removed_place(gone + 1), text(&before[gone])));
    }
    changes
}

fn removed_change(place: String, text: String) -> ItemChange {
    ItemChange {
        kind: "removed",
        place,
        words: vec![segment("del", text)],
        uses: Vec::new(),
        moved_from: String::new(),
    }
}

fn minutes(value: Option<i64>) -> String {
    value.map_or_else(|| "none".to_string(), |minutes| format!("{minutes} min"))
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Lists what a proposal changes, for the drawer.
pub(crate) fn proposal_view(proposal: &ChatProposal, stale: bool) -> ProposalView {
    let before = &proposal.before;
    let after = &proposal.after;
    let mut details = Vec::new();
    if before.title != after.title.trim() {
        details.push(DetailChange {
            name: "Title",
            before: before.title.clone(),
            after: after.title.trim().to_string(),
        });
    }
    if before.description.trim() != after.description.trim() {
        let shown = |text: &str| match text.trim() {
            "" => "none".to_string(),
            text => text.to_string(),
        };
        details.push(DetailChange {
            name: "Description",
            before: shown(&before.description),
            after: shown(&after.description),
        });
    }
    if before.servings != after.servings {
        let serves = |value: Option<i64>| {
            value.map_or_else(|| "none".to_string(), |count| count.to_string())
        };
        details.push(DetailChange {
            name: "Servings",
            before: serves(before.servings),
            after: serves(after.servings),
        });
    }
    if before.prep_minutes != after.prep_minutes {
        details.push(DetailChange {
            name: "Prep",
            before: minutes(before.prep_minutes),
            after: minutes(after.prep_minutes),
        });
    }
    if before.cook_minutes != after.cook_minutes {
        details.push(DetailChange {
            name: "Cook",
            before: minutes(before.cook_minutes),
            after: minutes(after.cook_minutes),
        });
    }
    let snapshot = after.snapshot();
    let ingredients = section_changes(
        &before.ingredients,
        &after
            .ingredients
            .iter()
            .map(|item| item.from)
            .zip(snapshot.ingredients)
            .collect::<Vec<_>>(),
        None,
        |item| ingredient_line(&item.name, &item.quantity, &item.unit, item.optional),
        |_, _| Vec::new(),
    );
    let steps = section_changes(
        &before.steps,
        &after
            .steps
            .iter()
            .map(|item| item.from)
            .zip(snapshot.steps)
            .collect::<Vec<_>>(),
        Some("Step"),
        |step| step.text.clone(),
        |old, new| uses_diff(&old.ingredients, &new.ingredients),
    );
    let mut parts = Vec::new();
    if !details.is_empty() {
        parts.push(plural(details.len(), "detail", "details"));
    }
    if !ingredients.is_empty() {
        parts.push(plural(ingredients.len(), "ingredient", "ingredients"));
    }
    if !steps.is_empty() {
        parts.push(plural(steps.len(), "step", "steps"));
    }
    ProposalView {
        summary: parts.join(" · "),
        details,
        ingredients,
        steps,
        stale,
    }
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
