use crate::chat::{
    Segment, accept_change, alter_redirect, ask, change_summary, chat_prompt, clear,
    decline_change, divider_label, model_label, proposal_view, return_target, toggle_drawer,
    valid_proposal, word_diff,
};
use crate::htmx::Flash;
use crate::recipes::{
    ChatMessage, ProposalRow, chat_thread, fail_interrupted_chats, finish_chat_answer, recipe_page,
    recipe_snapshot, start_chat_turn,
};
use crate::{
    AppError, AppState, AuthUser, ChatDrawerForm, ChatForm, ChatProposal, ProposedIngredient,
    ProposedStep, RecipeProposal, RecipeQuery, RecipeSnapshot, SnapshotIngredient, SnapshotStep,
    stamp,
};
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
};
use parking_lot::Mutex;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, collections::HashSet, sync::Arc};

/// Two accounts, each with one recipe: u1 owns "r", u2 owns "r2".
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
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at)
         VALUES('r','Shakshuka','u1',?1,?1),('r2','Dal','u2',?1,?1)",
    )
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

fn user(id: &str) -> AuthUser {
    AuthUser { id: id.into() }
}

fn question(text: &str) -> Form<ChatForm> {
    Form(ChatForm {
        question: text.into(),
        back: "/recipes/r".into(),
    })
}

fn drawer(open: &str) -> Form<ChatDrawerForm> {
    Form(ChatDrawerForm {
        open: open.into(),
        back: "/recipes/r".into(),
    })
}

fn card_query() -> Query<RecipeQuery> {
    Query(RecipeQuery {
        edit: None,
        edit_meta: None,
        view: None,
        step: None,
    })
}

async fn page(s: &Arc<AppState>, user_id: &str, recipe_id: &str) -> String {
    recipe_page(
        State(s.clone()),
        user(user_id),
        Path(recipe_id.into()),
        card_query(),
        Flash::default(),
    )
    .await
    .unwrap()
    .0
}

fn row(id: &str, role: &str, content: &str, status: &str) -> ChatMessage {
    ChatMessage {
        id: id.into(),
        role: role.into(),
        content: content.into(),
        status: status.into(),
        change_label: String::new(),
        change_state: String::new(),
        proposal_json: String::new(),
        proposal_base: String::new(),
        created_at: stamp(),
    }
}

fn snapshot() -> RecipeSnapshot {
    RecipeSnapshot {
        schema_version: 1,
        title: "Shakshuka".into(),
        description: "Eggs in sauce".into(),
        servings: Some(4),
        prep_minutes: Some(10),
        cook_minutes: Some(25),
        ingredients: vec![SnapshotIngredient {
            name: "eggs".into(),
            quantity: "6".into(),
            unit: String::new(),
            optional: false,
        }],
        steps: vec![SnapshotStep {
            text: "Crack in the eggs.".into(),
            ingredients: vec!["6 eggs".into()],
        }],
    }
}

/// Gives u1's shakshuka two ingredients and two steps, with fixed block ids.
async fn seed_blocks(db: &SqlitePool) {
    sqlx::query(
        "INSERT INTO recipe_blocks(id,recipe_id,section,position,text,quantity,unit,optional) VALUES
         ('i-eggs','r','ingredient',0,'eggs','6','',0),
         ('i-onion','r','ingredient',1,'onion','1','',0),
         ('s-sauce','r','step',0,'Simmer the onion in the sauce.','','',0),
         ('s-eggs','r','step',1,'Crack in the eggs and cover.','','',0)",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recipe_step_ingredients(id,step_id,position,text) VALUES
         ('u1','s-sauce',0,'1 onion'),('u2','s-eggs',0,'6 eggs')",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query("UPDATE recipes SET servings=4,chart_json='{\"chart\":1}' WHERE id='r'")
        .execute(db)
        .await
        .unwrap();
}

async fn current(db: &SqlitePool) -> RecipeSnapshot {
    let mut conn = db.acquire().await.unwrap();
    recipe_snapshot(&mut conn, "u1", "r").await.unwrap()
}

/// The vegan proposal over the seeded recipe: tofu for the eggs (changed in
/// place), the onion removed, salt added, the egg step reworded, a new last
/// step, and servings 4 to 6.
fn vegan(before: &RecipeSnapshot) -> RecipeProposal {
    RecipeProposal {
        title: before.title.clone(),
        description: before.description.clone(),
        servings: Some(6),
        prep_minutes: before.prep_minutes,
        cook_minutes: before.cook_minutes,
        ingredients: vec![
            ProposedIngredient {
                from: Some(0),
                name: "firm tofu".into(),
                quantity: "400".into(),
                unit: "g".into(),
                optional: false,
            },
            ProposedIngredient {
                from: None,
                name: "salt".into(),
                quantity: "1".into(),
                unit: "tsp".into(),
                optional: false,
            },
        ],
        steps: vec![
            ProposedStep {
                from: Some(0),
                text: before.steps[0].text.clone(),
                ingredients: before.steps[0].ingredients.clone(),
            },
            ProposedStep {
                from: Some(1),
                text: "Crumble in the tofu and cover.".into(),
                ingredients: vec!["400 g firm tofu".into(), "1 tsp salt".into()],
            },
            ProposedStep {
                from: None,
                text: "Serve with bread.".into(),
                ingredients: vec![],
            },
        ],
    }
}

/// A finished answer that proposes `after` over the recipe as it is now.
async fn seed_proposal(
    db: &SqlitePool,
    label: &str,
    after: impl Fn(&RecipeSnapshot) -> RecipeProposal,
) -> String {
    let before = current(db).await;
    let base: String = sqlx::query_scalar("SELECT updated_at FROM recipes WHERE id='r'")
        .fetch_one(db)
        .await
        .unwrap();
    let answer = start_chat_turn(db, "u1", "r", "Make it vegan")
        .await
        .unwrap();
    let json = serde_json::to_string(&ChatProposal {
        after: after(&before),
        before,
    })
    .unwrap();
    assert!(
        finish_chat_answer(
            db,
            &answer,
            "Swapped the eggs for tofu.",
            Some(ProposalRow {
                label,
                json: &json,
                base: &base
            })
        )
        .await
        .unwrap()
    );
    answer
}

async fn change_state(db: &SqlitePool, answer: &str) -> String {
    sqlx::query_scalar("SELECT change_state FROM recipe_chat_messages WHERE id=?")
        .bind(answer)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn thread_rows(
    db: &SqlitePool,
    user_id: &str,
    recipe_id: &str,
) -> Vec<(String, String, String)> {
    chat_thread(db, user_id, recipe_id)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.role, row.status, row.content))
        .collect()
}

#[tokio::test]
async fn asking_without_an_ai_login_stores_the_question_and_points_to_settings() {
    let db = database().await;
    let s = state(db.clone());
    let response = ask(
        State(s.clone()),
        user("u1"),
        Path("r".into()),
        question("  Can I use fresh tomatoes?  "),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/recipes/r#recipe-chat");

    // The answer runs on a background task; no login fails it quickly.
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = thread_rows(&db, "u1", "r").await;
        if rows.get(1).is_some_and(|row| row.1 != "pending") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        rows[0],
        (
            "user".into(),
            "done".into(),
            "Can I use fresh tomatoes?".into()
        )
    );
    assert_eq!(rows[1].0, "assistant");
    assert_eq!(rows[1].1, "needs_settings");
    assert!(rows[1].2.contains("Settings"), "{}", rows[1].2);

    let html = page(&s, "u1", "r").await;
    assert!(
        html.contains("id=\"recipe-chat\""),
        "asking opens the drawer"
    );
    assert!(html.contains("Can I use fresh tomatoes?"));
    assert!(html.contains("/settings#settings-ai"));
}

#[tokio::test]
async fn a_second_question_waits_for_the_running_answer() {
    let db = database().await;
    let s = state(db.clone());
    start_chat_turn(&db, "u1", "r", "First?").await.unwrap();
    let response = ask(
        State(s.clone()),
        user("u1"),
        Path("r".into()),
        question("Second?"),
    )
    .await
    .unwrap();
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie.starts_with("recipe_flash=chat_busy;"), "{cookie}");
    assert_eq!(thread_rows(&db, "u1", "r").await.len(), 2, "no new rows");
}

#[tokio::test]
async fn chats_are_private_to_the_account() {
    let db = database().await;
    let s = state(db.clone());
    start_chat_turn(&db, "u1", "r", "A private question")
        .await
        .unwrap();

    // Another account can neither ask about, open, nor clear u1's recipe chat.
    let asked = ask(
        State(s.clone()),
        user("u2"),
        Path("r".into()),
        question("Hi?"),
    )
    .await;
    assert!(matches!(asked, Err(AppError::NotFound)));
    let cleared = clear(State(s.clone()), user("u2"), Path("r".into()), drawer("")).await;
    assert!(matches!(cleared, Err(AppError::NotFound)));
    assert!(thread_rows(&db, "u2", "r").await.is_empty());
    assert_eq!(thread_rows(&db, "u1", "r").await.len(), 2);

    // u2's own recipe page with the drawer open shows none of it.
    toggle_drawer(
        State(s.clone()),
        user("u2"),
        Path("r2".into()),
        Form(ChatDrawerForm {
            open: "on".into(),
            back: "/recipes/r2".into(),
        }),
    )
    .await
    .unwrap();
    let html = page(&s, "u2", "r2").await;
    assert!(html.contains("id=\"recipe-chat\""));
    assert!(!html.contains("A private question"));

    // Clearing removes only the owner's thread.
    clear(State(s.clone()), user("u1"), Path("r".into()), drawer(""))
        .await
        .unwrap();
    assert!(thread_rows(&db, "u1", "r").await.is_empty());
}

#[tokio::test]
async fn the_drawer_toggles_and_returns_to_the_view_it_was_opened_from() {
    let db = database().await;
    let s = state(db.clone());
    assert!(page(&s, "u1", "r").await.contains("aria-pressed=\"false\""));
    assert!(!page(&s, "u1", "r").await.contains("id=\"recipe-chat\""));

    let response = toggle_drawer(
        State(s.clone()),
        user("u1"),
        Path("r".into()),
        Form(ChatDrawerForm {
            open: "on".into(),
            back: "/recipes/r?view=chart&step=2".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        response.headers()["location"],
        "/recipes/r?view=chart&step=2#recipe-chat"
    );
    let html = page(&s, "u1", "r").await;
    assert!(html.contains("id=\"recipe-chat\""));
    assert!(html.contains("aria-pressed=\"true\""));

    let response = toggle_drawer(
        State(s.clone()),
        user("u1"),
        Path("r".into()),
        drawer("off"),
    )
    .await
    .unwrap();
    assert_eq!(response.headers()["location"], "/recipes/r");
    assert!(!page(&s, "u1", "r").await.contains("id=\"recipe-chat\""));
}

#[test]
fn return_targets_stay_on_the_recipe_page() {
    assert_eq!(
        return_target("r", "/recipes/r?view=chart&step=3"),
        "/recipes/r?view=chart&step=3"
    );
    assert_eq!(return_target("r", "/recipes/r"), "/recipes/r");
    for outside in [
        "//evil.example",
        "/recipes/r2",
        "/recipes/rx",
        "/recipes/r#x",
        "https://x",
        "",
    ] {
        assert_eq!(return_target("r", outside), "/recipes/r", "{outside}");
    }
}

#[tokio::test]
async fn switching_the_chat_off_hides_it_and_closes_its_routes() {
    let db = database().await;
    let s = state(db.clone());
    sqlx::query(
        "INSERT INTO app_settings(user_id,key,value,updated_at) VALUES('u1','recipe_chat','off',?)",
    )
    .bind(stamp())
    .execute(&db)
    .await
    .unwrap();
    assert!(!page(&s, "u1", "r").await.contains("chat-toggle"));
    let asked = ask(
        State(s.clone()),
        user("u1"),
        Path("r".into()),
        question("Hi?"),
    )
    .await;
    assert!(matches!(asked, Err(AppError::NotFound)));
    assert!(thread_rows(&db, "u1", "r").await.is_empty());
    // The other account still has its chat.
    assert!(page(&s, "u2", "r2").await.contains("chat-toggle"));
}

#[tokio::test]
async fn accepting_a_proposal_applies_it_in_place_and_marks_the_thread() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let answer = seed_proposal(&db, "Made vegan", vegan).await;
    start_chat_turn(&db, "u1", "r", "Later question")
        .await
        .unwrap();

    let response = accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer.clone())),
        drawer(""),
    )
    .await
    .unwrap();
    assert_eq!(response.headers()["location"], "/recipes/r#recipe-chat");
    assert!(
        response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .starts_with("recipe_flash=change_applied;")
    );

    let blocks: Vec<(String, String, i64, String, String, String)> = sqlx::query_as(
        "SELECT id,section,position,text,quantity,unit FROM recipe_blocks WHERE recipe_id='r' ORDER BY section,position",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    // Kept items keep their rows (and so their step moments); the onion goes.
    assert_eq!(
        blocks[0],
        (
            "i-eggs".into(),
            "ingredient".into(),
            0,
            "firm tofu".into(),
            "400".into(),
            "g".into()
        )
    );
    assert_eq!(
        (blocks[1].1.as_str(), blocks[1].2, blocks[1].3.as_str()),
        ("ingredient", 1, "salt")
    );
    assert_eq!(blocks[2].0, "s-sauce");
    assert_eq!(
        (blocks[3].0.as_str(), blocks[3].3.as_str()),
        ("s-eggs", "Crumble in the tofu and cover.")
    );
    assert_eq!(
        (blocks[4].2, blocks[4].3.as_str()),
        (2, "Serve with bread.")
    );
    assert_eq!(blocks.len(), 5);
    let saved = current(&db).await;
    assert_eq!(saved.servings, Some(6));
    assert_eq!(
        saved.steps[1].ingredients,
        vec!["400 g firm tofu", "1 tsp salt"]
    );
    let chart: String = sqlx::query_scalar("SELECT chart_json FROM recipes WHERE id='r'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        chart, "",
        "a changed method drops the AI chart like any block edit"
    );

    // The divider lands where the recipe changed: after everything asked so far.
    let rows = thread_rows(&db, "u1", "r").await;
    assert_eq!(
        rows.last().unwrap(),
        &("change".into(), "done".into(), "Made vegan".into())
    );
    assert_eq!(change_state(&db, &answer).await, "applied");
    let labels: Vec<String> = sqlx::query_scalar(
        "SELECT label FROM recipe_versions WHERE recipe_id='r' ORDER BY version",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(labels, vec!["Original recipe", "Chat: Made vegan"]);

    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(html.contains("class=\"chat-change\""));
    assert!(html.contains("Saved to the recipe."));
    // Accepting again does nothing more.
    accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer)),
        drawer(""),
    )
    .await
    .unwrap();
    assert_eq!(
        thread_rows(&db, "u1", "r")
            .await
            .iter()
            .filter(|row| row.0 == "change")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_details_only_change_keeps_the_chart_and_an_unusable_label_falls_back() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let answer = seed_proposal(&db, "Scaled the whole recipe up for a crowd", |before| {
        let mut after = vegan(before);
        after.ingredients = before
            .ingredients
            .iter()
            .enumerate()
            .map(|(at, item)| ProposedIngredient {
                from: Some(at),
                name: item.name.clone(),
                quantity: item.quantity.clone(),
                unit: item.unit.clone(),
                optional: item.optional,
            })
            .collect();
        after.steps = before
            .steps
            .iter()
            .enumerate()
            .map(|(at, step)| ProposedStep {
                from: Some(at),
                text: step.text.clone(),
                ingredients: step.ingredients.clone(),
            })
            .collect();
        after
    })
    .await;
    accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer)),
        drawer(""),
    )
    .await
    .unwrap();
    assert_eq!(
        thread_rows(&db, "u1", "r").await.last().unwrap().2,
        "Servings changed"
    );
    let chart: String = sqlx::query_scalar("SELECT chart_json FROM recipes WHERE id='r'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(chart, "{\"chart\":1}");
}

#[tokio::test]
async fn a_proposal_is_refused_once_the_recipe_changed_since() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let answer = seed_proposal(&db, "Made vegan", vegan).await;
    sqlx::query("UPDATE recipes SET title='Green shakshuka',updated_at='2099-01-01T00:00:00+00:00' WHERE id='r'")
        .execute(&db)
        .await
        .unwrap();
    let response = accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer.clone())),
        drawer(""),
    )
    .await
    .unwrap();
    assert!(
        response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .starts_with("recipe_flash=change_stale;")
    );
    assert_eq!(change_state(&db, &answer).await, "ready");
    assert_eq!(current(&db).await.ingredients[0].name, "eggs");
    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(html.contains("can no longer be saved"));
    assert!(!html.contains(&format!("/chat/{answer}/accept")));
    assert!(html.contains(">Dismiss</button>"));
}

#[tokio::test]
async fn denying_a_proposal_leaves_the_recipe_as_it_was() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let before = current(&db).await;
    let answer = seed_proposal(&db, "Made vegan", vegan).await;
    let response = decline_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer.clone())),
        drawer(""),
    )
    .await
    .unwrap();
    assert_eq!(response.headers()["location"], "/recipes/r#recipe-chat");
    assert!(
        response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .starts_with("recipe_flash=change_declined;")
    );
    assert_eq!(change_state(&db, &answer).await, "discarded");
    assert_eq!(current(&db).await, before);
    assert_eq!(
        thread_rows(&db, "u1", "r")
            .await
            .iter()
            .filter(|row| row.0 == "change")
            .count(),
        0
    );
    // A denied proposal cannot be accepted afterwards.
    accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer)),
        drawer(""),
    )
    .await
    .unwrap();
    assert_eq!(current(&db).await, before);
}

#[tokio::test]
async fn another_account_cannot_accept_or_deny_a_proposal() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let answer = seed_proposal(&db, "Made vegan", vegan).await;
    let accepted = accept_change(
        State(s.clone()),
        user("u2"),
        Path(("r".into(), answer.clone())),
        drawer(""),
    )
    .await;
    assert!(matches!(accepted, Err(AppError::NotFound)));
    let denied = decline_change(
        State(s.clone()),
        user("u2"),
        Path(("r".into(), answer.clone())),
        drawer(""),
    )
    .await;
    assert!(matches!(denied, Err(AppError::NotFound)));
    // Nor through its own recipe's URL.
    accept_change(
        State(s.clone()),
        user("u2"),
        Path(("r2".into(), answer.clone())),
        drawer(""),
    )
    .await
    .unwrap();
    assert_eq!(change_state(&db, &answer).await, "ready");
}

#[tokio::test]
async fn a_ready_proposal_lists_its_changes_under_accept_and_deny() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let answer = seed_proposal(&db, "Made vegan", vegan).await;
    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(html.contains(&format!("/recipes/r/chat/{answer}/accept")));
    assert!(html.contains(&format!("/recipes/r/chat/{answer}/deny")));
    assert!(
        html.contains("1 detail · 3 ingredients · 2 steps"),
        "{html}"
    );
    assert!(html.contains("<span class=\"chat-diff-place\">Servings</span> <del>4</del>"));
    assert!(
        html.contains("<del>6 eggs</del> <ins>400 g firm tofu</ins>"),
        "{html}"
    );
    assert!(html.contains("<del>1 onion</del>"));
    assert!(html.contains("<ins>1 tsp salt</ins>"));
    assert!(
        html.contains(
            "<del>Crack</del> <ins>Crumble</ins> in the <del>eggs</del> <ins>tofu</ins> and cover."
        ),
        "{html}"
    );
    assert!(html.contains("<span class=\"chat-diff-place\">Step 3</span>"));
    assert!(
        !html.contains("data-chat-state"),
        "nothing is running, so nothing polls"
    );
}

#[tokio::test]
async fn a_running_answer_polls_and_disables_the_question_box() {
    let db = database().await;
    let s = state(db.clone());
    let answer = start_chat_turn(&db, "u1", "r", "How thick?").await.unwrap();
    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(html.contains(&format!("data-chat-state=\"{answer}:pending\"")));
    assert!(html.contains("Thinking…"));
    assert!(html.contains("data-chat-question disabled"));
}

#[tokio::test]
async fn interrupted_answers_and_drafts_fail_at_startup() {
    let db = database().await;
    let pending = start_chat_turn(&db, "u1", "r", "Still there?")
        .await
        .unwrap();
    let drafting = start_chat_turn(&db, "u1", "r", "Make it vegan")
        .await
        .unwrap();
    sqlx::query("UPDATE recipe_chat_messages SET status='done',change_state='drafting' WHERE id=?")
        .bind(&drafting)
        .execute(&db)
        .await
        .unwrap();
    fail_interrupted_chats(&db).await.unwrap();
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id,status,change_state FROM recipe_chat_messages WHERE role='assistant' ORDER BY rowid")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(rows[0], (pending, "failed".into(), String::new()));
    assert_eq!(rows[1], (drafting, "done".into(), "failed".into()));
}

#[test]
fn the_prompt_carries_the_recipe_history_dividers_and_latest_question() {
    let thread = vec![
        row("1", "user", "Can I use butter?", "done"),
        row("2", "assistant", "Yes, 30 g.", "done"),
        row("3", "user", "Make it vegan", "done"),
        row(
            "4",
            "assistant",
            "The answer was interrupted. Ask again.",
            "failed",
        ),
        row("5", "change", "Made vegan", "done"),
        row("6", "user", "How long now?", "done"),
        row("7", "assistant", "", "pending"),
    ];
    let (prompt, question) = chat_prompt(
        "{\"title\":\"Shakshuka\"}",
        "The cook has kept 2 notes on this recipe.",
        &thread,
        "7",
    )
    .unwrap();
    assert_eq!(question, "How long now?");
    assert!(prompt.starts_with(
        "Current recipe JSON:\n{\"title\":\"Shakshuka\"}\n\nThe cook has kept 2 notes on this recipe.\n\n"
    ));
    assert!(prompt.contains("Cook: Can I use butter?\nAssistant: Yes, 30 g.\nCook: Make it vegan\n[The cook saved a change to the recipe here: Made vegan."));
    assert!(
        !prompt.contains("interrupted"),
        "failed answers are left out"
    );
    assert!(prompt.ends_with("Latest question from the cook:\nHow long now?"));
    assert_eq!(
        prompt.matches("How long now?").count(),
        1,
        "the question is not repeated as history"
    );
    assert!(chat_prompt("{}", "", &thread, "missing").is_none());
}

#[test]
fn model_labels_are_tidied_or_dropped() {
    assert_eq!(model_label(" made vegan. "), "Made vegan");
    assert_eq!(model_label("Serves 6"), "Serves 6");
    assert_eq!(model_label("Eggs → tofu"), "Eggs → tofu");
    assert_eq!(model_label("Swapped the eggs for tofu"), "");
    assert_eq!(model_label("Extraordinarilylongsingleword"), "");
    assert_eq!(model_label("   "), "");
}

#[test]
fn change_summaries_name_what_differs() {
    let before = snapshot();
    let mut after = snapshot();
    assert_eq!(change_summary(&before, &after), "Recipe saved");
    after.servings = Some(6);
    assert_eq!(change_summary(&before, &after), "Servings changed");
    after.ingredients[0].quantity = "9".into();
    assert_eq!(
        change_summary(&before, &after),
        "Servings and ingredients changed"
    );
    after.steps[0].text = "Crack in 9 eggs.".into();
    assert_eq!(change_summary(&before, &after), "Recipe changed");
    assert_eq!(divider_label("Serves 6", &before, &after), "Serves 6");
    assert_eq!(divider_label("", &before, &after), "Recipe changed");
}

#[tokio::test]
async fn the_old_alter_page_opens_the_chat_instead() {
    let db = database().await;
    let s = state(db.clone());
    assert!(
        !page(&s, "u1", "r").await.contains("/ai/alter"),
        "no Alter with AI button"
    );
    let response = alter_redirect(State(s.clone()), user("u1"), Path("r".into()))
        .await
        .unwrap();
    assert_eq!(response.headers()["location"], "/recipes/r#recipe-chat");
    assert!(page(&s, "u1", "r").await.contains("id=\"recipe-chat\""));
    let other = alter_redirect(State(s.clone()), user("u2"), Path("r".into())).await;
    assert!(matches!(other, Err(AppError::NotFound)));
}

fn proposal_over(before: &RecipeSnapshot) -> RecipeProposal {
    RecipeProposal {
        title: before.title.clone(),
        description: before.description.clone(),
        servings: before.servings,
        prep_minutes: before.prep_minutes,
        cook_minutes: before.cook_minutes,
        ingredients: before
            .ingredients
            .iter()
            .enumerate()
            .map(|(at, item)| ProposedIngredient {
                from: Some(at),
                name: item.name.clone(),
                quantity: item.quantity.clone(),
                unit: item.unit.clone(),
                optional: item.optional,
            })
            .collect(),
        steps: before
            .steps
            .iter()
            .enumerate()
            .map(|(at, step)| ProposedStep {
                from: Some(at),
                text: step.text.clone(),
                ingredients: step.ingredients.clone(),
            })
            .collect(),
    }
}

#[test]
fn proposals_must_change_something_and_name_real_distinct_items() {
    let before = snapshot();
    let same = proposal_over(&before);
    assert!(!valid_proposal(&before, &same), "no visible change");
    let mut ok = same.clone();
    ok.servings = Some(8);
    assert!(valid_proposal(&before, &ok));
    let mut bad = ok.clone();
    bad.ingredients[0].from = Some(5);
    assert!(!valid_proposal(&before, &bad), "from past the end");
    let mut bad = ok.clone();
    bad.ingredients.push(bad.ingredients[0].clone());
    assert!(!valid_proposal(&before, &bad), "two items from one origin");
    let mut bad = ok.clone();
    bad.steps[0].text = "  ".into();
    assert!(!valid_proposal(&before, &bad));
    let mut bad = ok.clone();
    bad.title = String::new();
    assert!(!valid_proposal(&before, &bad));
    let mut bad = ok;
    bad.ingredients[0].name = "x".repeat(5000);
    assert!(!valid_proposal(&before, &bad));
}

#[test]
fn word_diffs_mark_only_the_changed_words() {
    let runs = |text: &[(&'static str, &str)]| {
        text.iter()
            .map(|(kind, text)| Segment {
                kind,
                text: text.to_string(),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        word_diff(
            "Bake at 180C for 25 minutes.",
            "Bake at 160C for 30 minutes."
        ),
        runs(&[
            ("same", "Bake at"),
            ("del", "180C"),
            ("ins", "160C"),
            ("same", "for"),
            ("del", "25"),
            ("ins", "30"),
            ("same", "minutes.")
        ])
    );
    assert_eq!(word_diff("", "Serve."), runs(&[("ins", "Serve.")]));
    assert_eq!(
        word_diff("same words", "same words"),
        runs(&[("same", "same words")])
    );
}

#[test]
fn the_view_shows_moves_and_puts_removed_items_where_they_were() {
    let mut before = snapshot();
    before.steps = ["One.", "Two.", "Three."]
        .iter()
        .map(|text| SnapshotStep {
            text: text.to_string(),
            ingredients: vec![],
        })
        .collect();
    let mut after = proposal_over(&before);
    // Three moves to the top, Two is removed.
    after.steps = vec![after.steps[2].clone(), after.steps[0].clone()];
    let view = proposal_view(&ChatProposal { before, after }, false);
    let steps: Vec<(&str, &str, &str)> = view
        .steps
        .iter()
        .map(|change| {
            (
                change.kind,
                change.place.as_str(),
                change.moved_from.as_str(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        vec![
            ("moved", "Step 1", "was step 3"),
            ("removed", "Was step 2", "")
        ]
    );
    assert_eq!(view.summary, "2 steps");
    assert!(view.details.is_empty() && view.ingredients.is_empty());
}

// ---------------------------------------------------------------------------
// Review gaps: the staleness guard against every hand edit, reorders, late
// answers, escaping, concurrency, size limits, and rows from before proposals.

use crate::recipes::{
    add_block, clear_chat, delete_block, move_block, restore_version, update_block, update_recipe,
};
use crate::{BlockForm, RecipeForm};
use sqlx::sqlite::SqliteJournalMode;

fn details_form(title: &str, description: &str) -> Form<RecipeForm> {
    Form(RecipeForm {
        title: title.into(),
        description: description.into(),
        servings: "4".into(),
        prep_minutes: String::new(),
        cook_minutes: String::new(),
    })
}

fn block_form(section: Option<&str>, text: &str, uses: Option<&str>) -> BlockForm {
    BlockForm {
        section: section.map(Into::into),
        text: text.into(),
        quantity: None,
        unit: None,
        optional: None,
        step_ingredients: uses.map(Into::into),
    }
}

fn flash_of(response: &axum::response::Response) -> String {
    response
        .headers()
        .get("set-cookie")
        .map(|value| value.to_str().unwrap().to_string())
        .unwrap_or_default()
}

async fn chat_versions(db: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT label FROM recipe_versions WHERE recipe_id='r' AND label LIKE 'Chat:%' ORDER BY version",
    )
    .fetch_all(db)
    .await
    .unwrap()
}

async fn dividers(db: &SqlitePool) -> usize {
    thread_rows(db, "u1", "r")
        .await
        .iter()
        .filter(|row| row.0 == "change")
        .count()
}

/// Each way the cook edits a recipe by hand, run between a proposal and its
/// Accept. The staleness guard rests on every one of them moving updated_at:
/// a details edit, a block edit, or a move keeps the block counts the same.
#[tokio::test]
async fn every_hand_edit_between_proposal_and_accept_makes_the_proposal_stale() {
    for edit in ["details", "add", "edit", "move", "delete", "restore"] {
        let db = database().await;
        let s = state(db.clone());
        seed_blocks(&db).await;
        if edit == "restore" {
            // Versions to restore: the original, then a details edit.
            update_recipe(
                State(s.clone()),
                user("u1"),
                Path("r".into()),
                details_form("Shakshuka", "Spicy"),
            )
            .await
            .unwrap();
        }
        let answer = seed_proposal(&db, "Made vegan", vegan).await;
        let proposed_over = current(&db).await;
        match edit {
            "details" => {
                update_recipe(
                    State(s.clone()),
                    user("u1"),
                    Path("r".into()),
                    details_form("Green shakshuka", ""),
                )
                .await
                .unwrap();
            }
            "add" => {
                add_block(
                    State(s.clone()),
                    user("u1"),
                    Path("r".into()),
                    Form(block_form(Some("step"), "Rest for 2 minutes.", None)),
                )
                .await
                .unwrap();
            }
            "edit" => {
                update_block(
                    State(s.clone()),
                    user("u1"),
                    Path(("r".into(), "s-eggs".into())),
                    Form(block_form(
                        None,
                        "Crack in 8 eggs and cover.",
                        Some("8 eggs"),
                    )),
                )
                .await
                .unwrap();
            }
            "move" => {
                move_block(
                    State(s.clone()),
                    user("u1"),
                    Path(("r".into(), "s-eggs".into(), "up".into())),
                    Ok(Form(block_form(None, "", None))),
                )
                .await
                .unwrap();
            }
            "delete" => {
                delete_block(
                    State(s.clone()),
                    user("u1"),
                    Path(("r".into(), "i-onion".into())),
                )
                .await
                .unwrap();
            }
            _ => {
                restore_version(State(s.clone()), user("u1"), Path(("r".into(), 1)))
                    .await
                    .unwrap();
            }
        }
        let edited = current(&db).await;
        assert_ne!(
            edited, proposed_over,
            "{edit}: the hand edit changed the recipe"
        );
        let response = accept_change(
            State(s.clone()),
            user("u1"),
            Path(("r".into(), answer.clone())),
            drawer(""),
        )
        .await
        .unwrap();
        assert!(
            flash_of(&response).starts_with("recipe_flash=change_stale;"),
            "{edit}: {}",
            flash_of(&response)
        );
        assert_eq!(current(&db).await, edited, "{edit}: the hand edit stands");
        assert_eq!(change_state(&db, &answer).await, "ready", "{edit}");
        assert_eq!(dividers(&db).await, 0, "{edit}");
    }
}

#[tokio::test]
async fn blocks_added_without_a_new_timestamp_still_make_the_proposal_stale() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let answer = seed_proposal(&db, "Made vegan", vegan).await;
    sqlx::query("INSERT INTO recipe_blocks(id,recipe_id,section,position,text) VALUES('s-extra','r','step',2,'Garnish.')")
        .execute(&db)
        .await
        .unwrap();
    let response = accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer)),
        drawer(""),
    )
    .await
    .unwrap();
    assert!(flash_of(&response).starts_with("recipe_flash=change_stale;"));
    assert_eq!(current(&db).await.steps.len(), 3);
}

#[tokio::test]
async fn accepting_saves_exactly_what_the_proposal_showed() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    sqlx::query(
        "INSERT INTO recipe_step_moments(step_id,seconds,source) VALUES('s-eggs',42,'audio')",
    )
    .execute(&db)
    .await
    .unwrap();
    let answer = seed_proposal(&db, "Œufs", |before| {
        let mut after = proposal_over(before);
        after.title = "  Shakshuka verte  ".into();
        after.ingredients[0].name = "œufs 🥚".into();
        after.steps[1].text = "Cassez les œufs — couvrez.".into();
        after.steps[1].ingredients = vec!["½ onion".into(), "½ onion".into()];
        after
    })
    .await;
    let proposal: String =
        sqlx::query_scalar("SELECT proposal_json FROM recipe_chat_messages WHERE id=?")
            .bind(&answer)
            .fetch_one(&db)
            .await
            .unwrap();
    let proposal: ChatProposal = serde_json::from_str(&proposal).unwrap();
    let response = accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer)),
        drawer(""),
    )
    .await
    .unwrap();
    assert!(flash_of(&response).starts_with("recipe_flash=change_applied;"));
    let seconds: i64 =
        sqlx::query_scalar("SELECT seconds FROM recipe_step_moments WHERE step_id='s-eggs'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(seconds, 42, "an edited step keeps its row and video moment");
    let mut expected = proposal.after.snapshot();
    expected.title = expected.title.trim().into();
    assert_eq!(
        current(&db).await,
        expected,
        "what was shown is what was saved"
    );
    assert_eq!(chat_versions(&db).await, vec!["Chat: Œufs"]);
}

/// The block rows each item of `after` lands on, in order.
async fn block_rows(db: &SqlitePool) -> Vec<(String, String, i64)> {
    sqlx::query_as(
        "SELECT id,section,position FROM recipe_blocks WHERE recipe_id='r' ORDER BY section,position",
    )
    .fetch_all(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn accepting_a_proposal_that_reorders_items_keeps_their_rows() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let answer = seed_proposal(&db, "Eggs first", |before| {
        let mut after = proposal_over(before);
        after.ingredients.reverse();
        after.steps.reverse();
        after
    })
    .await;
    let response = accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer)),
        drawer(""),
    )
    .await
    .expect("Accept saves a reordered recipe");
    assert!(flash_of(&response).starts_with("recipe_flash=change_applied;"));
    assert_eq!(
        block_rows(&db).await,
        vec![
            ("i-onion".into(), "ingredient".into(), 0),
            ("i-eggs".into(), "ingredient".into(), 1),
            ("s-eggs".into(), "step".into(), 0),
            ("s-sauce".into(), "step".into(), 1),
        ]
    );
}

#[tokio::test]
async fn accepting_a_step_added_at_the_start_shifts_the_others_down() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let answer = seed_proposal(&db, "Preheat first", |before| {
        let mut after = proposal_over(before);
        after.steps.insert(
            0,
            ProposedStep {
                from: None,
                text: "Heat the oven to 200C.".into(),
                ingredients: vec![],
            },
        );
        after
    })
    .await;
    accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer)),
        drawer(""),
    )
    .await
    .expect("Accept saves a step added at the start");
    let steps: Vec<String> = current(&db)
        .await
        .steps
        .into_iter()
        .map(|step| step.text)
        .collect();
    assert_eq!(steps[0], "Heat the oven to 200C.");
    assert_eq!(steps.len(), 3);
}

#[tokio::test]
async fn an_answer_landing_after_an_accept_shows_its_proposal_as_stale() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let first = seed_proposal(&db, "Made vegan", vegan).await;
    // A second question is running, over the recipe as it was, when the
    // first proposal is accepted.
    let read_before = current(&db).await;
    let base_before: String = sqlx::query_scalar("SELECT updated_at FROM recipes WHERE id='r'")
        .fetch_one(&db)
        .await
        .unwrap();
    let second = start_chat_turn(&db, "u1", "r", "Serve 8").await.unwrap();
    let response = accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), first)),
        drawer(""),
    )
    .await
    .unwrap();
    assert!(
        flash_of(&response).starts_with("recipe_flash=change_applied;"),
        "accepting while another answer runs is allowed"
    );
    let mut serves_8 = proposal_over(&read_before);
    serves_8.servings = Some(8);
    let json = serde_json::to_string(&ChatProposal {
        before: read_before,
        after: serves_8,
    })
    .unwrap();
    assert!(
        finish_chat_answer(
            &db,
            &second,
            "Serves 8 now.",
            Some(ProposalRow {
                label: "Serves 8",
                json: &json,
                base: &base_before,
            }),
        )
        .await
        .unwrap()
    );
    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(html.contains("can no longer be saved"));
    assert!(!html.contains(&format!("/chat/{second}/accept")));
    let response = accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), second)),
        drawer(""),
    )
    .await
    .unwrap();
    assert!(flash_of(&response).starts_with("recipe_flash=change_stale;"));
    assert_eq!(
        current(&db).await.servings,
        Some(6),
        "the vegan change stands"
    );
}

#[tokio::test]
async fn a_thread_cleared_while_the_model_runs_drops_the_late_answer() {
    let db = database().await;
    let answer = start_chat_turn(&db, "u1", "r", "Make it vegan")
        .await
        .unwrap();
    clear_chat(&db, "u1", "r").await.unwrap();
    let json = serde_json::to_string(&ChatProposal {
        before: snapshot(),
        after: proposal_over(&snapshot()),
    })
    .unwrap();
    let stored = finish_chat_answer(
        &db,
        &answer,
        "Done.",
        Some(ProposalRow {
            label: "Made vegan",
            json: &json,
            base: "x",
        }),
    )
    .await
    .unwrap();
    assert!(!stored);
    assert!(thread_rows(&db, "u1", "r").await.is_empty());
}

#[tokio::test]
async fn model_text_in_answers_and_proposals_is_escaped() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let script = "<script>alert(1)</script>";
    let answer = seed_proposal(&db, "<img src=x onerror=alert(1)>", |before| {
        let mut after = proposal_over(before);
        after.title = format!("Shakshuka {script}");
        after.description = "\"><svg onload=alert(1)>".into();
        after.ingredients[0].name = format!("eggs {script}");
        after.steps[0].text = format!("{script} Simmer.");
        after.steps[0].ingredients = vec![script.into()];
        after
    })
    .await;
    sqlx::query("UPDATE recipe_chat_messages SET content=? WHERE id=?")
        .bind(format!(
            "Done {script} <a href=\"javascript:alert(1)\">x</a>"
        ))
        .bind(&answer)
        .execute(&db)
        .await
        .unwrap();
    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    for raw in [
        "<script>alert",
        "<svg onload",
        "<img src=x",
        "href=\"javascript:",
    ] {
        assert!(!html.contains(raw), "{raw} reached the page unescaped");
    }
    assert!(
        html.contains("&#60;script&#62;alert(1)"),
        "the text is shown, escaped"
    );
    accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer)),
        drawer(""),
    )
    .await
    .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(
        !html.contains("<script>alert"),
        "nor once saved to the card"
    );
}

/// A file database in WAL mode with several connections, as the app runs.
async fn shared_database() -> (SqlitePool, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "kindle-recipes-chat-race-{}.sqlite3",
        uuid::Uuid::new_v4()
    ));
    let db = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true)
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    sqlx::query(
        "INSERT INTO recipes(id,title,user_id,created_at,updated_at) VALUES('r','Shakshuka','u1',?1,?1)",
    )
    .bind(stamp())
    .execute(&db)
    .await
    .unwrap();
    (db, path)
}

/// Accept and Deny (or two Accepts) racing on one proposal leave one
/// consistent outcome: applied with exactly one divider and version, or
/// denied with the recipe untouched. A request may fail with a busy
/// database, but never half-applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_accept_and_deny_leave_one_consistent_outcome() {
    for round in 0..12 {
        let (db, path) = shared_database().await;
        let s = state(db.clone());
        seed_blocks(&db).await;
        let before = current(&db).await;
        let answer = seed_proposal(&db, "Made vegan", vegan).await;
        let second_is_accept = round % 3 == 0;
        let first = tokio::spawn(accept_change(
            State(s.clone()),
            user("u1"),
            Path(("r".into(), answer.clone())),
            drawer(""),
        ));
        let second = {
            let s = s.clone();
            let answer = answer.clone();
            tokio::spawn(async move {
                if second_is_accept {
                    accept_change(State(s), user("u1"), Path(("r".into(), answer)), drawer(""))
                        .await
                } else {
                    decline_change(State(s), user("u1"), Path(("r".into(), answer)), drawer(""))
                        .await
                }
            })
        };
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        let outcome = change_state(&db, &answer).await;
        match outcome.as_str() {
            "applied" => {
                assert_eq!(current(&db).await.ingredients[0].name, "firm tofu");
                assert_eq!(dividers(&db).await, 1, "round {round}");
                assert_eq!(chat_versions(&db).await.len(), 1, "round {round}");
            }
            "discarded" => {
                assert!(!second_is_accept);
                assert_eq!(current(&db).await, before, "round {round}");
                assert_eq!(dividers(&db).await, 0, "round {round}");
                assert!(
                    first.is_err()
                        || flash_of(first.as_ref().unwrap()).is_empty()
                        || flash_of(first.as_ref().unwrap())
                            .starts_with("recipe_flash=change_stale")
                );
            }
            other => panic!(
                "round {round}: left {other} (accept: {:?}, other: {:?})",
                first.map(|r| flash_of(&r)),
                second.map(|r| flash_of(&r))
            ),
        }
        db.close().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}

#[test]
fn proposals_past_the_size_and_number_limits_are_refused() {
    let before = snapshot();
    let mut ok = proposal_over(&before);
    ok.steps[0].text = "é".repeat(4000);
    assert!(
        valid_proposal(&before, &ok),
        "the limit counts characters, not bytes"
    );
    let mut bad = ok.clone();
    bad.steps[0].text.push('é');
    assert!(!valid_proposal(&before, &bad));
    let mut bad = ok.clone();
    bad.steps[0].ingredients = vec!["x".repeat(4001)];
    assert!(!valid_proposal(&before, &bad));
    let mut bad = ok.clone();
    bad.ingredients
        .extend((0..200).map(|at| ProposedIngredient {
            from: None,
            name: format!("item {at}"),
            quantity: String::new(),
            unit: String::new(),
            optional: false,
        }));
    assert!(!valid_proposal(&before, &bad), "201 ingredients");
    for servings in [-1, 100_001] {
        let mut bad = ok.clone();
        bad.servings = Some(servings);
        assert!(!valid_proposal(&before, &bad), "servings {servings}");
    }
    let mut bad = ok.clone();
    bad.cook_minutes = Some(-5);
    assert!(!valid_proposal(&before, &bad));
    let mut bad = ok;
    bad.description = "d".repeat(4001);
    assert!(!valid_proposal(&before, &bad));
}

#[test]
fn long_and_unicode_texts_diff_without_losing_words() {
    let long: String = (0..401).map(|at| format!("w{at} ")).collect();
    let runs = word_diff(&long, "short");
    assert_eq!(runs.len(), 2, "past the word limit it is old text then new");
    assert_eq!((runs[0].kind, runs[1].kind), ("del", "ins"));
    assert_eq!(runs[0].text.split(' ').count(), 401);
    assert_eq!(
        word_diff("Add ½ tsp crème fraîche 🥚", "Add 1 tsp crème fraîche 🥚"),
        vec![
            Segment {
                kind: "same",
                text: "Add".into()
            },
            Segment {
                kind: "del",
                text: "½".into()
            },
            Segment {
                kind: "ins",
                text: "1".into()
            },
            Segment {
                kind: "same",
                text: "tsp crème fraîche 🥚".into()
            },
        ]
    );
}

#[tokio::test]
async fn a_ready_row_from_before_proposals_renders_and_cannot_be_accepted() {
    let db = database().await;
    let s = state(db.clone());
    seed_blocks(&db).await;
    let before = current(&db).await;
    // The old chat set 'ready' while a separate draft awaited review; 0022
    // added proposal_json empty for those rows.
    let answer = start_chat_turn(&db, "u1", "r", "Make it vegan")
        .await
        .unwrap();
    sqlx::query("UPDATE recipe_chat_messages SET status='done',content='I will make it vegan.',change_state='ready' WHERE id=?")
        .bind(&answer)
        .execute(&db)
        .await
        .unwrap();
    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(html.contains("This change can no longer be shown."));
    let response = accept_change(
        State(s.clone()),
        user("u1"),
        Path(("r".into(), answer.clone())),
        drawer(""),
    )
    .await
    .unwrap();
    assert!(flash_of(&response).is_empty());
    assert_eq!(current(&db).await, before);
}

#[tokio::test]
async fn a_ready_row_from_before_proposals_can_be_dismissed() {
    let db = database().await;
    let s = state(db.clone());
    let answer = start_chat_turn(&db, "u1", "r", "Make it vegan")
        .await
        .unwrap();
    sqlx::query("UPDATE recipe_chat_messages SET status='done',content='OK',change_state='ready' WHERE id=?")
        .bind(&answer)
        .execute(&db)
        .await
        .unwrap();
    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    assert!(
        page(&s, "u1", "r")
            .await
            .contains(&format!("/chat/{answer}/deny"))
    );
}

#[test]
fn dropping_a_repeated_step_line_shows_as_a_deletion() {
    let mut before = snapshot();
    before.steps[0].ingredients = vec!["6 eggs".into(), "6 eggs".into()];
    let mut after = proposal_over(&before);
    after.steps[0].ingredients = vec!["6 eggs".into()];
    let view = proposal_view(&ChatProposal { before, after }, false);
    assert!(
        view.steps[0].uses.iter().any(|line| line.kind == "del"),
        "{:?}",
        view.steps[0].uses
    );
}

#[test]
fn the_prompt_says_which_earlier_proposals_were_not_saved() {
    let mut proposed = row(
        "2",
        "assistant",
        "Made it vegan; accept or deny below.",
        "done",
    );
    proposed.change_state = "ready".into();
    let mut denied = row("4", "assistant", "Halved the salt.", "done");
    denied.change_state = "discarded".into();
    let thread = vec![
        row("1", "user", "Make it vegan", "done"),
        proposed,
        row("3", "user", "Less salt", "done"),
        denied,
        row("5", "user", "Now scale it to 6", "done"),
        row("6", "assistant", "", "pending"),
    ];
    let (prompt, _) = chat_prompt("{}", "", &thread, "6").unwrap();
    assert!(
        prompt.contains("not saved") || prompt.contains("denied"),
        "{prompt}"
    );
}
