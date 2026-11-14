use crate::scale::{Scale, scale_text};

#[test]
fn metric_amounts_round_to_round_figures() {
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
fn mixed_numbers_read_in_every_common_spelling() {
    // The American hyphenated style is one amount, not a range.
    assert_eq!(scale_text("1-1/2 cups", "", 2.0), "3 cups");
    assert_eq!(scale_text("1-1/2 cups", "", 0.5), "¾ cups");
    assert_eq!(scale_text("3-1/2", "cup", 2.0), "7");
    assert_eq!(scale_text("1-½ tsp", "", 2.0), "3 tsp");
    // The fraction slash, every vulgar fraction, and odd spacing.
    assert_eq!(scale_text("1⁄2 cup", "", 2.0), "1 cup");
    assert_eq!(scale_text("1 1⁄2 cups", "", 2.0), "3 cups");
    assert_eq!(scale_text("1⅚ cups", "", 2.0), "3⅔ cups");
    assert_eq!(scale_text("1  1/2 cups", "", 2.0), "3 cups");
    assert_eq!(scale_text("1\u{a0}½ cups", "", 2.0), "3 cups");
    // A count before a size is not a fraction.
    assert_eq!(
        scale_text("2 3-inch pieces ginger", "", 2.0),
        "4 3-inch pieces ginger"
    );
}

#[test]
fn a_second_measure_after_a_slash_scales_too() {
    assert_eq!(
        scale_text("400g/14oz can tomatoes", "", 2.0),
        "800g/28oz can tomatoes"
    );
    assert_eq!(scale_text("200 g / 7 oz", "", 0.5), "100 g / 3½ oz");
    assert_eq!(scale_text("2 tbsp/serving", "", 2.0), "4 tbsp/serving");
}

#[test]
fn amounts_that_cannot_be_read_whole_are_kept() {
    // A decimal comma, or a thousands separator: the number goes on.
    assert_eq!(scale_text("1,5 kg", "", 2.0), "1,5 kg");
    assert_eq!(scale_text("0,5", "l", 2.0), "0,5");
    assert_eq!(scale_text("1,000 g", "", 2.0), "1,000 g");
    // A fraction after a decimal, or one that is not a proper fraction.
    assert_eq!(scale_text("1.5 1/2 cup", "", 2.0), "1.5 1/2 cup");
    assert_eq!(scale_text("2 4/3 cups", "", 2.0), "2 4/3 cups");
    // A glyph the reader does not know.
    assert_eq!(scale_text("1⅟ cup", "", 2.0), "1⅟ cup");
    // A dash before a smaller number is neither a range nor a mixed number.
    assert_eq!(scale_text("3-2 eggs", "", 2.0), "3-2 eggs");
    assert_eq!(scale_text("2 - 1/2 cup", "", 2.0), "2 - 1/2 cup");
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
    assert_eq!(scale.page_query(), "?serves=2");
    assert_eq!(scale.choices(), vec![1, 2, 3, 4, 5, 6]);

    let as_written = Scale::new(Some(8), None);
    assert!(!as_written.is_scaled());
    assert_eq!(as_written.query, "");
    assert_eq!(as_written.page_query(), "");
    assert_eq!(as_written.choices(), vec![1, 2, 3, 4, 5, 6, 8]);

    // No servings: nothing to scale from. Out of range: as written.
    assert_eq!(Scale::new(None, Some(3)).serves, None);
    assert_eq!(Scale::new(Some(4), Some(0)).serves, Some(4));
    assert_eq!(Scale::new(Some(4), Some(1000)).serves, Some(4));
}
