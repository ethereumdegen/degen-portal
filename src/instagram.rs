//! Instagram: one browser trip, then a token that lives sixty days.
//!
//! This is **Instagram Login**, not Facebook Login: the app talks to
//! `graph.instagram.com` as an Instagram professional account, and no Facebook
//! Page, Page token or Business Manager is involved. That is the whole reason
//! it fits here — the Facebook-Login path needs a Page and a second consent
//! screen to post a photo.
//!
//! The token lifecycle is nothing like X's:
//!
//! - The code is exchanged for a **one-hour** token at `api.instagram.com`,
//!   which is immediately traded for a **sixty-day** one at
//!   `graph.instagram.com`. Only the long one is ever stored; a token that
//!   expires while you are at lunch is not worth writing to disk.
//! - There is **no refresh token**. The access token refreshes *itself*:
//!   `GET /refresh_access_token` with the token you already hold returns
//!   another sixty days. So [`oauth::Account::refresh_token`] stays `None`
//!   here, and the thing to protect is the access token itself.
//! - Meta refuses to refresh a token younger than 24 hours, and refuses
//!   outright once it has expired. Both are handled below: too young is not an
//!   error (the token is good for weeks yet), expired is, and it says to
//!   reconnect rather than pretending a retry might work.
//!
//! Instagram's app secret is required for both exchanges, so unlike X's public
//! client there is no PKCE: the secret is what proves the app, and it never
//! leaves this machine.

use std::time::Duration;

use degen_tools_core::config::{Credentials, load_credentials, lookup_credential};
use degen_tools_core::errors::DegenError;
use serde_json::Value;

use crate::oauth::{self, Account, RefreshLock, TokenResponse};

pub const AUTHORIZE_URL: &str = "https://www.instagram.com/oauth/authorize";
/// Short-lived token. Note the host: `api`, not `graph`.
pub const TOKEN_URL: &str = "https://api.instagram.com/oauth/access_token";
/// Short-lived token → sixty-day token.
pub const EXCHANGE_URL: &str = "https://graph.instagram.com/access_token";
/// Sixty-day token → another sixty days.
pub const REFRESH_URL: &str = "https://graph.instagram.com/refresh_access_token";
pub const ME_URL: &str = "https://graph.instagram.com/v25.0/me?fields=user_id,username";

/// Publishing and messaging both, because adding a scope later means sending
/// the human back through the browser.
pub const SCOPES: &str = "instagram_business_basic,instagram_business_content_publish,instagram_business_manage_messages,instagram_business_manage_comments";

pub const APP_ID: &str = "INSTAGRAM_APP_ID";
pub const APP_SECRET: &str = "INSTAGRAM_APP_SECRET";
pub const REDIRECT_URI: &str = "INSTAGRAM_REDIRECT_URI";
/// What the package's tools ask for. Never stored: it is minted from the
/// connected account, the way `$X_ACCESS_TOKEN` is not stored either.
pub const ACCESS_TOKEN: &str = "INSTAGRAM_ACCESS_TOKEN";

/// Refresh once the sixty days are this close to running out. Generous
/// because the cost of being late is a dead account and a browser trip, and
/// the cost of being early is one HTTP request.
pub const REFRESH_WINDOW_SECS: u64 = 7 * 86_400;

/// Meta refuses to refresh a token younger than this.
pub const MIN_REFRESH_AGE_SECS: u64 = 24 * 3600;

/// The app id and secret, when both are set. One of two is a half-finished
/// setup, and the error should say which half.
pub fn app(creds: &Credentials) -> Option<(String, String)> {
    let id = lookup_credential(creds, APP_ID)?;
    let secret = lookup_credential(creds, APP_SECRET)?;
    Some((id, secret))
}

/// Where Instagram sends the human back. Meta matches this string exactly
/// against the app's registered OAuth redirect URIs, and it requires HTTPS for
/// them — which is why this is a stored setting and not a constant. With
/// nothing stored, the loopback listener is used, for the app dashboards that
/// still take it.
pub fn redirect_uri(creds: &Credentials) -> String {
    lookup_credential(creds, REDIRECT_URI).unwrap_or_else(oauth::redirect_uri)
}

/// True when the redirect lands on this machine, so `connect` can catch the
/// code itself instead of asking a human to paste it.
pub fn is_loopback(redirect_uri: &str) -> bool {
    redirect_uri.starts_with(&format!("http://localhost:{}", oauth::CALLBACK_PORT))
        || redirect_uri.starts_with(&format!("http://127.0.0.1:{}", oauth::CALLBACK_PORT))
}

pub fn authorize_url(app_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}",
        oauth::percent_encode(app_id),
        oauth::percent_encode(redirect_uri),
        oauth::percent_encode(SCOPES),
        oauth::percent_encode(state)
    )
}

/// Instagram appends `#_` to the redirect it sends the browser to. It is not
/// part of the code, and sending it back earns "Matching code was not found".
pub fn clean_code(code: &str) -> &str {
    code.trim_end_matches("#_").trim()
}

/// Is Instagram usable at all right now?
pub fn configured() -> bool {
    oauth::load_metadata()
        .map(|accounts| accounts.accounts.values().any(|a| a.provider == "instagram"))
        .unwrap_or(false)
}

// ------------------------------------------------------------------- the flow

/// The code → short-lived token exchange. Returns the token and the
/// Instagram-scoped user id that came with it.
pub fn exchange_code(token_url: &str, app_id: &str, app_secret: &str, code: &str, redirect_uri: &str) -> Result<(String, String), DegenError> {
    let body = post_form(
        token_url,
        &[
            ("client_id", app_id),
            ("client_secret", app_secret),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
            ("code", clean_code(code)),
        ],
    )?;
    // Meta documents two shapes for this response: the flat object it has
    // always returned, and a `data` array in the newer docs. Take either.
    let payload = body.get("data").and_then(|d| d.get(0)).unwrap_or(&body);
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| DegenError::Http(format!("{token_url} returned no access_token: {body}")))?;
    let user_id = payload
        .get("user_id")
        .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
        .unwrap_or_default();
    Ok((token.to_string(), user_id))
}

/// One hour → sixty days. The app secret goes on this one, which is why it
/// happens here and not in anything a package file could name.
pub fn long_lived(exchange_url: &str, app_secret: &str, short_token: &str) -> Result<(String, u64), DegenError> {
    let url = format!(
        "{exchange_url}?grant_type=ig_exchange_token&client_secret={}&access_token={}",
        oauth::percent_encode(app_secret),
        oauth::percent_encode(short_token)
    );
    token_and_expiry(&get_json(&url, exchange_url)?, exchange_url)
}

/// Sixty days → another sixty days, from the token itself.
pub fn refresh(refresh_url: &str, token: &str) -> Result<(String, u64), DegenError> {
    let url = format!("{refresh_url}?grant_type=ig_refresh_token&access_token={}", oauth::percent_encode(token));
    token_and_expiry(&get_json(&url, refresh_url)?, refresh_url)
}

fn token_and_expiry(body: &Value, what: &str) -> Result<(String, u64), DegenError> {
    let token = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| DegenError::Http(format!("{what} returned no access_token: {body}")))?;
    // 60 days, unless Instagram says otherwise.
    let expires_in = body.get("expires_in").and_then(Value::as_u64).unwrap_or(60 * 86_400);
    Ok((token.to_string(), expires_in))
}

/// Who the fresh token belongs to, so the account is filed under the handle a
/// human recognises rather than a numeric id.
pub fn whoami(me_url: &str, token: &str) -> Result<(String, String), DegenError> {
    let separator = if me_url.contains('?') { '&' } else { '?' };
    let body = get_json(&format!("{me_url}{separator}access_token={}", oauth::percent_encode(token)), me_url)?;
    let id = body
        .get("user_id")
        .or_else(|| body.get("id"))
        .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
        .unwrap_or_default();
    let username = body.get("username").and_then(Value::as_str).unwrap_or_default().to_string();
    if id.is_empty() || username.is_empty() {
        return Err(DegenError::Http(format!(
            "authorization worked, but {me_url} did not return a user_id and username: {body}\n  \
             The account must be an Instagram professional account (Business or Creator)."
        )));
    }
    Ok((id, username))
}

/// Fold a fresh token into an account.
pub fn apply(account: &mut Account, token: String, expires_in: u64) {
    oauth::apply(account, TokenResponse { access_token: token, refresh_token: None, expires_in: Some(expires_in), scope: None });
}

// --------------------------------------------------------------- refreshing

/// A usable access token for `account_id`, refreshed first if the sixty days
/// are nearly up. The new token is stored before it is returned.
///
/// `refresh_url` is a parameter so this path can be tested against a local
/// server instead of Meta.
pub fn access_token(account_id: &str, refresh_url: &str) -> Result<String, DegenError> {
    let accounts = oauth::load()?;
    let account = account_of(&accounts, account_id)?;
    if !stale(account) {
        return Ok(account.access_token.clone());
    }

    let _lock = RefreshLock::acquire()?;
    // Whoever held the lock may have refreshed it already.
    let mut accounts = oauth::load()?;
    let account = accounts
        .accounts
        .get_mut(account_id)
        .ok_or_else(|| DegenError::InvalidArgs(format!("no connected account '{account_id}'")))?;
    if !stale(account) {
        return Ok(account.access_token.clone());
    }
    if account.expires_at <= oauth::now() {
        return Err(DegenError::InvalidArgs(format!(
            "{account_id}'s access token expired {} and Instagram will not refresh an expired token. Reconnect it:\n  degen-portal connect instagram",
            account.expiry_note()
        )));
    }
    // Too young to refresh, and in no danger: sixty days minus a few hours.
    if oauth::now().saturating_sub(account.connected_at) < MIN_REFRESH_AGE_SECS {
        return Ok(account.access_token.clone());
    }

    let (token, expires_in) = refresh(refresh_url, &account.access_token).map_err(|e| {
        DegenError::Http(format!(
            "{account_id}: refreshing the access token failed: {e}\n  \
             Reconnect with `degen-portal connect instagram` if this persists — an Instagram token that is not refreshed within sixty days is gone."
        ))
    })?;
    apply(account, token, expires_in);
    let fresh = account.access_token.clone();
    oauth::save(&accounts)?;
    Ok(fresh)
}

fn account_of<'a>(accounts: &'a oauth::Accounts, id: &str) -> Result<&'a Account, DegenError> {
    accounts
        .accounts
        .get(id)
        .ok_or_else(|| DegenError::InvalidArgs(format!("no connected account '{id}'")))
}

fn stale(account: &Account) -> bool {
    account.expires_at <= oauth::now() + REFRESH_WINDOW_SECS
}

// ------------------------------------------------------------------- the wire

fn client() -> Result<reqwest::blocking::Client, DegenError> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(degen_tools_core::app().user_agent)
        .build()
        .map_err(|e| DegenError::Http(format!("failed to create HTTP client: {e}")))
}

fn post_form(url: &str, form: &[(&str, &str)]) -> Result<Value, DegenError> {
    // `without_url`: a token or the app secret may be in the URL of a failure.
    let response = client()?
        .post(url)
        .form(form)
        .send()
        .map_err(|e| DegenError::Http(format!("{url} failed: {}", e.without_url())))?;
    read_json(response, url)
}

fn get_json(url: &str, what: &str) -> Result<Value, DegenError> {
    let response = client()?
        .get(url)
        .send()
        .map_err(|e| DegenError::Http(format!("{what} failed: {}", e.without_url())))?;
    read_json(response, what)
}

/// Meta answers a bad request with JSON and a 400, and the message in it is
/// the only useful part — "Matching code was not found or was already used"
/// tells a human exactly what to do, where "HTTP 400" does not.
fn read_json(response: reqwest::blocking::Response, what: &str) -> Result<Value, DegenError> {
    let status = response.status();
    let text = response.text().map_err(|e| DegenError::Http(format!("{what}: {}", e.without_url())))?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(DegenError::Http(format!("{what} returned HTTP {}: {}", status.as_u16(), error_message(&body).unwrap_or(text))));
    }
    if let Some(message) = error_message(&body) {
        return Err(DegenError::Http(format!("{what}: {message}")));
    }
    Ok(body)
}

/// Meta uses two error shapes: `{error: {message}}` from the Graph host and
/// `{error_message}` from the OAuth one.
fn error_message(body: &Value) -> Option<String> {
    body.get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .or_else(|| body.get("error_message").and_then(Value::as_str))
        .map(str::to_string)
}

/// Which shape is configured, for `status` and `list` to report.
pub fn describe() -> String {
    let creds = load_credentials().unwrap_or_default();
    let connected: Vec<String> = oauth::load_metadata()
        .map(|a| a.accounts.values().filter(|a| a.provider == "instagram").map(Account::id).collect())
        .unwrap_or_default();
    match (connected.is_empty(), app(&creds).is_some()) {
        (false, _) => format!("a connected account ({})", connected.join(", ")),
        (true, true) => "nothing yet — the app is set up, run `degen-portal connect instagram`".to_string(),
        (true, false) => format!("nothing yet — set {APP_ID} and {APP_SECRET}, then `degen-portal connect instagram`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_carries_the_scopes_meta_expects() {
        let url = authorize_url("123", "https://example.com/cb", "st4te");
        assert!(url.starts_with("https://www.instagram.com/oauth/authorize?"), "{url}");
        assert!(url.contains("client_id=123"), "{url}");
        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("redirect_uri=https%3A%2F%2Fexample.com%2Fcb"), "{url}");
        assert!(url.contains("state=st4te"), "{url}");
        // The scope list is comma-separated and percent-encoded as one value.
        assert!(url.contains("instagram_business_basic%2Cinstagram_business_content_publish"), "{url}");
        assert!(url.contains("instagram_business_manage_messages"), "{url}");
    }

    #[test]
    fn the_fragment_instagram_appends_is_not_part_of_the_code() {
        assert_eq!(clean_code("AQBx-hBsH3#_"), "AQBx-hBsH3");
        assert_eq!(clean_code(" AQBx-hBsH3 "), "AQBx-hBsH3");
        assert_eq!(clean_code("AQBx-hBsH3"), "AQBx-hBsH3");
    }

    #[test]
    fn a_redirect_that_comes_back_here_is_caught_rather_than_pasted() {
        assert!(is_loopback("http://localhost:7720/oauth/callback"));
        assert!(is_loopback("http://127.0.0.1:7720/oauth/callback"));
        assert!(!is_loopback("https://example.com/oauth/callback"));
        // HTTPS on localhost is not something this can listen on.
        assert!(!is_loopback("https://localhost:7720/oauth/callback"));
    }

    #[test]
    fn both_documented_token_response_shapes_are_read() {
        let flat = serde_json::json!({"access_token": "IGQ1", "user_id": 17841405793187218u64});
        let wrapped = serde_json::json!({"data": [{"access_token": "IGQ2", "user_id": "1020", "permissions": "instagram_business_basic"}]});
        for (body, token, user) in [(flat, "IGQ1", "17841405793187218"), (wrapped, "IGQ2", "1020")] {
            let payload = body.get("data").and_then(|d| d.get(0)).unwrap_or(&body);
            assert_eq!(payload["access_token"].as_str(), Some(token));
            let id = payload
                .get("user_id")
                .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
                .unwrap();
            assert_eq!(id, user, "a numeric user_id must not come back as JSON");
        }
    }

    #[test]
    fn a_meta_error_is_reported_by_its_message_not_its_status() {
        let oauth_shape = serde_json::json!({"error_type": "OAuthException", "code": 400, "error_message": "Matching code was not found or was already used"});
        assert_eq!(error_message(&oauth_shape).as_deref(), Some("Matching code was not found or was already used"));
        let graph_shape = serde_json::json!({"error": {"message": "Invalid OAuth access token", "code": 190}});
        assert_eq!(error_message(&graph_shape).as_deref(), Some("Invalid OAuth access token"));
        assert_eq!(error_message(&serde_json::json!({"access_token": "IGQ"})), None);
    }
}
