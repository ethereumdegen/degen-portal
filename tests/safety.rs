//! The safety layer, against a server that counts what actually arrives.
//!
//! An agent retrying a post is the failure this exists to stop, so the
//! assertion is not "the second call returned an error" but "the second call
//! never reached the wire".

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use degen_tools_core::run::RunOptions;
use degen_portal::{ledger, policy, queue};
use serde_json::{Map, Value};

static STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_state_home<T>(name: &str, body: impl FnOnce(u16, Arc<AtomicUsize>) -> T) -> T {
    let guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("degen-portal-safety-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Safe: the lock makes this the only thread touching HOME.
    unsafe { std::env::set_var("HOME", &dir) };
    degen_portal::init();

    let (port, hits) = mock_api();
    write_package(port);
    let out = body(port, hits);
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// Answers every request with a new post id, and counts them.
fn mock_api() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let body = format!("{{\"data\":{{\"id\":\"{n}\"}}}}");
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, hits)
}

/// A package whose one tool publishes: `post_id_path` is what makes the ledger
/// count it, deduplicate it and know how to undo it.
fn write_package(port: u16) {
    let dir = degen_tools_core::config::packages_dir().unwrap().join("x");
    std::fs::create_dir_all(dir.join("api_tools")).unwrap();
    std::fs::write(
        dir.join("integration.json"),
        r#"{"id":"x","name":"Mock X","version":"0.1.0","requires_env":[]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("api_tools/x_post.json"),
        format!(
            r#"{{"name":"x_post","method":"POST","url":"http://127.0.0.1:{port}/2/tweets",
                 "parameters":{{"type":"object","properties":{{"text":{{"type":"string"}}}},"required":["text"]}},
                 "body_mapping":"params","post_id_path":"data.id"}}"#
        ),
    )
    .unwrap();
}

fn post(text: &str) -> Result<degen_tools_core::run::Outcome, degen_tools_core::errors::DegenError> {
    let (pkg, tool) = degen_tools_core::package::find_tool("x_post").unwrap();
    let mut args = Map::new();
    args.insert("text".into(), Value::String(text.into()));
    degen_tools_core::run::execute(&pkg, &tool, args, &RunOptions::default())
}

#[test]
fn the_same_post_twice_is_published_once() {
    with_state_home("dedupe", |_port, hits| {
        // With an account connected, the ledger can link to what it published.
        let mut accounts = degen_portal::oauth::Accounts::default();
        accounts.accounts.insert(
            "x:tester".to_string(),
            degen_portal::oauth::Account {
                provider: "x".into(),
                handle: "tester".into(),
                user_id: "1".into(),
                scopes: String::new(),
                access_token: "t".into(),
                expires_at: degen_portal::oauth::now() + 3600,
                refresh_token: None,
                client_id: "CID".into(),
                connected_at: 0,
            },
        );
        degen_portal::oauth::save(&accounts).unwrap();

        let first = post("gm").unwrap();
        assert!(first.ok, "{:?}", first.error);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let second = post("gm").unwrap_err().to_string();
        assert!(second.contains("already published"), "{second}");
        assert!(second.contains("id 1"), "the agent needs the id of what it would have duplicated: {second}");
        assert!(second.contains("Nothing was sent"), "{second}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the retry must not reach the API");

        // Different text is a different post, not a repeat.
        post("gn").unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        // Both are in the ledger, with the call that takes them back.
        let entries = ledger::read();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].post_id.as_deref(), Some("1"));
        assert_eq!(entries[0].undo.as_ref().unwrap().tool, "x_delete_post");
        assert_eq!(entries[0].target, "x:tester");
        assert_eq!(entries[0].permalink.as_deref(), Some("https://x.com/tester/status/1"));
    });
}

#[test]
fn an_exhausted_budget_refuses_and_says_when_it_frees_up() {
    with_state_home("budget", |_port, hits| {
        policy::set_budget("x", policy::Budget { per_hour: 2, per_day: 50 }).unwrap();

        post("one").unwrap();
        post("two").unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        let refused = post("three").unwrap_err().to_string();
        assert!(refused.contains("published 2 times in the last hour"), "{refused}");
        assert!(refused.contains("frees in"), "a refusal has to say when it lifts: {refused}");
        assert!(refused.contains("degen-portal budget x"), "and how a human changes it: {refused}");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "nothing was sent");
    });
}

#[test]
fn the_daily_cap_is_enforced_as_well_as_the_hourly_one() {
    with_state_home("daily", |_port, hits| {
        policy::set_budget("x", policy::Budget { per_hour: 50, per_day: 1 }).unwrap();
        post("one").unwrap();
        let refused = post("two").unwrap_err().to_string();
        assert!(refused.contains("in the last day"), "{refused}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn a_queued_provider_holds_the_call_until_a_human_releases_it() {
    with_state_home("queue", |_port, hits| {
        policy::set_approval("x", policy::Approval::Queue).unwrap();

        let held = post("needs a human").unwrap();
        assert!(!held.ok);
        assert_eq!(held.response["queued"], Value::String("q1".into()));
        assert!(held.error.as_ref().unwrap().contains("degen-portal approve q1"), "{:?}", held.error);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "a held call must not be sent");

        let queued = queue::load().unwrap();
        assert_eq!(queued.held.len(), 1);
        assert_eq!(queued.held[0].args["text"], Value::String("needs a human".into()));

        queue::approve("q1").unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "approving sends it");
        assert!(queue::load().unwrap().held.is_empty(), "and takes it off the queue");
        assert_eq!(ledger::read().len(), 1, "an approved call is recorded like any other");
    });
}

#[test]
fn a_dropped_call_is_never_sent() {
    with_state_home("drop", |_port, hits| {
        policy::set_approval("x", policy::Approval::Queue).unwrap();
        post("never mind").unwrap();
        queue::drop_held("q1").unwrap();
        assert!(queue::load().unwrap().held.is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(ledger::read().is_empty(), "nothing was published, so nothing is in the ledger");
    });
}
