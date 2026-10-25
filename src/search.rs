//! Library search: every recipe becomes one EmbeddingGemma document vector
//! (`recipe_embeddings`), and a query is ranked against them by cosine
//! similarity. BM25 over every word of each recipe is the backup: it answers
//! when the embedding service is missing or down, or for every search when the
//! user picks it in Settings. The pure pieces (`recipe_document`, `rank`,
//! `keyword_terms`, `keyword_rank`) are unit-tested; `refresh_index` and
//! `search` orchestrate the embedding sidecar and the database through the
//! accessors in `recipes.rs`.

use crate::embeddings::{self, Embedder, Input};
use crate::recipes;
use crate::{AppState, Block};
use parking_lot::Mutex;
use sqlx::SqlitePool;
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
    time::Duration,
};
use tracing::{info, warn};

/// Longest recipe body (description, ingredients, method) that is embedded.
const BODY_LIMIT: usize = 6000;
/// Longest accepted search query, in characters.
const QUERY_LIMIT: usize = 200;
/// Most recipes a search returns.
pub(crate) const MAX_RESULTS: usize = 24;
/// A result must score within this much of the best score.
pub(crate) const RELATIVE_CUTOFF: f32 = 0.10;
/// And at least this much: on the live library, matching recipes scored
/// 0.64-0.80 and unrelated queries topped out near 0.62.
pub(crate) const MIN_SCORE: f32 = 0.60;
/// Added to the score of a recipe whose title contains the query.
pub(crate) const TITLE_BONUS: f32 = 0.10;
/// Stale recipes a search embeds before answering; the rest go to the background.
const INLINE_REFRESH: usize = 24;
/// Documents embedded and stored per round trip, so a failure keeps the earlier ones.
const REFRESH_BATCH: usize = 16;
/// Startup backfill: the sidecar may still be downloading its model.
const BACKFILL_ATTEMPTS: u32 = 60;
const BACKFILL_RETRY: Duration = Duration::from_secs(30);
/// BM25 term-frequency saturation.
const BM25_K1: f32 = 1.2;
/// BM25 document-length normalisation.
const BM25_B: f32 = 0.75;
/// Title words count this many times, so a title match outweighs a passing mention.
const TITLE_WEIGHT: usize = 3;
/// A word match must score at least this share of the best word match.
pub(crate) const KEYWORD_CUTOFF: f32 = 0.25;
/// Words too common in recipes and queries to rank by.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "by", "for", "from", "in", "into", "of", "on", "or", "the", "to",
    "with",
];

/// Users whose remaining stale recipes are already being embedded in the background.
static BACKGROUND: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// How many recipes `refresh_index` embedded and how many stale ones it left.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RefreshOutcome {
    pub(crate) refreshed: usize,
    pub(crate) remaining: usize,
}

/// One indexed recipe as ranked: id, title (for the title bonus) and its unit vector.
pub(crate) struct Indexed {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) vector: Vec<f32>,
}

/// What a library search produced: matching recipe ids, best first, and a
/// notice for the page (empty when there is nothing to say).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) ids: Vec<String>,
    pub(crate) notice: String,
}

/// Trims a raw `?q=` value and caps it at 200 characters.
pub(crate) fn normalize_query(raw: &str) -> String {
    raw.trim()
        .chars()
        .take(QUERY_LIMIT)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// The note shown while `count` recipes still wait for their vectors.
pub(crate) fn indexing_note(count: usize) -> String {
    let (noun, verb) = if count == 1 {
        ("recipe", "is")
    } else {
        ("recipes", "are")
    };
    format!("{count} {noun} {verb} still being indexed, so results may be incomplete.")
}

/// Collapses runs of whitespace (including newlines) to single spaces.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The text embedded for one recipe: the description, the ingredient lines
/// (`quantity unit name`, optional ones marked) and the numbered method, in
/// position order, capped at 6000 characters.
pub(crate) fn recipe_document(title: &str, description: &str, blocks: &[Block]) -> String {
    let mut ordered: Vec<&Block> = blocks.iter().collect();
    ordered.sort_by_key(|block| block.position);

    let ingredients: Vec<String> = ordered
        .iter()
        .filter(|block| block.section == "ingredient")
        .filter_map(|block| {
            let line = squash(&format!("{} {} {}", block.quantity, block.unit, block.text));
            (!line.is_empty()).then(|| {
                if block.optional() {
                    format!("{line} (optional)")
                } else {
                    line
                }
            })
        })
        .collect();
    let steps: Vec<String> = ordered
        .iter()
        .filter(|block| block.section == "step")
        .map(|block| squash(&block.text))
        .filter(|text| !text.is_empty())
        .enumerate()
        .map(|(index, text)| format!("{}. {text}", index + 1))
        .collect();

    let mut lines = Vec::new();
    let description = squash(description);
    if !description.is_empty() {
        lines.push(description);
    }
    if !ingredients.is_empty() {
        lines.push(format!("Ingredients: {}", ingredients.join("; ")));
    }
    if !steps.is_empty() {
        lines.push(format!("Method: {}", steps.join(" ")));
    }
    let mut body = lines.join("\n");
    if let Some((end, _)) = body.char_indices().nth(BODY_LIMIT) {
        body.truncate(end);
    }
    if body.is_empty() {
        body.push_str("none");
    }
    embeddings::document(title, &body)
}

/// Ids (with their fresh documents) of recipes whose stored document is
/// missing or differs from the document built from the current recipe, newest
/// edit first. `user` limits the check to one account.
pub(crate) async fn stale_documents(
    db: &SqlitePool,
    user: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let sources = recipes::index_sources(db, user)
        .await
        .map_err(|error| error.to_string())?;
    let stored = recipes::stored_documents(db, user)
        .await
        .map_err(|error| error.to_string())?;
    Ok(sources
        .into_iter()
        .filter_map(|source| {
            let document = recipe_document(&source.title, &source.description, &source.blocks);
            (stored.get(&source.id) != Some(&document)).then_some((source.id, document))
        })
        .collect())
}

/// Embeds up to `limit` stale recipes (of `user`, or of everyone) and stores
/// their vectors; `remaining` counts the stale recipes left undone. Progress
/// is saved batch by batch, so an embedding failure keeps what was stored.
pub(crate) async fn refresh_index(
    db: &SqlitePool,
    embedder: &Embedder,
    user: Option<&str>,
    limit: usize,
) -> Result<RefreshOutcome, String> {
    let mut stale = stale_documents(db, user).await?;
    let take = limit.min(stale.len());
    let remaining = stale.len() - take;
    stale.truncate(take);

    let mut refreshed = 0;
    for batch in stale.chunks(REFRESH_BATCH) {
        let inputs: Vec<Input> = batch
            .iter()
            .map(|(_, document)| Input::Text(document.clone()))
            .collect();
        let vectors = embedder.embed(&inputs).await?;
        if vectors.len() != batch.len() {
            return Err(format!(
                "embedding service returned {} vectors for {} documents",
                vectors.len(),
                batch.len()
            ));
        }
        for ((id, document), vector) in batch.iter().zip(&vectors) {
            recipes::store_embedding(db, id, document, vector)
                .await
                .map_err(|error| error.to_string())?;
            refreshed += 1;
        }
    }
    Ok(RefreshOutcome {
        refreshed,
        remaining,
    })
}

/// Ids of the entries to show for a query vector, best first: cosine
/// similarity plus a title bonus, at least `MIN_SCORE`, kept within
/// `RELATIVE_CUTOFF` of the best score, at most `MAX_RESULTS`.
pub(crate) fn rank(query: &str, query_vector: &[f32], entries: &[Indexed]) -> Vec<String> {
    let needle = query.trim().to_lowercase();
    let mut scored: Vec<(f32, &str)> = entries
        .iter()
        .map(|entry| {
            let bonus = if !needle.is_empty() && entry.title.to_lowercase().contains(&needle) {
                TITLE_BONUS
            } else {
                0.0
            };
            (
                embeddings::dot(query_vector, &entry.vector) + bonus,
                entry.id.as_str(),
            )
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let Some(&(best, _)) = scored.first() else {
        return Vec::new();
    };
    scored
        .into_iter()
        .take_while(|(score, _)| *score >= best - RELATIVE_CUTOFF && *score >= MIN_SCORE)
        .take(MAX_RESULTS)
        .map(|(_, id)| id.to_owned())
        .collect()
}

/// Lowercased words of `text`, split on anything but letters and digits,
/// stopwords dropped and simple plurals folded ("tomatoes" -> "tomato").
pub(crate) fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .filter(|word| !STOPWORDS.contains(&word.as_str()))
        .map(fold_plural)
        .collect()
}

fn fold_plural(word: String) -> String {
    let len = word.chars().count();
    if len > 4 && word.ends_with("ies") {
        format!("{}y", &word[..word.len() - 3])
    } else if len > 4
        && ["oes", "ches", "shes", "sses", "xes"]
            .iter()
            .any(|suffix| word.ends_with(suffix))
    {
        word[..word.len() - 2].to_owned()
    } else if len > 3 && word.ends_with('s') && !word.ends_with("ss") && !word.ends_with("us") {
        word[..word.len() - 1].to_owned()
    } else {
        word
    }
}

/// Every word of a recipe for BM25: the title (counted `TITLE_WEIGHT`
/// times), description, ingredient lines and method, uncapped.
pub(crate) fn keyword_terms(title: &str, description: &str, blocks: &[Block]) -> Vec<String> {
    let title = tokens(title);
    let mut terms = Vec::with_capacity(title.len() * TITLE_WEIGHT);
    for _ in 0..TITLE_WEIGHT {
        terms.extend(title.iter().cloned());
    }
    terms.extend(tokens(description));
    for block in blocks {
        terms.extend(tokens(&format!(
            "{} {} {}",
            block.quantity, block.unit, block.text
        )));
    }
    terms
}

/// Ids of the recipes (id, `keyword_terms`) that match a query by BM25, best
/// first: at least one query word, a score within `KEYWORD_CUTOFF` of the
/// best, at most `MAX_RESULTS`.
pub(crate) fn keyword_rank(query: &str, documents: &[(String, Vec<String>)]) -> Vec<String> {
    let mut terms = tokens(query);
    terms.sort();
    terms.dedup();
    if terms.is_empty() || documents.is_empty() {
        return Vec::new();
    }
    let count = documents.len() as f32;
    let average = (documents
        .iter()
        .map(|(_, words)| words.len())
        .sum::<usize>() as f32
        / count)
        .max(1.0);
    let frequencies: Vec<HashMap<&str, usize>> = documents
        .iter()
        .map(|(_, words)| {
            let mut found = HashMap::new();
            for word in words {
                if terms.contains(word) {
                    *found.entry(word.as_str()).or_default() += 1;
                }
            }
            found
        })
        .collect();
    let idf: Vec<f32> = terms
        .iter()
        .map(|term| {
            let with = frequencies
                .iter()
                .filter(|found| found.contains_key(term.as_str()))
                .count() as f32;
            (1.0 + (count - with + 0.5) / (with + 0.5)).ln()
        })
        .collect();

    let mut scored: Vec<(f32, &str)> = documents
        .iter()
        .zip(&frequencies)
        .filter(|(_, found)| !found.is_empty())
        .map(|((id, words), found)| {
            let norm = BM25_K1 * (1.0 - BM25_B + BM25_B * words.len() as f32 / average);
            let score = terms
                .iter()
                .zip(&idf)
                .filter_map(|(term, idf)| {
                    let tf = *found.get(term.as_str())? as f32;
                    Some(idf * tf * (BM25_K1 + 1.0) / (tf + norm))
                })
                .sum::<f32>();
            (score, id.as_str())
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let Some(&(best, _)) = scored.first() else {
        return Vec::new();
    };
    scored
        .into_iter()
        .take_while(|(score, _)| *score >= best * KEYWORD_CUTOFF)
        .take(MAX_RESULTS)
        .map(|(_, id)| id.to_owned())
        .collect()
}

/// Releases a user's background-refresh slot when the task ends.
struct BackgroundSlot(String);

impl Drop for BackgroundSlot {
    fn drop(&mut self) {
        BACKGROUND.lock().remove(&self.0);
    }
}

/// Embeds the rest of a user's stale recipes without blocking the request;
/// a no-op while one is already running for that user.
fn spawn_background_refresh(db: SqlitePool, embedder: Embedder, user_id: String) {
    if !BACKGROUND.lock().insert(user_id.clone()) {
        return;
    }
    tokio::spawn(async move {
        let _slot = BackgroundSlot(user_id.clone());
        match refresh_index(&db, &embedder, Some(&user_id), usize::MAX).await {
            Ok(outcome) => info!(
                refreshed = outcome.refreshed,
                "Library search index refreshed"
            ),
            Err(error) => warn!(%error, "Background library index refresh failed"),
        }
    });
}

/// Searches one user's library for `query` (already `normalize_query`d and
/// non-empty) with the engine chosen in Settings. Embedding search quietly
/// falls back to BM25 when the service is missing or fails, so the page
/// always gets results.
pub(crate) async fn search(state: &AppState, user_id: &str, query: &str) -> crate::Result<Outcome> {
    if crate::keyword_search_selected(&state.db, user_id).await? {
        return keyword_search(state, user_id, query).await;
    }
    let Some(embedder) = state.embedder.as_ref() else {
        return keyword_search(state, user_id, query).await;
    };
    match ranked(state, embedder, user_id, query).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            warn!(%error, "Library search failed; falling back to word matches");
            keyword_search(state, user_id, query).await
        }
    }
}

/// BM25 over every recipe of the user, read fresh, so it needs no index.
async fn keyword_search(state: &AppState, user_id: &str, query: &str) -> crate::Result<Outcome> {
    let documents: Vec<(String, Vec<String>)> = recipes::index_sources(&state.db, Some(user_id))
        .await?
        .into_iter()
        .map(|source| {
            let terms = keyword_terms(&source.title, &source.description, &source.blocks);
            (source.id, terms)
        })
        .collect();
    Ok(Outcome {
        ids: keyword_rank(query, &documents),
        notice: String::new(),
    })
}

async fn ranked(
    state: &AppState,
    embedder: &Embedder,
    user_id: &str,
    query: &str,
) -> Result<Outcome, String> {
    let refresh = refresh_index(&state.db, embedder, Some(user_id), INLINE_REFRESH).await?;
    if refresh.remaining > 0 {
        spawn_background_refresh(state.db.clone(), embedder.clone(), user_id.to_owned());
    }
    let notice = if refresh.remaining > 0 {
        indexing_note(refresh.remaining)
    } else {
        String::new()
    };
    let entries = recipes::indexed_recipes(&state.db, user_id)
        .await
        .map_err(|error| error.to_string())?;
    if entries.is_empty() {
        return Ok(Outcome {
            ids: Vec::new(),
            notice,
        });
    }
    let vector = embedder
        .embed(&[Input::Text(embeddings::query(query))])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "embedding service returned no query vector".to_owned())?;
    Ok(Outcome {
        ids: rank(query, &vector, &entries),
        notice,
    })
}

/// Startup task: embeds every user's stale recipes. The sidecar may still be
/// downloading its model, so a failure is retried every 30 s, up to 60 times.
pub(crate) async fn backfill(state: std::sync::Arc<AppState>) {
    let Some(embedder) = state.embedder.clone() else {
        return;
    };
    for attempt in 1..=BACKFILL_ATTEMPTS {
        match refresh_index(&state.db, &embedder, None, usize::MAX).await {
            Ok(outcome) => {
                info!(
                    refreshed = outcome.refreshed,
                    "Library search index is up to date"
                );
                return;
            }
            Err(error) => {
                warn!(attempt, %error, "Library search backfill failed");
                if attempt < BACKFILL_ATTEMPTS {
                    tokio::time::sleep(BACKFILL_RETRY).await;
                }
            }
        }
    }
    warn!("Library search backfill gave up; searches will index recipes on demand");
}
