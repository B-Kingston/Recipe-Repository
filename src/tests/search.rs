use crate::auth::AuthUser;
use crate::embeddings::{DIMENSIONS, Embedder, Input};
use crate::recipes::{home, recipe_vector, store_embedding};
use crate::search::{
    Indexed, IndexingSlot, MAX_RESULTS, StaleDocument, keyword_rank, keyword_terms,
    normalize_query, rank, recipe_document, refresh_user, reindex, search, squash, stale_documents,
    tokens,
};
use crate::{AppState, Block, HomeQuery, stamp};
use axum::extract::{Query, State};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

async fn database() -> SqlitePool {
    let db = super::memory_db().await;
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,title,description,user_id,created_at,updated_at) VALUES('r','Test','A quick supper','u1',?,?),('other','Other','','u2',?,?)",
    )
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    db
}

use super::test_state as state;

fn block(
    section: &str,
    position: i64,
    text: &str,
    quantity: &str,
    unit: &str,
    optional: bool,
) -> Block {
    Block {
        id: format!("{section}{position}"),
        section: section.into(),
        position,
        text: text.into(),
        quantity: quantity.into(),
        unit: unit.into(),
        optional: i64::from(optional),
    }
}

/// A unit vector whose dot product with `[1, 0, 0]` is `similarity`.
fn similar(similarity: f32) -> Vec<f32> {
    vec![similarity, (1.0 - similarity * similarity).sqrt(), 0.0]
}

fn entry(id: &str, title: &str, similarity: f32) -> Indexed {
    Indexed {
        id: id.into(),
        title: title.into(),
        vector: similar(similarity),
    }
}

const QUERY_VECTOR: [f32; 3] = [1.0, 0.0, 0.0];

#[test]
fn rank_keeps_only_scores_within_the_cutoff_of_the_best() {
    let entries = [
        entry("far", "Far", 0.80),
        entry("best", "Best", 0.95),
        entry("near", "Near", 0.88),
    ];
    assert_eq!(rank("anything", &QUERY_VECTOR, &entries), ["best", "near"]);
}

/// A query that matches nothing well must show no results, even though
/// every recipe has some similarity and the weakest is within the cutoff.
#[test]
fn rank_drops_everything_below_the_relevance_floor() {
    let entries = [entry("steak", "Steak", 0.59), entry("rice", "Rice", 0.55)];
    assert!(rank("pasta", &QUERY_VECTOR, &entries).is_empty());
    let entries = [entry("rice", "Rice", 0.66), entry("cake", "Cake", 0.58)];
    assert_eq!(rank("rice dish", &QUERY_VECTOR, &entries), ["rice"]);
}

#[test]
fn rank_returns_at_most_the_result_cap() {
    let entries: Vec<Indexed> = (0..MAX_RESULTS + 10)
        .map(|index| entry(&format!("r{index:02}"), "Same", 0.9))
        .collect();
    assert_eq!(rank("anything", &QUERY_VECTOR, &entries).len(), MAX_RESULTS);
}

#[test]
fn rank_of_nothing_is_empty() {
    assert!(rank("anything", &QUERY_VECTOR, &[]).is_empty());
}

#[test]
fn a_title_match_outranks_a_closer_vector_case_insensitively() {
    let entries = [
        entry("plain", "Plain soup", 0.95),
        entry("lemon", "Lemon cake", 0.89),
    ];
    assert_eq!(rank("LEMON", &QUERY_VECTOR, &entries), ["lemon", "plain"]);
    assert_eq!(rank("pasta", &QUERY_VECTOR, &entries), ["plain", "lemon"]);
}

#[test]
fn queries_are_trimmed_and_capped_at_200_characters() {
    assert_eq!(normalize_query("  soup \n"), "soup");
    assert_eq!(normalize_query("   "), "");
    assert_eq!(normalize_query(&"é".repeat(300)).chars().count(), 200);
}

#[test]
fn the_document_has_description_ingredients_and_numbered_steps_in_position_order() {
    let blocks = [
        block("step", 1, "Bake\nuntil golden.", "", "", false),
        block("ingredient", 1, "salt", "1", "tsp", true),
        block("step", 0, "Mix the flour.", "", "", false),
        block("ingredient", 0, "flour", "2", "cup", false),
        block("step", 2, "   ", "", "", false),
        block("ingredient", 2, "  ", "", "", false),
    ];
    let document = recipe_document("Bread", "A simple loaf.", &blocks);
    assert_eq!(
        document,
        "title: Bread | text: A simple loaf.\n\
         Ingredients: 2 cup flour; 1 tsp salt (optional)\n\
         Method: 1. Mix the flour. 2. Bake until golden."
    );
}

#[test]
fn a_recipe_without_content_embeds_its_title_alone() {
    assert_eq!(
        recipe_document("Toast", "", &[]),
        "title: Toast | text: none"
    );
}

#[test]
fn the_document_is_capped_in_bytes_on_a_character_boundary() {
    let long = "é".repeat(7000);
    let document = recipe_document(&"T".repeat(300), &long, &[]);
    assert_eq!(
        document,
        format!("title: {} | text: {}", "T".repeat(200), "é".repeat(3000))
    );
    // Byte 6000 falls inside a two-byte character, so the cut comes before it.
    let document = recipe_document("Odd", &format!("a{}", "é".repeat(5000)), &[]);
    assert_eq!(
        document,
        format!("title: Odd | text: a{}", "é".repeat(2999))
    );
}

/// A unit vector of the sidecar's size along one axis.
fn unit(axis: usize) -> Vec<f32> {
    let mut vector = vec![0.0; DIMENSIONS];
    vector[axis] = 1.0;
    vector
}

async fn store(db: &SqlitePool, stale: &StaleDocument) {
    store_embedding(db, &stale.id, &stale.document, &unit(0), &stale.updated_at)
        .await
        .unwrap();
}

/// Runs one statement whose only parameter is a fresh `updated_at`.
async fn touch(db: &SqlitePool, sql: &str) {
    sqlx::query(sql).bind(stamp()).execute(db).await.unwrap();
}

/// Edits a block the way the handlers do: the recipe's updated_at moves too.
async fn edit_block(db: &SqlitePool, sql: &str) {
    sqlx::query(sql).execute(db).await.unwrap();
    touch(db, "UPDATE recipes SET updated_at=? WHERE id='r'").await;
}

#[tokio::test]
async fn recipes_are_stale_until_embedded_and_again_after_an_edit() {
    let db = database().await;
    edit_block(&db, "INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('s0','r','step',0,'Boil water')").await;

    let stale = stale_documents(&db, Some("u1"), false).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, "r");
    assert!(stale[0].document.contains("Boil water"));
    store(&db, &stale[0]).await;
    assert!(
        stale_documents(&db, Some("u1"), false)
            .await
            .unwrap()
            .is_empty()
    );

    edit_block(
        &db,
        "UPDATE recipe_blocks SET text='Boil milk' WHERE id='s0'",
    )
    .await;
    let stale = stale_documents(&db, Some("u1"), false).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert!(stale[0].document.contains("Boil milk"));
    store(&db, &stale[0]).await;

    touch(
        &db,
        "UPDATE recipes SET title='Renamed', updated_at=? WHERE id='r'",
    )
    .await;
    let stale = stale_documents(&db, Some("u1"), false).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert!(stale[0].document.starts_with("title: Renamed"));
}

#[tokio::test]
async fn an_edit_that_leaves_the_document_alone_is_marked_current_without_embedding() {
    let db = database().await;
    let stale = stale_documents(&db, Some("u1"), false).await.unwrap();
    store(&db, &stale[0]).await;

    touch(
        &db,
        "UPDATE recipes SET servings=4, updated_at=? WHERE id='r'",
    )
    .await;
    assert!(
        stale_documents(&db, Some("u1"), false)
            .await
            .unwrap()
            .is_empty()
    );
    let (built_from, updated_at): (String, String) = sqlx::query_as(
        "SELECT e.updated_at, r.updated_at FROM recipe_embeddings e JOIN recipes r ON r.id=e.recipe_id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(built_from, updated_at);
}

#[tokio::test]
async fn only_the_full_check_rebuilds_documents_of_unedited_recipes() {
    let db = database().await;
    for stale in stale_documents(&db, None, false).await.unwrap() {
        store(&db, &stale).await;
    }
    // A change the recipe's updated_at does not show, like a new
    // `recipe_document` format, only the full comparison catches.
    sqlx::query("UPDATE recipe_embeddings SET document='an older format' WHERE recipe_id='r'")
        .execute(&db)
        .await
        .unwrap();
    assert!(stale_documents(&db, None, false).await.unwrap().is_empty());
    let stale = stale_documents(&db, None, true).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, "r");

    // A stored vector that is not a full vector is stale either way.
    sqlx::query("UPDATE recipe_embeddings SET vector=x'00' WHERE recipe_id='other'")
        .execute(&db)
        .await
        .unwrap();
    let stale = stale_documents(&db, None, false).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, "other");
}

#[tokio::test]
async fn staleness_is_scoped_to_the_requested_user() {
    let db = database().await;
    let ids = |stale: Vec<StaleDocument>| {
        let mut ids: Vec<String> = stale.into_iter().map(|stale| stale.id).collect();
        ids.sort();
        ids
    };
    assert_eq!(
        ids(stale_documents(&db, Some("u2"), false).await.unwrap()),
        ["other"]
    );
    assert_eq!(
        ids(stale_documents(&db, None, true).await.unwrap()),
        ["other", "r"]
    );
}

#[tokio::test]
async fn storing_a_vector_for_a_deleted_or_since_edited_recipe_is_skipped() {
    let db = database().await;
    store_embedding(
        &db,
        "gone",
        "document",
        &unit(0),
        "2026-01-01T00:00:00+00:00",
    )
    .await
    .unwrap();
    let stale = stale_documents(&db, Some("u1"), false).await.unwrap();
    touch(
        &db,
        "UPDATE recipes SET title='Edited', updated_at=? WHERE id='r'",
    )
    .await;
    store(&db, &stale[0]).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recipe_embeddings")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

async fn search_page(db: SqlitePool, query: &str) -> String {
    home(
        State(state(db)),
        AuthUser { id: "u1".into() },
        crate::htmx::Flash::default(),
        Query(HomeQuery {
            q: Some(query.into()),
            ..HomeQuery::default()
        }),
    )
    .await
    .unwrap()
    .0
}

#[tokio::test]
async fn without_an_embedding_service_search_falls_back_to_word_matches() {
    let db = database().await;
    sqlx::query(
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('b1','r','step',0,'Simmer the sauce for an hour.')",
    )
    .execute(&db)
    .await
    .unwrap();
    let page = search_page(db.clone(), "  supper ").await;
    assert!(!page.contains("class=\"notice\""));
    assert!(page.contains("Results for “supper”"));
    assert!(page.contains("href=\"/recipes/r\""));
    assert!(!page.contains("href=\"/recipes/other\""));
    assert!(page.contains("value=\"supper\""));

    // Step text is searched too; a word nowhere in the recipe finds nothing.
    assert!(
        search_page(db.clone(), "simmering sauces")
            .await
            .contains("href=\"/recipes/r\"")
    );
    assert!(
        search_page(db, "chocolate")
            .await
            .contains("No dishes match.")
    );
}

#[tokio::test]
async fn choosing_word_search_searches_by_word() {
    let db = database().await;
    sqlx::query(
        "INSERT INTO app_settings(user_id,key,value,updated_at) VALUES('u1','search_engine','bm25',?)",
    )
    .bind(stamp())
    .execute(&db)
    .await
    .unwrap();
    let page = search_page(db, "quick").await;
    assert!(!page.contains("class=\"notice\""));
    assert!(page.contains("href=\"/recipes/r\""));
}

#[test]
fn tokens_lowercase_drop_stopwords_and_fold_plurals() {
    assert_eq!(
        tokens("Tomatoes, Berries and the Glass of Dishes; hummus/couscous 2 eggs"),
        [
            "tomato", "berri", "glass", "dish", "hummus", "couscous", "2", "egg"
        ]
    );
}

#[test]
fn singular_and_plural_meet_on_one_word() {
    for (one, many) in [
        ("cookie", "cookies"),
        ("quiche", "quiches"),
        ("brownie", "brownies"),
        ("smoothie", "smoothies"),
        ("berry", "berries"),
        ("tomato", "tomatoes"),
        ("dish", "dishes"),
        ("glass", "glasses"),
        ("box", "boxes"),
        ("pie", "pies"),
    ] {
        assert_eq!(tokens(one), tokens(many), "{one} / {many}");
    }
    let docs = documents(&[
        ("cookies", "Chocolate chip cookies", "", &[]),
        ("brownies", "Fudgy brownies", "", &[]),
    ]);
    assert_eq!(keyword_rank("cookie", &docs), ["cookies"]);
    assert_eq!(keyword_rank("brownie", &docs), ["brownies"]);
}

#[test]
fn the_title_bonus_needs_whole_words() {
    let entries = [
        entry("pierogi", "Pierogi", 0.90),
        entry("pie", "Apple pie", 0.85),
    ];
    assert_eq!(rank("pie", &QUERY_VECTOR, &entries), ["pie", "pierogi"]);
    let entries = [
        entry("eggplant", "Eggplant curry", 0.90),
        entry("eggs", "Scrambled eggs", 0.85),
    ];
    assert_eq!(rank("egg", &QUERY_VECTOR, &entries), ["eggs", "eggplant"]);
}

fn documents(recipes: &[(&str, &str, &str, &[Block])]) -> Vec<(String, Vec<String>)> {
    recipes
        .iter()
        .map(|(id, title, description, blocks)| {
            (id.to_string(), keyword_terms(title, description, blocks))
        })
        .collect()
}

#[test]
fn keyword_rank_finds_words_anywhere_and_ranks_title_matches_first() {
    let steps = [block(
        "step",
        0,
        "Finish with a pinch of sumac.",
        "",
        "",
        false,
    )];
    let ingredients = [block("ingredient", 0, "sumac", "1", "tsp", false)];
    let docs = documents(&[
        ("salad", "Fattoush", "A bread salad.", &steps),
        ("rub", "Sumac rub", "", &[]),
        ("dip", "Dip", "", &ingredients),
        ("cake", "Cake", "A sponge.", &[]),
    ]);
    assert_eq!(keyword_rank("Sumac", &docs), ["rub", "dip", "salad"]);
    assert!(keyword_rank("the and of", &docs).is_empty());
    assert!(keyword_rank("saffron", &docs).is_empty());
    assert!(keyword_rank("sumac", &[]).is_empty());
}

#[test]
fn keyword_rank_prefers_recipes_matching_more_and_rarer_words() {
    let docs = documents(&[
        ("both", "Chicken and rice", "", &[]),
        ("chicken", "Roast chicken", "", &[]),
        ("rice", "Rice pudding", "", &[]),
        ("soup", "Soup", "Chicken stock.", &[]),
    ]);
    let ranked = keyword_rank("chicken rice", &docs);
    assert_eq!(ranked.first().map(String::as_str), Some("both"));
    assert!(ranked.contains(&"rice".to_owned()));
}

#[test]
fn keyword_rank_returns_at_most_the_result_cap() {
    let docs: Vec<(String, Vec<String>)> = (0..40)
        .map(|index| (format!("r{index:02}"), tokens("Lentil soup")))
        .collect();
    assert_eq!(keyword_rank("lentils", &docs).len(), MAX_RESULTS);
}

#[test]
fn web_addresses_are_left_out_of_what_is_indexed() {
    assert_eq!(
        squash(
            "Times retained. ([pbs.org](https://www.pbs.org/food/stories/spanish-rice-recipe?utm_source=openai))\nNext line"
        ),
        "Times retained. pbs.org Next line"
    );
    assert_eq!(squash("see https://example.com/thai-curry now"), "see now");
    assert_eq!(squash("  plain   words "), "plain words");
    let document = recipe_document(
        "Tomato rice",
        "Leftover rice ([pbs.org](https://www.pbs.org/spanish-rice))",
        &[],
    );
    assert!(!document.contains("spanish"));
    let terms = keyword_terms("Tomato rice", "From https://example.com/spanish-rice", &[]);
    assert!(!terms.iter().any(|term| term == "spanish"));
}

/// How long the fake sidecar takes to answer a text that says "slow".
pub(super) const SLOW: Duration = Duration::from_millis(800);

/// A stand-in for the embedding sidecar on a local port: `vector` gives
/// each text input its embedding, and every text it was asked about is kept.
pub(super) struct FakeSidecar {
    pub(super) url: String,
    pub(super) texts: Arc<Mutex<Vec<String>>>,
}

pub(super) async fn fake_sidecar(vector: fn(&str) -> Vec<f32>) -> FakeSidecar {
    let texts: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = texts.clone();
    let app = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                let inputs: Vec<String> = body["input"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|input| input.as_str().unwrap_or_default().to_owned())
                    .collect();
                seen.lock().extend(inputs.iter().cloned());
                if inputs.iter().any(|text| text.contains("slow")) {
                    tokio::time::sleep(SLOW).await;
                }
                let data: Vec<Value> = inputs
                    .iter()
                    .enumerate()
                    .map(|(index, text)| json!({"index": index, "embedding": vector(text)}))
                    .collect();
                axum::Json(json!({ "data": data }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeSidecar { url, texts }
}

/// The test state, with the embedder pointed at `url`.
pub(super) fn sidecar_state(db: SqlitePool, url: &str) -> Arc<AppState> {
    Arc::new(AppState {
        embedder: Some(Embedder::new(url)),
        invites: Default::default(),
        ..(*state(db)).clone()
    })
}

/// A unit vector of the sidecar's size, the same for the same text and
/// `model`, and far from any other text's or model's.
fn hashed(text: &str, model: u64) -> Vec<f32> {
    let mut hasher = DefaultHasher::new();
    (text, model).hash(&mut hasher);
    let mut seed = hasher.finish() | 1;
    let mut vector: Vec<f32> = (0..DIMENSIONS)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 2001) as f32 / 1000.0 - 1.0
        })
        .collect();
    crate::embeddings::normalize(&mut vector);
    vector
}

fn model_a(text: &str) -> Vec<f32> {
    hashed(text, 1)
}

fn model_b(text: &str) -> Vec<f32> {
    hashed(text, 2)
}

#[tokio::test]
async fn a_request_gives_up_at_its_own_timeout() {
    let sidecar = fake_sidecar(model_a).await;
    let embedder = Embedder::new(&sidecar.url).with_timeout(Duration::from_millis(100));
    let started = Instant::now();
    assert!(embedder.embed(&[Input::Text("slow".into())]).await.is_err());
    assert!(started.elapsed() < SLOW);
    assert!(embedder.embed(&[Input::Text("quick".into())]).await.is_ok());
}

#[tokio::test]
async fn vectors_of_the_wrong_size_are_reported_as_such() {
    fn short(_: &str) -> Vec<f32> {
        vec![1.0; 512]
    }
    let sidecar = fake_sidecar(short).await;
    let error = Embedder::new(&sidecar.url)
        .embed(&[Input::Text("soup".into())])
        .await
        .unwrap_err();
    assert!(error.contains("512-dimensional"), "{error}");
}

#[tokio::test]
async fn while_another_pass_indexes_a_library_a_search_only_counts_what_is_left() {
    let db = database().await;
    for id in ["held-1", "held-2"] {
        sqlx::query(
            "INSERT INTO recipes(id,title,description,user_id,created_at,updated_at) VALUES(?,'Stew','','held-user',?,?)",
        )
        .bind(id)
        .bind(stamp())
        .bind(stamp())
        .execute(&db)
        .await
        .unwrap();
    }
    let sidecar = fake_sidecar(model_a).await;
    let state = sidecar_state(db, &sidecar.url);
    let embedder = state.embedder.clone().unwrap();

    let slot = IndexingSlot::claim("held-user").unwrap();
    assert_eq!(refresh_user(&state, &embedder, "held-user").await, Ok(2));
    assert!(sidecar.texts.lock().is_empty());
    drop(slot);

    assert_eq!(refresh_user(&state, &embedder, "held-user").await, Ok(0));
    assert_eq!(sidecar.texts.lock().len(), 2);
    assert_eq!(refresh_user(&state, &embedder, "held-user").await, Ok(0));
    assert_eq!(sidecar.texts.lock().len(), 2);
}

#[tokio::test]
async fn a_new_embedding_model_rebuilds_every_vector() {
    let db = database().await;
    let old_model = fake_sidecar(model_a).await;
    let old_model = Embedder::new(&old_model.url);
    assert_eq!(reindex(&db, &old_model).await, Ok(2));
    assert_eq!(reindex(&db, &old_model).await, Ok(0));

    let new_model = fake_sidecar(model_b).await;
    assert_eq!(reindex(&db, &Embedder::new(&new_model.url)).await, Ok(2));
    let stored = recipe_vector(&db, "r").await.unwrap().unwrap();
    let document = recipe_document("Test", "A quick supper", &[]);
    assert!(crate::embeddings::dot(&stored, &model_b(&document)) > 0.999);
}

#[tokio::test]
async fn while_the_index_is_mostly_unbuilt_a_search_uses_word_matches() {
    let db = database().await;
    for (id, title) in [("wait-1", "Lamb stew"), ("wait-2", "Pea soup")] {
        sqlx::query(
            "INSERT INTO recipes(id,title,description,user_id,created_at,updated_at) VALUES(?,?,'','wait-user',?,?)",
        )
        .bind(id)
        .bind(title)
        .bind(stamp())
        .bind(stamp())
        .execute(&db)
        .await
        .unwrap();
    }
    let sidecar = fake_sidecar(model_a).await;
    let state = sidecar_state(db, &sidecar.url);

    // Another pass (the startup backfill, say) holds the user's index, so
    // nothing gets embedded and the ranking would see no recipes at all.
    let slot = IndexingSlot::claim("wait-user").unwrap();
    let outcome = search(&state, "wait-user", "stew").await.unwrap();
    drop(slot);
    assert_eq!(outcome.ids, ["wait-1"]);
    assert!(outcome.notice.contains("2 recipes"), "{}", outcome.notice);
    assert!(sidecar.texts.lock().is_empty());
}
