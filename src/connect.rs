//! `degen-portal connect x` — the one browser trip.
//!
//! The listener is bound before the URL is printed, so a fast approval cannot
//! arrive before anything is listening. X's authorization code expires 30
//! seconds after approval, which is also why `--headless` warns about pasting
//! promptly.

use std::time::Duration;

use degen_tools_core::config::{load_credentials, lookup_credential};
use degen_tools_core::errors::DegenError;

use crate::oauth::{self, Account};

const PROVIDERS: &[&str] = &["x"];

pub fn run(provider: &str, headless: bool) -> Result<(), DegenError> {
    if provider != "x" {
        return Err(DegenError::InvalidArgs(format!(
            "'{provider}' is not a provider that connects over OAuth (known: {}). Discord uses a bot token: `degen-portal auth set DISCORD_BOT_TOKEN`",
            PROVIDERS.join(", ")
        )));
    }
    let creds = load_credentials()?;
    let client_id = lookup_credential(&creds, "X_CLIENT_ID").ok_or_else(|| {
        DegenError::InvalidArgs(
            "X_CLIENT_ID is not set. Create an app at https://developer.x.com with OAuth 2.0 enabled \
             and the callback URL\n\n    http://localhost:7720/oauth/callback\n\n\
             then store its Client ID (it is not a secret):\n  degen-portal auth set X_CLIENT_ID <client id>"
                .to_string(),
        )
    })?;
    let client_secret = lookup_credential(&creds, "X_CLIENT_SECRET");

    let verifier = oauth::new_verifier()?;
    let state = oauth::new_verifier()?;
    let redirect_uri = oauth::redirect_uri();
    let url = oauth::authorize_url(&client_id, &redirect_uri, &state, &oauth::challenge_for(&verifier));

    let code = if headless { paste_code(&url)? } else { browser_code(&url, &state)? };

    let token = oauth::exchange_code(oauth::TOKEN_URL, &client_id, client_secret.as_deref(), &code, &verifier, &redirect_uri)?;
    let scopes = token.scope.clone().unwrap_or_else(|| oauth::SCOPES.to_string());
    let (user_id, handle) = whoami(&token.access_token)?;

    let mut account = Account {
        provider: "x".to_string(),
        handle: handle.clone(),
        user_id,
        scopes,
        access_token: String::new(),
        expires_at: 0,
        refresh_token: None,
        client_id,
        connected_at: oauth::now(),
    };
    oauth::apply(&mut account, token);
    if account.refresh_token.is_none() {
        eprintln!("warning: X returned no refresh token, so this will stop working in two hours.");
        eprintln!("         The app must request the offline.access scope.");
    }

    let id = account.id();
    let mut accounts = oauth::load()?;
    let replaced = accounts.accounts.insert(id.clone(), account).is_some();
    oauth::save(&accounts)?;

    println!("{} @{handle} ({})", if replaced { "Reconnected" } else { "Connected" }, id);
    let others = accounts.accounts.values().filter(|a| a.provider == "x").count();
    if others > 1 && !accounts.default.contains_key("x") {
        println!("\n{others} X accounts are connected and none is the default, so calls will ask which to use:");
        println!("  degen-portal accounts default {id}");
    }
    Ok(())
}

/// Listen first, then send the human to the browser.
fn browser_code(url: &str, state: &str) -> Result<String, DegenError> {
    let waiting = std::thread::spawn(|| oauth::wait_for_callback(oauth::CALLBACK_PORT, Duration::from_secs(300)));
    // Give the listener a moment to bind before anything can be approved.
    std::thread::sleep(Duration::from_millis(50));

    println!("Opening X to approve access. If nothing opens, paste this into a browser:\n\n{url}\n");
    open_browser(url);

    let callback = waiting.join().map_err(|_| DegenError::Http("the callback listener stopped".to_string()))??;
    if let Some(error) = callback.error {
        return Err(DegenError::Http(format!("X refused the request: {error}")));
    }
    // The state is what ties this callback to the request made a moment ago.
    // A mismatch means the code came from somewhere else.
    match callback.state.as_deref() {
        Some(returned) if returned == state => {}
        _ => return Err(DegenError::Http("the callback carried the wrong state — ignoring it".to_string())),
    }
    callback.code.ok_or_else(|| DegenError::Http("the callback carried no code".to_string()))
}

fn paste_code(url: &str) -> Result<String, DegenError> {
    println!("Open this on any device, approve, then paste the URL it lands on (or just the code):\n\n{url}\n");
    println!("The code expires 30 seconds after you approve, so have this terminal ready.");
    print!("code or redirect URL: ");
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Err(DegenError::InvalidArgs("nothing pasted".to_string()));
    }
    Ok(match line.split_once('?') {
        Some((_, query)) => oauth::parse_query(query)
            .get("code")
            .cloned()
            .ok_or_else(|| DegenError::InvalidArgs("that URL has no ?code=".to_string()))?,
        None => line.to_string(),
    })
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Who the fresh token belongs to, so the account is filed under the handle
/// rather than whatever the human typed.
fn whoami(access_token: &str) -> Result<(String, String), DegenError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(degen_tools_core::app().user_agent)
        .build()
        .map_err(|e| DegenError::Http(format!("failed to create HTTP client: {e}")))?;
    let response = client
        .get(oauth::ME_URL)
        .bearer_auth(access_token)
        .send()
        .map_err(|e| DegenError::Http(format!("{} failed: {}", oauth::ME_URL, e.without_url())))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        let hint = if text.contains("client-forbidden") || text.contains("client-not-enrolled") {
            "\n  The app is not enrolled: in the X developer console, move it to the Pay-per-use package and the Production environment."
        } else {
            ""
        };
        return Err(DegenError::Http(format!(
            "authorization worked, but {} returned HTTP {}: {text}{hint}",
            oauth::ME_URL,
            status.as_u16()
        )));
    }
    let body: serde_json::Value = serde_json::from_str(&text)?;
    let id = body["data"]["id"].as_str().unwrap_or_default().to_string();
    let username = body["data"]["username"].as_str().unwrap_or_default().to_string();
    if id.is_empty() || username.is_empty() {
        return Err(DegenError::Http(format!("{} did not return an id and username: {text}", oauth::ME_URL)));
    }
    Ok((id, username))
}

/// `degen-portal accounts`.
pub fn list() -> Result<(), DegenError> {
    let accounts = oauth::load()?;
    if accounts.accounts.is_empty() {
        println!("No account is connected. Connect one with `degen-portal connect x`.");
        return Ok(());
    }
    for (id, account) in &accounts.accounts {
        let mark = if accounts.default.get(&account.provider) == Some(id) { "*" } else { " " };
        println!("{mark} {id:<24} {}", account.expiry_note());
        println!("    scopes: {}", account.scopes);
    }
    if accounts.default.is_empty() && accounts.accounts.len() > 1 {
        println!("\nNo default. Set one with `degen-portal accounts default <id>`.");
    }
    Ok(())
}

pub fn set_default(id: &str) -> Result<(), DegenError> {
    let mut accounts = oauth::load()?;
    let account = accounts
        .accounts
        .get(id)
        .ok_or_else(|| DegenError::InvalidArgs(format!("no connected account '{id}'")))?;
    let provider = account.provider.clone();
    accounts.default.insert(provider.clone(), id.to_string());
    oauth::save(&accounts)?;
    println!("{id} is now the default {provider} account");
    Ok(())
}

/// Tell X to drop the token, then forget it here. The local half happens even
/// if the remote half fails, because a token you cannot use is worse kept.
pub fn revoke(id: &str) -> Result<(), DegenError> {
    let mut accounts = oauth::load()?;
    let account = accounts
        .accounts
        .remove(id)
        .ok_or_else(|| DegenError::InvalidArgs(format!("no connected account '{id}'")))?;
    accounts.default.retain(|_, v| v != id);
    oauth::save(&accounts)?;

    let client_secret = lookup_credential(&load_credentials()?, "X_CLIENT_SECRET");
    match oauth::revoke_remote(&account, client_secret.as_deref()) {
        Ok(()) => println!("{id} revoked at X and removed from this machine"),
        Err(e) => {
            println!("{id} removed from this machine");
            eprintln!("warning: X did not confirm the revocation ({e}).");
            eprintln!("         Revoke it by hand at https://x.com/settings/connected_apps if that matters.");
        }
    }
    Ok(())
}

/// `degen-portal accounts secure [--off]` — move the tokens between the state
/// file and the keychain. Both directions load first, so the tokens are in
/// hand before anything is rewritten.
pub fn secure(on: bool) -> Result<(), DegenError> {
    let mut accounts = oauth::load()?;
    if accounts.keychain == on {
        println!("tokens are already in {}", if on { "the keychain" } else { "the state file" });
        return Ok(());
    }
    if accounts.accounts.is_empty() {
        accounts.keychain = on;
        oauth::save(&accounts)?;
        println!("no accounts yet; new ones will go in {}", if on { "the keychain" } else { "the state file" });
        return Ok(());
    }

    let ids: Vec<String> = accounts.accounts.keys().cloned().collect();
    accounts.keychain = on;
    oauth::save(&accounts)?;
    if !on {
        // They are in the file now, so the keychain copies are stale secrets.
        let store = crate::secrets::Keychain;
        for id in &ids {
            let _ = crate::secrets::SecretStore::delete(&store, &crate::secrets::access_key(id));
            let _ = crate::secrets::SecretStore::delete(&store, &crate::secrets::refresh_key(id));
        }
    }
    println!(
        "moved {} account{} into {}",
        ids.len(),
        if ids.len() == 1 { "" } else { "s" },
        if on { "the keychain" } else { "~/.degen-portal/accounts.json" }
    );
    if on {
        println!("macOS will ask for permission the first time each rebuilt binary reads them.");
    }
    Ok(())
}
