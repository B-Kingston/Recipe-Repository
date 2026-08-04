use crate::ai::{
    dedupe_sources, normalize_generated, parse_pi_response, recipe_schema, validate_generated,
};
use crate::{GeneratedRecipe, GeneratedStep, Ingredient, IngredientUse, Source};
use serde_json::{Value, json};

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
