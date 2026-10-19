use crate::auth::AuthUser;
use crate::display::{
    DisplayForm, DisplayMode, DisplayQuery, chosen_mode, display_css, display_page, is_kindle,
    update_display, uses_eink,
};
use crate::{AppError, AppState, routes};
use axum::{
    Form,
    body::{Body, to_bytes},
    extract::Query,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
};
use parking_lot::Mutex;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::{collections::HashMap, collections::HashSet, sync::Arc};
use tower::ServiceExt;

const KINDLE: &str = "Mozilla/5.0 (X11; U; Linux armv7l like Android; en-us) AppleWebKit/531.2+ (KHTML, like Gecko) Version/5.0 Safari/533.2+ Kindle/3.0+";
const DESKTOP: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";

fn headers(cookie: Option<&str>, agent: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::USER_AGENT, HeaderValue::from_str(agent).unwrap());
    if let Some(cookie) = cookie {
        headers.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
    }
    headers
}

fn user() -> AuthUser {
    AuthUser { id: "u1".into() }
}

async fn body(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn automatic_mode_uses_eink_only_on_a_kindle() {
    assert_eq!(chosen_mode(&headers(None, KINDLE)), DisplayMode::Auto);
    assert!(is_kindle(&headers(None, KINDLE)));
    assert!(uses_eink(&headers(None, KINDLE)));
    assert!(!is_kindle(&headers(None, DESKTOP)));
    assert!(!uses_eink(&headers(None, DESKTOP)));
}

#[test]
fn a_saved_mode_overrides_the_browser_check() {
    assert!(uses_eink(&headers(
        Some("recipe_session=abc; recipe_display=eink"),
        DESKTOP
    )));
    assert!(!uses_eink(&headers(Some("recipe_display=colour"), KINDLE)));
    assert_eq!(
        chosen_mode(&headers(Some("recipe_display=sepia"), DESKTOP)),
        DisplayMode::Auto
    );
}

#[tokio::test]
async fn display_css_imports_the_eink_sheet_only_in_eink_mode() {
    let eink = display_css(headers(Some("recipe_display=eink"), DESKTOP)).await;
    assert_eq!(eink.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        eink.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    assert!(body(eink).await.contains("/static/eink.css"));
    assert_eq!(body(display_css(headers(None, DESKTOP)).await).await, "");
}

#[tokio::test]
async fn display_css_is_served_without_signing_in() {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().in_memory(true))
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    let state = Arc::new(AppState {
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
    });
    let response = routes(state)
        .oneshot(
            Request::get("/display.css")
                .header(header::USER_AGENT, KINDLE)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body(response).await.contains("/static/eink.css"));
}

#[tokio::test]
async fn saving_a_mode_sets_or_clears_the_cookie() {
    let saved = update_display(
        user(),
        Form(DisplayForm {
            mode: "eink".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(saved.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        saved.headers()[header::LOCATION],
        "/settings/display?saved=1"
    );
    let cookie = saved.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cookie.starts_with("recipe_display=eink;") && cookie.contains("Max-Age=31536000"),
        "{cookie}"
    );

    let auto = update_display(
        user(),
        Form(DisplayForm {
            mode: "auto".into(),
        }),
    )
    .await
    .unwrap();
    let cleared = auto.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cleared.starts_with("recipe_display=;") && cleared.contains("Max-Age=0"),
        "{cleared}"
    );

    let unknown = update_display(
        user(),
        Form(DisplayForm {
            mode: "sepia".into(),
        }),
    )
    .await;
    assert!(matches!(unknown, Err(AppError::BadRequest(_))));
}

#[tokio::test]
async fn display_page_checks_the_saved_mode() {
    let page = display_page(
        user(),
        headers(Some("recipe_display=colour"), KINDLE),
        Query(DisplayQuery {
            saved: Some("1".into()),
        }),
    )
    .await
    .unwrap()
    .0;
    assert!(page.contains(r#"value="colour" checked"#), "{page}");
    assert!(!page.contains(r#"value="auto" checked"#));
    assert!(page.contains("This browser is a Kindle"));
    assert!(page.contains("Display saved."));
}
