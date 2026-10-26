use crate::ai::{apply_draft, cancel_draft};
use crate::chat::{
    alter_redirect, ask, change_summary, chat_prompt, clear, divider_label, model_label,
    return_target, toggle_drawer,
};
use crate::htmx::Flash;
use crate::recipes::{
    ChatMessage, chat_thread, fail_interrupted_chats, recipe_page, start_chat_turn,
};
use crate::{
    AppError, AppState, ApplyDraftForm, AuthUser, ChatDrawerForm, ChatForm, GeneratedRecipe,
    GeneratedStep, Ingredient, IngredientUse, RecipeQuery, RecipeSnapshot, SnapshotIngredient,
    SnapshotStep, stamp,
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

/// The vegan version a chat draft holds.
fn vegan() -> GeneratedRecipe {
    GeneratedRecipe {
        title: "Shakshuka".into(),
        description: String::new(),
        prep_minutes: 10,
        cook_minutes: 25,
        servings: 4,
        ingredients: vec![Ingredient {
            name: "firm tofu, crumbled".into(),
            quantity: "400".into(),
            unit: "g".into(),
            optional: false,
        }],
        steps: vec![
            GeneratedStep {
                text: "Simmer the sauce.".into(),
                chart_label: "simmer sauce".into(),
                timer_seconds: 600,
                ingredient_uses: vec![],
                input_steps: vec![],
                ingredients: vec![],
            },
            GeneratedStep {
                text: "Spoon in the tofu and warm through.".into(),
                chart_label: "add tofu".into(),
                timer_seconds: 0,
                ingredient_uses: vec![IngredientUse {
                    ingredient: 0,
                    amount: "400 g".into(),
                }],
                input_steps: vec![0],
                ingredients: vec!["400 g firm tofu, crumbled".into()],
            },
        ],
    }
}

/// A finished chat answer that proposed a change, and a ready draft of it
/// based on the recipe as it is now.
async fn seed_chat_change(db: &SqlitePool, label: &str, draft_id: &str) -> String {
    let answer = start_chat_turn(db, "u1", "r", "Make it vegan")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE recipe_chat_messages SET status='done',content='I will swap the eggs for tofu.',
           change_request='Replace the eggs with tofu.',change_label=?,change_state='ready'
         WHERE id=?",
    )
    .bind(label)
    .bind(&answer)
    .execute(db)
    .await
    .unwrap();
    insert_chat_draft(db, draft_id, &answer).await;
    answer
}

async fn insert_chat_draft(db: &SqlitePool, draft_id: &str, answer: &str) {
    let updated: String = sqlx::query_scalar("SELECT updated_at FROM recipes WHERE id='r'")
        .fetch_one(db)
        .await
        .unwrap();
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO ai_drafts(id,recipe_id,base_updated_at,operation,recipe_json,sources_json,user_id,created_at,expires_at,chat_message_id)
         VALUES(?,'r',?,'alter',?,'[]','u1',?,?,?)",
    )
    .bind(draft_id)
    .bind(&updated)
    .bind(serde_json::to_string(&vegan()).unwrap())
    .bind(now.to_rfc3339())
    .bind((now + chrono::Duration::hours(24)).to_rfc3339())
    .bind(answer)
    .execute(db)
    .await
    .unwrap();
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
async fn saving_a_chat_change_marks_the_thread_with_the_models_label() {
    let db = database().await;
    let s = state(db.clone());
    let answer = seed_chat_change(&db, "Made vegan", "d1").await;
    // An older draft of the same change (before an alteration) goes too.
    insert_chat_draft(&db, "d0", &answer).await;
    start_chat_turn(&db, "u1", "r", "Later question")
        .await
        .unwrap();

    let response = apply_draft(
        State(s.clone()),
        user("u1"),
        Path("d1".into()),
        Form(ApplyDraftForm { thumbnail: None }),
    )
    .await
    .unwrap();
    assert_eq!(response.headers()["location"], "/recipes/r#recipe-chat");

    // The divider lands where the recipe changed: after everything asked so far.
    let rows = thread_rows(&db, "u1", "r").await;
    assert_eq!(
        rows.last().unwrap(),
        &("change".into(), "done".into(), "Made vegan".into())
    );
    let state: String =
        sqlx::query_scalar("SELECT change_state FROM recipe_chat_messages WHERE id=?")
            .bind(&answer)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(state, "applied");
    let drafts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ai_drafts")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(drafts, 0, "every draft of the saved change is consumed");
    let label: String = sqlx::query_scalar(
        "SELECT label FROM recipe_versions WHERE recipe_id='r' ORDER BY version DESC LIMIT 1",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(label, "Chat: Made vegan");
    let ingredient: String = sqlx::query_scalar(
        "SELECT text FROM recipe_blocks WHERE recipe_id='r' AND section='ingredient'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(ingredient, "firm tofu, crumbled");

    let html = page(&s, "u1", "r").await;
    assert!(html.contains("class=\"chat-change\""));
    assert!(html.contains("Made vegan"));
    assert!(html.contains("Saved to the recipe."));
}

#[tokio::test]
async fn a_long_model_label_falls_back_to_what_changed() {
    let db = database().await;
    let s = state(db.clone());
    seed_chat_change(&db, "Swapped the eggs for crumbled tofu", "d1").await;
    apply_draft(
        State(s.clone()),
        user("u1"),
        Path("d1".into()),
        Form(ApplyDraftForm { thumbnail: None }),
    )
    .await
    .unwrap();
    let rows = thread_rows(&db, "u1", "r").await;
    // The empty seeded recipe gains servings, timing, ingredients, and steps.
    assert_eq!(rows.last().unwrap().2, "Recipe changed");
}

#[tokio::test]
async fn discarding_a_chat_change_returns_to_the_chat() {
    let db = database().await;
    let s = state(db.clone());
    let answer = seed_chat_change(&db, "Made vegan", "d1").await;
    let response = cancel_draft(State(s.clone()), user("u1"), Path("d1".into()))
        .await
        .unwrap();
    assert_eq!(response.headers()["location"], "/recipes/r#recipe-chat");
    let state: String =
        sqlx::query_scalar("SELECT change_state FROM recipe_chat_messages WHERE id=?")
            .bind(&answer)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(state, "discarded");
    assert_eq!(
        thread_rows(&db, "u1", "r")
            .await
            .iter()
            .filter(|row| row.0 == "change")
            .count(),
        0
    );
}

#[tokio::test]
async fn a_ready_change_offers_save_preview_and_discard() {
    let db = database().await;
    let s = state(db.clone());
    seed_chat_change(&db, "Made vegan", "d1").await;
    toggle_drawer(State(s.clone()), user("u1"), Path("r".into()), drawer("on"))
        .await
        .unwrap();
    let html = page(&s, "u1", "r").await;
    assert!(html.contains("/ai/drafts/d1/apply"));
    assert!(html.contains("href=\"/ai/drafts/d1\""));
    assert!(html.contains("/ai/drafts/d1/cancel"));
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
    assert!(html.contains(&format!("data-chat-state=\"{answer}:pending:\"")));
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
