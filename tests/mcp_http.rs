// SPDX-License-Identifier: MIT OR Apache-2.0
//! Wire-level tests for the Streamable HTTP transport of `semcode-mcp`.
//!
//! These spawn the real binary and speak HTTP over a TCP socket, so they pin
//! the endpoint path, the header policy, and the session behaviour without a
//! test-only MCP client dependency.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(extra: &[&str], token: Option<&str>) -> Server {
    // The test binary runs tests in parallel, and several tests use the same
    // flags; a counter keeps their temporary databases apart.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "semcode-http-it-{}-{}-{}",
        std::process::id(),
        unique,
        extra.join("_").replace('/', "_")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let database = dir.join("test.db");

    let mut command = Command::new(env!("CARGO_BIN_EXE_semcode-mcp"));
    command
        .arg("--transport")
        .arg("http")
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("0")
        .arg("--database")
        .arg(&database)
        .arg("--git-repo")
        .arg(&dir)
        .arg("--no-auto-index")
        .args(extra)
        .env_remove("SEMCODE_MCP_TOKEN")
        .env_remove("SEMCODE_MCP_TOKEN_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(token) = token {
        command.env("SEMCODE_MCP_TOKEN", token);
    }
    let mut child = command.spawn().expect("spawn semcode-mcp");

    // The endpoint line carries the actual port chosen for --port 0.
    let stderr = child.stderr.take().expect("piped stderr");
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    let port = loop {
        assert!(Instant::now() < deadline, "server did not start in time");
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("server exited before printing its endpoint");
        }
        if let Some(rest) = line.split("MCP HTTP endpoint: http://127.0.0.1:").nth(1) {
            let port: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            break port.parse().expect("port in endpoint line");
        }
    };
    // Drain stderr in the background so a chatty server cannot block on a full
    // pipe while the test runs.
    std::thread::spawn(move || {
        let mut sink = String::new();
        let _ = reader.read_to_string(&mut sink);
    });

    Server { child, port }
}

fn request(port: u16, method: &str, headers: &[(&str, &str)], body: &str) -> (u16, String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let has_host = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"));
    let mut request = format!(
        "{method} /mcp HTTP/1.1\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !has_host {
        request.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    socket.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

fn initialize_body() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "semcode-test", "version": "1"}
        }
    })
    .to_string()
}

fn session_id(headers: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("mcp-session-id") {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

#[test]
fn unauthenticated_server_answers_and_rejects_unknown_hosts() {
    let server = start_server(&[], None);

    let (status, headers, body) = request(server.port, "POST", &[], &initialize_body());
    assert_eq!(status, 200, "{body}");
    assert!(
        headers.to_ascii_lowercase().contains("mcp-session-id"),
        "stateful mode must issue a session: {headers}"
    );
    assert!(body.contains("2025-11-25"), "{body}");

    let (status, _, body) = request(
        server.port,
        "POST",
        &[("Host", "evil.example:1234")],
        &initialize_body(),
    );
    assert_eq!(status, 403, "{body}");
}

#[test]
fn configured_token_is_enforced() {
    let server = start_server(&["--stateless-http"], Some(TOKEN));
    let auth = format!("Bearer {TOKEN}");

    let (status, _, _) = request(server.port, "POST", &[], &initialize_body());
    assert_eq!(status, 401);

    let (status, _, _) = request(
        server.port,
        "POST",
        &[("Authorization", "Bearer wrong-token-wrong-token-wrong")],
        &initialize_body(),
    );
    assert_eq!(status, 401);

    let (status, _, body) = request(
        server.port,
        "POST",
        &[("Authorization", &auth)],
        &initialize_body(),
    );
    assert_eq!(status, 200, "{body}");
}

#[test]
fn tools_list_exposes_the_semcode_tools() {
    let server = start_server(&["--stateless-http"], None);
    let (status, _, body) = request(
        server.port,
        "POST",
        &[],
        &serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
            .to_string(),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("find_function"), "{body}");
    assert!(body.contains("grep_functions"), "{body}");
    assert!(body.contains("indexing_status"), "{body}");
}

#[test]
fn lazy_mode_exposes_only_the_meta_tools() {
    let server = start_server(&["--stateless-http", "--lazy"], None);
    let (status, _, body) = request(
        server.port,
        "POST",
        &[],
        &serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
            .to_string(),
    );
    assert_eq!(status, 200, "{body}");
    // Descriptions may mention tool names as examples; assert the exact set.
    let response: serde_json::Value = serde_json::from_str(&body).expect("JSON response");
    let mut names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["call_tool", "get_tools", "list_categories"]);
}

#[test]
fn tool_call_crosses_the_blocking_boundary() {
    let server = start_server(&["--stateless-http"], None);
    let (status, _, body) = request(
        server.port,
        "POST",
        &[],
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "indexing_status", "arguments": {}}
        })
        .to_string(),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Indexing Status"), "{body}");
    // The handler omits isError on success; assert it did not report failure.
    assert!(!body.contains("\"isError\":true"), "{body}");
}

#[test]
fn stateful_session_is_required_after_initialize() {
    let server = start_server(&[], None);
    let (status, headers, body) = request(server.port, "POST", &[], &initialize_body());
    assert_eq!(status, 200, "{body}");
    let session = session_id(&headers).expect("session id");

    let list = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        .to_string();
    let (status, _, body) = request(server.port, "POST", &[("Mcp-Session-Id", &session)], &list);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("find_function"), "{body}");
}

#[test]
fn http_only_flags_are_rejected_with_stdio() {
    let output = Command::new(env!("CARGO_BIN_EXE_semcode-mcp"))
        .arg("--bind")
        .arg("0.0.0.0")
        .output()
        .expect("run semcode-mcp");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("require --transport http"), "{stderr}");
}
