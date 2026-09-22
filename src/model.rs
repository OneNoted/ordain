use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const RUBRIC_VERSION: u8 = 1;
pub const MAX_RUBRIC_RULES: usize = 512;
pub const MAX_RUBRIC_SOURCES: usize = 128;
pub const MAX_RULE_SCOPES: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rubric {
    pub version: u8,
    pub compiled_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiled_by: Option<String>,
    pub sources: Vec<RubricSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<Thresholds>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RubricSource {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Thresholds {
    pub act: f64,
    pub flag: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            act: 0.8,
            flag: 0.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub text: String,
    pub source: RuleSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Phase>,
    pub check: Check,
    #[serde(default)]
    pub status: RuleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<Calibration>,
    #[serde(skip)]
    pub origin: Option<Origin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSource {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Edit,
    Turn,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Turn => "turn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuleStatus {
    #[default]
    Active,
    Weak,
    Noisy,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Check {
    Lint {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        how: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlaps: Option<String>,
    },
    Model {
        question: Question,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlaps: Option<String>,
    },
    Deferred {
        reason: String,
    },
    Unenforceable {
        reason: String,
    },
}

impl Check {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Lint { .. } => "lint",
            Self::Model { .. } => "model",
            Self::Deferred { .. } => "deferred",
            Self::Unenforceable { .. } => "unenforceable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Boolean {
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<BTreeMap<String, String>>,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
        violating: Vec<String>,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
        #[serde(rename = "violatingFrom")]
        violating_from: usize,
    },
}

impl Question {
    pub fn instructions(&self) -> &str {
        match self {
            Self::Boolean { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => instructions,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Calibration {
    pub at: String,
    pub hunks: usize,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub fired: usize,
    pub verdict: CalibrationVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalibrationVerdict {
    Decisive,
    Weak,
    Noisy,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Project,
    Global,
    Preset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Band {
    Act,
    Flag,
    Clear,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub rule_id: String,
    pub probability: f64,
    pub band: Band,
    #[serde(default)]
    pub action: crate::config::Action,
    #[serde(default)]
    pub policy_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<crate::evidence::Manifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Event {
    Check {
        at: String,
        phase: Phase,
        #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(rename = "promptId", skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
        files: Vec<String>,
        rules: usize,
        #[serde(rename = "latencyMs")]
        latency_ms: u64,
        #[serde(rename = "modelLatencyMs", skip_serializing_if = "Option::is_none")]
        model_latency_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        verdicts: Vec<Verdict>,
        blocked: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        suppressed_rules: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        steered_rules: Vec<String>,
    },
    Skip {
        at: String,
        phase: Phase,
        #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files: Option<Vec<String>>,
    },
    Error {
        at: String,
        phase: String,
        #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        code: String,
        message: String,
        #[serde(rename = "latencyMs", skip_serializing_if = "Option::is_none")]
        latency_ms: Option<u64>,
    },
    CompileNeeded {
        at: String,
        #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        reason: String,
        sources: Vec<String>,
    },
    HistoryBoundary {
        at: String,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub file: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookCommon {
    pub session_id: String,
    #[serde(default)]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub cwd: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub prompt: Option<String>,
}

impl HookCommon {
    pub fn turn_id(&self) -> Option<&str> {
        self.prompt_id.as_deref().or(self.turn_id.as_deref())
    }
}

#[derive(Debug, Clone)]
pub enum HookOutput {
    Silent,
    SessionContext {
        additional_context: String,
        system_message: Option<String>,
    },
    Block {
        reason: String,
        system_message: Option<String>,
    },
    Notice {
        system_message: String,
    },
}

impl HookOutput {
    /// Incomplete checking must remain visible to both the operator and the agent.
    pub fn incomplete(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::SessionContext {
            additional_context: message.clone(),
            system_message: Some(message),
        }
    }

    pub fn to_protocol(&self, event: &str) -> Option<Value> {
        match self {
            Self::Silent => None,
            Self::SessionContext {
                additional_context,
                system_message,
            } => Some(serde_json::json!({
                "systemMessage": system_message,
                "hookSpecificOutput": {
                    "hookEventName": event,
                    "additionalContext": additional_context
                }
            })),
            Self::Block {
                reason,
                system_message,
            } => Some(serde_json::json!({
                "decision": "block",
                "reason": reason,
                "systemMessage": system_message
            })),
            Self::Notice { system_message } => {
                Some(serde_json::json!({"systemMessage": system_message}))
            }
        }
        .map(remove_nulls)
    }
}

fn remove_nulls(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.retain(|_, value| !value.is_null());
    }
    value
}

pub fn validate_rubric(rubric: &Rubric) -> Vec<String> {
    let mut issues = Vec::new();
    if rubric.version != RUBRIC_VERSION {
        issues.push(format!("version: expected {RUBRIC_VERSION}"));
    }
    if rubric.compiled_at.is_empty() {
        issues.push("compiledAt: must not be empty".into());
    }
    if rubric.rules.len() > MAX_RUBRIC_RULES {
        issues.push(format!(
            "rules: at most {MAX_RUBRIC_RULES} entries are allowed"
        ));
    }
    if rubric.sources.len() > MAX_RUBRIC_SOURCES {
        issues.push(format!(
            "sources: at most {MAX_RUBRIC_SOURCES} entries are allowed"
        ));
    }
    let thresholds = rubric.thresholds.unwrap_or_default();
    if !(0.0..=1.0).contains(&thresholds.flag)
        || !(0.0..=1.0).contains(&thresholds.act)
        || thresholds.flag >= thresholds.act
    {
        issues.push("thresholds: flag must be below act and both must be in [0,1]".into());
    }
    let mut ids = HashSet::new();
    for (index, rule) in rubric.rules.iter().enumerate() {
        let at = format!("rules.{index}");
        if !valid_rule_id(&rule.id) {
            issues.push(format!("{at}.id: rule ids are kebab-case"));
        }
        if !ids.insert(rule.id.as_str()) {
            issues.push(format!("{at}.id: duplicate rule id {:?}", rule.id));
        }
        if rule.text.is_empty() || rule.text.len() > 600 {
            issues.push(format!("{at}.text: must contain 1..=600 bytes"));
        }
        if rule.source.path.is_empty() || rule.source.line == Some(0) {
            issues.push(format!(
                "{at}.source: path is required and line must be positive"
            ));
        }
        if rule.scope.as_ref().is_some_and(|s| {
            s.is_empty()
                || s.len() > MAX_RULE_SCOPES
                || s.iter().any(|scope| scope.is_empty() || scope.len() > 500)
        }) {
            issues.push(format!(
                "{at}.scope: must contain 1..={MAX_RULE_SCOPES} globs of at most 500 bytes"
            ));
        } else if let Some(scope) = &rule.scope
            && let Err(error) = crate::paths::compile_globs(scope)
        {
            issues.push(format!("{at}.scope: invalid glob ({error})"));
        }
        match &rule.check {
            Check::Lint { how, pattern, .. } => {
                if how.as_ref().is_none_or(String::is_empty)
                    && pattern.as_ref().is_none_or(String::is_empty)
                {
                    issues.push(format!("{at}.check: lint needs how or pattern"));
                }
            }
            Check::Model { question, .. } => {
                if rule.when.is_none() {
                    issues.push(format!("{at}.when: model rules need edit or turn"));
                }
                validate_question(question, &format!("{at}.check.question"), &mut issues);
            }
            Check::Deferred { reason } | Check::Unenforceable { reason } => {
                if reason.is_empty() || reason.len() > 300 {
                    issues.push(format!("{at}.check.reason: must contain 1..=300 bytes"));
                }
            }
        }
        if let Some(calibration) = &rule.calibration {
            for (name, value) in [
                ("median", calibration.median),
                ("min", calibration.min),
                ("max", calibration.max),
            ] {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    issues.push(format!("{at}.calibration.{name}: must be in [0,1]"));
                }
            }
        }
    }
    for (index, source) in rubric.sources.iter().enumerate() {
        if source.path.is_empty() {
            issues.push(format!("sources.{index}.path: must not be empty"));
        }
        if source
            .sha
            .as_ref()
            .is_some_and(|sha| sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            issues.push(format!("sources.{index}.sha: must be sha256 hex"));
        }
    }
    issues
}

fn validate_question(question: &Question, at: &str, issues: &mut Vec<String>) {
    if question.instructions().is_empty() || question.instructions().len() > 2000 {
        issues.push(format!("{at}.instructions: must contain 1..=2000 bytes"));
    }
    match question {
        Question::Boolean { criteria, .. } => {
            if criteria.as_ref().is_some_and(|criteria| {
                criteria.iter().any(|(key, value)| {
                    !matches!(key.as_str(), "true" | "false") || value.len() > 1000
                })
            }) {
                issues.push(format!(
                    "{at}.criteria: boolean keys are true/false and values <=1000"
                ));
            }
        }
        Question::Choice {
            criteria,
            violating,
            ..
        } => {
            if criteria.len() < 2 || criteria.len() > 255 {
                issues.push(format!("{at}.criteria: choice needs 2..=255 options"));
            }
            if criteria
                .iter()
                .any(|(key, value)| key.is_empty() || value.len() > 1000)
            {
                issues.push(format!("{at}.criteria: invalid option"));
            }
            if violating.is_empty()
                || violating.len() >= criteria.len()
                || violating.iter().any(|name| !criteria.contains_key(name))
            {
                issues.push(format!(
                    "{at}.violating: needs known options and one compliant option"
                ));
            }
        }
        Question::Score {
            criteria,
            violating_from,
            ..
        } => {
            if !(2..=10).contains(&criteria.len())
                || criteria.iter().any(|value| value.len() > 1000)
            {
                issues.push(format!("{at}.criteria: score needs 2..=10 levels"));
            }
            if *violating_from == 0 || *violating_from >= criteria.len() {
                issues.push(format!("{at}.violatingFrom: must name a nonzero level"));
            }
        }
    }
}

fn valid_rule_id(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty()
        || !first.as_bytes()[0].is_ascii_lowercase()
        || !first
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return false;
    }
    parts.all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

pub fn band_for(probability: f64, thresholds: Thresholds) -> Band {
    if probability >= thresholds.act {
        Band::Act
    } else if probability >= thresholds.flag {
        Band::Flag
    } else {
        Band::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_semantics_reject_bad_model_and_choice() {
        let rubric: Rubric = serde_json::from_value(serde_json::json!({
            "version": 1,
            "compiledAt": "x",
            "sources": [{"path":"AGENTS.md"}],
            "rules": [{
                "id":"bad_id", "text":"x", "source":{"path":"AGENTS.md"},
                "check":{"type":"model", "question":{"type":"choice", "instructions":"?", "criteria":{"yes":"y"}, "violating":["no"]}}
            }]
        })).unwrap();
        let issues = validate_rubric(&rubric).join("\n");
        assert!(issues.contains("kebab-case"));
        assert!(issues.contains("model rules need"));
        assert!(issues.contains("choice needs"));
    }

    #[test]
    fn hook_protocol_omits_absent_fields() {
        let value = HookOutput::SessionContext {
            additional_context: "compile".into(),
            system_message: None,
        }
        .to_protocol("SessionStart")
        .unwrap();
        assert_eq!(value["hookSpecificOutput"]["additionalContext"], "compile");
        assert!(value.get("systemMessage").is_none());
    }
}
