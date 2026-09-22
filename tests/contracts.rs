mod common;
use common::{fixture_events, fixture_xdg};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ordain::credentials::{GATEWAY_KEY_ENV, TYPESAFE_KEY_ENV};
use ordain::model::{Band, Check, Phase, Question, Rule, RuleSource, RuleStatus, Thresholds};
use ordain::provider::{CheckState, ProviderClient};
use serde_json::{Value, json};
use tempfile::tempdir;
use tiny_http::{Header, Response, Server};
use wait_timeout::ChildExt;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_ordain")
}

fn run_with_input(command: &mut Command, input: &[u8]) -> std::process::Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn boolean_rule() -> Rule {
    Rule {
        id: "raw-error".into(),
        text: "Never expose raw errors".into(),
        source: RuleSource {
            path: "AGENTS.md".into(),
            line: Some(1),
        },
        scope: None,
        when: Some(Phase::Edit),
        check: Check::Model {
            question: Question::Boolean {
                instructions: "Does the change expose raw errors?".into(),
                criteria: None,
            },
            overlaps: None,
        },
        status: RuleStatus::Active,
        calibration: None,
        origin: None,
    }
}

fn mock_server(expected: &'static str, response: Value) -> (String, thread::JoinHandle<Value>) {
    let server = Server::http("127.0.0.1:0").unwrap();
    let address = format!("http://{}", server.server_addr());
    let handle = thread::spawn(move || {
        let mut request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(request.method().as_str(), "POST");
        assert!(request.url().ends_with(expected));
        let headers = request
            .headers()
            .iter()
            .map(|header| {
                (
                    header.field.as_str().to_ascii_lowercase().to_string(),
                    header.value.as_str().to_owned(),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer fixture-key")
        );
        if expected.ends_with("evaluation-model") {
            assert_eq!(
                headers
                    .get("ai-evaluation-model-specification-version")
                    .map(String::as_str),
                Some("4")
            );
            assert_eq!(
                headers.get("ai-model-id").map(String::as_str),
                Some("typesafe-ai/jev")
            );
        }
        let mut text = String::new();
        request.as_reader().read_to_string(&mut text).unwrap();
        let body: Value = serde_json::from_str(&text).unwrap();
        let content = response.to_string();
        request
            .respond(
                Response::from_string(content)
                    .with_header(Header::from_bytes("content-type", "application/json").unwrap()),
            )
            .unwrap();
        body
    });
    (address, handle)
}

#[test]
fn compile_refresh_retains_linked_sources_and_scope() {
    let repo = tempdir().unwrap();
    let home = tempdir().unwrap();
    common::init_repo(repo.path());
    fs::create_dir_all(repo.path().join("docs")).unwrap();
    fs::create_dir_all(repo.path().join(".ordain")).unwrap();
    fs::write(
        repo.path().join("docs/style.md"),
        "Handle failures explicitly.\n",
    )
    .unwrap();
    let mut rubric = json!({
        "version": 1, "compiledAt": "fixture",
        "sources": [{"path": "docs/style.md", "scope": "src/**"}],
        "rules": [{"id": "handle-errors", "text": "Handle failures explicitly.",
                   "source": {"path": "docs/style.md", "line": 1},
                   "check": {"type": "deferred", "reason": "fixture"}}]
    });
    let cli = || {
        let mut cmd = Command::new(binary());
        cmd.current_dir(repo.path())
            .env("HOME", home.path())
            .envs(fixture_xdg(home.path()))
            .env(TYPESAFE_KEY_ENV, "fixture-key")
            .env_remove(GATEWAY_KEY_ENV);
        cmd
    };
    let validate = |rubric: &Value| {
        fs::write(repo.path().join(".ordain/rubric.json"), rubric.to_string()).unwrap();
        assert!(
            cli()
                .args(["rubric", "validate"])
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    let start = || {
        let output = run_with_input(
            cli().args(["__hook", "session-start"]),
            json!({"session_id":"compile-fixture", "cwd": repo.path(),
                   "hook_event_name":"SessionStart"})
            .to_string()
            .as_bytes(),
        );
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    validate(&rubric);
    fs::write(
        repo.path().join("AGENTS.md"),
        "Read [the style guide](docs/style.md).\nKeep changes focused.\n",
    )
    .unwrap();
    fs::write(
        repo.path().join(".ordain/config.toml"),
        "[rules.handle-errors]\nenabled = true\n",
    )
    .unwrap();
    let prompt = start();
    assert!(prompt.contains("source: AGENTS.md"));
    assert!(prompt.contains("source: docs/style.md (rules apply to src/**"));
    rubric["sources"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path":"AGENTS.md"}));
    validate(&rubric);
    assert!(
        cli()
            .args(["config", "validate"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(start().trim().is_empty());
    fs::write(
        repo.path().join("docs/style.md"),
        "Never hide required I/O failures.\n",
    )
    .unwrap();
    let prompt = start();
    assert!(prompt.contains("changed: docs/style.md"));
    assert_eq!(prompt.matches("source: docs/style.md").count(), 1);
    fs::remove_file(repo.path().join("docs/style.md")).unwrap();
    let prompt = start();
    assert!(prompt.contains("gone: docs/style.md"));
    assert!(!prompt.contains("source: docs/style.md"));
}

#[test]
fn published_typesafe_and_gateway_protocols_work_against_labelled_loopback_fixtures() {
    let repo = tempdir().unwrap();
    let rules = [boolean_rule()];
    let state = CheckState {
        sources: Vec::new(),
        task: Some("synthetic protocol fixture".into()),
        file: Some("src/a.rs".into()),
        files: None,
        diff: "+danger".into(),
    };

    let (base, server) = mock_server(
        "/systemone",
        json!({"model":"jev-latest","answers":{"raw-error":{"type":"noul","noul":0.91}},"usage":{"input_tokens":100,"output_tokens":1}}),
    );
    fs::write(
        repo.path().join(".env.local"),
        format!("{TYPESAFE_KEY_ENV}=fixture-key\n"),
    )
    .unwrap();
    unsafe { std::env::set_var("ORDAIN_TYPESAFE_BASE_URL", &base) };
    let client = ProviderClient::new(repo.path()).unwrap();
    let outcome = client
        .check(
            &rules.iter().collect::<Vec<_>>(),
            &state,
            Thresholds::default(),
            Instant::now() + Duration::from_secs(3),
            0,
        )
        .unwrap();
    unsafe { std::env::remove_var("ORDAIN_TYPESAFE_BASE_URL") };
    assert_eq!(outcome.verdicts[0].band, Band::Act);
    let request = server.join().unwrap();
    assert_eq!(request["model"], "jev-latest");
    assert_eq!(request["questions"]["raw-error"]["type"], "noul");

    let (base, server) = mock_server(
        "/evaluation-model",
        json!({"answers":{"raw-error":{"type":"boolean","probability":0.49}},"rounding":{"probabilityDecimals":2},"usage":{"inputTokens":50,"outputTokens":1}}),
    );
    fs::write(
        repo.path().join(".env.local"),
        format!("{GATEWAY_KEY_ENV}=fixture-key\n"),
    )
    .unwrap();
    unsafe { std::env::set_var("ORDAIN_GATEWAY_BASE_URL", &base) };
    let client = ProviderClient::new(repo.path()).unwrap();
    let outcome = client
        .check(
            &rules.iter().collect::<Vec<_>>(),
            &state,
            Thresholds::default(),
            Instant::now() + Duration::from_secs(3),
            0,
        )
        .unwrap();
    unsafe { std::env::remove_var("ORDAIN_GATEWAY_BASE_URL") };
    assert_eq!(outcome.verdicts[0].band, Band::Clear);
    let request = server.join().unwrap();
    assert_eq!(request["questions"]["raw-error"]["type"], "boolean");
    assert_eq!(
        request["providerOptions"]["gateway"]["zeroDataRetention"],
        true
    );
}

#[test]
fn init_and_uninstall_preserve_unrelated_configuration_in_an_isolated_home() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join("AGENTS.md"), "- Never expose raw errors\n").unwrap();
    fs::create_dir(repo.path().join(".ordain")).unwrap();
    fs::write(repo.path().join(".ordain/.gitignore"), "custom-cache\n").unwrap();
    fs::create_dir_all(home.path().join(".claude")).unwrap();
    fs::create_dir_all(home.path().join(".codex")).unwrap();
    fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
    let existing = json!({"model":"fixture","hooks":{"Stop":[{"hooks":[{"type":"command","command":"their-hook"}]}]}});
    fs::write(
        home.path().join(".claude/settings.json"),
        existing.to_string(),
    )
    .unwrap();
    fs::write(home.path().join(".codex/hooks.json"), existing.to_string()).unwrap();

    let output = Command::new(binary())
        .args(["init", "claude", "codex", "opencode"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for file in [
        home.path().join(".claude/settings.json"),
        home.path().join(".codex/hooks.json"),
    ] {
        let value: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
        assert_eq!(value["model"], "fixture");
        assert_eq!(value["hooks"]["Stop"].as_array().unwrap().len(), 2);
    }
    let plugin = home.path().join(".config/opencode/plugins/ordain.js");
    assert!(
        fs::read_to_string(&plugin)
            .unwrap()
            .contains("ordain-opencode-plugin")
    );
    let ignore = fs::read_to_string(repo.path().join(".ordain/.gitignore")).unwrap();
    assert!(ignore.contains("custom-cache"));
    assert!(ignore.contains("events.jsonl"));

    let output = Command::new(binary())
        .arg("uninstall")
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .output()
        .unwrap();
    assert!(output.status.success());
    for file in [
        home.path().join(".claude/settings.json"),
        home.path().join(".codex/hooks.json"),
    ] {
        let value: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
        assert_eq!(value["model"], "fixture");
        assert_eq!(
            value["hooks"]["Stop"][0]["hooks"][0]["command"],
            "their-hook"
        );
    }
    assert!(!plugin.exists());

    let output = Command::new(binary())
        .args(["init", "claude", "codex", "opencode", "--project"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .output()
        .unwrap();
    assert!(output.status.success());
    for file in [
        repo.path().join(".claude/settings.json"),
        repo.path().join(".codex/hooks.json"),
        repo.path().join(".opencode/plugins/ordain.js"),
    ] {
        assert!(file.is_file(), "{}", file.display());
    }
    let output = Command::new(binary())
        .args(["uninstall", "--project"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!repo.path().join(".opencode/plugins/ordain.js").exists());
}

#[test]
fn hooks_are_silent_and_successful_on_malformed_input() {
    for name in ["session-start", "turn-start", "post-tool-use", "stop"] {
        for input in [b"".as_slice(), b"not json", b"{}"] {
            let output = run_with_input(Command::new(binary()).args(["__hook", name]), input);
            assert!(output.status.success());
            assert!(
                output.stdout.is_empty(),
                "{name}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
}

#[test]
fn post_tool_hook_blocks_from_a_loopback_judge_and_logs_no_secret() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join("AGENTS.md"), "Never expose raw errors\n").unwrap();
    fs::create_dir(repo.path().join(".ordain")).unwrap();
    fs::write(repo.path().join(".ordain/rubric.json"), json!({
        "version":1,"compiledAt":"fixture","sources":[{"path":"AGENTS.md"}],
        "rules":[{"id":"raw-error","text":"Never expose raw errors","source":{"path":"AGENTS.md","line":1},"when":"edit","check":{"type":"model","question":{"type":"boolean","instructions":"Does this expose a raw error?"}}}]
    }).to_string()).unwrap();
    let target = repo.path().join("src/a.rs");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "danger\n").unwrap();
    let (base, server) = mock_server(
        "/systemone",
        json!({"answers":{"raw-error":{"type":"noul","noul":0.91}},"usage":{"input_tokens":10,"output_tokens":0}}),
    );
    let payload = json!({
        "session_id":"fixture-session","prompt_id":"fixture-turn","cwd":repo.path(),"hook_event_name":"PostToolUse",
        "tool_name":"Write","tool_input":{"file_path":target,"content":"danger\n"},"tool_response":{"originalFile":null,"structuredPatch":[]}
    });
    let output = run_with_input(
        Command::new(binary())
            .args(["__hook", "post-tool-use"])
            .env("ORDAIN_HOME_DIR", home.path())
            .envs(fixture_xdg(home.path()))
            .env(TYPESAFE_KEY_ENV, "fixture-key")
            .env("ORDAIN_TYPESAFE_BASE_URL", base),
        payload.to_string().as_bytes(),
    );
    assert!(output.status.success());
    let protocol: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(protocol["decision"], "block");
    assert!(
        protocol["reason"]
            .as_str()
            .unwrap()
            .contains("AGENTS.md line 1")
    );
    let events = fs::read_to_string(fixture_events(repo.path(), home.path())).unwrap();
    assert!(events.contains("\"blocked\":true"));
    assert!(!events.contains("fixture-key"));
    server.join().unwrap();
}

#[test]
fn turn_start_kills_a_hanging_git_filter_within_the_hook_budget() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fn git(root: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.name", "Fixture"]);
    git(
        repo.path(),
        &["config", "user.email", "fixture@example.test"],
    );
    fs::write(repo.path().join("AGENTS.md"), "rules\n").unwrap();
    fs::write(repo.path().join(".gitattributes"), "*.rs filter=slow\n").unwrap();
    git(
        repo.path(),
        &["config", "filter.slow.clean", "sleep 30; cat"],
    );
    git(repo.path(), &["add", "-f", "AGENTS.md", ".gitattributes"]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    fs::write(repo.path().join("slow.rs"), "fn slow() {}\n").unwrap();
    let payload = json!({"session_id":"slow","prompt_id":"turn","cwd":repo.path(),"hook_event_name":"UserPromptSubmit","prompt":"fixture"});
    let started = Instant::now();
    let output = run_with_input(
        Command::new(binary())
            .args(["__hook", "turn-start"])
            .env("ORDAIN_HOME_DIR", home.path())
            .envs(fixture_xdg(home.path())),
        payload.to_string().as_bytes(),
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn saved_credentials_are_owner_only_and_preserve_other_env_lines() {
    let dir = tempdir().unwrap();
    let file = dir.path().join(".env.local");
    fs::write(&file, "DATABASE_URL=fixture\n").unwrap();
    ordain::credentials::save_key(&file, TYPESAFE_KEY_ENV, "fixture-key").unwrap();
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = fs::read_to_string(file).unwrap();
    assert!(text.contains("DATABASE_URL=fixture"));
    assert!(text.contains("TYPESAFE_AI_API_KEY=fixture-key"));
}

#[test]
fn isolated_cli_workflow_validates_checks_reports_compiles_and_logs_in() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["config", "user.email", "fixture@example.test"]);
    fs::write(
        repo.path().join("AGENTS.md"),
        "Run the project formatter.\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    )
    .unwrap();
    fs::create_dir(repo.path().join(".ordain")).unwrap();
    fs::write(
        repo.path().join(".ordain/rubric.json"),
        json!({
            "version":1,"compiledAt":"synthetic-fixture","sources":[{"path":"AGENTS.md"}],
            "rules":[{"id":"run-formatter","text":"Run the project formatter.","source":{"path":"AGENTS.md","line":1},"check":{"type":"lint","how":"cargo fmt --check"}}]
        })
        .to_string(),
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "fixture"]);
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 2 }\n",
    )
    .unwrap();

    let validate = Command::new(binary())
        .args(["rubric", "validate"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .output()
        .unwrap();
    assert!(validate.status.success());
    assert!(String::from_utf8_lossy(&validate.stdout).contains("lint 1"));

    let check = Command::new(binary())
        .args(["check", "--json"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let check: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(check["sections"].as_array().unwrap().len(), 2);
    assert!(
        check["sections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|section| section["calls"]["groups"] == 0)
    );

    let report = Command::new(binary())
        .args(["report", "--json"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .output()
        .unwrap();
    assert!(report.status.success());
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["rules"].as_array().unwrap().len(), 1);

    fs::write(
        repo.path().join("AGENTS.md"),
        "Run the project formatter.\nKeep public APIs documented.\n",
    )
    .unwrap();
    let compile = Command::new(binary())
        .args(["compile", "--print"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .output()
        .unwrap();
    assert!(compile.status.success());
    let prompt = String::from_utf8_lossy(&compile.stdout);
    assert!(prompt.contains("project rubric"));
    assert!(prompt.contains("AGENTS.md"));

    let login = run_with_input(
        Command::new(binary())
            .arg("login")
            .current_dir(repo.path())
            .env("ORDAIN_HOME_DIR", home.path())
            .envs(fixture_xdg(home.path())),
        b"piped-fixture-key\n",
    );
    assert!(login.status.success());
    assert!(!String::from_utf8_lossy(&login.stdout).contains("piped-fixture-key"));
    let credential = home.path().join(".config/ordain/.env");
    assert!(
        fs::read_to_string(&credential)
            .unwrap()
            .contains("TYPESAFE_AI_API_KEY=piped-fixture-key")
    );
    assert_eq!(
        fs::metadata(credential).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn stop_hook_checks_shell_changes_from_the_turn_baseline() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["config", "user.email", "fixture@example.test"]);
    fs::write(
        repo.path().join("AGENTS.md"),
        "Never leave a debug switch enabled.\n",
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    let target = repo.path().join("src/lib.rs");
    fs::write(&target, "pub const DEBUG: bool = false;\n").unwrap();
    fs::create_dir(repo.path().join(".ordain")).unwrap();
    fs::write(
        repo.path().join(".ordain/rubric.json"),
        json!({
            "version":1,"compiledAt":"synthetic-fixture","sources":[{"path":"AGENTS.md"}],
            "rules":[{"id":"no-debug-switch","text":"Never leave a debug switch enabled.","source":{"path":"AGENTS.md","line":1},"when":"turn","check":{"type":"model","question":{"type":"boolean","instructions":"Does the completed change leave a debug switch enabled?"}}}]
        })
        .to_string(),
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "fixture"]);

    let start = json!({"session_id":"shell-session","prompt_id":"turn-1","cwd":repo.path(),"hook_event_name":"UserPromptSubmit","prompt":"enable diagnostics"});
    let output = run_with_input(
        Command::new(binary())
            .args(["__hook", "turn-start"])
            .env("ORDAIN_HOME_DIR", home.path())
            .envs(fixture_xdg(home.path())),
        start.to_string().as_bytes(),
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    fs::write(&target, "pub const DEBUG: bool = true;\n").unwrap();

    let (base, server) = mock_server(
        "/systemone",
        json!({"answers":{"no-debug-switch":{"type":"noul","noul":0.95}},"usage":{"input_tokens":12,"output_tokens":1}}),
    );
    let stop = json!({"session_id":"shell-session","prompt_id":"turn-1","cwd":repo.path(),"hook_event_name":"Stop"});
    let output = run_with_input(
        Command::new(binary())
            .args(["__hook", "stop"])
            .env("ORDAIN_HOME_DIR", home.path())
            .envs(fixture_xdg(home.path()))
            .env(TYPESAFE_KEY_ENV, "fixture-key")
            .env("ORDAIN_TYPESAFE_BASE_URL", base),
        stop.to_string().as_bytes(),
    );
    assert!(output.status.success());
    let protocol: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(protocol["decision"], "block");
    let request = server.join().unwrap();
    assert!(
        request["state"]["diff"]
            .as_str()
            .unwrap()
            .contains("DEBUG: bool = true")
    );
    let events = fs::read_to_string(fixture_events(repo.path(), home.path())).unwrap();
    assert!(events.contains("\"phase\":\"turn\""));
    assert!(events.contains("\"blocked\":true"));
}

#[test]
fn hook_stdin_wait_is_time_limited() {
    let started = Instant::now();
    let mut child = Command::new(binary())
        .args(["__hook", "session-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _open_stdin = child.stdin.take().unwrap();
    let status = child
        .wait_timeout(Duration::from_secs(4))
        .unwrap()
        .expect("hook did not enforce its stdin deadline");
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(3));
}
