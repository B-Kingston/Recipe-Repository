use crate::auth::AuthUser;
use crate::folders::{
    CUISINES, DISH_FOLDERS, Prototypes, STACK_TITLES, Shelf, cuisine_of, dish_folder, file_recipe,
    filing, match_query, resolve, stacks, update_filing, update_library_view,
};
use crate::recipes::{LibraryCard, home};
use crate::{AppError, AppState, HomeQuery, stamp};
use axum::{
    Form,
    extract::{Path, Query, State},
};
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
    for (id, title, user) in [
        ("tart", "Lemon tart", "u1"),
        ("soup", "Tom yum", "u1"),
        ("bread", "Focaccia", "u1"),
        ("theirs", "Their pie", "u2"),
    ] {
        sqlx::query(
            "INSERT INTO recipes(id,title,description,user_id,created_at,updated_at) VALUES(?,?,'',?,?,?)",
        )
        .bind(id)
        .bind(title)
        .bind(user)
        .bind(&now)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();
    }
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
        rate_limits: Default::default(),
    })
}

fn user() -> AuthUser {
    AuthUser { id: "u1".into() }
}

/// A unit vector at `degrees` in the x/y plane.
fn at(degrees: f32) -> Vec<f32> {
    let radians = degrees.to_radians();
    vec![radians.cos(), radians.sin(), 0.0]
}

fn card(id: &str, title: &str) -> LibraryCard {
    LibraryCard {
        id: id.into(),
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

async fn library(db: &SqlitePool, folder: Option<&str>) -> String {
    home(
        State(state(db.clone())),
        user(),
        crate::htmx::Flash::default(),
        Query(HomeQuery {
            folder: folder.map(Into::into),
            ..HomeQuery::default()
        }),
    )
    .await
    .unwrap()
    .0
}

#[test]
fn a_dish_folder_is_the_one_whose_two_closest_examples_score_best() {
    // Folder 0 has one example right on the recipe and one far off; folder
    // 1 has two near ones. The two closest of folder 1 average higher.
    let dishes = vec![vec![at(0.0), at(80.0)], vec![at(15.0), at(20.0)]];
    assert_eq!(dish_folder(&at(0.0), &dishes), Some(1));
    // A folder with a single example still competes on that one.
    let dishes = vec![vec![at(5.0)], vec![at(30.0), at(40.0)]];
    assert_eq!(dish_folder(&at(0.0), &dishes), Some(0));
    assert_eq!(dish_folder(&at(0.0), &[]), None);
}

#[test]
fn a_cuisine_needs_a_clear_win_over_the_minimum_score() {
    // cos 20° = 0.94, cos 40° = 0.77: a clear win.
    assert_eq!(cuisine_of(&at(0.0), &[at(40.0), at(20.0)]), Some(1));
    // A near tie (cos 30° vs cos 30.5°) is no particular cuisine.
    assert_eq!(cuisine_of(&at(0.0), &[at(30.0), at(30.5)]), None);
    // So is a weak best (cos 60° = 0.5) however clear.
    assert_eq!(cuisine_of(&at(0.0), &[at(60.0), at(89.0)]), None);
    assert_eq!(cuisine_of(&at(0.0), &[]), None);
}

#[test]
fn a_pin_beats_the_embedding_and_no_vector_waits_unsorted() {
    let prototypes = Prototypes {
        dishes: DISH_FOLDERS
            .iter()
            .enumerate()
            .map(|(index, _)| vec![at(if index == 2 { 0.0 } else { 70.0 })])
            .collect(),
        cuisines: CUISINES
            .iter()
            .enumerate()
            .map(|(index, _)| at(if index == 6 { 0.0 } else { 70.0 }))
            .collect(),
    };
    let vector = at(0.0);
    let filed = |shelf, pin: &str, vector: Option<&[f32]>| {
        file_recipe(shelf, pin, vector, Some(&prototypes))
    };
    assert_eq!(
        filed(Shelf::Dishes, "", Some(&vector)),
        DISH_FOLDERS[2].slug
    );
    assert_eq!(filed(Shelf::Cuisines, "", Some(&vector)), "thai");
    assert_eq!(filed(Shelf::Dishes, "desserts", Some(&vector)), "desserts");
    assert_eq!(filed(Shelf::Cuisines, "other", Some(&vector)), "other");
    // A pin that names no folder of the shelf is ignored.
    assert_eq!(
        filed(Shelf::Dishes, "thai", Some(&vector)),
        DISH_FOLDERS[2].slug
    );
    assert_eq!(filed(Shelf::Dishes, "", None), "unsorted");
    assert_eq!(
        file_recipe(Shelf::Cuisines, "", Some(&vector), None),
        "unsorted"
    );
    assert_eq!(file_recipe(Shelf::Cuisines, "greek", None, None), "greek");
}

#[test]
fn a_search_that_names_a_folder_opens_it() {
    let found = |query: &str| match_query(query).map(|(shelf, folder)| (shelf, folder.slug));
    assert_eq!(found("thai"), Some((Shelf::Cuisines, "thai")));
    assert_eq!(found("  Thai food "), Some((Shelf::Cuisines, "thai")));
    assert_eq!(
        found("middle eastern cuisine"),
        Some((Shelf::Cuisines, "middle-eastern"))
    );
    assert_eq!(found("dessert"), Some((Shelf::Dishes, "desserts")));
    assert_eq!(found("Soups and stews"), Some((Shelf::Dishes, "soups")));
    assert_eq!(found("soup"), Some((Shelf::Dishes, "soups")));
    assert_eq!(found("pork"), Some((Shelf::Dishes, "meat")));
    assert_eq!(found("noodles"), Some((Shelf::Dishes, "pasta")));
    assert_eq!(found("chicken soup"), None);
    assert_eq!(found("other"), None);
    assert_eq!(found("food"), None);
}

#[test]
fn a_recipe_can_be_pinned_to_any_folder_of_its_shelf_but_not_sorted_yet() {
    let pinnable = |shelf: Shelf, slug: &str| shelf.pinnable(slug).map(|folder| folder.slug);
    assert_eq!(pinnable(Shelf::Dishes, "desserts"), Some("desserts"));
    assert_eq!(pinnable(Shelf::Cuisines, "other"), Some("other"));
    assert_eq!(pinnable(Shelf::Dishes, "thai"), None);
    assert_eq!(pinnable(Shelf::Dishes, "other"), None);
    assert_eq!(pinnable(Shelf::Cuisines, "unsorted"), None);
    assert_eq!(
        Shelf::Cuisines.folder("unsorted").map(|f| f.slug),
        Some("unsorted")
    );
}

#[test]
fn folder_links_resolve_on_either_shelf() {
    let resolved =
        |slug: &str, view| resolve(slug, view).map(|(shelf, folder)| (shelf, folder.slug));
    assert_eq!(resolved("thai", None), Some((Shelf::Cuisines, "thai")));
    assert_eq!(
        resolved("other", Some(Shelf::Dishes)),
        Some((Shelf::Cuisines, "other"))
    );
    assert_eq!(
        resolved("desserts", Some(Shelf::Cuisines)),
        Some((Shelf::Dishes, "desserts"))
    );
    assert_eq!(
        resolved("unsorted", Some(Shelf::Cuisines)),
        Some((Shelf::Cuisines, "unsorted"))
    );
    assert_eq!(
        resolved("unsorted", None),
        Some((Shelf::Dishes, "unsorted"))
    );
    assert_eq!(resolved("nowhere", None), None);
}

#[test]
fn stacks_follow_the_shelf_order_with_unsorted_last() {
    let mut cards = vec![card("u", "Unfiled")];
    let mut filed: HashMap<String, &'static str> = HashMap::from([("u".into(), "unsorted")]);
    for (index, title) in [
        "Pavlova",
        "brownies",
        "Apple pie",
        "Eton mess",
        "Custard",
        "2-minute mug cake",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("d{index}");
        cards.push(card(&id, title));
        filed.insert(id, "desserts");
    }
    cards.push(card("b", "Shakshuka"));
    filed.insert("b".into(), "breakfast");

    let stacks = stacks(Shelf::Dishes, &filed, &cards);
    let names: Vec<&str> = stacks.iter().map(|stack| stack.slug).collect();
    assert_eq!(names, ["breakfast", "desserts", "unsorted"]);
    let desserts = &stacks[1];
    assert_eq!(desserts.count, 6);
    assert_eq!(desserts.titles.len(), STACK_TITLES);
    // Filed A to Z, titles starting with a digit first.
    assert_eq!(
        desserts.titles,
        ["2-minute mug cake", "Apple pie", "brownies", "Custard"]
    );
    assert_eq!(desserts.more, 2);
    assert_eq!(desserts.layers(), 2);
    assert_eq!(stacks[0].layers(), 0);
}

#[tokio::test]
async fn the_folders_view_stacks_pinned_recipes_and_waits_on_the_rest() {
    let db = database().await;
    let page = library(&db, None).await;
    assert!(page.contains("filed A to Z"));
    assert!(page.contains("Lemon tart"));
    assert!(!page.contains("ticket-stack"));

    update_library_view(
        State(state(db.clone())),
        user(),
        Form(serde_json::from_value(serde_json::json!({"view": "folders"})).unwrap()),
    )
    .await
    .unwrap();
    update_filing(
        State(state(db.clone())),
        user(),
        Path("tart".into()),
        Form(
            serde_json::from_value(serde_json::json!({"folder": "desserts", "cuisine": "french"}))
                .unwrap(),
        ),
    )
    .await
    .unwrap();

    // No embedding sidecar here: the pinned tart has its folder, the rest
    // wait in Not sorted yet. Another account's recipe never shows.
    let page = library(&db, None).await;
    assert!(page.contains("by folder"));
    assert!(page.contains(r#"href="/?folder=desserts""#));
    assert!(page.contains(r#"href="/?folder=unsorted""#));
    assert!(page.contains("Desserts &#38; sweets"));
    assert!(!page.contains("Their pie"));
    assert!(!page.contains(r#"class="ticket-link""#));

    let page = library(&db, Some("desserts")).await;
    assert!(page.contains("<h1>Desserts &#38; sweets</h1>"));
    assert!(page.contains("Lemon tart"));
    assert!(!page.contains("Tom yum"));

    // The cuisine pin opens from either view.
    let page = library(&db, Some("french")).await;
    assert!(page.contains("<h1>French</h1>"));
    assert!(page.contains("Lemon tart"));
}

#[tokio::test]
async fn pinning_checks_the_folder_and_the_owner_and_leaves_the_recipe_alone() {
    let db = database().await;
    let before: String = sqlx::query_scalar("SELECT updated_at FROM recipes WHERE id='soup'")
        .fetch_one(&db)
        .await
        .unwrap();
    let pin = |id: &str, folder: &str, cuisine: &str| {
        let db = db.clone();
        let id = id.to_owned();
        let form = serde_json::json!({"folder": folder, "cuisine": cuisine});
        async move {
            update_filing(
                State(state(db)),
                user(),
                Path(id),
                Form(serde_json::from_value(form).unwrap()),
            )
            .await
        }
    };
    assert!(matches!(
        pin("soup", "thai", "").await,
        Err(AppError::BadRequest(_))
    ));
    assert!(matches!(
        pin("soup", "", "soups").await,
        Err(AppError::BadRequest(_))
    ));
    assert!(matches!(
        pin("soup", "unsorted", "").await,
        Err(AppError::BadRequest(_))
    ));
    assert!(matches!(
        pin("theirs", "soups", "").await,
        Err(AppError::NotFound)
    ));
    pin("soup", "soups", "thai").await.unwrap();

    let after: String = sqlx::query_scalar("SELECT updated_at FROM recipes WHERE id='soup'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(before, after);
    let versions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM recipe_versions WHERE recipe_id='soup'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(versions, 0);

    let shown = filing(&state(db.clone()), "u1", "soup").await.unwrap();
    assert_eq!(shown.summary(), "Soups & stews · Thai");
    // Pinning back to Automatic, with no sidecar to ask, shows nothing.
    pin("soup", "", "").await.unwrap();
    let shown = filing(&state(db.clone()), "u1", "soup").await.unwrap();
    assert_eq!(shown.summary(), "");
    assert!(matches!(
        filing(&state(db), "u2", "soup").await,
        Err(AppError::NotFound)
    ));
}

#[tokio::test]
async fn the_library_view_accepts_only_its_three_values() {
    let db = database().await;
    let result = update_library_view(
        State(state(db)),
        user(),
        Form(serde_json::from_value(serde_json::json!({"view": "shelves"})).unwrap()),
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
}
