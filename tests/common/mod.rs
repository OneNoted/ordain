#![allow(dead_code)]
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
pub fn fixture_xdg(home: &Path) -> [(&'static str, PathBuf); 7] {
    [
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("CLAUDE_CONFIG_DIR", home.join(".claude")),
        ("CODEX_HOME", home.join(".codex")),
        ("HERMES_HOME", home.join(".hermes")),
        ("OPENCODE_CONFIG_DIR", home.join(".config/opencode")),
        ("XDG_STATE_HOME", home.join(".local/state")),
        ("XDG_CACHE_HOME", home.join(".cache")),
    ]
}
pub fn fixture_events(root: &Path, home: &Path) -> PathBuf {
    let root = std::fs::canonicalize(root).unwrap();
    home.join(".local/state/ordain/projects")
        .join(hex::encode(Sha256::digest(
            root.as_os_str().as_encoded_bytes(),
        )))
        .join("events.jsonl")
}

use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Response, Server, StatusCode};
pub fn git(root: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
}

pub fn init_repo(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "core.excludesFile", "/dev/null"]);
    git(root, &["config", "user.name", "Fixture"]);
    git(root, &["config", "user.email", "fixture@example.test"]);
}

pub fn write_rubric(root: &Path, rules: Value) {
    fs::create_dir_all(root.join(".ordain")).unwrap();
    fs::write(
        root.join(".ordain/rubric.json"),
        json!({
            "version": 1,
            "compiledAt": "synthetic-review-fixture",
            "sources": [{"path":"AGENTS.md"}],
            "rules": rules
        })
        .to_string(),
    )
    .unwrap();
}

pub fn model_rule(id: &str, when: &str, scope: Option<Value>) -> Value {
    let mut rule = json!({
        "id": id,
        "text": format!("Synthetic fixture rule {id}"),
        "source": {"path":"AGENTS.md","line":1},
        "when": when,
        "check": {"type":"model","question":{"type":"boolean","instructions":"Synthetic fixture question"}}
    });
    if let Some(scope) = scope {
        rule["scope"] = scope;
    }
    rule
}

pub fn run_with_input(command: &mut Command, input: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

pub fn fixture_server<F>(
    requests: usize,
    mut respond: F,
) -> (String, thread::JoinHandle<Vec<Value>>)
where
    F: FnMut(&Value) -> (u16, Value) + Send + 'static,
{
    let server = Server::http("127.0.0.1:0").unwrap();
    let address = format!("http://{}", server.server_addr());
    let handle = thread::spawn(move || {
        let mut bodies = Vec::new();
        for _ in 0..requests {
            let mut request = server
                .recv_timeout(Duration::from_secs(8))
                .unwrap()
                .expect("fixture request did not arrive");
            let mut text = String::new();
            request.as_reader().read_to_string(&mut text).unwrap();
            let body: Value = serde_json::from_str(&text).unwrap();
            let (status, response) = respond(&body);
            request
                .respond(
                    Response::from_string(response.to_string())
                        .with_status_code(StatusCode(status))
                        .with_header(
                            Header::from_bytes("content-type", "application/json").unwrap(),
                        ),
                )
                .unwrap();
            bodies.push(body);
        }
        bodies
    });
    (address, handle)
}
