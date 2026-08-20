//! Integration test: spawn `repo mcp` and drive the MCP stdio protocol —
//! initialize → tools/list → tools/call (refs/search/context/task) — asserting
//! valid newline-delimited JSON-RPC responses and no stray stdout bytes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

struct Server {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl Server {
    fn request(&mut self, id: i64, method: &str, params: serde_json::Value) -> serde_json::Value {
        let line = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        })
        .to_string();
        writeln!(self.child.stdin.as_mut().unwrap(), "{line}").unwrap();
        self.child.stdin.as_mut().unwrap().flush().unwrap();
        self.read_response(id)
    }

    fn notify(&mut self, method: &str) {
        let line = serde_json::json!({ "jsonrpc": "2.0", "method": method }).to_string();
        writeln!(self.child.stdin.as_mut().unwrap(), "{line}").unwrap();
        self.child.stdin.as_mut().unwrap().flush().unwrap();
    }

    /// Read one protocol line and assert it answers `id`.
    fn read_response(&mut self, id: i64) -> serde_json::Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).expect("read line");
        assert!(n > 0, "server closed stdout");
        let msg: serde_json::Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line:?}"));
        assert_eq!(
            msg["id"],
            serde_json::json!(id),
            "unexpected response: {msg}"
        );
        assert!(msg.get("error").is_none(), "protocol error: {msg}");
        msg["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_workspace() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("repo-mcp-test-{nanos}"));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"mt\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "// haystack_needle_word marker for search\npub fn widget_factory() -> u32 { 42 }\n",
    )
    .unwrap();
    let _ = Command::new("git").arg("init").arg(&dir).output();
    dir
}

fn start_server(cwd: &std::path::Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_repo"))
        .arg("mcp")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn repo mcp");
    let reader = BufReader::new(child.stdout.take().unwrap());
    Server { child, reader }
}

fn call(srv: &mut Server, id: i64, name: &str, args: serde_json::Value) -> serde_json::Value {
    srv.request(
        id,
        "tools/call",
        serde_json::json!({ "name": name, "arguments": args }),
    )
}

#[test]
fn mcp_stdio_session() {
    let dir = temp_workspace();
    let mut srv = start_server(&dir);

    // Handshake.
    let init = srv.request(
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "integration-test", "version": "0" }
        }),
    );
    assert_eq!(init["serverInfo"]["name"], "repo", "{init}");
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    srv.notify("notifications/initialized");

    // Tool list.
    let tools = srv.request(2, "tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in ["context", "search", "refs", "task"] {
        assert!(names.contains(&expected), "tools: {names:?}");
    }

    // refs: definition lookup in the temp project.
    let r = call(
        &mut srv,
        3,
        "refs",
        serde_json::json!({ "symbol": "widget_factory" }),
    );
    let payload: serde_json::Value =
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["symbol"], "widget_factory");
    assert_eq!(payload["definitions"][0]["path"], "src/lib.rs");

    // search: BM25 (deterministic, no llama.cpp dependency).
    let r = call(
        &mut srv,
        4,
        "search",
        serde_json::json!({ "query": "haystack_needle_word", "bm25": true }),
    );
    let payload: serde_json::Value =
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["mode"], "bm25");
    assert!(
        payload["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"] == "src/lib.rs"),
        "{payload}"
    );

    // context: simple mode returns the markdown header.
    let r = call(
        &mut srv,
        5,
        "context",
        serde_json::json!({ "simple": true }),
    );
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("# Repository Context"), "{text}");

    // task: unknown project-type filter → caller-visible error (isError).
    let r = call(
        &mut srv,
        6,
        "task",
        serde_json::json!({ "kind": "lint", "type": "does-not-exist" }),
    );
    assert_eq!(r["isError"], serde_json::json!(true), "{r}");
    let payload: serde_json::Value =
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["ok"], false);
    assert!(payload["error"]
        .as_str()
        .unwrap()
        .contains("does-not-exist"));

    // Unknown tool → protocol error (METHOD_NOT_FOUND / invalid params).
    let line = serde_json::json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "no_such_tool", "arguments": {} }
    })
    .to_string();
    writeln!(srv.child.stdin.as_mut().unwrap(), "{line}").unwrap();
    srv.child.stdin.as_mut().unwrap().flush().unwrap();
    let mut buf = String::new();
    srv.reader.read_line(&mut buf).unwrap();
    let msg: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
    assert!(msg["error"].is_object(), "expected protocol error: {msg}");

    let _ = std::fs::remove_dir_all(&dir);
}
