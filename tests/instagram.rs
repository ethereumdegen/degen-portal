//! Instagram: the DM allowlist, and a token that refreshes itself.
//!
//! Both are assertions about what reaches the wire, not about what a function
//! returned: "the second call was refused" is worth nothing if the message
//! went out anyway, and "the token was refreshed" is worth nothing if the old
//! one is what arrived at Meta.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use degen_portal::{instagram, ledger, oauth, policy};
use parking_lot::Mutex;
use degen_tools_core::run::RunOptions;
use serde_json::{Map, Value};

static STATE: Mutex<()> = Mutex::new(());

/// Every request the fake Meta saw: method, path, and the bearer it carried.
#[derive(Default)]
struct Seen {
    calls: Mutex<Vec<(String, String, String)>>,
    hits: AtomicUsize,
}

impl Seen {
    fn count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn bearers(&self) -> Vec<String> {
        self.calls.lock().iter().map(|(_, _, auth)| auth.clone()).collect()
    }
}

fn with_state_home<T>(name: &str, body: impl FnOnce(u16, Arc<Seen>) -> T) -> T {
    let guard = STATE.lock();
    let dir = std::env::temp_dir().join(format!("degen-portal-ig-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Safe: the lock makes this the only thread touching HOME.
    unsafe { std::env::set_var("HOME", &dir) };
    degen_portal::init();

    let (port, seen) = mock_meta();
    write_package(port);
    let out = body(port, seen);
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// Answers `/refresh` with a fresh sixty-day token and everything else with a
/// sent-message id, recording what it was asked and with which bearer.
fn mock_meta() -> (u16, Arc<Seen>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Seen::default());
    let recorder = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap_or(0);
            let mut auth = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("authorization:") {
                    auth = value.trim().to_string();
                }
            }
            let mut parts = request.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let n = recorder.hits.fetch_add(1, Ordering::SeqCst) + 1;
            let body = if path.starts_with("/refresh") {
                "{\"access_token\":\"IG-refreshed\",\"token_type\":\"bearer\",\"expires_in\":5183944}".to_string()
            } else {
                format!("{{\"recipient_id\":\"igsid-1\",\"message_id\":\"mid.{n}\"}}")
            };
            recorder.calls.lock().push((method, path, auth));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, seen)
}

/// The real package's shape — a bearer from the connected account, a DM whose
/// recipient is `recipient_id`, and `message_id` as the published id — pointed
/// at the fake Meta.
fn write_package(port: u16) {
    let dir = degen_tools_core::config::packages_dir().unwrap().join("instagram");
    std::fs::create_dir_all(dir.join("api_tools")).unwrap();
    std::fs::write(
        dir.join("integration.json"),
        r#"{"id":"instagram","name":"Mock Instagram","version":"0.1.0","requires_env":["INSTAGRAM_ACCESS_TOKEN"]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("api_tools/instagram_send_dm.json"),
        format!(
            r#"{{"name":"instagram_send_dm","method":"POST","url":"http://127.0.0.1:{port}/v25.0/me/messages",
                 "headers":{{"Authorization":"Bearer $INSTAGRAM_ACCESS_TOKEN"}},
                 "parameters":{{"type":"object","properties":{{"recipient_id":{{"type":"string"}},"text":{{"type":"string"}}}},
                                "required":["recipient_id","text"]}},
                 "body_mapping":"params_nested",
                 "param_paths":{{"recipient_id":"recipient.id","text":"message.text"}},
                 "post_id_path":"message_id"}}"#
        ),
    )
    .unwrap();
}

/// A connected account with `days` of its sixty left, connected `age` seconds ago.
fn connect(days: u64, age: u64) {
    let mut accounts = oauth::Accounts::default();
    accounts.accounts.insert(
        "instagram:tester".to_string(),
        oauth::Account {
            provider: "instagram".into(),
            handle: "tester".into(),
            user_id: "17841400000000000".into(),
            scopes: instagram::SCOPES.into(),
            access_token: "IG-original".into(),
            expires_at: oauth::now() + days * 86_400,
            refresh_token: None,
            client_id: "app-1".into(),
            connected_at: oauth::now().saturating_sub(age),
        },
    );
    oauth::save(&accounts).unwrap();
}

fn dm(recipient: &str, text: &str) -> Result<degen_tools_core::run::Outcome, degen_tools_core::errors::DegenError> {
    let (pkg, tool) = degen_tools_core::package::find_tool("instagram_send_dm").unwrap();
    let mut args = Map::new();
    args.insert("recipient_id".into(), Value::String(recipient.into()));
    args.insert("text".into(), Value::String(text.into()));
    degen_tools_core::run::execute(&pkg, &tool, args, &RunOptions::default())
}

#[test]
fn a_dm_to_someone_a_human_has_not_allowed_never_reaches_meta() {
    with_state_home("allowlist", |_port, seen| {
        connect(60, 3600);

        let refused = dm("igsid-1", "hey").unwrap_err().to_string();
        assert!(refused.contains("refuses recipient igsid-1"), "{refused}");
        assert!(refused.contains("degen-portal instagram allow igsid-1"), "the refusal has to name the fix: {refused}");
        assert!(refused.contains("Nobody is allowed yet"), "{refused}");
        assert_eq!(seen.count(), 0, "a refused DM must not reach the API");
        assert!(ledger::read().is_empty(), "and nothing was published, so nothing is in the ledger");

        // A human allows that one person. Nobody else is thereby allowed.
        assert!(policy::allow_recipient("igsid-1").unwrap());
        let sent = dm("igsid-1", "hey").unwrap();
        assert!(sent.ok, "{:?}", sent.error);
        assert_eq!(seen.count(), 1);

        let other = dm("igsid-2", "hey you too").unwrap_err().to_string();
        assert!(other.contains("refuses recipient igsid-2"), "{other}");
        assert!(other.contains("Allowed: igsid-1"), "the refusal should say who is allowed: {other}");
        assert_eq!(seen.count(), 1, "allowing one recipient must not allow another");

        // Denying takes it back.
        assert!(policy::deny_recipient("igsid-1").unwrap());
        assert!(dm("igsid-1", "and again").is_err());
        assert_eq!(seen.count(), 1);
    });
}

#[test]
fn a_sent_dm_is_recorded_with_no_undo_because_instagram_has_no_delete() {
    with_state_home("ledger", |_port, seen| {
        connect(60, 3600);
        policy::allow_recipient("igsid-1").unwrap();

        let sent = dm("igsid-1", "on it").unwrap();
        assert!(sent.ok, "{:?}", sent.error);

        let entries = ledger::read();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.provider, "instagram");
        assert_eq!(entry.target, "instagram:tester", "the ledger records which account spoke");
        assert_eq!(entry.post_id.as_deref(), Some("mid.1"));
        assert_eq!(entry.permalink.as_deref(), Some("instagram: dm to igsid-1, message mid.1"));
        assert!(entry.undo.is_none(), "Instagram cannot unsend, so no undo may be recorded");

        // `undo` has to say so rather than fail obscurely or delete something else.
        let refused = degen_portal::undo_post(None).unwrap_err().to_string();
        assert!(refused.contains("nothing published from this machine can be undone"), "{refused}");

        // The same message again inside the repeat window is refused, unsent.
        let repeat = dm("igsid-1", "on it").unwrap_err().to_string();
        assert!(repeat.contains("already published"), "{repeat}");
        assert!(repeat.contains("mid.1"), "{repeat}");
        assert_eq!(seen.count(), 1, "the repeat must not reach the API");
    });
}

#[test]
fn a_token_near_the_end_of_its_sixty_days_is_refreshed_before_it_is_used() {
    with_state_home("refresh", |port, seen| {
        // Three days left, connected a week ago: due, and old enough to refresh.
        connect(3, 7 * 86_400);
        policy::allow_recipient("igsid-1").unwrap();

        let refresh_url = format!("http://127.0.0.1:{port}/refresh");
        let token = instagram::access_token("instagram:tester", &refresh_url).unwrap();
        assert_eq!(token, "IG-refreshed");
        assert_eq!(seen.count(), 1, "exactly one refresh");

        // Persisted, with a new expiry, and no refresh token invented for it.
        let stored = oauth::load().unwrap().accounts["instagram:tester"].clone();
        assert_eq!(stored.access_token, "IG-refreshed");
        assert!(stored.expires_at > oauth::now() + 55 * 86_400, "the new token is good for another sixty days");
        assert!(stored.refresh_token.is_none(), "Instagram has no refresh token; the access token is refreshed in place");

        // A second call is not another refresh: the stored token is live now.
        assert_eq!(instagram::access_token("instagram:tester", &refresh_url).unwrap(), "IG-refreshed");
        assert_eq!(seen.count(), 1);

        // And it is the refreshed token that arrives at the API.
        dm("igsid-1", "after the refresh").unwrap();
        assert_eq!(seen.bearers().last().unwrap(), "bearer ig-refreshed", "the call must carry the new token");
    });
}

#[test]
fn a_token_too_young_to_refresh_is_used_as_it_is_and_an_expired_one_says_to_reconnect() {
    with_state_home("young", |port, seen| {
        let refresh_url = format!("http://127.0.0.1:{port}/refresh");

        // Meta refuses to refresh a token younger than 24 hours. It is also in
        // no danger, so this is not an error — it is simply used.
        connect(59, 3600);
        assert_eq!(instagram::access_token("instagram:tester", &refresh_url).unwrap(), "IG-original");
        assert_eq!(seen.count(), 0, "a token with weeks left is not refreshed at all");

        // Nearly due, but still under a day old: still not refreshable.
        connect(3, 3600);
        assert_eq!(instagram::access_token("instagram:tester", &refresh_url).unwrap(), "IG-original");
        assert_eq!(seen.count(), 0, "Meta would refuse this refresh, so it is never attempted");

        // Past sixty days there is nothing to refresh from, and a retry cannot fix it.
        let mut accounts = oauth::load().unwrap();
        accounts.accounts.get_mut("instagram:tester").unwrap().expires_at = oauth::now() - 60;
        oauth::save(&accounts).unwrap();
        let err = instagram::access_token("instagram:tester", &refresh_url).unwrap_err().to_string();
        assert!(err.contains("expired"), "{err}");
        assert!(err.contains("degen-portal connect instagram"), "it has to say how to fix it: {err}");
        assert_eq!(seen.count(), 0, "an expired token is not sent to Meta to be refreshed");
    });
}

#[test]
fn publishing_a_post_records_the_media_id_and_still_offers_no_undo() {
    // The ledger's view of a publish, without a wire: `undo_for` and
    // `permalink` are what `log`, `undo` and the dashboard read.
    let mut args = Map::new();
    args.insert("creation_id".into(), Value::String("17999".into()));
    assert_eq!(
        ledger::permalink("instagram", Some("tester"), &args, "18000").as_deref(),
        Some("instagram: media 18000")
    );
    assert!(
        ledger::undo_for("instagram", &args, "18000").is_none(),
        "the Instagram API has no delete for media, so an undo would be a promise it cannot keep"
    );
}
