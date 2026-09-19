//! Turning a `$NAME` a package declares into a value.
//!
//! Everything resolves from the ordinary credential store, except the one name
//! that cannot: `$X_ACCESS_TOKEN` is minted from the connected account, and
//! refreshed first if it is about to expire. That is why the resolver returns a
//! `Result` — a failed refresh has to be reported as a failed refresh, not as a
//! credential that looks unset.

use degen_tools_core::config::{CallContext, CredentialResolver, load_credentials, lookup_credential, stored_or_fix};
use degen_tools_core::errors::DegenError;

use crate::oauth;

/// The name an X tool puts in its `Authorization` header.
pub const X_ACCESS_TOKEN: &str = "X_ACCESS_TOKEN";

pub struct PortalCredentials;

impl CredentialResolver for PortalCredentials {
    fn resolve(&self, name: &str, ctx: &CallContext<'_>) -> Result<Option<String>, DegenError> {
        if name == X_ACCESS_TOKEN {
            let accounts = oauth::load()?;
            let account = oauth::select(&accounts, "x", ctx.account)?;
            let secret = lookup_credential(&load_credentials()?, "X_CLIENT_SECRET");
            return oauth::access_token(&account.id(), oauth::TOKEN_URL, secret.as_deref()).map(Some);
        }
        Ok(lookup_credential(&load_credentials()?, name))
    }

    fn missing(&self, name: &str) -> Option<String> {
        if name == X_ACCESS_TOKEN {
            // Never stored by hand: it is minted from a connected account and
            // refreshed per call. Telling an agent to `auth set` it would be a
            // lie it would dutifully pass on to the user.
            let connected = oauth::load().map(|a| a.accounts.values().any(|x| x.provider == "x")).unwrap_or(false);
            return if connected { None } else { Some("degen-portal connect x".to_string()) };
        }
        stored_or_fix(name)
    }
}
