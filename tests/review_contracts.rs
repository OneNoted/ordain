mod common;
use common::{
    fixture_events, fixture_server, fixture_xdg, git, init_repo, model_rule, run_with_input,
    write_rubric,
};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use ordain::credentials::TYPESAFE_KEY_ENV;
use serde_json::{Value, json};
use tempfile::tempdir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_ordain")
}

#[test]
fn git_quoted_paths_are_decoded_before_secret_filtering_and_scope_matching() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    init_repo(repo.path());
    fs::write(repo.path().join("AGENTS.md"), "fixture\n").unwrap();
    fs::create_dir(repo.path().join("näme")).unwrap();
    fs::write(repo.path().join("näme/.env"), "SYNTHETIC_SECRET=old\n").unwrap();
    fs::write(repo.path().join("näme/file.rs"), "fn old() {}\n").unwrap();
    git(repo.path(), &["add", "-f", "."]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    write_rubric(
        repo.path(),
        json!([model_rule(
            "quoted-scope",
            "edit",
            Some(json!(["näme/*.rs"]))
        )]),
    );
    fs::write(repo.path().join("näme/.env"), "SYNTHETIC_SECRET=new\n").unwrap();
    fs::write(repo.path().join("näme/file.rs"), "fn new() {}\n").unwrap();

    let (base, server) = fixture_server(1, |_| {
        (
            200,
            json!({"answers":{"quoted-scope":{"type":"noul","noul":0.1}}}),
        )
    });
    let output = Command::new(binary())
        .args(["check", "--phase", "edit", "--json"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("ORDAIN_TYPESAFE_BASE_URL", base)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bodies = server.join().unwrap();
    assert_eq!(bodies[0]["state"]["file"], "näme/file.rs");
    let sent = bodies[0].to_string();
    assert!(!sent.contains("SYNTHETIC_SECRET"));
    assert!(!sent.contains("\\303"));
}

#[test]
fn git_collection_failure_is_not_reported_as_a_clean_check() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    init_repo(repo.path());
    fs::write(repo.path().join("AGENTS.md"), "fixture\n").unwrap();
    fs::write(repo.path().join("a.rs"), "old\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    fs::write(repo.path().join("a.rs"), "changed\n").unwrap();
    write_rubric(
        repo.path(),
        json!([model_rule("git-failure", "edit", None)]),
    );

    let tools = tempdir().unwrap();
    let real_git = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let wrapper = tools.path().join("git");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = rev-parse ]; then exec {} \"$@\"; fi\nexit 128\n",
            real_git.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        tools.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(binary())
        .args(["check", "--json"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("PATH", path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("GIT_UNAVAILABLE"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Nothing to check"));
}

#[test]
fn invalid_scope_glob_is_rejected_by_rubric_validation() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join("AGENTS.md"), "fixture\n").unwrap();
    write_rubric(
        repo.path(),
        json!([model_rule("bad-scope", "edit", Some(json!(["["])))]),
    );
    let output = Command::new(binary())
        .args(["rubric", "validate"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid glob"));
}

#[test]
fn mixed_hook_batch_keeps_a_known_violation_when_a_sibling_fails() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join("AGENTS.md"), "fixture\n").unwrap();
    write_rubric(repo.path(), json!([model_rule("mixed", "edit", None)]));
    fs::write(repo.path().join("bad.rs"), "bad\n").unwrap();
    fs::write(repo.path().join("error.rs"), "error\n").unwrap();
    let (base, server) = fixture_server(2, |body| {
        if body["state"]["file"] == "bad.rs" {
            (
                200,
                json!({"answers":{"mixed":{"type":"noul","noul":0.99}}}),
            )
        } else {
            (400, json!({"message":"synthetic fixture rejection"}))
        }
    });
    let payload = json!({
        "session_id":"mixed-fixture","prompt_id":"turn","cwd":repo.path(),"hook_event_name":"PostToolUse",
        "tool_name":"apply_patch","tool_input":{"command":"*** Begin Patch\n*** Add File: bad.rs\n+bad\n*** Add File: error.rs\n+error\n*** End Patch"}
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
    let events = fs::read_to_string(fixture_events(repo.path(), home.path())).unwrap();
    assert!(
        events
            .lines()
            .any(|line| line.contains("\"blocked\":true") && line.contains("bad.rs"))
    );
    assert!(
        events
            .lines()
            .any(|line| line.contains("\"kind\":\"error\"")
                && line.contains("synthetic fixture rejection"))
    );
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn failed_calibration_preserves_existing_rubric_bytes_and_exits_incomplete() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    init_repo(repo.path());
    fs::write(repo.path().join("AGENTS.md"), "fixture\n").unwrap();
    fs::write(repo.path().join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "initial fixture"]);
    fs::write(repo.path().join("a.rs"), "ONE\nTWO\nTHREE\nFOUR\n").unwrap();
    git(repo.path(), &["add", "a.rs"]);
    git(repo.path(), &["commit", "-qm", "changed fixture"]);
    let mut rule = model_rule("calibration-fixture", "edit", None);
    rule["calibration"] = json!({
        "at":"prior","hunks":9,"median":0.2,"min":0.1,"max":0.3,"fired":0,"verdict":"decisive"
    });
    write_rubric(repo.path(), json!([rule]));
    let rubric = repo.path().join(".ordain/rubric.json");
    let before = fs::read(&rubric).unwrap();
    let (base, server) = fixture_server(3, |_| {
        (500, json!({"message":"synthetic calibration outage"}))
    });
    let output = Command::new(binary())
        .args(["calibrate", "--hunks", "1", "--commits", "0", "--json"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("ORDAIN_TYPESAFE_BASE_URL", base)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["rubricUpdated"], false);
    assert!(!result["errors"].as_array().unwrap().is_empty());
    assert_eq!(fs::read(rubric).unwrap(), before);
    assert_eq!(server.join().unwrap().len(), 3);
}

#[test]
fn scope_groups_are_deterministic_and_attempt_counts_include_retries() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    init_repo(repo.path());
    fs::write(repo.path().join("AGENTS.md"), "fixture\n").unwrap();
    fs::create_dir(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "old a\n").unwrap();
    fs::write(repo.path().join("src/b.rs"), "old b\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    write_rubric(
        repo.path(),
        json!([
            model_rule("all-src", "turn", Some(json!(["src/**"]))),
            model_rule("only-a", "turn", Some(json!(["src/a.rs"])))
        ]),
    );
    fs::write(repo.path().join("src/a.rs"), "new a\n").unwrap();
    fs::write(repo.path().join("src/b.rs"), "new b\n").unwrap();
    let mut only_a_attempts = 0;
    let (base, server) = fixture_server(3, move |body| {
        let answers = body["questions"]
            .as_object()
            .unwrap()
            .keys()
            .map(|id| (id.clone(), json!({"type":"noul","noul":0.1})))
            .collect::<serde_json::Map<_, _>>();
        if body["questions"].get("only-a").is_some() && only_a_attempts == 0 {
            only_a_attempts += 1;
            (500, json!({"message":"synthetic transient"}))
        } else {
            (200, json!({"answers":answers}))
        }
    });
    let output = Command::new(binary())
        .args(["check", "--phase", "turn", "--json"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("ORDAIN_TYPESAFE_BASE_URL", base)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let calls = &result["sections"][0]["calls"];
    assert_eq!(calls["groups"], 2);
    assert_eq!(calls["attempts"], 3);
    assert_eq!(calls["succeeded"], 2);
    let bodies = server.join().unwrap();
    assert!(
        bodies
            .iter()
            .any(|body| body["state"]["files"].as_array().unwrap().len() == 2)
    );
    assert!(
        bodies
            .iter()
            .any(|body| body["state"]["files"] == json!(["src/a.rs"]))
    );
}

#[test]
fn report_marks_corrupt_history_instead_of_treating_it_as_clean() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    fs::write(repo.path().join("AGENTS.md"), "fixture\n").unwrap();
    write_rubric(
        repo.path(),
        json!([{
            "id":"lint-fixture","text":"Synthetic lint fixture","source":{"path":"AGENTS.md","line":1},
            "check":{"type":"lint","how":"true"}
        }]),
    );
    fs::create_dir_all(fixture_events(repo.path(), home.path()).parent().unwrap()).unwrap();
    fs::write(fixture_events(repo.path(), home.path()), "not-json\n").unwrap();
    let output = Command::new(binary())
        .args(["report", "--json"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["history"]["complete"], false);
    assert_eq!(report["history"]["corruptLines"], 1);
    assert_eq!(report["events"], 0);
}

#[test]
fn audit_and_replay_use_exit_two_for_incomplete_evaluation() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    init_repo(repo.path());
    fs::write(repo.path().join("AGENTS.md"), "fixture\n").unwrap();
    fs::write(repo.path().join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    write_rubric(
        repo.path(),
        json!([model_rule("batch-error", "edit", None)]),
    );

    let (audit_base, audit_server) =
        fixture_server(2, |_| (400, json!({"message":"synthetic audit rejection"})));
    let audit = Command::new(binary())
        .args(["audit", "--json", "--concurrency", "1"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("ORDAIN_TYPESAFE_BASE_URL", audit_base)
        .output()
        .unwrap();
    assert_eq!(audit.status.code(), Some(2));
    let audit_json: Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert!(audit_json["byFile"][0]["error"].is_string());
    assert_eq!(audit_server.join().unwrap().len(), 2);

    let transcript = repo.path().join("synthetic-claude-session.jsonl");
    let target = repo.path().join("a.rs");
    let rows = [
        json!({"type":"user","cwd":repo.path(),"message":{"content":"synthetic task"}}),
        json!({"type":"assistant","cwd":repo.path(),"message":{"content":[{"type":"tool_use","name":"Edit","id":"fixture-call","input":{"file_path":target,"old_string":"one","new_string":"ONE"}}]}}),
        json!({"type":"user","cwd":repo.path(),"toolUseResult":{"originalFile":"one\ntwo\nthree\nfour\n"},"message":{"content":[{"type":"tool_result","tool_use_id":"fixture-call"}]}}),
    ];
    fs::write(
        &transcript,
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let (replay_base, replay_server) = fixture_server(1, |_| {
        (400, json!({"message":"synthetic replay rejection"}))
    });
    let replay = Command::new(binary())
        .args([
            "replay",
            "claude",
            transcript.to_string_lossy().as_ref(),
            "--repo",
            repo.path().to_string_lossy().as_ref(),
            "--json",
            "--concurrency",
            "1",
        ])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("ORDAIN_TYPESAFE_BASE_URL", replay_base)
        .output()
        .unwrap();
    assert_eq!(replay.status.code(), Some(2));
    let replay_json: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert!(replay_json["editResults"][0]["error"].is_string());
    assert_eq!(replay_server.join().unwrap().len(), 1);
}

#[test]
fn replay_reports_unreplayed_and_malformed_history_instead_of_a_clean_pass() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    init_repo(repo.path());
    write_rubric(repo.path(), json!([model_rule("fixture", "edit", None)]));
    let first = repo.path().join("rollout-first.jsonl");
    let second = repo.path().join("rollout-second.jsonl");
    for path in [&first, &second] {
        fs::write(path, [
            json!({"type":"session_meta","payload":{"cwd":repo.path()}}).to_string(),
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"shell","arguments":"{}"}}).to_string(),
        ].join("\n")).unwrap();
    }
    fs::OpenOptions::new()
        .append(true)
        .open(&second)
        .unwrap()
        .write_all(b"\n{broken")
        .unwrap();
    let output = Command::new(binary())
        .args([
            "replay",
            "codex",
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            first.to_str().unwrap(),
            "--json",
        ])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("ORDAIN_TYPESAFE_BASE_URL", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["sessions"], 2);
    assert_eq!(data["edits"], 0);
    assert_eq!(data["coverage"]["verdicts"], 0);
    assert_eq!(data["coverage"]["incomplete"], true);
    let sessions = data["coverage"]["traces"].as_array().unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|s| s["counts"]["otherToolCalls"].as_u64().unwrap())
            .sum::<u64>(),
        2
    );
    assert_eq!(
        sessions
            .iter()
            .map(|s| s["counts"]["malformedRecords"].as_u64().unwrap())
            .sum::<u64>(),
        1
    );
}

#[test]
fn calibration_audit_bench_and_tune_share_the_same_active_rule_policy() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    init_repo(repo.path());
    fs::write(
        repo.path().join("AGENTS.md"),
        "Do not log credentials.\nUse opaque identifiers.\n",
    )
    .unwrap();
    fs::create_dir(repo.path().join("src")).unwrap();
    for index in 0..5 {
        fs::write(
            repo.path().join(format!("src/f{index}.rs")),
            format!("pub fn number_{index}() -> u8 {{\n    let value = {index};\n    value\n}}\n"),
        )
        .unwrap();
    }
    git(repo.path(), &["add", "-f", "."]);
    git(
        repo.path(),
        &["commit", "-qm", "labelled calibration history fixture"],
    );
    write_rubric(
        repo.path(),
        json!([
            model_rule("weak", "edit", None),
            model_rule("noisy", "edit", None),
            model_rule("decisive", "edit", None),
        ]),
    );
    let mut request_index = 0;
    let (base, server) = fixture_server(21, move |body| {
        request_index += 1;
        // The final request is the measured hook, after successful direct checks.
        if request_index == 21 {
            return (
                400,
                json!({"message":"labelled hook-only provider failure"}),
            );
        }
        let answers: serde_json::Map<String, Value> = body["questions"]
            .as_object()
            .unwrap()
            .keys()
            .map(|id| {
                let probability = match id.as_str() {
                    "weak" => 0.4,
                    "noisy" => 0.95,
                    _ => 0.01,
                };
                (id.clone(), json!({"type":"noul","noul":probability}))
            })
            .collect();
        (200, json!({"answers":answers}))
    });
    let run = |args: &[&str]| {
        let output = Command::new(binary())
            .args(args)
            .current_dir(repo.path())
            .env("ORDAIN_HOME_DIR", home.path())
            .envs(fixture_xdg(home.path()))
            .env(TYPESAFE_KEY_ENV, "fixture-key")
            .env("ORDAIN_TYPESAFE_BASE_URL", &base)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    let calibration = run(&["calibrate", "--hunks", "5", "--commits", "0", "--json"]);
    let calibration: Value = serde_json::from_slice(&calibration.stdout).unwrap();
    assert_eq!(calibration["calls"]["succeeded"], 5);
    let saved = fs::read(repo.path().join(".ordain/rubric.json")).unwrap();
    let rubric: Value = serde_json::from_slice(&saved).unwrap();
    let statuses: Vec<_> = rubric["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| rule["status"].as_str().unwrap_or("active"))
        .collect();
    assert_eq!(statuses, ["weak", "noisy", "active"]);
    fs::write(
        repo.path().join("src/f0.rs"),
        "pub fn number_0() -> u8 { 9 }\n",
    )
    .unwrap();
    run(&["check", "--phase", "edit", "--json"]);
    run(&["audit", "src", "--json", "--concurrency", "1"]);
    let bench = run(&["bench", "--runs", "1", "--json"]);
    let bench: Value = serde_json::from_slice(&bench.stdout).unwrap();
    assert_eq!(bench["activeModelRules"], 1);
    let tune = run(&["tune", "--print"]);
    let prompt = String::from_utf8_lossy(&tune.stdout);
    assert!(prompt.contains("weak") && prompt.contains("noisy"));
    assert_eq!(
        fs::read(repo.path().join(".ordain/rubric.json")).unwrap(),
        saved
    );
    run(&["report", "--json"]);
    let failed_bench = Command::new(binary())
        .args(["bench", "--runs", "1", "--json"])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("ORDAIN_TYPESAFE_BASE_URL", &base)
        .output()
        .unwrap();
    assert_eq!(
        failed_bench.status.code(),
        Some(2),
        "hook failure must not become a successful latency sample"
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 21);
    for body in &requests[5..] {
        assert_eq!(
            body["questions"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["decisive"]
        );
    }
}

#[test]
fn replay_turn_evidence_keeps_source_order_with_multiple_workers() {
    let home = tempdir().unwrap();
    let repo = tempdir().unwrap();
    init_repo(repo.path());
    write_rubric(
        repo.path(),
        json!([
            model_rule("edit-rule", "edit", None),
            model_rule("turn-rule", "turn", None)
        ]),
    );
    let trace = repo.path().join("claude.jsonl");
    let mut records =
        vec![json!({"type":"user","cwd":repo.path(),"message":{"content":"change twice"}})];
    for (id, before, after) in [("1", "BASE", "FIRST"), ("2", "FIRST", "SECOND")] {
        records.push(json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","id":id,"input":{"file_path":repo.path().join("a.rs"),"old_string":before,"new_string":after}}]}}));
        records.push(
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":id}]}}),
        );
    }
    fs::write(
        &trace,
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let (base, server) = fixture_server(3, |body| {
        if body["questions"].get("turn-rule").is_some() {
            let diff = body["state"]["diff"].as_str().unwrap();
            assert!(diff.find("+FIRST").unwrap() < diff.find("+SECOND").unwrap());
        }
        let answers: serde_json::Map<String, Value> = body["questions"]
            .as_object()
            .unwrap()
            .keys()
            .map(|id| (id.clone(), json!({"type":"noul","noul":0.01})))
            .collect();
        (200, json!({"answers":answers}))
    });
    let output = Command::new(binary())
        .args([
            "replay",
            "claude",
            trace.to_str().unwrap(),
            "--concurrency",
            "4",
            "--json",
        ])
        .current_dir(repo.path())
        .env("ORDAIN_HOME_DIR", home.path())
        .envs(fixture_xdg(home.path()))
        .env(TYPESAFE_KEY_ENV, "fixture-key")
        .env("ORDAIN_TYPESAFE_BASE_URL", base)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap().len(), 3);
}
