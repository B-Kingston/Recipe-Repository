//! Display mode: how pages look in this browser. Colour is the normal look;
//! e-ink adds the black-on-white overrides in `static/eink.css`. Automatic,
//! the default, picks e-ink for Kindle browsers. The choice lives in a
//! per-browser cookie, so a Kindle and a laptop can each keep their own.

use crate::{AppError, AuthUser, DisplayTemplate, Result, render};
use axum::{
    Form,
    extract::Query,
    http::{HeaderMap, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;

/// Name of the cookie that holds the chosen mode. No cookie means Automatic.
const DISPLAY_COOKIE: &str = "recipe_display";
/// A saved choice lasts a year from the last time it was saved.
const DISPLAY_COOKIE_MAX_AGE: i64 = 365 * 24 * 60 * 60;
/// The body of `/display.css` in e-ink mode. Every page links `/display.css`
/// after `app.css`; other modes get an empty stylesheet.
const EINK_STYLESHEET: &str = "@import url(\"/static/eink.css\");\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisplayMode {
    Auto,
    Colour,
    Eink,
}

impl DisplayMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "colour" => Some(Self::Colour),
            "eink" => Some(Self::Eink),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Colour => "colour",
            Self::Eink => "eink",
        }
    }
}

/// The mode saved in this browser, or `Auto` when none was saved.
pub(crate) fn chosen_mode(headers: &HeaderMap) -> DisplayMode {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == DISPLAY_COOKIE).then(|| DisplayMode::parse(value))?
        })
        .unwrap_or(DisplayMode::Auto)
}

/// Whether the request comes from a Kindle's built-in browser.
pub(crate) fn is_kindle(headers: &HeaderMap) -> bool {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|agent| agent.to_ascii_lowercase().contains("kindle"))
}

/// Whether pages for this request should use the e-ink overrides.
pub(crate) fn uses_eink(headers: &HeaderMap) -> bool {
    match chosen_mode(headers) {
        DisplayMode::Eink => true,
        DisplayMode::Colour => false,
        DisplayMode::Auto => is_kindle(headers),
    }
}

/// `GET /display.css`: the e-ink overrides in e-ink mode, an empty stylesheet
/// otherwise. It is never cached, so a new choice applies on the next page.
pub(crate) async fn display_css(headers: HeaderMap) -> Response {
    let body = if uses_eink(&headers) {
        EINK_STYLESHEET
    } else {
        ""
    };
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct DisplayQuery {
    pub(crate) saved: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct DisplayForm {
    pub(crate) mode: String,
}

pub(crate) async fn display_page(
    _user: AuthUser,
    headers: HeaderMap,
    Query(query): Query<DisplayQuery>,
) -> Result<Html<String>> {
    render(DisplayTemplate {
        mode: chosen_mode(&headers).as_str(),
        kindle: is_kindle(&headers),
        saved: query.saved.is_some(),
    })
}

/// Saves the mode in this browser's cookie. Automatic clears the cookie.
pub(crate) async fn update_display(
    _user: AuthUser,
    Form(form): Form<DisplayForm>,
) -> Result<Response> {
    let mode = DisplayMode::parse(&form.mode)
        .ok_or_else(|| AppError::BadRequest("Choose Automatic, Colour or E-ink.".into()))?;
    let cookie = match mode {
        DisplayMode::Auto => {
            format!("{DISPLAY_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
        }
        mode => format!(
            "{DISPLAY_COOKIE}={}; Path=/; Max-Age={DISPLAY_COOKIE_MAX_AGE}; HttpOnly; SameSite=Lax",
            mode.as_str()
        ),
    };
    Ok((
        [(header::SET_COOKIE, cookie)],
        Redirect::to("/settings/display?saved=1"),
    )
        .into_response())
}
