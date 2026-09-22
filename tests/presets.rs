use std::fs;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Project(TempDir);
impl Project {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ordain"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.0.path())
            .env("XDG_CONFIG_HOME", self.0.path().join("config"))
            .env("XDG_STATE_HOME", self.0.path().join("state"))
            .env("XDG_CACHE_HOME", self.0.path().join("cache"))
            .current_dir(self.0.path())
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let result = self.cli(args);
        assert!(
            result.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice(&result.stdout).unwrap()
    }
    fn snapshot(&self) -> Value {
        serde_json::from_slice(&fs::read(ordain::presets::path(self.0.path())).unwrap()).unwrap()
    }
    fn save(&self, snapshot: &Value) {
        fs::write(
            ordain::presets::path(self.0.path()),
            serde_json::to_vec(snapshot).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn snapshot_lifecycle_is_opt_in_idempotent_and_explicitly_updated() {
    let project = Project::new();
    let list = project.ok(&["preset", "list"]);
    assert_eq!(list.as_array().unwrap().len(), 3);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .all(|r| r["selected"].is_null())
    );
    assert!(!project.0.path().join(".ordain").exists());
    for name in ["core", "rust", "typescript"] {
        let bundle = project.ok(&["preset", "show", name]);
        let parsed = serde_json::from_value(bundle).unwrap();
        ordain::presets::validate(&parsed).unwrap();
        project.ok(&["preset", "add", name]);
    }
    assert_eq!(project.snapshot()["rules"].as_array().unwrap().len(), 8);
    assert!(!project.0.path().join(".ordain/rubric.json").exists());
    let original = project.snapshot();
    let mut tuned = original.clone();
    tuned["rules"][0]["text"] = json!("Project-tuned text, retained until explicit update.");
    tuned["rules"][0]["status"] = json!("disabled");
    project.save(&tuned);
    assert!(project.cli(&["preset", "add", "core"]).status.success());
    assert_eq!(project.snapshot(), tuned);
    let preview = project.ok(&["preset", "update", "core"]);
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["before"][0]["text"], tuned["rules"][0]["text"]);
    assert_eq!(project.snapshot(), tuned);
    project.ok(&["preset", "update", "core", "--apply"]);
    let updated = project.snapshot();
    assert!(
        updated["rules"]
            .as_array()
            .unwrap()
            .contains(&original["rules"][0])
    );
    project.ok(&["preset", "remove", "rust"]);
    assert_eq!(project.snapshot()["rules"].as_array().unwrap().len(), 6);
    project.ok(&["preset", "validate"]);
}

#[test]
fn preset_policy_is_advisory_overridable_and_lower_priority_than_explicit_rules() {
    let project = Project::new();
    project.ok(&["preset", "add", "rust"]);
    let id = "preset-rust-bounded-lifetimes";
    let explanation = project.ok(&["config", "explain", "--rule", id]);
    assert_eq!(
        explanation["rules"][id]["actions"],
        json!([{"at":0.8,"action":"notice"}])
    );
    assert_eq!(explanation["rules"][id]["origins"]["rule"], "preset:rust@2");
    assert!(
        explanation["rules"][id]["exclude"]
            .as_array()
            .unwrap()
            .contains(&json!("**/generated/**"))
    );
    fs::write(
        project.0.path().join(".ordain/config.toml"),
        format!("[rules.{id}]\nactions = [{{ at = 0.85, action = 'block' }}]\n"),
    )
    .unwrap();
    let configured = project.ok(&["config", "explain", "--rule", id]);
    assert_eq!(
        configured["rules"][id]["actions"],
        json!([{"at":0.85,"action":"block"}])
    );
    let before = project.snapshot();
    assert!(!project.cli(&["preset", "remove", "rust"]).status.success());
    assert_eq!(
        project.snapshot(),
        before,
        "orphaned overrides must not leave partial removal"
    );
    let mut explicit = before.clone();
    explicit["rules"] = json!([explicit["rules"][0].clone()]);
    explicit["rules"][0]["source"]["path"] = json!("AGENTS.md");
    explicit["sources"] = json!([{"path":"AGENTS.md"}]);
    explicit["rules"][0]["status"] = json!("disabled");
    fs::write(
        project.0.path().join(".ordain/rubric.json"),
        explicit.to_string(),
    )
    .unwrap();
    let result = project.ok(&["config", "explain", "--rule", id]);
    assert_eq!(result["rules"][id]["enabled"], false);
    assert_eq!(result["rules"][id]["origins"]["rule"], "AGENTS.md");
    project.ok(&["preset", "remove", "rust"]);
}

#[test]
fn invalid_snapshot_is_not_silent_opt_out_and_calibration_does_not_need_history() {
    let project = Project::new();
    project.ok(&["preset", "add", "core"]);
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(project.0.path())
        .output()
        .unwrap();
    assert!(init.status.success());
    let before = fs::read(ordain::presets::path(project.0.path())).unwrap();
    let calibration = project.ok(&["calibrate", "--presets", "--json"]);
    assert_eq!(calibration["skipped"], "no usable history");
    assert_eq!(
        fs::read(ordain::presets::path(project.0.path())).unwrap(),
        before
    );
    let mut snapshot = project.snapshot();
    snapshot["rules"][0]["source"]["path"] = json!("missing-source");
    project.save(&snapshot);
    assert!(!project.cli(&["preset", "validate"]).status.success());
    assert!(!project.cli(&["config", "explain"]).status.success());
    assert!(!project.cli(&["check", "--json"]).status.success());
    assert!(!project.cli(&["preset", "add", "rust"]).status.success());
    assert_eq!(project.snapshot(), snapshot);
    fs::remove_file(ordain::presets::path(project.0.path())).unwrap();
    let outside = project.0.path().join("outside");
    fs::write(&outside, "do not touch").unwrap();
    std::os::unix::fs::symlink(&outside, ordain::presets::path(project.0.path())).unwrap();
    assert!(!project.cli(&["preset", "add", "rust"]).status.success());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "do not touch");
    fs::remove_file(ordain::presets::path(project.0.path())).unwrap();
    fs::create_dir(ordain::presets::path(project.0.path())).unwrap();
    assert!(
        !project.cli(&["config", "explain"]).status.success(),
        "an unreadable snapshot must not silently disable the selected rules"
    );
}
