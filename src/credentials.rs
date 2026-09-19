//! Turning a `$NAME` a package declares into a value.
//!
//! Everything resolves from the ordinary credential store. X is the exception
//! and it is not handled here: its credential is either an HMAC over the
//! request or a token that has to be refreshed first, neither of which is a
//! value you can substitute into a header. See [`crate::xauth`].

use degen_tools_core::config::{CallContext, CredentialResolver, load_credentials, lookup_credential, stored_or_fix};
use degen_tools_core::errors::DegenError;

/// The name X's own developer portal gives the OAuth 1.0a access token.
pub const X_ACCESS_TOKEN: &str = crate::xauth::TOKEN;

pub struct PortalCredentials;

impl CredentialResolver for PortalCredentials {
    fn resolve(&self, name: &str, _ctx: &CallContext<'_>) -> Result<Option<String>, DegenError> {
        Ok(lookup_credential(&load_credentials()?, name))
    }

    fn missing(&self, name: &str) -> Option<String> {
        if name == X_ACCESS_TOKEN {
            // Either shape counts as configured, and neither is something to
            // tell an agent to `auth set` blindly.
            return if crate::xauth::configured() {
                None
            } else {
                Some("degen-portal connect x   (or set the four X_API_* keys)".to_string())
            };
        }
        stored_or_fix(name)
    }
}
