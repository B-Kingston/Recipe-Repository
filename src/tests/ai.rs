use crate::ai::{
    SearchMode, apply_draft, cancel_draft, dedupe_sources, draft_page, draft_thumbnail,
    import_recipe, normalize_generated, parse_pi_response, recipe_schema, retry_guidance,
    system_recipe_prompt, validate_generated,
};
use crate::auth::setup_create;
use crate::media::MediaChannels;
use crate::recipes::{
    choose_thumbnail, discard_thumbnail, home, remove_thumbnail, start_thumbnail_recompute,
};
use crate::{
    AppError, AppState, ApplyDraftForm, AuthUser, GeneratedRecipe, GeneratedStep, Ingredient,
    IngredientUse, MediaImportForm, SetupForm, Source, ThumbnailPickForm, stamp,
};
use axum::{
    Form,
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode, header},
    response::IntoResponse,
};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, collections::HashSet, sync::Arc};
use tower::ServiceExt;

fn recipe() -> GeneratedRecipe {
    GeneratedRecipe {
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
                text: "Heat pan.".into(),
                chart_label: "heat pan".into(),
                timer_seconds: 0,
                ingredient_uses: vec![],
                input_steps: vec![],
                ingredients: vec![],
            },
            GeneratedStep {
                text: "Toast until golden.".into(),
                chart_label: "toast bread".into(),
                timer_seconds: 180,
                ingredient_uses: vec![IngredientUse {
                    ingredient: 0,
                    amount: "1 slice".into(),
                }],
                input_steps: vec![0],
                ingredients: vec!["1 slice bread".into()],
            },
        ],
    }
}

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
    let mut invalid = recipe();
    invalid.title = " ".into();
    assert!(validate_generated(&invalid).is_err());
    assert!(validate_generated(&recipe()).is_ok());
}

#[test]
fn legacy_draft_supports_unmeasured_ingredients() {
    let mut legacy = recipe();
    legacy.ingredients[0].name = "salt".into();
    legacy.ingredients[0].quantity.clear();
    legacy.ingredients[0].unit.clear();
    for step in &mut legacy.steps {
        step.chart_label.clear();
        step.ingredient_uses.clear();
        step.input_steps.clear();
    }
    legacy.steps[1].ingredients = vec!["salt".into()];
    assert!(normalize_generated(&mut legacy).is_ok());
    assert_eq!(legacy.steps[1].ingredient_uses[0].amount, "as needed");
}

#[test]
fn chart_flow_accepts_branch_and_merge_and_rejects_bad_references() {
    let mut branched = recipe();
    branched.ingredients.push(Ingredient {
        name: "butter".into(),
        quantity: "1".into(),
        unit: "tbsp".into(),
        optional: false,
    });
    branched.steps[0].ingredient_uses.push(IngredientUse {
        ingredient: 1,
        amount: "1 tbsp".into(),
    });
    assert!(validate_generated(&branched).is_ok());
    branched.steps[1].input_steps = vec![1];
    assert!(validate_generated(&branched).is_err());
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
fn pi_response_extracts_recipe_and_search_sources() {
    let response = json!({
        "recipe": recipe(),
        "sources": [{"title":"Toast","url":"https://example.com/toast"}]
    });
    let (parsed, sources, suggestions) = parse_pi_response(&response, true).unwrap();
    assert_eq!(parsed.title, "Toast");
    assert_eq!(sources.len(), 1);
    assert!(suggestions.is_empty());
}

#[test]
fn grounded_pi_response_requires_search_sources() {
    let response = json!({"recipe": recipe(), "sources": []});
    assert!(parse_pi_response(&response, true).is_err());
}

#[test]
fn ungrounded_pi_response_accepts_no_sources() {
    let response = json!({"recipe": recipe(), "sources": []});
    let (_, sources, suggestions) = parse_pi_response(&response, false).unwrap();
    assert!(sources.is_empty());
    assert!(suggestions.is_empty());
}

#[test]
fn gap_fill_mode_researches_without_requiring_sources() {
    assert_eq!(
        system_recipe_prompt(SearchMode::GapFill),
        crate::EVIDENCE_RECIPE_PROMPT
    );
    assert_eq!(
        system_recipe_prompt(SearchMode::Grounded),
        crate::GROUNDED_RECIPE_PROMPT
    );
    assert_eq!(system_recipe_prompt(SearchMode::Off), crate::RECIPE_PROMPT);
    assert!(!SearchMode::GapFill.requires_sources());
    assert!(SearchMode::Grounded.requires_sources());
    assert_eq!(SearchMode::GapFill.as_str(), "gapfill");
    assert_eq!(SearchMode::Off.as_str(), "off");
    let guidance = retry_guidance(SearchMode::GapFill);
    assert!(guidance.contains("web_search"));
    assert!(guidance.contains("gap the video evidence leaves open"));
}

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

fn state(db: SqlitePool) -> Arc<AppState> {
    Arc::new(AppState {
        db,
        model: String::new(),
        pi_worker_path: String::new(),
        auth_script_path: String::new(),
        search_grounding: false,
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(None)),
        media_debug_runs: Arc::new(Mutex::new(HashMap::new())),
        thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
    })
}

async fn response_body(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn import_form(
    url: &str,
    use_description: bool,
    use_audio: bool,
    use_ocr: bool,
) -> MediaImportForm {
    MediaImportForm {
        url: url.into(),
        notes: String::new(),
        // Checkboxes serialize only when ticked; None means unticked.
        use_description: use_description.then(|| "on".into()),
        use_audio: use_audio.then(|| "on".into()),
        use_ocr: use_ocr.then(|| "on".into()),
    }
}

#[test]
fn media_channels_default_to_every_source_enabled() {
    let all = MediaChannels::default();
    assert!(all.description && all.audio && all.ocr);
    assert!(all.any());

    let none = MediaChannels {
        description: false,
        audio: false,
        ocr: false,
    };
    assert!(!none.any());
    assert!(
        MediaChannels {
            description: true,
            audio: false,
            ocr: false
        }
        .any()
    );
    assert!(
        MediaChannels {
            description: false,
            audio: true,
            ocr: false
        }
        .any()
    );
    assert!(
        MediaChannels {
            description: false,
            audio: false,
            ocr: true
        }
        .any()
    );
}

#[tokio::test]
async fn import_recipe_rejects_import_with_every_source_unticked() {
    let state = state(database().await);
    let url = "https://www.instagram.com/reel/AbCdEf123/";
    let response = import_recipe(
        State(state),
        AuthUser { id: "u1".into() },
        Form(import_form(url, false, false, false)),
    )
    .await
    .unwrap();
    let body = response_body(response).await;
    assert!(
        body.contains("Tick at least one evidence source"),
        "expected the all-sources-unticked error, got: {body}"
    );
    // The failed submission keeps the pasted URL and the (unticked) boxes.
    assert!(body.contains(r#"value="https://www.instagram.com/reel/AbCdEf123/""#));
    assert!(!body.contains("name=\"use_description\" checked"));
}

#[tokio::test]
async fn import_recipe_accepts_any_single_ticked_source() {
    let state = state(database().await);
    let url = "https://www.instagram.com/reel/AbCdEf123/";
    for (description, audio, ocr) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let response = import_recipe(
            State(state.clone()),
            AuthUser { id: "u1".into() },
            Form(import_form(url, description, audio, ocr)),
        )
        .await
        .unwrap();
        let body = response_body(response).await;
        assert!(
            !body.contains("Tick at least one evidence source"),
            "single ticked source must be accepted"
        );
    }
}

async fn insert_draft_with_candidates(db: &SqlitePool, draft_id: &str) {
    let now = stamp();
    let recipe_json = serde_json::to_string(&recipe()).unwrap();
    sqlx::query(
        "INSERT INTO ai_drafts(id,user_id,operation,recipe_json,sources_json,created_at,expires_at)
         VALUES(?,'u1','generate',?,'[]',?,?)",
    )
    .bind(draft_id)
    .bind(&recipe_json)
    .bind(&now)
    .bind(chrono::Utc::now() + chrono::Duration::hours(24))
    .execute(db)
    .await
    .unwrap();
    for (idx, seconds, jpeg) in [(0_i64, 4_i64, b"first".as_slice()), (1, 9, b"second")] {
        sqlx::query("INSERT INTO draft_thumbnails(draft_id,idx,seconds,jpeg)VALUES(?,?,?,?)")
            .bind(draft_id)
            .bind(idx)
            .bind(seconds)
            .bind(jpeg)
            .execute(db)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn preview_offers_candidates_and_apply_persists_the_chosen_photo() {
    let db = database().await;
    let s = state(db.clone());
    insert_draft_with_candidates(&db, "td1").await;

    let html = response_body(
        draft_page(
            State(s.clone()),
            AuthUser { id: "u1".into() },
            Path("td1".into()),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(html.contains("Dish photo"), "picker missing from preview");
    assert!(html.contains("/ai/drafts/td1/thumb/0"));
    assert!(html.contains("checked"), "best candidate must default");

    let response = apply_draft(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path("td1".into()),
        Form(ApplyDraftForm {
            thumbnail: Some("1".into()),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()["location"].to_str().unwrap().to_string();

    let (stored, remaining): (Vec<u8>, i64) = sqlx::query_as(
        "SELECT (SELECT thumbnail_jpeg FROM recipes WHERE id=?), \
         (SELECT COUNT(*) FROM draft_thumbnails WHERE draft_id='td1')",
    )
    .bind(location.trim_start_matches("/recipes/"))
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(stored, b"second");
    assert_eq!(remaining, 0, "candidates die with the applied draft");
}

#[tokio::test]
async fn candidate_bytes_and_choices_stay_scoped_to_the_owner() {
    let db = database().await;
    let s = state(db.clone());
    insert_draft_with_candidates(&db, "td2").await;

    let stranger = draft_thumbnail(
        State(s.clone()),
        AuthUser { id: "u2".into() },
        Path(("td2".into(), 0)),
    )
    .await
    .unwrap_err();
    assert!(matches!(stranger, AppError::NotFound));

    let owner_bytes = draft_thumbnail(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path(("td2".into(), 0)),
    )
    .await
    .unwrap();
    assert_eq!(owner_bytes.status(), StatusCode::OK);

    let missing = draft_thumbnail(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path(("td2".into(), 5)),
    )
    .await
    .unwrap_err();
    assert!(matches!(missing, AppError::NotFound));

    // Discarding the draft removes its candidates too.
    cancel_draft(State(s), AuthUser { id: "u1".into() }, Path("td2".into()))
        .await
        .unwrap();
    let leftovers: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM draft_thumbnails WHERE draft_id='td2'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(leftovers, 0);
}

#[tokio::test]
async fn library_cards_flag_recipes_that_have_a_dish_photo() {
    let db = database().await;
    let s = state(db.clone());
    insert_draft_with_candidates(&db, "td3").await;
    let now = stamp();
    sqlx::query("INSERT INTO recipes(id,user_id,title,thumbnail_jpeg,created_at,updated_at)VALUES('r1','u1','With photo',x'424F5750',?,?)")
        .bind(&now)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO recipes(id,user_id,title,created_at,updated_at)VALUES('r2','u1','Without photo',?,?)")
        .bind(&now)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();
    let html = response_body(
        home(State(s), AuthUser { id: "u1".into() })
            .await
            .unwrap()
            .into_response(),
    )
    .await;
    assert!(html.contains("/recipes/r1/thumbnail"));
    assert!(!html.contains("/recipes/r2/thumbnail"));
}

async fn seed_recipe_with_source(db: &SqlitePool, recipe_id: &str, source_url: &str) {
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,user_id,title,created_at,updated_at)VALUES(?,'u1','Photo test',?,?)",
    )
    .bind(recipe_id)
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await
    .unwrap();
    if !source_url.is_empty() {
        sqlx::query(
            "INSERT INTO recipe_sources(id,recipe_id,position,title,url)VALUES('s1',?,0,'Reel',?)",
        )
        .bind(recipe_id)
        .bind(source_url)
        .execute(db)
        .await
        .unwrap();
    }
}

fn fake_jpeg() -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.extend(std::iter::repeat_n(0x11_u8, 64));
    bytes.extend_from_slice(&[0xFF, 0xD9]);
    bytes
}

/// Full-router upload: proves the multipart route, auth, storage, and that a
/// manual upload clears any pending frame picks. In ffmpeg-equipped
/// environments the stored bytes are the normalised square; without it the
/// sniffed original is kept — either way it must be a JPEG.
#[tokio::test]
async fn upload_replaces_the_photo_and_clears_pending_frames() {
    let db = database().await;
    let s = state(db.clone());
    setup_create(
        State(s.clone()),
        Form(SetupForm {
            username: "alice".into(),
            password: "secret".into(),
        }),
    )
    .await
    .unwrap();
    let alice_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username='alice'")
        .fetch_one(&db)
        .await
        .unwrap();
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,user_id,title,created_at,updated_at)VALUES('r1',?,'Photo test',?,?)",
    )
    .bind(&alice_id)
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recipe_thumbnail_candidates(recipe_id,idx,seconds,jpeg)VALUES('r1',0,9,x'00')",
    )
    .execute(&db)
    .await
    .unwrap();

    let mut body = b"--B\r\nContent-Disposition: form-data; name=\"image\"; filename=\"p.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n".to_vec();
    body.extend(fake_jpeg());
    body.extend_from_slice(b"\r\n--B--\r\n");
    let authorization = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("alice:secret")
    );
    let request = Request::builder()
        .method("POST")
        .uri("/recipes/r1/thumbnail/upload")
        .header(header::AUTHORIZATION, &authorization)
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=B")
        .body(Body::from(body))
        .unwrap();
    let response = crate::routes(s).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/recipes/r1");

    let stored: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT thumbnail_jpeg FROM recipes WHERE id='r1'")
            .fetch_one(&db)
            .await
            .unwrap();
    let stored = stored.expect("upload must store a photo");
    assert_eq!(&stored[..2], &[0xFF, 0xD8]);
    let leftovers: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM recipe_thumbnail_candidates WHERE recipe_id='r1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(leftovers, 0, "manual upload supersedes pending frames");
}

#[tokio::test]
async fn choose_persists_the_pick_while_discard_and_remove_behave() {
    let db = database().await;
    let s = state(db.clone());
    seed_recipe_with_source(&db, "r2", "").await;
    for (idx, jpeg) in [(0_i64, &b"AA"[..]), (1, &b"BB"[..])] {
        sqlx::query(
            "INSERT INTO recipe_thumbnail_candidates(recipe_id,idx,seconds,jpeg)VALUES('r2',?,7,?)",
        )
        .bind(idx)
        .bind(jpeg)
        .execute(&db)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE recipes SET thumbnail_jpeg=x'4B454550' WHERE id='r2'")
        .execute(&db)
        .await
        .unwrap();

    let response = choose_thumbnail(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path("r2".into()),
        Form(ThumbnailPickForm { choice: "1".into() }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let stored: Vec<u8> = sqlx::query_scalar("SELECT thumbnail_jpeg FROM recipes WHERE id='r2'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(stored, b"BB");
    let leftovers: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM recipe_thumbnail_candidates WHERE recipe_id='r2'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(leftovers, 0);

    // Discard clears future picks but keeps the current photo.
    sqlx::query(
        "INSERT INTO recipe_thumbnail_candidates(recipe_id,idx,seconds,jpeg)VALUES('r2',0,3,x'00')",
    )
    .execute(&db)
    .await
    .unwrap();
    discard_thumbnail(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path("r2".into()),
    )
    .await
    .unwrap();
    let still_there: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT thumbnail_jpeg FROM recipes WHERE id='r2'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(still_there.as_deref(), Some(&b"BB"[..]));

    // Remove clears both.
    remove_thumbnail(State(s), AuthUser { id: "u1".into() }, Path("r2".into()))
        .await
        .unwrap();
    let gone: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT thumbnail_jpeg FROM recipes WHERE id='r2'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(gone.is_none());
}

#[tokio::test]
async fn recompute_needs_a_social_video_source() {
    let db = database().await;
    let s = state(db.clone());
    seed_recipe_with_source(&db, "r3", "https://example.com/not-a-reel").await;
    let outcome =
        start_thumbnail_recompute(State(s), AuthUser { id: "u1".into() }, Path("r3".into())).await;
    match outcome {
        Err(AppError::BadRequest(message)) => {
            assert!(message.contains("no social-video source"));
        }
        other => panic!("expected a bad-request rejection, got {other:?}"),
    }
}
