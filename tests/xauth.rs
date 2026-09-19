//! Which credential an X request actually carries.
//!
//! The RFC vector in the unit tests pins the signature algorithm. What this
//! checks is the wiring: that four keys in the store produce a signed request
//! with no bearer token anywhere, that a connected account produces a bearer
//! token instead, and that keys win when both are present.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

use degen_tools_core::run::RunOptions;
use parking_lot::Mutex;
use serde_json::{Map, Value};

static STATE: Mutex<()> = Mutex::new(());

/// Records the Authorization header of each request.
fn mock_x() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut authorization = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if line.to_ascii_lowercase().starts_with("authorization:") {
                    authorization = line.splitn(2, ':').nth(1).unwrap_or("").trim().to_string();
                }
            }
            recorder.lock().push(authorization);
            let body = "{\"data\":{\"id\":\"1\"}}";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, seen)
}

fn with_home<T>(name: &str, port: u16, body: impl FnOnce() -> T) -> T {
    let guard = STATE.lock();
    let home = std::env::temp_dir().join(format!("degen-portal-xauth-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // Safe: the lock makes this the only thread touching HOME.
    unsafe { std::env::set_var("HOME", &home) };
    degen_portal::init();

    // An "x" package pointed at the mock. The signer keys off the package id.
    let dir = degen_tools_core::config::packages_dir().unwrap().join("x");
    std::fs::create_dir_all(dir.join("api_tools")).unwrap();
    std::fs::write(dir.join("integration.json"), r#"{"id":"x","name":"X","version":"0.1.0","requires_env":[]}"#).unwrap();
    std::fs::write(
        dir.join("api_tools/x_post.json"),
        format!(
            r#"{{"name":"x_post","method":"POST","url":"http://127.0.0.1:{port}/2/tweets",
                 "parameters":{{"type":"object","properties":{{"text":{{"type":"string"}}}},"required":["text"]}},
                 "body_mapping":"params","post_id_path":"data.id"}}"#
        ),
    )
    .unwrap();

    let out = body();
    drop(guard);
    let _ = std::fs::remove_dir_all(&home);
    out
}

fn store_keys(pairs: &[(&str, &str)]) {
    let mut creds = degen_tools_core::config::load_credentials().unwrap();
    for (name, value) in pairs {
        creds.keys.insert((*name).to_string(), (*value).to_string());
    }
    degen_tools_core::config::save_credentials(&creds).unwrap();
}

fn connect_account() {
    let mut accounts = degen_portal::oauth::Accounts::default();
    accounts.accounts.insert(
        "x:tester".to_string(),
        degen_portal::oauth::Account {
            provider: "x".into(),
            handle: "tester".into(),
            user_id: "1".into(),
            scopes: degen_portal::oauth::SCOPES.into(),
            access_token: "BEARER-TOKEN".into(),
            expires_at: degen_portal::oauth::now() + 3600,
            refresh_token: None,
            client_id: "CID".into(),
            connected_at: 0,
        },
    );
    degen_portal::oauth::save(&accounts).unwrap();
}

fn post(text: &str) -> Result<degen_tools_core::run::Outcome, degen_tools_core::errors::DegenError> {
    let (pkg, tool) = degen_tools_core::package::find_tool("x_post").unwrap();
    let mut args = Map::new();
    args.insert("text".into(), Value::String(text.into()));
    degen_tools_core::run::execute(&pkg, &tool, args, &RunOptions::default())
}

#[test]
fn four_keys_produce_a_signed_request_and_no_bearer_token() {
    let (port, seen) = mock_x();
    with_home("oauth1", port, || {
        store_keys(&[
            ("X_API_KEY", "consumer-key"),
            ("X_API_SECRET", "consumer-secret"),
            ("X_ACCESS_TOKEN", "access-token"),
            ("X_ACCESS_TOKEN_SECRET", "access-secret"),
        ]);
        assert!(post("gm").unwrap().ok);

        let seen = seen.lock();
        let header = seen.last().expect("a request arrived");
        assert!(header.starts_with("OAuth "), "{header}");
        assert!(header.contains(r#"oauth_consumer_key="consumer-key""#), "{header}");
        assert!(header.contains(r#"oauth_token="access-token""#), "{header}");
        assert!(header.contains(r#"oauth_signature_method="HMAC-SHA1""#), "{header}");
        assert!(header.contains("oauth_signature="), "{header}");
        assert!(!header.contains("Bearer"), "no bearer token should be anywhere near this: {header}");
        // The secrets sign the request; they are never sent.
        assert!(!header.contains("consumer-secret") && !header.contains("access-secret"), "{header}");
    });
}

#[test]
fn a_connected_account_is_used_when_there_are_no_keys() {
    let (port, seen) = mock_x();
    with_home("oauth2", port, || {
        connect_account();
        assert!(post("gm").unwrap().ok);
        let seen = seen.lock();
        assert_eq!(seen.last().unwrap(), "Bearer BEARER-TOKEN");
    });
}

#[test]
fn keys_win_over_a_connected_account() {
    let (port, seen) = mock_x();
    with_home("both", port, || {
        connect_account();
        store_keys(&[
            ("X_API_KEY", "consumer-key"),
            ("X_API_SECRET", "consumer-secret"),
            ("X_ACCESS_TOKEN", "access-token"),
            ("X_ACCESS_TOKEN_SECRET", "access-secret"),
        ]);
        assert!(post("gm").unwrap().ok);
        let seen = seen.lock();
        let header = seen.last().unwrap();
        assert!(header.starts_with("OAuth "), "keys need no browser and no refresh, so they win: {header}");
    });
}

#[test]
fn with_neither_the_error_names_both_ways_in() {
    let (port, _seen) = mock_x();
    with_home("neither", port, || {
        let err = post("gm").unwrap_err().to_string();
        assert!(err.contains("connect x"), "{err}");
    });
}

/// Each request gets its own nonce and timestamp, so two identical posts do
/// not carry the same signature — a replayed one would be rejected by X.
#[test]
fn two_requests_are_signed_separately() {
    let (port, seen) = mock_x();
    with_home("nonce", port, || {
        store_keys(&[
            ("X_API_KEY", "k"),
            ("X_API_SECRET", "s"),
            ("X_ACCESS_TOKEN", "t"),
            ("X_ACCESS_TOKEN_SECRET", "ts"),
        ]);
        post("one").unwrap();
        post("two").unwrap();
        let seen = seen.lock();
        assert_eq!(seen.len(), 2);
        assert_ne!(seen[0], seen[1], "same credential, different signature");
    });
}
