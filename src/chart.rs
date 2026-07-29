use crate::ai::validate_generated;
use crate::{
    Block, ChartCell, ChartDetail, ChartLeaf, ChartRecipe, ChartView, FlowStep, GeneratedRecipe,
    GeneratedStep, Ingredient, Recipe, ViewStep,
};
use std::collections::HashSet;

pub(crate) fn selected_chart_step(requested: Option<usize>, step_count: usize) -> Option<usize> {
    if step_count == 0 {
        None
    } else {
        requested.map(|step| step.saturating_sub(1).min(step_count - 1))
    }
}

pub(crate) fn build_chart(
    recipe: &Recipe,
    ingredients: &[Block],
    steps: &[ViewStep],
    requested: Option<usize>,
) -> ChartView {
    let rich = (!recipe.chart_json.trim().is_empty())
        .then_some(recipe.chart_json.as_str())
        .and_then(|raw| serde_json::from_str::<ChartRecipe>(raw).ok())
        .filter(|chart| chart.version == 1 && chart.steps.len() == steps.len())
        .filter(|chart| {
            let candidate = GeneratedRecipe {
                title: recipe.title.clone(),
                description: String::new(),
                prep_minutes: 0,
                cook_minutes: 0,
                servings: 0,
                ingredients: ingredients
                    .iter()
                    .map(|block| Ingredient {
                        name: block.text.clone(),
                        quantity: block.quantity.clone(),
                        unit: block.unit.clone(),
                        optional: block.optional(),
                    })
                    .collect(),
                steps: chart
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| GeneratedStep {
                        text: steps[index].block.text.clone(),
                        chart_label: step.chart_label.clone(),
                        timer_seconds: step.timer_seconds,
                        ingredient_uses: step.ingredient_uses.clone(),
                        input_steps: step.input_steps.clone(),
                        ingredients: Vec::new(),
                    })
                    .collect(),
            };
            validate_generated(&candidate).is_ok()
        });
    let using_rich = rich.is_some();
    let flow: Vec<FlowStep> = if let Some(chart) = rich {
        chart
            .steps
            .into_iter()
            .map(|step| FlowStep {
                label: step.chart_label,
                timer_seconds: step.timer_seconds,
                additions: step
                    .ingredient_uses
                    .into_iter()
                    .filter_map(|use_| {
                        ingredients.get(use_.ingredient).map(|ingredient| {
                            format!("{} {}", use_.amount, ingredient.text)
                                .trim()
                                .to_string()
                        })
                    })
                    .collect(),
                inputs: step.input_steps,
            })
            .collect()
    } else {
        steps
            .iter()
            .enumerate()
            .map(|(index, step)| FlowStep {
                label: step
                    .block
                    .text
                    .split_whitespace()
                    .take(5)
                    .collect::<Vec<_>>()
                    .join(" "),
                timer_seconds: 0,
                additions: step.ingredients.clone(),
                inputs: if index == 0 {
                    Vec::new()
                } else {
                    vec![index - 1]
                },
            })
            .collect()
    };
    let unlinked = if using_rich {
        Vec::new()
    } else {
        ingredients
            .iter()
            .filter(|ingredient| {
                !steps
                    .iter()
                    .flat_map(|step| step.ingredients.iter())
                    .any(|used| {
                        used.to_lowercase()
                            .contains(&ingredient.text.to_lowercase())
                    })
            })
            .map(|ingredient| {
                format!(
                    "{} {} {}",
                    ingredient.quantity, ingredient.unit, ingredient.text
                )
                .trim()
                .to_string()
            })
            .collect()
    };
    let selected = selected_chart_step(requested, flow.len());
    let mut leaves = Vec::<(String, usize)>::new();
    let mut ranges = vec![(0usize, 0usize); flow.len()];
    fn layout(
        index: usize,
        flow: &[FlowStep],
        leaves: &mut Vec<(String, usize)>,
        ranges: &mut [(usize, usize)],
    ) {
        let start = leaves.len();
        for &input in &flow[index].inputs {
            layout(input, flow, leaves, ranges);
        }
        for item in &flow[index].additions {
            leaves.push((item.clone(), index));
        }
        if flow[index].inputs.is_empty() && flow[index].additions.is_empty() {
            leaves.push(("Preparation".into(), index));
        }
        ranges[index] = (start, leaves.len().max(start + 1));
    }
    if !flow.is_empty() {
        layout(flow.len() - 1, &flow, &mut leaves, &mut ranges);
    }
    let mut active_steps = HashSet::new();
    fn ancestors(index: usize, flow: &[FlowStep], active: &mut HashSet<usize>) {
        if active.insert(index) {
            for &input in &flow[index].inputs {
                ancestors(input, flow, active);
            }
        }
    }
    if let Some(current) = selected {
        ancestors(current, &flow, &mut active_steps);
    }
    let is_selected = selected.is_some();
    let leaves = leaves
        .into_iter()
        .enumerate()
        .map(|(row, (label, source))| ChartLeaf {
            label,
            row,
            active: active_steps.contains(&source),
            dimmed: is_selected && !active_steps.contains(&source),
        })
        .collect();
    let cells = flow
        .iter()
        .enumerate()
        .map(|(step, item)| {
            let (row, end) = ranges[step];
            ChartCell {
                step: step + 1,
                label: item.label.clone(),
                row,
                span: (end - row).max(1),
                active: selected == Some(step),
                dimmed: is_selected && !active_steps.contains(&step),
                href: format!("/recipes/{}?view=chart&step={}", recipe.id, step + 1),
            }
        })
        .collect();
    let detail = selected.map(|step| ChartDetail {
        step: step + 1,
        text: steps[step].block.text.clone(),
        additions: flow[step].additions.clone(),
        timer_seconds: flow[step].timer_seconds,
        previous_href: format!("/recipes/{}?view=chart&step={}", recipe.id, step),
        next_href: format!("/recipes/{}?view=chart&step={}", recipe.id, step + 2),
        has_previous: step > 0,
        has_next: step + 1 < flow.len(),
    });
    ChartView {
        cells,
        leaves,
        unlinked,
        detail,
        step_count: flow.len(),
    }
}
