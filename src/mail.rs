//! Outgoing mail through Resend, shared by group invites (groups.rs) and
//! account recovery (recovery.rs). Off when `RESEND_API_KEY` is unset. An
//! address is only ever logged or shown to others masked (`mask_email`), and
//! the API key never leaves the client.

use axum::http::{HeaderMap, header};
#[cfg(test)]
use std::sync::Arc;
use tracing::warn;

/// The sender when `RESEND_FROM` is unset.
const DEFAULT_FROM: &str = "Recipe Repository <invites@mail.reciperepository.app>";
/// Resend gets this long to accept a message.
const MAIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The mailer, and the public address links in mail start with. Read from
/// the environment in `main()`.
#[derive(Clone, Default)]
pub(crate) struct Mail {
    pub(crate) mailer: Mailer,
    /// `PUBLIC_BASE_URL` without a trailing slash; None builds invite links
    /// from the request's Host header, over plain HTTP.
    pub(crate) base_url: Option<String>,
}

impl Mail {
    /// The mail setup from `RESEND_API_KEY`, `RESEND_FROM`, and
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

    /// Where recovery links (email verification, password reset) point, or
    /// None when accounts cannot recover by email: mail is off, or
    /// `PUBLIC_BASE_URL` is unset. Unlike invite links these never come from
    /// the Host header, which a signed-out visitor sets at will, so a reset
    /// link cannot be pointed at another site.
    pub(crate) fn recovery_base(&self) -> Option<&str> {
        self.base_url.as_deref().filter(|_| self.mailer.enabled())
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
            warn!("PUBLIC_BASE_URL is not an http(s) URL; it is ignored");
            None
        }
    }
}

/// Sends mail. `Off` sends nothing.
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

/// One message.
#[derive(Clone, Debug)]
pub(crate) struct OutgoingMail {
    pub(crate) to: String,
    pub(crate) subject: String,
    pub(crate) text: String,
    pub(crate) html: String,
}

impl Mailer {
    /// Whether mail goes out.
    pub(crate) fn enabled(&self) -> bool {
        !matches!(self, Mailer::Off)
    }

    /// Sends one message; the error is safe to log: the recipient's address
    /// is masked and the API key never appears.
    pub(crate) async fn send(&self, mail: OutgoingMail) -> std::result::Result<(), String> {
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

/// Escapes text for an HTML mail body.
pub(crate) fn escape_html(text: &str) -> String {
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

/// Where invite links point: `PUBLIC_BASE_URL`, else the host the inviter's
/// browser asked for, over plain HTTP (the LAN default).
pub(crate) fn link_base(mail: &Mail, headers: &HeaderMap) -> String {
    if let Some(base) = &mail.base_url {
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
