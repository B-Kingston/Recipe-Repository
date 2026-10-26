//! htmx glue: one-time flash notices and the response headers that keep
//! boosted navigation honest.
//!
//! Every page is still a full server-rendered document; htmx (loaded by
//! `base.html` with `hx-boost` on the body) swaps it in without a reload.
//! Status notices ride in the `recipe_flash` cookie instead of the query
//! string, so `?saved=1` never sits in the address bar or comes back on
//! reload: a mutating handler answers `flash_redirect`, the page it lands on
//! reads the key through the `Flash` extractor, and `htmx_layer` clears the
//! cookie on that first HTML response.

use axum::{
    extract::{FromRequestParts, Request},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts},
    middleware::Next,
    response::{AppendHeaders, IntoResponse, Redirect, Response},
};
use std::convert::Infallible;

pub(crate) const FLASH_COOKIE: &str = "recipe_flash";
/// A flash outlives its redirect by a minute at most; it is meant for the
/// very next page.
const FLASH_MAX_AGE: u32 = 60;

/// True when htmx sent the request (boosted links and forms, polls, live
/// search). Plain browser requests and the JSON fetches in `app.js` lack it.
pub(crate) fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// The `Set-Cookie` value carrying one flash key.
pub(crate) fn flash_cookie(key: &str) -> HeaderValue {
    debug_assert!(valid_key(key), "flash keys are short ASCII words");
    HeaderValue::from_str(&format!(
        "{FLASH_COOKIE}={key}; Path=/; Max-Age={FLASH_MAX_AGE}; HttpOnly; SameSite=Lax"
    ))
    .expect("flash keys are header-safe")
}

fn clear_flash_cookie() -> HeaderValue {
    HeaderValue::from_static("recipe_flash=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
}

/// A 303 to `to` that shows the `key` notice there once.
pub(crate) fn flash_redirect(to: &str, key: &str) -> Response {
    (
        AppendHeaders([(header::SET_COOKIE, flash_cookie(key))]),
        Redirect::to(to),
    )
        .into_response()
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 40 && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// The flash key the browser brought, or empty. Anything that is not a short
/// lowercase word is ignored, so a forged cookie can only pick among the
/// notices a template already knows.
#[derive(Debug, Default, Clone)]
pub(crate) struct Flash(pub(crate) String);

impl Flash {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .find_map(|pair| {
                let (name, value) = pair.trim().split_once('=')?;
                (name == FLASH_COOKIE && valid_key(value)).then(|| Self(value.to_string()))
            })
            .unwrap_or_default()
    }

    pub(crate) fn is(&self, key: &str) -> bool {
        self.0 == key
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Flash {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_headers(&parts.headers))
    }
}

/// Wraps every route.
///
/// - The first successful HTML page after a flash clears the cookie (unless
///   the response sets a new one).
/// - A non-GET htmx request answered with a page rather than a redirect (a
///   form shown again with its error) keeps the address bar where it was:
///   pushing the POST URL would make reload and Back fail.
/// - An htmx request that fails gets its error page swapped into the main
///   column, whatever region the request was aiming at.
pub(crate) async fn htmx_layer(req: Request, next: Next) -> Response {
    let htmx = is_htmx(req.headers());
    let had_flash = !Flash::from_headers(req.headers()).0.is_empty();
    let get = matches!(*req.method(), Method::GET | Method::HEAD);
    let mut response = next.run(req).await;
    let status = response.status();
    let html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    let sets_flash = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|value| value.as_bytes().starts_with(b"recipe_flash="));
    let headers = response.headers_mut();
    if had_flash && html && status.is_success() && !sets_flash {
        headers.append(header::SET_COOKIE, clear_flash_cookie());
    }
    if htmx && !get && html && status.is_success() {
        headers.insert("hx-push-url", HeaderValue::from_static("false"));
    }
    if htmx && html && (status.is_client_error() || status.is_server_error()) {
        headers.insert("hx-retarget", HeaderValue::from_static(".app-shell"));
        headers.insert("hx-reselect", HeaderValue::from_static(".app-shell"));
        headers.insert("hx-reswap", HeaderValue::from_static("outerHTML show:top"));
        if !get {
            headers.insert("hx-push-url", HeaderValue::from_static("false"));
        }
    }
    response
}

/// The htmx event that ends a sign-in popup (see `web/src/auth-popup.ts`).
pub(crate) const POPUP_FINISHED: &str = "popup-finished";

/// Ends a step that may run inside the Settings sign-in popup. An htmx
/// request gets no swap (204), the `key` notice for the page underneath, and
/// the `popup-finished` event, which closes the popup and refreshes that
/// page's panel. A plain request gets the usual flash redirect to `to`.
pub(crate) fn finish_popup(headers: &HeaderMap, to: &str, key: &str) -> Response {
    if !is_htmx(headers) {
        return flash_redirect(to, key);
    }
    (
        StatusCode::NO_CONTENT,
        AppendHeaders([
            (header::SET_COOKIE, flash_cookie(key)),
            (
                header::HeaderName::from_static("hx-trigger"),
                HeaderValue::from_static(POPUP_FINISHED),
            ),
        ]),
    )
        .into_response()
}

/// A redirect htmx can follow: plain requests get the usual 303, while htmx
/// requests get `HX-Redirect`, which makes the browser load `to` as a whole
/// page. Used where the next page has a different shell (sign-in, setup) or
/// where the redirect would otherwise be followed inside the XHR.
pub(crate) fn full_page_redirect(headers: &HeaderMap, to: &str) -> Response {
    if is_htmx(headers) {
        let mut response = StatusCode::OK.into_response();
        if let Ok(value) = HeaderValue::from_str(to) {
            response.headers_mut().insert("hx-redirect", value);
        }
        return response;
    }
    Redirect::to(to).into_response()
}
