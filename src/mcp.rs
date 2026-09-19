//! `degen-portal mcp` — the same tools over the Model Context Protocol, so a
//! client that speaks MCP (Claude Desktop, Cursor, an agent framework) can post
//! without a shell or the loopback API.
//!
//! JSON-RPC 2.0, one message per line, stdin to stdout. Nothing else may be
//! written to stdout or the client's parser breaks: every log line goes to
//! stderr.
//!
//! This is a second face on the same engine, not a second implementation. A
//! call arrives here and goes through `run::execute`, which means the channel
//! allowlist, the repeat window, the budgets and the approval queue all still
//! apply. An MCP client cannot reach past them.

use std::io::{BufRead, Write};

use degen_tools_core::errors::DegenError;
use degen_tools_core::run::RunOptions;
use degen_tools_core::{package, run};
use serde_json::{Map, Value, json};

/// Revisions whose base protocol and `tools/*` shapes this server matches. The
/// client's choice is echoed back when it is one of them, which is what the
/// handshake asks for; anything else gets the newest one we know.
const KNOWN_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"];
const DEFAULT_VERSION: &str = "2026-07-28";

/// Read messages until stdin closes.
pub fn serve() -> Result<(), DegenError> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    eprintln!("degen-portal mcp: ready on stdin/stdout");
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle(&line) else { continue };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

/// One request in, at most one response out. Notifications (no `id`) are acted
/// on and answered with nothing, as JSON-RPC requires.
fn handle(line: &str) -> Option<String> {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error_response(Value::Null, -32700, &format!("invalid JSON: {e}"))),
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    // No id means a notification: nothing to answer, so nothing to do. Running
    // the work and discarding it would be a way to post without a reply.
    let Some(id) = request.get("id").cloned() else {
        return None;
    };

    let outcome = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => tools_list(),
        "tools/call" => tools_call(&params),
        other => {
            return Some(error_response(id, -32601, &format!("this server implements initialize, ping, tools/list and tools/call, not '{other}'")));
        }
    };

    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
        Err(e) => error_response(id, -32603, &e.to_string()),
    })
}

fn error_response(id: Value, code: i32, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
    let version = if KNOWN_VERSIONS.contains(&asked) { asked } else { DEFAULT_VERSION };
    let app = degen_tools_core::app();
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": app.name, "version": app.version },
        // The rules an agent has to know before it posts — the same guide
        // `degen-portal skill` prints, minus the package listing, which
        // tools/list already carries.
        "instructions": degen_tools_core::skill::strip_frontmatter(app.overview).replace("{packages}", "").replace("{version}", app.version),
    })
}

/// The name of the extra argument every tool accepts, for picking which
/// connected account to act as.
const ACCOUNT_ARG: &str = "account";

fn tools_list() -> Result<Value, DegenError> {
    let mut tools = vec![json!({
        "name": "portal_status",
        "description": "What this machine can post right now: connected accounts and when their tokens expire, \
                        which Discord channels are writable, how much of each budget is left, and whether a \
                        provider is holding calls for approval. Check this before a burst of posts.",
        "inputSchema": { "type": "object", "properties": {} },
    })];

    for pkg in package::available()? {
        let accounts = pkg.integration.requires_env.iter().any(|v| v == crate::credentials::X_ACCESS_TOKEN);
        for tool in pkg.tools()? {
            let mut schema = tool.parameters.clone();
            if accounts && let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                properties.insert(
                    ACCOUNT_ARG.to_string(),
                    json!({ "type": "string", "description": "Which connected account to act as, e.g. 'x:handle'. Omit when only one is connected." }),
                );
            }
            tools.push(json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": schema,
            }));
        }
    }
    Ok(json!({ "tools": tools }))
}

/// Everything that can go wrong with a *tool* — an unknown name, a bad
/// argument, a refusal by the policy, an API error — is a result the model has
/// to read and act on, so it comes back as tool content with `isError`, never
/// as a JSON-RPC error. A JSON-RPC error here would mean the request itself
/// was malformed, and most clients hide those from the model.
fn tools_call(params: &Value) -> Result<Value, DegenError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| DegenError::InvalidArgs("tools/call needs a tool name".to_string()))?;
    let args: Map<String, Value> = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    Ok(match invoke(name, args) {
        Ok((text, ok)) => text_result(&text, !ok),
        Err(e) => text_result(&e.to_string(), true),
    })
}

/// Returns the text to show and whether the call succeeded.
fn invoke(name: &str, mut args: Map<String, Value>) -> Result<(String, bool), DegenError> {
    if name == "portal_status" {
        return Ok((status()?, true));
    }
    // `account` is ours, not the tool's: take it out before the schema check.
    let account = args.remove(ACCOUNT_ARG).and_then(|v| v.as_str().map(str::to_string));
    let (pkg, tool) = package::find_tool(name)?;
    degen_tools_core::args::check_known(&tool, &args)?;

    let opts = RunOptions { account, ..Default::default() };
    let outcome = run::execute(&pkg, &tool, args, &opts)?;
    let body = serde_json::to_string_pretty(&outcome.response).unwrap_or_default();
    let text = match &outcome.error {
        Some(error) => format!("{error}\n\n{body}"),
        None if outcome.saved_media.is_empty() => body,
        None => format!("saved: {}\n\n{body}", outcome.saved_media.join(", ")),
    };
    Ok((text, outcome.ok))
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// A plain-language answer to "can I post right now?".
fn status() -> Result<String, DegenError> {
    let accounts = crate::oauth::load()?;
    let policy = crate::policy::load()?;
    let entries = crate::ledger::read();
    let now = crate::oauth::now();

    let mut out = String::new();
    if accounts.accounts.is_empty() {
        out.push_str("No X account is connected. A human connects one with `degen-portal connect x`.\n");
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
    for provider in ["x", "discord"] {
        let budget = policy.budget(provider);
        let hour = crate::ledger::published_since(&entries, provider, 3600, now);
        let day = crate::ledger::published_since(&entries, provider, 86_400, now);
        let held = if policy.approval(provider) == crate::policy::Approval::Queue {
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
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `handle` reads the app's name and version, so the engine has to be named.
    fn engine() {
        crate::init();
    }

    #[test]
    fn the_handshake_echoes_a_version_the_client_knows() {
        engine();
        let response: Value = serde_json::from_str(
            &handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#).unwrap(),
        )
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], json!("2025-06-18"));
        assert!(response["result"]["capabilities"]["tools"].is_object());
        assert_eq!(response["id"], json!(1));
    }

    #[test]
    fn an_unknown_version_gets_the_newest_one_we_speak() {
        engine();
        let response: Value = serde_json::from_str(
            &handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#).unwrap(),
        )
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], json!(DEFAULT_VERSION));
    }

    #[test]
    fn a_notification_is_answered_with_silence() {
        engine();
        assert!(handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert!(handle(r#"{"jsonrpc":"2.0","method":"tools/list"}"#).is_none(), "no id means no reply, even for a real method");
    }

    #[test]
    fn a_broken_line_does_not_kill_the_session() {
        engine();
        let response: Value = serde_json::from_str(&handle("{not json").unwrap()).unwrap();
        assert_eq!(response["error"]["code"], json!(-32700));
    }

    #[test]
    fn an_unimplemented_method_says_what_is_implemented() {
        engine();
        let response: Value = serde_json::from_str(&handle(r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#).unwrap()).unwrap();
        assert_eq!(response["error"]["code"], json!(-32601));
        assert!(response["error"]["message"].as_str().unwrap().contains("tools/call"));
    }
}
