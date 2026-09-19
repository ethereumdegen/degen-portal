//! What this machine is allowed to post, and where.
//!
//! An agent that retries is normal. An agent that retries a post is a spam
//! incident, and a channel id it read out of a list is a channel it can post
//! in. So a write to a Discord channel is refused unless that exact channel was
//! named by a human, once, with `degen-portal discord allow <channel id>`.
//!
//! Reads are never gated: listing channels or reading messages is how an agent
//! finds the id to ask you about.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use degen_core::CallPolicy;
use degen_core::config::{CallContext, data_dir};
use degen_core::errors::DegenError;
use degen_core::tool::ToolConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `~/.degen-portal/policy.json`.
#[derive(Serialize, Deserialize, Default)]
pub struct Policy {
    /// Discord channel ids this machine may write to.
    #[serde(default)]
    pub channels: BTreeSet<String>,
}

fn path() -> Result<PathBuf, DegenError> {
    Ok(data_dir()?.join("policy.json"))
}

pub fn load() -> Result<Policy, DegenError> {
    let path = path()?;
    if !path.is_file() {
        return Ok(Policy::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(&path)?)?)
}

pub fn save(policy: &Policy) -> Result<(), DegenError> {
    fs::write(path()?, serde_json::to_string_pretty(policy)?)?;
    Ok(())
}

/// Add a channel. Returns false when it was already allowed.
pub fn allow(channel_id: &str) -> Result<bool, DegenError> {
    let mut policy = load()?;
    let added = policy.channels.insert(channel_id.to_string());
    save(&policy)?;
    Ok(added)
}

/// Remove a channel. Returns false when it was not allowed anyway.
pub fn deny(channel_id: &str) -> Result<bool, DegenError> {
    let mut policy = load()?;
    let removed = policy.channels.remove(channel_id);
    save(&policy)?;
    Ok(removed)
}

pub struct PortalPolicy;

impl CallPolicy for PortalPolicy {
    fn check(&self, tool: &ToolConfig, args: &Map<String, Value>, _ctx: &CallContext<'_>) -> Result<(), DegenError> {
        refuse(tool, args, &load()?.channels)
    }
}

/// The rule itself: a write that names a channel needs that channel allowed.
/// Reads pass, because finding the id is how a human gets asked for it.
fn refuse(tool: &ToolConfig, args: &Map<String, Value>, allowed: &BTreeSet<String>) -> Result<(), DegenError> {
    if tool.method.eq_ignore_ascii_case("GET") {
        return Ok(());
    }
    let Some(channel) = args.get("channel_id").and_then(Value::as_str).filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    if allowed.contains(channel) {
        return Ok(());
    }
    Err(DegenError::InvalidArgs(format!(
        "{} refuses channel {channel}: it is not on this machine's allowlist. A human adds it with\n  degen-portal discord allow {channel}\n{}",
        tool.name,
        if allowed.is_empty() {
            "No channel is allowed yet.".to_string()
        } else {
            format!("Allowed: {}", allowed.iter().cloned().collect::<Vec<_>>().join(", "))
        }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, method: &str) -> ToolConfig {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "method": method,
            "url": "https://discord.com/api/v10/channels/{channel_id}/messages",
        }))
        .unwrap()
    }

    fn args(channel: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("channel_id".into(), Value::String(channel.into()));
        m.insert("content".into(), Value::String("gm".into()));
        m
    }

    fn allowed(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_write_to_an_unallowed_channel_is_refused() {
        let err = refuse(&tool("discord_send_message", "POST"), &args("999"), &allowed(&["123"])).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("999"), "{message}");
        assert!(message.contains("degen-portal discord allow 999"), "the refusal must say how to fix it: {message}");
    }

    #[test]
    fn a_write_to_an_allowed_channel_passes() {
        refuse(&tool("discord_send_message", "POST"), &args("123"), &allowed(&["123"])).unwrap();
    }

    #[test]
    fn editing_deleting_and_reacting_are_writes_too() {
        for method in ["PATCH", "DELETE", "PUT"] {
            assert!(refuse(&tool("discord_edit_message", method), &args("999"), &allowed(&["123"])).is_err(), "{method}");
        }
    }

    #[test]
    fn reads_are_never_refused() {
        refuse(&tool("discord_get_messages", "GET"), &args("999"), &BTreeSet::new()).unwrap();
    }

    #[test]
    fn a_webhook_post_names_no_channel_so_the_url_is_the_scope() {
        let mut only_content = Map::new();
        only_content.insert("content".into(), Value::String("gm".into()));
        refuse(&tool("discord_send_webhook", "POST"), &only_content, &BTreeSet::new()).unwrap();
    }
}
