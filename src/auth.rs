use crate::{
    AppError, AppState, LoginForm, LoginQuery, LoginTemplate, NewAccountTemplate,
    ResetPasswordForm, ResetPasswordTemplate, Result, SetupForm, SetupTemplate, SwitchAccountForm,
    render, stamp, trim,
};
use argon2::{Argon2, PasswordHasher, PasswordVerifier, password_hash::PasswordHash};
use axum::{
    extract::{FromRequestParts, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, header, request::Parts},
    middleware::Next,
    response::{AppendHeaders, Html, IntoResponse, Redirect, Response},
};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{
    fmt::Write,
    sync::{Arc, LazyLock},
};
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct AuthUser {
    pub(crate) id: String,
}

impl<S: Send + Sync> FromRequestParts<S> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self> {
        parts
            .extensions
            .get::<AuthUser>()
            .cloned()
            .ok_or_else(|| AppError::Internal("authenticated user context missing".into()))
    }
}

pub(crate) fn hash_password(password: &str) -> Result<String> {
    let salt = argon2::password_hash::SaltString::generate(&mut rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| AppError::Internal(format!("password hashing failed: {error}")))
}

pub(crate) fn verify_password(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// Hash that unknown usernames are checked against, so a failed sign-in costs
/// the same Argon2 work whether or not the account exists.
static UNKNOWN_USER_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password("kindle-recipes-unknown-user").unwrap_or_default());

/// Argon2 hashing on the blocking pool, so the async runtime is never stalled.
async fn hash_off_runtime(password: String) -> Result<String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|error| AppError::Internal(format!("password hashing task failed: {error}")))?
}

/// Argon2 verification on the blocking pool. An unknown account (`None`) is
/// checked against the dummy hash, so it costs the same as a wrong password.
async fn check_password(password: String, stored: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || match stored {
        Some(stored) => verify_password(&password, &stored),
        None => {
            verify_password(&password, &UNKNOWN_USER_HASH);
            false
        }
    })
    .await
    .unwrap_or(false)
}

const MIN_PASSWORD_CHARS: usize = 8;

/// Rules for a password being set: not blank, typed identically twice, and at
/// least MIN_PASSWORD_CHARS characters. The password is measured as typed and
/// stored exactly, never trimmed.
fn check_new_password(password: &str, confirm: &str, blank_message: &str) -> Result<()> {
    if password.trim().is_empty() {
        return Err(AppError::BadRequest(blank_message.into()));
    }
    if password != confirm {
        return Err(AppError::BadRequest("Passwords do not match.".into()));
    }
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(AppError::BadRequest(format!(
            "Password must be at least {MIN_PASSWORD_CHARS} characters."
        )));
    }
    Ok(())
}

const MAX_USERNAME_CHARS: usize = 40;

/// A new account's username, trimmed: required, and short enough for the
/// account menu.
fn check_new_username(username: &str) -> Result<String> {
    let username = trim(username);
    if username.is_empty() {
        return Err(AppError::BadRequest("Username is required.".into()));
    }
    if username.chars().count() > MAX_USERNAME_CHARS {
        return Err(AppError::BadRequest(format!(
            "Username must be at most {MAX_USERNAME_CHARS} characters."
        )));
    }
    Ok(username)
}

/// Name of the cookie that carries the active session's token.
const SESSION_COOKIE: &str = "recipe_session";
/// Name of the cookie that carries the tokens of the browser's other
/// signed-in accounts, joined with `.`, so the account menu can switch to them
/// without a password.
const ACCOUNTS_COOKIE: &str = "recipe_accounts";
/// Most accounts one browser stays signed in to, the active one included.
const MAX_BROWSER_ACCOUNTS: usize = 8;
/// A session ends after this many days without a request.
const SESSION_DAYS: i64 = 30;
/// The middleware extends a session at most once in this many hours, so most
/// requests only read the `sessions` table.
const SESSION_REFRESH_HOURS: i64 = 24;
/// `Set-Cookie` value that removes the session cookie from the browser.
const CLEARED_SESSION_COOKIE: &str = "recipe_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax";
/// `Set-Cookie` value that removes the other-accounts cookie from the browser.
const CLEARED_ACCOUNTS_COOKIE: &str = "recipe_accounts=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax";

/// Two `Set-Cookie` headers: the session cookie, then the accounts cookie.
type BrowserCookies = AppendHeaders<[(HeaderName, HeaderValue); 2]>;

/// SHA-256 of a session token, as stored in `sessions.token_hash`. The table
/// never holds the token itself.
fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Whether `token` has the shape of a session token: 32 bytes in unpadded
/// base64url, 43 characters.
fn is_token(token: &str) -> bool {
    token.len() == 43
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn session_cookie_header(name: &str, value: &str) -> Result<HeaderValue> {
    let max_age = SESSION_DAYS * 24 * 60 * 60;
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax"
    ))
    .map_err(|_| AppError::Internal("session cookie is not a valid header".into()))
}

fn session_cookie(token: &str) -> Result<HeaderValue> {
    session_cookie_header(SESSION_COOKIE, token)
}

/// `Set-Cookie` value that remembers the browser's other signed-in accounts,
/// or removes the cookie when there are none.
fn accounts_cookie(tokens: &[&str]) -> Result<HeaderValue> {
    if tokens.is_empty() {
        return Ok(HeaderValue::from_static(CLEARED_ACCOUNTS_COOKIE));
    }
    session_cookie_header(ACCOUNTS_COOKIE, &tokens.join("."))
}

/// Makes `active` the browser's session and remembers `others` behind it.
fn browser_cookies(active: &str, others: &[&str]) -> Result<BrowserCookies> {
    Ok(AppendHeaders([
        (header::SET_COOKIE, session_cookie(active)?),
        (header::SET_COOKIE, accounts_cookie(others)?),
    ]))
}

/// Signs the browser out of every account.
fn cleared_cookies() -> BrowserCookies {
    AppendHeaders([
        (
            header::SET_COOKIE,
            HeaderValue::from_static(CLEARED_SESSION_COOKIE),
        ),
        (
            header::SET_COOKIE,
            HeaderValue::from_static(CLEARED_ACCOUNTS_COOKIE),
        ),
    ])
}

/// Starts a session for the user and returns its token.
async fn start_session(db: impl sqlx::SqliteExecutor<'_>, user_id: &str) -> Result<String> {
    let mut bytes = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let now = Utc::now();
    sqlx::query("INSERT INTO sessions(token_hash,user_id,created_at,expires_at) VALUES(?,?,?,?)")
        .bind(token_hash(&token))
        .bind(user_id)
        .bind(now.to_rfc3339())
        .bind((now + Duration::days(SESSION_DAYS)).to_rfc3339())
        .execute(db)
        .await?;
    Ok(token)
}

/// Starts a session for the user and returns the `Set-Cookie` value that
/// carries its token to the browser.
async fn create_session(db: impl sqlx::SqliteExecutor<'_>, user_id: &str) -> Result<HeaderValue> {
    session_cookie(&start_session(db, user_id).await?)
}

async fn end_session(db: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash(token))
        .execute(db)
        .await?;
    Ok(())
}

/// The value of the named cookie from the request's `Cookie` headers, or
/// `None` when the browser sent none.
fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name && !value.is_empty()).then_some(value)
        })
}

/// The active session's token, if the browser sent one.
fn session_token(headers: &HeaderMap) -> Option<&str> {
    cookie_value(headers, SESSION_COOKIE)
}

/// Tokens of the browser's other signed-in accounts in the order the cookie
/// lists them, without duplicates or anything that cannot be a token.
fn remembered_tokens(headers: &HeaderMap) -> Vec<&str> {
    let mut tokens: Vec<&str> = Vec::new();
    for token in cookie_value(headers, ACCOUNTS_COOKIE)
        .unwrap_or_default()
        .split('.')
    {
        if is_token(token) && !tokens.contains(&token) && tokens.len() < MAX_BROWSER_ACCOUNTS {
            tokens.push(token);
        }
    }
    tokens
}

/// One live session this browser holds.
struct BrowserSession {
    token: String,
    user_id: String,
    expires_at: String,
}

/// The browser's live sessions: the active one first, then the remembered
/// ones in cookie order. Each user appears once; expired and unknown tokens
/// are left out. When the active cookie has gone stale, the first remembered
/// session leads the list, and the middleware makes it the active one.
async fn browser_sessions(db: &SqlitePool, headers: &HeaderMap) -> Result<Vec<BrowserSession>> {
    let now = Utc::now().to_rfc3339();
    let mut sessions: Vec<BrowserSession> = Vec::new();
    for token in session_token(headers)
        .into_iter()
        .chain(remembered_tokens(headers))
    {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT user_id, expires_at FROM sessions WHERE token_hash = ? AND expires_at > ?",
        )
        .bind(token_hash(token))
        .bind(&now)
        .fetch_optional(db)
        .await?;
        let Some((user_id, expires_at)) = row else {
            continue;
        };
        if sessions.iter().all(|session| session.user_id != user_id) {
            sessions.push(BrowserSession {
                token: token.to_string(),
                user_id,
                expires_at,
            });
        }
    }
    Ok(sessions)
}

/// Whether a session was extended in the last SESSION_REFRESH_HOURS, so it
/// needs no new expiry yet.
fn extended_recently(expires_at: &str) -> bool {
    let full_expiry = Utc::now() + Duration::days(SESSION_DAYS);
    DateTime::parse_from_rfc3339(expires_at).is_ok_and(|expires| {
        expires.with_timezone(&Utc) > full_expiry - Duration::hours(SESSION_REFRESH_HOURS)
    })
}

/// Gives a session a full new lifetime from now.
async fn extend_session(db: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("UPDATE sessions SET expires_at = ? WHERE token_hash = ?")
        .bind((Utc::now() + Duration::days(SESSION_DAYS)).to_rfc3339())
        .bind(token_hash(token))
        .execute(db)
        .await?;
    Ok(())
}

/// Signs the browser in to `user_id` with a new session and makes it the
/// active one. The browser's other accounts stay signed in behind it, up to
/// MAX_BROWSER_ACCOUNTS in all. An older session of the same user on this
/// browser ends, as does the oldest one that no longer fits.
async fn sign_in_browser(
    db: &SqlitePool,
    headers: &HeaderMap,
    user_id: &str,
) -> Result<BrowserCookies> {
    let previous = browser_sessions(db, headers).await?;
    let token = start_session(db, user_id).await?;
    let mut others: Vec<&str> = Vec::new();
    for session in &previous {
        if session.user_id != user_id && others.len() + 1 < MAX_BROWSER_ACCOUNTS {
            others.push(&session.token);
        } else {
            end_session(db, &session.token).await?;
        }
    }
    browser_cookies(&token, &others)
}

/// `next` when it is a path on this site, else the library. Absolute URLs and
/// the protocol-relative forms `//host` and `/\host` are refused, so the
/// sign-in page cannot send the browser to another site.
fn local_target(next: &str) -> &str {
    let bytes = next.as_bytes();
    let local = bytes.first() == Some(&b'/') && !matches!(bytes.get(1), Some(b'/' | b'\\'));
    if local && !next.chars().any(char::is_control) {
        next
    } else {
        "/"
    }
}

/// Percent-encodes a path for use as a query value. Unreserved characters and
/// `/` stay as they are; every other byte becomes `%XX`.
fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(char::from(byte));
            }
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

/// Sends a signed-out visitor to the sign-in page. A GET keeps its path and
/// query in `next`, so signing in returns there. Other methods go to the
/// library after sign-in, because their form data is gone. An htmx request
/// gets `HX-Redirect`, so the sign-in page loads whole instead of inside the
/// page that asked.
fn login_redirect(req: &Request) -> Response {
    let target = req
        .uri()
        .path_and_query()
        .map_or("/", |target| target.as_str());
    if !matches!(*req.method(), Method::GET | Method::HEAD) || target == "/" {
        return crate::htmx::full_page_redirect(req.headers(), "/login");
    }
    let to = format!("/login?next={}", encode_query_value(target));
    crate::htmx::full_page_redirect(req.headers(), &to)
}

/// The sign-in page for one account, its username filled in.
fn sign_in_url(username: &str) -> String {
    format!("/login?username={}", encode_query_value(username))
}

/// One other account in the header's account menu.
pub(crate) struct MenuAccount {
    pub(crate) id: String,
    pub(crate) username: String,
    pub(crate) initial: String,
    /// The browser holds a live session for it, so choosing it switches at
    /// once; otherwise it links to the sign-in page.
    pub(crate) signed_in: bool,
    pub(crate) sign_in_url: String,
}

/// The header's account menu: the signed-in account and every other account
/// on this server.
pub(crate) struct AccountMenu {
    pub(crate) username: String,
    pub(crate) initial: String,
    pub(crate) others: Vec<MenuAccount>,
    /// The browser is signed in to more than one account.
    pub(crate) several_signed_in: bool,
}

tokio::task_local! {
    static ACCOUNT_MENU: Arc<AccountMenu>;
}

/// The account menu for the page being rendered. The auth middleware sets it
/// for every signed-in request, so `base.html` reads it here instead of every
/// template struct carrying it. Pages served outside the middleware (sign-in,
/// setup) get `None` and render no menu.
pub(crate) fn account_menu() -> Option<Arc<AccountMenu>> {
    ACCOUNT_MENU.try_with(Arc::clone).ok()
}

/// The first letter of a username, upper-cased, for the account avatar.
fn initial(username: &str) -> String {
    username
        .chars()
        .next()
        .map(|first| first.to_uppercase().collect())
        .unwrap_or_default()
}

async fn load_account_menu(
    db: &SqlitePool,
    user_id: &str,
    sessions: &[BrowserSession],
) -> Result<AccountMenu> {
    let users: Vec<(String, String)> =
        sqlx::query_as("SELECT id, username FROM users ORDER BY username COLLATE NOCASE, username")
            .fetch_all(db)
            .await?;
    let mut username = String::new();
    let mut others = Vec::new();
    for (id, name) in users {
        if id == user_id {
            username = name;
            continue;
        }
        others.push(MenuAccount {
            signed_in: sessions.iter().any(|session| session.user_id == id),
            initial: initial(&name),
            sign_in_url: sign_in_url(&name),
            id,
            username: name,
        });
    }
    Ok(AccountMenu {
        initial: initial(&username),
        username,
        others,
        several_signed_in: sessions.len() > 1,
    })
}

pub(crate) async fn user_count(db: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await?)
}

/// Creates the first account at setup and returns its id. The account claims
/// every row left without an owner ('').
pub(crate) async fn create_user(
    db: &SqlitePool,
    username: &str,
    password: &str,
    confirm: &str,
) -> Result<String> {
    let username = check_new_username(username)?;
    check_new_password(password, confirm, "Password is required.")?;
    let password_hash = hash_off_runtime(password.to_string()).await?;
    let user_id = Uuid::new_v4().to_string();
    let mut tx = db.begin().await?;
    // Conditional insert: the check and the write are one statement, so two
    // racing setup requests cannot both create the account.
    let created = sqlx::query(
        "INSERT INTO users(id,username,password_hash,created_at)
         SELECT ?,?,?,? WHERE NOT EXISTS (SELECT 1 FROM users)",
    )
    .bind(&user_id)
    .bind(&username)
    .bind(password_hash)
    .bind(stamp())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if created == 0 {
        return Err(AppError::BadRequest("A user already exists.".into()));
    }
    for table in [
        "recipes",
        "ai_drafts",
        "pi_credentials",
        "ai_endpoints",
        "app_settings",
    ] {
        let query = format!("UPDATE {table} SET user_id=? WHERE user_id='' ");
        sqlx::query(&query).bind(&user_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(user_id)
}

/// Adds another account and returns its id. It starts with an empty library
/// and default settings. Usernames are unique regardless of letter case.
pub(crate) async fn add_user(
    db: &SqlitePool,
    username: &str,
    password: &str,
    confirm: &str,
) -> Result<String> {
    let username = check_new_username(username)?;
    check_new_password(password, confirm, "Password is required.")?;
    let password_hash = hash_off_runtime(password.to_string()).await?;
    let user_id = Uuid::new_v4().to_string();
    // The check and the write are one statement, so two racing requests
    // cannot both take the username.
    let created = sqlx::query(
        "INSERT INTO users(id,username,password_hash,created_at)
         SELECT ?,?,?,? WHERE NOT EXISTS
           (SELECT 1 FROM users WHERE username = ? COLLATE NOCASE)",
    )
    .bind(&user_id)
    .bind(&username)
    .bind(password_hash)
    .bind(stamp())
    .bind(&username)
    .execute(db)
    .await?
    .rows_affected();
    if created == 0 {
        return Err(AppError::BadRequest("That username is taken.".into()));
    }
    Ok(user_id)
}

pub(crate) async fn setup_page(State(state): State<Arc<AppState>>) -> Result<Response> {
    if user_count(&state.db).await? > 0 {
        return Ok(Redirect::to("/").into_response());
    }
    render(SetupTemplate {
        error: String::new(),
        username: String::new(),
    })
    .map(IntoResponse::into_response)
}

/// Creates the account and signs the browser in with it.
pub(crate) async fn setup_create(
    State(state): State<Arc<AppState>>,
    axum::Form(form): axum::Form<SetupForm>,
) -> Result<Response> {
    if user_count(&state.db).await? > 0 {
        return Ok(Redirect::to("/").into_response());
    }
    match create_user(
        &state.db,
        &form.username,
        &form.password,
        &form.confirm_password,
    )
    .await
    {
        Ok(user_id) => {
            let cookie = create_session(&state.db, &user_id).await?;
            Ok(([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response())
        }
        Err(AppError::BadRequest(error)) => render(SetupTemplate {
            error,
            username: trim(&form.username),
        })
        .map(IntoResponse::into_response),
        Err(error) => Err(error),
    }
}

pub(crate) async fn new_account_page(_user: AuthUser) -> Result<Html<String>> {
    render(NewAccountTemplate {
        error: String::new(),
        username: String::new(),
    })
}

/// Creates another account from the account menu and switches the browser to
/// it. The account that made it stays signed in behind it.
pub(crate) async fn create_account(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
    headers: HeaderMap,
    axum::Form(form): axum::Form<SetupForm>,
) -> Result<Response> {
    match add_user(
        &state.db,
        &form.username,
        &form.password,
        &form.confirm_password,
    )
    .await
    {
        Ok(user_id) => {
            let cookies = sign_in_browser(&state.db, &headers, &user_id).await?;
            Ok((cookies, Redirect::to("/")).into_response())
        }
        Err(AppError::BadRequest(error)) => render(NewAccountTemplate {
            error,
            username: trim(&form.username),
        })
        .map(IntoResponse::into_response),
        Err(error) => Err(error),
    }
}

/// Makes another account this browser is signed in to the active one and
/// opens its library. An account the browser is not signed in to goes to the
/// sign-in page with its username filled in.
pub(crate) async fn switch_account(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    headers: HeaderMap,
    axum::Form(form): axum::Form<SwitchAccountForm>,
) -> Result<Response> {
    if form.user_id == user.id {
        return Ok(Redirect::to("/").into_response());
    }
    let sessions = browser_sessions(&state.db, &headers).await?;
    let Some(target) = sessions
        .iter()
        .find(|session| session.user_id == form.user_id)
    else {
        let username: Option<String> =
            sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
                .bind(&form.user_id)
                .fetch_optional(&state.db)
                .await?;
        let username = username.ok_or(AppError::NotFound)?;
        return Ok(Redirect::to(&sign_in_url(&username)).into_response());
    };
    // The account that was active moves to the front of the others, so
    // signing out of the new one returns to it.
    let others: Vec<&str> = sessions
        .iter()
        .filter(|session| session.token != target.token)
        .map(|session| session.token.as_str())
        .collect();
    let cookies = browser_cookies(&target.token, &others)?;
    Ok((cookies, Redirect::to("/")).into_response())
}

pub(crate) async fn login_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Result<Response> {
    if user_count(&state.db).await? == 0 {
        return Ok(Redirect::to("/setup").into_response());
    }
    render(LoginTemplate {
        error: String::new(),
        username: trim(query.username.as_deref().unwrap_or_default()),
        next: local_target(query.next.as_deref().unwrap_or("/")).to_string(),
        adding: !browser_sessions(&state.db, &headers).await?.is_empty(),
    })
    .map(IntoResponse::into_response)
}

/// Checks the username and password, then starts a session and returns to the
/// page the visitor asked for. A failure re-renders the form with one message
/// for an unknown user and a wrong password alike. Accounts the browser is
/// already signed in to stay signed in behind the new one.
pub(crate) async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<LoginForm>,
) -> Result<Response> {
    let next = local_target(&form.next).to_string();
    let username = trim(&form.username);
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE username = ?")
            .bind(&username)
            .fetch_optional(&state.db)
            .await?;
    let (user_id, stored_hash) = row.unzip();
    // An unknown user is checked against the dummy hash, so both failures
    // cost the same Argon2 work.
    let verified = check_password(form.password, stored_hash).await;
    let Some(user_id) = user_id.filter(|_| verified) else {
        return render(LoginTemplate {
            error: "Username or password is incorrect.".into(),
            username,
            next,
            adding: !browser_sessions(&state.db, &headers).await?.is_empty(),
        })
        .map(IntoResponse::into_response);
    };
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(stamp())
        .execute(&state.db)
        .await?;
    let cookies = sign_in_browser(&state.db, &headers, &user_id).await?;
    Ok((cookies, Redirect::to(&next)).into_response())
}

/// Ends the active account's session. When the browser is signed in to
/// another account, that one takes over and the library opens; otherwise both
/// cookies are cleared and the sign-in page opens.
pub(crate) async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let sessions = browser_sessions(&state.db, &headers).await?;
    let ending = sessions.first().map(|session| session.token.as_str());
    if let Some(token) = session_token(&headers) {
        end_session(&state.db, token).await?;
    }
    if let Some(token) = ending {
        end_session(&state.db, token).await?;
    }
    let others: Vec<&str> = sessions
        .iter()
        .skip(1)
        .map(|session| session.token.as_str())
        .collect();
    match others.split_first() {
        Some((next, rest)) => Ok((browser_cookies(next, rest)?, Redirect::to("/")).into_response()),
        None => Ok((cleared_cookies(), Redirect::to("/login")).into_response()),
    }
}

/// Ends every session this browser holds and clears both cookies.
pub(crate) async fn logout_all(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    for token in session_token(&headers)
        .into_iter()
        .chain(remembered_tokens(&headers))
    {
        end_session(&state.db, token).await?;
    }
    Ok((cleared_cookies(), Redirect::to("/login")).into_response())
}

pub(crate) async fn reset_password_page(_user: AuthUser) -> Result<Html<String>> {
    render(ResetPasswordTemplate {
        error: String::new(),
    })
}

/// Verifies the current password against the stored hash, then replaces it
/// with the new one. The change ends every session of the user, and this
/// browser gets a new one. Failures re-render the form with an error,
/// mirroring `setup_create`.
pub(crate) async fn reset_password(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    axum::Form(form): axum::Form<ResetPasswordForm>,
) -> Result<Response> {
    let stored_hash: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
            .bind(&user.id)
            .fetch_optional(&state.db)
            .await?;
    let Some(stored_hash) = stored_hash else {
        return Err(AppError::NotFound);
    };
    if !check_password(form.current_password.clone(), Some(stored_hash)).await {
        return render(ResetPasswordTemplate {
            error: "Current password is incorrect.".into(),
        })
        .map(IntoResponse::into_response);
    }
    match check_new_password(
        &form.new_password,
        &form.confirm_password,
        "New password is required.",
    ) {
        Ok(()) => {}
        Err(AppError::BadRequest(error)) => {
            return render(ResetPasswordTemplate { error }).map(IntoResponse::into_response);
        }
        Err(error) => return Err(error),
    }
    let password_hash = hash_off_runtime(form.new_password).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(password_hash)
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    let cookie = create_session(&mut *tx, &user.id).await?;
    tx.commit().await?;
    Ok((
        AppendHeaders([
            (header::SET_COOKIE, cookie),
            (
                header::SET_COOKIE,
                crate::htmx::flash_cookie("password_changed"),
            ),
        ]),
        Redirect::to("/settings#settings-account"),
    )
        .into_response())
}

/// Admits a request with a live session cookie, puts its user into the
/// request extensions as `AuthUser`, and serves the request with the account
/// menu set for `base.html`. Without an account the visitor goes to `/setup`;
/// without a session, to `/login`. When the active session has ended but the
/// browser is signed in to another account, that account becomes the active
/// one. Remembered sessions that have ended are dropped from the accounts
/// cookie.
pub(crate) async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response> {
    let sessions = browser_sessions(&state.db, req.headers()).await?;
    let Some(active) = sessions.first() else {
        if user_count(&state.db).await? == 0 {
            return Ok(crate::htmx::full_page_redirect(req.headers(), "/setup"));
        }
        return Ok(login_redirect(&req));
    };
    let others: Vec<&str> = sessions[1..]
        .iter()
        .map(|session| session.token.as_str())
        .collect();
    let mut cookies = Vec::new();
    let promoted = session_token(req.headers()) != Some(active.token.as_str());
    if promoted || !extended_recently(&active.expires_at) {
        extend_session(&state.db, &active.token).await?;
        cookies.push(session_cookie(&active.token)?);
    }
    if others != remembered_tokens(req.headers()) {
        cookies.push(accounts_cookie(&others)?);
    }
    let menu = load_account_menu(&state.db, &active.user_id, &sessions).await?;
    req.extensions_mut().insert(AuthUser {
        id: active.user_id.clone(),
    });
    let mut response = ACCOUNT_MENU.scope(Arc::new(menu), next.run(req)).await;
    for cookie in cookies {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(response)
}
