mod common;
use common::*;
use serde_json::{Value, json};
use std::fs;
use std::process::Command;
use tempfile::{TempDir, tempdir};

struct Project {
    root: TempDir,
    home: TempDir,
}
impl Project {
    fn new() -> Self {
        let p = Self {
            root: tempdir().unwrap(),
            home: tempdir().unwrap(),
        };
        fs::write(
            p.root.path().join("AGENTS.md"),
            "Use the local project convention.\n",
        )
        .unwrap();
        write_rubric(
            p.root.path(),
            json!([model_rule("local-style", "edit", None)]),
        );
        p
    }
    fn config(&self, text: &str) {
        fs::write(self.root.path().join(".ordain/config.toml"), text).unwrap();
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ordain"));
        cmd.current_dir(self.root.path())
            .env("ORDAIN_HOME_DIR", self.home.path())
            .envs(fixture_xdg(self.home.path()))
            .env("TYPESAFE_AI_API_KEY", "fixture-key")
            .env_remove("AI_GATEWAY_API_KEY");
        cmd
    }
    fn payload(&self, after: &str) -> Value {
        fs::write(self.root.path().join("a.rs"), after).unwrap();
        json!({"session_id":"policy-contract","prompt_id":"one","cwd":self.root.path(),"hook_event_name":"PostToolUse","tool_name":"Write", "tool_input":{"file_path":self.root.path().join("a.rs"),"content":after}, "tool_response":{"originalFile":null}})
    }
    fn hook(&self, payload: &Value, endpoint: &str) -> Value {
        let output = run_with_input(
            self.command()
                .args(["__hook", "post-tool-use"])
                .env("ORDAIN_TYPESAFE_BASE_URL", endpoint),
            payload.to_string().as_bytes(),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if output.stdout.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&output.stdout).unwrap()
        }
    }
    fn events(&self) -> String {
        fs::read_to_string(fixture_events(self.root.path(), self.home.path())).unwrap()
    }
}
fn score(body: &Value, value: f64) -> (u16, Value) {
    let answers: serde_json::Map<String, Value> = body["questions"]
        .as_object()
        .unwrap()
        .keys()
        .map(|id| (id.clone(), json!({"type":"noul","noul":value})))
        .collect();
    (200, json!({"answers":answers}))
}

#[test]
fn calibration_uses_commit_sources_and_preserves_rubric_on_missing_evidence() {
    let p = Project::new();
    init_repo(p.root.path());
    let original = "pub fn value() -> &'static str {\n    // retained historical context\n    \"COMMITTED_BEFORE\"\n}\n";
    let changed = "pub fn value() -> &'static str {\n    // The historical fixture deliberately exceeds the commit sampler's minimum patch size.\n    // Both edit and turn calibration must receive the same immutable source images.\n    let value = \"COMMITTED_AFTER\";\n    value\n}\n";
    fs::write(p.root.path().join("a.rs"), original).unwrap();
    fs::write(p.root.path().join("support.rs"), "COMMITTED_RELATED\n").unwrap();
    git(p.root.path(), &["add", "a.rs", "support.rs"]);
    git(p.root.path(), &["commit", "-qm", "baseline"]);
    fs::write(p.root.path().join("a.rs"), changed).unwrap();
    git(
        p.root.path(),
        &["commit", "-qam", "Change the value implementation"],
    );
    write_rubric(
        p.root.path(),
        json!([
            model_rule("edit-style", "edit", Some(json!(["a.rs"]))),
            model_rule("turn-style", "turn", Some(json!(["a.rs"])))
        ]),
    );
    p.config("[defaults.context]\nmode='changed_files'\ninclude=['support.rs']\n");
    fs::write(p.root.path().join("a.rs"), "TODAYS_WORKTREE\n").unwrap();
    fs::write(p.root.path().join("support.rs"), "TODAYS_RELATED\n").unwrap();
    let (endpoint, server) = fixture_server(2, |body| {
        let sent = body.to_string();
        for marker in ["COMMITTED_BEFORE", "COMMITTED_AFTER", "COMMITTED_RELATED"] {
            assert!(sent.contains(marker), "missing {marker}: {body}");
        }
        assert!(!sent.contains("TODAYS_"), "{body}");
        score(body, 0.1)
    });
    let output = p
        .command()
        .args(["calibrate", "--hunks", "1", "--commits", "1", "--json"])
        .env("ORDAIN_TYPESAFE_BASE_URL", endpoint)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap().len(), 2);
    let rubric_path = p.root.path().join(".ordain/rubric.json");
    let rubric = fs::read(&rubric_path).unwrap();
    // A required file available only today cannot backfill missing historical context.
    fs::write(p.root.path().join("today-only.rs"), "NOT_HISTORICAL\n").unwrap();
    for policy in [
        "[defaults.context]\nmode='changed_files'\ninclude=['today-only.rs']\n",
        "[defaults.context]\nmode='changed_files'\nmax_bytes=1024\n",
    ] {
        p.config(policy);
        let output = p
            .command()
            .args(["calibrate", "--hunks", "1", "--commits", "1", "--json"])
            .env("ORDAIN_TYPESAFE_BASE_URL", "http://127.0.0.1:9")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["calls"]["attempts"], 0, "{result}");
        assert_eq!(result["rubricUpdated"], false);
        assert!(
            result["errors"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["code"] == "CONTEXT_INCOMPLETE"),
            "{result}"
        );
        assert_eq!(fs::read(&rubric_path).unwrap(), rubric);
    }
}

#[test]
fn preset_calibration_updates_only_its_snapshot_and_preserves_concurrent_edits() {
    for concurrent_edit in [false, true] {
        let p = Project::new();
        assert!(
            p.command()
                .args(["preset", "add", "rust"])
                .output()
                .unwrap()
                .status
                .success()
        );
        let project_path = p.root.path().join(".ordain/rubric.json");
        let project_before = fs::read(&project_path).unwrap();
        init_repo(p.root.path());
        fs::write(
            p.root.path().join("a.rs"),
            "pub fn count(values: &[String]) -> usize { values.len() }\n",
        )
        .unwrap();
        git(p.root.path(), &["add", "a.rs"]);
        git(p.root.path(), &["commit", "-qm", "baseline"]);
        fs::write(p.root.path().join("a.rs"), "pub fn count(values: &[String]) -> usize {\n    // Count names using an ordinary borrowed view of the existing collection.\n    values.len()\n}\n").unwrap();
        git(
            p.root.path(),
            &["commit", "-qam", "Document the borrowed count"],
        );
        let snapshot = ordain::presets::path(p.root.path());
        let outside = format!("{}\n", fs::read_to_string(&snapshot).unwrap());
        let target = snapshot.clone();
        let replacement = outside.clone();
        let (endpoint, server) = fixture_server(1, move |body| {
            assert!(
                body["questions"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .all(|id| id.starts_with("preset-rust-"))
            );
            if concurrent_edit {
                fs::write(&target, &replacement).unwrap();
            }
            score(body, 0.1)
        });
        let output = p
            .command()
            .args([
                "calibrate",
                "--presets",
                "--hunks",
                "1",
                "--commits",
                "0",
                "--json",
            ])
            .env("ORDAIN_TYPESAFE_BASE_URL", endpoint)
            .output()
            .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&project_path).unwrap(), project_before);
        if concurrent_edit {
            assert!(!output.status.success());
            assert_eq!(fs::read_to_string(&snapshot).unwrap(), outside);
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let saved: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
            assert!(
                saved["rules"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["calibration"]["hunks"] == 1)
            );
        }
    }
}

#[test]
fn historical_mixed_context_checks_do_not_require_sources_for_diff_only_rules() {
    let p = Project::new();
    init_repo(p.root.path());
    for file in ["a.rs", "b.rs"] {
        fs::write(p.root.path().join(file), "pub fn before() {}\n").unwrap();
    }
    git(p.root.path(), &["add", "a.rs", "b.rs"]);
    git(p.root.path(), &["commit", "-qm", "baseline"]);
    for file in ["a.rs", "b.rs"] {
        fs::write(p.root.path().join(file), "pub fn after() {\n    // A changed implementation for historical policy evaluation.\n}\n").unwrap();
    }
    git(p.root.path(), &["commit", "-qam", "Update both functions"]);
    write_rubric(
        p.root.path(),
        json!([
            model_rule("source-style", "turn", Some(json!(["a.rs"]))),
            model_rule("diff-style", "turn", Some(json!(["b.rs"])))
        ]),
    );
    p.config("[rules.source-style.context]\nmode='changed_files'\n");
    let (endpoint, server) = fixture_server(2, |body| score(body, 0.1));
    let output = p
        .command()
        .args(["calibrate", "--hunks", "0", "--commits", "1", "--json"])
        .env("ORDAIN_TYPESAFE_BASE_URL", endpoint)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn native_repair_uses_validated_prior_observation_not_an_ambiguous_brace() {
    let p = Project::new();
    p.config("[defaults.context]\nmode = \"changed_files\"\n");
    let before = "fn first() {\n    helper();\n}\nfn helper() {\n    work();\n}\nfn other() {\n}\n";
    let after = "fn first() {\n    work();\n}\nfn other() {\n}\n";
    let initial = format!(
        "*** Begin Patch\n*** Add File: a.rs\n{}\n*** End Patch",
        before
            .lines()
            .map(|s| format!("+{s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let repair = "*** Begin Patch\n*** Update File: a.rs\n@@\n fn first() {\n-    helper();\n+    work();\n@@\n-}\n-fn helper() {\n-    work();\n }\n*** End Patch";
    let payload = |command: &str| json!({"session_id":"policy-contract", "prompt_id":"one", "cwd":p.root.path(), "hook_event_name":"PostToolUse", "tool_name":"apply_patch", "tool_input":{"command":command}, "tool_response":{"success":true}});
    let (endpoint, server) = fixture_server(2, |body| score(body, 0.1));
    fs::write(p.root.path().join("a.rs"), before).unwrap();
    assert!(p.hook(&payload(&initial), &endpoint).is_null());
    fs::write(p.root.path().join("a.rs"), after).unwrap();
    assert!(p.hook(&payload(repair), &endpoint).is_null());
    let requests = server.join().unwrap();
    let source = &requests[1]["state"]["source_evidence"][0];
    assert_eq!(source["before"][0]["text"], before);
    assert_eq!(source["after"][0]["text"], after);
    assert!(!p.events().contains("CONTEXT_INCOMPLETE"));

    // A shell edit invalidates the saved candidate. Never guess which brace the
    // deletion belonged to, or send fabricated complete-file evidence.
    fs::write(
        p.root.path().join("a.rs"),
        format!("{after}// concurrent change\n"),
    )
    .unwrap();
    let result = p.hook(&payload(repair), "http://127.0.0.1:1");
    assert!(result.to_string().contains("CONTEXT_INCOMPLETE"));
}

#[test]
fn policy_inheritance_is_sparse_strict_and_explainable_without_credentials() {
    let p = Project::new();
    p.config(
        r#"
[defaults]
actions = [{ at = 0.5, action = "notice" }, { at = 0.8, action = "block" }]
deadline_ms = 4000
[defaults.context]
mode = "changed_files"
max_bytes = 120000
[rules.local-style]
actions = [{ at = 0.35, action = "block" }]
[rules.local-style.context]
max_bytes = 180000
"#,
    );
    let output = p
        .command()
        .args(["config", "explain", "--rule", "local-style"])
        .env_remove("TYPESAFE_AI_API_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let policy = &value["rules"]["local-style"];
    assert_eq!(policy["context"]["mode"], "changed_files");
    assert_eq!(policy["context"]["max_bytes"], 180000);
    assert_eq!(policy["deadline_ms"], 4000);
    assert_eq!(policy["actions"].as_array().unwrap().len(), 1);
    assert_eq!(policy["origins"]["context.max_bytes"], "rules.local-style");
    assert_eq!(policy["origins"]["deadline_ms"], "project defaults");
    for bad in [
        "[rules.missing]\nenabled=false",
        "[defaults]\ndeadline_mss=4",
        "[defaults]\nactions=[{at=0.8,action='block'},{at=0.3,action='notice'}]",
        "[defaults]\nexclude=['[']",
        "[defaults.context]\ninclude=['../private']",
        "[defaults]\nphases=['turn']\nactions=[{at=0.3,action='steer'}]",
    ] {
        p.config(bad);
        let output = p.command().args(["config", "validate"]).output().unwrap();
        assert!(!output.status.success(), "accepted {bad}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("SETTINGS_INVALID"));
    }
}

#[test]
fn per_rule_action_boundaries_grouping_and_repair_caps_are_real_hook_decisions() {
    let p = Project::new();
    write_rubric(
        p.root.path(),
        json!([
            model_rule("local-style", "edit", None),
            model_rule("second-style", "edit", None)
        ]),
    );
    p.config(
        r#"
[rules.local-style]
actions=[{at=0.4,action="block"}]
max_repairs=1
[rules.second-style]
actions=[]
"#,
    );
    let payload = p.payload("let n = 1;\n");
    let (endpoint, server) = fixture_server(2, |b| score(b, 0.4));
    let first = p.hook(&payload, &endpoint);
    assert_eq!(first["decision"], "block");
    assert!(first["reason"].as_str().unwrap().contains("0.40"));
    let second = p.hook(&payload, &endpoint);
    assert!(second["decision"].is_null());
    assert!(
        second["systemMessage"]
            .as_str()
            .unwrap()
            .contains("Repair limit reached"),
        "{second}"
    );
    let bodies = server.join().unwrap();
    assert!(
        bodies
            .iter()
            .all(|b| b["questions"].as_object().unwrap().len() == 2),
        "threshold-only differences must share requests"
    );
    let events = p.events();
    assert!(events.contains("policyRevision"));
    assert!(events.contains("snapshot"));
    assert!(!events.contains("fixture-key"));
}

#[test]
fn preset_only_hooks_deliver_notices_unless_explicitly_configured_to_block() {
    let p = Project::new();
    fs::remove_file(p.root.path().join(".ordain/rubric.json")).unwrap();
    assert!(
        p.command()
            .args(["preset", "add", "rust"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let payload = p.payload("fn inspect(values: &[String]) -> usize { values.to_vec().len() }\n");
    let (endpoint, server) = fixture_server(1, |body| score(body, 0.96));
    let notice = p.hook(&payload, &endpoint);
    server.join().unwrap();
    assert!(notice["decision"].is_null());
    assert!(notice["hookSpecificOutput"].is_null());
    assert!(
        notice["systemMessage"]
            .as_str()
            .unwrap()
            .contains("preset-rust-borrow-inspection")
    );
    p.config("[rules.preset-rust-borrow-inspection]\nactions=[{at=0.8,action='block'}]");
    let (endpoint, server) = fixture_server(1, |body| score(body, 0.96));
    let block = p.hook(&payload, &endpoint);
    server.join().unwrap();
    assert_eq!(block["decision"], "block");
    assert!(block["reason"].as_str().unwrap().contains("preset:rust@2"));
    let repair = p.payload("fn inspect(values: &[String]) -> usize { values.len() }\n");
    let (endpoint, server) = fixture_server(1, |body| score(body, 0.04));
    assert!(p.hook(&repair, &endpoint).is_null());
    server.join().unwrap();
}

#[test]
fn edit_steering_is_agent_context_not_a_ui_notice_or_block() {
    let p = Project::new();
    p.config("[rules.local-style]\nactions=[{at=0.3,action='steer'}]");
    let payload = p.payload("fn f() {}\n");
    let (endpoint, server) = fixture_server(1, |b| score(b, 0.4));
    let out = p.hook(&payload, &endpoint);
    assert!(out["decision"].is_null());
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("confidence 0.40")
    );
    server.join().unwrap();
}

#[test]
fn full_source_and_related_evidence_are_bounded_and_never_silently_downgraded() {
    let p = Project::new();
    p.config("[defaults.context]\nmode='changed_files'\ninclude=['api.rs']");
    fs::write(
        p.root.path().join("api.rs"),
        "// API accepts malformed inputs.\n",
    )
    .unwrap();
    let payload = p.payload(&format!("fn parse() {{}}\n{}", "// context\n".repeat(100)));
    let (endpoint, server) = fixture_server(1, |b| score(b, 0.1));
    p.hook(&payload, &endpoint);
    let bodies = server.join().unwrap();
    let sources = &bodies[0]["state"]["source_evidence"];
    assert_eq!(sources.as_array().unwrap().len(), 2);
    assert!(
        sources[0]["after"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("fn parse() {}\n")
    );
    assert_eq!(
        sources[1]["after"][0]["text"],
        "// API accepts malformed inputs.\n"
    );
    assert!(bodies[0]["state"]["rules"][0]["text"].is_string());
    let events = fs::read_to_string(fixture_events(p.root.path(), p.home.path())).unwrap();
    let check: Value = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["verdicts"].is_array())
        .unwrap();
    assert_eq!(
        check["verdicts"][0]["evidence"]["files"],
        json!(["a.rs", "api.rs"])
    );

    for config in [
        "[defaults.context]\nmode='changed_files'\nmax_bytes=1024",
        "[defaults.context]\ninclude=['.env']",
        "[defaults.context]\ninclude=['missing.rs']",
    ] {
        p.config(config);
        let out = p.hook(&payload, "http://127.0.0.1:1");
        assert!(
            out["systemMessage"]
                .as_str()
                .unwrap()
                .contains("CONTEXT_INCOMPLETE"),
            "{out}"
        );
        assert!(out["decision"].is_null());
    }
    // A historical patch is not permission to send today's complete source as its context.
    p.config("[defaults.context]\nmode='changed_files'");
    let patch = p.root.path().join("saved.patch");
    fs::write(&patch, "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n").unwrap();
    let out = p
        .command()
        .args(["check", "--phase", "edit", "--diff"])
        .arg(patch)
        .arg("--json")
        .env("ORDAIN_TYPESAFE_BASE_URL", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("CONTEXT_INCOMPLETE"));
}

#[test]
fn source_and_policy_changes_during_inference_suppress_stale_delivery() {
    for (file, replacement) in [
        ("a.rs", "fn changed() {}\n"),
        (".ordain/config.toml", "[defaults]\nactions=[]"),
        (
            ".ordain/presets.json",
            "{\"version\":1,\"compiledAt\":\"now\",\"sources\":[],\"rules\":[]}",
        ),
    ] {
        let p = Project::new();
        let payload = p.payload("fn before() {}\n");
        let target = p.root.path().join(file);
        let (endpoint, server) = fixture_server(1, move |b| {
            fs::write(&target, replacement).unwrap();
            score(b, 0.99)
        });
        let out = p.hook(&payload, &endpoint);
        server.join().unwrap();
        assert!(out["decision"].is_null());
        assert!(
            out["systemMessage"]
                .as_str()
                .unwrap()
                .contains("SUPERSEDED"),
            "{out}"
        );
        assert!(!p.events().contains("\"blocked\":true"));
    }
}

#[test]
fn xdg_paths_and_worktree_state_are_isolated_and_relative_xdg_is_ignored() {
    let p = Project::new();
    let other = Project::new();
    let payload = p.payload("fn a() {}\n");
    let (endpoint, server) = fixture_server(1, |b| score(b, 0.1));
    p.hook(&payload, &endpoint);
    server.join().unwrap();
    assert!(fixture_events(p.root.path(), p.home.path()).exists());
    assert!(!fixture_events(other.root.path(), p.home.path()).exists());
    assert!(!p.root.path().join(".ordain/events.jsonl").exists());
    assert!(!p.home.path().join(".ordain").exists());
    let out = p
        .command()
        .args(["config", "explain"])
        .env("XDG_CONFIG_HOME", "relative")
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        value["config_home"],
        p.home
            .path()
            .join(".config/ordain")
            .to_string_lossy()
            .as_ref()
    );
}

#[test]
fn policy_changes_invalidate_edit_coverage_at_stop() {
    let p = Project::new();
    init_repo(p.root.path());
    git(p.root.path(), &["config", "core.excludesFile", "/dev/null"]);
    git(p.root.path(), &["add", "AGENTS.md"]);
    git(p.root.path(), &["commit", "-qm", "baseline"]);
    let mut lifecycle = json!({"session_id":"policy-contract","prompt_id":"one","cwd":p.root.path(),"hook_event_name":"UserPromptSubmit","prompt":"Add a function."});
    assert!(
        run_with_input(
            p.command().args(["__hook", "turn-start"]),
            lifecycle.to_string().as_bytes()
        )
        .status
        .success()
    );
    let payload = p.payload("fn example() {}\n");
    let (endpoint, server) = fixture_server(2, |b| score(b, 0.4));
    let edit = p.hook(&payload, &endpoint);
    assert!(edit["decision"].is_null());
    p.config("[rules.local-style]\nactions=[{at=0.35,action='block'}]");
    lifecycle["hook_event_name"] = json!("Stop");
    let stop = run_with_input(
        p.command()
            .args(["__hook", "stop"])
            .env("ORDAIN_TYPESAFE_BASE_URL", &endpoint),
        lifecycle.to_string().as_bytes(),
    );
    let result: Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert_eq!(result["decision"], "block", "{result}");
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn changed_ranges_preserve_coordinates_and_reject_ambiguous_native_evidence() {
    use ordain::{
        config::{ContextMode, ContextPolicy},
        evidence::Snapshot,
        model::FileDiff,
    };
    use std::time::{Duration, Instant};
    let p = Project::new();
    let before = (1..=100).map(|n| format!("line {n}\n")).collect::<String>();
    let after = before.replace("line 50\n", "updated 50\n");
    let mut snapshot = Snapshot::default();
    snapshot.insert("a.rs".into(), Some(before), Some(after));
    let diff = FileDiff {
        file: "a.rs".into(),
        text: "@@ -50 +50 @@\n-line 50\n+updated 50".into(),
    };
    let policy = ContextPolicy {
        mode: ContextMode::ChangedRanges,
        surrounding_lines: 2,
        ..ContextPolicy::default()
    };
    let evidence = snapshot.select(&[&diff], &policy).unwrap();
    assert!(!evidence[0].complete_file);
    let range = &evidence[0].after.as_ref().unwrap()[0];
    assert!(range.start_line > 1 && range.end_line < 100);
    assert!(range.text.contains("updated 50\n"));
    snapshot.insert("api.rs".into(), None, Some("pub fn api() {}\n".into()));
    let policy = ContextPolicy {
        mode: ContextMode::Diff,
        include: vec!["api.rs".into()],
        ..ContextPolicy::default()
    };
    let evidence = snapshot.select(&[&diff], &policy).unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].file, "api.rs");
    fs::write(p.root.path().join("a.rs"), "same\nsame\n").unwrap();
    let ambiguous = FileDiff {
        file: "a.rs".into(),
        text: "@@\n-old\n+same".into(),
    };
    let error = Snapshot::capture(
        p.root.path(),
        &[ambiguous],
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap_err();
    assert_eq!(error.code.as_str(), "CONTEXT_INCOMPLETE");
}

#[test]
fn edit_steering_found_by_stop_is_reported_as_unsupported_not_delivered() {
    let p = Project::new();
    init_repo(p.root.path());
    git(p.root.path(), &["add", "AGENTS.md"]);
    git(p.root.path(), &["commit", "-qm", "baseline"]);
    p.config("[rules.local-style]\nactions=[{at=0.35,action='steer'}]");
    let mut lifecycle = json!({"session_id":"fallback","prompt_id":"one","cwd":p.root.path(),"hook_event_name":"UserPromptSubmit","prompt":"Add a function."});
    run_with_input(
        p.command().args(["__hook", "turn-start"]),
        lifecycle.to_string().as_bytes(),
    );
    fs::write(p.root.path().join("a.rs"), "fn example() {}\n").unwrap();
    lifecycle["hook_event_name"] = json!("Stop");
    let (endpoint, server) = fixture_server(1, |b| score(b, 0.4));
    let result = run_with_input(
        p.command()
            .args(["__hook", "stop"])
            .env("ORDAIN_TYPESAFE_BASE_URL", endpoint),
        lifecycle.to_string().as_bytes(),
    );
    let result: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(result["hookSpecificOutput"].is_null(), "{result}");
    assert!(
        result["systemMessage"]
            .as_str()
            .unwrap()
            .contains("UNSUPPORTED_DELIVERY")
    );
    server.join().unwrap();
    let events = fs::read_to_string(fixture_events(p.root.path(), p.home.path())).unwrap();
    assert!(events.contains("UNSUPPORTED_DELIVERY"));
    assert!(!events.contains("\"steered_rules\":[\"local-style\"]"));
}
