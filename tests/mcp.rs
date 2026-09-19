//! The MCP server driven the way a client drives it: a spawned process, one
//! JSON-RPC message per line, over real pipes.
//!
//! The point worth proving is that this face on the engine is not a way around
//! it — a post through MCP goes through the same gates, and the second
//! identical one never reaches the wire.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

/// Answers any POST with a post id, and counts what arrives.
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
            counter.fetch_add(1, Ordering::SeqCst);
            let body = "{\"data\":{\"id\":\"1790\"}}";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, hits)
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    /// Spawn the real binary with its state in `home`.
    fn start(home: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_degen-portal"))
            .arg("mcp")
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the binary runs");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, next_id: 0 }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("a reply per request");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {e}: {line}"))
    }

    fn notify(&mut self, method: &str) {
        writeln!(self.stdin, "{}", json!({ "jsonrpc": "2.0", "method": method })).unwrap();
        self.stdin.flush().unwrap();
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({ "name": tool, "arguments": arguments }))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A publishing tool pointed at the mock, installed in this test's own HOME.
fn home_with_package(name: &str, port: u16) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("degen-portal-mcp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let dir = home.join(".degen-portal/packages/x");
    std::fs::create_dir_all(dir.join("api_tools")).unwrap();
    std::fs::write(dir.join("integration.json"), r#"{"id":"x","name":"Mock X","version":"0.1.0","requires_env":[]}"#).unwrap();
    std::fs::write(
        dir.join("api_tools/x_post.json"),
        format!(
            r#"{{"name":"x_post","description":"POSTS IN PUBLIC: publish on X.","method":"POST",
                 "url":"http://127.0.0.1:{port}/2/tweets",
                 "parameters":{{"type":"object","properties":{{"text":{{"type":"string"}}}},"required":["text"]}},
                 "body_mapping":"params","post_id_path":"data.id"}}"#
        ),
    )
    .unwrap();
    home
}

#[test]
fn a_client_can_handshake_list_tools_and_post() {
    let (port, hits) = mock_api();
    let home = home_with_package("session", port);
    let mut client = Client::start(&home);

    let hello = client.request("initialize", json!({ "protocolVersion": "2025-06-18", "clientInfo": { "name": "test", "version": "1" } }));
    assert_eq!(hello["result"]["protocolVersion"], json!("2025-06-18"));
    assert_eq!(hello["result"]["serverInfo"]["name"], json!("degen-portal"));
    let instructions = hello["result"]["instructions"].as_str().unwrap();
    assert!(!instructions.starts_with("---"), "the YAML header is not for the model: {instructions:.40}");
    assert!(instructions.contains("permanent"), "the rules have to reach the model");
    assert!(instructions.contains("Never repeat a failed write"), "including the one that matters most");
    client.notify("notifications/initialized");

    let listed = client.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"x_post"), "{names:?}");
    assert!(names.contains(&"portal_status"), "an agent needs to be able to ask what it may do: {names:?}");
    let posted = tools.iter().find(|t| t["name"] == json!("x_post")).unwrap();
    assert_eq!(posted["inputSchema"]["required"], json!(["text"]), "the schema is the tool's own");

    let status = client.call("portal_status", json!({}));
    assert_eq!(status["result"]["isError"], json!(false));
    assert!(status["result"]["content"][0]["text"].as_str().unwrap().contains("published this hour"));

    let first = client.call("x_post", json!({ "text": "gm" }));
    assert_eq!(first["result"]["isError"], json!(false), "{first}");
    assert!(first["result"]["content"][0]["text"].as_str().unwrap().contains("1790"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // The gates are not bypassed by coming in over MCP.
    let repeat = client.call("x_post", json!({ "text": "gm" }));
    assert_eq!(repeat["result"]["isError"], json!(true), "{repeat}");
    let text = repeat["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("already published"), "{text}");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the repeat must not reach the API");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_refusal_is_tool_content_the_model_can_read_not_a_transport_error() {
    let (port, _hits) = mock_api();
    let home = home_with_package("refusal", port);
    let mut client = Client::start(&home);
    client.request("initialize", json!({ "protocolVersion": "2026-07-28" }));

    // No such parameter: the engine refuses, and the model is told which are real.
    let bad = client.call("x_post", json!({ "nonsense": "x" }));
    assert!(bad["error"].is_null(), "a refused call is a result, not a JSON-RPC error: {bad}");
    assert_eq!(bad["result"]["isError"], json!(true));
    assert!(bad["result"]["content"][0]["text"].as_str().unwrap().contains("no parameter"), "{bad}");

    let missing = client.call("nope_not_a_tool", json!({}));
    assert_eq!(missing["result"]["isError"], json!(true));

    let _ = std::fs::remove_dir_all(&home);
}
