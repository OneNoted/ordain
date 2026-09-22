use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

use crate::error::{ErrorCode, OrdainError, Result};
use crate::model::{Origin, Rubric, Rule, Thresholds, validate_rubric};
use crate::paths::{
    MAX_FILE_READ_BYTES, canonical_source_path, global_rubric_path, home_dir, read_regular,
    read_regular_text, resolve_source_path, rubric_path, to_source_path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceOrigin {
    Root,
    Nested,
    Global,
    Contributing,
    Recorded,
}

#[derive(Debug, Clone)]
pub struct SourceCandidate {
    pub path: String,
    pub absolute: PathBuf,
    pub scope: String,
    pub required: bool,
    pub origin: SourceOrigin,
}

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "out",
    ".next",
    "vendor",
    "coverage",
    ".turbo",
    ".cache",
    "target",
    ".ordain",
    ".claude",
    ".codex",
    ".opencode",
];

pub fn discover_project_sources(root: &Path) -> Vec<SourceCandidate> {
    let mut found = Vec::new();
    for name in ["AGENTS.md", "CLAUDE.md", ".cursorrules"] {
        let file = root.join(name);
        if file.is_file() {
            found.push(SourceCandidate {
                path: name.into(),
                absolute: file,
                scope: "**/*".into(),
                required: true,
                origin: SourceOrigin::Root,
            });
        }
    }
    let walker = WalkDir::new(root)
        .min_depth(1)
        .max_depth(6)
        .follow_links(false)
        .into_iter()
        .filter_entry(walkable);
    for entry in walker.flatten() {
        if !entry.file_type().is_file() || entry.depth() < 2 {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !matches!(name.as_ref(), "AGENTS.md" | "CLAUDE.md") {
            continue;
        }
        let directory = entry.path().parent().unwrap_or(root);
        let relative_directory = to_source_path(root, directory);
        found.push(SourceCandidate {
            path: to_source_path(root, entry.path()),
            absolute: entry.path().to_path_buf(),
            scope: format!("{relative_directory}/**/*"),
            required: true,
            origin: SourceOrigin::Nested,
        });
    }
    let contributing = root.join("CONTRIBUTING.md");
    if contributing.is_file() {
        found.push(SourceCandidate {
            path: "CONTRIBUTING.md".into(),
            absolute: contributing,
            scope: "**/*".into(),
            required: false,
            origin: SourceOrigin::Contributing,
        });
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

fn walkable(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref())
}

pub fn discover_global_sources() -> Vec<SourceCandidate> {
    [
        "~/.claude/CLAUDE.md",
        "~/.codex/AGENTS.md",
        "~/.config/opencode/AGENTS.md",
    ]
    .into_iter()
    .filter_map(|path| {
        let absolute = home_dir().join(path.trim_start_matches("~/"));
        absolute.is_file().then(|| SourceCandidate {
            path: path.into(),
            absolute,
            scope: "**/*".into(),
            required: true,
            origin: SourceOrigin::Global,
        })
    })
    .collect()
}

pub fn source_hash(path: &Path) -> Option<String> {
    let bytes = read_regular(path, MAX_FILE_READ_BYTES, true)?;
    Some(hex::encode(Sha256::digest(bytes)))
}

#[derive(Debug, Clone)]
pub enum RubricRead {
    Missing { path: PathBuf },
    Invalid { path: PathBuf, issues: Vec<String> },
    Ok { path: PathBuf, rubric: Rubric },
}

pub fn read_rubric(path: &Path) -> RubricRead {
    if path.parent().is_some_and(|parent| {
        fs::symlink_metadata(parent).is_ok_and(|metadata| !metadata.file_type().is_dir())
    }) || fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return RubricRead::Invalid {
            path: path.to_path_buf(),
            issues: vec!["rubric path must not be a symbolic link".into()],
        };
    }
    let Some(raw) = read_regular_text(path, MAX_FILE_READ_BYTES, false) else {
        return RubricRead::Missing {
            path: path.to_path_buf(),
        };
    };
    let rubric: Rubric = match serde_json::from_str(&raw) {
        Ok(rubric) => rubric,
        Err(error) => {
            return RubricRead::Invalid {
                path: path.to_path_buf(),
                issues: vec![error.to_string()],
            };
        }
    };
    let issues = validate_rubric(&rubric);
    if issues.is_empty() {
        RubricRead::Ok {
            path: path.to_path_buf(),
            rubric,
        }
    } else {
        RubricRead::Invalid {
            path: path.to_path_buf(),
            issues,
        }
    }
}

pub fn write_rubric(path: &Path, rubric: &Rubric) -> Result<()> {
    if path.parent().is_some_and(|parent| {
        fs::symlink_metadata(parent).is_ok_and(|metadata| !metadata.file_type().is_dir())
    }) {
        return Err(OrdainError::new(
            ErrorCode::RubricInvalid,
            format!("refusing unsafe rubric directory for {}", path.display()),
        ));
    }
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.file_type().is_file())
    {
        return Err(OrdainError::new(
            ErrorCode::RubricInvalid,
            format!("refusing to replace non-regular rubric {}", path.display()),
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(rubric)
        .map_err(|error| OrdainError::new(ErrorCode::RubricInvalid, error.to_string()))?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    output.write_all(&bytes)?;
    Ok(())
}

pub fn fill_source_hashes(rubric: &mut Rubric, root: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for source in &mut rubric.sources {
        let absolute = resolve_source_path(root, &source.path);
        let canonical = canonical_source_path(root, &source.path);
        source.path = canonical;
        match source_hash(&absolute) {
            Some(sha) => source.sha = Some(sha),
            None => missing.push(source.path.clone()),
        }
    }
    for rule in &mut rubric.rules {
        rule.source.path = canonical_source_path(root, &rule.source.path);
    }
    missing
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    Missing,
    Fresh,
    Stale {
        changed: Vec<String>,
        added: Vec<String>,
        removed: Vec<String>,
        unhashed: Vec<String>,
    },
}

pub fn check_staleness(
    rubric: Option<&Rubric>,
    candidates: &[SourceCandidate],
    root: &Path,
) -> Staleness {
    let Some(rubric) = rubric else {
        return Staleness::Missing;
    };
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut unhashed = Vec::new();
    for source in &rubric.sources {
        match source_hash(&resolve_source_path(root, &source.path)) {
            None => removed.push(source.path.clone()),
            Some(_) if source.sha.is_none() => unhashed.push(source.path.clone()),
            Some(now) if source.sha.as_deref() != Some(&now) => changed.push(source.path.clone()),
            Some(_) => {}
        }
    }
    let listed: HashSet<PathBuf> = rubric
        .sources
        .iter()
        .map(|source| resolve_source_path(root, &source.path))
        .collect();
    let added = candidates
        .iter()
        .filter(|candidate| candidate.required && !listed.contains(&candidate.absolute))
        .map(|candidate| candidate.path.clone())
        .collect::<Vec<_>>();
    if changed.is_empty() && added.is_empty() && removed.is_empty() && unhashed.is_empty() {
        Staleness::Fresh
    } else {
        Staleness::Stale {
            changed,
            added,
            removed,
            unhashed,
        }
    }
}

pub fn describe_staleness(staleness: &Staleness) -> String {
    match staleness {
        Staleness::Missing => "no rubric yet".into(),
        Staleness::Fresh => "up to date".into(),
        Staleness::Stale {
            changed,
            added,
            removed,
            unhashed,
        } => {
            let mut parts = Vec::new();
            if !changed.is_empty() {
                parts.push(format!("changed: {}", changed.join(", ")));
            }
            if !added.is_empty() {
                parts.push(format!("new: {}", added.join(", ")));
            }
            if !removed.is_empty() {
                parts.push(format!("gone: {}", removed.join(", ")));
            }
            if !unhashed.is_empty() {
                parts.push(format!("never hashed: {}", unhashed.join(", ")));
            }
            parts.join("; ")
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedRules {
    pub project: Option<Rubric>,
    pub global: Option<Rubric>,
    pub rules: Vec<Rule>,
    pub thresholds: Thresholds,
    pub problems: Vec<String>,
}

pub fn load_rules(root: &Path) -> LoadedRules {
    let mut problems = Vec::new();
    let project = pick_rubric(&rubric_path(root), &mut problems);
    let global = pick_rubric(&global_rubric_path(), &mut problems);
    let mut rules = Vec::new();
    let project_ids: HashSet<String> = project
        .as_ref()
        .map(|rubric| rubric.rules.iter().map(|rule| rule.id.clone()).collect())
        .unwrap_or_default();
    if let Some(rubric) = &project {
        rules.extend(rubric.rules.iter().cloned().map(|mut rule| {
            rule.origin = Some(Origin::Project);
            rule
        }));
    }
    if let Some(rubric) = &global {
        rules.extend(
            rubric
                .rules
                .iter()
                .filter(|rule| !project_ids.contains(&rule.id))
                .cloned()
                .map(|mut rule| {
                    rule.origin = Some(Origin::Global);
                    rule
                }),
        );
    }
    let presets = match crate::presets::load(root) {
        Ok(presets) => presets,
        Err(error) => {
            problems.push(error.to_string());
            None
        }
    };
    if let Some(presets) = &presets {
        let explicit: HashSet<_> = rules.iter().map(|rule| rule.id.clone()).collect();
        rules.extend(
            presets
                .rules
                .iter()
                .filter(|rule| !explicit.contains(&rule.id))
                .cloned(),
        );
    }
    let thresholds = project
        .as_ref()
        .and_then(|rubric| rubric.thresholds)
        .or_else(|| global.as_ref().and_then(|rubric| rubric.thresholds))
        .or_else(|| presets.as_ref().and_then(|rubric| rubric.thresholds))
        .unwrap_or_default();
    LoadedRules {
        project,
        global,
        rules,
        thresholds,
        problems,
    }
}

fn pick_rubric(path: &Path, problems: &mut Vec<String>) -> Option<Rubric> {
    match read_rubric(path) {
        RubricRead::Ok { rubric, .. } => Some(rubric),
        RubricRead::Invalid { path, issues } => {
            problems.push(format!(
                "{}: {}",
                path.display(),
                issues.first().map(String::as_str).unwrap_or("invalid")
            ));
            None
        }
        RubricRead::Missing { .. } => None,
    }
}

pub fn bucket_counts(rules: &[Rule]) -> (HashMap<&'static str, usize>, HashMap<String, usize>) {
    let mut by_check = HashMap::from([
        ("lint", 0),
        ("model", 0),
        ("deferred", 0),
        ("unenforceable", 0),
    ]);
    let mut by_status = HashMap::new();
    for rule in rules {
        *by_check.entry(rule.check.kind()).or_default() += 1;
        *by_status
            .entry(format!("{:?}", rule.status).to_lowercase())
            .or_default() += 1;
    }
    (by_check, by_status)
}

pub fn require_valid(path: &Path) -> Result<Rubric> {
    match read_rubric(path) {
        RubricRead::Ok { rubric, .. } => Ok(rubric),
        RubricRead::Missing { path } => Err(OrdainError::new(
            ErrorCode::RubricMissing,
            format!("{} does not exist yet; compile first", path.display()),
        )),
        RubricRead::Invalid { path, issues } => Err(OrdainError::new(
            ErrorCode::RubricInvalid,
            format!(
                "{}: {}",
                path.display(),
                issues.first().map(String::as_str).unwrap_or("invalid")
            ),
        )),
    }
}

pub fn find_lint_configs(root: &Path) -> Vec<String> {
    const NAMES: &[&str] = &[
        "eslint.config.js",
        "eslint.config.mjs",
        "eslint.config.cjs",
        "eslint.config.ts",
        ".eslintrc",
        ".eslintrc.js",
        ".eslintrc.cjs",
        ".eslintrc.json",
        ".eslintrc.yml",
        ".eslintrc.yaml",
        "biome.json",
        "biome.jsonc",
        ".stylelintrc",
        ".stylelintrc.json",
        ".stylelintrc.js",
        ".stylelintrc.cjs",
        ".stylelintrc.yml",
        "stylelint.config.js",
        "stylelint.config.mjs",
        "stylelint.config.cjs",
        ".oxlintrc.json",
    ];
    let mut found = Vec::new();
    for entry in WalkDir::new(root)
        .max_depth(3)
        .follow_links(false)
        .into_iter()
        .filter_entry(walkable)
        .flatten()
    {
        if entry.file_type().is_file()
            && NAMES.contains(&entry.file_name().to_string_lossy().as_ref())
        {
            found.push(to_source_path(root, entry.path()));
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_nested_sources_and_detects_staleness() {
        let repo = tempdir().unwrap();
        fs::write(repo.path().join("AGENTS.md"), "rules\n").unwrap();
        fs::create_dir_all(repo.path().join("apps/web")).unwrap();
        fs::write(repo.path().join("apps/web/CLAUDE.md"), "web\n").unwrap();
        fs::create_dir_all(repo.path().join("node_modules/x")).unwrap();
        fs::write(repo.path().join("node_modules/x/AGENTS.md"), "ignore\n").unwrap();
        let found = discover_project_sources(repo.path());
        assert_eq!(
            found
                .iter()
                .map(|source| source.path.as_str())
                .collect::<Vec<_>>(),
            ["AGENTS.md", "apps/web/CLAUDE.md"]
        );
        let mut rubric: Rubric = serde_json::from_value(serde_json::json!({
            "version":1,"compiledAt":"x","sources":[{"path":"AGENTS.md"},{"path":"apps/web/CLAUDE.md"}],"rules":[]
        })).unwrap();
        fill_source_hashes(&mut rubric, repo.path());
        assert_eq!(
            check_staleness(Some(&rubric), &found, repo.path()),
            Staleness::Fresh
        );
        fs::write(repo.path().join("AGENTS.md"), "changed\n").unwrap();
        assert!(matches!(
            check_staleness(Some(&rubric), &found, repo.path()),
            Staleness::Stale { .. }
        ));
    }
}
