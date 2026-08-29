use crate::ai::{
    DebugEventsQuery, DebugRunMap, DebugUrlState, MAX_DEBUG_URLS, MediaDebugRun, MediaRunPurpose,
    absorb_event, find_run, import_events, media_debug_frame, media_debug_start, parse_debug_urls,
    valid_frame_file, valid_run_id,
};
use crate::auth::AuthUser;
use crate::media::MediaChannels;
use crate::{AppError, AppState, MediaDebugForm};
use axum::extract::{Form, Path, Query, State};
use parking_lot::Mutex;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

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
    db
}

fn state(db: SqlitePool, runs: DebugRunMap) -> Arc<AppState> {
    Arc::new(AppState {
        db,
        model: String::new(),
        pi_worker_path: String::new(),
        auth_script_path: String::new(),
        search_grounding: false,
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(None)),
        media_debug_runs: runs,
        thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
    })
}

fn empty_run(dir: PathBuf) -> MediaDebugRun {
    MediaDebugRun {
        created: std::time::Instant::now(),
        dir,
        urls: Arc::new(vec![Arc::new(Mutex::new(DebugUrlState::default()))]),
        history: Arc::new(Mutex::new(Vec::new())),
        pending: Arc::new(AtomicUsize::new(0)),
        owner_id: "u1".into(),
        purpose: MediaRunPurpose::Debug,
        channels: MediaChannels::default(),
        draft_id: Arc::new(Mutex::new(None)),
    }
}

#[test]
fn parse_debug_urls_canonicalises_and_reports_line_numbers() {
    let (urls, errors) = parse_debug_urls(
        "\nhttps://www.instagram.com/reel/AbCdEf123/ \nhttps://www.facebook.com/reel/xyz987\n",
    );
    assert!(errors.is_empty());
    assert_eq!(
        urls,
        vec![
            "https://www.instagram.com/reel/AbCdEf123/",
            "https://www.facebook.com/reel/xyz987"
        ]
    );

    let (_, errors) = parse_debug_urls("https://example.com/not-social\n\njunk");
    assert_eq!(errors.len(), 2);
    assert!(errors[0].starts_with("Line 1:"));
    assert!(errors[1].starts_with("Line 3:"));
}

#[test]
fn parse_debug_urls_enforces_the_per_run_cap() {
    let input = (0..=MAX_DEBUG_URLS)
        .map(|index| format!("https://www.instagram.com/p/post{index}/"))
        .collect::<Vec<_>>()
        .join("\n");
    let (urls, errors) = parse_debug_urls(&input);
    assert_eq!(urls.len(), MAX_DEBUG_URLS);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("at most"));
}

#[test]
fn frame_file_names_are_strictly_numbered_thumbnails() {
    assert!(valid_frame_file("f0007.jpg"));
    assert!(!valid_frame_file("f00007.jpg"));
    assert!(!valid_frame_file("f00x7.jpg"));
    assert!(!valid_frame_file("g0007.jpg"));
    assert!(!valid_frame_file("f0007.png"));
    assert!(!valid_frame_file("f0007.jpg/../evil"));
    assert!(!valid_frame_file("../secret.jpg"));
    assert!(!valid_frame_file(""));
}

#[test]
fn run_ids_must_look_like_uuids() {
    assert!(valid_run_id(&uuid::Uuid::new_v4().to_string()));
    assert!(!valid_run_id(""));
    assert!(!valid_run_id("../escape"));
    assert!(!valid_run_id("id/../../etc"));
}

#[test]
fn absorbed_events_build_the_review_state() {
    let states: Arc<Vec<Arc<Mutex<DebugUrlState>>>> =
        Arc::new(vec![Arc::new(Mutex::new(DebugUrlState::default()))]);
    absorb_event(
        &states,
        &serde_json::json!({
            "url": 0, "kind": "description",
            "title": "Crispy rice", "description": "A reel caption",
            "durationSeconds": 42
        }),
    );
    absorb_event(
        &states,
        &serde_json::json!({
            "url": 0, "kind": "audio",
            "chars": 5, "transcript": "hello"
        }),
    );
    absorb_event(
        &states,
        &serde_json::json!({"url": 0, "kind": "warning", "message": "No local audio transcript was available."}),
    );
    absorb_event(
        &states,
        &serde_json::json!({
            "url": 0,
            "kind": "cleaned",
            "text": "Dish: Crispy rice\nIngredients:\n- 1 cup rice"
        }),
    );
    absorb_event(
        &states,
        &serde_json::json!({"url": 0, "kind": "ocr-captures", "captures": [
            {"slot": 0, "seconds": 3, "image": "f0000.jpg", "raw": "cup flour", "text": "cup flour", "card": 0},
            {"slot": 1, "seconds": 4, "image": "", "raw": "72 Ss an", "text": null, "card": null}
        ], "cards": [
            {"seconds": 3, "text": "cup flour", "kept": true}
        ]}),
    );
    let state = states[0].lock();
    assert_eq!(state.title, "Crispy rice");
    assert_eq!(state.description, "A reel caption");
    assert_eq!(state.duration_seconds, Some(42));
    assert_eq!(state.transcript, "hello");
    assert_eq!(
        state.cleaned_recipe_text,
        "Dish: Crispy rice\nIngredients:\n- 1 cup rice"
    );
    assert_eq!(state.warnings.len(), 1);
    assert_eq!(state.captures.len(), 2);
    assert_eq!(state.captures[0].cleaned.as_deref(), Some("cup flour"));
    assert_eq!(state.captures[0].card, Some(0));
    assert_eq!(state.captures[1].cleaned, None);
    assert_eq!(state.cards.len(), 1);
    assert!(state.cards[0].kept);
    // Unknown URLs and kinds are ignored rather than panicking.
    drop(state);
    absorb_event(
        &states,
        &serde_json::json!({"url": 9, "kind": "description", "title": "x"}),
    );
    absorb_event(&states, &serde_json::json!({"kind": "run-done"}));
}

#[tokio::test]
async fn starting_a_run_with_only_invalid_urls_rerenders_with_errors() {
    let runs = DebugRunMap::default();
    let response = media_debug_start(
        State(state(database().await, runs.clone())),
        AuthUser { id: "u1".into() },
        Form(MediaDebugForm {
            urls: "not a url at all".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert!(runs.lock().is_empty());
}

#[tokio::test]
async fn import_event_feeds_are_scoped_to_the_run_owner() {
    let runs = DebugRunMap::default();
    let run_id = uuid::Uuid::new_v4().to_string();
    let dir = std::env::temp_dir().join(format!("kindle-recipes-import-test-{run_id}"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut run = empty_run(dir.clone());
    run.purpose = MediaRunPurpose::Import;
    run.owner_id = "u1".into();
    run.history.lock().push(serde_json::json!({
        "url": 0, "kind": "status", "state": "extracting"
    }));
    runs.lock().insert(run_id.clone(), run);
    let app = state(database().await, runs);

    let payload = import_events(
        State(app.clone()),
        AuthUser { id: "u1".into() },
        Path(run_id.clone()),
        Query(DebugEventsQuery { since: 0 }),
    )
    .await
    .unwrap();
    assert_eq!(payload.0["events"][0]["state"], "extracting");

    let foreign = import_events(
        State(app),
        AuthUser { id: "u2".into() },
        Path(run_id),
        Query(DebugEventsQuery { since: 0 }),
    )
    .await;
    assert!(matches!(foreign, Err(AppError::NotFound)));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn missing_runs_are_not_found() {
    let runs = DebugRunMap::default();
    assert!(matches!(
        find_run(&runs, "does-not-exist"),
        Err(AppError::NotFound)
    ));
    // Path-shaped ids never reach the filesystem lookup.
    assert!(matches!(
        find_run(&runs, "../../etc/passwd"),
        Err(AppError::NotFound)
    ));
}

#[test]
fn pruning_keeps_only_the_newest_runs_and_deletes_their_frames() {
    let base = std::env::temp_dir().join(format!(
        "kindle-recipes-debug-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(base.join("frames")).unwrap();
    std::fs::write(base.join("frames").join("f0001.jpg"), b"jpg").unwrap();

    let second = std::env::temp_dir().join(format!(
        "kindle-recipes-debug-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(second.join("frames")).unwrap();

    let runs = DebugRunMap::default();
    let mut dirs = vec![second, base];
    for dir in &dirs {
        runs.lock()
            .insert(uuid::Uuid::new_v4().to_string(), empty_run(dir.clone()));
    }

    // Fill the map up to the run limit; pruning must evict down to LIMIT - 1
    // (making room for the next run) and delete exactly one directory.
    while dirs.len() < crate::ai::DEBUG_RUN_LIMIT {
        let dir = std::env::temp_dir().join(format!(
            "kindle-recipes-debug-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        runs.lock()
            .insert(uuid::Uuid::new_v4().to_string(), empty_run(dir.clone()));
        dirs.push(dir);
    }
    assert_eq!(runs.lock().len(), crate::ai::DEBUG_RUN_LIMIT);
    crate::ai::prune_debug_runs(&runs);
    assert_eq!(runs.lock().len(), crate::ai::DEBUG_RUN_LIMIT - 1);
    let survivors = dirs.iter().filter(|dir| dir.is_dir()).count();
    assert_eq!(survivors, crate::ai::DEBUG_RUN_LIMIT - 1);

    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test]
async fn frames_are_served_only_from_the_retained_directory() {
    let dir = std::env::temp_dir().join(format!(
        "kindle-recipes-debug-frames-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(dir.join("frames").join("0")).unwrap();
    std::fs::write(
        dir.join("frames").join("0").join("f0001.jpg"),
        b"jpeg-bytes",
    )
    .unwrap();

    let runs = DebugRunMap::default();
    let run_id = uuid::Uuid::new_v4().to_string();
    runs.lock().insert(run_id.clone(), empty_run(dir.clone()));

    let app = state(database().await, runs);

    // A valid capture is served as JPEG.
    let response = media_debug_frame(
        State(app.clone()),
        Path((run_id.clone(), 0usize, "f0001.jpg".into())),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);

    // Anything that is not a numbered thumbnail is rejected before the disk.
    for evil in ["../../boot", "f0001.jpg%00", "sub/f0001.jpg"] {
        let outcome = media_debug_frame(
            State(app.clone()),
            Path((run_id.clone(), 0usize, evil.into())),
        )
        .await;
        assert!(matches!(outcome, Err(AppError::NotFound)), "{evil}");
    }
    let _ = std::fs::remove_dir_all(dir);
}
