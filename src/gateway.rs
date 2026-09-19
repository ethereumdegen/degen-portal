//! Discord's gateway: an outbound WebSocket that pushes messages as they are
//! posted, instead of polling for them.
//!
//! This is the one long-running thing in a tool made of one-shot commands, so
//! it is a command of its own — `degen-portal listen` — and not something
//! `serve` starts behind your back. It runs while you run it. Close it and
//! nothing is listening, which is the same deal as everything else here.
//!
//! It only reads. Each message is printed as one JSON line, so an agent can
//! pipe it, `grep` it, or read it a line at a time; replying is a separate
//! `discord_send_message` call that goes through the allowlist like any other.
//! That split is deliberate: a listener that could also post is a robot that
//! answers itself.

use std::time::Duration;

use degen_tools_core::config::{load_credentials, lookup_credential};
use degen_tools_core::errors::DegenError;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// GUILDS (1) | GUILD_MESSAGES (512) | MESSAGE_CONTENT (32768).
///
/// The last one is privileged: without it enabled in the Developer Portal,
/// every `content` arrives empty and the listener is useless for reading.
pub const INTENTS: u64 = 1 | 512 | 32768;

/// One message, as a line of JSON on stdout.
#[derive(serde::Serialize)]
pub struct Heard {
    pub message_id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub author_id: String,
    pub author: String,
    pub bot: bool,
    pub content: String,
    pub timestamp: String,
}

/// `degen-portal listen`. Blocks until interrupted.
pub fn listen(channels: Vec<String>, include_bots: bool) -> Result<(), DegenError> {
    let token = lookup_credential(&load_credentials()?, "DISCORD_BOT_TOKEN").ok_or_else(|| {
        DegenError::CredentialNotFound("DISCORD_BOT_TOKEN".to_string())
    })?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    eprintln!("listening on the Discord gateway; one JSON line per message, Ctrl-C to stop");
    if !channels.is_empty() {
        eprintln!("only channels: {}", channels.join(", "));
    }
    eprintln!("(content is empty unless the app has the Message Content intent enabled)");

    runtime.block_on(async move {
        let mut backoff = 1u64;
        loop {
            let outcome = session(&url(), &token, INTENTS, &mut |heard: Heard| {
                if !include_bots && heard.bot {
                    return;
                }
                if !channels.is_empty() && !channels.contains(&heard.channel_id) {
                    return;
                }
                println!("{}", serde_json::to_string(&heard).unwrap_or_default());
                use std::io::Write;
                let _ = std::io::stdout().flush();
            })
            .await;
            match outcome {
                // A gateway session ends all the time — Discord recycles them.
                // Reconnecting is normal operation, not error recovery.
                Ok(()) => backoff = 1,
                Err(e) => eprintln!("gateway: {e}"),
            }
            eprintln!("gateway: reconnecting in {backoff}s");
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        }
    })
}

/// One connection, from HELLO to the socket closing.
///
/// Public so a test can point it at a local server: the handshake, the
/// heartbeat and the dispatch filtering are the parts worth checking, and
/// none of them can be checked against Discord itself.
pub async fn session(
    url: &str,
    token: &str,
    intents: u64,
    on_message: &mut dyn FnMut(Heard),
) -> Result<(), DegenError> {
    let (stream, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| DegenError::Http(format!("connecting to the gateway failed: {e}")))?;
    let (mut sink, mut source) = stream.split();

    // HELLO first, always: it carries the heartbeat interval.
    let hello = next_json(&mut source).await?.ok_or_else(|| DegenError::Http("the gateway closed before saying hello".to_string()))?;
    let interval = hello["d"]["heartbeat_interval"]
        .as_u64()
        .ok_or_else(|| DegenError::Http(format!("the gateway's hello had no heartbeat interval: {hello}")))?;

    send(&mut sink, &json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents,
            "properties": { "os": std::env::consts::OS, "browser": "degen-portal", "device": "degen-portal" },
        }
    }))
    .await?;

    let mut beat = tokio::time::interval(Duration::from_millis(interval));
    beat.tick().await; // the first tick is immediate
    let mut sequence: Option<u64> = None;

    loop {
        tokio::select! {
            _ = beat.tick() => {
                send(&mut sink, &json!({ "op": 1, "d": sequence })).await?;
            }
            message = next_json(&mut source) => {
                let Some(message) = message? else { return Ok(()) };
                if let Some(s) = message["s"].as_u64() {
                    sequence = Some(s);
                }
                match message["op"].as_u64().unwrap_or(u64::MAX) {
                    // Dispatch.
                    0 => {
                        if message["t"].as_str() == Some("MESSAGE_CREATE")
                            && let Some(heard) = heard_from(&message["d"])
                        {
                            on_message(heard);
                        }
                    }
                    // The gateway wants a heartbeat now.
                    1 => send(&mut sink, &json!({ "op": 1, "d": sequence })).await?,
                    // Reconnect, or a session we cannot resume: both mean start over.
                    7 | 9 => return Ok(()),
                    // Heartbeat acknowledged; hello was handled above.
                    10 | 11 => {}
                    _ => {}
                }
            }
        }
    }
}

fn heard_from(d: &Value) -> Option<Heard> {
    Some(Heard {
        message_id: d["id"].as_str()?.to_string(),
        channel_id: d["channel_id"].as_str()?.to_string(),
        guild_id: d["guild_id"].as_str().map(str::to_string),
        author_id: d["author"]["id"].as_str().unwrap_or_default().to_string(),
        author: d["author"]["username"].as_str().unwrap_or_default().to_string(),
        bot: d["author"]["bot"].as_bool().unwrap_or(false),
        content: d["content"].as_str().unwrap_or_default().to_string(),
        timestamp: d["timestamp"].as_str().unwrap_or_default().to_string(),
    })
}

async fn send<S>(sink: &mut S, value: &Value) -> Result<(), DegenError>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    sink.send(Message::Text(value.to_string().into()))
        .await
        .map_err(|e| DegenError::Http(format!("sending to the gateway failed: {e}")))
}

/// The next JSON frame, or `None` when the socket closed.
async fn next_json<S>(source: &mut S) -> Result<Option<Value>, DegenError>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(message) = source.next().await {
        match message.map_err(|e| DegenError::Http(format!("gateway read failed: {e}")))? {
            Message::Text(text) => {
                return serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|e| DegenError::Http(format!("the gateway sent something unexpected: {e}")));
            }
            Message::Close(_) => return Ok(None),
            _ => continue,
        }
    }
    Ok(None)
}

/// Tests point this at a local server. Only loopback is honoured: this URL
/// receives the bot token.
fn url() -> String {
    match lookup_credential(&load_credentials().unwrap_or_default(), "DISCORD_GATEWAY_URL") {
        Some(url) if url.starts_with("ws://127.0.0.1:") || url.starts_with("ws://localhost:") => url,
        Some(url) => {
            eprintln!("warning: ignoring DISCORD_GATEWAY_URL={url} — only a loopback address may replace {GATEWAY}");
            GATEWAY.to_string()
        }
        None => GATEWAY.to_string(),
    }
}
