//! X's chunked upload, against a server that implements the four steps.
//!
//! The things that break in a chunked upload are the segment indices, the
//! last short chunk, and giving up before the server has finished
//! transcoding. So this asserts on what the server received, not on a return
//! value.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use parking_lot::Mutex;
use std::sync::Arc;

use degen_tools_core::run::RunOptions;
use serde_json::{Map, Value, json};

static STATE: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Seen {
    initialize: Vec<Value>,
    /// One entry per append: its `segment_index` field and its body length.
    appends: Vec<(String, usize)>,
    finalizes: usize,
    statuses: usize,
    tokens: Vec<String>,
}

/// Answers X's four endpoints. `processing` says how many STATUS polls report
/// `in_progress` before one reports `succeeded`.
fn mock_x(processing: usize) -> (u16, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let recorder = seen.clone();
    std::thread::spawn(move || {
        let mut polls = 0usize;
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let target = first.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (mut length, mut token) = (0usize, String::new());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("authorization:") {
                    token = line.splitn(2, ':').nth(1).unwrap_or("").trim().to_string();
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();

            let mut seen = recorder.lock();
            if !token.is_empty() {
                seen.tokens.push(token);
            }
            let answer = if target.ends_with("/initialize") {
                seen.initialize.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                json!({ "data": { "id": "media-77", "expires_after_secs": 86400 } })
            } else if target.ends_with("/append") {
                let text = String::from_utf8_lossy(&body);
                let index = text
                    .split("name=\"segment_index\"")
                    .nth(1)
                    .and_then(|rest| rest.split("\r\n\r\n").nth(1))
                    .map(|v| v.split("\r\n").next().unwrap_or("").to_string())
                    .unwrap_or_default();
                seen.appends.push((index, body.len()));
                json!({})
            } else if target.ends_with("/finalize") {
                seen.finalizes += 1;
                if processing == 0 {
                    json!({ "data": { "id": "media-77", "size": 1 } })
                } else {
                    json!({ "data": { "id": "media-77", "processing_info": { "state": "pending", "check_after_secs": 1 } } })
                }
            } else {
                seen.statuses += 1;
                polls += 1;
                if polls >= processing {
                    json!({ "data": { "processing_info": { "state": "succeeded" } } })
                } else {
                    json!({ "data": { "processing_info": { "state": "in_progress", "check_after_secs": 1, "progress_percent": 40 } } })
                }
            };
            drop(seen);
            let payload = answer.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (port, seen)
}

/// A HOME of this test's own, with an account connected and the media base URL
/// pointed at the mock.
fn with_home<T>(name: &str, port: u16, body: impl FnOnce() -> T) -> T {
    let guard = STATE.lock();
    let home = std::env::temp_dir().join(format!("degen-portal-video-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // Safe: the lock makes this the only thread touching HOME.
    unsafe { std::env::set_var("HOME", &home) };
    unsafe { std::env::set_var("X_MEDIA_BASE_URL", format!("http://127.0.0.1:{port}/2/media/upload")) };
    degen_portal::init();

    let mut accounts = degen_portal::oauth::Accounts::default();
    accounts.accounts.insert(
        "x:tester".to_string(),
        degen_portal::oauth::Account {
            provider: "x".into(),
            handle: "tester".into(),
            user_id: "1".into(),
            scopes: degen_portal::oauth::SCOPES.into(),
            access_token: "live-token".into(),
            expires_at: degen_portal::oauth::now() + 3600,
            refresh_token: None,
            client_id: "CID".into(),
            connected_at: 0,
        },
    );
    degen_portal::oauth::save(&accounts).unwrap();

    let out = body();
    drop(guard);
    let _ = std::fs::remove_dir_all(&home);
    out
}

fn upload(path: &std::path::Path) -> Result<degen_tools_core::run::Outcome, degen_tools_core::errors::DegenError> {
    let (pkg, tool) = degen_tools_core::package::find_tool("x_upload_video").unwrap();
    let mut args = Map::new();
    args.insert("media".into(), Value::String(path.to_string_lossy().into_owned()));
    degen_tools_core::run::execute(&pkg, &tool, args, &RunOptions::default())
}

fn write_video(name: &str, bytes: usize) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("degen-portal-{name}-{}.mp4", std::process::id()));
    std::fs::write(&path, vec![7u8; bytes]).unwrap();
    path
}

#[test]
fn a_video_is_initialized_chunked_finalized_and_waited_for() {
    let (port, seen) = mock_x(2);
    // Two full 4 MB segments and a short one.
    let file = write_video("clip", 4 * 1024 * 1024 * 2 + 1000);

    let outcome = with_home("upload", port, || upload(&file).unwrap());
    assert!(outcome.ok, "{:?}", outcome.error);
    assert_eq!(outcome.response["data"]["id"], json!("media-77"));
    assert_eq!(outcome.response["data"]["processing"], json!("succeeded"));

    let seen = seen.lock();
    assert_eq!(seen.initialize.len(), 1);
    assert_eq!(seen.initialize[0]["media_type"], json!("video/mp4"), "X picks the transcode from this");
    assert_eq!(seen.initialize[0]["total_bytes"], json!(4 * 1024 * 1024 * 2 + 1000));
    assert_eq!(seen.initialize[0]["media_category"], json!("tweet_video"));

    assert_eq!(seen.appends.len(), 3, "two full segments and the remainder");
    let indices: Vec<&str> = seen.appends.iter().map(|(i, _)| i.as_str()).collect();
    assert_eq!(indices, vec!["0", "1", "2"], "segments are indexed from zero, in order");
    assert!(seen.appends[2].1 < seen.appends[0].1, "the last segment is the short one");

    assert_eq!(seen.finalizes, 1);
    assert_eq!(seen.statuses, 2, "polled until the server said succeeded");
    assert!(seen.tokens.iter().all(|t| t == "Bearer live-token"), "every call carries the account's token");

    let _ = std::fs::remove_file(&file);
}

#[test]
fn an_upload_that_needs_no_transcode_does_not_poll() {
    let (port, seen) = mock_x(0);
    let file = write_video("small", 1024);

    let outcome = with_home("nopoll", port, || upload(&file).unwrap());
    assert!(outcome.ok, "{:?}", outcome.error);

    let seen = seen.lock();
    assert_eq!(seen.appends.len(), 1);
    assert_eq!(seen.statuses, 0, "finalize said nothing was processing, so there is nothing to wait for");

    let _ = std::fs::remove_file(&file);
}

#[test]
fn a_format_x_cannot_take_is_refused_before_any_call() {
    let (port, seen) = mock_x(0);
    let path = std::env::temp_dir().join(format!("degen-portal-still-{}.png", std::process::id()));
    std::fs::write(&path, [0u8; 8]).unwrap();

    let outcome = with_home("format", port, || upload(&path).unwrap());
    assert!(!outcome.ok);
    let error = outcome.error.unwrap();
    assert!(error.contains("x_upload_media"), "it should point at the tool that does take a png: {error}");
    assert_eq!(seen.lock().initialize.len(), 0, "nothing was opened on the server");

    let _ = std::fs::remove_file(&path);
}
