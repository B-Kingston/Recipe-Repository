use crate::ai::{
    DebugCardRow, DebugUrlState, MediaDebugRun, MediaRunPurpose, PromptRoute, SearchMode,
    acquire_subscription_worker, apply_draft, cancel_draft, dedupe_sources, draft_page,
    draft_thumbnail, forget_media_runs, generate_recipe, import_sources, media_debug_frame,
    media_debug_page, media_debug_run_page, normalize_generated, parse_pi_response, recipe_schema,
    remove_leftover_temp_files, retry_guidance, route_prompt, strip_markdown_links,
    system_recipe_prompt, validate_generated,
};
use crate::auth::setup_create;
use crate::media::MediaChannels;
use crate::recipes::{
    choose_thumbnail, discard_thumbnail, home, recipe_page, recipe_still, remove_thumbnail,
    start_thumbnail_recompute,
};
use crate::{
    AppError, AppState, ApplyDraftForm, AuthUser, GeneratedRecipe, GeneratedStep, Ingredient,
    IngredientUse, PromptForm, SetupForm, Source, ThumbnailPickForm, stamp,
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
use std::sync::atomic::AtomicUsize;
use std::{collections::HashMap, collections::HashSet, sync::Arc};
use tower::ServiceExt;

fn recipe() -> GeneratedRecipe {
    GeneratedRecipe {
        title: "Toast".into(),
        plate_name: "Toast".into(),
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
fn generated_recipes_need_named_ingredients_and_sane_sizes() {
    let mut unnamed = recipe();
    unnamed.ingredients[0].name = "  ".into();
    assert!(validate_generated(&unnamed).is_err());

    let mut endless_timer = recipe();
    endless_timer.steps[1].timer_seconds = 8 * 24 * 60 * 60;
    assert!(validate_generated(&endless_timer).is_err());

    let mut runaway = recipe();
    let extra = runaway.steps[0].clone();
    runaway.steps.splice(0..0, std::iter::repeat_n(extra, 100));
    assert!(validate_generated(&runaway).is_err());
}

#[test]
fn output_without_chart_labels_is_refused_so_the_retry_runs() {
    let mut unlabelled = recipe();
    for step in &mut unlabelled.steps {
        step.chart_label.clear();
    }
    assert!(normalize_generated(&mut unlabelled).is_err());
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
    use crate::ai::{EVIDENCE_RECIPE_PROMPT, GROUNDED_RECIPE_PROMPT, RECIPE_PROMPT};
    assert_eq!(
        system_recipe_prompt(true, SearchMode::GapFill),
        EVIDENCE_RECIPE_PROMPT
    );
    assert_eq!(
        system_recipe_prompt(false, SearchMode::Grounded),
        GROUNDED_RECIPE_PROMPT
    );
    assert_eq!(system_recipe_prompt(false, SearchMode::Off), RECIPE_PROMPT);
    assert!(!SearchMode::GapFill.requires_sources());
    assert!(SearchMode::Grounded.requires_sources());
    assert_eq!(SearchMode::GapFill.as_str(), "gapfill");
    assert_eq!(SearchMode::Off.as_str(), "off");
    let guidance = retry_guidance(true, SearchMode::GapFill);
    assert!(guidance.contains("web_search"));
    assert!(guidance.contains("gap the video evidence leaves open"));
}

/// `PI_SEARCH_ENABLED=false` turns off search for video imports too, which
/// keep their evidence prompt without the tool it would offer.
#[test]
fn video_evidence_without_search_keeps_its_prompt_but_drops_the_tool() {
    use crate::ai::{EVIDENCE_OFFLINE_RECIPE_PROMPT, generation_search_mode};
    assert_eq!(generation_search_mode(false, true), SearchMode::Off);
    assert_eq!(generation_search_mode(false, false), SearchMode::Off);
    assert_eq!(generation_search_mode(true, true), SearchMode::GapFill);
    assert_eq!(generation_search_mode(true, false), SearchMode::Grounded);

    let offline = system_recipe_prompt(true, SearchMode::Off);
    assert_eq!(offline, EVIDENCE_OFFLINE_RECIPE_PROMPT);
    assert!(offline.contains("imported social-video evidence"));
    assert!(!offline.contains("web_search"));
    assert!(!retry_guidance(true, SearchMode::Off).contains("web_search"));
}

/// The three prompt families share one recipe contract, and a request to
/// leave out an ingredient is handled in each, video imports included.
#[test]
fn every_recipe_prompt_carries_the_shared_rules() {
    use crate::ai::{
        EVIDENCE_OFFLINE_RECIPE_PROMPT, EVIDENCE_RECIPE_PROMPT, GROUNDED_RECIPE_PROMPT,
        RECIPE_PROMPT,
    };
    for prompt in [
        RECIPE_PROMPT,
        GROUNDED_RECIPE_PROMPT,
        EVIDENCE_RECIPE_PROMPT,
        EVIDENCE_OFFLINE_RECIPE_PROMPT,
    ] {
        assert!(prompt.contains("Return one complete recipe, not a patch."));
        assert!(prompt.contains("Keep total timings consistent."));
        assert!(prompt.contains("\"egg free\""), "{prompt}");
    }
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
        rate_limits: Default::default(),
        invites: Default::default(),
    })
}

async fn response_body(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The Add recipe card as posted, with its video tick boxes.
fn card_form(prompt: &str, use_description: bool, use_audio: bool, use_ocr: bool) -> PromptForm {
    PromptForm {
        prompt: prompt.into(),
        video_options: Some("on".into()),
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
    let prompt = "https://www.instagram.com/reel/AbCdEf123/";
    let response = generate_recipe(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Form(card_form(prompt, false, false, false)),
    )
    .await
    .unwrap();
    let body = response_body(response).await;
    assert!(
        body.contains("Tick at least one source"),
        "expected the all-sources-unticked error, got: {body}"
    );
    // The refused card keeps the prompt and the (unticked) boxes.
    assert!(body.contains(&format!(">{prompt}</textarea>")));
    assert!(!body.contains(r#"data-channel="caption" checked"#));
    assert!(body.contains("<details class=\"advanced-options\" open>"));

    for (description, audio, ocr) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let response = generate_recipe(
            State(state.clone()),
            AuthUser { id: "u1".into() },
            Form(card_form(prompt, description, audio, ocr)),
        )
        .await
        .unwrap();
        let body = response_body(response).await;
        assert!(
            !body.contains("Tick at least one source"),
            "single ticked source must be accepted"
        );
    }
}

#[test]
fn a_video_link_anywhere_in_a_prompt_becomes_an_import() {
    let video = |url: &str, notes: &str| PromptRoute::Video {
        url: url.into(),
        notes: notes.into(),
    };
    assert_eq!(
        route_prompt("make it vegan https://www.instagram.com/reel/AbC123/, please"),
        video(
            "https://www.instagram.com/reel/AbC123/",
            "make it vegan please"
        )
    );
    assert_eq!(
        route_prompt("https://www.facebook.com/reel/42"),
        video("https://www.facebook.com/reel/42", "")
    );
    // Other links stay in the notes for the recipe model, line breaks kept.
    assert_eq!(
        route_prompt(
            "Like this, but vegetarian (https://www.instagram.com/reel/AbC123/)\n\nSauce from https://example.com/miso-pasta"
        ),
        video(
            "https://www.instagram.com/reel/AbC123/",
            "Like this, but vegetarian\nSauce from https://example.com/miso-pasta"
        )
    );
    // The same video twice is one video.
    assert_eq!(
        route_prompt(
            "https://www.instagram.com/reel/AbC123/ and again https://www.instagram.com/reel/AbC123/"
        ),
        video("https://www.instagram.com/reel/AbC123/", "and again")
    );
}

#[test]
fn prompts_without_one_video_are_researched_or_refused() {
    assert_eq!(
        route_prompt("spicy miso mushroom pasta"),
        PromptRoute::Research
    );
    assert_eq!(
        route_prompt("https://example.com/miso-pasta and https://example.org/ragu"),
        PromptRoute::Research
    );
    // Not a video the importer reads: an Instagram profile, or plain http.
    assert_eq!(
        route_prompt("https://www.instagram.com/somecook/"),
        PromptRoute::Research
    );
    assert_eq!(
        route_prompt("http://www.instagram.com/reel/AbC123/"),
        PromptRoute::Research
    );
    assert_eq!(
        route_prompt("https://www.instagram.com/reel/AbC123/ https://www.facebook.com/reel/42"),
        PromptRoute::TooManyVideos(2)
    );
}

#[tokio::test]
async fn the_card_sends_a_reel_link_to_the_video_import() {
    let state = state(database().await);
    let prompt = "serves four https://www.instagram.com/reel/AbC123/";
    let response = generate_recipe(
        State(state.clone()),
        AuthUser { id: "u1".into() },
        Form(PromptForm {
            prompt: prompt.into(),
            ..PromptForm::default()
        }),
    )
    .await
    .unwrap();
    let body = response_body(response).await;
    // No video cleaner is connected in tests, so the import stops on the
    // card with the prompt kept as typed.
    assert!(body.contains("AI provider is not connected. Choose one in Settings"));
    assert!(body.contains(&format!(">{prompt}</textarea>")));
    // A post without the tick boxes reads every part of the video.
    assert!(body.contains(r#"data-channel="caption" checked"#));

    let response = generate_recipe(
        State(state),
        AuthUser { id: "u1".into() },
        Form(card_form(
            "https://www.instagram.com/reel/AbC123/ https://www.facebook.com/reel/42",
            true,
            true,
            true,
        )),
    )
    .await
    .unwrap();
    assert!(
        response_body(response)
            .await
            .contains("This card links 2 videos")
    );
}

#[test]
fn import_sources_word_each_lane_for_the_reader() {
    let channels = MediaChannels {
        description: true,
        audio: true,
        ocr: false,
    };
    let mut lane = DebugUrlState::default();
    let words = |state: &DebugUrlState, stopped| {
        import_sources(state, channels, stopped)
            .into_iter()
            .map(|source| (source.state, source.detail))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        words(&lane, false),
        [
            ("waiting", "Reading…".to_string()),
            ("waiting", "Listening…".to_string()),
            ("skipped", "Skipped".to_string()),
        ]
    );

    lane.description = "Two eggs, one pan".into();
    lane.description_read = true;
    lane.audio_read = true;
    assert_eq!(
        words(&lane, false)[..2],
        [
            ("found", "4 words".to_string()),
            ("none", "No speech".to_string()),
        ]
    );

    // A stopped run never leaves a lane waiting.
    let stalled = DebugUrlState::default();
    assert_eq!(words(&stalled, true)[1], ("none", "Not read".to_string()));

    let mut ocr = DebugUrlState {
        ocr_read: true,
        ..DebugUrlState::default()
    };
    ocr.cards = vec![
        DebugCardRow {
            seconds: 1,
            text: "200g pasta".into(),
            kept: true,
        },
        DebugCardRow {
            seconds: 3,
            text: "follow me".into(),
            kept: false,
        },
    ];
    let all = MediaChannels::default();
    let sources = import_sources(&ocr, all, false);
    assert_eq!(
        (sources[2].state, sources[2].detail.as_str()),
        ("found", "1 card")
    );
}

async fn insert_draft(db: &SqlitePool, draft_id: &str) {
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO ai_drafts(id,recipe_json,sources_json,user_id,created_at,expires_at)
         VALUES(?,?,'[]','u1',?,?)",
    )
    .bind(draft_id)
    .bind(serde_json::to_string(&recipe()).unwrap())
    .bind(now.to_rfc3339())
    .bind((now + chrono::Duration::hours(24)).to_rfc3339())
    .execute(db)
    .await
    .unwrap();
}

async fn insert_draft_with_candidates(db: &SqlitePool, draft_id: &str) {
    insert_draft(db, draft_id).await;
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
    insert_draft(&db, "tm1").await;
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
    assert!(preview.contains(r#"title="Matched to the narration">0:06 in the video"#));
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
            serves: None,
        }),
        crate::htmx::Flash::default(),
    )
    .await
    .unwrap()
    .0;
    assert!(page.contains(r#"title="Matched to the narration">0:06 in the video"#));
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

async fn recipe_count(db: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM recipes")
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_draft_is_saved_as_one_new_recipe() {
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

    insert_draft(&db, "ta1").await;
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
    let steps: Vec<(String, i64)> = sqlx::query_as(
        "SELECT b.text, COUNT(i.id) FROM recipe_blocks b
         LEFT JOIN recipe_step_ingredients i ON i.step_id=b.id
         WHERE b.recipe_id=? AND b.section='step' GROUP BY b.id ORDER BY b.position",
    )
    .bind(&recipe_id)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        steps,
        [
            ("Heat pan.".to_string(), 0),
            ("Toast until golden.".to_string(), 1)
        ]
    );

    // Saving again finds the draft consumed.
    let again = apply("ta1").await.unwrap();
    assert_eq!(again.status(), StatusCode::NOT_FOUND);
    assert_eq!(recipe_count(&db).await, 1);
}

/// Two Saves of one draft at once: the second waits for the first to commit
/// and then finds the draft gone, so it saves nothing. The trigger plays the
/// first Save, consuming the draft while the second one is mid-transaction.
#[tokio::test]
async fn a_second_save_of_a_consumed_draft_saves_nothing() {
    let db = database().await;
    let s = state(db.clone());
    insert_draft(&db, "tr1").await;
    sqlx::query(
        "CREATE TRIGGER consume_draft AFTER INSERT ON recipes
         BEGIN DELETE FROM ai_drafts WHERE id='tr1'; END",
    )
    .execute(&db)
    .await
    .unwrap();

    let response = apply_draft(
        State(s),
        AuthUser { id: "u1".into() },
        Path("tr1".into()),
        Form(ApplyDraftForm { thumbnail: None }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response_body(response)
            .await
            .contains("This draft is no longer available")
    );
    assert_eq!(recipe_count(&db).await, 0, "the second save rolled back");
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
            crate::htmx::Flash::default(),
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

#[tokio::test]
async fn an_overlong_request_is_refused_on_the_card() {
    let state = state(database().await);
    let card = |prompt: String| {
        generate_recipe(
            State(state.clone()),
            AuthUser { id: "u1".into() },
            Form(PromptForm {
                prompt,
                ..PromptForm::default()
            }),
        )
    };
    let body = response_body(card("a".repeat(4_001)).await.unwrap()).await;
    assert!(
        body.contains("Keep the request under 4000 characters."),
        "{body}"
    );

    // The carriage return a browser sends with each line break is not counted.
    let body = response_body(card("ab\r\n".repeat(1_200)).await.unwrap()).await;
    assert!(!body.contains("Keep the request under"), "{body}");
    assert!(body.contains("The AI service is not configured"), "{body}");
}

#[test]
fn pages_that_show_a_failure_hide_server_side_detail() {
    assert_eq!(
        AppError::Internal("sqlite exploded at row 7".into()).user_message(),
        "Something went wrong on our side. Please try again."
    );
    assert_eq!(
        AppError::BadRequest("Tick a box.".into()).user_message(),
        "Tick a box."
    );
    assert!(
        AppError::Ai
            .user_message()
            .contains("could not prepare the recipe")
    );
}

fn debug_run(owner: &str, dir: std::path::PathBuf) -> MediaDebugRun {
    MediaDebugRun {
        created: std::time::Instant::now(),
        dir,
        urls: Arc::new(vec![Arc::new(Mutex::new(DebugUrlState::default()))]),
        history: Arc::new(Mutex::new(Vec::new())),
        pending: Arc::new(AtomicUsize::new(0)),
        owner_id: owner.into(),
        purpose: MediaRunPurpose::Debug,
        channels: MediaChannels::default(),
        draft_id: Arc::new(Mutex::new(None)),
    }
}

/// Debugger runs hold raw transcripts, OCR text, and frames: only the account
/// that started one sees it listed or can open it, and deleting the account
/// drops its runs with their frames.
#[tokio::test]
async fn debugger_runs_belong_to_the_account_that_started_them() {
    let s = state(database().await);
    let dir = std::env::temp_dir().join(format!("kr-debug-owner-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("frames").join("0")).unwrap();
    std::fs::write(dir.join("frames").join("0").join("f0001.jpg"), b"jpeg").unwrap();
    let run_id = uuid::Uuid::new_v4().to_string();
    s.media_debug_runs
        .lock()
        .insert(run_id.clone(), debug_run("u1", dir.clone()));
    let owner = || AuthUser { id: "u1".into() };
    let stranger = || AuthUser { id: "u2".into() };

    let listed = media_debug_page(State(s.clone()), owner()).await.unwrap().0;
    assert!(listed.contains(&run_id));
    let listed = media_debug_page(State(s.clone()), stranger())
        .await
        .unwrap()
        .0;
    assert!(!listed.contains(&run_id));

    assert!(
        media_debug_run_page(State(s.clone()), owner(), Path(run_id.clone()))
            .await
            .is_ok()
    );
    assert!(matches!(
        media_debug_run_page(State(s.clone()), stranger(), Path(run_id.clone())).await,
        Err(AppError::NotFound)
    ));
    let frame = |user: AuthUser| {
        media_debug_frame(
            State(s.clone()),
            user,
            Path((run_id.clone(), 0, "f0001.jpg".into())),
        )
    };
    assert_eq!(frame(owner()).await.unwrap().status(), StatusCode::OK);
    assert!(matches!(frame(stranger()).await, Err(AppError::NotFound)));

    forget_media_runs(&s.media_debug_runs, "u2");
    assert_eq!(s.media_debug_runs.lock().len(), 1);
    forget_media_runs(&s.media_debug_runs, "u1");
    assert!(s.media_debug_runs.lock().is_empty());
    assert!(!dir.exists());
}

/// A refresh token rotates, so runs on one login take turns; other accounts'
/// logins and the other provider do not wait for them.
#[tokio::test]
async fn subscription_runs_take_turns_only_on_the_same_login() {
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    let held = acquire_subscription_worker(&first, "openai-codex")
        .await
        .unwrap();
    let wait = |user: &str, provider: &'static str| {
        let user = user.to_string();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                acquire_subscription_worker(&user, provider),
            )
            .await
        }
    };
    assert!(wait(&second, "openai-codex").await.is_ok());
    assert!(wait(&first, "anthropic").await.is_ok());
    assert!(
        wait(&first, "openai-codex").await.is_err(),
        "same login waits"
    );
    drop(held);
    assert!(wait(&first, "openai-codex").await.is_ok());
}

/// A token the worker refreshed is written back over the credential the run
/// started from, and never over a login the cook changed during the run.
#[tokio::test]
async fn a_refreshed_token_is_written_back_only_over_the_login_it_came_from() {
    let db = database().await;
    let codex = crate::CODEX_PI_PROVIDER;
    let login = crate::Login::Subscription {
        user_id: "u1",
        pi_provider: codex,
    };
    let started = json!({"type": "oauth", "access": "a1", "refresh": "r1", "expires": 1});
    let refreshed = json!({"type": "oauth", "access": "a2", "refresh": "r2", "expires": 2});
    crate::store_pi_credential(&db, "u1", codex, &started)
        .await
        .unwrap();
    crate::mark_login_refused(&db, login, true).await.unwrap();

    crate::store_refreshed_pi_credential(&db, "u1", codex, &started, &refreshed)
        .await
        .unwrap();
    assert_eq!(
        crate::pi_credential(&db, "u1", codex).await.unwrap(),
        Some(refreshed.clone())
    );
    // A refresh (a Refresh sign-in that then failed, say) proves nothing, so
    // Settings still shows the earlier refusal.
    assert!(
        crate::subscription_login_refused(&db, "u1", codex)
            .await
            .unwrap()
    );

    // Reconnected during the run: the new login stays.
    let reconnected = json!({"type": "oauth", "access": "b1", "refresh": "s1", "expires": 3});
    crate::store_pi_credential(&db, "u1", codex, &reconnected)
        .await
        .unwrap();
    crate::store_refreshed_pi_credential(&db, "u1", codex, &refreshed, &started)
        .await
        .unwrap();
    assert_eq!(
        crate::pi_credential(&db, "u1", codex).await.unwrap(),
        Some(reconnected.clone())
    );

    // Disconnected (or the account deleted) during the run: nothing comes back.
    sqlx::query("DELETE FROM pi_credentials WHERE user_id='u1'")
        .execute(&db)
        .await
        .unwrap();
    crate::store_refreshed_pi_credential(&db, "u1", codex, &reconnected, &refreshed)
        .await
        .unwrap();
    assert_eq!(crate::pi_credential(&db, "u1", codex).await.unwrap(), None);
}

#[test]
fn startup_clears_only_the_apps_leftover_temp_files() {
    let dir = std::env::temp_dir().join(format!("kr-leftovers-{}", uuid::Uuid::new_v4()));
    let upload = dir.join("kindle-recipes-upload-1");
    let import = dir.join("kindle-recipes-media-import-2");
    let auth = dir.join("kindle-recipes-auth-3.json");
    let unrelated = dir.join("unrelated.txt");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&upload).unwrap();
    std::fs::write(upload.join("photo"), b"jpeg").unwrap();
    std::fs::create_dir_all(import.join("frames")).unwrap();
    std::fs::write(&auth, b"{}").unwrap();
    std::fs::write(&unrelated, b"keep").unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), b"keep").unwrap();
    // A link named like a media directory is removed as a link at most,
    // never followed.
    std::os::unix::fs::symlink(&outside, dir.join("kindle-recipes-media-link")).unwrap();

    remove_leftover_temp_files(&dir);
    assert!(!upload.exists());
    assert!(!import.exists());
    assert!(!auth.exists());
    assert!(unrelated.exists());
    assert!(outside.join("keep").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Migration 0025 rebuilds `ai_drafts`. Dropping the old table cascades into
/// the drafts' moment pools, so the migration sets them aside and puts them
/// back; drafts that altered a saved recipe go, with their candidates.
#[tokio::test]
async fn the_draft_table_rebuild_keeps_each_drafts_moment_pool() {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let all = sqlx::migrate!();
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            all.iter()
                .filter(|migration| migration.version < 25)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before.run(&db).await.unwrap();
    for statement in [
        "INSERT INTO users(id,username,password_hash,created_at) VALUES('u1','alice','x','t')",
        "INSERT INTO recipes(id,title,created_at,updated_at,user_id) VALUES('r1','Soup','t','t','u1')",
        "INSERT INTO recipe_chat_messages(id,user_id,recipe_id,role,status,change_request,change_state,created_at)
         VALUES('m1','u1','r1','assistant','done','Make it vegan','drafting','t')",
        "INSERT INTO ai_drafts(id,recipe_id,operation,recipe_json,sources_json,created_at,expires_at,user_id,evidence_json)
         VALUES('keep',NULL,'generate','{}','[]','t','t','u1','{\"title\":\"Reel\"}')",
        "INSERT INTO ai_drafts(id,recipe_id,operation,recipe_json,sources_json,created_at,expires_at,user_id,chat_message_id)
         VALUES('alter','r1','alter','{}','[]','t','t','u1','m1')",
        "INSERT INTO draft_moment_vectors(draft_id,kind,seconds,vector) VALUES('keep','audio',1.0,x'00')",
        "INSERT INTO draft_moment_vectors(draft_id,kind,seconds,vector) VALUES('alter','frame',2.0,x'00')",
        "INSERT INTO draft_moment_stills(draft_id,seconds,jpeg) VALUES('keep',2,x'ffd8')",
        "INSERT INTO draft_step_moments(draft_id,step_index,seconds,source) VALUES('keep',0,1,'audio')",
        "INSERT INTO draft_thumbnails(draft_id,idx,seconds,jpeg) VALUES('keep',0,1,x'ffd8')",
        "INSERT INTO draft_thumbnails(draft_id,idx,seconds,jpeg) VALUES('alter',0,1,x'ffd8')",
    ] {
        sqlx::query(statement).execute(&db).await.unwrap();
    }

    all.run(&db).await.unwrap();

    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM draft_moment_vectors WHERE draft_id='keep'),
                (SELECT COUNT(*) FROM draft_moment_stills WHERE draft_id='keep'),
                (SELECT COUNT(*) FROM draft_step_moments WHERE draft_id='keep'),
                (SELECT COUNT(*) FROM draft_moment_vectors WHERE draft_id='alter'),
                (SELECT COUNT(*) FROM draft_thumbnails WHERE draft_id='alter')",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1, 1, 0, 0));
    let drafts: Vec<(String, String)> = sqlx::query_as("SELECT id,evidence_json FROM ai_drafts")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(drafts, [("keep".into(), "{\"title\":\"Reel\"}".into())]);
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('ai_drafts')")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        columns,
        [
            "id",
            "user_id",
            "recipe_json",
            "sources_json",
            "evidence_json",
            "created_at",
            "expires_at"
        ]
    );
    let change_state: String =
        sqlx::query_scalar("SELECT change_state FROM recipe_chat_messages WHERE id='m1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(change_state, "failed");
    let violations: Vec<(String,)> =
        sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
            .fetch_all(&db)
            .await
            .unwrap();
    assert!(violations.is_empty(), "{violations:?}");

    // The pool still hangs off the rebuilt table: discarding the draft takes it.
    sqlx::query("DELETE FROM ai_drafts WHERE id='keep'")
        .execute(&db)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM draft_moment_vectors) + (SELECT COUNT(*) FROM draft_moment_stills)
              + (SELECT COUNT(*) FROM draft_step_moments)",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(left, 0);
}
