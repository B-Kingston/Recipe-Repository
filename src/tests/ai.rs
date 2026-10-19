use crate::ai::{
    SearchMode, apply_draft, cancel_draft, dedupe_sources, draft_page, draft_thumbnail,
    import_recipe, normalize_generated, parse_pi_response, recipe_schema, retry_guidance,
    strip_markdown_links, system_recipe_prompt, validate_generated,
};
use crate::auth::setup_create;
use crate::media::MediaChannels;
use crate::recipes::{
    choose_thumbnail, discard_thumbnail, home, recipe_page, recipe_still, remove_thumbnail,
    start_thumbnail_recompute,
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

    let mut dangling = recipe();
    dangling.steps[1].input_steps.clear();
    assert!(
        validate_generated(&dangling).is_err(),
        "step 0 must flow somewhere"
    );
}

#[test]
fn legacy_ingredient_matching_survives_case_folding_with_length_change() {
    // The Kelvin sign lowercases to ASCII "k" but is three bytes long, so the
    // display string cannot be split at the ingredient name's byte length.
    let mut legacy = recipe();
    legacy.ingredients[0].name = "\u{212A}".into();
    legacy.ingredients[0].quantity.clear();
    legacy.ingredients[0].unit.clear();
    for step in &mut legacy.steps {
        step.chart_label.clear();
        step.ingredient_uses.clear();
        step.input_steps.clear();
    }
    legacy.steps[1].ingredients = vec!["k".into()];
    assert!(normalize_generated(&mut legacy).is_ok());
    assert_eq!(legacy.steps[1].ingredient_uses[0].amount, "as needed");
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
    let (parsed, sources) = parse_pi_response(response, true).unwrap();
    assert_eq!(parsed.title, "Toast");
    assert_eq!(sources.len(), 1);
}

#[test]
fn grounding_decides_whether_empty_sources_are_accepted() {
    let empty = || json!({"recipe": recipe(), "sources": []});
    assert!(parse_pi_response(empty(), true).is_err());
    let (_, sources) = parse_pi_response(empty(), false).unwrap();
    assert!(sources.is_empty());
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
async fn import_requires_a_ticked_evidence_source() {
    let state = state(database().await);
    let url = "https://www.instagram.com/reel/AbCdEf123/";
    let response = import_recipe(
        State(state.clone()),
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

async fn insert_draft(
    db: &SqlitePool,
    draft_id: &str,
    recipe_id: Option<&str>,
    base_updated_at: Option<&str>,
) {
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO ai_drafts(id,recipe_id,base_updated_at,operation,recipe_json,sources_json,user_id,created_at,expires_at)
         VALUES(?,?,?,?,?,'[]','u1',?,?)",
    )
    .bind(draft_id)
    .bind(recipe_id)
    .bind(base_updated_at)
    .bind(if recipe_id.is_some() { "alter" } else { "generate" })
    .bind(serde_json::to_string(&recipe()).unwrap())
    .bind(now.to_rfc3339())
    .bind((now + chrono::Duration::hours(24)).to_rfc3339())
    .execute(db)
    .await
    .unwrap();
}

async fn insert_draft_with_candidates(db: &SqlitePool, draft_id: &str) {
    insert_draft(db, draft_id, None, None).await;
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

/// A video draft's moment pool and step moments move onto the saved recipe:
/// each moment lands on the step block created for it, its stills are served
/// to the owner only, and nothing stays behind on the consumed draft.
#[tokio::test]
async fn applying_a_video_draft_moves_its_moments_onto_the_recipe_steps() {
    let db = database().await;
    let s = state(db.clone());
    insert_draft(&db, "tm1", None, None).await;
    sqlx::query("UPDATE ai_drafts SET sources_json=? WHERE id='tm1'")
        .bind(
            json!([{"title": "Reel", "url": "https://www.instagram.com/reel/AbCdEf123/"}])
                .to_string(),
        )
        .execute(&db)
        .await
        .unwrap();
    let vector = crate::embeddings::to_blob(&vec![0.0_f32; crate::embeddings::DIMENSIONS]);
    for (kind, seconds) in [("audio", 0.0_f64), ("audio", 6.0), ("frame", 4.0)] {
        sqlx::query(
            "INSERT INTO draft_moment_vectors(draft_id,kind,seconds,vector)VALUES('tm1',?,?,?)",
        )
        .bind(kind)
        .bind(seconds)
        .bind(&vector)
        .execute(&db)
        .await
        .unwrap();
    }
    for seconds in (0_i64..=16).step_by(2) {
        sqlx::query("INSERT INTO draft_moment_stills(draft_id,seconds,jpeg)VALUES('tm1',?,?)")
            .bind(seconds)
            .bind(format!("still-{seconds}").into_bytes())
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO draft_step_moments(draft_id,step_index,seconds,source)VALUES('tm1',1,6,'audio')",
    )
    .execute(&db)
    .await
    .unwrap();

    let preview = response_body(
        draft_page(
            State(s.clone()),
            AuthUser { id: "u1".into() },
            Path("tm1".into()),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        preview.matches("class=\"step-moment\"").count(),
        1,
        "only step 2 has a moment"
    );
    assert!(preview.contains("0:06 · Matched to the narration"));
    // An audio moment shows stills spread over its 12 s window.
    for seconds in [8, 12, 16] {
        assert!(preview.contains(&format!("/ai/drafts/tm1/stills/{seconds}\"")));
    }

    let response = apply_draft(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path("tm1".into()),
        Form(ApplyDraftForm { thumbnail: None }),
    )
    .await
    .unwrap();
    let recipe_id = response.headers()["location"]
        .to_str()
        .unwrap()
        .trim_start_matches("/recipes/")
        .to_string();
    let second_step: String = sqlx::query_scalar(
        "SELECT id FROM recipe_blocks WHERE recipe_id=? AND section='step' AND position=1",
    )
    .bind(&recipe_id)
    .fetch_one(&db)
    .await
    .unwrap();
    let moments: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT step_id,seconds,source FROM recipe_step_moments")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(moments, vec![(second_step, 6, "audio".to_string())]);
    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM recipe_moment_vectors WHERE recipe_id=?1),
                (SELECT COUNT(*) FROM recipe_moment_stills WHERE recipe_id=?1),
                (SELECT COUNT(*) FROM draft_moment_vectors),
                (SELECT COUNT(*) FROM draft_moment_stills),
                (SELECT COUNT(*) FROM draft_step_moments)",
    )
    .bind(&recipe_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        counts,
        (3, 9, 0, 0, 0),
        "the pool moves and the draft keeps nothing"
    );

    let page = recipe_page(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path(recipe_id.clone()),
        axum::extract::Query(crate::RecipeQuery {
            edit: None,
            edit_meta: None,
            view: None,
            step: None,
        }),
    )
    .await
    .unwrap()
    .0;
    assert!(page.contains("0:06 · Matched to the narration"));
    assert!(page.contains(&format!("/recipes/{recipe_id}/stills/12\"")));

    let still = recipe_still(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path((recipe_id.clone(), 12)),
    )
    .await
    .unwrap();
    assert_eq!(response_body(still).await, "still-12");

    // "Find step moments" stores a fresh pool: it replaces the old one.
    let fresh = crate::moments::MomentPool {
        vectors: vec![crate::moments::MomentVector {
            kind: crate::moments::MomentKind::Frame,
            seconds: 12.0,
            vector: vec![0.0; crate::embeddings::DIMENSIONS],
        }],
        stills: [12, 14]
            .into_iter()
            .map(|seconds| crate::moments::MomentStill {
                seconds,
                jpeg: format!("fresh-{seconds}").into_bytes(),
            })
            .collect(),
    };
    crate::recipes::store_recipe_moment_pool(&db, &recipe_id, &fresh)
        .await
        .unwrap();
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM recipe_moment_vectors WHERE recipe_id=?1),
                (SELECT COUNT(*) FROM recipe_moment_stills WHERE recipe_id=?1)",
    )
    .bind(&recipe_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(counts, (1, 2));
    let still = recipe_still(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path((recipe_id.clone(), 12)),
    )
    .await
    .unwrap();
    assert_eq!(response_body(still).await, "fresh-12");
    let stranger = recipe_still(
        State(s.clone()),
        AuthUser { id: "u2".into() },
        Path((recipe_id, 12)),
    )
    .await
    .unwrap_err();
    assert!(matches!(stranger, AppError::NotFound));
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

    cancel_draft(
        State(s.clone()),
        AuthUser { id: "u2".into() },
        Path("td2".into()),
    )
    .await
    .unwrap();
    let kept: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM draft_thumbnails WHERE draft_id='td2'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(kept, 2, "a stranger cannot discard the candidates");

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

async fn version_labels(db: &SqlitePool, recipe_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT label FROM recipe_versions WHERE recipe_id=? ORDER BY version")
        .bind(recipe_id)
        .fetch_all(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn alteration_applies_only_to_the_version_it_was_based_on() {
    let db = database().await;
    let s = state(db.clone());
    let apply = |draft: &'static str| {
        apply_draft(
            State(s.clone()),
            AuthUser { id: "u1".into() },
            Path(draft.into()),
            Form(ApplyDraftForm { thumbnail: None }),
        )
    };

    insert_draft(&db, "ta1", None, None).await;
    let created = apply("ta1").await.unwrap();
    let recipe_id = created.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .trim_start_matches("/recipes/")
        .to_string();
    assert_eq!(
        version_labels(&db, &recipe_id).await,
        ["Recipe created with AI"]
    );

    let updated_at: String = sqlx::query_scalar("SELECT updated_at FROM recipes WHERE id=?")
        .bind(&recipe_id)
        .fetch_one(&db)
        .await
        .unwrap();
    insert_draft(
        &db,
        "ta2",
        Some(&recipe_id),
        Some("2000-01-01T00:00:00+00:00"),
    )
    .await;
    // A stale base re-renders the preview with the draft kept, and writes nothing.
    let stale = apply("ta2").await.unwrap();
    assert_eq!(stale.status(), StatusCode::OK);
    assert!(
        response_body(stale)
            .await
            .contains("This recipe changed after the draft was made.")
    );
    assert_eq!(version_labels(&db, &recipe_id).await.len(), 1);
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ai_drafts WHERE id='ta2'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(kept, 1);

    insert_draft(&db, "ta3", Some(&recipe_id), Some(&updated_at)).await;
    apply("ta3").await.unwrap();
    assert_eq!(
        version_labels(&db, &recipe_id)
            .await
            .last()
            .map(String::as_str),
        Some("AI alteration applied")
    );

    // A recipe saved before history existed gets its pre-AI state recorded first.
    seed_recipe_with_source(&db, "ta4", "").await;
    let legacy_updated: String =
        sqlx::query_scalar("SELECT updated_at FROM recipes WHERE id='ta4'")
            .fetch_one(&db)
            .await
            .unwrap();
    insert_draft(&db, "ta5", Some("ta4"), Some(&legacy_updated)).await;
    apply("ta5").await.unwrap();
    assert_eq!(version_labels(&db, "ta4").await.len(), 2);
}

#[tokio::test]
async fn expired_or_saved_drafts_show_the_expired_page() {
    let db = database().await;
    let response = draft_page(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path("gone".into()),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response_body(response)
            .await
            .contains("This draft is no longer available")
    );
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
        home(
            State(s),
            AuthUser { id: "u1".into() },
            axum::extract::Query(crate::HomeQuery::default()),
        )
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
    let setup = setup_create(
        State(s.clone()),
        Form(SetupForm {
            username: "alice".into(),
            password: "secret-pass".into(),
            confirm_password: "secret-pass".into(),
        }),
    )
    .await
    .unwrap();
    let session = setup.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
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
    let request = Request::builder()
        .method("POST")
        .uri("/recipes/r1/thumbnail/upload")
        .header(header::COOKIE, &session)
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

#[test]
fn generated_text_drops_markdown_citations_and_keeps_link_labels() {
    assert_eq!(
        strip_markdown_links(
            "Garlic prep is inferred. ([pbs.org](https://www.pbs.org/food/rice?utm_source=openai))"
        ),
        "Garlic prep is inferred."
    );
    assert_eq!(
        strip_markdown_links("See [this guide](https://example.com) (2 min) [a] b"),
        "See this guide (2 min) [a] b"
    );
    assert_eq!(
        strip_markdown_links("Unclosed [x](http"),
        "Unclosed [x](http"
    );
}
