use crate::{
    AppError, AppState, ResetPasswordForm, ResetPasswordTemplate, Result, SetupForm, SetupTemplate,
    render, stamp, trim,
};
use argon2::{Argon2, PasswordHasher, PasswordVerifier, password_hash::PasswordHash};
use axum::{
    extract::{FromRequestParts, Request, State},
    http::{StatusCode, header, request::Parts},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
};
use base64::Engine;
use sqlx::SqlitePool;
use std::sync::{Arc, OnceLock};
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
fn unknown_user_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| hash_password("kindle-recipes-unknown-user").unwrap_or_default())
}

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
            verify_password(&password, unknown_user_hash());
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

pub(crate) async fn user_count(db: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await?)
}

pub(crate) async fn create_user(
    db: &SqlitePool,
    username: &str,
    password: &str,
    confirm: &str,
) -> Result<()> {
    let username = trim(username);
    if username.is_empty() {
        return Err(AppError::BadRequest("Username is required.".into()));
    }
    if username.contains(':') {
        return Err(AppError::BadRequest(
            "Username cannot contain a colon.".into(),
        ));
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
    Ok(())
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
        Ok(()) => Ok(Redirect::to("/").into_response()),
        Err(AppError::BadRequest(error)) => render(SetupTemplate {
            error,
            username: trim(&form.username),
        })
        .map(IntoResponse::into_response),
        Err(error) => Err(error),
    }
}

pub(crate) async fn reset_password_page(_user: AuthUser) -> Result<Html<String>> {
    render(ResetPasswordTemplate {
        error: String::new(),
    })
}

/// Verifies the current password against the stored hash, then replaces it
/// with the new one. Failures re-render the form with an error, mirroring
/// `setup_create`.
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
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(password_hash)
        .bind(&user.id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/settings?notice=password_changed").into_response())
}

fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"Recipe Repository\"")],
        Html(
            "<!doctype html><main style=\"font:18px Georgia;margin:2rem\"><h1>Sign in required</h1><p>This library is protected. Enter the username and password from setup.</p></main>",
        ),
    )
        .into_response()
}

/// Username and password from an `Authorization: Basic` header, or `None` when
/// the header is absent or malformed.
fn basic_credentials(req: &Request) -> Option<(String, String)> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

pub(crate) async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> std::result::Result<Response, Response> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await
        .map_err(|_| AppError::Internal("user lookup failed".into()).into_response())?;
    if count == 0 {
        return Ok(Redirect::to("/setup").into_response());
    }
    let Some((username, password)) = basic_credentials(&req) else {
        return Ok(challenge());
    };
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE username = ?")
            .bind(&username)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| AppError::Internal("user lookup failed".into()).into_response())?;
    let authenticated = match row {
        Some((id, stored_hash)) => check_password(password, Some(stored_hash))
            .await
            .then_some(id),
        None => {
            check_password(password, None).await;
            None
        }
    };
    match authenticated {
        Some(id) => {
            req.extensions_mut().insert(AuthUser { id });
            Ok(next.run(req).await)
        }
        None => Ok(challenge()),
    }
}
