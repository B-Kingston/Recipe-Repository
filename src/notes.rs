//! The cook's notes on a recipe: text, a photo, or both. A note belongs to
//! the recipe as a whole and may point at one saved version ("about v3"), so
//! "too salty" stays attached to the amounts that were actually cooked. The
//! recipe page lists every note; a version page lists the notes about it.
//! The recipe chat reads them through its tools (see `chat::answer`).

use crate::htmx::{Flash, flash_redirect};
use crate::recipes::{
    NOTE_PHOTO_FILTER, RecipeNote, VersionListRow, current_version, delete_note as remove_note,
    find_note, find_recipe, insert_note, normalize_upload_jpeg, note_photo_jpeg, recipe_notes,
    recipe_versions, update_note as store_note,
};
use crate::{AppError, AppState, AuthUser, NoteForm, Result, display_timestamp, image_response};
use axum::{
    Form,
    extract::{Multipart, Path, State},
    response::Response,
};
use std::sync::Arc;

pub(crate) const MAX_NOTE_CHARS: usize = 4000;

/// The notes block on a recipe or version page.
pub(crate) struct NotesPanel {
    pub(crate) recipe_id: String,
    pub(crate) notes: Vec<NoteView>,
    /// The "About" choices for a new note.
    pub(crate) choices: Vec<VersionChoice>,
    /// On a version page: the version shown, which new notes are about.
    pub(crate) fixed_version: Option<i64>,
    /// The page the forms come back to.
    pub(crate) back: String,
    pub(crate) notice: &'static str,
}

pub(crate) struct NoteView {
    pub(crate) id: String,
    pub(crate) body: String,
    pub(crate) has_photo: bool,
    pub(crate) time: String,
    /// "Whole recipe", "About v3 · Less salt", or "About v2 (deleted)".
    pub(crate) about: String,
    /// The version page to link `about` to, when that version still exists.
    pub(crate) version_link: Option<i64>,
    pub(crate) from_chat: bool,
    /// The "About" choices for editing this note, with its own selected.
    pub(crate) choices: Vec<VersionChoice>,
}

pub(crate) struct VersionChoice {
    /// "" (whole recipe), "current", or a version number.
    pub(crate) value: String,
    pub(crate) label: String,
    pub(crate) selected: bool,
}

/// What a note is about, as the forms send it: the whole recipe, the version
/// matching the recipe now, or one saved version.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum About {
    Recipe,
    Current,
    Version(i64),
}

pub(crate) fn parse_about(value: &str) -> Result<About> {
    match value.trim() {
        "" => Ok(About::Recipe),
        "current" => Ok(About::Current),
        number => number
            .parse::<i64>()
            .ok()
            .filter(|version| *version > 0)
            .map(About::Version)
            .ok_or_else(|| AppError::BadRequest("Choose what the note is about.".into())),
    }
}

/// The version number to store for `about`. A saved version must exist,
/// except the one the note already names (kept when an edit leaves it).
async fn resolve_about(
    state: &AppState,
    user: &AuthUser,
    recipe_id: &str,
    about: About,
    keep: Option<i64>,
) -> Result<Option<i64>> {
    match about {
        About::Recipe => Ok(None),
        About::Current => Ok(Some(current_version(&state.db, &user.id, recipe_id).await?)),
        About::Version(version) => {
            let saved = recipe_versions(&state.db, &user.id, recipe_id).await?;
            if keep == Some(version) || saved.iter().any(|row| row.version == version) {
                Ok(Some(version))
            } else {
                Err(AppError::BadRequest(format!(
                    "Version {version} of this recipe does not exist."
                )))
            }
        }
    }
}

/// `back` when it is this recipe's page or one of its version pages, else
/// the recipe page.
pub(crate) fn return_target(recipe_id: &str, back: &str) -> String {
    let page = format!("/recipes/{recipe_id}");
    let version_page = back
        .strip_prefix(&page)
        .and_then(|rest| rest.strip_prefix("/versions/"))
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()));
    if back == page || version_page {
        back.to_string()
    } else {
        page
    }
}

/// The "About" choices: the whole recipe, this version (the newest, which
/// matches the recipe now), and every older version. `selected` is the
/// version a note names; a version deleted since stays listed for it.
fn choices(versions: &[VersionListRow], selected: Option<i64>) -> Vec<VersionChoice> {
    let current = versions.first();
    let mut list = vec![VersionChoice {
        value: String::new(),
        label: "The whole recipe".into(),
        selected: selected.is_none(),
    }];
    list.push(VersionChoice {
        value: "current".into(),
        label: match current {
            Some(row) => format!("This version (v{} · {})", row.version, row.label),
            None => "This version".into(),
        },
        selected: selected.is_some() && selected == current.map(|row| row.version),
    });
    for row in versions.iter().skip(1) {
        list.push(VersionChoice {
            value: row.version.to_string(),
            label: format!("v{} · {} · {}", row.version, row.label, row.created_display),
            selected: selected == Some(row.version),
        });
    }
    if let Some(version) =
        selected.filter(|version| !versions.iter().any(|row| row.version == *version))
    {
        list.push(VersionChoice {
            value: version.to_string(),
            label: format!("v{version} (deleted)"),
            selected: true,
        });
    }
    list
}

fn about_text(versions: &[VersionListRow], version: Option<i64>) -> (String, Option<i64>) {
    let Some(version) = version else {
        return ("Whole recipe".into(), None);
    };
    match versions.iter().find(|row| row.version == version) {
        Some(row) => (format!("About v{version} · {}", row.label), Some(version)),
        None => (format!("About v{version} (deleted)"), None),
    }
}

/// The notes block: every note on a recipe page, or the notes about one
/// version on that version's page.
pub(crate) async fn panel(
    state: &AppState,
    user: &AuthUser,
    recipe_id: &str,
    fixed_version: Option<i64>,
    back: String,
    flash: &Flash,
) -> Result<NotesPanel> {
    let versions = recipe_versions(&state.db, &user.id, recipe_id).await?;
    let notes = recipe_notes(&state.db, &user.id, recipe_id)
        .await?
        .into_iter()
        .filter(|note| fixed_version.is_none() || note.version == fixed_version)
        .map(|note: RecipeNote| {
            let (about, version_link) = about_text(&versions, note.version);
            NoteView {
                choices: choices(&versions, note.version),
                time: display_timestamp(&note.created_at),
                from_chat: note.chat_message_id.is_some(),
                id: note.id,
                body: note.body,
                has_photo: note.has_photo,
                about,
                version_link,
            }
        })
        .collect();
    let notice = match flash.0.as_str() {
        "note_added" => "Note added.",
        "note_saved" => "Note saved.",
        "note_deleted" => "Note deleted.",
        "note_kept" => "Kept in this recipe's notes.",
        _ => "",
    };
    Ok(NotesPanel {
        recipe_id: recipe_id.to_string(),
        notes,
        choices: choices(&versions, None),
        fixed_version,
        back,
        notice,
    })
}

/// Adds a note: text, a photo, or both, about the whole recipe or one
/// version. Posted as a normal page load (multipart), like the dish photo.
pub(crate) async fn add_note(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<Response> {
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    let (mut body, mut about, mut back, mut photo) =
        (String::new(), String::new(), String::new(), None);
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::BadRequest("The note could not be read.".into()))?
    {
        let name = field.name().unwrap_or_default().to_string();
        if name == "photo" {
            let bytes = field
                .bytes()
                .await
                .map_err(|_| AppError::BadRequest("Photos are limited to 8 MiB.".into()))?;
            photo = Some(bytes.to_vec()).filter(|bytes| !bytes.is_empty());
            continue;
        }
        let text = field
            .text()
            .await
            .map_err(|_| AppError::BadRequest("The note could not be read.".into()))?;
        match name.as_str() {
            "body" => body = text,
            "about" => about = text,
            "back" => back = text,
            _ => {}
        }
    }
    let body = note_body(&body)?;
    if body.is_empty() && photo.is_none() {
        return Err(AppError::BadRequest(
            "Write a note or choose a photo.".into(),
        ));
    }
    let version = resolve_about(&s, &user, &recipe.id, parse_about(&about)?, None).await?;
    let photo = match photo {
        Some(raw) => Some(normalize_upload_jpeg(&raw, NOTE_PHOTO_FILTER).await?),
        None => None,
    };
    insert_note(
        &s.db,
        &user.id,
        &recipe.id,
        version,
        &body,
        photo.as_deref(),
        None,
    )
    .await?;
    let back = return_target(&recipe.id, &back);
    Ok(flash_redirect(&format!("{back}#notes"), "note_added"))
}

/// Changes a note's text or what it is about. The photo stays.
pub(crate) async fn update_note(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, note_id)): Path<(String, String)>,
    Form(form): Form<NoteForm>,
) -> Result<Response> {
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    let note = find_note(&s.db, &user.id, &recipe.id, &note_id).await?;
    let body = note_body(&form.body)?;
    if body.is_empty() && !note.has_photo {
        return Err(AppError::BadRequest(
            "A note without a photo needs some text. Delete it instead.".into(),
        ));
    }
    let version = resolve_about(
        &s,
        &user,
        &recipe.id,
        parse_about(&form.about)?,
        note.version,
    )
    .await?;
    store_note(&s.db, &user.id, &recipe.id, &note.id, &body, version).await?;
    let back = return_target(&recipe.id, &form.back);
    Ok(flash_redirect(&format!("{back}#notes"), "note_saved"))
}

pub(crate) async fn delete_note(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, note_id)): Path<(String, String)>,
    Form(form): Form<NoteForm>,
) -> Result<Response> {
    let recipe = find_recipe(&s.db, &user.id, &id).await?;
    remove_note(&s.db, &user.id, &recipe.id, &note_id).await?;
    let back = return_target(&recipe.id, &form.back);
    Ok(flash_redirect(&format!("{back}#notes"), "note_deleted"))
}

/// Serves a note's photo to its owner only.
pub(crate) async fn note_photo(
    State(s): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, note_id)): Path<(String, String)>,
) -> Result<Response> {
    let jpeg = note_photo_jpeg(&s.db, &user.id, &id, &note_id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(image_response(jpeg))
}

fn note_body(raw: &str) -> Result<String> {
    let body = raw.trim().replace("\r\n", "\n");
    if body.chars().count() > MAX_NOTE_CHARS {
        return Err(AppError::BadRequest(format!(
            "Keep a note under {MAX_NOTE_CHARS} characters."
        )));
    }
    Ok(body)
}
