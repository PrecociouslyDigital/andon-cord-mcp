//! A small MCP client, and a sandboxed `andon` to point it at.
//!
//! Everything here drives the real binary over real stdio. Nothing is mocked:
//! the point of these tests is that the thing we ship works.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use serde_json::{Value, json};

pub const BIN: &str = env!("CARGO_BIN_EXE_andon");

/// Old enough to keep the pre-2026 lifecycle, which needs no per-request
/// metadata — the shape most clients in the wild still speak.
const PROTOCOL: &str = "2025-06-18";

/// Every version rmcp will negotiate. Real clients pick the newest they know,
/// so testing only the comfortable one tests a path nobody takes.
pub const KNOWN_PROTOCOLS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    "2025-06-18",
    "2025-11-25",
    "2026-07-28",
];

/// A state directory of its own, handed to child processes through their
/// environment. Nothing here mutates this process's environment, so these
/// tests are free to run in parallel.
pub struct Sandbox {
    pub dir: PathBuf,
    vars: Vec<(String, String)>,
}

impl Sandbox {
    pub fn new(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("andon-it-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("sandbox");
        let mut sandbox = Sandbox {
            vars: Vec::new(),
            dir,
        };
        let state = sandbox.dir.join("state");
        sandbox.set("ANDON_STATE_DIR", state.display().to_string());
        let config = sandbox.dir.join("config.json");
        sandbox.set("ANDON_CONFIG", config.display().to_string());
        // A config of our own, so the developer's real one can never reach into
        // a test — and no bell rings on their terminal while the suite runs.
        sandbox.config(r#"{ "notify": [] }"#);
        sandbox
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) -> &mut Sandbox {
        self.vars.push((key.to_string(), value.into()));
        self
    }

    pub fn config(&mut self, body: &str) -> &mut Sandbox {
        std::fs::write(self.dir.join("config.json"), body).expect("config");
        self
    }

    /// A child environment with our variables applied and every `CLAUDE_*`
    /// variable stripped, so a test only sees a harness it asked for.
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        // Its own working directory too, so `--project` settings and a cord's
        // recorded cwd both land inside the sandbox.
        command.current_dir(&self.dir);
        for key in std::env::vars().map(|(k, _)| k) {
            if key.starts_with("CLAUDE_") {
                command.env_remove(key);
            }
        }
        for (key, value) in &self.vars {
            command.env(key, value);
        }
        command
    }

    pub fn andon(&self, args: &[&str]) -> Output {
        self.command(BIN)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run andon")
    }

    /// `andon <args>` with a payload on stdin.
    pub fn andon_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command(BIN)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn andon");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait")
    }

    pub fn cords(&self) -> Vec<String> {
        entries(self.dir.join("state/cords"))
    }

    pub fn archive(&self) -> Vec<String> {
        entries(self.dir.join("state/archive"))
    }

    /// Waits for exactly one open cord and returns its id.
    pub fn wait_for_cord(&self) -> String {
        for _ in 0..200 {
            let cords = self.cords();
            if let [name] = cords.as_slice() {
                return name.trim_end_matches(".json").to_string();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("no cord appeared in {}", self.dir.display());
    }

    /// Rewrites a cord file, for planting states the happy path cannot reach.
    pub fn set_cord(&self, id: &str, cord: &Value) {
        let path = self.dir.join(format!("state/cords/{id}.json"));
        std::fs::write(path, serde_json::to_vec_pretty(cord).expect("cord")).expect("write cord");
    }

    pub fn archived_json(&self, name: &str) -> Value {
        let path = self.dir.join("state/archive").join(name);
        serde_json::from_slice(&std::fs::read(path).expect("archived file")).expect("json")
    }

    pub fn cord_json(&self, id: &str) -> Value {
        let path = self.dir.join(format!("state/cords/{id}.json"));
        serde_json::from_slice(&std::fs::read(path).expect("cord file")).expect("cord json")
    }

    pub fn serve(&self) -> Server {
        Server::start(self)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn entries(dir: PathBuf) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".json"))
        .collect();
    names.sort();
    names
}

/// An MCP server under test, plus a reader thread so the client can leave a
/// request outstanding — which is the whole point of a blocking cord.
pub struct Server {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: i64,
}

impl Server {
    pub fn start(sandbox: &Sandbox) -> Server {
        let mut child = sandbox
            .command(BIN)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn server");

        let stdout = child.stdout.take().expect("stdout");
        let (tx, incoming) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                // stdout is the transport: anything that is not JSON here is a
                // bug, and the test should say so rather than hang.
                let message = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("non-JSON on stdout: {line:?} ({e})"));
                if tx.send(message).is_err() {
                    return;
                }
            }
        });

        Server {
            stdin: child.stdin.take().expect("stdin"),
            child,
            incoming,
            next_id: 0,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").expect("write to server");
        self.stdin.flush().expect("flush");
    }

    /// Sends a request and returns its id without waiting for the response —
    /// so a caller can leave a `pull_andon_cord` outstanding.
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Waits for the response to one request, ignoring notifications that
    /// arrive in the meantime.
    pub fn response(&mut self, id: i64, timeout: Duration) -> Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.incoming.recv_timeout(left) {
                Ok(message) if message.get("id") == Some(&json!(id)) => return message,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no response to request {id} in {timeout:?}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("server exited before answering {id}")
                }
            }
        }
    }

    /// Waits for a request coming the other way — the server asking *us*
    /// something, which is what elicitation is.
    pub fn server_request(&mut self, method: &str, timeout: Duration) -> Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.incoming.recv_timeout(left) {
                Ok(m) if m.get("method") == Some(&json!(method)) && m.get("id").is_some() => {
                    return m;
                }
                Ok(_) => continue,
                Err(_) => panic!("the server never sent a {method} request"),
            }
        }
    }

    /// Answers a server-initiated request.
    pub fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }));
    }

    /// Every notification seen so far, drained without blocking.
    pub fn notifications(&mut self) -> Vec<Value> {
        let mut seen = Vec::new();
        while let Ok(message) = self.incoming.try_recv() {
            if message.get("id").is_none() {
                seen.push(message);
            }
        }
        seen
    }

    pub fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Value {
        let id = self.request(method, params);
        self.response(id, timeout)
    }

    pub fn initialize(&mut self) -> Value {
        self.initialize_with(json!({}))
    }

    /// Initialize advertising whatever the test wants the client to support.
    pub fn initialize_with(&mut self, capabilities: Value) -> Value {
        self.initialize_at(PROTOCOL, capabilities)
    }

    /// Initialize at a specific protocol version.
    pub fn initialize_at(&mut self, protocol: &str, capabilities: Value) -> Value {
        let result = self.call(
            "initialize",
            json!({
                "protocolVersion": protocol,
                "capabilities": capabilities,
                "clientInfo": { "name": "andon-smoke", "version": "0" },
            }),
            Duration::from_secs(10),
        );
        self.notify("notifications/initialized", json!({}));
        result
    }

    pub fn tools(&mut self) -> Vec<Value> {
        self.tools_result()["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The whole `tools/list` result, envelope included — which is where the
    /// fields a strict client validates actually live.
    pub fn tools_result(&mut self) -> Value {
        self.call("tools/list", json!({}), Duration::from_secs(10))["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The text a `tools/call` result carries back to the model.
pub fn tool_text(response: &Value) -> String {
    response["result"]["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
