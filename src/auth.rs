use crate::htmx::{cookie_values, set_cookie, sets_cookie};
use crate::{
    AppError, AppState, DeleteAccountTemplate, LoginForm, LoginQuery, LoginTemplate,
    NewAccountTemplate, ResetPasswordForm, ResetPasswordTemplate, Result, SetupForm, SetupTemplate,
    SwitchAccountForm, render, stamp, trim,
};
use argon2::{Argon2, PasswordHasher, PasswordVerifier, password_hash::PasswordHash};
use axum::{
    extract::{ConnectInfo, FromRequestParts, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header, request::Parts},
    middleware::Next,
    response::{AppendHeaders, Html, IntoResponse, Redirect, Response},
};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{
    collections::{HashMap, VecDeque},
    fmt::Write,
    net::{IpAddr, SocketAddr},
    sync::{Arc, LazyLock},
    time::{Duration as StdDuration, Instant},
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

/// What a brute-force limit counts. Each kind has its own allowance and
/// window, and its own keys: an address, a username, or a user id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Limit {
    /// Failed sign-ins from one address.
    LoginAddress,
    /// Failed sign-ins to one username, known or not, letter case ignored.
    LoginUsername,
    /// Wrong current passwords on the password page, per user.
    Password,
    /// Accounts created, or refused as a taken username, from one address.
    NewAccount,
}

impl Limit {
    /// Most hits a key may gather inside the window.
    pub(crate) fn allowance(self) -> usize {
        match self {
            Self::LoginAddress => 30,
            Self::LoginUsername | Self::Password | Self::NewAccount => 10,
        }
    }

    /// The sliding window a key's hits are counted over.
    pub(crate) fn window(self) -> StdDuration {
        StdDuration::from_secs(match self {
            Self::LoginAddress | Self::LoginUsername | Self::Password => 15 * 60,
            Self::NewAccount => 60 * 60,
        })
    }
}

/// Most keys the limiter holds at once. A new key past the cap first drops
/// every key whose hits have all left their window, then the key hit least
/// recently.
pub(crate) const MAX_LIMIT_KEYS: usize = 10_000;
/// Usernames are keyed by at most this many characters: no username is
/// longer, so a longer one signs in nowhere, and a huge one makes no huge key.
const MAX_LIMIT_KEY_CHARS: usize = 64;

pub(crate) type LimitKey = (Limit, String);

/// The limiter's key for a username typed at sign-in: trimmed and lower-cased
/// in ASCII, the way SQLite's `NOCASE` matches it to an account.
pub(crate) fn username_key(username: &str) -> LimitKey {
    let name: String = username.trim().chars().take(MAX_LIMIT_KEY_CHARS).collect();
    (Limit::LoginUsername, name.to_ascii_lowercase())
}

/// In-memory brute-force limits for sign-in, the password page, and account
/// creation, shared by every request of this process. Each key keeps the
/// times of its recent hits, never more than its allowance, so a key frees
/// up as its oldest hit leaves the window. A restart forgets them all.
pub(crate) struct RateLimits {
    /// Take the client address from the left-most `X-Forwarded-For` entry
    /// (`TRUST_PROXY_HEADERS`) instead of the connection's peer address.
    trust_proxy_headers: bool,
    hits: parking_lot::Mutex<HashMap<LimitKey, VecDeque<Instant>>>,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self::new(false)
    }
}

impl RateLimits {
    pub(crate) fn new(trust_proxy_headers: bool) -> Self {
        Self {
            trust_proxy_headers,
            hits: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Records one hit on every key, unless one of them is already at its
    /// allowance: then nothing is recorded and the longest wait comes back.
    /// The check and the hits are one step under the lock, so requests sent
    /// at once cannot slip past the allowance together.
    pub(crate) fn attempt(&self, keys: &[LimitKey]) -> std::result::Result<(), StdDuration> {
        self.attempt_at(keys, Instant::now())
    }

    pub(crate) fn attempt_at(
        &self,
        keys: &[LimitKey],
        now: Instant,
    ) -> std::result::Result<(), StdDuration> {
        let mut hits = self.hits.lock();
        let wait = keys
            .iter()
            .filter_map(|key| {
                hits.get_mut(key)
                    .and_then(|times| wait_for(key.0, times, now))
            })
            .max();
        if let Some(wait) = wait {
            return Err(wait);
        }
        for key in keys {
            hit(&mut hits, key.clone(), now);
        }
        Ok(())
    }

    /// The wait before `key` may take another hit, or `None` when it may.
    pub(crate) fn blocked(&self, key: &LimitKey) -> Option<StdDuration> {
        let mut hits = self.hits.lock();
        wait_for(key.0, hits.get_mut(key)?, Instant::now())
    }

    /// Records one hit on `key`, for a limit checked with `blocked` before
    /// the work and counted only by how it ended.
    pub(crate) fn record(&self, key: LimitKey) {
        hit(&mut self.hits.lock(), key, Instant::now());
    }

    /// Takes back the newest hit on `key`, for an attempt that succeeded.
    pub(crate) fn forgive(&self, key: &LimitKey) {
        let mut hits = self.hits.lock();
        if let Some(times) = hits.get_mut(key) {
            times.pop_back();
            if times.is_empty() {
                hits.remove(key);
            }
        }
    }

    /// Forgets every hit on `key`.
    pub(crate) fn clear(&self, key: &LimitKey) {
        self.hits.lock().remove(key);
    }

    #[cfg(test)]
    pub(crate) fn key_count(&self) -> usize {
        self.hits.lock().len()
    }
}

/// Drops the hits that have left the window.
fn expire(limit: Limit, times: &mut VecDeque<Instant>, now: Instant) {
    while times
        .front()
        .is_some_and(|oldest| now.duration_since(*oldest) >= limit.window())
    {
        times.pop_front();
    }
}

/// How long until a key at its allowance takes a hit again; `None` when it
/// is below its allowance.
fn wait_for(limit: Limit, times: &mut VecDeque<Instant>, now: Instant) -> Option<StdDuration> {
    expire(limit, times, now);
    if times.len() < limit.allowance() {
        return None;
    }
    let oldest = times.front()?;
    Some(limit.window().saturating_sub(now.duration_since(*oldest)))
}

/// Adds one hit to `key`, keeping no more than its allowance, and makes room
/// first when a new key would pass MAX_LIMIT_KEYS.
fn hit(hits: &mut HashMap<LimitKey, VecDeque<Instant>>, key: LimitKey, now: Instant) {
    if !hits.contains_key(&key) && hits.len() >= MAX_LIMIT_KEYS {
        hits.retain(|key, times| {
            expire(key.0, times, now);
            !times.is_empty()
        });
        if hits.len() >= MAX_LIMIT_KEYS {
            let stalest = hits
                .iter()
                .min_by_key(|(_, times)| times.back().copied())
                .map(|(key, _)| key.clone());
            if let Some(stalest) = stalest {
                hits.remove(&stalest);
            }
        }
    }
    let limit = key.0;
    let times = hits.entry(key).or_default();
    expire(limit, times, now);
    times.push_back(now);
    while times.len() > limit.allowance() {
        times.pop_front();
    }
}

/// "Try again in N minutes.", rounded up, for a limited request's message.
pub(crate) fn try_again(wait: StdDuration) -> String {
    let minutes = wait.as_secs().div_ceil(60).max(1);
    let unit = if minutes == 1 { "minute" } else { "minutes" };
    format!("Try again in {minutes} {unit}.")
}

/// The client's address for the per-address limits: the connection's peer
/// address, or with `TRUST_PROXY_HEADERS` the left-most `X-Forwarded-For`
/// entry when it parses. IPv6 addresses count per /64, the block one client
/// usually holds. `None` when neither is known (a router served without
/// connect info, as in tests): the per-address limits are then skipped, and
/// the per-username and per-user ones still apply.
pub(crate) struct ClientAddress(pub(crate) Option<String>);

impl FromRequestParts<Arc<AppState>> for ClientAddress {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        let forwarded = if state.rate_limits.trust_proxy_headers {
            forwarded_for(&parts.headers)
        } else {
            None
        };
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip());
        Ok(Self(forwarded.or(peer).map(address_bucket)))
    }
}

/// The left-most address in `X-Forwarded-For`, the client as the first proxy
/// saw it. The proxy in front must set the header itself rather than pass on
/// one the client sent.
fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    let value = headers.get("x-forwarded-for")?.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    first
        .parse::<IpAddr>()
        .ok()
        .or_else(|| first.parse::<SocketAddr>().ok().map(|address| address.ip()))
}

/// The limiter's key for an address: an IPv4 address as written (an
/// IPv4-mapped IPv6 one too), or an IPv6 address's /64.
pub(crate) fn address_bucket(address: IpAddr) -> String {
    match address {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let [a, b, c, d, ..] = v6.segments();
                format!("{a:x}:{b:x}:{c:x}:{d:x}::/64")
            }
        },
    }
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

/// A new account's username, trimmed: required, one line of printable text,
/// and short enough for the account menu.
fn check_new_username(username: &str) -> Result<String> {
    let username = trim(username);
    if username.is_empty() {
        return Err(AppError::BadRequest("Username is required.".into()));
    }
    if username.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "Username must be on one line, without tabs or control characters.".into(),
        ));
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

/// Two `Set-Cookie` headers: the session cookie, then the accounts cookie.
pub(crate) type BrowserCookies = AppendHeaders<[(HeaderName, HeaderValue); 2]>;

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

/// A session cookie lasts as long as the session it carries.
const SESSION_COOKIE_MAX_AGE: i64 = SESSION_DAYS * 24 * 60 * 60;

fn session_cookie(token: &str) -> Result<HeaderValue> {
    set_cookie(SESSION_COOKIE, token, SESSION_COOKIE_MAX_AGE)
}

/// `Set-Cookie` value that remembers the browser's other signed-in accounts,
/// or removes the cookie when there are none.
fn accounts_cookie(tokens: &[&str]) -> Result<HeaderValue> {
    if tokens.is_empty() {
        return set_cookie(ACCOUNTS_COOKIE, "", 0);
    }
    set_cookie(ACCOUNTS_COOKIE, &tokens.join("."), SESSION_COOKIE_MAX_AGE)
}

/// Makes `active` the browser's session and remembers `others` behind it.
fn browser_cookies(active: &str, others: &[&str]) -> Result<BrowserCookies> {
    Ok(AppendHeaders([
        (header::SET_COOKIE, session_cookie(active)?),
        (header::SET_COOKIE, accounts_cookie(others)?),
    ]))
}

/// Signs the browser out of every account.
fn cleared_cookies() -> Result<BrowserCookies> {
    Ok(AppendHeaders([
        (header::SET_COOKIE, set_cookie(SESSION_COOKIE, "", 0)?),
        (header::SET_COOKIE, accounts_cookie(&[])?),
    ]))
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
fn cookie_value<'a>(headers: &'a HeaderMap, name: &'a str) -> Option<&'a str> {
    cookie_values(headers, name).find(|value| !value.is_empty())
}

/// The active session's token, if the browser sent one.
fn session_token(headers: &HeaderMap) -> Option<&str> {
    cookie_value(headers, SESSION_COOKIE)
}

/// Tokens of the browser's other signed-in accounts in the order the cookie
/// lists them, without duplicates or anything that cannot be a token, and no
/// more than fit behind the active one.
fn remembered_tokens(headers: &HeaderMap) -> Vec<&str> {
    let mut tokens: Vec<&str> = Vec::new();
    for token in cookie_value(headers, ACCOUNTS_COOKIE)
        .unwrap_or_default()
        .split('.')
    {
        if is_token(token) && !tokens.contains(&token) && tokens.len() + 1 < MAX_BROWSER_ACCOUNTS {
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

/// The browser's active account, for pages outside the sign-in wall (a
/// group invite link): the user of its first live session, if any.
pub(crate) async fn signed_in_user(db: &SqlitePool, headers: &HeaderMap) -> Result<Option<String>> {
    Ok(browser_sessions(db, headers)
        .await?
        .into_iter()
        .next()
        .map(|session| session.user_id))
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
pub(crate) async fn sign_in_browser(
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

/// Another account this browser is signed in to, in the header's account
/// menu; choosing it switches at once.
pub(crate) struct MenuAccount {
    pub(crate) id: String,
    pub(crate) username: String,
    pub(crate) initial: String,
}

/// The header's account menu: the signed-in account and the other accounts
/// this browser is signed in to. Accounts the browser holds no session for
/// never appear, so the menu does not list who else uses the server.
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
    // Only the browser's own sessions (at most MAX_BROWSER_ACCOUNTS), so
    // the placeholders are few.
    let placeholders = vec!["?"; sessions.len()].join(",");
    let query = format!(
        "SELECT id, username FROM users WHERE id IN ({placeholders})
         ORDER BY username COLLATE NOCASE, username"
    );
    let mut users = sqlx::query_as::<_, (String, String)>(&query);
    for session in sessions {
        users = users.bind(&session.user_id);
    }
    let mut username = String::new();
    let mut others = Vec::new();
    for (id, name) in users.fetch_all(db).await? {
        if id == user_id {
            username = name;
            continue;
        }
        others.push(MenuAccount {
            initial: initial(&name),
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
    let mut tx = crate::recipes::begin_write(db).await?;
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

/// The refusal of a username another account has, letter case ignored.
const USERNAME_TAKEN: &str = "That username is taken.";

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
        return Err(AppError::BadRequest(USERNAME_TAKEN.into()));
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
///
/// Each client address may create Limit::NewAccount accounts an hour; a
/// username refused as taken counts too, so the form cannot test usernames
/// at will. Past that the form comes back with 429 before any hashing.
pub(crate) async fn create_account(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
    ClientAddress(address): ClientAddress,
    headers: HeaderMap,
    axum::Form(form): axum::Form<SetupForm>,
) -> Result<Response> {
    let address_key = address.map(|address| (Limit::NewAccount, address));
    let again = |status: StatusCode, error: String| -> Result<Response> {
        let page = render(NewAccountTemplate {
            error,
            username: trim(&form.username),
        })?;
        Ok((status, page).into_response())
    };
    if let Some(wait) = address_key
        .as_ref()
        .and_then(|key| state.rate_limits.blocked(key))
    {
        let error = AppError::TooManyRequests(format!(
            "Too many accounts were created from this network. {}",
            try_again(wait)
        ));
        return again(StatusCode::TOO_MANY_REQUESTS, error.user_message());
    }
    let created = add_user(
        &state.db,
        &form.username,
        &form.password,
        &form.confirm_password,
    )
    .await;
    if let Some(address_key) = address_key
        && is_counted(&created)
    {
        state.rate_limits.record(address_key);
    }
    match created {
        Ok(user_id) => {
            let cookies = sign_in_browser(&state.db, &headers, &user_id).await?;
            Ok((cookies, Redirect::to("/")).into_response())
        }
        Err(AppError::BadRequest(error)) => again(StatusCode::OK, error),
        Err(error) => Err(error),
    }
}

/// Whether an account-creation outcome counts against Limit::NewAccount: a
/// new account, or a username refused as taken.
fn is_counted(created: &Result<String>) -> bool {
    match created {
        Ok(_) => true,
        Err(AppError::BadRequest(error)) => error == USERNAME_TAKEN,
        Err(_) => false,
    }
}

/// Makes another account this browser is signed in to the active one and
/// opens its library. Any other id goes to the sign-in page, whether or not
/// the account exists, so the form tells nothing about other accounts.
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
        return Ok(Redirect::to("/login").into_response());
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
    flash: crate::htmx::Flash,
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
        notice: if flash.is("account_deleted") {
            "Account deleted."
        } else {
            ""
        },
    })
    .map(IntoResponse::into_response)
}

/// The sign-in form again, with `error` under it and the given status.
async fn login_again(
    state: &AppState,
    headers: &HeaderMap,
    status: StatusCode,
    error: String,
    username: String,
    next: String,
) -> Result<Response> {
    let page = render(LoginTemplate {
        error,
        username,
        next,
        adding: !browser_sessions(&state.db, headers).await?.is_empty(),
        notice: "",
    })?;
    Ok((status, page).into_response())
}

/// Checks the username and password, then starts a session and returns to the
/// page the visitor asked for. A failure re-renders the form with one message
/// for an unknown user and a wrong password alike. Accounts the browser is
/// already signed in to stay signed in behind the new one.
///
/// Every attempt first takes a hit on the client address and the username
/// (`RateLimits::attempt`); one at its allowance answers 429 with the form
/// before the database or Argon2 is touched, the same for a username that
/// exists and one that does not. A success forgets the username's failures
/// and takes back its own hit on the address, whose other failures stay.
pub(crate) async fn login(
    State(state): State<Arc<AppState>>,
    ClientAddress(address): ClientAddress,
    headers: HeaderMap,
    axum::Form(form): axum::Form<LoginForm>,
) -> Result<Response> {
    let next = local_target(&form.next).to_string();
    let username = trim(&form.username);
    let username_key = username_key(&username);
    let address_key = address.map(|address| (Limit::LoginAddress, address));
    let keys: Vec<LimitKey> = address_key
        .iter()
        .cloned()
        .chain([username_key.clone()])
        .collect();
    if let Err(wait) = state.rate_limits.attempt(&keys) {
        tracing::warn!(address = ?address_key.map(|key| key.1), "Sign-in attempts limited");
        let error =
            AppError::TooManyRequests(format!("Too many sign-in attempts. {}", try_again(wait)));
        return login_again(
            &state,
            &headers,
            StatusCode::TOO_MANY_REQUESTS,
            error.user_message(),
            username,
            next,
        )
        .await;
    }
    // Letter case is ignored, as it is when a username is taken, so "alice"
    // signs in to the account set up as "Alice".
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE username = ? COLLATE NOCASE")
            .bind(&username)
            .fetch_optional(&state.db)
            .await?;
    let (user_id, stored_hash) = row.unzip();
    // An unknown user is checked against the dummy hash, so both failures
    // cost the same Argon2 work.
    let verified = check_password(form.password, stored_hash).await;
    let Some(user_id) = user_id.filter(|_| verified) else {
        return login_again(
            &state,
            &headers,
            StatusCode::OK,
            "Username or password is incorrect.".into(),
            username,
            next,
        )
        .await;
    };
    state.rate_limits.clear(&username_key);
    if let Some(address_key) = &address_key {
        state.rate_limits.forgive(address_key);
    }
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(stamp())
        .execute(&state.db)
        .await?;
    let cookies = sign_in_browser(&state.db, &headers, &user_id).await?;
    Ok((cookies, Redirect::to(&next)).into_response())
}

/// Ends the active account's session. When the browser is signed in to
/// another account, that one takes over and the library opens; otherwise both
/// cookies are cleared and the sign-in page opens. Only the session in the
/// session cookie ends: when it has already ended elsewhere (a new password,
/// a deleted account), the remembered accounts all stay signed in.
pub(crate) async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let sessions = browser_sessions(&state.db, &headers).await?;
    let ending = session_token(&headers);
    if let Some(token) = ending {
        end_session(&state.db, token).await?;
    }
    let others: Vec<&str> = sessions
        .iter()
        .map(|session| session.token.as_str())
        .filter(|token| Some(*token) != ending)
        .collect();
    match others.split_first() {
        Some((next, rest)) => Ok((browser_cookies(next, rest)?, Redirect::to("/")).into_response()),
        None => Ok((cleared_cookies()?, Redirect::to("/login")).into_response()),
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
    Ok((cleared_cookies()?, Redirect::to("/login")).into_response())
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
///
/// Each check takes a hit on the user's Limit::Password key first; at its
/// allowance the form comes back with 429 and Argon2 never runs. The right
/// current password forgets the user's earlier misses.
pub(crate) async fn reset_password(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    axum::Form(form): axum::Form<ResetPasswordForm>,
) -> Result<Response> {
    let limit_key = (Limit::Password, user.id.clone());
    if let Err(wait) = state.rate_limits.attempt(std::slice::from_ref(&limit_key)) {
        tracing::warn!(user_id = %user.id, "Password checks limited");
        let error =
            AppError::TooManyRequests(format!("Too many password attempts. {}", try_again(wait)));
        let page = render(ResetPasswordTemplate {
            error: error.user_message(),
        })?;
        return Ok((StatusCode::TOO_MANY_REQUESTS, page).into_response());
    }
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
    state.rate_limits.clear(&limit_key);
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
    let mut tx = crate::recipes::begin_write(&state.db).await?;
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

/// The confirmation page for deleting the signed-in account. It names the
/// group members whose generations run on this account's AI logins, which
/// they lose with it.
pub(crate) async fn delete_account_page(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Html<String>> {
    let username: String = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
        .bind(&user.id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let sharing = crate::groups::ai_dependents(&state.db, &user.id).await?;
    render(DeleteAccountTemplate { username, sharing })
}

/// Deletes everything `user_id` owns and the account itself, in one
/// transaction. Tables keyed by `user_id` without a foreign key to `users`
/// are cleared explicitly; the rest go by cascade from `recipes`,
/// `ai_drafts`, and `users`. Returns the ids of the deleted endpoints and
/// recipes, so their in-memory state can be dropped.
pub(crate) async fn delete_user(
    db: &SqlitePool,
    user_id: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut tx = crate::recipes::begin_write(db).await?;
    let endpoints: Vec<String> =
        sqlx::query_scalar("SELECT id FROM ai_endpoints WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&mut *tx)
            .await?;
    let recipes: Vec<String> = sqlx::query_scalar("SELECT id FROM recipes WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
    // Out of its group first: grants go, and a group left with one member
    // is dissolved.
    crate::groups::leave_group_in(&mut tx, user_id).await?;
    // Every statement binds the id once as ?1; never mix ? and ?N.
    for statement in [
        // No foreign keys: thumbnail candidates of the recipes and drafts.
        "DELETE FROM recipe_thumbnail_candidates
         WHERE recipe_id IN (SELECT id FROM recipes WHERE user_id = ?1)",
        "DELETE FROM draft_thumbnails WHERE draft_id IN (SELECT id FROM ai_drafts WHERE user_id = ?1)",
        // Cascades to draft_moment_vectors, draft_moment_stills, and
        // draft_step_moments.
        "DELETE FROM ai_drafts WHERE user_id = ?1",
        "DELETE FROM recipe_notes WHERE user_id = ?1",
        "DELETE FROM recipe_chat_messages WHERE user_id = ?1",
        // Cascades to recipe_blocks (and so recipe_step_ingredients and
        // recipe_step_moments), recipe_sources, recipe_versions,
        // recipe_version_counters, recipe_embeddings, recipe_moment_vectors,
        // recipe_moment_stills, recipe_chat_messages, and recipe_notes.
        "DELETE FROM recipes WHERE user_id = ?1",
        "DELETE FROM app_settings WHERE user_id = ?1",
        "DELETE FROM ai_endpoints WHERE user_id = ?1",
        "DELETE FROM pi_credentials WHERE user_id = ?1",
        "DELETE FROM sessions WHERE user_id = ?1",
        // Cascades to every other table keyed to the user by a foreign key
        // (group_invites and group_invite_sends among them).
        "DELETE FROM users WHERE id = ?1",
    ] {
        sqlx::query(statement)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((endpoints, recipes))
}

/// Deletes the signed-in (active) account and everything it owns, after the
/// confirmation page. Every session of the account ends, on every browser.
/// This browser falls back to its next remembered account and opens the
/// library; with none, both cookies are cleared and the sign-in page opens
/// (which goes on to `/setup` when no account is left).
pub(crate) async fn delete_account(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    headers: HeaderMap,
) -> Result<Response> {
    let sessions = browser_sessions(&state.db, &headers).await?;
    let (endpoints, recipes) = delete_user(&state.db, &user.id).await?;
    for endpoint in &endpoints {
        crate::ai::forget_model_catalogue(&state, endpoint);
    }
    {
        let mut thumbnail_jobs = state.thumbnail_jobs.lock();
        let mut moment_jobs = state.moment_jobs.lock();
        for recipe in &recipes {
            thumbnail_jobs.remove(recipe);
            moment_jobs.remove(recipe);
        }
    }
    state
        .codex_flows
        .lock()
        .retain(|_, flow| flow.user_id != user.id);
    state
        .claude_flows
        .lock()
        .retain(|_, flow| flow.user_id != user.id);
    crate::ai::forget_media_runs(&state.media_debug_runs, &user.id);
    tracing::info!(user_id = %user.id, recipes = recipes.len(), "Account deleted");
    let others: Vec<&str> = sessions
        .iter()
        .filter(|session| session.user_id != user.id)
        .map(|session| session.token.as_str())
        .collect();
    let flash = crate::htmx::flash_cookie("account_deleted");
    match others.split_first() {
        Some((next, rest)) => {
            let [session, accounts] = browser_cookies(next, rest)?.0;
            Ok((
                AppendHeaders([session, accounts, (header::SET_COOKIE, flash)]),
                Redirect::to("/"),
            )
                .into_response())
        }
        None => {
            let [session, accounts] = cleared_cookies()?.0;
            Ok((
                AppendHeaders([session, accounts, (header::SET_COOKIE, flash)]),
                Redirect::to("/login"),
            )
                .into_response())
        }
    }
}

/// Refuses a state-changing request another site made the browser send.
/// The session cookie is `SameSite=Lax`, which still lets a page on the same
/// host under another port (a different origin but the same site) post a form
/// here with it, so every route checks the request's own origin, the way Go's
/// `CrossOriginProtection` does: `Sec-Fetch-Site` when the browser sends it,
/// else `Origin` against `Host`. A request with neither (an old browser, a
/// script) is let through; it carries no proof of a forgery.
pub(crate) async fn same_origin_only(req: Request, next: Next) -> Result<Response> {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && !same_origin(req.headers()) {
        tracing::warn!(
            method = %req.method(),
            path = %req.uri().path(),
            "Cross-origin request refused"
        );
        return Err(AppError::CrossOrigin);
    }
    Ok(next.run(req).await)
}

/// True unless the headers show the request came from another origin.
/// `none` is a request the user started (a bookmark, the address bar).
fn same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return matches!(site.as_bytes(), b"same-origin" | b"none");
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    // Behind a TLS proxy the app sees plain HTTP, so only the authority is
    // compared; the proxy passes the browser's `Host` through.
    let authority = origin.to_str().ok().and_then(|origin| {
        origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
    });
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    matches!((authority, host), (Some(authority), Some(host)) if authority.eq_ignore_ascii_case(host))
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
        cookies.push((SESSION_COOKIE, session_cookie(&active.token)?));
    }
    if others != remembered_tokens(req.headers()) {
        cookies.push((ACCOUNTS_COOKIE, accounts_cookie(&others)?));
    }
    let menu = load_account_menu(&state.db, &active.user_id, &sessions).await?;
    req.extensions_mut().insert(AuthUser {
        id: active.user_id.clone(),
    });
    let mut response = ACCOUNT_MENU.scope(Arc::new(menu), next.run(req)).await;
    // A handler that sets a session cookie itself (switching accounts, a new
    // password, a deleted account) decides the browser's sessions. Browsers
    // apply Set-Cookie headers in order, so appending this request's view
    // after it would undo the handler's.
    for (name, cookie) in cookies {
        if !sets_cookie(response.headers(), name) {
            response.headers_mut().append(header::SET_COOKIE, cookie);
        }
    }
    Ok(response)
}
