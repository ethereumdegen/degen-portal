//! OAuth 2.0 authorization code + PKCE, for X.
//!
//! No backend is involved. The browser is sent to x.com with a challenge, X
//! redirects to a listener on this machine, and the code is exchanged here for
//! a token that lives in `~/.degen-portal/accounts.json`. The `code_verifier`
//! never leaves the process that made it, so nothing in the middle — including
//! whatever is serving the redirect — can spend the code.
//!
//! X's access tokens last two hours, and `offline.access` gives a refresh token
//! that X rotates on every use. Losing the rotation means losing the account, so
//! a refresh happens under a lock file and the new pair is written before the
//! old one is dropped.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use degen_tools_core::config::data_dir;
use degen_tools_core::errors::DegenError;
use degen_tools_core::server::new_token;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const AUTHORIZE_URL: &str = "https://x.com/i/oauth2/authorize";
pub const TOKEN_URL: &str = "https://api.x.com/2/oauth2/token";
pub const ME_URL: &str = "https://api.x.com/2/users/me";
pub const REVOKE_URL: &str = "https://api.x.com/2/oauth2/revoke";

/// `media.write` is requested now, though nothing uploads yet: adding a scope
/// later means sending the user back through the browser.
pub const SCOPES: &str = "tweet.read tweet.write users.read media.write offline.access";

/// The callback port, deliberately not the API's: `serve` may be running.
pub const CALLBACK_PORT: u16 = 7720;
pub const CALLBACK_PATH: &str = "/oauth/callback";

/// A token this close to expiry is refreshed before it is used.
const REFRESH_MARGIN_SECS: u64 = 60;

pub fn redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------- the store

#[derive(Serialize, Deserialize, Clone)]
pub struct Account {
    pub provider: String,
    /// The handle as X spells it, without the `@`.
    pub handle: String,
    pub user_id: String,
    pub scopes: String,
    pub access_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub refresh_token: Option<String>,
    /// Kept with the account so a refresh works even if the credential moves.
    pub client_id: String,
    pub connected_at: u64,
}

impl Account {
    pub fn id(&self) -> String {
        format!("{}:{}", self.provider, self.handle)
    }

    pub fn expired(&self) -> bool {
        self.expires_at <= now() + REFRESH_MARGIN_SECS
    }

    /// "in 47m", "expired 3m ago".
    pub fn expiry_note(&self) -> String {
        let now = now();
        if self.expires_at > now {
            let mins = (self.expires_at - now) / 60;
            format!("expires in {mins}m")
        } else {
            let mins = (now - self.expires_at) / 60;
            format!("expired {mins}m ago (refreshes on next use)")
        }
    }
}

/// Not derived: a refresh token is permanent control of the account, and
/// `{:?}` in a log line or a test failure is exactly how one escapes.
impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("id", &self.id())
            .field("user_id", &self.user_id)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .field("access_token", &"<secret>")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<secret>"))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct Accounts {
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
    /// provider → account id, when more than one is connected.
    #[serde(default)]
    pub default: BTreeMap<String, String>,
    /// When set, the tokens are in the OS keychain and this file holds only
    /// which accounts exist, their scopes and their expiry.
    #[serde(default)]
    pub keychain: bool,
}

fn accounts_path() -> Result<PathBuf, DegenError> {
    Ok(data_dir()?.join("accounts.json"))
}

pub fn load() -> Result<Accounts, DegenError> {
    load_with(&crate::secrets::Keychain)
}

/// The accounts without their tokens: who is connected, their scopes and when
/// each expires. Anything that only lists or displays reads this — with the
/// keychain on, fetching the secrets would put a permission prompt behind a
/// dashboard that refreshes every second.
pub fn load_metadata() -> Result<Accounts, DegenError> {
    let path = accounts_path()?;
    if !path.is_file() {
        return Ok(Accounts::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(&path)?)?)
}

pub fn load_with(store: &dyn crate::secrets::SecretStore) -> Result<Accounts, DegenError> {
    let mut accounts = load_metadata()?;
    if accounts.keychain {
        for (id, account) in accounts.accounts.iter_mut() {
            account.access_token = store.get(&crate::secrets::access_key(id))?.unwrap_or_default();
            account.refresh_token = store.get(&crate::secrets::refresh_key(id))?;
        }
    }
    Ok(accounts)
}

/// Owner-only from the moment it exists, then renamed over the old file: a
/// refresh token is never briefly world-readable and never half-written.
/// With the keychain on, the tokens go there first and the file gets blanks.
pub fn save(accounts: &Accounts) -> Result<(), DegenError> {
    save_with(accounts, &crate::secrets::Keychain)
}

pub fn save_with(accounts: &Accounts, store: &dyn crate::secrets::SecretStore) -> Result<(), DegenError> {
    let mut on_disk = Accounts { accounts: accounts.accounts.clone(), default: accounts.default.clone(), keychain: accounts.keychain };
    if accounts.keychain {
        for (id, account) in accounts.accounts.iter() {
            // Stored before the file is written: a crash in between leaves a
            // keychain entry with no account, which is harmless, rather than
            // an account whose token is nowhere.
            store.set(&crate::secrets::access_key(id), &account.access_token)?;
            match &account.refresh_token {
                Some(refresh) => store.set(&crate::secrets::refresh_key(id), refresh)?,
                None => store.delete(&crate::secrets::refresh_key(id))?,
            }
        }
        for account in on_disk.accounts.values_mut() {
            account.access_token = String::new();
            account.refresh_token = account.refresh_token.as_ref().map(|_| String::new());
        }
    }

    let path = accounts_path()?;
    let tmp = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(serde_json::to_string_pretty(&on_disk)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Which account a call acts as: the one named, then the project's
/// `PORTAL_<PROVIDER>_ACCOUNT`, then the stored default, then the only one
/// connected. Never a silent first-of-several — posting as the wrong handle is
/// not a mistake you can take back.
pub fn select(accounts: &Accounts, provider: &str, asked: Option<&str>) -> Result<Account, DegenError> {
    let of_provider: Vec<&Account> = accounts.accounts.values().filter(|a| a.provider == provider).collect();
    if let Some(name) = asked {
        let id = if name.contains(':') { name.to_string() } else { format!("{provider}:{name}") };
        return accounts
            .accounts
            .get(&id)
            .cloned()
            .ok_or_else(|| DegenError::InvalidArgs(format!("no connected account '{id}'.{}", listing(&of_provider))));
    }
    if let Some(id) = accounts.default.get(provider)
        && let Some(account) = accounts.accounts.get(id)
    {
        return Ok(account.clone());
    }
    match of_provider.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(DegenError::InvalidArgs(format!(
            "no {provider} account is connected. Connect one with:\n  degen-portal connect {provider}"
        ))),
        many => Err(DegenError::InvalidArgs(format!(
            "{} {provider} accounts are connected and none is the default. Name one with --account, or set a default:\n  degen-portal accounts default {}",
            many.len(),
            many[0].id()
        ))),
    }
}

fn listing(accounts: &[&Account]) -> String {
    if accounts.is_empty() {
        return String::new();
    }
    format!(" Connected: {}", accounts.iter().map(|a| a.id()).collect::<Vec<_>>().join(", "))
}

// ------------------------------------------------------------------- PKCE

/// base64url without padding, as PKCE and JWTs use it.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for (i, c) in chars.iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[*c as usize] as char);
            }
        }
    }
    out
}

/// The S256 challenge for a verifier.
pub fn challenge_for(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str, challenge: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        percent_encode(client_id),
        percent_encode(redirect_uri),
        percent_encode(SCOPES),
    )
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `a=1&b=2` → pairs, percent-decoded.
pub fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect()
}

// -------------------------------------------------------- the callback wait

/// What X sent back.
pub struct Callback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// Serve exactly one callback on the loopback interface, then stop.
///
/// Both `127.0.0.1` and `::1` are bound, because a browser resolving
/// `localhost` may pick either and the redirect must land regardless.
pub fn wait_for_callback(port: u16, timeout: Duration) -> Result<Callback, DegenError> {
    let (tx, rx) = mpsc::channel();
    let mut bound = 0;
    for addr in [SocketAddr::from(([127, 0, 0, 1], port)), SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port))] {
        let Ok(listener) = TcpListener::bind(addr) else { continue };
        bound += 1;
        let tx = tx.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Some(callback) = serve_one(stream)
                    && tx.send(callback).is_ok()
                {
                    return;
                }
            }
        });
    }
    if bound == 0 {
        return Err(DegenError::InvalidArgs(format!(
            "could not listen on 127.0.0.1:{port} — something else is using it. Close it, or connect with --headless"
        )));
    }
    rx.recv_timeout(timeout)
        .map_err(|_| DegenError::InvalidArgs("timed out waiting for the browser to come back".to_string()))
}

/// Read one request, answer it, and report the query it carried. Requests to
/// any other path (a browser asking for a favicon) are answered and ignored.
fn serve_one(mut stream: TcpStream) -> Option<Callback> {
    let mut line = String::new();
    BufReader::new(stream.try_clone().ok()?).read_line(&mut line).ok()?;
    let target = line.split_whitespace().nth(1)?.to_string();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if path != CALLBACK_PATH {
        let _ = respond(&mut stream, "404 Not Found", "<h1>Not here</h1>");
        return None;
    }
    let params = parse_query(query);
    let callback = Callback {
        code: params.get("code").cloned(),
        state: params.get("state").cloned(),
        error: params.get("error_description").or_else(|| params.get("error")).cloned(),
    };
    let body = match (&callback.code, &callback.error) {
        (Some(_), _) => "<h1>Connected.</h1><p>Close this tab and go back to the terminal.</p>",
        (None, Some(_)) => "<h1>Refused.</h1><p>X sent an error back. The terminal has the detail.</p>",
        _ => "<h1>Nothing to do.</h1>",
    };
    let _ = respond(&mut stream, "200 OK", body);
    Some(callback)
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let page = format!(
        "<!doctype html><meta charset=utf-8><title>degen-portal</title>\
         <body style=\"font:16px system-ui;padding:3rem;max-width:32rem\">{body}</body>"
    );
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    )?;
    stream.flush()
}

// ------------------------------------------------------------ token endpoint

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Ask X to invalidate the account's tokens. Best effort: the caller has
/// already forgotten them locally.
pub fn revoke_remote(account: &Account, client_secret: Option<&str>) -> Result<(), DegenError> {
    let token = account.refresh_token.as_deref().unwrap_or(&account.access_token);
    let hint = if account.refresh_token.is_some() { "refresh_token" } else { "access_token" };
    post_form(
        REVOKE_URL,
        &account.client_id,
        client_secret,
        &[("token", token), ("token_type_hint", hint), ("client_id", &account.client_id)],
    )
    .map(|_| ())
}

/// One call to the token endpoint. `token_url` is a parameter so the refresh
/// path can be tested against a local server instead of X.
fn post_form(token_url: &str, client_id: &str, client_secret: Option<&str>, form: &[(&str, &str)]) -> Result<TokenResponse, DegenError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(degen_tools_core::app().user_agent)
        .build()
        .map_err(|e| DegenError::Http(format!("failed to create HTTP client: {e}")))?;
    let mut request = client.post(token_url).form(form);
    // A public client proves itself with PKCE alone and must send client_id in
    // the body. A confidential app (X's "Web App" type) also wants Basic auth.
    if let Some(secret) = client_secret {
        request = request.basic_auth(client_id, Some(secret));
    }
    let response = request.send().map_err(|e| DegenError::Http(format!("{token_url} failed: {}", e.without_url())))?;
    let status = response.status();
    let text = response.text().map_err(|e| DegenError::Http(format!("{token_url}: {}", e.without_url())))?;
    if !status.is_success() {
        return Err(DegenError::Http(format!("{token_url} returned HTTP {}: {text}", status.as_u16())));
    }
    serde_json::from_str(&text).map_err(|e| DegenError::Http(format!("{token_url} returned something unexpected: {e}")))
}

pub fn exchange_code(
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, DegenError> {
    post_form(
        token_url,
        client_id,
        client_secret,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ],
    )
}

pub fn refresh_token(token_url: &str, client_id: &str, client_secret: Option<&str>, refresh: &str) -> Result<TokenResponse, DegenError> {
    post_form(
        token_url,
        client_id,
        client_secret,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", client_id)],
    )
}

/// Fold a token response into an account, keeping the old refresh token when
/// the response does not carry a new one.
pub fn apply(account: &mut Account, token: TokenResponse) {
    account.access_token = token.access_token;
    account.expires_at = now() + token.expires_in.unwrap_or(7200);
    if let Some(refresh) = token.refresh_token {
        account.refresh_token = Some(refresh);
    }
    if let Some(scope) = token.scope {
        account.scopes = scope;
    }
}

// --------------------------------------------------------------- refreshing

/// Held while a refresh is in flight, because X rotates refresh tokens: two
/// processes refreshing the same account at once would leave one of them
/// holding a token X has already invalidated, and that account would be dead
/// until the human reconnected it. Instagram does not rotate, but its token
/// is a sixty-day credential replaced in place, so a lost race there loses
/// the account just as thoroughly.
pub(crate) struct RefreshLock(PathBuf);

impl RefreshLock {
    pub(crate) fn acquire() -> Result<Self, DegenError> {
        let path = data_dir()?.join("refresh.lock");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A process that died mid-refresh must not wedge the account.
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(60))
                        .unwrap_or(false);
                    if stale {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        return Err(DegenError::Http("another degen-portal is still refreshing this token".to_string()));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(DegenError::IoError(e)),
            }
        }
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A usable access token for `account_id`, refreshed first if it is at or near
/// expiry. The refreshed pair is stored before it is returned.
pub fn access_token(account_id: &str, token_url: &str, client_secret: Option<&str>) -> Result<String, DegenError> {
    let accounts = load()?;
    let account = accounts
        .accounts
        .get(account_id)
        .ok_or_else(|| DegenError::InvalidArgs(format!("no connected account '{account_id}'")))?;
    if !account.expired() {
        return Ok(account.access_token.clone());
    }

    let _lock = RefreshLock::acquire()?;
    // Whoever held the lock may have refreshed it already.
    let mut accounts = load()?;
    let account = accounts
        .accounts
        .get_mut(account_id)
        .ok_or_else(|| DegenError::InvalidArgs(format!("no connected account '{account_id}'")))?;
    if !account.expired() {
        return Ok(account.access_token.clone());
    }
    let refresh = account.refresh_token.clone().ok_or_else(|| {
        DegenError::InvalidArgs(format!(
            "{account_id}'s access token has expired and it has no refresh token (it was connected without offline.access). Reconnect it:\n  degen-portal connect {}",
            account.provider
        ))
    })?;
    let client_id = account.client_id.clone();
    let token = refresh_token(token_url, &client_id, client_secret, &refresh).map_err(|e| {
        DegenError::Http(format!("{account_id}: refreshing the access token failed: {e}\n  Reconnect with `degen-portal connect {}` if this persists.", account.provider))
    })?;
    apply(account, token);
    let fresh = account.access_token.clone();
    save(&accounts)?;
    Ok(fresh)
}

/// A verifier: 64 hex characters, inside PKCE's 43-128 unreserved range.
pub fn new_verifier() -> Result<String, DegenError> {
    new_token()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B.
    #[test]
    fn pkce_matches_the_rfc_test_vector() {
        assert_eq!(challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn base64url_has_no_padding_and_no_plus_or_slash() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(&[0xfb, 0xff, 0xfe]), "-__-");
    }

    #[test]
    fn a_verifier_is_long_enough_for_pkce() {
        let v = new_verifier().unwrap();
        assert!((43..=128).contains(&v.len()), "{} characters", v.len());
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
    }

    #[test]
    fn the_authorize_url_carries_what_x_requires() {
        let url = authorize_url("CID", "http://localhost:7720/oauth/callback", "st4te", "chal");
        assert!(url.starts_with("https://x.com/i/oauth2/authorize?response_type=code&client_id=CID"));
        assert!(url.contains("&redirect_uri=http%3A%2F%2Flocalhost%3A7720%2Foauth%2Fcallback"));
        assert!(url.contains("&code_challenge=chal&code_challenge_method=S256"));
        assert!(url.contains("offline.access"), "without it there is no refresh token");
        assert!(url.contains("&state=st4te"));
    }

    #[test]
    fn a_query_is_decoded() {
        let q = parse_query("code=abc%2Fdef&state=xyz&error_description=not+allowed");
        assert_eq!(q["code"], "abc/def");
        assert_eq!(q["state"], "xyz");
        assert_eq!(q["error_description"], "not allowed");
    }

    fn account(expires_at: u64) -> Account {
        Account {
            provider: "x".into(),
            handle: "degen".into(),
            user_id: "1".into(),
            scopes: SCOPES.into(),
            access_token: "old".into(),
            expires_at,
            refresh_token: Some("r1".into()),
            client_id: "CID".into(),
            connected_at: 0,
        }
    }

    #[test]
    fn a_token_inside_the_margin_counts_as_expired() {
        assert!(account(now() + 30).expired(), "a token with 30s left must be refreshed, not used");
        assert!(!account(now() + 600).expired());
        assert!(account(0).expired());
    }

    #[test]
    fn applying_a_response_without_a_new_refresh_token_keeps_the_old_one() {
        let mut a = account(0);
        apply(&mut a, TokenResponse { access_token: "new".into(), refresh_token: None, expires_in: Some(7200), scope: None });
        assert_eq!(a.access_token, "new");
        assert_eq!(a.refresh_token.as_deref(), Some("r1"));
        assert!(a.expires_at > now() + 7000);

        apply(&mut a, TokenResponse { access_token: "newer".into(), refresh_token: Some("r2".into()), expires_in: None, scope: None });
        assert_eq!(a.refresh_token.as_deref(), Some("r2"), "a rotated refresh token replaces the old one");
    }

    fn accounts_with(ids: &[&str]) -> Accounts {
        let mut accounts = Accounts::default();
        for id in ids {
            let (provider, handle) = id.split_once(':').unwrap();
            let mut a = account(now() + 3600);
            a.provider = provider.into();
            a.handle = handle.into();
            accounts.accounts.insert(id.to_string(), a);
        }
        accounts
    }

    #[test]
    fn one_connected_account_is_the_one_used() {
        let accounts = accounts_with(&["x:degen"]);
        assert_eq!(select(&accounts, "x", None).unwrap().id(), "x:degen");
    }

    #[test]
    fn two_accounts_and_no_default_is_an_error_not_a_guess() {
        let accounts = accounts_with(&["x:degen", "x:other"]);
        let err = select(&accounts, "x", None).unwrap_err().to_string();
        assert!(err.contains("accounts default"), "{err}");
        assert_eq!(select(&accounts, "x", Some("other")).unwrap().id(), "x:other");
        assert_eq!(select(&accounts, "x", Some("x:degen")).unwrap().id(), "x:degen");
    }

    #[test]
    fn a_default_settles_it() {
        let mut accounts = accounts_with(&["x:degen", "x:other"]);
        accounts.default.insert("x".into(), "x:other".into());
        assert_eq!(select(&accounts, "x", None).unwrap().id(), "x:other");
    }

    #[test]
    fn no_account_says_how_to_connect_one() {
        let err = select(&Accounts::default(), "x", None).unwrap_err().to_string();
        assert!(err.contains("degen-portal connect x"), "{err}");
    }

    /// The tokens move out of the state file and come back whole.
    #[test]
    fn the_keychain_holds_the_tokens_and_the_file_holds_none() {
        let home = std::env::temp_dir().join(format!("degen-portal-keychain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // Safe: this is the only test in this binary that touches HOME.
        unsafe { std::env::set_var("HOME", &home) };
        crate::init();

        let store = crate::secrets::memory::Memory::default();
        let mut accounts = Accounts::default();
        let mut a = account(now() + 3600);
        a.handle = "kept".into();
        a.access_token = "ACCESS-SECRET".into();
        a.refresh_token = Some("REFRESH-SECRET".into());
        accounts.accounts.insert("x:kept".to_string(), a);

        // Off: the file is the store, as it always has been.
        save_with(&accounts, &store).unwrap();
        let file = home.join(".degen-portal/accounts.json");
        assert!(std::fs::read_to_string(&file).unwrap().contains("REFRESH-SECRET"));

        // On: the file keeps the account, not the secrets.
        accounts.keychain = true;
        save_with(&accounts, &store).unwrap();
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(!on_disk.contains("ACCESS-SECRET"), "the access token must not be in the file: {on_disk}");
        assert!(!on_disk.contains("REFRESH-SECRET"), "and neither must the refresh token");
        assert!(on_disk.contains("x:kept"), "the account itself still is");

        // And they come back whole.
        let reloaded = load_with(&store).unwrap();
        let account = &reloaded.accounts["x:kept"];
        assert_eq!(account.access_token, "ACCESS-SECRET");
        assert_eq!(account.refresh_token.as_deref(), Some("REFRESH-SECRET"));
        assert!(reloaded.keychain);
        assert!(!account.expired(), "the expiry survives the round trip");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn debug_output_never_carries_a_token() {
        let mut a = account(now() + 3600);
        a.access_token = "AAAAsecretAAAA".into();
        a.refresh_token = Some("RRRRsecretRRRR".into());
        let shown = format!("{a:?}");
        assert!(!shown.contains("AAAAsecretAAAA"), "{shown}");
        assert!(!shown.contains("RRRRsecretRRRR"), "{shown}");
        assert!(shown.contains("x:degen"), "{shown}");
    }
}
