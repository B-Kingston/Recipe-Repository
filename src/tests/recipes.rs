use crate::auth::AuthUser;
use crate::recipes::{
    LibraryCard, TICKET_COLUMNS_AFTER, delete_block, delete_version, file_a_to_z, find_version,
    stamp_added,
    home, move_block, recipe_versions, record_version, update_block, update_recipe, view_version,
};
use crate::{AppError, AppState, BlockForm, HomeQuery, RecipeForm, stamp};
use axum::Form;
use axum::extract::{Path, Query, State};
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
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at) VALUES('r','Test','u1',?,?)",
    )
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

#[tokio::test]
async fn moving_a_block_swaps_positions_without_changing_ids() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('first','r','step',0,'First'),('second','r','step',1,'Second')").execute(&db).await.unwrap();
    move_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "first".into(), "down".into())),
        Ok(Form(BlockForm::default())),
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
async fn moving_a_block_without_a_neighbour_records_no_version() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('only','r','step',0,'Only')").execute(&db).await.unwrap();
    move_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "only".into(), "down".into())),
        Ok(Form(BlockForm::default())),
    )
    .await
    .unwrap();
    assert!(recipe_versions(&db, "u1", "r").await.unwrap().is_empty());
}

#[tokio::test]
async fn moving_a_block_saves_the_text_submitted_with_the_move_buttons() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('first','r','step',0,'First'),('second','r','step',1,'Second')").execute(&db).await.unwrap();
    move_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "first".into(), "down".into())),
        Ok(Form(BlockForm {
            text: "First, edited".into(),
            step_ingredients: Some("2 eggs".into()),
            ..BlockForm::default()
        })),
    )
    .await
    .unwrap();
    let ordered: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT id,position,text FROM recipe_blocks ORDER BY position")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        ordered,
        vec![
            ("second".into(), 0, "Second".into()),
            ("first".into(), 1, "First, edited".into()),
        ]
    );
    let ingredients: Vec<String> = sqlx::query_scalar(
        "SELECT text FROM recipe_step_ingredients WHERE step_id='first' ORDER BY position",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(ingredients, vec!["2 eggs".to_string()]);
    let labels: Vec<String> = recipe_versions(&db, "u1", "r")
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.label)
        .collect();
    assert_eq!(
        labels,
        vec!["Step reordered", "Step edited", "Original recipe"]
    );
}

#[tokio::test]
async fn moving_the_top_block_still_saves_its_submitted_text() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('first','r','step',0,'First')").execute(&db).await.unwrap();
    move_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "first".into(), "up".into())),
        Ok(Form(BlockForm {
            text: "First, edited".into(),
            ..BlockForm::default()
        })),
    )
    .await
    .unwrap();
    let text: String = sqlx::query_scalar("SELECT text FROM recipe_blocks WHERE id='first'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(text, "First, edited");
    let labels: Vec<String> = recipe_versions(&db, "u1", "r")
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.label)
        .collect();
    assert_eq!(labels, vec!["Step edited", "Original recipe"]);
}

#[tokio::test]
async fn moving_a_block_with_an_unchanged_form_records_only_the_move() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('first','r','step',0,'First'),('second','r','step',1,'Second')").execute(&db).await.unwrap();
    move_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "first".into(), "down".into())),
        Ok(Form(BlockForm {
            text: "First".into(),
            ..BlockForm::default()
        })),
    )
    .await
    .unwrap();
    let labels: Vec<String> = recipe_versions(&db, "u1", "r")
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.label)
        .collect();
    assert_eq!(labels, vec!["Step reordered", "Original recipe"]);
}

#[test]
fn version_dates_read_as_short_utc_dates() {
    assert_eq!(
        crate::display_timestamp("2026-10-15T22:11:05+00:00"),
        "15 Oct 2026, 22:11"
    );
    assert_eq!(
        crate::display_timestamp("2026-10-16T08:05:00+10:00"),
        "15 Oct 2026, 22:05"
    );
    assert_eq!(crate::display_timestamp("not a date"), "not a date");
}

#[tokio::test]
async fn deleting_a_block_compacts_the_remaining_positions() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('a','r','ingredient',0,'A'),('b','r','ingredient',1,'B'),('c','r','ingredient',2,'C')").execute(&db).await.unwrap();
    delete_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "b".into())),
    )
    .await
    .unwrap();
    let ordered: Vec<(String, i64)> =
        sqlx::query_as("SELECT id,position FROM recipe_blocks ORDER BY position")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(ordered, vec![("a".into(), 0), ("c".into(), 1)]);
}

#[tokio::test]
async fn first_edit_keeps_the_original_recipe_as_version_one() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit) VALUES('i1','r','ingredient',0,'Flour','1','cup')")
        .execute(&db).await.unwrap();
    update_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "i1".into())),
        Form(BlockForm {
            section: None,
            text: "Flour".into(),
            quantity: Some("2".into()),
            unit: Some("cup".into()),
            optional: None,
            step_ingredients: None,
        }),
    )
    .await
    .unwrap();
    let original = find_version(&db, "u1", "r", 1).await.unwrap();
    assert_eq!(original.label, "Original recipe");
    assert_eq!(original.snapshot.ingredients[0].quantity, "1");
    let edited = find_version(&db, "u1", "r", 2).await.unwrap();
    assert_eq!(edited.label, "Ingredient edited");
    assert_eq!(edited.snapshot.ingredients[0].quantity, "2");
}

#[tokio::test]
async fn revisions_store_version_snapshots_and_render_read_only() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('b1','r','step',0,'Boil water')")
        .execute(&db).await.unwrap();
    update_recipe(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path("r".into()),
        Form(RecipeForm {
            title: "Edited".into(),
            description: String::new(),
            servings: "4".into(),
            prep_minutes: String::new(),
            cook_minutes: String::new(),
        }),
    )
    .await
    .unwrap();
    let original = find_version(&db, "u1", "r", 1).await.unwrap();
    assert_eq!(original.label, "Original recipe");
    assert_eq!(original.snapshot.title, "Test");
    assert_eq!(original.snapshot.steps[0].text, "Boil water");
    let edited = find_version(&db, "u1", "r", 2).await.unwrap();
    assert_eq!(edited.label, "Details edited");
    assert_eq!(edited.snapshot.title, "Edited");
    // Missing version is not found; wrong owner is not found.
    assert!(matches!(
        find_version(&db, "u1", "r", 3).await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        find_version(&db, "other", "r", 1).await,
        Err(AppError::NotFound)
    ));
    let page = view_version(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), 1)),
    )
    .await
    .unwrap();
    assert!(page.0.contains("Boil water"));
    delete_version(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), 2)),
    )
    .await
    .unwrap();
    assert!(matches!(
        find_version(&db, "u1", "r", 2).await,
        Err(AppError::NotFound)
    ));
    // Deleted numbers are never reused, even when the latest one is gone.
    let mut tx = db.begin().await.unwrap();
    record_version(&mut tx, "u1", "r", "Manual edit")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        find_version(&db, "u1", "r", 3).await.unwrap().label,
        "Manual edit"
    );
}

fn card(title: &str) -> LibraryCard {
    LibraryCard {
        id: title.into(),
        title: title.into(),
        description: String::new(),
        servings: None,
        prep_minutes: None,
        cook_minutes: None,
        has_thumbnail: false,
        created_at: String::new(),
        ingredients: Vec::new(),
        added: String::new(),
    }
}

#[test]
fn ticket_stamp_adds_the_time_only_when_dates_clash() {
    let mut cards = vec![card("Dal"), card("Pho"), card("Ragu"), card("Old")];
    cards[0].created_at = "2026-10-15T09:05:00+00:00".into();
    cards[1].created_at = "2026-10-15T22:11:00+00:00".into();
    cards[2].created_at = "2026-10-16T08:00:00+00:00".into();
    cards[3].created_at = "not a time".into();
    stamp_added(&mut cards);
    let added: Vec<&str> = cards.iter().map(|card| card.added.as_str()).collect();
    assert_eq!(
        added,
        ["15 Oct 2026, 09:05", "15 Oct 2026, 22:11", "16 Oct 2026", ""]
    );
}

#[test]
fn library_is_filed_a_to_z() {
    let mut cards = vec![
        card("chicken adobo"),
        card("Banana bread"),
        card("10-minute soup"),
        card("Charred corn"),
        card("Éclairs"),
    ];
    file_a_to_z(&mut cards);
    let titles: Vec<&str> = cards.iter().map(|card| card.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "10-minute soup",
            "Éclairs",
            "Banana bread",
            "Charred corn",
            "chicken adobo"
        ]
    );
}

#[test]
fn library_card_shows_servings_and_total_time() {
    let mut dal = card("Dal");
    assert_eq!(dal.serves(), "");
    assert_eq!(dal.total_time(), "");
    dal.servings = Some(4);
    dal.prep_minutes = Some(15);
    dal.cook_minutes = Some(20);
    assert_eq!(dal.serves(), "Serves 4");
    assert_eq!(dal.total_time(), "35 min");
    dal.cook_minutes = Some(55);
    assert_eq!(dal.total_time(), "1 h 10 min");
    dal.prep_minutes = None;
    dal.cook_minutes = Some(120);
    assert_eq!(dal.total_time(), "2 h");
}

#[tokio::test]
async fn library_tickets_print_ingredients_and_pack_long_lists() {
    let db = database().await;
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,title,servings,prep_minutes,user_id,created_at,updated_at)
         VALUES('big','Big stew',6,40,'u1',?,?)",
    )
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit,optional)
         VALUES('i0','r','ingredient',0,'Silken tofu','300','g',0),
               ('i1','r','ingredient',1,'Chilli flakes','1/2','tsp',1),
               ('s0','r','step',0,'Blend everything','','',0)",
    )
    .execute(&db)
    .await
    .unwrap();
    for position in 0..=TICKET_COLUMNS_AFTER {
        sqlx::query(
            "INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES(?,'big','ingredient',?,?)",
        )
        .bind(format!("b{position}"))
        .bind(position as i64)
        .bind(format!("Item {position}"))
        .execute(&db)
        .await
        .unwrap();
    }
    let page = home(
        State(state(db)),
        AuthUser { id: "u1".into() },
        Query(HomeQuery::default()),
    )
    .await
    .unwrap()
    .0;
    // One rail hangs the whole library; later rows are pinned on in CSS.
    assert_eq!(page.matches(r#"<div class="rail" aria-hidden="true">"#).count(), 1);
    assert!(page.contains("Ready in 40 min"));
    assert!(page.contains("Serves 6"));
    assert!(!page.contains("Filed under") && !page.contains("ticket-box"));
    assert!(page.contains(
        r#"<b class="ticket-qty">300 g</b> <span class="ticket-item">Silken tofu</span>"#
    ));
    assert!(page.contains("Chilli flakes <em>(optional)</em>"));
    assert!(!page.contains("Blend everything"));
    assert_eq!(page.matches(r#"class="ticket-items is-packed""#).count(), 1);
    assert_eq!(page.matches(r#"class="ticket-items""#).count(), 1);
}
