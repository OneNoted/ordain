//! Bundled Hermes plugin and profile-scoped native configuration.
use std::fs;
use std::path::Path;

use serde_yaml_ng::{Mapping, Value};

use super::{ConfigFile, INTEGRATION_VERSION, IntegrationState, invalid_settings};
use crate::error::{ErrorCode, OrdainError, Result};

const MARKER: &str = "# ordain-managed-hermes:";
const ASSETS: &[(&str, &str)] = &[
    (
        "plugin.yaml",
        include_str!("../../integrations/hermes/plugin.yaml"),
    ),
    (
        "__init__.py",
        include_str!("../../integrations/hermes/__init__.py"),
    ),
];

pub(super) fn install(home: &Path, workspace: &Path, binary: &Path) -> Result<()> {
    let workspace = fs::canonicalize(workspace)?;
    if !workspace.is_dir() {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "Hermes workspace must be a directory",
        ));
    }
    let directory = home.join("plugins/ordain");
    let files = owned_files(&directory)?;
    let path = home.join("config.yaml");
    let file = ConfigFile::read(&path)?;
    let mut config = parse(&file, &path)?;
    let plugins = object(&mut config, "plugins", &path)?;
    set_membership(plugins, "enabled", true, &path)?;
    set_membership(plugins, "disabled", false, &path)?;
    let entries = object(plugins, "entries", &path)?;
    let entry = object(entries, "ordain", &path)?;
    entry.insert("allow_tool_override".into(), Value::Bool(false));
    let settings = object(entry, "settings", &path)?;
    settings.insert(
        "project".into(),
        workspace.to_string_lossy().into_owned().into(),
    );
    settings.insert(
        "binary".into(),
        binary.to_string_lossy().into_owned().into(),
    );
    let rendered = render(&config, &path)?;
    // All configuration and ownership checks precede writes; enable only after
    // both plugin files exist. Failed I/O is reported and reinstall repairs it.
    for ((_, source), file) in ASSETS.iter().zip(files) {
        file.write(&asset(source))?;
    }
    file.write(&rendered)
}

pub(super) fn uninstall(home: &Path) -> Result<usize> {
    let directory = home.join("plugins/ordain");
    let files = owned_files(&directory)?;
    let path = home.join("config.yaml");
    let file = ConfigFile::read(&path)?;
    let mut config = parse(&file, &path)?;
    let mut changed = false;
    if let Some(plugins) = config.get_mut("plugins") {
        let plugins = plugins
            .as_mapping_mut()
            .ok_or_else(|| invalid_settings(&path))?;
        changed |= set_membership(plugins, "enabled", false, &path)?;
        changed |= set_membership(plugins, "disabled", false, &path)?;
    }
    if changed {
        file.write(&render(&config, &path)?)?;
    }
    let mut removed = usize::from(changed);
    for ((name, _), file) in ASSETS.iter().zip(files) {
        if file.exists() {
            fs::remove_file(directory.join(name))?;
            removed += 1;
        }
    }
    // Never recursively delete user additions or follow plugin-directory links.
    match fs::remove_dir(&directory) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(removed)
}

pub(super) fn status(home: &Path, binary: &Path) -> Result<IntegrationState> {
    let directory = home.join("plugins/ordain");
    let files = match owned_files(&directory) {
        Ok(files) => files,
        Err(error) if error.code == ErrorCode::SettingsInvalid => {
            return Ok(IntegrationState::Conflict);
        }
        Err(error) => return Err(error),
    };
    let path = home.join("config.yaml");
    let file = ConfigFile::read(&path)?;
    let config = parse(&file, &path)?;
    let plugins = config.get("plugins");
    let enabled = membership(plugins, "enabled", &path)?;
    let disabled = membership(plugins, "disabled", &path)?;
    if files.iter().all(|file| !file.exists()) && !enabled {
        return Ok(IntegrationState::Missing);
    }
    let current = ASSETS
        .iter()
        .zip(&files)
        .all(|((_, source), file)| file.text() == asset(source));
    let settings = plugins
        .and_then(|p| p.get("entries"))
        .and_then(|p| p.get("ordain"))
        .and_then(|p| p.get("settings"));
    let configured_binary = settings
        .and_then(|s| s.get("binary"))
        .and_then(Value::as_str);
    let workspace = settings
        .and_then(|s| s.get("project"))
        .and_then(Value::as_str);
    let ready = enabled
        && !disabled
        && configured_binary == binary.to_str()
        && workspace.is_some_and(|path| Path::new(path).is_absolute() && Path::new(path).is_dir());
    Ok(if current && ready {
        IntegrationState::Current
    } else {
        IntegrationState::NeedsRepair
    })
}

fn owned_files(directory: &Path) -> Result<Vec<ConfigFile>> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if !metadata.is_dir() => return Err(invalid_settings(directory)),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    ASSETS
        .iter()
        .map(|(name, source)| {
            let path = directory.join(name);
            let file = ConfigFile::read(&path)?;
            // An exact bundled file permits adoption of a manual installation.
            if file.exists() && file.text() != *source && !file.text().starts_with(MARKER) {
                return Err(invalid_settings(&path));
            }
            Ok(file)
        })
        .collect()
}

fn asset(source: &str) -> String {
    format!("{MARKER} {INTEGRATION_VERSION}\n{source}")
}

fn parse(file: &ConfigFile, path: &Path) -> Result<Mapping> {
    if file.text().trim().is_empty() {
        return Ok(Mapping::new());
    }
    serde_yaml_ng::from_str::<Mapping>(file.text()).map_err(|_| invalid_settings(path))
}

fn render(config: &Mapping, path: &Path) -> Result<String> {
    serde_yaml_ng::to_string(config).map_err(|_| invalid_settings(path))
}

fn object<'a>(mapping: &'a mut Mapping, key: &str, path: &Path) -> Result<&'a mut Mapping> {
    mapping
        .entry(Value::from(key))
        .or_insert_with(|| Value::Mapping(Mapping::new()))
        .as_mapping_mut()
        .ok_or_else(|| invalid_settings(path))
}

fn set_membership(plugins: &mut Mapping, key: &str, present: bool, path: &Path) -> Result<bool> {
    if !plugins.contains_key(key) && !present {
        return Ok(false);
    }
    let values = plugins
        .entry(Value::from(key))
        .or_insert_with(|| Value::Sequence(Vec::new()))
        .as_sequence_mut()
        .ok_or_else(|| invalid_settings(path))?;
    if values.iter().any(|v| !v.is_string()) {
        return Err(invalid_settings(path));
    }
    let before = values.clone();
    values.retain(|value| value.as_str() != Some("ordain"));
    if present {
        values.push("ordain".into());
    }
    Ok(*values != before)
}

fn membership(plugins: Option<&Value>, key: &str, path: &Path) -> Result<bool> {
    let Some(plugins) = plugins else {
        return Ok(false);
    };
    let plugins = plugins.as_mapping().ok_or_else(|| invalid_settings(path))?;
    let Some(values) = plugins.get(key) else {
        return Ok(false);
    };
    let values = values.as_sequence().ok_or_else(|| invalid_settings(path))?;
    if values.iter().any(|v| !v.is_string()) {
        return Err(invalid_settings(path));
    }
    Ok(values.iter().any(|v| v.as_str() == Some("ordain")))
}
