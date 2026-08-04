use crate::recipes::{delete_block, infer_step_ingredients, move_block};
use crate::{AppState, Block, stamp};
use axum::extract::{Path, State};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

fn ingredient(text: &str, quantity: &str, unit: &str) -> Block {
    Block {
        id: String::new(),
        section: "ingredient".into(),
        position: 0,
        text: text.into(),
        quantity: quantity.into(),
        unit: unit.into(),
        optional: 0,
    }
}

#[test]
fn old_steps_infer_explicitly_measured_ingredients() {
    assert_eq!(
        infer_step_ingredients(
            "Heat 2 tbsp olive oil until shimmering.",
            &[
                ingredient("olive oil", "2", "tbsp"),
                ingredient("salt", "1", "tsp")
            ]
        ),
        vec!["2 tbsp olive oil"]
    );
}

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
    sqlx::query("INSERT INTO recipes(id,title,created_at,updated_at) VALUES('r','Test',?,?)")
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
        search_grounding: false,
        codex_flows: Arc::new(Mutex::new(HashMap::new())),
    })
}

#[tokio::test]
async fn moving_a_block_swaps_positions_without_changing_ids() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('first','r','step',0,'First'),('second','r','step',1,'Second')").execute(&db).await.unwrap();
    move_block(
        State(state(db.clone())),
        Path(("r".into(), "first".into(), "down".into())),
    )
    .await
    .unwrap();
    let ordered: Vec<(String, i64)> =
        sqlx::query_as("SELECT id,position FROM recipe_blocks ORDER BY position")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(ordered, vec![("second".into(), 0), ("first".into(), 1)]);
}

#[tokio::test]
async fn deleting_a_block_compacts_the_remaining_positions() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('a','r','ingredient',0,'A'),('b','r','ingredient',1,'B'),('c','r','ingredient',2,'C')").execute(&db).await.unwrap();
    delete_block(State(state(db.clone())), Path(("r".into(), "b".into())))
        .await
        .unwrap();
    let ordered: Vec<(String, i64)> =
        sqlx::query_as("SELECT id,position FROM recipe_blocks ORDER BY position")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(ordered, vec![("a".into(), 0), ("c".into(), 1)]);
}
