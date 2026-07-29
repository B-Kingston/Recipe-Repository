use crate::chart::{build_chart, selected_chart_step};
use crate::{Block, ChartRecipe, ChartStep, IngredientUse, Recipe, ViewBlock, ViewStep};

#[test]
fn chart_step_query_is_clamped_to_valid_bounds() {
    assert_eq!(selected_chart_step(None, 5), None);
    assert_eq!(selected_chart_step(Some(0), 5), Some(0));
    assert_eq!(selected_chart_step(Some(3), 5), Some(2));
    assert_eq!(selected_chart_step(Some(999), 5), Some(4));
    assert_eq!(selected_chart_step(Some(1), 0), None);
}

#[test]
fn chart_layout_merges_steps() {
    let recipe = Recipe {
        id: "r".into(),
        title: "Toast".into(),
        description: String::new(),
        servings: None,
        prep_minutes: None,
        cook_minutes: None,
        chart_json: serde_json::to_string(&ChartRecipe {
            version: 1,
            steps: vec![
                ChartStep {
                    chart_label: "heat".into(),
                    timer_seconds: 0,
                    ingredient_uses: vec![IngredientUse {
                        ingredient: 0,
                        amount: "1 slice".into(),
                    }],
                    input_steps: vec![],
                },
                ChartStep {
                    chart_label: "toast".into(),
                    timer_seconds: 180,
                    ingredient_uses: vec![],
                    input_steps: vec![0],
                },
            ],
        })
        .unwrap(),
        updated_at: String::new(),
    };
    let ingredient = Block {
        id: "i".into(),
        section: "ingredient".into(),
        position: 0,
        text: "bread".into(),
        quantity: "1".into(),
        unit: "slice".into(),
        optional: 0,
    };
    let step = |position: i64, text: &str| ViewStep {
        block: ViewBlock {
            id: position.to_string(),
            position,
            text: text.into(),
            quantity: String::new(),
            unit: String::new(),
            optional: false,
            editing: false,
        },
        ingredients: vec![],
        ingredients_text: String::new(),
    };
    let chart = build_chart(
        &recipe,
        &[ingredient],
        &[step(0, "Heat pan"), step(1, "Toast bread")],
        Some(2),
    );
    assert_eq!(chart.cells.len(), 2);
    assert!(chart.cells[1].active);
}
