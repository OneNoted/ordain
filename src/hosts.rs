use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::error::{ErrorCode, OrdainError, Result};
use crate::paths::home_dir;
use crate::process::{ProcessOptions, run};

use crate::config_file;
mod hermes;
use config_file::ConfigFile;

pub const INTEGRATION_VERSION: u32 = 1;

pub const HOOK_MARKER: &str = "ORDAIN_MANAGED_HOOK";
pub const OPENCODE_MARKER: &str = "ordain-opencode-plugin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Claude,
    Codex,
    Opencode,
    Hermes,
}

impl Host {
    pub const ALL: [Self; 4] = [Self::Claude, Self::Codex, Self::Opencode, Self::Hermes];

    pub fn parse(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::Opencode),
            "hermes" => Ok(Self::Hermes),
            _ => Err(OrdainError::new(
                ErrorCode::HostUnknown,
                format!("{name:?} is not one of claude, codex, opencode, hermes"),
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Hermes => "hermes",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Opencode => "OpenCode",
            Self::Hermes => "Hermes",
        }
    }
}

pub fn host_dir(host: Host) -> PathBuf {
    let (variable, fallback) = match host {
        Host::Claude => ("CLAUDE_CONFIG_DIR", home_dir().join(".claude")),
        Host::Codex => ("CODEX_HOME", home_dir().join(".codex")),
        Host::Hermes => ("HERMES_HOME", home_dir().join(".hermes")),
        Host::Opencode => {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home_dir().join(".config"));
            ("OPENCODE_CONFIG_DIR", config.join("opencode"))
        }
    };
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}

pub fn install_target(host: Host, root: &Path, project: bool) -> PathBuf {
    match (host, project) {
        (Host::Claude, true) => root.join(".claude/settings.json"),
        (Host::Claude, false) => host_dir(host).join("settings.json"),
        (Host::Codex, true) => root.join(".codex/hooks.json"),
        (Host::Codex, false) => host_dir(host).join("hooks.json"),
        (Host::Opencode, true) => root.join(".opencode/plugins/ordain.js"),
        (Host::Opencode, false) => host_dir(host).join("plugins/ordain.js"),
        (Host::Hermes, _) => host_dir(host).join("plugins/ordain"),
    }
}

fn on_path(binary: &str) -> bool {
    run(
        "which",
        &[binary.into()],
        ProcessOptions {
            cwd: None,
            timeout: Duration::from_secs(2),
            input: None,
            env: None,
            max_output: 1024,
            inherit_output: false,
        },
    )
    .status
        == Some(0)
}

pub fn host_present(host: Host) -> bool {
    host_dir(host).is_dir() || on_path(host.name())
}

pub fn detect_hosts() -> Vec<Host> {
    Host::ALL
        .into_iter()
        .filter(|host| host_present(*host))
        .collect()
}

fn parse_settings(file: &ConfigFile, path: &Path) -> Result<Value> {
    let raw = file.text();
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| invalid_settings(path))?;
    validate_settings(&value)
        .then_some(value)
        .ok_or_else(|| invalid_settings(path))
}

fn validate_settings(value: &Value) -> bool {
    let Some(root) = value.as_object() else {
        return false;
    };
    let Some(hooks) = root.get("hooks") else {
        return true;
    };
    let Some(hooks) = hooks.as_object() else {
        return false;
    };
    hooks.values().all(|groups| {
        groups.as_array().is_some_and(|groups| {
            groups.iter().all(|group| {
                group
                    .as_object()
                    .and_then(|group| group.get("hooks"))
                    .and_then(Value::as_array)
                    .is_some_and(|entries| entries.iter().all(Value::is_object))
            })
        })
    })
}

fn invalid_settings(path: &Path) -> OrdainError {
    OrdainError::new(
        ErrorCode::SettingsInvalid,
        format!("{} is not a settings file Ordain can edit", path.display()),
    )
}

fn is_ours(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .and_then(shlex::split)
        .is_some_and(|words| {
            let mut words = words.iter().map(String::as_str);
            if words.next() != Some("env") || words.next() != Some("ORDAIN_MANAGED_HOOK=1") {
                return false;
            }
            let Some(mut binary) = words.next() else {
                return false;
            };
            if binary.starts_with("ORDAIN_INTEGRATION_VERSION=") {
                let Some(next) = words.next() else {
                    return false;
                };
                binary = next;
            }
            !binary.is_empty()
                && words.next() == Some("__hook")
                && words.next().is_some()
                && words.next().is_none()
        })
}

fn without_ours(groups: &[Value]) -> (Vec<Value>, usize) {
    let mut kept = Vec::new();
    let mut removed = 0;
    for group in groups {
        let Some(mut object) = group.as_object().cloned() else {
            continue;
        };
        let entries = object
            .get("hooks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let retained = entries
            .iter()
            .filter(|entry| !is_ours(entry))
            .cloned()
            .collect::<Vec<_>>();
        removed += entries.len() - retained.len();
        if !retained.is_empty() || entries.is_empty() {
            object.insert("hooks".into(), Value::Array(retained));
            kept.push(Value::Object(object));
        }
    }
    (kept, removed)
}

struct HookSpec {
    event: &'static str,
    subcommand: &'static str,
    matcher: Option<&'static str>,
    timeout: u64,
}

const SPECS: &[HookSpec] = &[
    HookSpec {
        event: "SessionStart",
        subcommand: "session-start",
        matcher: None,
        timeout: 10,
    },
    HookSpec {
        event: "UserPromptSubmit",
        subcommand: "turn-start",
        matcher: None,
        timeout: 10,
    },
    HookSpec {
        event: "PostToolUse",
        subcommand: "post-tool-use",
        matcher: Some("Edit|Write|MultiEdit|apply_patch"),
        timeout: 20,
    },
    HookSpec {
        event: "Stop",
        subcommand: "stop",
        matcher: None,
        timeout: 30,
    },
];

pub fn install_hooks(path: &Path, binary: &Path) -> Result<()> {
    let original = ConfigFile::read(path)?;
    let mut settings = parse_settings(&original, path)?;
    merge_hooks(&mut settings, binary);
    original.write(&json_text(&settings)?)
}

fn merge_hooks(settings: &mut Value, binary: &Path) {
    let root = settings.as_object_mut().expect("validated settings object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("validated hooks object");
    strip_hooks(hooks);
    for spec in SPECS {
        let mut kept = hooks
            .get(spec.event)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let command = hook_command(binary, spec.subcommand);
        let mut group = Map::new();
        if let Some(matcher) = spec.matcher {
            group.insert("matcher".into(), Value::String(matcher.into()));
        }
        group.insert(
            "hooks".into(),
            json!([{"type":"command", "command":command, "timeout":spec.timeout}]),
        );
        kept.push(Value::Object(group));
        hooks.insert(spec.event.into(), Value::Array(kept));
    }
}

fn hook_command(binary: &Path, subcommand: &str) -> String {
    format!(
        "env {HOOK_MARKER}=1 ORDAIN_INTEGRATION_VERSION={INTEGRATION_VERSION} {} __hook {subcommand}",
        shell_quote(binary.to_string_lossy().as_ref())
    )
}

pub fn uninstall_hooks(path: &Path) -> Result<usize> {
    let original = ConfigFile::read(path)?;
    let mut settings = parse_settings(&original, path)?;
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(0);
    };
    let removed = strip_hooks(hooks);
    if removed > 0 {
        original.write(&json_text(&settings)?)?;
    }
    Ok(removed)
}

fn strip_hooks(hooks: &mut Map<String, Value>) -> usize {
    let mut removed = 0;
    let names = hooks.keys().cloned().collect::<Vec<_>>();
    for event in names {
        let groups = hooks[&event].as_array().expect("validated hook groups");
        let (kept, count) = without_ours(groups);
        removed += count;
        if count == 0 {
            continue;
        }
        if kept.is_empty() {
            hooks.remove(&event);
        } else {
            hooks.insert(event, Value::Array(kept));
        }
    }
    removed
}

fn json_text(value: &Value) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|error| OrdainError::new(ErrorCode::SettingsInvalid, error.to_string()))?;
    text.push('\n');
    Ok(text)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug)]
pub struct Installed {
    pub host: Host,
    pub target: PathBuf,
    pub what: String,
    pub afterwards: Option<String>,
}

pub fn install_host(host: Host, root: &Path, project: bool, binary: &Path) -> Result<Installed> {
    validate_scope(host, project)?;
    let target = install_target(host, root, project);
    let afterwards = match host {
        Host::Claude | Host::Codex => {
            // Parse both files before changing either. A later I/O error is reported,
            // not a claim of transactional installation across two files.
            let codex = if host == Host::Codex {
                Some(codex_config(&target.with_file_name("config.toml"))?)
            } else {
                None
            };
            install_hooks(&target, binary)?;
            if let Some((file, config)) = codex {
                file.write(&config.to_string())?;
            }
            if host == Host::Codex {
                "Start Codex, open /hooks and approve Ordain's entries. Runtime loading has not been verified."
            } else {
                "Start a fresh Claude session. Runtime loading has not been verified."
            }
        }
        Host::Opencode => {
            install_opencode(&target, binary)?;
            "Start a fresh OpenCode process. Runtime loading has not been verified."
        }
        Host::Hermes => {
            hermes::install(&host_dir(host), root, binary)?;
            "Start a fresh Hermes process; a running gateway needs a controlled restart. Runtime loading has not been verified."
        }
    };
    Ok(Installed {
        host,
        target,
        what: "integration configured".into(),
        afterwards: Some(afterwards.into()),
    })
}

pub fn uninstall_host(host: Host, root: &Path, project: bool) -> Result<usize> {
    validate_scope(host, project)?;
    let target = install_target(host, root, project);
    match host {
        Host::Claude | Host::Codex => uninstall_hooks(&target),
        Host::Opencode => uninstall_opencode(&target).map(usize::from),
        Host::Hermes => hermes::uninstall(&host_dir(host)),
    }
}

pub fn validate_scope(host: Host, project: bool) -> Result<()> {
    if host == Host::Hermes && project {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "Hermes uses a profile-wide plugin: select HERMES_HOME, and use --workspace for its repository; --project is unsupported",
        ));
    }
    Ok(())
}

fn codex_config(path: &Path) -> Result<(ConfigFile, toml_edit::DocumentMut)> {
    let file = ConfigFile::read(path)?;
    let mut config = file
        .text()
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid_settings(path))?;
    if !config.contains_key("features") {
        config["features"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let features = config["features"]
        .as_table_like_mut()
        .ok_or_else(|| invalid_settings(path))?;
    features.insert("hooks", toml_edit::value(true));
    features.remove("codex_hooks");
    Ok((file, config))
}

fn install_opencode(target: &Path, binary: &Path) -> Result<()> {
    let original = ConfigFile::read(target)?;
    if original.exists() && !original.text().starts_with("// ordain-opencode-plugin:") {
        return Err(invalid_settings(target));
    }
    original.write(&opencode_source(binary)?)
}

fn opencode_source(binary: &Path) -> Result<String> {
    let binary = serde_json::to_string(&binary.to_string_lossy())
        .map_err(|error| OrdainError::new(ErrorCode::SettingsInvalid, error.to_string()))?;
    Ok(include_str!("../assets/opencode-plugin.mjs").replace("__ORDAIN_BINARY__", &binary))
}

fn uninstall_opencode(target: &Path) -> Result<bool> {
    let file = ConfigFile::read(target)?;
    if !file.exists() || !file.text().starts_with("// ordain-opencode-plugin:") {
        return Ok(false);
    }
    fs::remove_file(target)?;
    Ok(true)
}

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    Missing,
    Current,
    NeedsRepair,
    Conflict,
}

#[derive(Debug, serde::Serialize)]
pub struct IntegrationStatus {
    pub host: &'static str,
    pub target: PathBuf,
    pub state: IntegrationState,
    pub bundled_version: u32,
    pub runtime: &'static str,
}

pub fn integration_status(
    host: Host,
    root: &Path,
    project: bool,
    binary: &Path,
) -> Result<IntegrationStatus> {
    validate_scope(host, project)?;
    let target = install_target(host, root, project);
    let state = match host {
        Host::Hermes => hermes::status(&host_dir(host), binary)?,
        Host::Opencode => {
            let file = ConfigFile::read(&target)?;
            if !file.exists() {
                IntegrationState::Missing
            } else if !file.text().starts_with("// ordain-opencode-plugin:") {
                IntegrationState::Conflict
            } else if file.text() == opencode_source(binary)? {
                IntegrationState::Current
            } else {
                IntegrationState::NeedsRepair
            }
        }
        Host::Claude | Host::Codex => {
            let file = ConfigFile::read(&target)?;
            let settings = parse_settings(&file, &target)?;
            let count = settings
                .get("hooks")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|events| events.values())
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter(|entry| is_ours(entry))
                .count();
            if count == 0 {
                IntegrationState::Missing
            } else {
                let mut expected = settings.clone();
                merge_hooks(&mut expected, binary);
                let enabled = if host == Host::Codex {
                    let path = target.with_file_name("config.toml");
                    let file = ConfigFile::read(&path)?;
                    let config = file
                        .text()
                        .parse::<toml_edit::DocumentMut>()
                        .map_err(|_| invalid_settings(&path))?;
                    config
                        .get("features")
                        .and_then(|f| f.get("hooks"))
                        .and_then(toml_edit::Item::as_bool)
                        == Some(true)
                } else {
                    true
                };
                if expected == settings && enabled {
                    IntegrationState::Current
                } else {
                    IntegrationState::NeedsRepair
                }
            }
        }
    };
    Ok(IntegrationStatus {
        host: host.name(),
        target,
        state,
        bundled_version: INTEGRATION_VERSION,
        runtime: "not_verified",
    })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn install_is_idempotent_and_preserves_other_hooks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"model":"x","hooks":{"Stop":[{"hooks":[{"type":"command","command":"theirs"}]}]}}"#,
        )
        .unwrap();
        install_hooks(&path, Path::new("/bin/ordain")).unwrap();
        install_hooks(&path, Path::new("/bin/ordain")).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["model"], "x");
        assert_eq!(value["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(uninstall_hooks(&path).unwrap(), 4);
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["hooks"]["Stop"][0]["hooks"][0]["command"], "theirs");
    }

    #[test]
    fn malformed_settings_are_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{ nope").unwrap();
        assert_eq!(
            install_hooks(&path, Path::new("/bin/ordain"))
                .unwrap_err()
                .code,
            ErrorCode::SettingsInvalid
        );
    }

    #[test]
    fn opencode_install_does_not_overwrite_an_unowned_plugin() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ordain.js");
        fs::write(&path, "export default 'theirs';\n").unwrap();
        assert_eq!(
            install_opencode(&path, Path::new("/bin/ordain"))
                .unwrap_err()
                .code,
            ErrorCode::SettingsInvalid
        );
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "export default 'theirs';\n"
        );
    }
}
