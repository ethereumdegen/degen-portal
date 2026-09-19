//! The two halves of the OAuth flow that only fail in the real world: the
//! browser coming back to a socket on this machine, and an expired token being
//! swapped for a fresh one without losing the rotated refresh token.
//!
//! Both run against a local server rather than X, so they need no credentials
//! and no network.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use degen_portal::oauth::{self, Account, Accounts};

/// Every test in this file keeps its state in a temp HOME, so a run never
/// touches the real one.
///
/// `HOME` is process-global, so the tests that touch the account file take a
/// lock, point HOME at their own directory, and start from an empty store.
/// Nothing here can reach the real `~/.degen-portal`.
static STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_state_home<T>(name: &str, body: impl FnOnce() -> T) -> T {
    let guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("degen-portal-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Safe: the lock makes this the only thread reading or writing HOME.
    unsafe { std::env::set_var("HOME", &dir) };
    degen_portal::init();
    let out = body();
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn get(port: u16, target: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(stream, "GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).unwrap();
    body
}

#[test]
fn the_browser_callback_is_caught_and_answered() {
    let port = free_port();
    let waiting = std::thread::spawn(move || oauth::wait_for_callback(port, Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(100));

    // A browser asking for something else must not end the wait.
    let favicon = get(port, "/favicon.ico");
    assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");

    let answer = get(port, "/oauth/callback?code=the%2Fcode&state=st4te");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("Connected"), "the human needs to be told to go back: {answer}");

    let callback = waiting.join().unwrap().unwrap();
    assert_eq!(callback.code.as_deref(), Some("the/code"));
    assert_eq!(callback.state.as_deref(), Some("st4te"));
    assert!(callback.error.is_none());
}

#[test]
fn a_refusal_comes_back_as_an_error_not_a_code() {
    let port = free_port();
    let waiting = std::thread::spawn(move || oauth::wait_for_callback(port, Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(100));

    get(port, "/oauth/callback?error=access_denied&error_description=user+said+no&state=st4te");

    let callback = waiting.join().unwrap().unwrap();
    assert!(callback.code.is_none());
    assert_eq!(callback.error.as_deref(), Some("user said no"));
}

/// A token endpoint that answers one request, recording what it was sent.
fn mock_token_endpoint(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://127.0.0.1:{}/token", listener.local_addr().unwrap().port());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
            if line == "\r\n" || line.is_empty() {
                break;
            }
            request.push_str(&line);
        }
        let mut form = vec![0u8; length];
        reader.read_exact(&mut form).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        String::from_utf8(form).unwrap()
    });
    (url, handle)
}

#[test]
fn an_expired_token_is_refreshed_and_the_rotated_refresh_token_is_kept() {
    with_state_home("refresh", || {
    let mut accounts = Accounts::default();
    accounts.accounts.insert(
        "x:tester".to_string(),
        Account {
            provider: "x".into(),
            handle: "tester".into(),
            user_id: "42".into(),
            scopes: oauth::SCOPES.into(),
            access_token: "stale".into(),
            expires_at: 1, // long expired
            refresh_token: Some("old-refresh".into()),
            client_id: "CID".into(),
            connected_at: 0,
        },
    );
    oauth::save(&accounts).unwrap();

    let (url, server) = mock_token_endpoint(
        r#"{"access_token":"fresh","refresh_token":"rotated","expires_in":7200,"token_type":"bearer"}"#,
    );
    let token = oauth::access_token("x:tester", &url, None).unwrap();
    let sent = server.join().unwrap();

    assert_eq!(token, "fresh");
    // X requires client_id in the body for a public client, and the grant type.
    assert!(sent.contains("grant_type=refresh_token"), "{sent}");
    assert!(sent.contains("refresh_token=old-refresh"), "{sent}");
    assert!(sent.contains("client_id=CID"), "{sent}");

    // Persisted, or the next process would refresh with a token X just rotated
    // away and lock the account out.
    let stored = oauth::load().unwrap();
    let account = &stored.accounts["x:tester"];
    assert_eq!(account.access_token, "fresh");
    assert_eq!(account.refresh_token.as_deref(), Some("rotated"));
    assert!(!account.expired(), "a token good for two hours must not read as expired");
    });
}

#[test]
fn a_live_token_is_used_as_is_and_no_refresh_happens() {
    with_state_home("live", || {
    let mut accounts = Accounts::default();
    accounts.accounts.insert(
        "x:live".to_string(),
        Account {
            provider: "x".into(),
            handle: "live".into(),
            user_id: "7".into(),
            scopes: oauth::SCOPES.into(),
            access_token: "still-good".into(),
            expires_at: oauth::now() + 3600,
            refresh_token: Some("unused".into()),
            client_id: "CID".into(),
            connected_at: 0,
        },
    );
    oauth::save(&accounts).unwrap();
    // An unroutable token URL: reaching it at all would fail the test.
    let token = oauth::access_token("x:live", "http://127.0.0.1:1/token", None).unwrap();
    assert_eq!(token, "still-good");
    });
}

/// A one-shot server that records the request it was sent and answers `{}`.
fn mock_api() -> (u16, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line);
        }
        let body = "{\"data\":{\"id\":\"1\"}}";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        head
    });
    (port, handle)
}

/// The whole chain: a connected account, `$X_ACCESS_TOKEN` in a package's
/// header, and the token arriving at the server as a bearer credential —
/// without the caller ever naming it.
#[test]
fn a_connected_account_becomes_the_authorization_header() {
    with_state_home("inject", || {
        let mut accounts = Accounts::default();
        accounts.accounts.insert(
            "x:inject".to_string(),
            Account {
                provider: "x".into(),
                handle: "inject".into(),
                user_id: "9".into(),
                scopes: oauth::SCOPES.into(),
                access_token: "live-access-token".into(),
                expires_at: oauth::now() + 3600,
                refresh_token: Some("r".into()),
                client_id: "CID".into(),
                connected_at: 0,
            },
        );
        oauth::save(&accounts).unwrap();

        let (port, server) = mock_api();
        let dir = degen_core::config::packages_dir().unwrap().join("mock");
        std::fs::create_dir_all(dir.join("api_tools")).unwrap();
        std::fs::write(
            dir.join("integration.json"),
            r#"{"id":"mock","name":"Mock","version":"0.1.0","requires_env":["X_ACCESS_TOKEN"]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("api_tools/mock_post.json"),
            format!(
                r#"{{"name":"mock_post","method":"POST","url":"http://127.0.0.1:{port}/post",
                     "headers":{{"Authorization":"Bearer $X_ACCESS_TOKEN"}},
                     "parameters":{{"type":"object","properties":{{"text":{{"type":"string"}}}},"required":["text"]}},
                     "body_mapping":"params"}}"#
            ),
        )
        .unwrap();

        let (pkg, tool) = degen_core::package::find_tool("mock_post").unwrap();
        let mut args = serde_json::Map::new();
        args.insert("text".into(), serde_json::Value::String("gm".into()));
        let opts = degen_core::run::RunOptions { account: Some("x:inject".into()), ..Default::default() };
        let outcome = degen_core::run::execute(&pkg, &tool, args, &opts).unwrap();
        assert!(outcome.ok, "{:?}", outcome.error);

        let head = server.join().unwrap();
        assert!(head.contains("authorization: Bearer live-access-token"), "the account's token must be sent: {head}");
    });
}


