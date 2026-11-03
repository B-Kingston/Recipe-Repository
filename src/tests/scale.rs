use crate::scale::{Scale, scale_text};

#[test]
fn metric_amounts_round_to_whole_numbers() {
    assert_eq!(scale_text("200", "g", 1.5), "300");
    assert_eq!(scale_text("250", "g", 0.75), "190");
    assert_eq!(scale_text("15", "ml", 0.5), "7.5");
    assert_eq!(scale_text("200g flour", "", 0.25), "50g flour");
    assert_eq!(
        scale_text("1.5 kg potatoes", "", 2.0 / 3.0),
        "1 kg potatoes"
    );
}

#[test]
fn other_amounts_round_to_kitchen_fractions() {
    assert_eq!(scale_text("1 1/2", "cup", 2.0), "3");
    assert_eq!(scale_text("1/2", "tsp", 0.5), "¼");
    assert_eq!(
        scale_text("1½ tbsp olive oil", "", 1.0 / 3.0),
        "½ tbsp olive oil"
    );
    assert_eq!(scale_text("3", "", 0.5), "1½");
    assert_eq!(scale_text("1", "", 1.0 / 3.0), "⅓");
    assert_eq!(scale_text("2 eggs", "", 3.0), "6 eggs");
    assert_eq!(scale_text("8", "", 1.5), "12");
    // A tiny amount never rounds away.
    assert_eq!(scale_text("1/8", "tsp", 0.25), "⅛");
}

#[test]
fn ranges_scale_at_both_ends() {
    assert_eq!(scale_text("2-3", "", 2.0), "4-6");
    assert_eq!(
        scale_text("2 – 3 cloves garlic", "", 2.0),
        "4 – 6 cloves garlic"
    );
    assert_eq!(scale_text("1 to 2", "tbsp", 2.0), "2 to 4");
}

#[test]
fn text_without_an_amount_is_kept() {
    assert_eq!(scale_text("to taste", "", 2.0), "to taste");
    assert_eq!(scale_text("a pinch of salt", "", 2.0), "a pinch of salt");
    assert_eq!(scale_text("", "g", 2.0), "");
    // Unscaled text is left exactly as written.
    assert_eq!(scale_text("0.5 cup", "", 1.0), "0.5 cup");
}

#[test]
fn scale_follows_the_requested_servings() {
    let scale = Scale::new(Some(4), Some(2));
    assert!(scale.is_scaled());
    assert_eq!(scale.factor, 0.5);
    assert_eq!(scale.query, "&serves=2");
    assert_eq!(scale.choices(), vec![1, 2, 3, 4, 5, 6]);

    let as_written = Scale::new(Some(8), None);
    assert!(!as_written.is_scaled());
    assert_eq!(as_written.query, "");
    assert_eq!(as_written.choices(), vec![1, 2, 3, 4, 5, 6, 8]);

    // No servings: nothing to scale from. Out of range: as written.
    assert_eq!(Scale::new(None, Some(3)).serves, None);
    assert_eq!(Scale::new(Some(4), Some(0)).serves, Some(4));
    assert_eq!(Scale::new(Some(4), Some(1000)).serves, Some(4));
}
