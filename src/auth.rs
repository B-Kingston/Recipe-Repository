use crate::{
    AppError, AppState, LoginForm, LoginQuery, LoginTemplate, ResetPasswordForm,
    ResetPasswordTemplate, Result, SetupForm, SetupTemplate, render, stamp, trim,
};
use argon2::{Argon2, PasswordHasher, PasswordVerifier, password_hash::PasswordHash};
use axum::{
    extract::{FromRequestParts, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, header, request::Parts},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
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

/// Name of the cookie that carries the session token.
const SESSION_COOKIE: &str = "recipe_session";
/// A session ends after this many days without a request.
const SESSION_DAYS: i64 = 30;
/// The middleware extends a session at most once in this many hours, so most
/// requests only read the `sessions` table.
const SESSION_REFRESH_HOURS: i64 = 24;
/// `Set-Cookie` value that removes the session cookie from the browser.
const CLEARED_SESSION_COOKIE: &str = "recipe_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax";

/// SHA-256 of a session token, as stored in `sessions.token_hash`. The table
/// never holds the token itself.
fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn session_cookie(token: &str) -> Result<HeaderValue> {
    let max_age = SESSION_DAYS * 24 * 60 * 60;
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax"
    ))
    .map_err(|_| AppError::Internal("session cookie is not a valid header".into()))
}

/// Starts a session for the user and returns the `Set-Cookie` value that
/// carries its token to the browser.
async fn create_session(db: impl sqlx::SqliteExecutor<'_>, user_id: &str) -> Result<HeaderValue> {
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
    session_cookie(&token)
}

/// The session token from the request's `Cookie` headers, or `None` when the
/// browser sent none.
fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == SESSION_COOKIE && !value.is_empty()).then_some(value)
        })
}

/// Resolves the request's session cookie to a user id. A session that was
/// extended in the last SESSION_REFRESH_HOURS is used as it is. An older one
/// gets a new expiry, and the returned cookie gives the browser the new
/// lifetime.
async fn resume_session(
    db: &SqlitePool,
    headers: &HeaderMap,
) -> Result<Option<(String, Option<HeaderValue>)>> {
    let Some(token) = session_token(headers) else {
        return Ok(None);
    };
    let hash = token_hash(token);
    let now = Utc::now();
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT user_id, expires_at FROM sessions WHERE token_hash = ? AND expires_at > ?",
    )
    .bind(&hash)
    .bind(now.to_rfc3339())
    .fetch_optional(db)
    .await?;
    let Some((user_id, expires_at)) = row else {
        return Ok(None);
    };
    let full_expiry = now + Duration::days(SESSION_DAYS);
    let extended_recently = DateTime::parse_from_rfc3339(&expires_at).is_ok_and(|expires| {
        expires.with_timezone(&Utc) > full_expiry - Duration::hours(SESSION_REFRESH_HOURS)
    });
    if extended_recently {
        return Ok(Some((user_id, None)));
    }
    sqlx::query("UPDATE sessions SET expires_at = ? WHERE token_hash = ?")
        .bind(full_expiry.to_rfc3339())
        .bind(&hash)
        .execute(db)
        .await?;
    Ok(Some((user_id, Some(session_cookie(token)?))))
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
/// library after sign-in, because their form data is gone.
fn login_redirect(req: &Request) -> Response {
    let target = req
        .uri()
        .path_and_query()
        .map_or("/", |target| target.as_str());
    if !matches!(*req.method(), Method::GET | Method::HEAD) || target == "/" {
        return Redirect::to("/login").into_response();
    }
    Redirect::to(&format!("/login?next={}", encode_query_value(target))).into_response()
}

pub(crate) async fn user_count(db: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await?)
}

/// Creates the single account and returns its id.
pub(crate) async fn create_user(
    db: &SqlitePool,
    username: &str,
    password: &str,
    confirm: &str,
) -> Result<String> {
    let username = trim(username);
    if username.is_empty() {
        return Err(AppError::BadRequest("Username is required.".into()));
    }
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
    for table in ["recipes", "ai_drafts", "pi_credentials", "ai_endpoints"] {
        let query = format!("UPDATE {table} SET user_id=? WHERE user_id='' ");
        sqlx::query(&query).bind(&user_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
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

pub(crate) async fn login_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LoginQuery>,
) -> Result<Response> {
    if user_count(&state.db).await? == 0 {
        return Ok(Redirect::to("/setup").into_response());
    }
    render(LoginTemplate {
        error: String::new(),
        username: String::new(),
        next: local_target(query.next.as_deref().unwrap_or("/")).to_string(),
    })
    .map(IntoResponse::into_response)
}

/// Checks the username and password, then starts a session and returns to the
/// page the visitor asked for. A failure re-renders the form with one message
/// for an unknown user and a wrong password alike.
pub(crate) async fn login(
    State(state): State<Arc<AppState>>,
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
        })
        .map(IntoResponse::into_response);
    };
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(stamp())
        .execute(&state.db)
        .await?;
    let cookie = create_session(&state.db, &user_id).await?;
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to(&next)).into_response())
}

/// Ends the browser's session and clears its cookie.
pub(crate) async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    if let Some(token) = session_token(&headers) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash(token))
            .execute(&state.db)
            .await?;
    }
    Ok((
        [(
            header::SET_COOKIE,
            HeaderValue::from_static(CLEARED_SESSION_COOKIE),
        )],
        Redirect::to("/login"),
    )
        .into_response())
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
        [(header::SET_COOKIE, cookie)],
        Redirect::to("/settings?notice=password_changed"),
    )
        .into_response())
}

/// Admits a request with a live session cookie and puts its user into the
/// request extensions as `AuthUser`. Without an account the visitor goes to
/// `/setup`; without a session, to `/login`.
pub(crate) async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response> {
    let Some((id, cookie)) = resume_session(&state.db, req.headers()).await? else {
        if user_count(&state.db).await? == 0 {
            return Ok(Redirect::to("/setup").into_response());
        }
        return Ok(login_redirect(&req));
    };
    req.extensions_mut().insert(AuthUser { id });
    let mut response = next.run(req).await;
    if let Some(cookie) = cookie {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(response)
}
