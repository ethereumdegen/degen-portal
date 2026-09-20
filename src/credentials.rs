//! Turning a `$NAME` a package declares into a value.
//!
//! Most things resolve from the ordinary credential store. The two social
//! providers do not:
//!
//! - **X** is not handled here at all: its credential is either an HMAC over
//!   the request or a bearer that has to be refreshed first, neither of which
//!   is a value you can substitute into a header. See [`crate::xauth`].
//! - **Instagram** is a bearer, so it *is* substitutable — but it belongs to a
//!   connected account and may be a refresh away from existing. It is minted
//!   here, which is what `CredentialResolver` returning a `Result` is for: a
//!   failed refresh is an error to report, not a credential that looks unset.

use degen_tools_core::config::{CallContext, CredentialResolver, load_credentials, lookup_credential, stored_or_fix};
use degen_tools_core::errors::DegenError;

use crate::{instagram, oauth};

/// The name X's own developer portal gives the OAuth 1.0a access token.
pub const X_ACCESS_TOKEN: &str = crate::xauth::TOKEN;

pub struct PortalCredentials;

impl CredentialResolver for PortalCredentials {
    fn resolve(&self, name: &str, ctx: &CallContext<'_>) -> Result<Option<String>, DegenError> {
        if name == instagram::ACCESS_TOKEN {
            // Which account, decided the same way every other call decides it:
            // `--account`, the project's default, the stored default, or the
            // only one connected. Never a silent first-of-several.
            let account = oauth::select(&oauth::load_metadata()?, "instagram", ctx.account)?;
            return instagram::access_token(&account.id(), instagram::REFRESH_URL).map(Some);
        }
        Ok(lookup_credential(&load_credentials()?, name))
    }

    fn missing(&self, name: &str) -> Option<String> {
        match name {
            X_ACCESS_TOKEN => {
                // Either shape counts as configured, and neither is something
                // to tell an agent to `auth set` blindly.
                if crate::xauth::configured() {
                    None
                } else {
                    Some("degen-portal connect x   (or set the four X_API_* keys)".to_string())
                }
            }
            // An OAuth token is connected, not typed in.
            instagram::ACCESS_TOKEN => {
                if instagram::configured() {
                    None
                } else {
                    Some("degen-portal connect instagram".to_string())
                }
            }
            _ => stored_or_fix(name),
        }
    }
}
