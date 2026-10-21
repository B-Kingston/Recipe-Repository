use crate::auth::AuthUser;
use crate::recipes::{home, store_embedding};
use crate::search::{
    Indexed, MAX_RESULTS, NOT_CONFIGURED, normalize_query, rank, recipe_document, stale_documents,
};
use crate::{AppState, Block, HomeQuery, stamp};
use axum::extract::{Query, State};
use parking_lot::Mutex;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, collections::HashSet, sync::Arc};

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

fn state(db: SqlitePool) -> Arc<AppState> {
    Arc::new(AppState {
        db,
        model: String::new(),
        pi_worker_path: String::new(),
        auth_script_path: String::new(),
        claude_auth_script_path: String::new(),
        search_grounding: false,
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
        claude_flows: Arc::new(Mutex::new(HashMap::new())),
        model_catalogue: Arc::new(Mutex::new(HashMap::new())),
        media_debug_runs: Arc::new(Mutex::new(HashMap::new())),
        thumbnail_jobs: Arc::new(Mutex::new(HashSet::new())),
        moment_jobs: Arc::new(Mutex::new(HashSet::new())),
        embedder: None,
    })
}

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
fn the_document_body_is_capped_at_6000_characters() {
    let long = "é".repeat(7000);
    let document = recipe_document("Long", &long, &[]);
    assert_eq!(
        document,
        format!("title: Long | text: {}", "é".repeat(6000))
    );
}

#[tokio::test]
async fn recipes_are_stale_until_embedded_and_again_after_an_edit() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('s0','r','step',0,'Boil water')")
        .execute(&db)
        .await
        .unwrap();

    let stale = stale_documents(&db, Some("u1")).await.unwrap();
    assert_eq!(stale.len(), 1);
    let (id, document) = &stale[0];
    assert_eq!(id, "r");
    assert!(document.contains("Boil water"));

    store_embedding(&db, id, document, &[0.5, 0.5])
        .await
        .unwrap();
    assert!(stale_documents(&db, Some("u1")).await.unwrap().is_empty());

    sqlx::query("UPDATE recipe_blocks SET text='Boil milk' WHERE id='s0'")
        .execute(&db)
        .await
        .unwrap();
    let stale = stale_documents(&db, Some("u1")).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert!(stale[0].1.contains("Boil milk"));
    store_embedding(&db, &stale[0].0, &stale[0].1, &[0.5, 0.5])
        .await
        .unwrap();
    assert!(stale_documents(&db, Some("u1")).await.unwrap().is_empty());

    sqlx::query("UPDATE recipes SET title='Renamed' WHERE id='r'")
        .execute(&db)
        .await
        .unwrap();
    let stale = stale_documents(&db, Some("u1")).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert!(stale[0].1.starts_with("title: Renamed"));
}

#[tokio::test]
async fn staleness_is_scoped_to_the_requested_user() {
    let db = database().await;
    let ids = |stale: Vec<(String, String)>| {
        let mut ids: Vec<String> = stale.into_iter().map(|(id, _)| id).collect();
        ids.sort();
        ids
    };
    assert_eq!(
        ids(stale_documents(&db, Some("u2")).await.unwrap()),
        ["other"]
    );
    assert_eq!(
        ids(stale_documents(&db, None).await.unwrap()),
        ["other", "r"]
    );
}

#[tokio::test]
async fn storing_a_vector_for_a_deleted_recipe_is_skipped() {
    let db = database().await;
    store_embedding(&db, "gone", "document", &[1.0])
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recipe_embeddings")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn searching_without_an_embedding_service_explains_and_lists_every_recipe() {
    let db = database().await;
    let page = home(
        State(state(db)),
        AuthUser { id: "u1".into() },
        Query(HomeQuery {
            q: Some("  supper ".into()),
        }),
    )
    .await
    .unwrap()
    .0;
    assert!(page.contains(NOT_CONFIGURED));
    assert!(page.contains("Recipe box"));
    assert!(page.contains("filed A to Z"));
    assert!(page.contains("href=\"/recipes/r\""));
    assert!(!page.contains("href=\"/recipes/other\""));
    assert!(page.contains("value=\"supper\""));
}
