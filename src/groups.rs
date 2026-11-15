//! Groups: a household of accounts that share the recipes they choose and,
//! member by member, their AI logins. An account is in at most one group. A
//! group does not exist until an invite is accepted: the first accepted
//! invite creates it with the inviter and the invitee, any member can invite
//! more, and the last but one member to leave dissolves it.
//!
//! Invites go out by email through Resend when `RESEND_API_KEY` is set;
//! without it (self-hosting) the inviter is shown the link to send
//! themselves. The link carries a random token of which the database keeps
//! only the SHA-256; the address is kept only masked and hashed, and only the
//! masked form is ever logged or rendered.
//!
//! The SQL on `account_groups`, `group_members`, `group_invites`, and
//! `group_invite_sends` lives here; the AI grants are read and written by
//! the AI-setup accessors in settings.rs, recipe queries in recipes.rs.

use crate::auth::AuthUser;
use crate::htmx::{Flash, flash_redirect};
use crate::{AppError, AppState, InviteTemplate, Result, SetupForm, render, stamp, trim};
use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, header},
    response::{AppendHeaders, Html, IntoResponse, Redirect, Response},
};
use base64::Engine;
use chrono::{Duration, Utc};
use rand_core::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

/// An invite link works for this many days.
const INVITE_DAYS: i64 = 7;
/// Most invites one account may send in 24 hours: mail goes out from the
/// operator's domain, so a runaway account must not burn its reputation.
pub(crate) const MAX_INVITES_PER_DAY: i64 = 20;
/// The sender when `RESEND_FROM` is unset.
pub(crate) const DEFAULT_FROM: &str = "Recipe Repository <invites@mail.reciperepository.app>";
/// Resend gets this long to accept a message before the inviter is shown
/// the link instead.
const MAIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How invites reach people: the mailer, and the public address links in
/// the mail start with. Read from the environment in `main()`.
#[derive(Clone, Default)]
pub(crate) struct Invites {
    pub(crate) mailer: Mailer,
    /// `PUBLIC_BASE_URL` without a trailing slash; None builds links from
    /// the request's Host header, over plain HTTP.
    pub(crate) base_url: Option<String>,
}

impl Invites {
    /// The invite setup from `RESEND_API_KEY`, `RESEND_FROM`, and
    /// `PUBLIC_BASE_URL` (None or blank when unset). The key is never logged.
    pub(crate) fn new(
        api_key: Option<String>,
        from: Option<String>,
        base_url: Option<String>,
    ) -> Self {
        let mailer = match api_key.map(|key| key.trim().to_string()) {
            Some(key) if !key.is_empty() => {
                let from = from
                    .map(|from| from.trim().to_string())
                    .filter(|from| !from.is_empty())
                    .unwrap_or_else(|| DEFAULT_FROM.to_string());
                let client = reqwest::Client::builder()
                    .timeout(MAIL_TIMEOUT)
                    .build()
                    .unwrap_or_default();
                Mailer::Resend {
                    client: resend_rs::Resend::with_client(&key, client),
                    from,
                }
            }
            _ => Mailer::Off,
        };
        Self {
            mailer,
            base_url: base_url.and_then(|url| public_base_url(&url)),
        }
    }
}

/// `PUBLIC_BASE_URL` as links use it: an http(s) URL with a host, without a
/// trailing slash. Anything else is ignored with a warning, and links fall
/// back to the request's host.
fn public_base_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed = reqwest::Url::parse(raw).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some_and(|h| !h.is_empty())
    });
    match parsed {
        Some(url) => Some(url.as_str().trim_end_matches('/').to_string()),
        None => {
            warn!("PUBLIC_BASE_URL is not an http(s) URL; invite links use the request's host");
            None
        }
    }
}

/// Sends invite mail. `Off` sends nothing (the inviter copies the link).
#[derive(Clone, Default)]
pub(crate) enum Mailer {
    #[default]
    Off,
    Resend {
        client: resend_rs::Resend,
        from: String,
    },
    /// Records every message instead of sending it.
    #[cfg(test)]
    Fake(Arc<parking_lot::Mutex<Vec<OutgoingMail>>>),
    /// Refuses every message, like a provider outage.
    #[cfg(test)]
    Failing,
}

/// One invite message.
#[derive(Clone, Debug)]
pub(crate) struct OutgoingMail {
    pub(crate) to: String,
    pub(crate) subject: String,
    pub(crate) text: String,
    pub(crate) html: String,
}

impl Mailer {
    /// Whether invites go out by email.
    pub(crate) fn enabled(&self) -> bool {
        !matches!(self, Mailer::Off)
    }

    /// Sends one message; the error is safe to log: the recipient's address
    /// is masked and the API key never appears.
    async fn send(&self, mail: OutgoingMail) -> std::result::Result<(), String> {
        match self {
            Mailer::Off => Err("mail is not configured".into()),
            Mailer::Resend { client, from } => {
                let email = resend_rs::types::CreateEmailBaseOptions::new(
                    from.as_str(),
                    [mail.to.as_str()],
                    mail.subject.as_str(),
                )
                .with_text(&mail.text)
                .with_html(&mail.html);
                client
                    .emails
                    .send(email)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string().replace(&mail.to, &mask_email(&mail.to)))
            }
            #[cfg(test)]
            Mailer::Fake(sent) => {
                sent.lock().push(mail);
                Ok(())
            }
            #[cfg(test)]
            Mailer::Failing => Err(format!("refused mail to {}", mask_email(&mail.to))),
        }
    }
}

/// An address as it may be shown or logged: the first character of the
/// local part, then `***@` and the domain, e.g. `j***@example.com`.
pub(crate) fn mask_email(address: &str) -> String {
    match address.rsplit_once('@') {
        Some((local, domain)) => {
            let first: String = local.chars().take(1).collect();
            format!("{first}***@{domain}")
        }
        None => "***".into(),
    }
}

/// Whether `address` looks like a deliverable email address: one line, at
/// most 254 bytes, a local part of at most 64 bytes without leading,
/// trailing, or doubled dots, and a dotted domain of letters, digits, and
/// hyphens (non-ASCII allowed for international addresses).
pub(crate) fn valid_email(address: &str) -> bool {
    if address.len() > 254 || address.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((local, domain)) = address.rsplit_once('@') else {
        return false;
    };
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local.chars().all(|c| {
            !c.is_ascii() || c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c)
        });
    let labels: Vec<&str> = domain.split('.').collect();
    let domain_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| !c.is_ascii() || c.is_ascii_alphanumeric() || c == '-')
        });
    local_ok && domain_ok
}

fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// SHA-256 of an invite token, as stored in `group_invites.token_hash`.
pub(crate) fn invite_token_hash(token: &str) -> String {
    sha256_hex(token)
}

/// A new invite token: 32 random bytes in unpadded base64url.
fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Escapes text for an HTML mail body.
fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// The invite message for one address.
fn invite_mail(to: &str, inviter: &str, link: &str) -> OutgoingMail {
    let text = format!(
        "{inviter} invited you to join their group on Recipe Repository. Members of a group can open the recipes each other shares, and can share their AI logins.\n\nAccept or decline the invite:\n{link}\n\nThe link works for {INVITE_DAYS} days. If you did not expect this invite, ignore this email."
    );
    let inviter_html = escape_html(inviter);
    let link_html = escape_html(link);
    let html = format!(
        "<p>{inviter_html} invited you to join their group on Recipe Repository. Members of a group can open the recipes each other shares, and can share their AI logins.</p>\
         <p><a href=\"{link_html}\">Accept or decline the invite</a></p>\
         <p>The link works for {INVITE_DAYS} days. If you did not expect this invite, ignore this email.</p>"
    );
    OutgoingMail {
        to: to.to_string(),
        subject: format!("{inviter} invited you to their recipe group"),
        text,
        html,
    }
}

/// Where invite links point: `PUBLIC_BASE_URL`, else the host the inviter's
/// browser asked for, over plain HTTP (the LAN default).
fn link_base(invites: &Invites, headers: &HeaderMap) -> String {
    if let Some(base) = &invites.base_url {
        return base.clone();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .filter(|host| {
            !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        })
        .unwrap_or("localhost");
    format!("http://{host}")
}

/// The group `user_id` belongs to, if any.
pub(crate) async fn group_of<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    user_id: &str,
) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT group_id FROM group_members WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(conn)
            .await?,
    )
}

/// One member of a group, as Settings lists it.
pub(crate) struct Member {
    pub(crate) id: String,
    pub(crate) username: String,
}

/// The members of a group, by username.
pub(crate) async fn members(db: &SqlitePool, group_id: &str) -> Result<Vec<Member>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT u.id, u.username FROM group_members m JOIN users u ON u.id = m.user_id
         WHERE m.group_id = ? ORDER BY u.username COLLATE NOCASE, u.username",
    )
    .bind(group_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, username)| Member { id, username })
        .collect())
}

/// One invite the signed-in account sent and nobody has answered yet.
pub(crate) struct PendingInvite {
    pub(crate) id: String,
    pub(crate) email: String,
    pub(crate) expires: String,
}

async fn pending_invites(db: &SqlitePool, inviter_id: &str) -> Result<Vec<PendingInvite>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, email_masked, expires_at FROM group_invites
         WHERE inviter_id = ? AND expires_at > ? ORDER BY created_at",
    )
    .bind(inviter_id)
    .bind(stamp())
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, email, expires_at)| PendingInvite {
            id,
            email,
            expires: crate::display_timestamp(&expires_at),
        })
        .collect())
}

/// A live invite found by its token.
struct Invite {
    id: String,
    inviter_id: String,
    inviter: String,
}

async fn find_invite<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    token: &str,
) -> Result<Option<Invite>> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT i.id, i.inviter_id, u.username FROM group_invites i
         JOIN users u ON u.id = i.inviter_id
         WHERE i.token_hash = ? AND i.expires_at > ?",
    )
    .bind(invite_token_hash(token))
    .bind(stamp())
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(id, inviter_id, inviter)| Invite {
        id,
        inviter_id,
        inviter,
    }))
}

/// Deletes expired invites and the send log older than a day. Runs with the
/// hourly draft prune.
pub(crate) async fn prune_invites(db: &SqlitePool) -> Result<u64> {
    let removed = sqlx::query("DELETE FROM group_invites WHERE expires_at <= ?")
        .bind(stamp())
        .execute(db)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM group_invite_sends WHERE sent_at <= ?")
        .bind((Utc::now() - Duration::hours(24)).to_rfc3339())
        .execute(db)
        .await?;
    Ok(removed)
}

/// Takes `user_id` out of its group inside the caller's transaction: its AI
/// grants to and from other members go, its recipes stop being shared, and a
/// group left with one member is dissolved (that member's recipes stop being
/// shared too, so a later group does not see them unasked). Used by Leave
/// and by account deletion.
pub(crate) async fn leave_group_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
) -> Result<()> {
    let Some(group_id) = group_of(&mut **tx, user_id).await? else {
        return Ok(());
    };
    for statement in [
        "DELETE FROM ai_grants WHERE granter_id = ?1 OR grantee_id = ?1",
        "DELETE FROM group_members WHERE user_id = ?1",
        "UPDATE recipes SET shared = 0 WHERE user_id = ?1 AND shared <> 0",
    ] {
        sqlx::query(statement)
            .bind(user_id)
            .execute(&mut **tx)
            .await?;
    }
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_members WHERE group_id = ?")
        .bind(&group_id)
        .fetch_one(&mut **tx)
        .await?;
    if left <= 1 {
        sqlx::query(
            "UPDATE recipes SET shared = 0
             WHERE shared <> 0 AND user_id IN (SELECT user_id FROM group_members WHERE group_id = ?)",
        )
        .bind(&group_id)
        .execute(&mut **tx)
        .await?;
        // Cascades to the last membership and any grant left in the group.
        sqlx::query("DELETE FROM account_groups WHERE id = ?")
            .bind(&group_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// What accepting an invite did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Acceptance {
    /// The account joined the inviter's group (created now if needed).
    Joined,
    /// The account was already in the inviter's group; the invite is used up.
    AlreadyMember,
    /// The invite is unknown, used, or expired.
    Gone,
    /// The account sent this invite itself.
    OwnInvite,
    /// The account is in another group and must leave it first.
    OtherGroup,
}

/// Accepts the invite behind `token` for `user_id`, in one transaction: the
/// account joins the inviter's group, which the first accepted invite
/// creates with the inviter in it, and the invite is consumed.
pub(crate) async fn accept(db: &SqlitePool, token: &str, user_id: &str) -> Result<Acceptance> {
    let mut tx = crate::recipes::begin_write(db).await?;
    let Some(invite) = find_invite(&mut *tx, token).await? else {
        return Ok(Acceptance::Gone);
    };
    if invite.inviter_id == user_id {
        return Ok(Acceptance::OwnInvite);
    }
    let theirs = group_of(&mut *tx, &invite.inviter_id).await?;
    let mine = group_of(&mut *tx, user_id).await?;
    let outcome = match (&mine, &theirs) {
        (Some(mine), Some(theirs)) if mine == theirs => Acceptance::AlreadyMember,
        (Some(_), _) => return Ok(Acceptance::OtherGroup),
        (None, _) => Acceptance::Joined,
    };
    if outcome == Acceptance::Joined {
        let now = stamp();
        let group_id = match theirs {
            Some(group_id) => group_id,
            None => {
                let group_id = Uuid::new_v4().to_string();
                sqlx::query("INSERT INTO account_groups (id, created_at) VALUES (?, ?)")
                    .bind(&group_id)
                    .bind(&now)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "INSERT INTO group_members (user_id, group_id, joined_at) VALUES (?, ?, ?)",
                )
                .bind(&invite.inviter_id)
                .bind(&group_id)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                group_id
            }
        };
        sqlx::query("INSERT INTO group_members (user_id, group_id, joined_at) VALUES (?, ?, ?)")
            .bind(user_id)
            .bind(&group_id)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM group_invites WHERE id = ?")
        .bind(&invite.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    info!(%user_id, inviter = %invite.inviter_id, ?outcome, "Group invite accepted");
    Ok(outcome)
}

/// What the Group block shows besides the stored state: the link of an
/// invite just made without mail, and a refusal of the invite form.
#[derive(Default)]
pub(crate) struct GroupExtras {
    pub(crate) invite_link: String,
    pub(crate) invite_to: String,
    pub(crate) error: String,
}

/// A member in the AI-sharing lists of the Group block.
pub(crate) struct GrantChoice {
    pub(crate) id: String,
    pub(crate) username: String,
    /// Granted (in the list of members this account shares with), or in
    /// use (in the list of members who share with this account).
    pub(crate) checked: bool,
}

/// The Group block in Settings.
pub(crate) struct GroupPanel {
    /// Every member, this account included; empty outside a group.
    pub(crate) members: Vec<Member>,
    pub(crate) user_id: String,
    pub(crate) invites: Vec<PendingInvite>,
    pub(crate) mail_enabled: bool,
    pub(crate) notice: &'static str,
    pub(crate) extras: GroupExtras,
    /// This account has an AI setup of its own, so it can share it.
    pub(crate) can_grant: bool,
    /// The other members, ticked when this account shares its AI with them.
    pub(crate) grants: Vec<GrantChoice>,
    /// Members who share their AI with this account, ticked on the one in use.
    pub(crate) received: Vec<GrantChoice>,
    /// This account generates with its own AI setup.
    pub(crate) uses_own_ai: bool,
}

impl GroupPanel {
    pub(crate) fn in_group(&self) -> bool {
        !self.members.is_empty()
    }

    /// Whether the AI-sharing form has anything to offer: other members, and
    /// an AI setup to share or grants to take back.
    pub(crate) fn shows_grant_form(&self) -> bool {
        !self.grants.is_empty() && (self.can_grant || self.grants.iter().any(|g| g.checked))
    }
}

/// Builds the Group block for `user_id`.
pub(crate) async fn panel(
    state: &AppState,
    user_id: &str,
    flash: &Flash,
    extras: GroupExtras,
) -> Result<GroupPanel> {
    let db = &state.db;
    let members = match group_of(db, user_id).await? {
        Some(group_id) => members(db, &group_id).await?,
        None => Vec::new(),
    };
    let granted = crate::ai_grantees(db, user_id).await?;
    let grants = members
        .iter()
        .filter(|member| member.id != user_id)
        .map(|member| GrantChoice {
            id: member.id.clone(),
            username: member.username.clone(),
            checked: granted.contains(&member.id),
        })
        .collect();
    let ai_user = crate::ai_account(db, user_id).await?;
    let granters = crate::ai_grants_received(db, user_id).await?;
    // The radio marks the member in use; on one's own logins (or when the
    // one in use is not among them), the saved pick, else the earliest.
    let picked = crate::app_setting(db, user_id, crate::AI_SOURCE_SETTING).await?;
    let marked = [Some(ai_user.clone()), picked]
        .into_iter()
        .flatten()
        .find(|id| granters.iter().any(|(granter, _)| granter == id))
        .or_else(|| granters.first().map(|(id, _)| id.clone()))
        .unwrap_or_default();
    let received = granters
        .into_iter()
        .map(|(id, username)| GrantChoice {
            checked: id == marked,
            id,
            username,
        })
        .collect();
    Ok(GroupPanel {
        user_id: user_id.to_string(),
        invites: pending_invites(db, user_id).await?,
        mail_enabled: state.invites.mailer.enabled(),
        notice: match flash.0.as_str() {
            "group_joined" => "You joined the group.",
            "group_already" => "You are already in this group.",
            "group_left" => "You left the group.",
            "invite_cancelled" => "Invite cancelled.",
            "grants_saved" => "AI sharing saved.",
            "ai_source_saved" => "Saved.",
            "account_created" => "Account created.",
            _ => "",
        },
        extras,
        can_grant: crate::own_ai_configured(db, user_id).await?,
        grants,
        received,
        uses_own_ai: ai_user == user_id,
        members,
    })
}

#[derive(Deserialize)]
pub(crate) struct InviteForm {
    #[serde(default)]
    email: String,
}

/// `POST /settings/group/invite`: invites an address to the signed-in
/// account's group (or to make one with it). The invite is stored first;
/// then it is emailed, or, without mail (or when sending fails), the
/// Settings page shows the link for the inviter to send. Answers with the
/// Settings page itself, since the link can be shown only once.
pub(crate) async fn send_invite(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    headers: HeaderMap,
    flash: Flash,
    Form(form): Form<InviteForm>,
) -> Result<Html<String>> {
    let address = trim(&form.email);
    let refuse = |error: &str| GroupExtras {
        error: error.to_string(),
        ..GroupExtras::default()
    };
    if !valid_email(&address) {
        let extras = refuse("Enter an email address like name@example.com.");
        return crate::settings_view(&state, &user, &flash, extras).await;
    }
    let since = (Utc::now() - Duration::hours(24)).to_rfc3339();
    let sent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM group_invite_sends WHERE user_id = ? AND sent_at > ?",
    )
    .bind(&user.id)
    .bind(&since)
    .fetch_one(&state.db)
    .await?;
    if sent >= MAX_INVITES_PER_DAY {
        warn!(user_id = %user.id, "Group invite refused: daily limit reached");
        let extras = refuse(&format!(
            "You have sent {MAX_INVITES_PER_DAY} invites today. Try again tomorrow."
        ));
        return crate::settings_view(&state, &user, &flash, extras).await;
    }
    let inviter: String = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
        .bind(&user.id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let masked = mask_email(&address);
    let email_hash = sha256_hex(&address.to_lowercase());
    let token = new_token();
    let now = Utc::now();
    let mut tx = crate::recipes::begin_write(&state.db).await?;
    // Inviting the same address again replaces the earlier link.
    sqlx::query("DELETE FROM group_invites WHERE inviter_id = ? AND email_hash = ?")
        .bind(&user.id)
        .bind(&email_hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO group_invites (id, token_hash, inviter_id, email_hash, email_masked, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(invite_token_hash(&token))
    .bind(&user.id)
    .bind(&email_hash)
    .bind(&masked)
    .bind(now.to_rfc3339())
    .bind((now + Duration::days(INVITE_DAYS)).to_rfc3339())
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO group_invite_sends (id, user_id, sent_at) VALUES (?, ?, ?)")
        .bind(Uuid::new_v4().to_string())
        .bind(&user.id)
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let link = format!("{}/invites/{token}", link_base(&state.invites, &headers));
    let mut extras = GroupExtras {
        invite_to: masked.clone(),
        ..GroupExtras::default()
    };
    if state.invites.mailer.enabled() {
        match state
            .invites
            .mailer
            .send(invite_mail(&address, &inviter, &link))
            .await
        {
            Ok(()) => info!(user_id = %user.id, to = %masked, "Group invite emailed"),
            Err(error) => {
                warn!(user_id = %user.id, to = %masked, %error, "Group invite email failed");
                extras.error =
                    "The invite email could not be sent. Copy the link below and send it yourself."
                        .into();
                extras.invite_link = link;
            }
        }
    } else {
        info!(user_id = %user.id, to = %masked, "Group invite created without mail");
        extras.invite_link = link;
    }
    crate::settings_view(&state, &user, &flash, extras).await
}

/// `POST /settings/group/invites/{id}/cancel`: withdraws one of the signed-in
/// account's pending invites; its link stops working.
pub(crate) async fn cancel_invite(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response> {
    let removed = sqlx::query("DELETE FROM group_invites WHERE id = ? AND inviter_id = ?")
        .bind(&id)
        .bind(&user.id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if removed == 0 {
        return Err(AppError::NotFound);
    }
    Ok(flash_redirect(
        "/settings#settings-group",
        "invite_cancelled",
    ))
}

/// `POST /settings/group/leave`: the signed-in account leaves its group.
pub(crate) async fn leave(State(state): State<Arc<AppState>>, user: AuthUser) -> Result<Response> {
    let mut tx = crate::recipes::begin_write(&state.db).await?;
    leave_group_in(&mut tx, &user.id).await?;
    tx.commit().await?;
    info!(user_id = %user.id, "Left group");
    Ok(flash_redirect("/settings#settings-group", "group_left"))
}

/// `POST /settings/group/ai-grants`: the members the signed-in account lets
/// use its AI logins, one `grantee` field per ticked member. Sharing with a
/// new member needs an AI setup of one's own; taking a grant back never does.
pub(crate) async fn update_ai_grants(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response> {
    let group_id = group_of(&state.db, &user.id)
        .await?
        .ok_or_else(|| AppError::BadRequest("Join a group to share your AI logins.".into()))?;
    let member_ids: Vec<String> = members(&state.db, &group_id)
        .await?
        .into_iter()
        .map(|member| member.id)
        .filter(|id| *id != user.id)
        .collect();
    let mut grantees: Vec<String> = Vec::new();
    for (name, value) in fields {
        if name != "grantee" || grantees.contains(&value) {
            continue;
        }
        if !member_ids.contains(&value) {
            return Err(AppError::BadRequest("Choose members of your group.".into()));
        }
        grantees.push(value);
    }
    let existing = crate::ai_grantees(&state.db, &user.id).await?;
    let adds = grantees.iter().any(|id| !existing.contains(id));
    if adds && !crate::own_ai_configured(&state.db, &user.id).await? {
        return Err(AppError::BadRequest(
            "Connect an AI provider of your own before sharing it.".into(),
        ));
    }
    crate::set_ai_grants(&state.db, &group_id, &user.id, &grantees).await?;
    Ok(flash_redirect("/settings#settings-group", "grants_saved"))
}

#[derive(Deserialize)]
pub(crate) struct AiSourceForm {
    #[serde(default)]
    granter: String,
}

/// `POST /settings/group/ai-source`: which member's shared AI logins the
/// signed-in account uses when several share theirs with it.
pub(crate) async fn update_ai_source(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<AiSourceForm>,
) -> Result<Response> {
    let granted = crate::ai_grants_received(&state.db, &user.id).await?;
    if !granted.iter().any(|(id, _)| *id == form.granter) {
        return Err(AppError::BadRequest(
            "Choose a member who shares their AI logins with you.".into(),
        ));
    }
    crate::set_app_setting(&state.db, &user.id, crate::AI_SOURCE_SETTING, &form.granter).await?;
    Ok(flash_redirect(
        "/settings#settings-group",
        "ai_source_saved",
    ))
}

/// The members of `user_id`'s group whose generations run on its AI logins
/// right now, by username: the ones the delete-account page warns about.
pub(crate) async fn ai_dependents(db: &SqlitePool, user_id: &str) -> Result<Vec<String>> {
    let Some(group_id) = group_of(db, user_id).await? else {
        return Ok(Vec::new());
    };
    let mut dependents = Vec::new();
    for member in members(db, &group_id).await? {
        if member.id != user_id && crate::ai_account(db, &member.id).await? == user_id {
            dependents.push(member.username);
        }
    }
    Ok(dependents)
}

/// The invite page's states.
pub(crate) const INVITE_GONE: &str = "gone";
pub(crate) const INVITE_SIGNED_OUT: &str = "signed_out";
pub(crate) const INVITE_ACCEPT: &str = "accept";
pub(crate) const INVITE_OWN: &str = "own";
pub(crate) const INVITE_MEMBER: &str = "member";
pub(crate) const INVITE_OTHER_GROUP: &str = "other_group";
pub(crate) const INVITE_DECLINED: &str = "declined";

/// The invite page for `token` as the browser's active account sees it.
async fn invite_view(
    state: &AppState,
    headers: &HeaderMap,
    token: &str,
    error: String,
    username: String,
) -> Result<InviteTemplate> {
    let mut page = InviteTemplate {
        token: token.to_string(),
        inviter: String::new(),
        state: INVITE_GONE,
        viewer: String::new(),
        error,
        username,
    };
    let Some(invite) = find_invite(&state.db, token).await? else {
        return Ok(page);
    };
    page.inviter = invite.inviter;
    let Some(viewer_id) = crate::auth::signed_in_user(&state.db, headers).await? else {
        page.state = INVITE_SIGNED_OUT;
        return Ok(page);
    };
    page.viewer = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
        .bind(&viewer_id)
        .fetch_optional(&state.db)
        .await?
        .unwrap_or_default();
    let mine = group_of(&state.db, &viewer_id).await?;
    let theirs = group_of(&state.db, &invite.inviter_id).await?;
    page.state = if viewer_id == invite.inviter_id {
        INVITE_OWN
    } else if mine.is_some() && mine == theirs {
        INVITE_MEMBER
    } else if mine.is_some() {
        INVITE_OTHER_GROUP
    } else {
        INVITE_ACCEPT
    };
    Ok(page)
}

/// `GET /invites/{token}`: outside the sign-in wall. A signed-in browser
/// gets Accept and Decline; a signed-out one gets Sign in, or Create an
/// account through the invite (the one way in for someone without an
/// account), and Decline.
pub(crate) async fn invite_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Html<String>> {
    render(invite_view(&state, &headers, &token, String::new(), String::new()).await?)
}

/// `POST /invites/{token}/accept`: the signed-in account joins the
/// inviter's group.
pub(crate) async fn accept_invite(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Response> {
    match accept(&state.db, &token, &user.id).await? {
        Acceptance::Joined => Ok(flash_redirect("/settings#settings-group", "group_joined")),
        Acceptance::AlreadyMember => Ok(flash_redirect("/settings#settings-group", "group_already")),
        Acceptance::OwnInvite => Err(AppError::BadRequest(
            "This is your own invite. Send the link to the person you invited.".into(),
        )),
        Acceptance::OtherGroup => Err(AppError::BadRequest(
            "You are already in a group. Leave it in Settings first, then open the invite link again."
                .into(),
        )),
        Acceptance::Gone => Ok(render(
            invite_view(&state, &headers, &token, String::new(), String::new()).await?,
        )?
        .into_response()),
    }
}

/// `POST /invites/{token}/decline`: anyone holding the link may decline it,
/// signed in or not; the invite is consumed either way.
pub(crate) async fn decline_invite(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Result<Html<String>> {
    let declined = sqlx::query("DELETE FROM group_invites WHERE token_hash = ? AND expires_at > ?")
        .bind(invite_token_hash(&token))
        .bind(stamp())
        .execute(&state.db)
        .await?
        .rows_affected();
    render(InviteTemplate {
        token: String::new(),
        inviter: String::new(),
        state: if declined > 0 {
            INVITE_DECLINED
        } else {
            INVITE_GONE
        },
        viewer: String::new(),
        error: String::new(),
        username: String::new(),
    })
}

/// `POST /invites/{token}/signup`: creates an account through a live invite
/// (sign-up is otherwise closed to signed-out visitors; the invite is the
/// ticket), signs the browser in to it, and joins the inviter's group.
pub(crate) async fn signup_through_invite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(form): Form<SetupForm>,
) -> Result<Response> {
    if find_invite(&state.db, &token).await?.is_none() {
        return Ok(render(
            invite_view(&state, &headers, &token, String::new(), String::new()).await?,
        )?
        .into_response());
    }
    let user_id = match crate::auth::add_user(
        &state.db,
        &form.username,
        &form.password,
        &form.confirm_password,
    )
    .await
    {
        Ok(user_id) => user_id,
        Err(AppError::BadRequest(error)) => {
            let page = invite_view(&state, &headers, &token, error, trim(&form.username)).await?;
            return Ok(render(page)?.into_response());
        }
        Err(error) => return Err(error),
    };
    info!(%user_id, "Account created through a group invite");
    let outcome = accept(&state.db, &token, &user_id).await?;
    let cookies = crate::auth::sign_in_browser(&state.db, &headers, &user_id).await?;
    let key = if outcome == Acceptance::Joined {
        "group_joined"
    } else {
        "account_created"
    };
    // One AppendHeaders for all three: a plain header array would replace
    // the session cookies instead of adding to them.
    let [session, accounts] = cookies.0;
    Ok((
        AppendHeaders([
            session,
            accounts,
            (header::SET_COOKIE, crate::htmx::flash_cookie(key)),
        ]),
        Redirect::to("/settings#settings-group"),
    )
        .into_response())
}

#[derive(Deserialize)]
pub(crate) struct ShareForm {
    #[serde(default)]
    shared: String,
}

/// `POST /recipes/{id}/share`: the owner shares a recipe with their group,
/// or stops sharing it. Members can then open it read-only. Not an edit: it
/// keeps `updated_at` and records no version, like a folder pin.
pub(crate) async fn update_share(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<ShareForm>,
) -> Result<Response> {
    let shared = form.shared == "on";
    if shared && group_of(&state.db, &user.id).await?.is_none() {
        return Err(AppError::BadRequest(
            "Join a group in Settings to share recipes.".into(),
        ));
    }
    crate::recipes::set_recipe_shared(&state.db, &user.id, &id, shared).await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}
