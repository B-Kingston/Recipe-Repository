use crate::auth::{AuthUser, setup_create};
use crate::recipes::{
    AcceptOutcome, AnswerProposal, LibraryCard, ProposalRow, TICKET_COLUMNS_AFTER,
    accept_chat_proposal, add_block, clean_plate_name, delete_block, delete_recipe, delete_version,
    fail_chat_answer, file_a_to_z, find_version, finish_chat_answer, home, move_block,
    plate_initial, plate_name_scale, recipe_page, recipe_snapshot, recipe_versions, record_version,
    restore_version, stamp_added, start_chat_turn, update_block, update_recipe, view_version,
};
use crate::{
    AppError, AppState, BlockForm, ChatProposal, HomeQuery, RecipeForm, RecipeQuery, SetupForm,
    routes, stamp,
};
use axum::Form;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Request, StatusCode, Uri, header};
use sqlx::SqlitePool;
use std::sync::Arc;
use tower::ServiceExt;

async fn database() -> SqlitePool {
    let db = super::memory_db().await;
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

use super::test_state as state;

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
async fn pruning_drafts_removes_only_expired_ones_with_their_candidates() {
    let db = database().await;
    for (id, expires_at) in [
        ("expired", "2026-01-01T00:00:00+00:00"),
        ("live", "2026-01-03T00:00:00+00:00"),
    ] {
        sqlx::query(
            "INSERT INTO ai_drafts(id,recipe_json,sources_json,created_at,expires_at)
             VALUES(?,'{}','[]','2026-01-01T00:00:00+00:00',?)",
        )
        .bind(id)
        .bind(expires_at)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO draft_thumbnails(draft_id,idx,seconds,jpeg) VALUES(?,0,3,x'ff')")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
    }
    let removed = crate::recipes::delete_expired_drafts(&db, "2026-01-02T00:00:00+00:00")
        .await
        .unwrap();
    assert_eq!(removed, 1);
    let drafts: Vec<String> = sqlx::query_scalar("SELECT id FROM ai_drafts")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(drafts, ["live"]);
    let candidates: Vec<String> = sqlx::query_scalar("SELECT draft_id FROM draft_thumbnails")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(candidates, ["live"]);
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
            plate_name: String::new(),
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
        crate::htmx::Flash::default(),
    )
    .await
    .unwrap();
    assert!(page.0.contains("Boil water"));
    assert!(page.0.contains("Delete version"));
    // The newest version is the recipe as it is now: it stays, and its page
    // offers no Delete.
    let newest = view_version(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), 2)),
        crate::htmx::Flash::default(),
    )
    .await
    .unwrap();
    assert!(!newest.0.contains("Delete version"));
    assert!(matches!(
        delete_version(
            State(state(db.clone())),
            AuthUser { id: "u1".into() },
            Path(("r".into(), 2)),
        )
        .await,
        Err(AppError::BadRequest(_))
    ));
    let mut tx = db.begin().await.unwrap();
    record_version(&mut tx, "u1", "r", "Manual edit")
        .await
        .unwrap();
    tx.commit().await.unwrap();
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
    sqlx::query("DELETE FROM recipe_versions WHERE recipe_id='r' AND version=3")
        .execute(&db)
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    record_version(&mut tx, "u1", "r", "Another edit")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        find_version(&db, "u1", "r", 4).await.unwrap().label,
        "Another edit"
    );
}

#[tokio::test]
async fn restoring_a_version_makes_it_current_and_keeps_the_history() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit) VALUES('i1','r','ingredient',0,'Flour','1','cup')")
        .execute(&db).await.unwrap();
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('s1','r','step',0,'Mix')")
        .execute(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO recipe_step_ingredients(id,step_id,position,text) VALUES('u1','s1',0,'Flour')",
    )
    .execute(&db)
    .await
    .unwrap();
    update_recipe(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path("r".into()),
        Form(RecipeForm {
            title: "Edited".into(),
            plate_name: String::new(),
            description: String::new(),
            servings: "4".into(),
            prep_minutes: String::new(),
            cook_minutes: String::new(),
        }),
    )
    .await
    .unwrap();
    delete_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "s1".into())),
    )
    .await
    .unwrap();
    // Another account cannot restore this recipe's versions.
    assert!(matches!(
        restore_version(
            State(state(db.clone())),
            AuthUser { id: "other".into() },
            Path(("r".into(), 1)),
        )
        .await,
        Err(AppError::NotFound)
    ));
    let original = find_version(&db, "u1", "r", 1).await.unwrap().snapshot;
    restore_version(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), 1)),
    )
    .await
    .unwrap();
    let restored = find_version(&db, "u1", "r", 4).await.unwrap();
    assert_eq!(restored.label, "Restored version 1");
    assert_eq!(restored.snapshot, original);
    assert_eq!(restored.snapshot.title, "Test");
    assert_eq!(
        restored.snapshot.steps[0].ingredients,
        vec!["Flour".to_string()]
    );
    // The replaced recipe is still in the history.
    assert_eq!(
        find_version(&db, "u1", "r", 3)
            .await
            .unwrap()
            .snapshot
            .steps
            .len(),
        0
    );
    // Restoring what is already current records nothing.
    restore_version(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), 1)),
    )
    .await
    .unwrap();
    assert_eq!(recipe_versions(&db, "u1", "r").await.unwrap().len(), 4);
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
        owner: String::new(),
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
        [
            "15 Oct 2026, 09:05",
            "15 Oct 2026, 22:11",
            "16 Oct 2026",
            ""
        ]
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

#[test]
fn recipe_plates_without_a_photo_show_the_first_letter_or_digit() {
    assert_eq!(clean_plate_name("  Pad   thai "), "Pad thai");
    assert_eq!(clean_plate_name("Soft wholemeal roti"), "");
    assert_eq!(clean_plate_name("Supercalifragilistic"), "");
    assert_eq!(plate_name_scale("S"), 1.0);
    assert!(plate_name_scale("Roti") > plate_name_scale("Banana bread"));
    assert!(plate_name_scale("Roti") <= 0.62);
    assert_eq!(plate_initial("chicken adobo"), "C");
    assert_eq!(plate_initial("  \u{201c}éclairs\u{201d}"), "É");
    assert_eq!(plate_initial("10-minute soup"), "1");
    assert_eq!(plate_initial("--"), "");
}

#[tokio::test]
async fn library_tickets_are_pinned_print_ingredients_and_pack_long_lists() {
    let db = database().await;
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,title,servings,prep_minutes,user_id,created_at,updated_at,thumbnail_jpeg)
         VALUES('big','Big stew',6,40,'u1',?,?,X'FFD8FFD9')",
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
        crate::htmx::Flash::default(),
        Query(HomeQuery::default()),
    )
    .await
    .unwrap()
    .0;
    // Every ticket hangs on its own pin; there is no rail.
    assert_eq!(page.matches(r#"<article class="ticket">"#).count(), 2);
    assert!(!page.contains("class=\"rail\""));
    assert!(page.contains("Ready in 40 min"));
    assert!(page.contains("Serves 6"));
    assert!(page.contains(
        r#"<b class="ticket-qty">300 g</b> <span class="ticket-item">Silken tofu</span>"#
    ));
    assert!(page.contains("Chilli flakes <em>(optional)</em>"));
    assert!(!page.contains("Blend everything"));
    assert_eq!(page.matches(r#"class="ticket-items is-packed""#).count(), 1);
    assert_eq!(page.matches(r#"class="ticket-items""#).count(), 1);
    // Only the stew has a photo, behind its thumbnail toggle.
    assert_eq!(page.matches(r#"class="ticket-thumb-toggle""#).count(), 1);
    assert_eq!(
        page.matches(r#"<img class="ticket-photo" src="/recipes/big/thumbnail""#)
            .count(),
        1
    );
}

#[tokio::test]
async fn recipe_plate_rim_carries_a_short_title_and_the_timings() {
    let db = database().await;
    let now = stamp();
    sqlx::query(
        "UPDATE recipes SET title='Dal makhani',servings=6,prep_minutes=20,cook_minutes=80 WHERE id='r'",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at)
         VALUES('long','Brown butter gnocchi with crispy sage','u1',?,?)",
    )
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    let s = state(db);
    let page = |id: &'static str| {
        let s = s.clone();
        async move {
            recipe_page(
                State(s),
                AuthUser { id: "u1".into() },
                Path(id.into()),
                Query(RecipeQuery {
                    edit: None,
                    edit_meta: None,
                    view: None,
                    step: None,
                    serves: None,
                }),
                crate::htmx::Flash::default(),
            )
            .await
            .unwrap()
            .0
        }
    };
    let dal = page("r").await;
    assert!(dal.contains(r##"startOffset="50%" text-anchor="middle">DAL MAKHANI</textPath>"##));
    assert!(dal.contains("SERVES 6  \u{2605}  1 H 40 MIN</textPath>"));
    assert!(dal.contains(
        r#"<span class="plate-initial" aria-hidden="true" style="font-size: 1.00em">D</span>"#
    ));
    assert!(!dal.contains("plate-photo"));
    // Too long for the rim: the title stays in the heading only, and a
    // recipe without timings leaves the rim plain.
    let long = page("long").await;
    assert!(!long.contains("BROWN BUTTER"));
    assert!(!long.contains("plate-rim-text"));
}

#[tokio::test]
async fn recipe_page_scales_amounts_to_the_chosen_servings() {
    let db = database().await;
    sqlx::query("UPDATE recipes SET servings=4 WHERE id='r'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit)VALUES
         ('flour','r','ingredient',0,'plain flour','200','g'),
         ('salt','r','ingredient',1,'salt to taste','',''),
         ('mix','r','step',0,'Mix the flour.','','')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recipe_step_ingredients(id,step_id,position,text)VALUES('u','mix',0,'200 g plain flour')",
    )
    .execute(&db)
    .await
    .unwrap();
    let s = state(db);
    let page = |serves: Option<i64>| {
        let s = s.clone();
        async move {
            recipe_page(
                State(s),
                AuthUser { id: "u1".into() },
                Path("r".into()),
                Query(RecipeQuery {
                    edit: None,
                    edit_meta: None,
                    view: None,
                    step: None,
                    serves,
                }),
                crate::htmx::Flash::default(),
            )
            .await
            .unwrap()
            .0
        }
    };
    let written = page(None).await;
    assert!(written.contains(r#"<span class="ingredient-qty">200 g</span>"#));
    assert!(!written.contains("Amounts scaled"));
    let halved = page(Some(2)).await;
    assert!(halved.contains(r#"<span class="ingredient-qty">100 g</span>"#));
    assert!(halved.contains(r#"<span class="step-ingredient">100 g plain flour</span>"#));
    assert!(halved.contains("<dt>Serves</dt><dd>2</dd>"));
    assert!(halved.contains("Amounts scaled from 4 servings"));
    // The edit forms keep the amounts as written.
    assert!(halved.contains(r#"name="quantity" value="200""#));
    assert!(halved.contains(">200 g plain flour</textarea>"));
    // The other views keep the choice.
    assert!(halved.contains("step=1&#38;serves=2"));
}

/// The recipe page for `r` as u1 sees it.
async fn recipe_view(
    s: &Arc<AppState>,
    view: Option<&str>,
    step: Option<usize>,
    serves: Option<i64>,
) -> String {
    recipe_page(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path("r".into()),
        Query(RecipeQuery {
            edit: None,
            edit_meta: None,
            view: view.map(Into::into),
            step,
            serves,
        }),
        crate::htmx::Flash::default(),
    )
    .await
    .unwrap()
    .0
}

/// Every hand edit of the blocks drops the AI chart and stamps the recipe,
/// which the chat's and the drafts' staleness guards compare.
#[tokio::test]
async fn every_block_edit_drops_the_chart_and_stamps_the_recipe() {
    const OLD: &str = "2000-01-01T00:00:00+00:00";
    for edit in ["add", "edit", "move", "delete", "restore"] {
        let db = database().await;
        let s = state(db.clone());
        let user = || AuthUser { id: "u1".into() };
        sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('a','r','step',0,'First'),('b','r','step',1,'Second')").execute(&db).await.unwrap();
        if edit == "restore" {
            // Version 1 is the original; version 2 has the edited step.
            update_block(
                State(s.clone()),
                user(),
                Path(("r".into(), "a".into())),
                Form(BlockForm {
                    text: "First, edited".into(),
                    ..BlockForm::default()
                }),
            )
            .await
            .unwrap();
        }
        sqlx::query("UPDATE recipes SET chart_json='{\"version\":1}',updated_at=? WHERE id='r'")
            .bind(OLD)
            .execute(&db)
            .await
            .unwrap();
        match edit {
            "add" => add_block(
                State(s.clone()),
                user(),
                Path("r".into()),
                Form(BlockForm {
                    section: Some("step".into()),
                    text: "Third".into(),
                    ..BlockForm::default()
                }),
            )
            .await
            .unwrap(),
            "edit" => update_block(
                State(s.clone()),
                user(),
                Path(("r".into(), "a".into())),
                Form(BlockForm {
                    text: "First, edited".into(),
                    ..BlockForm::default()
                }),
            )
            .await
            .unwrap(),
            "move" => move_block(
                State(s.clone()),
                user(),
                Path(("r".into(), "a".into(), "down".into())),
                Ok(Form(BlockForm::default())),
            )
            .await
            .unwrap(),
            "delete" => delete_block(State(s.clone()), user(), Path(("r".into(), "b".into())))
                .await
                .unwrap(),
            _ => restore_version(State(s.clone()), user(), Path(("r".into(), 1)))
                .await
                .unwrap(),
        };
        let (chart, updated): (String, String) =
            sqlx::query_as("SELECT chart_json,updated_at FROM recipes WHERE id='r'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(chart, "", "{edit} drops the chart");
        assert_ne!(updated, OLD, "{edit} stamps the recipe");
    }
}

#[tokio::test]
async fn saving_an_unchanged_block_keeps_the_chart_and_records_nothing() {
    let db = database().await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit) VALUES('i1','r','ingredient',0,'Flour','1','cup')")
        .execute(&db).await.unwrap();
    sqlx::query("UPDATE recipes SET chart_json='{\"version\":1}' WHERE id='r'")
        .execute(&db)
        .await
        .unwrap();
    update_block(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "i1".into())),
        Form(BlockForm {
            text: "Flour".into(),
            quantity: Some("1".into()),
            unit: Some("cup".into()),
            ..BlockForm::default()
        }),
    )
    .await
    .unwrap();
    let chart: String = sqlx::query_scalar("SELECT chart_json FROM recipes WHERE id='r'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(chart, "{\"version\":1}");
    assert!(recipe_versions(&db, "u1", "r").await.unwrap().is_empty());
}

#[tokio::test]
async fn inferred_step_ingredients_show_but_an_untouched_form_does_not_save_them() {
    let db = database().await;
    sqlx::query(
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit)VALUES
         ('flour','r','ingredient',0,'plain flour','200','g'),
         ('mix','r','step',0,'Mix 200 g flour with water.','',''),
         ('rest','r','step',1,'Rest.','','')",
    )
    .execute(&db)
    .await
    .unwrap();
    let s = state(db.clone());
    let page = recipe_view(&s, None, None, None).await;
    assert!(page.contains(r#"<span class="step-ingredient">200 g plain flour</span>"#));
    assert!(!page.contains(">200 g plain flour</textarea>"));
    // The move arrows post the step's form as the page shows it.
    move_block(
        State(s.clone()),
        AuthUser { id: "u1".into() },
        Path(("r".into(), "mix".into(), "down".into())),
        Ok(Form(BlockForm {
            text: "Mix 200 g flour with water.".into(),
            step_ingredients: Some(String::new()),
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
    let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recipe_step_ingredients")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(links, 0);
}

fn details(title: &str, plate_name: &str) -> Form<RecipeForm> {
    Form(RecipeForm {
        title: title.into(),
        plate_name: plate_name.into(),
        description: String::new(),
        servings: String::new(),
        prep_minutes: String::new(),
        cook_minutes: String::new(),
    })
}

async fn plate_name(db: &SqlitePool) -> String {
    sqlx::query_scalar("SELECT plate_name FROM recipes WHERE id='r'")
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn versions_keep_and_restore_the_plate_name() {
    let db = database().await;
    let s = state(db.clone());
    let user = || AuthUser { id: "u1".into() };
    // A plate name alone is an edit worth a version, and one that restores.
    update_recipe(
        State(s.clone()),
        user(),
        Path("r".into()),
        details("Test", "Roti"),
    )
    .await
    .unwrap();
    let plate_of = |version: i64| {
        let db = db.clone();
        async move {
            find_version(&db, "u1", "r", version)
                .await
                .unwrap()
                .snapshot
                .plate_name
        }
    };
    assert_eq!(plate_of(1).await, Some(String::new()));
    assert_eq!(plate_of(2).await, Some("Roti".into()));
    restore_version(State(s.clone()), user(), Path(("r".into(), 1)))
        .await
        .unwrap();
    assert_eq!(plate_name(&db).await, "");
    restore_version(State(s.clone()), user(), Path(("r".into(), 2)))
        .await
        .unwrap();
    assert_eq!(plate_name(&db).await, "Roti");
    // A version stored without the field keeps the plate name while the
    // title stays, and clears it under another title.
    for (title, expected) in [("Test", "Roti"), ("Older", "")] {
        sqlx::query(
            "UPDATE recipe_versions SET snapshot=json_remove(json_set(snapshot,'$.title',?),'$.plateName')
             WHERE recipe_id='r' AND version=1",
        )
        .bind(title)
        .execute(&db)
        .await
        .unwrap();
        // The live recipe differs from version 1 either way.
        sqlx::query(
            "UPDATE recipes SET title='Test',plate_name='Roti',description='Changed' WHERE id='r'",
        )
        .execute(&db)
        .await
        .unwrap();
        restore_version(State(s.clone()), user(), Path(("r".into(), 1)))
            .await
            .unwrap();
        assert_eq!(plate_name(&db).await, expected, "version titled {title}");
    }
}

/// u1's "r" with one step and the plate name "Roti", and a ready chat
/// proposal over it that serves 6 under `title`; returns the answer id.
async fn seed_title_proposal(db: &SqlitePool, title: &str) -> String {
    sqlx::query(
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('s','r','step',0,'Stir.')",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query("UPDATE recipes SET plate_name='Roti' WHERE id='r'")
        .execute(db)
        .await
        .unwrap();
    let before = {
        let mut conn = db.acquire().await.unwrap();
        recipe_snapshot(&mut conn, "u1", "r").await.unwrap()
    };
    let base: String = sqlx::query_scalar("SELECT updated_at FROM recipes WHERE id='r'")
        .fetch_one(db)
        .await
        .unwrap();
    let after = crate::RecipeProposal {
        title: title.into(),
        description: before.description.clone(),
        servings: Some(6),
        prep_minutes: before.prep_minutes,
        cook_minutes: before.cook_minutes,
        ingredients: Vec::new(),
        steps: vec![crate::ProposedStep {
            from: Some(0),
            text: "Stir.".into(),
            ingredients: Vec::new(),
        }],
    };
    assert!(after.changes(&before));
    let json = serde_json::to_string(&ChatProposal { before, after }).unwrap();
    let answer = start_chat_turn(db, "u1", "r", "Serve 6")
        .await
        .unwrap()
        .unwrap();
    finish_chat_answer(
        db,
        "u1",
        "r",
        &answer,
        "Done.",
        AnswerProposal::Ready(ProposalRow {
            label: "Serves 6",
            json: &json,
            base: &base,
        }),
    )
    .await
    .unwrap();
    answer
}

#[tokio::test]
async fn an_accepted_chat_change_keeps_the_plate_name_only_under_the_same_title() {
    for (title, expected) in [("Test", "Roti"), ("Flatbread", "")] {
        let db = database().await;
        let answer = seed_title_proposal(&db, title).await;
        assert!(matches!(
            accept_chat_proposal(&db, "u1", "r", &answer).await.unwrap(),
            AcceptOutcome::Applied
        ));
        assert_eq!(plate_name(&db).await, expected, "title {title}");
    }
}

#[test]
fn a_chat_proposal_that_only_drops_the_plate_name_changes_nothing() {
    let before = crate::RecipeSnapshot {
        schema_version: 1,
        title: "Roti".into(),
        plate_name: Some("Roti".into()),
        description: String::new(),
        servings: None,
        prep_minutes: None,
        cook_minutes: None,
        ingredients: Vec::new(),
        steps: Vec::new(),
    };
    let same = crate::RecipeProposal {
        title: "Roti".into(),
        description: String::new(),
        servings: None,
        prep_minutes: None,
        cook_minutes: None,
        ingredients: Vec::new(),
        steps: Vec::new(),
    };
    assert!(!same.changes(&before));
}

#[tokio::test]
async fn chat_answers_are_filled_in_only_in_their_own_thread() {
    let db = database().await;
    let answer = start_chat_turn(&db, "u1", "r", "Why?")
        .await
        .unwrap()
        .unwrap();
    for (user_id, recipe_id) in [("u2", "r"), ("u1", "other")] {
        finish_chat_answer(
            &db,
            user_id,
            recipe_id,
            &answer,
            "No.",
            AnswerProposal::None,
        )
        .await
        .unwrap();
        fail_chat_answer(&db, user_id, recipe_id, &answer, "failed", "No.")
            .await
            .unwrap();
    }
    let status: String = sqlx::query_scalar("SELECT status FROM recipe_chat_messages WHERE id=?")
        .bind(&answer)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn deleting_a_recipe_takes_its_frame_candidates_along() {
    let db = database().await;
    let now = stamp();
    sqlx::query(
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at) VALUES('keep','Keep','u1',?,?)",
    )
    .bind(&now)
    .bind(&now)
    .execute(&db)
    .await
    .unwrap();
    for recipe in ["r", "keep"] {
        sqlx::query(
            "INSERT INTO recipe_thumbnail_candidates(recipe_id,idx,seconds,jpeg) VALUES(?,0,1,X'FFD8FFD9')",
        )
        .bind(recipe)
        .execute(&db)
        .await
        .unwrap();
    }
    // Another account cannot delete it.
    assert!(matches!(
        delete_recipe(
            State(state(db.clone())),
            AuthUser { id: "u2".into() },
            Path("r".into())
        )
        .await,
        Err(AppError::NotFound)
    ));
    delete_recipe(
        State(state(db.clone())),
        AuthUser { id: "u1".into() },
        Path("r".into()),
    )
    .await
    .unwrap();
    let candidates: Vec<String> =
        sqlx::query_scalar("SELECT recipe_id FROM recipe_thumbnail_candidates")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(candidates, vec!["keep".to_string()]);
}

#[tokio::test]
async fn the_page_keeps_the_servings_on_its_forms_and_links() {
    let db = database().await;
    sqlx::query("UPDATE recipes SET servings=4 WHERE id='r'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('a','r','step',0,'First'),('b','r','step',1,'Second')").execute(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO recipe_sources(id,recipe_id,position,title,url)
         VALUES('src','r',0,'Reel','https://www.instagram.com/reel/AbCdEf123/')",
    )
    .execute(&db)
    .await
    .unwrap();
    let s = state(db.clone());
    s.moment_jobs.lock().insert("r".into());
    let card = recipe_view(&s, None, None, Some(2)).await;
    // The chat drawer's toggle and the Add note form come back to the
    // scaled card.
    assert_eq!(
        card.matches(r#"name="back" value="/recipes/r?serves=2""#)
            .count(),
        2
    );
    // So do the moments poller, the edit links, and the Card tab.
    assert!(card.contains(r#"hx-get="/recipes/r?serves=2" hx-trigger="every 3s""#));
    assert!(card.contains(r#"href="/recipes/r?serves=2">Check again</a>"#));
    assert!(card.contains(r#"href="/recipes/r?edit_meta=1&#38;serves=2#recipe-details""#));
    assert!(card.contains(r#"data-recipe-view="standard" href="/recipes/r?serves=2""#));
    let chart = recipe_view(&s, Some("chart"), Some(2), Some(2)).await;
    assert!(chart.contains(r#"name="back" value="/recipes/r?view=chart&#38;step=2&#38;serves=2""#));
    // The servings links keep the step being cooked.
    assert!(chart.contains(r#"href="/recipes/r?view=chart&step=2&serves=4""#));
    // As written, the links carry nothing extra.
    let written = recipe_view(&s, None, None, None).await;
    assert!(written.contains(r#"name="back" value="/recipes/r""#));
    assert!(written.contains(r#"data-recipe-view="standard" href="/recipes/r""#));
}

#[test]
fn a_bad_servings_or_step_shows_the_page_as_written() {
    let parse = |uri: &str| {
        Query::<RecipeQuery>::try_from_uri(&uri.parse::<Uri>().unwrap())
            .unwrap()
            .0
    };
    let bad = parse("/recipes/r?serves=abc&step=");
    assert_eq!((bad.serves, bad.step), (None, None));
    assert_eq!(parse("/recipes/r?serves=").serves, None);
    let good = parse("/recipes/r?view=chart&step=3&serves=2");
    assert_eq!((good.serves, good.step), (Some(2), Some(3)));
}

/// The app with one account owning recipe "r", and its session cookie.
async fn signed_in_app() -> (axum::Router, String) {
    let db = database().await;
    let s = state(db.clone());
    let response = setup_create(
        State(s.clone()),
        Form(SetupForm {
            username: "alice".into(),
            password: "correct horse".into(),
            confirm_password: "correct horse".into(),
        }),
    )
    .await
    .unwrap();
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    sqlx::query("UPDATE recipes SET user_id=(SELECT id FROM users) WHERE id='r'")
        .execute(&db)
        .await
        .unwrap();
    (routes(s), cookie)
}

/// Where a form posted to `path` from the page `from` (a header naming it)
/// lands.
async fn lands_on(
    app: &axum::Router,
    cookie: &str,
    path: &str,
    form: &str,
    from: Option<(&str, &str)>,
) -> String {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, cookie);
    if let Some((name, page)) = from {
        request = request.header(name, page);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(form.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
    response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn an_edit_returns_to_the_view_and_servings_it_was_made_from() {
    let (app, cookie) = signed_in_app().await;
    let add = "section=ingredient&text=Salt";
    // htmx names the page; without JavaScript the Referer does.
    let from_htmx = (
        "hx-current-url",
        "http://box:3000/recipes/r?edit=x&serves=2",
    );
    assert_eq!(
        lands_on(&app, &cookie, "/recipes/r/blocks", add, Some(from_htmx)).await,
        "/recipes/r?serves=2"
    );
    let from_chart = (
        "referer",
        "http://box:3000/recipes/r?serves=3&step=4&view=chart#notes",
    );
    assert_eq!(
        lands_on(
            &app,
            &cookie,
            "/recipes/r/filing",
            "folder=&cuisine=",
            Some(from_chart)
        )
        .await,
        "/recipes/r?view=chart&step=4&serves=3"
    );
    // Another page, a bad value, or no page at all: the bare recipe page.
    for from in [
        Some(("hx-current-url", "http://box:3000/recipes/other?serves=2")),
        Some((
            "hx-current-url",
            "http://box:3000/recipes/r/versions/1?serves=2",
        )),
        Some(("referer", "http://box:3000/recipes/r?serves=999&step=2")),
        None,
    ] {
        assert_eq!(
            lands_on(&app, &cookie, "/recipes/r/blocks", add, from).await,
            "/recipes/r",
            "{from:?}"
        );
    }
}
