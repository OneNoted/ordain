//! Explicit, project-owned snapshots of curated rules. No inference or host changes.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::config_file::ConfigFile;
use crate::error::{ErrorCode, OrdainError, Result};
use crate::model::{Origin, Rubric, validate_rubric};

const BUNDLES: [(&str, &str); 3] = [
    ("core", include_str!("../assets/presets/core.json")),
    ("rust", include_str!("../assets/presets/rust.json")),
    (
        "typescript",
        include_str!("../assets/presets/typescript.json"),
    ),
];

pub fn path(root: &Path) -> PathBuf {
    crate::paths::ordain_dir(root).join("presets.json")
}

pub fn bundled(name: &str) -> Result<Rubric> {
    let text = BUNDLES
        .iter()
        .find(|(key, _)| *key == name)
        .ok_or_else(|| {
            invalid(format!(
                "unknown preset {name:?}; choose core, rust or typescript"
            ))
        })?
        .1;
    let rubric: Rubric = serde_json::from_str(text)
        .map_err(|error| invalid(format!("invalid bundled preset: {error}")))?;
    validate(&rubric)?;
    Ok(rubric)
}

/// An absent snapshot means opt-out. Invalid selected data must not look like opt-out.
pub fn load(root: &Path) -> Result<Option<Rubric>> {
    parse(&read(root)?)
}

fn parse(file: &ConfigFile) -> Result<Option<Rubric>> {
    if !file.exists() {
        return Ok(None);
    }
    let mut rubric: Rubric = serde_json::from_str(file.text())
        .map_err(|error| invalid(format!("invalid preset snapshot: {error}")))?;
    validate(&rubric)?;
    for rule in &mut rubric.rules {
        rule.origin = Some(Origin::Preset);
    }
    Ok(Some(rubric))
}

pub fn validate(rubric: &Rubric) -> Result<()> {
    let mut issues = validate_rubric(rubric);
    let mut names = HashSet::new();
    for source in &rubric.sources {
        match source
            .path
            .strip_prefix("preset:")
            .and_then(|s| s.split_once('@'))
        {
            Some((name, revision))
                if BUNDLES.iter().any(|(key, _)| *key == name)
                    && revision.parse::<u32>().is_ok_and(|r| r > 0)
                    && names.insert(name) => {}
            _ => issues.push(format!(
                "invalid or duplicate preset source: {}",
                source.path
            )),
        }
    }
    for rule in &rubric.rules {
        let source = rubric.sources.iter().find(|s| s.path == rule.source.path);
        let name = source
            .and_then(|s| s.path.strip_prefix("preset:"))
            .and_then(|s| s.split_once('@'))
            .map(|(n, _)| n);
        if !name.is_some_and(|name| rule.id.starts_with(&format!("preset-{name}-"))) {
            issues.push(format!(
                "{} must belong to its listed preset source and namespace",
                rule.id
            ));
        }
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(invalid(issues.join("; ")))
    }
}

pub(crate) fn read(root: &Path) -> Result<ConfigFile> {
    if std::fs::symlink_metadata(crate::paths::ordain_dir(root))
        .is_ok_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(invalid("unsafe .ordain directory"));
    }
    ConfigFile::read(&path(root))
}

pub(crate) fn write(file: &ConfigFile, rubric: &Rubric) -> Result<()> {
    validate(rubric)?;
    file.write(&(serde_json::to_string_pretty(rubric).expect("validated preset serializes") + "\n"))
}

/// `update` is a preview unless `apply` is explicitly selected. Adding twice is a no-op.
pub fn run(action: &str, name: Option<&str>, apply: bool) -> Result<i32> {
    let root = crate::paths::find_repo_root(&std::env::current_dir()?);
    let file = read(&root)?;
    let installed = parse(&file)?;
    if action == "list" {
        let entries: Vec<_> = BUNDLES.iter().map(|(name, _)| {
            let bundled = bundled(name).expect("bundled preset validated by tests");
            let selected = installed.as_ref().and_then(|r| r.sources.iter().find(|s| pack_name(&s.path) == *name));
            json!({"name":name,"bundled":bundled.sources[0].path,"rules":bundled.rules.len(),"selected":selected.map(|s| &s.path)})
        }).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).expect("validated preset serializes")
        );
        return Ok(0);
    }
    if action == "validate" {
        println!(
            "{}",
            json!({"path":path(&root),"selected":installed.is_some(),"rules":installed.as_ref().map_or(0, |r|r.rules.len())})
        );
        return Ok(0);
    }
    let name = name.ok_or_else(|| invalid("preset name required"))?;
    let bundle = bundled(name)?;
    if action == "show" {
        println!(
            "{}",
            serde_json::to_string_pretty(&bundle).expect("validated preset serializes")
        );
        return Ok(0);
    }
    let mut snapshot = installed.unwrap_or_else(|| Rubric {
        version: crate::model::RUBRIC_VERSION,
        compiled_at: chrono::Utc::now().to_rfc3339(),
        compiled_by: Some("ordain preset".into()),
        sources: Vec::new(),
        thresholds: None,
        rules: Vec::new(),
    });
    let selected = snapshot.sources.iter().any(|s| pack_name(&s.path) == name);
    match action {
        "add" if selected => {
            println!(
                "{name} already selected; use update to preview replacement. Local edits retained."
            );
            return Ok(0);
        }
        "update" if !selected => return Err(invalid(format!("{name} is not selected; use add"))),
        "update" if !apply => {
            let before: Vec<_> = snapshot
                .rules
                .iter()
                .filter(|r| pack_name(&r.source.path) == name)
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"preset":name,"before":before,"after":bundle.rules,"revision":bundle.sources,"applied":false})
                ).expect("validated preset serializes")
            );
            return Ok(0);
        }
        "add" | "update" | "remove" => {}
        _ => return Err(invalid("unknown preset action")),
    }
    snapshot.rules.retain(|r| pack_name(&r.source.path) != name);
    snapshot.sources.retain(|s| pack_name(&s.path) != name);
    if action != "remove" {
        snapshot.sources.extend(bundle.sources);
        snapshot.rules.extend(bundle.rules);
    }
    // Do not strand policy overrides, including on a future update that removes IDs.
    let loaded = crate::rubric::load_rules(&root);
    if !loaded.problems.is_empty() {
        return Err(invalid(format!(
            "cannot change presets with invalid rules: {:?}",
            loaded.problems
        )));
    }
    let mut effective = loaded.rules;
    effective.retain(|r| r.origin != Some(Origin::Preset));
    let explicit: HashSet<_> = effective.iter().map(|r| r.id.clone()).collect();
    effective.extend(
        snapshot
            .rules
            .iter()
            .filter(|r| !explicit.contains(&r.id))
            .cloned(),
    );
    crate::config::ProjectConfig::load(&root)?
        .resolve(&effective, snapshot.thresholds.unwrap_or_default())?;
    snapshot.compiled_at = chrono::Utc::now().to_rfc3339();
    write(&file, &snapshot)?;
    let readback =
        load(&root)?.ok_or_else(|| invalid("preset snapshot disappeared after write"))?;
    if serde_json::to_value(&readback).expect("validated preset serializes")
        != serde_json::to_value(&snapshot).expect("validated preset serializes")
    {
        return Err(invalid("preset snapshot changed after write"));
    }
    println!(
        "{}",
        json!({"action":action,"preset":name,"path":path(&root),"rules":snapshot.rules.len(),"defaultDelivery":"notice","applied":true})
    );
    Ok(0)
}

fn pack_name(source: &str) -> &str {
    source
        .strip_prefix("preset:")
        .and_then(|s| s.split_once('@'))
        .map_or("", |(name, _)| name)
}
fn invalid(message: impl Into<String>) -> OrdainError {
    OrdainError::new(ErrorCode::RubricInvalid, message)
}
