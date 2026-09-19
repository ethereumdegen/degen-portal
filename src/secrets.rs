//! Where the tokens themselves live.
//!
//! By default: in `accounts.json`, owner-only, like every other credential
//! here. Optionally: in the OS keychain, with `accounts.json` keeping only the
//! handle, the scopes and the expiry.
//!
//! It is optional rather than the default because of how macOS keychain ACLs
//! work — they are per-binary, so every `cargo build` produces a binary the
//! keychain has not seen and the prompt comes back. That is the right trade
//! for a tool you installed and use, and the wrong one for a tool you are
//! rebuilding every few minutes. `degen-portal accounts secure` chooses.
//!
//! What it buys: a 0600 file is readable by anything running as you, including
//! whatever an agent decides to `cat`. A keychain entry is not, without a
//! prompt. A refresh token is permanent control of the account, so that
//! difference is worth the option.

use degen_tools_core::errors::DegenError;

/// The service name keychain entries are filed under.
const SERVICE: &str = "degen-portal";

/// Somewhere to put a secret that is not a file in the state directory.
pub trait SecretStore: Send + Sync {
    fn set(&self, key: &str, value: &str) -> Result<(), DegenError>;
    fn get(&self, key: &str) -> Result<Option<String>, DegenError>;
    fn delete(&self, key: &str) -> Result<(), DegenError>;
}

pub struct Keychain;

impl Keychain {
    fn entry(key: &str) -> Result<keyring::Entry, DegenError> {
        keyring::Entry::new(SERVICE, key).map_err(|e| DegenError::InvalidArgs(format!("keychain: {e}")))
    }
}

impl SecretStore for Keychain {
    fn set(&self, key: &str, value: &str) -> Result<(), DegenError> {
        Self::entry(key)?
            .set_password(value)
            .map_err(|e| DegenError::InvalidArgs(format!("keychain: storing {key} failed: {e}")))
    }

    fn get(&self, key: &str) -> Result<Option<String>, DegenError> {
        match Self::entry(key)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(DegenError::InvalidArgs(format!(
                "keychain: reading {key} failed: {e}\n  \
                 If this is a permission prompt you dismissed, run the command again and allow it, \
                 or move the tokens back to a file with `degen-portal accounts secure --off`."
            ))),
        }
    }

    fn delete(&self, key: &str) -> Result<(), DegenError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(DegenError::InvalidArgs(format!("keychain: removing {key} failed: {e}"))),
        }
    }
}

/// `x:handle` → the two entries its tokens live in.
pub fn access_key(account_id: &str) -> String {
    format!("{account_id}/access_token")
}

pub fn refresh_key(account_id: &str) -> String {
    format!("{account_id}/refresh_token")
}

#[cfg(test)]
pub mod memory {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    /// A stand-in for the keychain, so the moving of tokens can be tested
    /// without a prompt on the machine running the tests.
    #[derive(Default)]
    pub struct Memory(pub Mutex<HashMap<String, String>>);

    impl SecretStore for Memory {
        fn set(&self, key: &str, value: &str) -> Result<(), DegenError> {
            self.0.lock().insert(key.to_string(), value.to_string());
            Ok(())
        }

        fn get(&self, key: &str) -> Result<Option<String>, DegenError> {
            Ok(self.0.lock().get(key).cloned())
        }

        fn delete(&self, key: &str) -> Result<(), DegenError> {
            self.0.lock().remove(key);
            Ok(())
        }
    }
}
