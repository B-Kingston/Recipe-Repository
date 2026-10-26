use crate::ai::chat_library_log;
use crate::chat::{chat_library, keep_answer, library_summary};
use crate::htmx::Flash;
use crate::notes::{
    About, add_note, delete_note, note_photo, parse_about, return_target, update_note,
};
use crate::recipes::{
    chat_notes, recipe_notes, recipe_page, recipe_versions, start_chat_turn, view_version,
};
use crate::{AppError, AppState, AuthUser, ChatDrawerForm, NoteForm, RecipeQuery, stamp};
use axum::{
    Form,
    body::Body,
    extract::{FromRequest, Multipart, Path, Query, State},
    http::{Request, StatusCode, header},
};
use parking_lot::Mutex;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, collections::HashSet, sync::Arc};

/// Not a decodable image, but JPEG-shaped: kept as-is when ffmpeg cannot
/// convert it, which is how the test container stores it.
const PHOTO: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];

/// Two accounts, each with one recipe: u1 owns "r", u2 owns "r2". "r" has
/// no version history yet.
async fn database() -> SqlitePool {
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
    sqlx::query(
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at)
         VALUES('r','Shakshuka','u1',?1,?1),('r2','Dal','u2',?1,?1)",
    )
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    db
}

fn state(db: SqlitePool) -> Arc<AppState> {
    Arc::new(AppState {
        db,
        model: String::new(),
        pi_worker_path: String::new(),
        auth_script_path: String::new(),
        claude_auth_script_path: String::new(),
        search_grounding: false,
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        claude_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(HashMap::new())),
        media_debug_runs: Arc::new(Mutex::new(HashMap::new())),
        thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
        moment_jobs: Arc::new(Mutex::new(HashSet::new())),
        embedder: None,
    })
}

fn user(id: &str) -> AuthUser {
    AuthUser { id: id.into() }
}

/// The add-note form as a browser posts it.
async fn multipart(fields: &[(&str, &str)], photo: Option<&[u8]>) -> Multipart {
    let boundary = "noteboundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some(photo) = photo {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"photo\"; filename=\"dish.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(photo);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    Multipart::from_request(request, &()).await.unwrap()
}

async fn add(
    s: &Arc<AppState>,
    user_id: &str,
    recipe_id: &str,
    fields: &[(&str, &str)],
    photo: Option<&[u8]>,
) -> crate::Result<axum::response::Response> {
    add_note(
        State(s.clone()),
        user(user_id),
        Path(recipe_id.into()),
        multipart(fields, photo).await,
    )
    .await
}

async fn page(s: &Arc<AppState>, user_id: &str, recipe_id: &str) -> String {
    recipe_page(
        State(s.clone()),
        user(user_id),
        Path(recipe_id.into()),
        Query(RecipeQuery {
            edit: None,
            edit_meta: None,
            view: None,
            step: None,
        }),
        Flash::default(),
    )
    .await
    .unwrap()
    .0
}

async fn note_rows(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Vec<(Option<i64>, String, bool)> {
    recipe_notes(db, user_id, recipe_id)
        .await
        .unwrap()
        .into_iter()
        .map(|note| (note.version, note.body, note.has_photo))
        .collect()
}

#[tokio::test]
async fn a_note_goes_to_the_whole_recipe_by_default() {
    let db = database().await;
    let s = state(db.clone());
    let response = add(
        &s,
        "u1",
        "r",
        &[
            ("body", "  Too salty with a whole stock cube.  "),
            ("about", ""),
            ("back", "/recipes/r"),
        ],
        None,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/recipes/r#notes");
    assert_eq!(
        note_rows(&db, "u1", "r").await,
        vec![(None, "Too salty with a whole stock cube.".into(), false)]
    );
    let html = page(&s, "u1", "r").await;
    assert!(html.contains("id=\"notes\""));
    assert!(html.contains("Too salty with a whole stock cube."));
    assert!(html.contains("Whole recipe"));
}

#[tokio::test]
async fn pointing_at_this_version_starts_the_history_and_shows_on_the_version_page() {
    let db = database().await;
    let s = state(db.clone());
    add(
        &s,
        "u1",
        "r",
        &[("body", "Crust soggy at 200C"), ("about", "current")],
        None,
    )
    .await
    .unwrap();
    let versions = recipe_versions(&db, "u1", "r").await.unwrap();
    assert_eq!(versions.len(), 1, "the recipe's first version is recorded");
    assert_eq!(versions[0].label, "Original recipe");
    assert_eq!(note_rows(&db, "u1", "r").await[0].0, Some(1));

    let html = page(&s, "u1", "r").await;
    assert!(html.contains("href=\"/recipes/r/versions/1\">About v1 · Original recipe</a>"));
    let version_page = view_version(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), 1)),
        Flash::default(),
    )
    .await
    .unwrap()
    .0;
    assert!(version_page.contains("Notes about this version"));
    assert!(version_page.contains("Crust soggy at 200C"));
    assert!(
        version_page.contains("name=\"about\" value=\"1\""),
        "new notes there are about v1"
    );
}

#[tokio::test]
async fn a_note_needs_text_or_a_photo_and_a_real_version() {
    let db = database().await;
    let s = state(db.clone());
    let empty = add(&s, "u1", "r", &[("body", "   "), ("about", "")], None).await;
    assert!(matches!(empty, Err(AppError::BadRequest(_))));
    let missing = add(&s, "u1", "r", &[("body", "Hi"), ("about", "7")], None).await;
    assert!(matches!(missing, Err(AppError::BadRequest(_))));
    let long = "x".repeat(crate::notes::MAX_NOTE_CHARS + 1);
    let too_long = add(&s, "u1", "r", &[("body", &long), ("about", "")], None).await;
    assert!(matches!(too_long, Err(AppError::BadRequest(_))));
    assert!(note_rows(&db, "u1", "r").await.is_empty());

    // A photo alone is a note.
    add(&s, "u1", "r", &[("body", ""), ("about", "")], Some(PHOTO))
        .await
        .unwrap();
    assert_eq!(
        note_rows(&db, "u1", "r").await,
        vec![(None, String::new(), true)]
    );
}

#[tokio::test]
async fn notes_and_their_photos_are_private_to_the_account() {
    let db = database().await;
    let s = state(db.clone());
    add(
        &s,
        "u1",
        "r",
        &[("body", "Family loved it"), ("about", "")],
        Some(PHOTO),
    )
    .await
    .unwrap();
    let id = recipe_notes(&db, "u1", "r").await.unwrap()[0].id.clone();

    let photo = note_photo(State(s.clone()), user("u1"), Path(("r".into(), id.clone())))
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(photo.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&bytes[..], PHOTO);

    let stranger = note_photo(State(s.clone()), user("u2"), Path(("r".into(), id.clone()))).await;
    assert!(matches!(stranger, Err(AppError::NotFound)));
    let added = add(&s, "u2", "r", &[("body", "Mine now"), ("about", "")], None).await;
    assert!(matches!(added, Err(AppError::NotFound)));
    let deleted = delete_note(
        State(s.clone()),
        user("u2"),
        Path(("r".into(), id.clone())),
        Form(NoteForm {
            body: String::new(),
            about: String::new(),
            back: String::new(),
        }),
    )
    .await;
    assert!(matches!(deleted, Err(AppError::NotFound)));
    // A note id from another recipe is not found under this one either.
    let elsewhere = note_photo(State(s.clone()), user("u2"), Path(("r2".into(), id))).await;
    assert!(matches!(elsewhere, Err(AppError::NotFound)));
    assert!(!page(&s, "u2", "r2").await.contains("Family loved it"));
    assert_eq!(note_rows(&db, "u1", "r").await.len(), 1);
}

#[tokio::test]
async fn editing_a_note_repoints_it_and_keeps_a_deleted_version() {
    let db = database().await;
    let s = state(db.clone());
    add(
        &s,
        "u1",
        "r",
        &[("body", "First go"), ("about", "current")],
        None,
    )
    .await
    .unwrap();
    let id = recipe_notes(&db, "u1", "r").await.unwrap()[0].id.clone();
    // Version 1 goes away; the note still names it, and saving keeps it.
    sqlx::query("DELETE FROM recipe_versions WHERE recipe_id='r' AND version=1")
        .execute(&db)
        .await
        .unwrap();
    assert!(page(&s, "u1", "r").await.contains("About v1 (deleted)"));
    let form = |body: &str, about: &str| {
        Form(NoteForm {
            body: body.into(),
            about: about.into(),
            back: "/recipes/r/versions/1".into(),
        })
    };
    let response = update_note(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), id.clone())),
        form("First go, edited", "1"),
    )
    .await
    .unwrap();
    assert_eq!(
        response.headers()["location"],
        "/recipes/r/versions/1#notes"
    );
    assert_eq!(
        note_rows(&db, "u1", "r").await,
        vec![(Some(1), "First go, edited".into(), false)]
    );
    // Other missing versions are still refused, and text cannot be emptied
    // on a note with no photo.
    let refused = update_note(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), id.clone())),
        form("x", "9"),
    )
    .await;
    assert!(matches!(refused, Err(AppError::BadRequest(_))));
    let emptied = update_note(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), id.clone())),
        form("", ""),
    )
    .await;
    assert!(matches!(emptied, Err(AppError::BadRequest(_))));
    update_note(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), id.clone())),
        form("Whole thing", ""),
    )
    .await
    .unwrap();
    assert_eq!(note_rows(&db, "u1", "r").await[0].0, None);

    delete_note(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), id)),
        form("", ""),
    )
    .await
    .unwrap();
    assert!(note_rows(&db, "u1", "r").await.is_empty());
}

#[tokio::test]
async fn a_chat_answer_can_be_kept_as_a_note_once() {
    let db = database().await;
    let s = state(db.clone());
    let answer = start_chat_turn(&db, "u1", "r", "It came out too salty")
        .await
        .unwrap();
    sqlx::query("UPDATE recipe_chat_messages SET status='done',content='Next time: halve the stock cube.' WHERE id=?")
        .bind(&answer)
        .execute(&db)
        .await
        .unwrap();
    let form = || {
        Form(ChatDrawerForm {
            open: String::new(),
            back: "/recipes/r".into(),
        })
    };
    for _ in 0..2 {
        let response = keep_answer(
            State(s.clone()),
            user("u1"),
            Path(("r".into(), answer.clone())),
            form(),
        )
        .await
        .unwrap();
        assert_eq!(response.headers()["location"], "/recipes/r#recipe-chat");
    }
    assert_eq!(
        note_rows(&db, "u1", "r").await,
        vec![(None, "Next time: halve the stock cube.".into(), false)]
    );
    sqlx::query("INSERT INTO app_settings(user_id,key,value,updated_at) VALUES('u1','recipe_chat_drawer','on',?)")
        .bind(stamp())
        .execute(&db)
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(html.contains("Kept in notes"));
    assert!(html.contains("From the chat"));

    let stranger = keep_answer(
        State(s.clone()),
        user("u2"),
        Path(("r".into(), answer)),
        form(),
    )
    .await;
    assert!(matches!(stranger, Err(AppError::NotFound)));
}

#[tokio::test]
async fn the_chat_library_carries_notes_photos_and_versions_but_logs_no_photos() {
    let db = database().await;
    let s = state(db.clone());
    add(
        &s,
        "u1",
        "r",
        &[("body", "Original was bland"), ("about", "current")],
        Some(PHOTO),
    )
    .await
    .unwrap();
    add(
        &s,
        "u1",
        "r",
        &[("body", "Try more cumin"), ("about", "")],
        None,
    )
    .await
    .unwrap();
    let notes = chat_notes(&db, "u1", "r", 40).await.unwrap();
    let versions = recipe_versions(&db, "u1", "r").await.unwrap();
    let library = chat_library(&s, &user("u1"), "r", &notes, &versions)
        .await
        .unwrap();

    assert_eq!(library["currentVersion"], 1);
    assert_eq!(
        library["notes"][0]["text"], "Try more cumin",
        "newest first"
    );
    assert_eq!(library["notes"][0]["version"], serde_json::Value::Null);
    assert_eq!(library["notes"][1]["version"], 1);
    assert_eq!(library["notes"][1]["hasPhoto"], true);
    let photo = library["notes"][1]["photo"].as_str().unwrap();
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(photo)
            .unwrap(),
        PHOTO
    );
    assert_eq!(library["versions"][0]["label"], "Original recipe");
    assert_eq!(library["versions"][0]["recipe"]["title"], "Shakshuka");

    let logged = chat_library_log(&library).to_string();
    assert!(logged.contains("Original was bland"));
    assert!(logged.contains("\"photoSent\":true"));
    assert!(!logged.contains(photo), "photo bytes never reach the log");

    assert_eq!(
        library_summary(&notes, &versions),
        "The cook has kept 2 notes on this recipe, 1 with a photo. The recipe has 1 saved version; the current recipe is version 1."
    );
    assert_eq!(
        library_summary(&[], &[]),
        "The cook has not kept any notes on this recipe yet. The recipe has no saved versions yet."
    );
}

#[test]
fn about_values_and_return_targets_are_checked() {
    assert_eq!(parse_about("").unwrap(), About::Recipe);
    assert_eq!(parse_about("current").unwrap(), About::Current);
    assert_eq!(parse_about(" 3 ").unwrap(), About::Version(3));
    for bad in ["0", "-1", "v3", "latest"] {
        assert!(parse_about(bad).is_err(), "{bad}");
    }
    assert_eq!(return_target("r", "/recipes/r"), "/recipes/r");
    assert_eq!(
        return_target("r", "/recipes/r/versions/12"),
        "/recipes/r/versions/12"
    );
    for outside in [
        "/recipes/r/versions/",
        "/recipes/r/versions/1x",
        "/recipes/r2",
        "//x",
        "/recipes/r?x=1",
    ] {
        assert_eq!(return_target("r", outside), "/recipes/r", "{outside}");
    }
}
