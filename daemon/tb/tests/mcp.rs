//! MCP server tests: stdio and streamable HTTP, against the fake extension.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::Env;
use serde_json::{Value, json};
use tb_testkit::{INBOX, TRASH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout};

struct Stdio_ {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Stdio_ {
    async fn start(env: &Env) -> Self {
        let mut cmd = env.command();
        let mut child = cmd
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = s
            .call(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
            )
            .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "tb-agent");
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        s
    }

    async fn send(&mut self, message: &Value) {
        self.stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), self.stdout.read_line(&mut line))
                .await
                .expect("MCP answer in time")
                .unwrap();
            assert!(n > 0, "MCP server closed stdout");
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }

    async fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.call("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }
}

fn text_of(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    serde_json::from_str(text).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_lists_one_tool_per_operation_with_schemas() {
    let env = Env::new().await;
    let mut mcp = Stdio_::start(&env).await;
    let list = mcp.call("tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    let expected: Vec<&str> = tb_protocol::ops().iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, expected);
    let search = tools
        .iter()
        .find(|t| t["name"] == "searchMessages")
        .unwrap();
    assert_eq!(search["inputSchema"]["required"], json!(["query"]));
    assert_eq!(search["annotations"]["readOnlyHint"], true);
    let delete = tools
        .iter()
        .find(|t| t["name"] == "deleteMessages")
        .unwrap();
    assert_eq!(delete["annotations"]["destructiveHint"], true);
    let _ = mcp.child.kill().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_tool_calls_go_through_the_daemon_to_the_extension() {
    let env = Env::new().await;
    let mut mcp = Stdio_::start(&env).await;

    let accounts = text_of(&mcp.tool("listAccounts", json!({})).await);
    assert_eq!(accounts.as_array().unwrap().len(), 2);

    let found = text_of(&mcp.tool("searchMessages", json!({"query": "lunch"})).await);
    assert_eq!(found[0]["id"], "m2@example.com");

    let moved = mcp
        .tool(
            "updateMessage",
            json!({"messageId": "m2@example.com", "folderPath": INBOX, "moveTo": TRASH}),
        )
        .await;
    assert_eq!(moved["result"]["isError"], false);
    assert_eq!(env.last_call().1["moveTo"], TRASH);
    let _ = mcp.child.kill().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_errors_are_tool_errors_with_the_protocol_code() {
    let env = Env::new().await;
    let mut mcp = Stdio_::start(&env).await;
    let missing = mcp
        .tool(
            "getMessage",
            json!({"messageId": "nope", "folderPath": INBOX}),
        )
        .await;
    assert_eq!(missing["result"]["isError"], true);
    assert_eq!(text_of(&missing)["error"]["code"], "not_found");

    let invalid = mcp.tool("searchMessages", json!({})).await;
    assert_eq!(invalid["result"]["isError"], true);
    assert_eq!(text_of(&invalid)["error"]["code"], "bad_request");

    let unknown = mcp.tool("noSuchTool", json!({})).await;
    assert!(
        unknown.get("error").is_some(),
        "unknown tools are protocol errors: {unknown}"
    );
    let _ = mcp.child.kill().await;
}

/// Minimal HTTP/1.1 client: one POST per connection, returns (status, body).
async fn post(addr: &str, session: Option<&str>, body: &Value) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = body.to_string();
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(s) = session {
        request.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, payload) = raw.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let payload = if head.to_lowercase().contains("transfer-encoding: chunked") {
        dechunk(payload)
    } else {
        payload.to_owned()
    };
    let json = payload
        .lines()
        .find_map(|l| serde_json::from_str::<Value>(l.strip_prefix("data: ").unwrap_or(l)).ok())
        .unwrap_or(Value::Null);
    (status, json)
}

fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.push_str(&tail[..n]);
        rest = &tail[n + 2..];
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_serves_tools_on_loopback() {
    let env = Env::new().await;
    let mut cmd = env.command();
    let mut child = cmd
        .args(["mcp", "--http", "127.0.0.1:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).await.unwrap();
    let addr = line
        .split("http://")
        .nth(1)
        .and_then(|s| s.split("/mcp").next())
        .unwrap_or_else(|| panic!("no address in {line:?}"))
        .to_owned();

    let (status, init) = post(
        &addr,
        None,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
    )
    .await;
    assert_eq!(status, 200, "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "tb-agent");

    let (status, list) = post(
        &addr,
        None,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(
        list["result"]["tools"].as_array().unwrap().len(),
        tb_protocol::ops().len()
    );

    let (status, call) = post(
        &addr,
        None,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "listAccounts", "arguments": {}}}),
    )
    .await;
    assert_eq!(status, 200, "{call}");
    assert_eq!(text_of(&call).as_array().unwrap().len(), 2);
    let _ = child.kill().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_rejects_foreign_host_headers() {
    let env = Env::new().await;
    let mut cmd = env.command();
    let mut child = cmd
        .args(["mcp", "--http", "127.0.0.1:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).await.unwrap();
    let addr = line
        .split("http://")
        .nth(1)
        .and_then(|s| s.split("/mcp").next())
        .unwrap()
        .to_owned();

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: evil.example\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    assert!(raw.starts_with("HTTP/1.1 403"), "{raw}");
    let _ = child.kill().await;
}
