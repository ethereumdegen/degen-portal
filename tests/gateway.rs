//! The gateway session against a WebSocket server that behaves like Discord's.
//!
//! What breaks here is invisible from the outside: an IDENTIFY without the
//! Message Content intent (every message arrives blank), a heartbeat that
//! never goes out (Discord hangs up after a minute), or a dispatch parsed into
//! the wrong shape. So the mock asserts on what the client sent, and on what
//! the client handed back.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[derive(Default)]
struct Seen {
    identify: Option<Value>,
    heartbeats: usize,
}

/// A gateway that says hello, waits for IDENTIFY, then pushes two messages and
/// closes. `interval` is the heartbeat interval it asks for.
async fn mock_gateway(interval: u64) -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Seen::default()));
    let recorder = seen.clone();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();

        socket
            .send(Message::Text(json!({ "op": 10, "d": { "heartbeat_interval": interval } }).to_string().into()))
            .await
            .unwrap();

        // Wait for IDENTIFY before pushing anything.
        while let Some(Ok(message)) = socket.next().await {
            let Message::Text(text) = message else { continue };
            let frame: Value = serde_json::from_str(&text).unwrap();
            match frame["op"].as_u64() {
                Some(2) => {
                    recorder.lock().identify = Some(frame);
                    break;
                }
                Some(1) => recorder.lock().heartbeats += 1,
                _ => {}
            }
        }

        socket
            .send(Message::Text(
                json!({ "op": 0, "s": 1, "t": "READY", "d": { "session_id": "abc", "user": { "id": "bot-1" } } }).to_string().into(),
            ))
            .await
            .unwrap();

        for (id, author, bot, content) in [
            ("m1", "human", false, "gm"),
            ("m2", "the bot itself", true, "gm back"),
        ] {
            socket
                .send(Message::Text(
                    json!({
                        "op": 0, "s": 2, "t": "MESSAGE_CREATE",
                        "d": {
                            "id": id, "channel_id": "chan-9", "guild_id": "guild-1",
                            "content": content, "timestamp": "2026-09-19T00:00:00Z",
                            "author": { "id": format!("u-{id}"), "username": author, "bot": bot }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        }

        // Let the heartbeat fire at least once before hanging up.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(interval * 3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    if frame["op"].as_u64() == Some(1) {
                        recorder.lock().heartbeats += 1;
                        // Acknowledge, as Discord does.
                        socket.send(Message::Text(json!({ "op": 11 }).to_string().into())).await.unwrap();
                    }
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_)) | None) => break,
                Err(_) => {}
            }
        }
        let _ = socket.close(None).await;
    });

    (url, seen)
}

#[tokio::test]
async fn a_session_identifies_heartbeats_and_hands_back_messages() {
    let (url, seen) = mock_gateway(120).await;
    let heard = Arc::new(Mutex::new(Vec::new()));
    let collector = heard.clone();

    let mut handler = move |message: degen_portal::gateway::Heard| collector.lock().push(message);
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        degen_portal::gateway::session(&url, "bot-token", degen_portal::gateway::INTENTS, &mut handler),
    )
    .await
    .expect("the session should end when the server closes");
    outcome.expect("a closed socket is a normal end, not an error");

    let seen = seen.lock();
    let identify = seen.identify.as_ref().expect("the client must identify before anything else");
    assert_eq!(identify["d"]["token"], json!("bot-token"));
    let intents = identify["d"]["intents"].as_u64().unwrap();
    assert_eq!(intents & 32768, 32768, "without MESSAGE_CONTENT every message arrives blank");
    assert_eq!(intents & 512, 512, "GUILD_MESSAGES is what delivers them at all");
    assert!(seen.heartbeats >= 1, "Discord hangs up on a client that never beats");

    let heard = heard.lock();
    assert_eq!(heard.len(), 2, "the session hands back every message; filtering is the caller's job");
    assert_eq!(heard[0].message_id, "m1");
    assert_eq!(heard[0].channel_id, "chan-9");
    assert_eq!(heard[0].guild_id.as_deref(), Some("guild-1"));
    assert_eq!(heard[0].author, "human");
    assert_eq!(heard[0].content, "gm");
    assert!(!heard[0].bot);
    assert!(heard[1].bot, "the bot's own message is marked, so `listen` can drop it");
}

#[tokio::test]
async fn a_gateway_that_never_says_hello_is_an_error_not_a_hang() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = socket.close(None).await;
    });

    let mut handler = |_: degen_portal::gateway::Heard| {};
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        degen_portal::gateway::session(&url, "t", degen_portal::gateway::INTENTS, &mut handler),
    )
    .await
    .expect("it must not hang");
    assert!(outcome.is_err(), "no hello means no interval, which is not something to guess at");
}
