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
use std::sync::Arc;
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

pub(crate) async fn user_count(db: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await?)
}

pub(crate) async fn create_user(db: &SqlitePool, username: &str, password: &str) -> Result<()> {
    let username = trim(username);
    if username.is_empty() {
        return Err(AppError::BadRequest("Username is required.".into()));
    }
    if username.contains(':') {
        return Err(AppError::BadRequest(
            "Username cannot contain a colon.".into(),
        ));
    }
    if password.is_empty() {
        return Err(AppError::BadRequest("Password is required.".into()));
    }
    let password_hash = hash_password(password)?;
    let user_id = Uuid::new_v4().to_string();
    let mut tx = db.begin().await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&mut *tx)
        .await?;
    if count > 0 {
        return Err(AppError::BadRequest("A user already exists.".into()));
    }
    sqlx::query("INSERT INTO users(id,username,password_hash,created_at) VALUES(?,?,?,?)")
        .bind(&user_id)
        .bind(&username)
        .bind(password_hash)
        .bind(stamp())
        .execute(&mut *tx)
        .await?;
    for table in ["recipes", "ai_drafts", "pi_credentials"] {
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
    match create_user(&state.db, &form.username, &form.password).await {
        Ok(()) => Ok(Redirect::to("/").into_response()),
        Err(AppError::BadRequest(error)) => render(SetupTemplate {
            error,
            username: trim(&form.username),
        })
        .map(IntoResponse::into_response),
        Err(error) => Err(error),
    }
}

pub(crate) async fn reset_password_page(
    State(_state): State<Arc<AppState>>,
    _user: AuthUser,
) -> Result<Html<String>> {
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
    if !verify_password(&form.current_password, &stored_hash) {
        return render(ResetPasswordTemplate {
            error: "Current password is incorrect.".into(),
        })
        .map(IntoResponse::into_response);
    }
    let new_password = trim(&form.new_password);
    if new_password.is_empty() {
        return render(ResetPasswordTemplate {
            error: "New password is required.".into(),
        })
        .map(IntoResponse::into_response);
    }
    let password_hash = hash_password(&new_password)?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(password_hash)
        .bind(&user.id)
        .execute(&state.db)
        .await?;
    Ok(Redirect::to("/settings").into_response())
}

fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"Kindle Recipes\"")],
        Html(
            "<!doctype html><main style=\"font:18px Georgia;margin:2rem\"><h1>Sign in required</h1><p>This library is protected. Enter the username and password from setup.</p></main>",
        ),
    )
        .into_response()
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
    let Some(header_value) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(challenge());
    };
    let Some(encoded) = header_value.strip_prefix("Basic ") else {
        return Ok(challenge());
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return Ok(challenge());
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return Ok(challenge());
    };
    let Some((username, password)) = decoded.split_once(':') else {
        return Ok(challenge());
    };
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| AppError::Internal("user lookup failed".into()).into_response())?;
    match row {
        Some((id, stored_hash)) if verify_password(password, &stored_hash) => {
            req.extensions_mut().insert(AuthUser { id });
            Ok(next.run(req).await)
        }
        _ => Ok(challenge()),
    }
}
