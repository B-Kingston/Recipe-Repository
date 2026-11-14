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
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tracing::{info, warn};

/// Longest recipe title that is embedded, in bytes.
const TITLE_LIMIT: usize = 200;
/// Longest recipe body (description, ingredients, method) that is embedded,
/// in bytes. A token never covers less than a byte, so a document stays far
/// inside the sidecar's 8192-token context whatever its script; a document
/// the sidecar refused would fail every refresh and never be indexed.
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
/// Added to the score of a recipe whose title has the query's words, in order.
pub(crate) const TITLE_BONUS: f32 = 0.10;
/// Stale recipes a search embeds before answering; the rest go to the background.
const INLINE_REFRESH: usize = 24;
/// Documents embedded and stored per round trip, so a failure keeps the earlier ones.
const REFRESH_BATCH: usize = 16;
/// A fresh embedding of a stored document at least this close to its stored
/// vector means the sidecar still serves the model that built the index: the
/// same model reproduces a vector all but exactly, another lands far off.
const SAME_MODEL_MIN_COSINE: f32 = 0.95;
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

/// Users whose stale recipes are being embedded, inline or in the background.
static INDEXING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
/// Set while the startup backfill embeds every user's stale recipes.
static BACKFILLING: AtomicBool = AtomicBool::new(false);

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

/// Collapses runs of whitespace (including newlines) to single spaces and
/// drops web addresses: a Markdown link keeps its label, a bare URL goes.
/// A citation's slug ("…/spanish-rice-recipe-using-leftover-rice") once
/// outweighed the dish itself and filed an Egyptian tomato rice as Spanish.
pub(crate) fn squash(text: &str) -> String {
    text.split_whitespace()
        .map(
            |word| match word.find("http://").or_else(|| word.find("https://")) {
                Some(start) => word[..start]
                    .trim_end_matches(['(', '[', ']', '<'])
                    .trim_start_matches(['(', '[']),
                None => word,
            },
        )
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The text embedded for one recipe: the description, the ingredient lines
/// (`quantity unit name`, optional ones marked) and the numbered method, in
/// position order. The title is capped at 200 bytes, the body at 6000.
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
    body.truncate(char_floor(&body, BODY_LIMIT));
    if body.is_empty() {
        body.push_str("none");
    }
    let title = title.trim();
    embeddings::document(&title[..char_floor(title, TITLE_LIMIT)], &body)
}

/// The longest prefix of `text`, in bytes, that ends on a character
/// boundary and is at most `limit` bytes.
fn char_floor(text: &str, limit: usize) -> usize {
    if text.len() <= limit {
        return text.len();
    }
    (0..=limit)
        .rev()
        .find(|&end| text.is_char_boundary(end))
        .unwrap_or(0)
}

/// A recipe whose search vector must be built: its fresh document and the
/// recipe's `updated_at` the document was built from.
pub(crate) struct StaleDocument {
    pub(crate) id: String,
    pub(crate) document: String,
    pub(crate) updated_at: String,
}

/// Recipes (of `user`, or of everyone) whose stored vector does not match
/// their document, newest edit first. With `all`, every recipe's document is
/// rebuilt and compared, which also catches a change to `recipe_document`
/// itself (the startup backfill); otherwise only the recipes edited since
/// their vector was built, or without a usable one, which keeps the check
/// to one query per search. A recipe whose edit left its document alone is
/// marked current instead of embedded again.
pub(crate) async fn stale_documents(
    db: &SqlitePool,
    user: Option<&str>,
    all: bool,
) -> Result<Vec<StaleDocument>, String> {
    let candidates = recipes::index_candidates(db, user, all)
        .await
        .map_err(|error| error.to_string())?;
    let mut stale = Vec::new();
    for candidate in candidates {
        let source = candidate.source;
        let document = recipe_document(&source.title, &source.description, &source.blocks);
        if candidate.stored.as_deref() != Some(document.as_str()) {
            stale.push(StaleDocument {
                id: source.id,
                document,
                updated_at: candidate.updated_at,
            });
        } else if candidate.built_from.as_deref() != Some(candidate.updated_at.as_str()) {
            recipes::confirm_embedding(db, &source.id, &document, &candidate.updated_at)
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(stale)
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
    let stale = stale_documents(db, user, false).await?;
    embed_documents(db, embedder, stale, limit).await
}

async fn embed_documents(
    db: &SqlitePool,
    embedder: &Embedder,
    mut stale: Vec<StaleDocument>,
    limit: usize,
) -> Result<RefreshOutcome, String> {
    let take = limit.min(stale.len());
    let remaining = stale.len() - take;
    stale.truncate(take);

    let mut refreshed = 0;
    for batch in stale.chunks(REFRESH_BATCH) {
        let inputs: Vec<Input> = batch
            .iter()
            .map(|stale| Input::Text(stale.document.clone()))
            .collect();
        let vectors = embedder.embed(&inputs).await?;
        if vectors.len() != batch.len() {
            return Err(format!(
                "embedding service returned {} vectors for {} documents",
                vectors.len(),
                batch.len()
            ));
        }
        for (stale, vector) in batch.iter().zip(&vectors) {
            recipes::store_embedding(db, &stale.id, &stale.document, vector, &stale.updated_at)
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
    let needle = tokens(query);
    let mut scored: Vec<(f32, &str)> = entries
        .iter()
        .map(|entry| {
            let bonus = if has_phrase(&tokens(&entry.title), &needle) {
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

/// Whether `words` hold all of `phrase`, consecutive and whole.
fn has_phrase(words: &[String], phrase: &[String]) -> bool {
    !phrase.is_empty() && words.windows(phrase.len()).any(|window| window == phrase)
}

/// Lowercased words of `text`, split on anything but letters and digits,
/// stopwords dropped and each cut to a stem its singular and plural share
/// ("tomatoes" and "tomato" -> "tomato").
pub(crate) fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .filter(|word| !STOPWORDS.contains(&word.as_str()))
        .map(stem)
        .collect()
}

/// Drops a plural "s", then a final "e", then turns a final "y" into "i",
/// so both forms of a word meet: dish(es) -> "dish", cookie(s) -> "cooki",
/// berry/berries -> "berri", quiche(s) -> "quich". Words of three letters or
/// fewer, and "-ss" and "-us" words (glass, hummus), keep their "s".
fn stem(mut word: String) -> String {
    let long = |word: &String| word.chars().count() > 3;
    if long(&word) && word.ends_with('s') && !word.ends_with("ss") && !word.ends_with("us") {
        word.pop();
    }
    if long(&word) && word.ends_with('e') {
        word.pop();
    }
    if long(&word) && word.ends_with('y') {
        word.pop();
        word.push('i');
    }
    word
}

/// Every word of a recipe for BM25: the title (counted `TITLE_WEIGHT`
/// times), description, ingredient lines and method, uncapped.
pub(crate) fn keyword_terms(title: &str, description: &str, blocks: &[Block]) -> Vec<String> {
    let title = tokens(title);
    let mut terms = Vec::with_capacity(title.len() * TITLE_WEIGHT);
    for _ in 0..TITLE_WEIGHT {
        terms.extend(title.iter().cloned());
    }
    terms.extend(tokens(&squash(description)));
    for block in blocks {
        terms.extend(tokens(&squash(&format!(
            "{} {} {}",
            block.quantity, block.unit, block.text
        ))));
    }
    terms
}

/// Ids of the recipes (id, `keyword_terms`) that match a query by BM25, best
/// first: at least one query word, a score of at least `KEYWORD_CUTOFF`
/// times the best, at most `MAX_RESULTS`.
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

/// A user's claim on embedding their stale recipes, so an inline refresh, a
/// background refresh, and the startup backfill never embed the same
/// recipes side by side. Released on drop.
pub(crate) struct IndexingSlot(String);

impl IndexingSlot {
    /// None while another pass is indexing the user's recipes.
    pub(crate) fn claim(user_id: &str) -> Option<Self> {
        if BACKFILLING.load(Ordering::Acquire) {
            return None;
        }
        INDEXING
            .lock()
            .insert(user_id.to_owned())
            .then(|| Self(user_id.to_owned()))
    }
}

impl Drop for IndexingSlot {
    fn drop(&mut self) {
        INDEXING.lock().remove(&self.0);
    }
}

/// Embeds the rest of a user's stale recipes without blocking the request;
/// a no-op while another pass is indexing that user's recipes.
fn spawn_background_refresh(db: SqlitePool, embedder: Embedder, user_id: String) {
    let Some(slot) = IndexingSlot::claim(&user_id) else {
        return;
    };
    tokio::spawn(async move {
        let _slot = slot;
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
        Ok(Ranked::Results(outcome)) => Ok(outcome),
        // Most of the library is not embedded yet (another pass, such as the
        // startup backfill, is still indexing it): word matches stand in, so
        // a search does not miss recipes the index has not reached.
        Ok(Ranked::Unindexed(waiting)) => {
            let mut outcome = keyword_search(state, user_id, query).await?;
            outcome.notice = indexing_note(waiting);
            Ok(outcome)
        }
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

/// Embeds up to `INLINE_REFRESH` of a user's stale recipes now, with a
/// page's short timeout, and hands the rest (or a failed batch) to a
/// background task; returns how many were left over. While another pass is
/// already indexing the user's recipes, it only counts them.
pub(crate) async fn refresh_user(
    state: &AppState,
    embedder: &Embedder,
    user_id: &str,
) -> Result<usize, String> {
    let Some(slot) = IndexingSlot::claim(user_id) else {
        return Ok(stale_documents(&state.db, Some(user_id), false)
            .await?
            .len());
    };
    let refresh = refresh_index(
        &state.db,
        &embedder.interactive(),
        Some(user_id),
        INLINE_REFRESH,
    )
    .await;
    drop(slot);
    match refresh {
        Ok(outcome) if outcome.remaining == 0 => Ok(0),
        refresh => {
            spawn_background_refresh(state.db.clone(), embedder.clone(), user_id.to_owned());
            refresh.map(|outcome| outcome.remaining)
        }
    }
}

/// What an embedding search found.
enum Ranked {
    Results(Outcome),
    /// More of the library (this many recipes) is waiting to be embedded
    /// than is embedded, so a ranking would miss most of it.
    Unindexed(usize),
}

/// Ranks the user's embedded recipes for `query`.
async fn ranked(
    state: &AppState,
    embedder: &Embedder,
    user_id: &str,
    query: &str,
) -> Result<Ranked, String> {
    let remaining = refresh_user(state, embedder, user_id).await?;
    let notice = if remaining > 0 {
        indexing_note(remaining)
    } else {
        String::new()
    };
    let entries = recipes::indexed_recipes(&state.db, user_id)
        .await
        .map_err(|error| error.to_string())?;
    if remaining > entries.len() {
        return Ok(Ranked::Unindexed(remaining));
    }
    if entries.is_empty() {
        return Ok(Ranked::Results(Outcome {
            ids: Vec::new(),
            notice,
        }));
    }
    let vector = embedder
        .interactive()
        .embed(&[Input::Text(embeddings::query(query))])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "embedding service returned no query vector".to_owned())?;
    Ok(Ranked::Results(Outcome {
        ids: rank(query, &vector, &entries),
        notice,
    }))
}

/// Startup task: waits for the sidecar, which may still be downloading its
/// model, then embeds the library folder prototypes and every stale recipe,
/// one after the other so they do not queue against each other in its
/// single slot.
pub(crate) async fn backfill(state: Arc<AppState>) {
    let Some(embedder) = state.embedder.clone() else {
        return;
    };
    if !embedder.wait_until_ready().await {
        warn!("The embedding service never answered; searches will index recipes on demand");
        return;
    }
    crate::folders::warm(&embedder).await;
    let outcome = {
        let _backfilling = Backfilling::start();
        reindex(&state.db, &embedder).await
    };
    match outcome {
        Ok(refreshed) => info!(refreshed, "Library search index is up to date"),
        Err(error) => warn!(
            %error,
            "Library search backfill failed; searches will index recipes on demand"
        ),
    }
}

/// Marks the backfill as running until dropped.
struct Backfilling;

impl Backfilling {
    fn start() -> Self {
        BACKFILLING.store(true, Ordering::Release);
        Self
    }
}

impl Drop for Backfilling {
    fn drop(&mut self) {
        BACKFILLING.store(false, Ordering::Release);
    }
}

/// Brings every user's search vectors up to date by rebuilding and comparing
/// every recipe's document; when the sidecar serves another model than the
/// one that built the index, every vector is built again. Returns how many
/// recipes were embedded.
pub(crate) async fn reindex(db: &SqlitePool, embedder: &Embedder) -> Result<usize, String> {
    if model_changed(db, embedder).await? {
        warn!("The embedding model changed; every recipe is indexed again");
        recipes::clear_embeddings(db)
            .await
            .map_err(|error| error.to_string())?;
    }
    let stale = stale_documents(db, None, true).await?;
    Ok(embed_documents(db, embedder, stale, usize::MAX)
        .await?
        .refreshed)
}

/// Whether the stored vectors came from another model than the sidecar
/// serves now: the newest one is compared with a fresh embedding of its
/// document.
async fn model_changed(db: &SqlitePool, embedder: &Embedder) -> Result<bool, String> {
    let Some((document, stored)) = recipes::newest_embedding(db)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    let fresh = embedder.embed(&[Input::Text(document)]).await?;
    Ok(fresh
        .first()
        .is_none_or(|fresh| embeddings::dot(fresh, &stored) < SAME_MODEL_MIN_COSINE))
}
