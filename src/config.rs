//! Project-owned checking policy, separate from generated rubrics.
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{ErrorCode, OrdainError, Result};
use crate::model::{Band, Check, Phase, Rule, RuleStatus, Thresholds, Verdict};
use crate::paths::{compile_globs, ordain_dir, read_regular_text};

pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;
pub const MAX_CONTEXT_BYTES: usize = 1024 * 1024;
// Application request guard, NOT a claim about Jev's token window.
pub const DEFAULT_CONTEXT_BYTES: usize = 96 * 1024;
pub const MAX_DEADLINE_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    #[default]
    Record,
    Notice,
    Steer,
    Block,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionThreshold {
    pub at: f64,
    pub action: Action,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    #[default]
    Diff,
    ChangedRanges,
    ChangedFiles,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationUnit {
    File,
    #[default]
    Changeset,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextPolicy {
    pub mode: ContextMode,
    pub unit: EvaluationUnit,
    pub include: Vec<String>,
    pub max_bytes: usize,
    pub surrounding_lines: usize,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            mode: ContextMode::Diff,
            unit: EvaluationUnit::Changeset,
            include: Vec::new(),
            max_bytes: DEFAULT_CONTEXT_BYTES,
            surrounding_lines: 20,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextPatch {
    pub mode: Option<ContextMode>,
    pub unit: Option<EvaluationUnit>,
    pub include: Option<Vec<String>>,
    pub max_bytes: Option<usize>,
    pub surrounding_lines: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyPatch {
    pub enabled: Option<bool>,
    pub actions: Option<Vec<ActionThreshold>>,
    pub scope: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub phases: Option<Vec<Phase>>,
    pub deadline_ms: Option<u64>,
    pub max_repairs: Option<usize>,
    pub context: ContextPatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_repairs: usize,
    pub max_stop_checks: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_repairs: 8,
            max_stop_checks: 2,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    pub defaults: PolicyPatch,
    pub rules: BTreeMap<String, PolicyPatch>,
    pub limits: Limits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulePolicy {
    pub enabled: bool,
    pub actions: Vec<ActionThreshold>,
    pub scope: Vec<String>,
    pub exclude: Vec<String>,
    pub phases: Vec<Phase>,
    pub deadline_ms: u64,
    pub max_repairs: usize,
    pub context: ContextPolicy,
    pub origins: BTreeMap<String, String>,
    pub revision: String,
}

impl ProjectConfig {
    pub fn load(root: &Path) -> Result<Self> {
        let path = ordain_dir(root).join("config.toml");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
            Ok(_) => {
                let text = read_regular_text(&path, MAX_CONFIG_BYTES, false).ok_or_else(|| {
                    invalid("project config must be a bounded regular UTF-8 file")
                })?;
                Self::parse(&text)
            }
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|error| invalid(format!("invalid project config: {error}")))
    }

    pub fn resolve(
        &self,
        rules: &[Rule],
        thresholds: Thresholds,
    ) -> Result<BTreeMap<String, RulePolicy>> {
        for id in self.rules.keys() {
            if !rules.iter().any(|rule| &rule.id == id) {
                return Err(invalid(format!("orphaned rule override: {id}")));
            }
        }
        if self.limits.max_repairs > 32 || !(1..=8).contains(&self.limits.max_stop_checks) {
            return Err(invalid(
                "limits: max_repairs must be <=32 and max_stop_checks 1..=8",
            ));
        }
        // Validate defaults even when the rubric is empty or every rule overrides them.
        validate_patch(&self.defaults)?;
        for patch in self.rules.values() {
            validate_patch(patch)?;
        }
        let mut resolved = BTreeMap::new();
        for rule in rules {
            let mut policy = RulePolicy {
                enabled: rule.status == RuleStatus::Active,
                actions: vec![
                    ActionThreshold {
                        at: thresholds.flag,
                        action: Action::Notice,
                    },
                    ActionThreshold {
                        at: thresholds.act,
                        action: Action::Block,
                    },
                ],
                scope: rule.scope.clone().unwrap_or_else(|| vec!["**".into()]),
                exclude: Vec::new(),
                phases: rule.when.into_iter().collect(),
                deadline_ms: MAX_DEADLINE_MS,
                max_repairs: 2,
                context: ContextPolicy {
                    unit: if rule.when == Some(Phase::Edit) {
                        EvaluationUnit::File
                    } else {
                        EvaluationUnit::Changeset
                    },
                    ..ContextPolicy::default()
                },
                origins: BTreeMap::from([
                    ("rule".into(), rule.source.path.clone()),
                    ("enabled".into(), "rubric".into()),
                    ("actions".into(), "rubric thresholds / built-in".into()),
                    ("scope".into(), "rubric / built-in".into()),
                    ("phases".into(), "rubric".into()),
                    ("exclude".into(), "built-in".into()),
                    ("deadline_ms".into(), "built-in".into()),
                    ("max_repairs".into(), "built-in".into()),
                    ("context.mode".into(), "built-in".into()),
                    ("context.unit".into(), "built-in".into()),
                    ("context.include".into(), "built-in".into()),
                    ("context.max_bytes".into(), "built-in".into()),
                    ("context.surrounding_lines".into(), "built-in".into()),
                ]),
                revision: String::new(),
            };
            if rule.origin == Some(crate::model::Origin::Preset) {
                policy.actions = vec![ActionThreshold {
                    at: thresholds.act,
                    action: Action::Notice,
                }];
                policy.exclude = [
                    "**/vendor/**",
                    "**/node_modules/**",
                    "**/target/**",
                    "**/dist/**",
                    "**/generated/**",
                    "**/*.generated.*",
                ]
                .into_iter()
                .map(String::from)
                .collect();
                policy
                    .origins
                    .insert("actions".into(), "preset (notice-only)".into());
                policy.origins.insert("exclude".into(), "preset".into());
            }
            policy.overlay(&self.defaults, "project defaults");
            if let Some(patch) = self.rules.get(&rule.id) {
                policy.overlay(patch, &format!("rules.{}", rule.id));
            }
            if policy.enabled
                && matches!(rule.check, Check::Model { .. })
                && policy.phases.is_empty()
            {
                return Err(invalid(format!("rule {} has no evaluation phase", rule.id)));
            }
            if policy.phases.contains(&Phase::Turn)
                && policy.actions.iter().any(|a| a.action == Action::Steer)
            {
                return Err(invalid(format!(
                    "rule {}: nonblocking steer is supported only at edit time; use notice/block for turn checks",
                    rule.id
                )));
            }
            policy.revision = hex::encode(Sha256::digest(
                serde_json::to_vec(&(&policy, rule, &self.limits))
                    .expect("finite validated policy"),
            ));
            resolved.insert(rule.id.clone(), policy);
        }
        Ok(resolved)
    }
}

impl RulePolicy {
    fn overlay(&mut self, patch: &PolicyPatch, origin: &str) {
        macro_rules! assign {
            ($($field:ident),* $(,)?) => { $(if let Some(value) = &patch.$field {
                self.$field = value.clone();
                self.origins.insert(stringify!($field).into(), origin.into());
            })* };
        }
        assign!(
            enabled,
            actions,
            scope,
            exclude,
            phases,
            deadline_ms,
            max_repairs
        );
        macro_rules! context {
            ($($field:ident),* $(,)?) => { $(if let Some(value) = &patch.context.$field {
                self.context.$field = value.clone();
                self.origins.insert(concat!("context.", stringify!($field)).into(), origin.into());
            })* };
        }
        context!(mode, unit, include, max_bytes, surrounding_lines);
    }

    pub fn decide(&self, score: f64) -> Action {
        self.actions
            .iter()
            .rev()
            .find(|entry| score >= entry.at)
            .map_or(Action::Record, |entry| entry.action)
    }

    pub fn apply(&self, verdict: &mut Verdict) {
        verdict.action = self.decide(verdict.probability);
        verdict.band = match verdict.action {
            Action::Block => Band::Act,
            Action::Notice | Action::Steer => Band::Flag,
            Action::Record => Band::Clear,
        };
        verdict.policy_revision = self.revision.clone();
    }
}

fn validate_patch(patch: &PolicyPatch) -> Result<()> {
    if let Some(actions) = &patch.actions {
        let mut previous = None;
        for entry in actions {
            if !entry.at.is_finite()
                || !(0.0..=1.0).contains(&entry.at)
                || entry.action == Action::Record
            {
                return Err(invalid(
                    "actions require finite thresholds in [0,1] and notice, steer or block",
                ));
            }
            if previous.is_some_and(|(at, action)| entry.at <= at || entry.action <= action) {
                return Err(invalid(
                    "action thresholds and strengths must strictly increase",
                ));
            }
            previous = Some((entry.at, entry.action));
        }
    }
    for patterns in [&patch.scope, &patch.exclude, &patch.context.include]
        .into_iter()
        .flatten()
    {
        if patterns.len() > 64 || patterns.iter().any(|p| p.len() > 500) {
            return Err(invalid(
                "path selectors allow at most 64 patterns of 500 bytes",
            ));
        }
        if !patterns.is_empty() {
            compile_globs(patterns).map_err(invalid)?;
        }
    }
    if patch.context.include.as_ref().is_some_and(|ps| {
        ps.iter().any(|p| {
            Path::new(p).is_absolute()
                || Path::new(p)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
        })
    }) {
        return Err(invalid(
            "context.include must use repository-relative paths without parent traversal",
        ));
    }
    if patch
        .phases
        .as_ref()
        .is_some_and(|p| p.is_empty() || p.len() > 2 || (p.len() == 2 && p[0] == p[1]))
    {
        return Err(invalid(
            "phases must contain edit, turn or both, without duplicates",
        ));
    }
    if patch
        .deadline_ms
        .is_some_and(|n| n == 0 || n > MAX_DEADLINE_MS)
    {
        return Err(invalid(
            "deadline_ms must be 1..=15000 (also bounded by the invocation deadline)",
        ));
    }
    if patch.max_repairs.is_some_and(|n| n > 32) {
        return Err(invalid("max_repairs must be <=32"));
    }
    if patch
        .context
        .max_bytes
        .is_some_and(|n| !(1024..=MAX_CONTEXT_BYTES).contains(&n))
    {
        return Err(invalid(
            "context.max_bytes must be 1024..=1048576 request bytes",
        ));
    }
    if patch.context.surrounding_lines.is_some_and(|n| n > 500) {
        return Err(invalid("context.surrounding_lines must be <=500"));
    }
    Ok(())
}

pub fn invalid(message: impl Into<String>) -> OrdainError {
    OrdainError::new(ErrorCode::SettingsInvalid, message)
}

/// Offline policy inspection needs neither credentials nor a provider request.
pub fn run(action: &str, rule_id: Option<&str>, json: bool) -> Result<i32> {
    let root = crate::paths::find_repo_root(&std::env::current_dir()?);
    let loaded = crate::rubric::load_rules(&root);
    if !loaded.problems.is_empty() {
        return Err(invalid(loaded.problems.join("; ")));
    }
    let config = ProjectConfig::load(&root)?;
    let mut policies = config.resolve(&loaded.rules, loaded.thresholds)?;
    if let Some(id) = rule_id {
        let policy = policies
            .remove(id)
            .ok_or_else(|| invalid(format!("unknown rule: {id}")))?;
        policies = BTreeMap::from([(id.to_owned(), policy)]);
    }
    if !matches!(action, "validate" | "explain") {
        return Err(invalid("config action must be validate or explain"));
    }
    let warnings: Vec<_> = policies
        .iter()
        .filter(|(_, p)| p.actions.first().is_some_and(|a| a.at == 0.0))
        .map(|(id, _)| format!("{id}: threshold zero delivers every score"))
        .collect();
    let value = serde_json::json!({"valid":true,"rules":policies,"limits":config.limits,"warnings":warnings,"config_home":crate::paths::global_dir(),"state_home":crate::paths::state_dir(),"cache_home":crate::paths::cache_dir(),"project_config":ordain_dir(&root).join("config.toml")});
    if json || action == "explain" {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("validated policy")
        );
    } else {
        println!("Project configuration valid ({} rules).", policies.len());
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
    }
    Ok(0)
}
