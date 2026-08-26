use crate::ai::{
    SearchMode, dedupe_sources, import_recipe, normalize_generated, parse_pi_response,
    recipe_schema, retry_guidance, system_recipe_prompt, validate_generated,
};
use crate::auth::AuthUser;
use crate::media::MediaChannels;
use crate::{
    AppState, GeneratedRecipe, GeneratedStep, Ingredient, IngredientUse, MediaImportForm, Source,
};
use axum::{Form, extract::State};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, sync::Arc};

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
            "a single ticked source must pass channel validation, got: {body}"
        );
    }
}
