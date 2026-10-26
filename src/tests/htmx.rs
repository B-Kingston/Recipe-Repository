use crate::auth::setup_create;
use crate::htmx::Flash;
use crate::{AppState, SetupForm, routes};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Form, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::Response,
};
use parking_lot::Mutex;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{collections::HashMap, collections::HashSet, sync::Arc};
use tower::ServiceExt;

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

/// The app with one account, and that account's session cookie.
async fn signed_in() -> (Router, String) {
    let state = state(database().await);
    let response = setup_create(
        State(state.clone()),
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
    (routes(state), cookie)
}

fn get(path: &str, cookie: Option<&str>, htmx: bool) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if htmx {
        builder = builder.header("HX-Request", "true");
    }
    builder.body(Body::empty()).unwrap()
}

fn post(path: &str, cookie: &str, form: &str, htmx: bool) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, cookie);
    if htmx {
        builder = builder.header("HX-Request", "true");
    }
    builder.body(Body::from(form.to_string())).unwrap()
}

fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect()
}

fn header_value<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap())
}

async fn body(response: Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[test]
fn flash_reads_only_short_lowercase_keys() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_static("recipe_display=eink; recipe_flash=cleaner_saved"),
    );
    assert!(Flash::from_headers(&headers).is("cleaner_saved"));
    for forged in [
        "recipe_flash=<script>",
        "recipe_flash=Saved",
        "recipe_flash=",
        "recipe_flash=a%20b",
    ] {
        headers.insert(header::COOKIE, HeaderValue::from_static(forged));
        assert!(Flash::from_headers(&headers).0.is_empty(), "{forged}");
    }
}

#[tokio::test]
async fn a_saved_setting_shows_its_notice_once_and_leaves_the_url_clean() {
    let (app, cookie) = signed_in().await;
    let saved = app
        .clone()
        .oneshot(post("/settings/step-moments", &cookie, "enabled=on", false))
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        saved.headers()[header::LOCATION],
        "/settings#settings-moments"
    );
    let flash = set_cookies(&saved)
        .into_iter()
        .find(|cookie| cookie.starts_with("recipe_flash="))
        .expect("the save sets a flash");
    assert!(flash.starts_with("recipe_flash=moments_saved;"), "{flash}");

    // The page it lands on shows the notice in its block and clears the flash.
    let with_flash = format!("{cookie}; recipe_flash=moments_saved");
    let page = app
        .clone()
        .oneshot(get("/settings", Some(&with_flash), false))
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        set_cookies(&page)
            .iter()
            .any(|cookie| cookie.starts_with("recipe_flash=;") && cookie.contains("Max-Age=0")),
        "{:?}",
        set_cookies(&page)
    );
    let html = body(page).await;
    assert!(html.contains("Step moments saved."), "{html}");
    assert!(!html.contains("?saved=1"));

    let again = app
        .clone()
        .oneshot(get("/settings", Some(&cookie), false))
        .await
        .unwrap();
    assert!(
        set_cookies(&again)
            .iter()
            .all(|cookie| !cookie.starts_with("recipe_flash="))
    );
    assert!(!body(again).await.contains("Step moments saved."));
}

#[tokio::test]
async fn a_flash_survives_responses_that_are_not_its_page() {
    let (app, cookie) = signed_in().await;
    let with_flash = format!("{cookie}; recipe_flash=moments_saved");
    // A stylesheet (or a JSON fetch from the page script) must not eat the
    // notice; only an HTML page does.
    let sheet = app
        .clone()
        .oneshot(get("/display.css", Some(&with_flash), false))
        .await
        .unwrap();
    assert_eq!(sheet.status(), StatusCode::OK);
    assert!(
        set_cookies(&sheet)
            .iter()
            .all(|cookie| !cookie.starts_with("recipe_flash="))
    );
    // A save answered while an older flash is still set keeps only the new one.
    let saved = app
        .clone()
        .oneshot(post(
            "/settings/search-engine",
            &with_flash,
            "engine=bm25",
            false,
        ))
        .await
        .unwrap();
    let flashes: Vec<String> = set_cookies(&saved)
        .into_iter()
        .filter(|cookie| cookie.starts_with("recipe_flash="))
        .collect();
    assert_eq!(flashes.len(), 1, "{flashes:?}");
    assert!(flashes[0].starts_with("recipe_flash=search_saved;"));
}

#[tokio::test]
async fn signed_out_htmx_requests_load_the_sign_in_page_whole() {
    let (app, _) = signed_in().await;
    let plain = app
        .clone()
        .oneshot(get("/recipes/new", None, false))
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        plain.headers()[header::LOCATION],
        "/login?next=/recipes/new"
    );

    let htmx = app
        .clone()
        .oneshot(get("/recipes/new", None, true))
        .await
        .unwrap();
    assert_eq!(htmx.status(), StatusCode::OK);
    assert_eq!(
        header_value(&htmx, "hx-redirect"),
        Some("/login?next=/recipes/new")
    );
    assert!(htmx.headers().get(header::LOCATION).is_none());
}

#[tokio::test]
async fn htmx_errors_land_in_the_main_column() {
    let (app, cookie) = signed_in().await;
    let missing = app
        .clone()
        .oneshot(get("/recipes/no-such-recipe", Some(&cookie), true))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(header_value(&missing, "hx-retarget"), Some(".app-shell"));
    assert_eq!(header_value(&missing, "hx-reselect"), Some(".app-shell"));
    assert_eq!(
        header_value(&missing, "hx-reswap"),
        Some("outerHTML show:top")
    );
    // A GET error keeps its URL, so reloading shows it again.
    assert_eq!(header_value(&missing, "hx-push-url"), None);

    let plain = app
        .clone()
        .oneshot(get("/recipes/no-such-recipe", Some(&cookie), false))
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::NOT_FOUND);
    assert!(plain.headers().get("hx-retarget").is_none());
}

#[tokio::test]
async fn a_form_shown_again_keeps_the_address_bar() {
    let (app, cookie) = signed_in().await;
    let form = "current_password=wrong&new_password=new-password&confirm_password=new-password";
    let htmx = app
        .clone()
        .oneshot(post("/settings/password", &cookie, form, true))
        .await
        .unwrap();
    assert_eq!(htmx.status(), StatusCode::OK);
    assert_eq!(header_value(&htmx, "hx-push-url"), Some("false"));
    assert!(body(htmx).await.contains("Current password is incorrect."));

    let plain = app
        .clone()
        .oneshot(post("/settings/password", &cookie, form, false))
        .await
        .unwrap();
    assert!(plain.headers().get("hx-push-url").is_none());
}

#[tokio::test]
async fn pages_load_htmx_and_the_compiled_script() {
    let (app, cookie) = signed_in().await;
    let html = body(
        app.clone()
            .oneshot(get("/", Some(&cookie), false))
            .await
            .unwrap(),
    )
    .await;
    assert!(html.contains(r#"<body hx-boost="true""#), "{html}");
    assert!(html.contains(r#"<script src="/static/htmx.js" defer></script>"#));
    assert!(html.contains(r#"<script src="/static/app.js" defer></script>"#));
    assert!(html.contains(r#""historyCacheSize":0"#));
}

#[test]
fn a_popup_step_ends_with_an_event_for_htmx_and_a_redirect_otherwise() {
    let plain = crate::htmx::finish_popup(
        &HeaderMap::new(),
        "/settings#settings-ai",
        "claude_connected",
    );
    assert_eq!(plain.status(), StatusCode::SEE_OTHER);
    assert_eq!(plain.headers()[header::LOCATION], "/settings#settings-ai");

    let mut headers = HeaderMap::new();
    headers.insert("hx-request", HeaderValue::from_static("true"));
    let popup = crate::htmx::finish_popup(&headers, "/settings#settings-ai", "claude_connected");
    assert_eq!(popup.status(), StatusCode::NO_CONTENT);
    assert_eq!(popup.headers()["hx-trigger"], crate::htmx::POPUP_FINISHED);
    let cookie = popup.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cookie.starts_with("recipe_flash=claude_connected;"),
        "{cookie}"
    );
}
