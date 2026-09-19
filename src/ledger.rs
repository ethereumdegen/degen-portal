//! What was published, when, and how to take it back.
//!
//! One append-only file, `~/.degen-portal/ledger.jsonl`, answers three
//! questions that all need the same facts:
//!
//! - **Did I already post this?** An agent that retries after a timeout would
//!   otherwise publish the same thing twice.
//! - **How much have I posted?** X bills per call, and a loop is expensive in
//!   money as well as reputation.
//! - **How do I undo it?** Every published entry carries the call that deletes
//!   it, worked out while the context is still at hand.
//!
//! A call that publishes is one whose tool declares `post_id_path`. Edits,
//! deletes and reactions are written down but neither counted nor deduplicated.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use degen_tools_core::config::data_dir;
use degen_tools_core::errors::DegenError;
use degen_tools_core::paths;
use degen_tools_core::run::Outcome;
use degen_tools_core::tool::ToolConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// How long an identical call counts as a repeat.
pub const DEDUPE_WINDOW_SECS: u64 = 900;

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    /// Unix seconds.
    pub at: u64,
    pub provider: String,
    /// Account id for X, channel id for Discord: what the call acted on.
    pub target: String,
    pub tool: String,
    /// Identifies a repeat of the same call; never the content itself.
    pub digest: String,
    /// Set when the call published something.
    #[serde(default)]
    pub post_id: Option<String>,
    #[serde(default)]
    pub permalink: Option<String>,
    /// The call that takes it back, ready to run.
    #[serde(default)]
    pub undo: Option<Undo>,
    pub ok: bool,
    pub status: u16,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Undo {
    pub tool: String,
    pub args: Map<String, Value>,
}

fn path() -> Result<PathBuf, DegenError> {
    Ok(data_dir()?.join("ledger.jsonl"))
}

pub fn append(entry: &Entry) -> Result<(), DegenError> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path()?)?;
    use std::io::Write;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

/// Every entry, oldest first. A broken line is skipped rather than fatal: the
/// ledger is evidence, and half a line should not stop a post.
pub fn read() -> Vec<Entry> {
    let Ok(path) = path() else { return Vec::new() };
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

/// Published entries for a provider inside the last `window` seconds.
pub fn published_since(entries: &[Entry], provider: &str, window: u64, now: u64) -> usize {
    entries
        .iter()
        .filter(|e| e.ok && e.post_id.is_some() && e.provider == provider && e.at + window > now)
        .count()
}

/// When the oldest call inside the window falls out of it, freeing a slot.
pub fn resets_at(entries: &[Entry], provider: &str, window: u64, now: u64) -> Option<u64> {
    entries
        .iter()
        .filter(|e| e.ok && e.post_id.is_some() && e.provider == provider && e.at + window > now)
        .map(|e| e.at + window)
        .min()
}

/// The same tool, target and arguments — not the response, which may differ.
pub fn digest(tool: &str, target: &str, args: &Map<String, Value>) -> String {
    let mut hasher = DefaultHasher::new();
    tool.hash(&mut hasher);
    target.hash(&mut hasher);
    // A BTreeMap iterates in key order, so argument order cannot change this.
    for (key, value) in args.iter().collect::<std::collections::BTreeMap<_, _>>() {
        key.hash(&mut hasher);
        value.to_string().hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// An earlier successful publish of exactly this call, still inside the window.
pub fn recent_duplicate<'a>(entries: &'a [Entry], digest: &str, now: u64) -> Option<&'a Entry> {
    entries
        .iter()
        .rev()
        .find(|e| e.ok && e.post_id.is_some() && e.digest == digest && e.at + DEDUPE_WINDOW_SECS > now)
}

/// Pull the created id out of a response, following the tool's `post_id_path`.
pub fn post_id(tool: &ToolConfig, outcome: &Outcome) -> Option<String> {
    let path = tool.post_id_path.as_deref()?;
    paths::find(&outcome.response, path)
        .into_iter()
        .find_map(|(_, value)| value.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// Where a human can go and look at what was posted. `handle` is absent when
/// the call did not act as a named account, and a guessed URL is worse than
/// none: it would 404 and look like the post failed.
pub fn permalink(provider: &str, handle: Option<&str>, args: &Map<String, Value>, post_id: &str) -> Option<String> {
    match provider {
        "x" => handle.map(|handle| format!("https://x.com/{handle}/status/{post_id}")),
        // Discord needs the guild id to link, and a channel post does not carry
        // one. The channel and message ids are enough to find it.
        "discord" => args
            .get("channel_id")
            .and_then(Value::as_str)
            .map(|channel| format!("discord: channel {channel}, message {post_id}")),
        _ => None,
    }
}

/// The call that deletes what was just published.
pub fn undo_for(provider: &str, args: &Map<String, Value>, post_id: &str) -> Option<Undo> {
    let mut undo_args = Map::new();
    match provider {
        "x" => {
            undo_args.insert("id".into(), Value::String(post_id.to_string()));
            Some(Undo { tool: "x_delete_post".into(), args: undo_args })
        }
        "discord" => {
            let channel = args.get("channel_id").and_then(Value::as_str)?;
            undo_args.insert("channel_id".into(), Value::String(channel.to_string()));
            undo_args.insert("message_id".into(), Value::String(post_id.to_string()));
            Some(Undo { tool: "discord_delete_message".into(), args: undo_args })
        }
        // A webhook message can be edited or deleted only through the webhook
        // URL it came from, which is a credential, not an argument here.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("text".into(), Value::String(text.into()));
        m
    }

    fn entry(at: u64, digest: &str, published: bool, ok: bool) -> Entry {
        Entry {
            at,
            provider: "x".into(),
            target: "x:degen".into(),
            tool: "x_post".into(),
            digest: digest.into(),
            post_id: published.then(|| "1790".to_string()),
            permalink: None,
            undo: None,
            ok,
            status: if ok { 201 } else { 429 },
        }
    }

    #[test]
    fn the_same_call_digests_the_same_whatever_the_argument_order() {
        let mut a = Map::new();
        a.insert("text".into(), Value::String("gm".into()));
        a.insert("reply_to".into(), Value::String("1".into()));
        let mut b = Map::new();
        b.insert("reply_to".into(), Value::String("1".into()));
        b.insert("text".into(), Value::String("gm".into()));
        assert_eq!(digest("x_post", "x:degen", &a), digest("x_post", "x:degen", &b));
    }

    #[test]
    fn different_text_target_or_tool_are_different_calls() {
        let base = digest("x_post", "x:degen", &args("gm"));
        assert_ne!(base, digest("x_post", "x:degen", &args("gn")));
        assert_ne!(base, digest("x_post", "x:other", &args("gm")));
        assert_ne!(base, digest("x_reply", "x:degen", &args("gm")));
    }

    #[test]
    fn a_repeat_inside_the_window_is_found_and_an_old_one_is_not() {
        let now = 10_000;
        let entries = vec![entry(now - 60, "abc", true, true)];
        assert!(recent_duplicate(&entries, "abc", now).is_some());
        assert!(recent_duplicate(&entries, "xyz", now).is_none());

        let stale = vec![entry(now - DEDUPE_WINDOW_SECS - 1, "abc", true, true)];
        assert!(recent_duplicate(&stale, "abc", now).is_none(), "past the window it is a new post, not a repeat");
    }

    #[test]
    fn a_failed_call_is_not_a_duplicate_to_block_the_retry() {
        let now = 10_000;
        let failed = vec![entry(now - 60, "abc", false, false)];
        assert!(recent_duplicate(&failed, "abc", now).is_none(), "nothing was published, so this is not a repeat");
    }

    #[test]
    fn only_successful_publishes_count_against_a_budget() {
        let now = 10_000;
        let entries = vec![
            entry(now - 60, "a", true, true),
            entry(now - 120, "b", true, false), // failed
            entry(now - 180, "c", false, true), // a delete: no post id
            entry(now - 7200, "d", true, true), // outside the hour
        ];
        assert_eq!(published_since(&entries, "x", 3600, now), 1);
        assert_eq!(published_since(&entries, "x", 86400, now), 2);
        assert_eq!(published_since(&entries, "discord", 3600, now), 0);
    }

    #[test]
    fn the_budget_frees_up_when_the_oldest_call_ages_out() {
        let now = 10_000;
        let entries = vec![entry(now - 600, "a", true, true), entry(now - 60, "b", true, true)];
        assert_eq!(resets_at(&entries, "x", 3600, now), Some(now - 600 + 3600));
    }

    #[test]
    fn a_post_by_an_unnamed_account_gets_no_guessed_link() {
        assert!(permalink("x", None, &Map::new(), "1790").is_none());
        assert_eq!(
            permalink("x", Some("degen"), &Map::new(), "1790").as_deref(),
            Some("https://x.com/degen/status/1790")
        );
    }

    #[test]
    fn undo_knows_how_to_delete_each_kind_of_post() {
        let x = undo_for("x", &Map::new(), "1790").unwrap();
        assert_eq!(x.tool, "x_delete_post");
        assert_eq!(x.args["id"], Value::String("1790".into()));

        let mut discord_args = Map::new();
        discord_args.insert("channel_id".into(), Value::String("777".into()));
        let d = undo_for("discord", &discord_args, "999").unwrap();
        assert_eq!(d.tool, "discord_delete_message");
        assert_eq!(d.args["channel_id"], Value::String("777".into()));
        assert_eq!(d.args["message_id"], Value::String("999".into()));

        assert!(undo_for("discord", &Map::new(), "999").is_none(), "a webhook post has no channel to delete from");
    }
}
