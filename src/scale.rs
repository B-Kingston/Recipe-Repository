//! Servings scaling for the recipe page: reads the amount at the start of an
//! ingredient quantity or a step's ingredient line ("1 1/2", "½", "200g",
//! "2–3"), multiplies it, and writes it back rounded the way a cook measures:
//! metric weights and volumes to sensible whole numbers, everything else to
//! kitchen fractions. Text with no leading amount ("to taste", "a pinch")
//! stays as it is. Pure functions, no I/O.

/// The servings links the recipe page offers, besides the recipe's own.
pub(crate) const SERVING_CHOICES: std::ops::RangeInclusive<i64> = 1..=6;
/// The largest `?serves=` the page accepts.
pub(crate) const MAX_SERVINGS: i64 = 48;

/// How a page shows a recipe's amounts: the servings it was written for, the
/// servings asked for, and the query suffix that keeps the choice on links.
#[derive(Debug, Clone)]
pub(crate) struct Scale {
    pub(crate) original: Option<i64>,
    pub(crate) serves: Option<i64>,
    pub(crate) factor: f64,
    /// `&serves=N` when scaled, else empty.
    pub(crate) query: String,
}

/// One servings link: its count, whether it is the one shown, and whether
/// it is the recipe as written.
#[derive(Debug, Clone)]
pub(crate) struct ServesLink {
    pub(crate) serves: i64,
    pub(crate) active: bool,
    pub(crate) as_written: bool,
}

impl Scale {
    /// Unscaled, for pages without a servings choice.
    pub(crate) fn none() -> Self {
        Self {
            original: None,
            serves: None,
            factor: 1.0,
            query: String::new(),
        }
    }

    /// The scale for a recipe written for `original` servings when the page
    /// asks for `requested`. A recipe without servings never scales; an
    /// out-of-range request shows the recipe as written.
    pub(crate) fn new(original: Option<i64>, requested: Option<i64>) -> Self {
        let Some(original) = original.filter(|servings| *servings > 0) else {
            return Self::none();
        };
        let serves = requested
            .filter(|serves| (1..=MAX_SERVINGS).contains(serves))
            .unwrap_or(original);
        Self {
            original: Some(original),
            serves: Some(serves),
            factor: serves as f64 / original as f64,
            query: if serves == original {
                String::new()
            } else {
                format!("&serves={serves}")
            },
        }
    }

    pub(crate) fn is_scaled(&self) -> bool {
        self.original != self.serves
    }

    /// The servings links: 1 to 6, plus the recipe's own count when larger.
    pub(crate) fn choices(&self) -> Vec<i64> {
        let mut choices: Vec<i64> = SERVING_CHOICES.collect();
        for extra in [self.original, self.serves].into_iter().flatten() {
            if !choices.contains(&extra) {
                choices.push(extra);
            }
        }
        choices.sort_unstable();
        choices
    }

    /// The servings links as the page draws them.
    pub(crate) fn links(&self) -> Vec<ServesLink> {
        self.choices()
            .into_iter()
            .map(|serves| ServesLink {
                serves,
                active: self.serves == Some(serves),
                as_written: self.original == Some(serves),
            })
            .collect()
    }

    /// An ingredient quantity, scaled. `unit` is the ingredient's own unit
    /// field (it decides the rounding); empty means the quantity carries it.
    pub(crate) fn quantity(&self, quantity: &str, unit: &str) -> String {
        scale_text(quantity, unit, self.factor)
    }

    /// A free-text line that starts with its amount, such as "200 g flour".
    pub(crate) fn line(&self, line: &str) -> String {
        scale_text(line, "", self.factor)
    }
}

/// Scales the amount at the start of `text` by `factor`, keeping the rest of
/// the text as written. Returns `text` unchanged when it has no leading
/// amount or the factor is 1.
pub(crate) fn scale_text(text: &str, unit: &str, factor: f64) -> String {
    if (factor - 1.0).abs() < 1e-9 || !factor.is_finite() || factor <= 0.0 {
        return text.to_string();
    }
    let leading = text.len() - text.trim_start().len();
    let body = &text[leading..];
    let Some(amount) = parse_amount(body) else {
        return text.to_string();
    };
    let rest = &body[amount.end..];
    let unit_word = if unit.trim().is_empty() {
        rest.split(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .find(|word| !word.is_empty())
            .unwrap_or_default()
    } else {
        unit.trim()
    };
    let metric = is_metric(unit_word);
    let values: Vec<String> = amount
        .values
        .iter()
        .map(|value| format_amount(value * factor, metric))
        .collect();
    let scaled = match values.as_slice() {
        [single] => single.clone(),
        [low, high] => format!("{low}{}{high}", amount.separator),
        _ => return text.to_string(),
    };
    format!("{}{scaled}{rest}", &text[..leading])
}

struct Amount {
    values: Vec<f64>,
    /// The text between the two ends of a range, kept as written.
    separator: String,
    /// Bytes of the text the amount took.
    end: usize,
}

/// A leading number, fraction, mixed number, or range of two of those.
fn parse_amount(text: &str) -> Option<Amount> {
    let (first, mut end) = parse_number(text)?;
    let mut values = vec![first];
    let mut separator = String::new();
    let after = &text[end..];
    let trimmed = after.trim_start();
    let gap = after.len() - trimmed.len();
    for dash in ["-", "\u{2013}", "\u{2014}", "to "] {
        if let Some(tail) = trimmed.strip_prefix(dash) {
            let tail_trimmed = tail.trim_start();
            if let Some((second, used)) = parse_number(tail_trimmed) {
                if second >= first {
                    let consumed = gap + dash.len() + (tail.len() - tail_trimmed.len());
                    separator = after[..consumed].to_string();
                    end += consumed + used;
                    values.push(second);
                }
            }
            break;
        }
    }
    Some(Amount {
        values,
        separator,
        end,
    })
}

/// One number at the start of `text`: "2", "1.5", "1/2", "1 1/2", "½", "1½",
/// "1 ½". Returns the value and the bytes it took.
fn parse_number(text: &str) -> Option<(f64, usize)> {
    if let Some((value, used)) = glyph(text) {
        return Some((value, used));
    }
    let digits = text
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
        .map_or(text.len(), |(index, _)| index);
    if digits == 0 {
        return None;
    }
    let whole_text = text[..digits].trim_end_matches('.');
    let whole: f64 = whole_text.parse().ok()?;
    let mut end = whole_text.len();
    let rest = &text[end..];
    // A plain fraction: "1/2".
    if let Some(denominator) = rest.strip_prefix('/') {
        let (bottom, used) = integer(denominator)?;
        if bottom == 0 || whole_text.contains('.') {
            return None;
        }
        return Some((whole / bottom as f64, end + 1 + used));
    }
    // A glued or spaced fraction after the whole number: "1½", "1 ½", "1 1/2".
    if !whole_text.contains('.') {
        let spaced = rest.trim_start_matches(' ');
        let gap = rest.len() - spaced.len();
        if gap <= 1 {
            if let Some((part, used)) = glyph(spaced) {
                return Some((whole + part, end + gap + used));
            }
        }
        if gap == 1 {
            if let Some((top, top_used)) = integer(spaced) {
                if let Some(bottom_text) = spaced[top_used..].strip_prefix('/') {
                    if let Some((bottom, bottom_used)) = integer(bottom_text) {
                        if bottom != 0 && top < bottom {
                            end += gap + top_used + 1 + bottom_used;
                            return Some((whole + top as f64 / bottom as f64, end));
                        }
                    }
                }
            }
        }
    }
    Some((whole, end))
}

fn integer(text: &str) -> Option<(u32, usize)> {
    let used = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map_or(text.len(), |(index, _)| index);
    if used == 0 {
        return None;
    }
    text[..used].parse().ok().map(|value| (value, used))
}

const GLYPHS: [(char, f64); 11] = [
    ('½', 0.5),
    ('⅓', 1.0 / 3.0),
    ('⅔', 2.0 / 3.0),
    ('¼', 0.25),
    ('¾', 0.75),
    ('⅛', 0.125),
    ('⅜', 0.375),
    ('⅝', 0.625),
    ('⅞', 0.875),
    ('⅕', 0.2),
    ('⅙', 1.0 / 6.0),
];

fn glyph(text: &str) -> Option<(f64, usize)> {
    let first = text.chars().next()?;
    GLYPHS
        .iter()
        .find(|(glyph, _)| *glyph == first)
        .map(|(_, value)| (*value, first.len_utf8()))
}

/// Metric weights and volumes round to decimals; everything else (spoons,
/// cups, ounces, counted items) rounds to kitchen fractions.
fn is_metric(unit: &str) -> bool {
    let unit = unit.trim_end_matches('.').to_lowercase();
    matches!(
        unit.as_str(),
        "g" | "gr"
            | "gram"
            | "grams"
            | "kg"
            | "kgs"
            | "kilogram"
            | "kilograms"
            | "mg"
            | "ml"
            | "millilitre"
            | "millilitres"
            | "milliliter"
            | "milliliters"
            | "l"
            | "litre"
            | "litres"
            | "liter"
            | "liters"
            | "cl"
            | "dl"
    )
}

fn format_amount(value: f64, metric: bool) -> String {
    if metric {
        format_metric(value)
    } else {
        format_fraction(value)
    }
}

/// 437 → "435", 37.4 → "37", 2.25 → "2.3", 0.333 → "0.33".
fn format_metric(value: f64) -> String {
    let rounded = if value >= 100.0 {
        (value / 5.0).round() * 5.0
    } else if value >= 10.0 {
        value.round()
    } else if value >= 1.0 {
        (value * 10.0).round() / 10.0
    } else {
        ((value * 100.0).round() / 100.0).max(0.01)
    };
    let text = format!("{rounded:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The nearest kitchen fraction: halves, thirds, quarters, and eighths, as
/// "1½"; whole numbers from 10 up. Never rounds a real amount down to 0.
fn format_fraction(value: f64) -> String {
    if value >= 10.0 {
        return format!("{}", value.round() as i64);
    }
    const PARTS: [(f64, &str); 8] = [
        (0.0, ""),
        (0.125, "⅛"),
        (0.25, "¼"),
        (1.0 / 3.0, "⅓"),
        (0.5, "½"),
        (2.0 / 3.0, "⅔"),
        (0.75, "¾"),
        (1.0, ""),
    ];
    let mut whole = value.trunc();
    let remainder = value - whole;
    let (part, glyph) = PARTS
        .iter()
        .min_by(|a, b| {
            (a.0 - remainder)
                .abs()
                .partial_cmp(&(b.0 - remainder).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
        .unwrap_or((0.0, ""));
    if part == 1.0 {
        whole += 1.0;
    }
    match (whole as i64, glyph) {
        (0, "") => "⅛".to_string(),
        (0, glyph) => glyph.to_string(),
        (whole, glyph) => format!("{whole}{glyph}"),
    }
}
