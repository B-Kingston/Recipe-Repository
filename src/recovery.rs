//! Account recovery by email: an optional recovery address per account,
//! proved by a link sent to it (or by a group invite sent to it), and
//! "Forgot password?" links sent only to a proved address. All of it exists
//! only while `Mail::recovery_base` is set (Resend on and `PUBLIC_BASE_URL`
//! set); without it there is no email field and `/forgot` is a 404.
//!
//! A link carries a random token of which `email_tokens` keeps only the
//! SHA-256. A verification link works once within VERIFY_HOURS, a reset link
//! once within RESET_MINUTES, and a new link replaces the account's earlier
//! one of its kind. A new address waits in its verification token; the
//! verified one in `users` stays in force until the link is followed.
//!
//! The SQL on `users.email`, `users.email_verified_at`, and `email_tokens`
//! lives here. Addresses are logged masked; tokens are never logged.

use crate::auth::{
    AuthUser, ClientAddress, Limit, check_new_password, check_password, hash_off_runtime,
    new_token, sign_in_browser, token_hash, try_again, username_key,
};
use crate::htmx::{flash_cookie, flash_redirect};
use crate::mail::{OutgoingMail, escape_html, mask_email, valid_email};
use crate::{AppError, AppState, EmailTemplate, RecoveryTemplate, Result, render, stamp, trim};
use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{AppendHeaders, Html, IntoResponse, Redirect, Response},
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{info, warn};

/// A verification link works for this many hours.
const VERIFY_HOURS: i64 = 24;
/// A reset link works for this many minutes.
const RESET_MINUTES: i64 = 60;
/// `email_tokens.purpose` values.
const VERIFY: &str = "verify";
const RESET: &str = "reset";

/// The refusal of an address another account has verified.
const ADDRESS_TAKEN: &str = "Another account already uses this email address.";

/// The recovery page's states (`RecoveryTemplate::state`).
const PAGE_FORGOT: &str = "forgot";
const PAGE_SENT: &str = "sent";
const PAGE_RESET: &str = "reset";
const PAGE_RESET_EXPIRED: &str = "reset_expired";
const PAGE_VERIFY: &str = "verify";
const PAGE_VERIFIED: &str = "verified";
const PAGE_VERIFY_EXPIRED: &str = "verify_expired";

fn page(state: &'static str, token: &str, error: String) -> Result<Html<String>> {
    render(RecoveryTemplate {
        state,
        token: token.to_string(),
        error,
    })
}

/// The recovery address as Settings shows it to its owner.
pub(crate) struct EmailStatus {
    /// The verified address, or empty.
    pub(crate) verified: String,
    /// An address waiting for its verification link, or empty.
    pub(crate) pending: String,
}

pub(crate) async fn email_status(db: &SqlitePool, user_id: &str) -> Result<EmailStatus> {
    let verified: Option<Option<String>> = sqlx::query_scalar(
        "SELECT email FROM users WHERE id = ? AND email_verified_at IS NOT NULL",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    let pending: Option<Option<String>> = sqlx::query_scalar(
        "SELECT email FROM email_tokens WHERE user_id = ? AND purpose = 'verify' AND expires_at > ?",
    )
    .bind(user_id)
    .bind(stamp())
    .fetch_optional(db)
    .await?;
    Ok(EmailStatus {
        verified: verified.flatten().unwrap_or_default(),
        pending: pending.flatten().unwrap_or_default(),
    })
}

/// Stores a new link for `user_id` and returns its token, replacing the
/// account's earlier link of the same purpose.
async fn issue_token(
    db: &SqlitePool,
    user_id: &str,
    purpose: &str,
    email: Option<&str>,
    lifetime: Duration,
) -> Result<String> {
    let token = new_token();
    let now = Utc::now();
    let mut tx = crate::recipes::begin_write(db).await?;
    sqlx::query("DELETE FROM email_tokens WHERE user_id = ? AND purpose = ?")
        .bind(user_id)
        .bind(purpose)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO email_tokens (token_hash, user_id, purpose, email, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(token_hash(&token))
    .bind(user_id)
    .bind(purpose)
    .bind(email)
    .bind(now.to_rfc3339())
    .bind((now + lifetime).to_rfc3339())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(token)
}

/// Whether `token` is a live link of `purpose`.
async fn is_live(db: &SqlitePool, token: &str, purpose: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM email_tokens WHERE token_hash = ? AND purpose = ? AND expires_at > ?)",
    )
    .bind(token_hash(token))
    .bind(purpose)
    .bind(stamp())
    .fetch_one(db)
    .await?)
}

/// Uses up the live link `token` of `purpose` and returns its account and
/// address; None when it is unknown, used, or expired.
async fn take_token<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    token: &str,
    purpose: &str,
) -> Result<Option<(String, Option<String>)>> {
    Ok(sqlx::query_as(
        "DELETE FROM email_tokens WHERE token_hash = ? AND purpose = ? AND expires_at > ?
         RETURNING user_id, email",
    )
    .bind(token_hash(token))
    .bind(purpose)
    .bind(stamp())
    .fetch_optional(conn)
    .await?)
}

/// Makes `address` the verified recovery address of `user_id`, unless
/// another account has it verified (BadRequest). The account's other links
/// end with it: a reset link went to the address this one replaces.
async fn set_verified_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    address: &str,
) -> Result<()> {
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users
         WHERE email = ?1 COLLATE NOCASE AND email_verified_at IS NOT NULL AND id <> ?2)",
    )
    .bind(address)
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    if taken {
        return Err(AppError::BadRequest(ADDRESS_TAKEN.into()));
    }
    sqlx::query("UPDATE users SET email = ?, email_verified_at = ? WHERE id = ?")
        .bind(address)
        .bind(stamp())
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM email_tokens WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn set_verified(db: &SqlitePool, user_id: &str, address: &str) -> Result<()> {
    let mut tx = crate::recipes::begin_write(db).await?;
    set_verified_in(&mut tx, user_id, address).await?;
    Ok(tx.commit().await?)
}

/// A message with one link: `lead`, the link as `action`, then `after`.
fn link_mail(
    to: &str,
    subject: &str,
    lead: &str,
    action: &str,
    link: &str,
    after: &str,
) -> OutgoingMail {
    let html = format!(
        "<p>{}</p><p><a href=\"{}\">{}</a></p><p>{}</p>",
        escape_html(lead),
        escape_html(link),
        escape_html(action),
        escape_html(after)
    );
    OutgoingMail {
        to: to.to_string(),
        subject: subject.to_string(),
        text: format!("{lead}\n\n{action}:\n{link}\n\n{after}"),
        html,
    }
}

/// Emails `address` a link that makes it `user_id`'s recovery address. At
/// most Limit::AccountMail mails an hour go out for one account
/// (TooManyRequests past that). A failed send withdraws the link and is
/// logged with the address masked (BadRequest with the reader's text).
async fn send_verification(
    state: &AppState,
    base: &str,
    user_id: &str,
    address: &str,
) -> Result<()> {
    if let Err(wait) = state
        .rate_limits
        .attempt(&[(Limit::AccountMail, user_id.to_string())])
    {
        warn!(%user_id, "Account mail limited");
        return Err(AppError::TooManyRequests(format!(
            "Too many emails were sent for this account. {}",
            try_again(wait)
        )));
    }
    let token = issue_token(
        &state.db,
        user_id,
        VERIFY,
        Some(address),
        Duration::hours(VERIFY_HOURS),
    )
    .await?;
    let mail = link_mail(
        address,
        "Verify your recovery email",
        "Use this address to recover your Recipe Repository account if you forget your password.",
        "Verify the address",
        &format!("{base}/email/verify/{token}"),
        "The link works once, for 24 hours. If you did not ask for this, ignore this email.",
    );
    let masked = mask_email(address);
    match state.mail.mailer.send(mail).await {
        Ok(()) => {
            info!(%user_id, to = %masked, "Verification link emailed");
            Ok(())
        }
        Err(error) => {
            warn!(%user_id, to = %masked, %error, "Verification email failed");
            sqlx::query("DELETE FROM email_tokens WHERE token_hash = ?")
                .bind(token_hash(&token))
                .execute(&state.db)
                .await?;
            Err(AppError::BadRequest(
                "The email could not be sent. Try again later.".into(),
            ))
        }
    }
}

/// The recovery address typed (and already checked with `valid_email`) on
/// an invite form, for the account that just joined or signed up through
/// it: verified at once when it is the address the invite went to
/// (`invite_email_hash`, see `groups::email_hash`), otherwise sent a
/// verification link as from Settings. The invite never fails over it; a
/// problem is logged.
pub(crate) async fn invite_email(
    state: &AppState,
    user_id: &str,
    address: &str,
    invite_email_hash: &str,
) {
    let Some(base) = state.mail.recovery_base() else {
        return;
    };
    if address.is_empty() {
        return;
    }
    let outcome = if crate::groups::email_hash(address) == invite_email_hash {
        set_verified(&state.db, user_id, address).await
    } else {
        send_verification(state, base, user_id, address).await
    };
    let to = mask_email(address);
    match outcome {
        Ok(()) => info!(%user_id, %to, "Recovery email set through an invite"),
        Err(error) => warn!(%user_id, %to, %error, "Recovery email from an invite not set"),
    }
}

/// `GET /settings/email`: the recovery address and the form that sets it.
pub(crate) async fn email_page(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Html<String>> {
    if state.mail.recovery_base().is_none() {
        return Err(AppError::NotFound);
    }
    render(EmailTemplate {
        status: email_status(&state.db, &user.id).await?,
        email: String::new(),
        error: String::new(),
    })
}

/// The email form again, with `error` under it and the given status.
async fn email_again(
    db: &SqlitePool,
    user_id: &str,
    status: StatusCode,
    email: String,
    error: String,
) -> Result<Response> {
    let page = render(EmailTemplate {
        status: email_status(db, user_id).await?,
        email,
        error,
    })?;
    Ok((status, page).into_response())
}

#[derive(Deserialize)]
pub(crate) struct EmailForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    current_password: String,
}

/// `POST /settings/email`: sends a verification link to a new recovery
/// address, or with the address left blank removes the account's address
/// and any pending one. Both need the current password, checked under the
/// password page's Limit::Password, so the two forms share one allowance.
pub(crate) async fn update_email(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<EmailForm>,
) -> Result<Response> {
    let Some(base) = state.mail.recovery_base() else {
        return Err(AppError::NotFound);
    };
    let address = trim(&form.email);
    let limit_key = (Limit::Password, user.id.clone());
    if let Err(wait) = state.rate_limits.attempt(std::slice::from_ref(&limit_key)) {
        warn!(user_id = %user.id, "Password checks limited");
        let error = format!("Too many password attempts. {}", try_again(wait));
        return email_again(
            &state.db,
            &user.id,
            StatusCode::TOO_MANY_REQUESTS,
            address,
            error,
        )
        .await;
    }
    let stored: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(&user.id)
        .fetch_optional(&state.db)
        .await?;
    let Some(stored) = stored else {
        return Err(AppError::NotFound);
    };
    if !check_password(form.current_password, Some(stored)).await {
        let error = "Current password is incorrect.".to_string();
        return email_again(&state.db, &user.id, StatusCode::OK, address, error).await;
    }
    state.rate_limits.clear(&limit_key);
    if address.is_empty() {
        let mut tx = crate::recipes::begin_write(&state.db).await?;
        sqlx::query("UPDATE users SET email = NULL, email_verified_at = NULL WHERE id = ?")
            .bind(&user.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM email_tokens WHERE user_id = ?")
            .bind(&user.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        info!(user_id = %user.id, "Recovery email removed");
        return Ok(flash_redirect(
            "/settings#settings-account",
            "email_removed",
        ));
    }
    if !valid_email(&address) {
        let error = "Enter an email address like name@example.com.".to_string();
        return email_again(&state.db, &user.id, StatusCode::OK, address, error).await;
    }
    match send_verification(&state, base, &user.id, &address).await {
        Ok(()) => Ok(flash_redirect("/settings#settings-account", "email_sent")),
        Err(AppError::TooManyRequests(error)) => {
            let status = StatusCode::TOO_MANY_REQUESTS;
            email_again(&state.db, &user.id, status, address, error).await
        }
        Err(AppError::BadRequest(error)) => {
            email_again(&state.db, &user.id, StatusCode::OK, address, error).await
        }
        Err(error) => Err(error),
    }
}

/// `GET /email/verify/{token}`: outside the sign-in wall, as the token names
/// the account. Verifying takes a button press, so a mail scanner that opens
/// the link does not use it up.
pub(crate) async fn verify_page(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Result<Html<String>> {
    if is_live(&state.db, &token, VERIFY).await? {
        page(PAGE_VERIFY, &token, String::new())
    } else {
        page(PAGE_VERIFY_EXPIRED, "", String::new())
    }
}

/// `POST /email/verify/{token}`: makes the link's address its account's
/// verified recovery address and uses the link up.
pub(crate) async fn verify_email(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Result<Html<String>> {
    let mut tx = crate::recipes::begin_write(&state.db).await?;
    let Some((user_id, Some(address))) = take_token(&mut *tx, &token, VERIFY).await? else {
        return page(PAGE_VERIFY_EXPIRED, "", String::new());
    };
    match set_verified_in(&mut tx, &user_id, &address).await {
        Ok(()) => {}
        // Dropping the transaction keeps the link.
        Err(AppError::BadRequest(error)) => return page(PAGE_VERIFY, &token, error),
        Err(error) => return Err(error),
    }
    tx.commit().await?;
    info!(%user_id, to = %mask_email(&address), "Recovery email verified");
    page(PAGE_VERIFIED, "", String::new())
}

/// `GET /forgot`: asks for a username or recovery address.
pub(crate) async fn forgot_page(State(state): State<Arc<AppState>>) -> Result<Html<String>> {
    if state.mail.recovery_base().is_none() {
        return Err(AppError::NotFound);
    }
    page(PAGE_FORGOT, "", String::new())
}

#[derive(Deserialize)]
pub(crate) struct ForgotForm {
    #[serde(default)]
    account: String,
}

/// `POST /forgot`: emails a reset link to the verified address of the
/// account a username or address names. The answer is the same whether or
/// not anything matched, and the mail goes out after it, so neither the page
/// nor its timing tells who has an account. Each client address may ask
/// Limit::ForgotAddress times an hour (the one refusal shown, as it is about
/// the asker); each account gets at most Limit::AccountMail mails an hour,
/// past which requests are dropped silently.
pub(crate) async fn forgot(
    State(state): State<Arc<AppState>>,
    ClientAddress(address): ClientAddress,
    Form(form): Form<ForgotForm>,
) -> Result<Response> {
    let Some(base) = state.mail.recovery_base() else {
        return Err(AppError::NotFound);
    };
    if let Some(address) = address
        && let Err(wait) = state
            .rate_limits
            .attempt(&[(Limit::ForgotAddress, address)])
    {
        warn!("Password reset requests limited");
        let error = format!("Too many requests from this network. {}", try_again(wait));
        let page = page(PAGE_FORGOT, "", error)?;
        return Ok((StatusCode::TOO_MANY_REQUESTS, page).into_response());
    }
    let target = trim(&form.account);
    let matches: Vec<(String, String, String)> = if target.is_empty() {
        Vec::new()
    } else {
        // A username may look like an address, so both columns are matched.
        sqlx::query_as(
            "SELECT id, username, email FROM users WHERE email_verified_at IS NOT NULL
             AND (username = ?1 COLLATE NOCASE OR email = ?1 COLLATE NOCASE)",
        )
        .bind(&target)
        .fetch_all(&state.db)
        .await?
    };
    for (user_id, username, email) in matches {
        if let Err(wait) = state
            .rate_limits
            .attempt(&[(Limit::AccountMail, user_id.clone())])
        {
            warn!(%user_id, wait_secs = wait.as_secs(), "Password reset mail limited");
            continue;
        }
        let token = issue_token(
            &state.db,
            &user_id,
            RESET,
            None,
            Duration::minutes(RESET_MINUTES),
        )
        .await?;
        let mail = link_mail(
            &email,
            "Reset your Recipe Repository password",
            &format!("Someone asked to reset the password of {username} on Recipe Repository."),
            "Set a new password",
            &format!("{base}/reset/{token}"),
            "The link works once, for one hour. If you did not ask for this, ignore this email; your password stays as it is.",
        );
        let mailer = state.mail.mailer.clone();
        tokio::spawn(async move {
            let to = mask_email(&mail.to);
            match mailer.send(mail).await {
                Ok(()) => info!(%user_id, %to, "Password reset link emailed"),
                Err(error) => warn!(%user_id, %to, %error, "Password reset email failed"),
            }
        });
    }
    Ok(page(PAGE_SENT, "", String::new())?.into_response())
}

/// `GET /reset/{token}`: the new-password form, or the expired page.
pub(crate) async fn reset_page(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Result<Html<String>> {
    if is_live(&state.db, &token, RESET).await? {
        page(PAGE_RESET, &token, String::new())
    } else {
        page(PAGE_RESET_EXPIRED, "", String::new())
    }
}

#[derive(Deserialize)]
pub(crate) struct NewPasswordForm {
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}

/// `POST /reset/{token}`: in one transaction replaces the password, uses the
/// link up, and ends every session of the account; then signs this browser
/// in to it (its other accounts stay signed in) and forgets the username's
/// failed sign-ins.
pub(crate) async fn reset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(form): Form<NewPasswordForm>,
) -> Result<Response> {
    if !is_live(&state.db, &token, RESET).await? {
        return Ok(page(PAGE_RESET_EXPIRED, "", String::new())?.into_response());
    }
    match check_new_password(
        &form.new_password,
        &form.confirm_password,
        "New password is required.",
    ) {
        Ok(()) => {}
        Err(AppError::BadRequest(error)) => {
            return Ok(page(PAGE_RESET, &token, error)?.into_response());
        }
        Err(error) => return Err(error),
    }
    let password_hash = hash_off_runtime(form.new_password).await?;
    let mut tx = crate::recipes::begin_write(&state.db).await?;
    let Some((user_id, _)) = take_token(&mut *tx, &token, RESET).await? else {
        return Ok(page(PAGE_RESET_EXPIRED, "", String::new())?.into_response());
    };
    let username: String =
        sqlx::query_scalar("UPDATE users SET password_hash = ? WHERE id = ? RETURNING username")
            .bind(password_hash)
            .bind(&user_id)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.rate_limits.clear(&username_key(&username));
    info!(%user_id, "Password reset through an emailed link");
    let [session, accounts] = sign_in_browser(&state.db, &headers, &user_id).await?.0;
    Ok((
        AppendHeaders([
            session,
            accounts,
            (header::SET_COOKIE, flash_cookie("password_changed")),
        ]),
        Redirect::to("/settings#settings-account"),
    )
        .into_response())
}

/// Deletes expired links. Runs with the hourly draft prune.
pub(crate) async fn prune_tokens(db: &SqlitePool) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM email_tokens WHERE expires_at <= ?")
            .bind(stamp())
            .execute(db)
            .await?
            .rows_affected(),
    )
}
