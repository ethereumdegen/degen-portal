//! Uploading a file, checked against a server that reads the body it is sent.
//!
//! The parts of a multipart request that break silently are the field names,
//! the filename, the content type and the boundary. So this asserts on the
//! bytes rather than on "the call returned ok".

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

use degen_tools_core::run::RunOptions;
use serde_json::{Map, Value};

static STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The raw request a call produced.
struct Seen {
    content_type: String,
    body: Vec<u8>,
}

fn with_home<T>(name: &str, body: impl FnOnce(u16, &mpsc::Receiver<Seen>) -> T) -> T {
    let guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("degen-portal-media-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Safe: the lock makes this the only thread touching HOME.
    unsafe { std::env::set_var("HOME", &dir) };
    degen_portal::init();

    let (port, rx) = mock_api();
    let out = body(port, &rx);
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn mock_api() -> (u16, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut length, mut content_type) = (0usize, String::new());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("content-type:") {
                    content_type = line.splitn(2, ':').nth(1).unwrap_or("").trim().to_string();
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            let _ = tx.send(Seen { content_type, body });
            let answer = "{\"data\":{\"id\":\"m1\"}}";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (port, rx)
}

/// A two-tool package: one shaped like X's upload, one like Discord's.
fn write_package(port: u16) {
    let dir = degen_tools_core::config::packages_dir().unwrap().join("up");
    std::fs::create_dir_all(dir.join("api_tools")).unwrap();
    std::fs::write(dir.join("integration.json"), r#"{"id":"up","name":"Up","version":"0.1.0","requires_env":[]}"#).unwrap();
    std::fs::write(
        dir.join("api_tools/up_media.json"),
        format!(
            r#"{{"name":"up_media","method":"POST","url":"http://127.0.0.1:{port}/2/media/upload",
                 "parameters":{{"type":"object","properties":{{"media":{{"type":"string"}}}},"required":["media"]}},
                 "body_mapping":"multipart","file_params":["media"],
                 "body_defaults":{{"media_category":"tweet_image"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("api_tools/up_attachment.json"),
        format!(
            r#"{{"name":"up_attachment","method":"POST","url":"http://127.0.0.1:{port}/channels/{{channel_id}}/messages",
                 "headers":{{"Content-Type":"application/json"}},
                 "parameters":{{"type":"object","properties":{{"channel_id":{{"type":"string"}},"file":{{"type":"string"}},"content":{{"type":"string"}}}},"required":["channel_id","file"]}},
                 "body_mapping":"multipart","file_params":["file"],"param_paths":{{"file":"files[0]"}},
                 "payload_json_field":"payload_json","post_id_path":"id"}}"#
        ),
    )
    .unwrap();
}

fn call(tool: &str, args: Value) -> Result<degen_tools_core::run::Outcome, degen_tools_core::errors::DegenError> {
    let (pkg, config) = degen_tools_core::package::find_tool(tool).unwrap();
    let args: Map<String, Value> = args.as_object().unwrap().clone();
    degen_tools_core::run::execute(&pkg, &config, args, &RunOptions::default())
}

/// A tiny but real PNG, so the bytes on the wire are a file's bytes.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89,
];

fn write_png() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("degen-portal-pixel-{}.png", std::process::id()));
    std::fs::write(&path, PNG).unwrap();
    path
}

#[test]
fn an_image_is_uploaded_as_a_named_file_part_with_its_real_type() {
    with_home("upload", |port, rx| {
        write_package(port);
        let png = write_png();

        let outcome = call("up_media", serde_json::json!({ "media": png.to_str().unwrap() })).unwrap();
        assert!(outcome.ok, "{:?}", outcome.error);
        assert_eq!(outcome.response["data"]["id"], Value::String("m1".into()));

        let seen = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(seen.content_type.starts_with("multipart/form-data; boundary="), "{}", seen.content_type);

        let body = String::from_utf8_lossy(&seen.body);
        assert!(body.contains(r#"name="media""#), "the file part keeps the parameter's name: {body}");
        assert!(body.contains(r#"filename="#), "a file part needs a filename or APIs reject it");
        assert!(body.contains(".png\""), "the real filename is sent: {body}");
        assert!(body.contains("Content-Type: image/png"), "guessed from the extension, not octet-stream: {body}");
        assert!(body.contains(r#"name="media_category""#) && body.contains("tweet_image"), "defaults ride along: {body}");
        // The file's own bytes, not a path string.
        assert!(seen.body.windows(8).any(|w| w == &PNG[..8]), "the PNG header must be in the body");
    });
}

#[test]
fn an_attachment_goes_beside_the_message_json_in_one_request() {
    with_home("attachment", |port, rx| {
        write_package(port);
        // An upload is a write like any other: the channel has to be allowed.
        degen_portal::policy::allow("777").unwrap();
        let png = write_png();

        let outcome = call(
            "up_attachment",
            serde_json::json!({ "channel_id": "777", "file": png.to_str().unwrap(), "content": "look" }),
        )
        .unwrap();
        assert!(outcome.ok, "{:?}", outcome.error);

        let seen = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(seen.content_type.starts_with("multipart/form-data"), "the tool's own Content-Type must not survive: {}", seen.content_type);

        let body = String::from_utf8_lossy(&seen.body);
        assert!(body.contains(r#"name="files[0]""#), "Discord's field name, not the parameter's: {body}");
        assert!(body.contains(r#"name="payload_json""#), "{body}");
        assert!(body.contains(r#"\"content\":\"look\""#) || body.contains(r#""content":"look""#), "{body}");
        assert!(!body.contains(r#"name="channel_id""#), "the channel went into the URL, not the form: {body}");
    });
}

#[test]
fn a_missing_file_is_refused_before_anything_is_sent() {
    with_home("missing", |port, rx| {
        write_package(port);
        let err = call("up_media", serde_json::json!({ "media": "/no/such/file.png" })).unwrap_err().to_string();
        assert!(err.contains("cannot read"), "{err}");
        assert!(err.contains("/no/such/file.png"), "{err}");
        assert!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "nothing should have been sent");
    });
}
