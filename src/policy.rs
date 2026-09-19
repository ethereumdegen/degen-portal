//! What this machine is allowed to post, how much of it, and how often.
//!
//! Four gates, cheapest first, all in front of the same `check`:
//!
//! 1. **Where** — a Discord write needs the channel allowed by a human. An
//!    agent that can list channels can find `#announcements`, and having the id
//!    is not permission to post in it.
//! 2. **Again** — an identical publish inside 15 minutes is refused, naming the
//!    post it would have duplicated. An agent that retries after a timeout is
//!    behaving normally; publishing twice is not.
//! 3. **How much** — per-provider hourly and daily caps on published calls. X
//!    bills per call, so a loop costs money as well as credibility.
//! 4. **Who says** — a provider can be put behind an approval queue, where a
//!    call is written down instead of sent and a human runs it or drops it.
//!
//! Reads pass all four: finding the id is how a human gets asked for it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use degen_core::config::{CallContext, data_dir};
use degen_core::errors::DegenError;
use degen_core::run::Outcome;
use degen_core::tool::ToolConfig;
use degen_core::{CallPolicy, Verdict};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{ledger, oauth, queue};

/// Conservative on X because every post is billed; looser on Discord, which is
/// a chat room and free.
pub const DEFAULT_BUDGETS: &[(&str, Budget)] = &[
    ("x", Budget { per_hour: 10, per_day: 20 }),
    ("discord", Budget { per_hour: 30, per_day: 200 }),
];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub per_hour: usize,
    pub per_day: usize,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    /// Send it.
    #[default]
    Auto,
    /// Write it down and wait for a human.
    Queue,
}

/// `~/.degen-portal/policy.json`.
#[derive(Serialize, Deserialize, Default)]
pub struct Policy {
    /// Discord channel ids this machine may write to.
    #[serde(default)]
    pub channels: BTreeSet<String>,
    /// Per-provider caps. Absent means the default.
    #[serde(default)]
    pub budgets: std::collections::BTreeMap<String, Budget>,
    /// Per-provider approval mode. Absent means `auto`.
    #[serde(default)]
    pub approval: std::collections::BTreeMap<String, Approval>,
}

impl Policy {
    pub fn budget(&self, provider: &str) -> Budget {
        self.budgets
            .get(provider)
            .copied()
            .or_else(|| DEFAULT_BUDGETS.iter().find(|(p, _)| *p == provider).map(|(_, b)| *b))
            .unwrap_or(Budget { per_hour: 10, per_day: 20 })
    }

    pub fn approval(&self, provider: &str) -> Approval {
        self.approval.get(provider).copied().unwrap_or_default()
    }
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

pub fn set_budget(provider: &str, budget: Budget) -> Result<(), DegenError> {
    let mut policy = load()?;
    policy.budgets.insert(provider.to_string(), budget);
    save(&policy)
}

pub fn set_approval(provider: &str, approval: Approval) -> Result<(), DegenError> {
    let mut policy = load()?;
    policy.approval.insert(provider.to_string(), approval);
    save(&policy)
}

/// The provider a call belongs to: the package it came from.
pub fn provider_of(ctx: &CallContext<'_>) -> String {
    ctx.package.to_string()
}

/// What the call acts on, and what a human would recognise it by: the account
/// for X, the channel for Discord.
pub fn target_of(provider: &str, args: &Map<String, Value>, ctx: &CallContext<'_>) -> String {
    if provider == "discord" {
        return args.get("channel_id").and_then(Value::as_str).unwrap_or("webhook").to_string();
    }
    oauth::load()
        .ok()
        .and_then(|accounts| oauth::select(&accounts, provider, ctx.account).ok())
        .map(|a| a.id())
        .unwrap_or_else(|| provider.to_string())
}

pub struct PortalPolicy;

impl CallPolicy for PortalPolicy {
    fn check(&self, tool: &ToolConfig, args: &Map<String, Value>, ctx: &CallContext<'_>) -> Result<Verdict, DegenError> {
        if tool.method.eq_ignore_ascii_case("GET") {
            return Ok(Verdict::Send);
        }
        let policy = load()?;
        let provider = provider_of(ctx);
        let target = target_of(&provider, args, ctx);

        refuse_unallowed_channel(tool, args, &policy.channels)?;

        // Everything below is about publishing. An edit, a delete or a reaction
        // is a write, but it is not a new thing in the world.
        if tool.post_id_path.is_none() {
            return Ok(Verdict::Send);
        }

        let now = oauth::now();
        let entries = ledger::read();
        let digest = ledger::digest(&tool.name, &target, args);
        if let Some(earlier) = ledger::recent_duplicate(&entries, &digest, now) {
            let ago = (now.saturating_sub(earlier.at)) / 60;
            return Err(DegenError::InvalidArgs(format!(
                "{} already published exactly this {ago}m ago{}. Nothing was sent.\n  \
                 If it really should go out again, change the text, or wait {}m for the repeat window to pass.",
                tool.name,
                earlier.post_id.as_ref().map(|id| format!(" (id {id})")).unwrap_or_default(),
                (ledger::DEDUPE_WINDOW_SECS.saturating_sub(now - earlier.at)) / 60 + 1
            )));
        }

        let budget = policy.budget(&provider);
        for (window, cap, unit) in [(3600u64, budget.per_hour, "hour"), (86_400, budget.per_day, "day")] {
            let used = ledger::published_since(&entries, &provider, window, now);
            if used >= cap {
                let resets = ledger::resets_at(&entries, &provider, window, now).unwrap_or(now);
                let mins = (resets.saturating_sub(now)) / 60 + 1;
                return Err(DegenError::InvalidArgs(format!(
                    "{provider} has published {used} times in the last {unit}, which is its cap. Nothing was sent.\n  \
                     The next slot frees in {mins}m. A human can change the cap with:\n  \
                     degen-portal budget {provider} --per-hour {} --per-day {}",
                    budget.per_hour, budget.per_day
                )));
            }
        }

        if policy.approval(&provider) == Approval::Queue && !queue::approving() {
            let id = queue::push(&provider, ctx.account, &tool.name, args)?;
            return Ok(Verdict::Hold(Box::new(held(tool, ctx, &id))));
        }

        Ok(Verdict::Send)
    }

    fn record(&self, tool: &ToolConfig, args: &Map<String, Value>, ctx: &CallContext<'_>, outcome: &Outcome) {
        if tool.method.eq_ignore_ascii_case("GET") {
            return;
        }
        let provider = provider_of(ctx);
        let target = target_of(&provider, args, ctx);
        let post_id = ledger::post_id(tool, outcome);
        let handle = target.split_once(':').map(|(_, h)| h.to_string());
        let entry = ledger::Entry {
            at: oauth::now(),
            provider: provider.clone(),
            target: target.clone(),
            tool: tool.name.clone(),
            digest: ledger::digest(&tool.name, &target, args),
            permalink: post_id.as_deref().and_then(|id| ledger::permalink(&provider, handle.as_deref(), args, id)),
            undo: post_id.as_deref().and_then(|id| ledger::undo_for(&provider, args, id)),
            post_id,
            ok: outcome.ok,
            status: outcome.status,
        };
        if let Err(e) = ledger::append(&entry) {
            eprintln!("warning: the call went out but the ledger could not be written: {e}");
        }
    }
}

/// The outcome a held call reports: not sent, and what to do about it.
fn held(tool: &ToolConfig, ctx: &CallContext<'_>, id: &str) -> Outcome {
    Outcome {
        tool: tool.name.clone(),
        package: ctx.package.to_string(),
        ok: false,
        status: 0,
        error: Some(format!(
            "held for approval as {id}: nothing was sent. A human releases it with `degen-portal approve {id}` or discards it with `degen-portal drop {id}`."
        )),
        response: json!({ "queued": id }),
        saved_media: Vec::new(),
        stored_secrets: Vec::new(),
        duration_ms: 0,
    }
}

/// A write that names a channel needs that channel allowed.
fn refuse_unallowed_channel(tool: &ToolConfig, args: &Map<String, Value>, allowed: &BTreeSet<String>) -> Result<(), DegenError> {
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

    fn tool(name: &str, method: &str, publishes: bool) -> ToolConfig {
        serde_json::from_value(json!({
            "name": name,
            "method": method,
            "url": "https://discord.com/api/v10/channels/{channel_id}/messages",
            "post_id_path": if publishes { Some("id") } else { None },
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
        let err = refuse_unallowed_channel(&tool("discord_send_message", "POST", true), &args("999"), &allowed(&["123"])).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("999"), "{message}");
        assert!(message.contains("degen-portal discord allow 999"), "the refusal must say how to fix it: {message}");
    }

    #[test]
    fn a_write_to_an_allowed_channel_passes() {
        refuse_unallowed_channel(&tool("discord_send_message", "POST", true), &args("123"), &allowed(&["123"])).unwrap();
    }

    #[test]
    fn a_webhook_post_names_no_channel_so_the_url_is_the_scope() {
        let mut only_content = Map::new();
        only_content.insert("content".into(), Value::String("gm".into()));
        refuse_unallowed_channel(&tool("discord_send_webhook", "POST", true), &only_content, &BTreeSet::new()).unwrap();
    }

    #[test]
    fn budgets_fall_back_to_the_defaults_and_can_be_overridden() {
        let mut policy = Policy::default();
        assert_eq!(policy.budget("x"), Budget { per_hour: 10, per_day: 20 });
        assert_eq!(policy.budget("discord"), Budget { per_hour: 30, per_day: 200 });
        policy.budgets.insert("x".into(), Budget { per_hour: 1, per_day: 2 });
        assert_eq!(policy.budget("x"), Budget { per_hour: 1, per_day: 2 });
    }

    #[test]
    fn approval_is_automatic_unless_a_human_asked_otherwise() {
        let mut policy = Policy::default();
        assert_eq!(policy.approval("x"), Approval::Auto);
        policy.approval.insert("x".into(), Approval::Queue);
        assert_eq!(policy.approval("x"), Approval::Queue);
        assert_eq!(policy.approval("discord"), Approval::Auto);
    }
}
