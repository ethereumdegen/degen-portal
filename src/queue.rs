//! Calls a human has not agreed to yet.
//!
//! A provider set to `queue` writes the call down instead of sending it. The
//! agent is told what was held and how to ask about it; a human lists the
//! queue, reads the exact text, and releases or drops it. Approving runs the
//! call for real — through every other gate, because a human agreeing to the
//! text is not agreeing to blow the daily budget.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use degen_core::config::data_dir;
use degen_core::errors::DegenError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Set only while an approved call is being replayed, so the approval gate
/// does not queue the very call it was asked to release.
static APPROVING: AtomicBool = AtomicBool::new(false);

pub fn approving() -> bool {
    APPROVING.load(Ordering::SeqCst)
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Held {
    pub id: String,
    pub at: u64,
    pub provider: String,
    #[serde(default)]
    pub account: Option<String>,
    pub tool: String,
    pub args: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Queue {
    #[serde(default)]
    pub next: u64,
    #[serde(default)]
    pub held: Vec<Held>,
}

fn path() -> Result<PathBuf, DegenError> {
    Ok(data_dir()?.join("queue.json"))
}

pub fn load() -> Result<Queue, DegenError> {
    let path = path()?;
    if !path.is_file() {
        return Ok(Queue::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(&path)?)?)
}

pub fn save(queue: &Queue) -> Result<(), DegenError> {
    fs::write(path()?, serde_json::to_string_pretty(queue)?)?;
    Ok(())
}

/// Hold a call. Returns the id a human will refer to it by.
pub fn push(provider: &str, account: Option<&str>, tool: &str, args: &Map<String, Value>) -> Result<String, DegenError> {
    let mut queue = load()?;
    queue.next += 1;
    let id = format!("q{}", queue.next);
    queue.held.push(Held {
        id: id.clone(),
        at: crate::oauth::now(),
        provider: provider.to_string(),
        account: account.map(str::to_string),
        tool: tool.to_string(),
        args: args.clone(),
    });
    save(&queue)?;
    Ok(id)
}

pub fn take(id: &str) -> Result<Held, DegenError> {
    let mut queue = load()?;
    let index = queue
        .held
        .iter()
        .position(|h| h.id == id)
        .ok_or_else(|| DegenError::InvalidArgs(format!("nothing is held as '{id}'. `degen-portal queue` lists what is.")))?;
    let held = queue.held.remove(index);
    save(&queue)?;
    Ok(held)
}

/// `degen-portal queue`.
pub fn list() -> Result<(), DegenError> {
    let queue = load()?;
    if queue.held.is_empty() {
        println!("Nothing is waiting for approval.");
        return Ok(());
    }
    for held in &queue.held {
        let age = (crate::oauth::now().saturating_sub(held.at)) / 60;
        println!("{}  {} ({}, {age}m ago)", held.id, held.tool, held.account.as_deref().unwrap_or(&held.provider));
        for (key, value) in &held.args {
            let shown = value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string());
            println!("    {key}: {shown}");
        }
    }
    println!("\nRelease with `degen-portal approve <id>`, discard with `degen-portal drop <id>`.");
    Ok(())
}

/// Run a held call for real.
pub fn approve(id: &str) -> Result<(), DegenError> {
    let held = take(id)?;
    let (pkg, tool) = degen_core::package::find_tool(&held.tool)?;
    let opts = degen_core::run::RunOptions { account: held.account.clone(), ..Default::default() };

    APPROVING.store(true, Ordering::SeqCst);
    let outcome = degen_core::run::execute(&pkg, &tool, held.args.clone(), &opts);
    APPROVING.store(false, Ordering::SeqCst);

    let outcome = outcome.inspect_err(|_| {
        // The call never happened, so put it back rather than losing the text.
        if let Ok(mut queue) = load() {
            queue.held.push(held.clone());
            let _ = save(&queue);
        }
    })?;
    println!("{}", serde_json::to_string_pretty(&outcome.response)?);
    match outcome.error {
        Some(error) => Err(DegenError::Http(error)),
        None => Ok(()),
    }
}

pub fn drop_held(id: &str) -> Result<(), DegenError> {
    let held = take(id)?;
    println!("dropped {} ({})", held.id, held.tool);
    Ok(())
}
