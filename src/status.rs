//! `degen-portal status` — what this machine can post right now.
//!
//! One answer to the question an agent should ask before a burst of posts, and
//! a human before trusting one: who it can act as, where it may write, how
//! much budget is left, and whether anything is being held.

use degen_tools_core::errors::DegenError;

use crate::{instagram, ledger, oauth, policy};

/// Every provider with a budget, in the order a human thinks of them.
pub const PROVIDERS: [&str; 3] = ["x", "discord", "instagram"];

/// A plain-language answer to "can I post right now?".
pub fn status() -> Result<String, DegenError> {
    let accounts = oauth::load_metadata()?;
    let policy = policy::load()?;
    let entries = ledger::read();
    let now = oauth::now();

    let mut out = String::new();
    out.push_str(&format!("x authenticates with {}\n", crate::xauth::describe()));
    out.push_str(&format!("instagram authenticates with {}\n", instagram::describe()));
    if accounts.accounts.is_empty() {
        // Keys need no account, so this is only worth saying when there is
        // also nothing else configured.
        if !crate::xauth::configured() {
            out.push_str("No X account is connected. A human connects one with `degen-portal connect x`.\n");
        }
    } else {
        for (id, account) in &accounts.accounts {
            let default = if accounts.default.get(&account.provider) == Some(id) { " (default)" } else { "" };
            out.push_str(&format!("account {id}{default}: {}\n", account.expiry_note()));
        }
    }
    out.push_str(&match policy.channels.len() {
        0 => "writable Discord channels: none. A human allows one with `degen-portal discord allow <channel id>`.\n".to_string(),
        _ => format!("writable Discord channels: {}\n", policy.channels.iter().cloned().collect::<Vec<_>>().join(", ")),
    });
    out.push_str(&match policy.recipients.len() {
        0 => "Instagram DM recipients: none. A human allows one with `degen-portal instagram allow <instagram-scoped id>`.\n".to_string(),
        _ => format!("Instagram DM recipients: {}\n", policy.recipients.iter().cloned().collect::<Vec<_>>().join(", ")),
    });
    for provider in PROVIDERS {
        let budget = policy.budget(provider);
        let hour = ledger::published_since(&entries, provider, 3600, now);
        let day = ledger::published_since(&entries, provider, 86_400, now);
        let held = if policy.approval(provider) == policy::Approval::Queue {
            ", calls wait for a human to approve them"
        } else {
            ""
        };
        out.push_str(&format!(
            "{provider}: {hour}/{} published this hour, {day}/{} today{held}\n",
            budget.per_hour, budget.per_day
        ));
    }
    out.push_str("\nA post is public and permanent. If a call fails, read the error and stop; do not call it again.");
    out.push_str("\nAn Instagram post cannot be deleted through the API at all, so `undo` cannot take one back.");
    Ok(out)
}
