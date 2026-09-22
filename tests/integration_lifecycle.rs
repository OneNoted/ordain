use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Home {
    directory: TempDir,
    workspace: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        Self {
            directory,
            workspace,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.directory.path().join(relative)
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ordain"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.directory.path())
            .env("CLAUDE_CONFIG_DIR", self.path("custom claude's home"))
            .env("CODEX_HOME", self.path("custom codex"))
            .env("HERMES_HOME", self.path("profile"))
            .env("XDG_CONFIG_HOME", self.path("xdg"))
            .current_dir(&self.workspace)
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn state(&self, host: &str) -> String {
        let status: Value =
            serde_json::from_str(&self.ok(&["integration", "status", host, "--json"])).unwrap();
        assert_eq!(status[0]["runtime"], "not_verified");
        status[0]["state"].as_str().unwrap().into()
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn hook_lifecycle_preserves_other_owners_and_repairs_drift_without_credentials() {
    for (host, relative) in [
        ("claude", "custom claude's home/settings.json"),
        ("codex", "custom codex/hooks.json"),
    ] {
        let home = Home::new();
        let path = home.path(relative);
        let unrelated = json!({"model":"keep", "hooks":{"Stop":[
            {"matcher":"unrelated", "hooks":[{"type":"command","command":"echo ORDAIN_MANAGED_HOOK"}]},
            {"hooks":[]}
        ]}});
        write(&path, &unrelated.to_string());
        let codex = home.path("custom codex/config.toml");
        if host == "codex" {
            write(
                &codex,
                "# keep comment\nmodel = 'keep'\n[features]\ncodex_hooks = false\nother = true\n[profiles.other.features]\nhooks = false\n",
            );
        }
        assert_eq!(home.state(host), "missing");
        home.ok(&["integration", "install", host]);
        assert_eq!(home.state(host), "current");
        let first = fs::read(&path).unwrap();
        home.ok(&["integration", "install", host]);
        assert_eq!(fs::read(&path).unwrap(), first);
        let mut drift: Value = serde_json::from_slice(&first).unwrap();
        drift["hooks"]["Stop"].as_array_mut().unwrap().pop();
        fs::write(&path, drift.to_string()).unwrap();
        assert_eq!(home.state(host), "needs_repair");
        home.ok(&["integration", "install", host]);
        assert_eq!(home.state(host), "current");
        if host == "codex" {
            let text = fs::read_to_string(&codex).unwrap();
            fs::write(&codex, text.replace("hooks = true", "hooks = false")).unwrap();
            assert_eq!(home.state(host), "needs_repair");
            home.ok(&["integration", "install", host]);
            assert_eq!(home.state(host), "current");
        }
        home.ok(&["integration", "uninstall", host]);
        assert_eq!(home.state(host), "missing");
        // The installer may retain an empty hooks object, but never alters other groups.
        let remaining: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(remaining, unrelated);
        let before = fs::read(&path).unwrap();
        home.ok(&["integration", "uninstall", host]);
        assert_eq!(fs::read(&path).unwrap(), before);
        if host == "codex" {
            let text = fs::read_to_string(&codex).unwrap();
            assert!(text.contains("# keep comment"));
            let config: toml::Value = toml::from_str(&text).unwrap();
            assert_eq!(config["features"]["hooks"].as_bool(), Some(true));
            assert_eq!(config["features"]["other"].as_bool(), Some(true));
            assert_eq!(
                config["profiles"]["other"]["features"]["hooks"].as_bool(),
                Some(false)
            );
        }
        assert!(!home.path(".claude").exists());
        assert!(!home.path(".codex").exists());
        assert!(!home.workspace.join(".ordain").exists());
    }
}

#[test]
fn plugin_lifecycle_is_profile_scoped_and_leaves_unrelated_files_and_settings() {
    let home = Home::new();
    let config = home.path("profile/config.yaml");
    write(
        &config,
        "terminal:\n  cwd: /keep\nplugins:\n  enabled: [other]\n  disabled: [ordain, something]\n  entries:\n    other:\n      settings: {keep: true}\n",
    );
    assert_eq!(home.state("hermes"), "missing");
    let args = [
        "integration",
        "install",
        "hermes",
        "--workspace",
        home.workspace.to_str().unwrap(),
    ];
    home.ok(&args);
    assert_eq!(home.state("hermes"), "current");
    let first = fs::read(&config).unwrap();
    home.ok(&args);
    assert_eq!(fs::read(&config).unwrap(), first);
    let plugin = home.path("profile/plugins/ordain");
    write(&plugin.join("user-note"), "keep");
    fs::remove_file(plugin.join("__init__.py")).unwrap();
    assert_eq!(home.state("hermes"), "needs_repair");
    home.ok(&args);
    assert_eq!(home.state("hermes"), "current");
    home.ok(&["integration", "uninstall", "hermes"]);
    assert_eq!(home.state("hermes"), "missing");
    home.ok(&["integration", "uninstall", "hermes"]);
    assert!(!plugin.join("plugin.yaml").exists());
    assert_eq!(
        fs::read_to_string(plugin.join("user-note")).unwrap(),
        "keep"
    );
    let after: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&fs::read_to_string(config).unwrap()).unwrap();
    assert_eq!(after["terminal"]["cwd"].as_str(), Some("/keep"));
    assert_eq!(after["plugins"]["enabled"][0].as_str(), Some("other"));
    assert_eq!(
        after["plugins"]["entries"]["other"]["settings"]["keep"].as_bool(),
        Some(true)
    );
    assert_eq!(
        after["plugins"]["entries"]["ordain"]["allow_tool_override"].as_bool(),
        Some(false)
    );
    assert!(!home.path(".hermes").exists());

    home.ok(&["integration", "install", "opencode"]);
    assert_eq!(home.state("opencode"), "current");
    let plugin = home.path("xdg/opencode/plugins/ordain.js");
    let first = fs::read(&plugin).unwrap();
    home.ok(&["integration", "install", "opencode"]);
    assert_eq!(fs::read(&plugin).unwrap(), first);
    fs::write(&plugin, "// ordain-opencode-plugin: old\n").unwrap();
    assert_eq!(home.state("opencode"), "needs_repair");
    home.ok(&["integration", "install", "opencode"]);
    home.ok(&["integration", "uninstall", "opencode"]);
    assert_eq!(home.state("opencode"), "missing");
    home.ok(&["integration", "uninstall", "opencode"]);
}

#[test]
fn invalid_configs_and_unowned_plugins_are_not_overwritten() {
    let home = Home::new();
    let codex = home.path("custom codex/config.toml");
    write(&codex, "features = 'not a table'\n");
    assert!(
        !home
            .cli(&["integration", "install", "codex"])
            .status
            .success()
    );
    assert!(!home.path("custom codex/hooks.json").exists());
    assert_eq!(
        fs::read_to_string(&codex).unwrap(),
        "features = 'not a table'\n"
    );
    let plugin = home.path("profile/plugins/ordain/__init__.py");
    write(&plugin, "# belongs to someone else\n");
    let args = [
        "integration",
        "install",
        "hermes",
        "--workspace",
        home.workspace.to_str().unwrap(),
    ];
    assert!(!home.cli(&args).status.success());
    assert_eq!(home.state("hermes"), "conflict");
    assert!(
        !home
            .cli(&["integration", "uninstall", "hermes"])
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(plugin).unwrap(),
        "# belongs to someone else\n"
    );
    assert!(!home.path("profile/config.yaml").exists());
    assert!(
        !home
            .cli(&["integration", "install", "hermes", "--project"])
            .status
            .success()
    );
    assert!(
        !home
            .cli(&["integration", "install", "hermes"])
            .status
            .success()
    );
}

#[test]
fn project_hooks_do_not_mutate_global_configuration() {
    let home = Home::new();
    for host in ["claude", "codex", "opencode"] {
        home.ok(&["integration", "install", host, "--project"]);
        assert_eq!(home.state(host), "missing");
        let status: Value =
            serde_json::from_str(&home.ok(&["integration", "status", host, "--project", "--json"]))
                .unwrap();
        assert_eq!(status[0]["state"], "current");
        home.ok(&["integration", "uninstall", host, "--project"]);
    }
    assert!(!home.path("custom codex").exists());
    assert!(!home.path("xdg").exists());
}
