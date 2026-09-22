use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use globset::GlobSet;
use serde::Serialize;

use crate::error::{ErrorCode, OrdainError, Result};
use crate::model::{Band, Check, FileDiff, Phase, Rule, Thresholds, Usage, Verdict};
use crate::paths::compile_globs;
use crate::provider::{CheckState, ProviderClient};

pub const MAX_CHECK_FILES: usize = 1_000;
const MAX_CONCURRENT_PROVIDER_CALLS: usize = 16;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallCounts {
    pub groups: usize,
    pub attempts: usize,
    pub succeeded: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckFailure {
    pub files: Vec<String>,
    pub rules: Vec<String>,
    pub code: ErrorCode,
    pub message: String,
    pub attempts: usize,
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub verdicts: Vec<Verdict>,
    pub model_rules: Vec<String>,
    pub calls: CallCounts,
    pub usage: Usage,
    pub model_latency_ms: u64,
    pub failures: Vec<CheckFailure>,
}

pub struct CheckRequest<'a> {
    pub phase: Phase,
    pub file_diffs: &'a [FileDiff],
    pub task: Option<&'a str>,
    pub timeout: Duration,
    pub retries: usize,
}

pub struct EvaluationContext {
    rules: Vec<PreparedRule>,
    thresholds: Thresholds,
    provider: ProviderClient,
    root: std::path::PathBuf,
    config: crate::config::ProjectConfig,
    rubric_inputs: Vec<(std::path::PathBuf, Option<Vec<u8>>)>,
}
struct PreparedRule {
    rule: Rule,
    scope: GlobSet,
    exclude: Option<GlobSet>,
    policy: crate::config::RulePolicy,
}
type GroupKey = (Vec<usize>, crate::config::ContextPolicy, u64);

enum EvidenceInput<'a> {
    Diff,
    Live(Option<&'a crate::evidence::Snapshot>),
    Revision(&'a str),
}

impl EvaluationContext {
    pub fn new(root: &Path, rules: &[Rule], thresholds: Thresholds) -> Result<Self> {
        let config = crate::config::ProjectConfig::load(root)?;
        let mut policy_rules = rules.to_vec();
        for rule in crate::rubric::load_rules(root).rules {
            if !policy_rules.iter().any(|r| r.id == rule.id) {
                policy_rules.push(rule);
            }
        }
        let policies = config.resolve(&policy_rules, thresholds)?;
        let mut prepared = Vec::new();
        for rule in rules {
            let policy = policies[&rule.id].clone();
            if !policy.enabled || !matches!(rule.check, Check::Model { .. }) {
                continue;
            }
            prepared.push(PreparedRule {
                rule: rule.clone(),
                scope: compile_globs(&policy.scope).map_err(crate::config::invalid)?,
                exclude: if policy.exclude.is_empty() {
                    None
                } else {
                    Some(compile_globs(&policy.exclude).map_err(crate::config::invalid)?)
                },
                policy,
            });
        }
        Ok(Self {
            rules: prepared,
            thresholds,
            provider: ProviderClient::new(root)?,
            root: root.to_path_buf(),
            config,
            rubric_inputs: [
                crate::paths::rubric_path(root),
                crate::paths::global_rubric_path(),
                crate::presets::path(root),
            ]
            .into_iter()
            .map(|p| {
                let bytes =
                    crate::paths::read_regular(&p, crate::paths::MAX_FILE_READ_BYTES, false);
                (p, bytes)
            })
            .collect(),
        })
    }
    /// Coverage is reusable only while both rule definitions and policy agree.
    pub fn revision(&self) -> String {
        use sha2::{Digest, Sha256};
        let rules: Vec<_> = self
            .rules
            .iter()
            .map(|r| (&r.rule, &r.policy.revision))
            .collect();
        hex::encode(Sha256::digest(
            serde_json::to_vec(&rules).expect("rule policy serializes"),
        ))
    }
    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().map(|r| &r.rule)
    }
    pub fn has_applicable_rule(&self, phase: Phase, file: &str) -> bool {
        self.rules.iter().any(|r| {
            r.policy.phases.contains(&phase)
                && r.scope.is_match(file)
                && !r.exclude.as_ref().is_some_and(|g| g.is_match(file))
        })
    }
    pub fn limits(&self) -> &crate::config::Limits {
        &self.config.limits
    }
    pub fn policy(&self, id: &str) -> Option<&crate::config::RulePolicy> {
        self.rules
            .iter()
            .find(|r| r.rule.id == id)
            .map(|r| &r.policy)
    }
    /// Historical checks never borrow today's working tree.
    pub fn evaluate(&self, request: CheckRequest<'_>) -> CheckOutcome {
        self.evaluate_inner(request, EvidenceInput::Diff)
    }
    pub fn evaluate_live(
        &self,
        request: CheckRequest<'_>,
        snapshot: Option<&crate::evidence::Snapshot>,
    ) -> CheckOutcome {
        self.evaluate_inner(request, EvidenceInput::Live(snapshot))
    }
    /// Evaluate a diff sampled from this exact non-merge commit. Source evidence
    /// comes from Git objects; current files cannot fill gaps in its history.
    pub fn evaluate_revision(&self, request: CheckRequest<'_>, revision: &str) -> CheckOutcome {
        self.evaluate_inner(request, EvidenceInput::Revision(revision))
    }
    fn evaluate_group(
        &self,
        request: &CheckRequest<'_>,
        key: &GroupKey,
        rule_indices: &[usize],
        snapshot: Option<&crate::evidence::Snapshot>,
        deadline: Instant,
    ) -> std::result::Result<crate::provider::ModelCheckResult, crate::provider::ProviderFailure>
    {
        let (indices, policy, _) = key;
        let rules: Vec<_> = rule_indices.iter().map(|i| &self.rules[*i].rule).collect();
        let planned = (|| -> Result<_> {
            crate::evidence::check_deadline(deadline)?;
            let mut state = state_for(request.phase, request.task, request.file_diffs, indices);
            if policy.mode != crate::config::ContextMode::Diff || !policy.include.is_empty() {
                let snapshot = snapshot.ok_or_else(|| {
                    crate::evidence::incomplete(
                        "requested source context is absent from historical/imported evidence",
                    )
                })?;
                let files: Vec<_> = indices.iter().map(|i| &request.file_diffs[*i]).collect();
                state.sources = snapshot.select(&files, policy)?;
            }
            let bytes = serde_json::to_vec(&self.provider.request_body(&rules, &state))
                .map_err(|e| crate::evidence::incomplete(e.to_string()))?
                .len();
            if bytes > policy.max_bytes {
                return Err(crate::evidence::incomplete(format!(
                    "required request is {bytes} bytes, limit is {}; no evidence truncated",
                    policy.max_bytes
                )));
            }
            let files: std::collections::BTreeSet<_> = indices
                .iter()
                .map(|i| request.file_diffs[*i].file.clone())
                .chain(state.sources.iter().map(|s| s.file.clone()))
                .collect();
            let manifest = crate::evidence::Manifest {
                snapshot: crate::evidence::fingerprint(&state.diff, &state.sources),
                mode: policy.mode,
                files: files.into_iter().collect(),
                complete_files: policy.mode == crate::config::ContextMode::ChangedFiles,
                request_bytes: bytes,
            };
            Ok((state, manifest))
        })();
        let (state, manifest) =
            planned.map_err(|error| crate::provider::ProviderFailure { error, attempts: 0 })?;
        let mut result =
            self.provider
                .check(&rules, &state, self.thresholds, deadline, request.retries)?;
        for verdict in &mut result.verdicts {
            if let Some(prepared) = rule_indices
                .iter()
                .map(|i| &self.rules[*i])
                .find(|r| r.rule.id == verdict.rule_id)
            {
                prepared.policy.apply(verdict);
                verdict.evidence = Some(manifest.clone());
            }
        }
        Ok(result)
    }

    fn evaluate_inner(
        &self,
        request: CheckRequest<'_>,
        evidence: EvidenceInput<'_>,
    ) -> CheckOutcome {
        let started = Instant::now();
        if request
            .task
            .is_some_and(|t| t == crate::paths::INCOMPLETE_TASK)
        {
            return failed_outcome(
                started,
                ErrorCode::ContextIncomplete,
                "task context exceeded capture limit; no verdict requested".into(),
            );
        }
        let deadline = started + request.timeout;
        let files = request.file_diffs;
        if files.len() > MAX_CHECK_FILES
            || files.iter().map(|f| f.text.len()).sum::<usize>()
                > 16 * crate::config::MAX_CONTEXT_BYTES
        {
            return failed_outcome(
                started,
                ErrorCode::ContextIncomplete,
                "check exceeds file or diff memory safety limit".into(),
            );
        }
        let mut groups = BTreeMap::<GroupKey, Vec<usize>>::new();
        let mut model_rules = Vec::new();
        for (index, prepared) in self.rules.iter().enumerate() {
            if Instant::now() >= deadline {
                return planning_timeout(started, model_rules, files);
            }
            if !prepared.policy.phases.contains(&request.phase) {
                continue;
            }
            let indices: Vec<_> = files
                .iter()
                .enumerate()
                .filter(|(_, file)| {
                    !crate::paths::is_excluded_path(&file.file)
                        && prepared.scope.is_match(&file.file)
                        && !prepared
                            .exclude
                            .as_ref()
                            .is_some_and(|g| g.is_match(&file.file))
                })
                .map(|(index, _)| index)
                .collect();
            if indices.is_empty() {
                continue;
            }
            model_rules.push(prepared.rule.id.clone());
            if prepared.policy.context.unit == crate::config::EvaluationUnit::File {
                for file_index in indices {
                    groups
                        .entry((
                            vec![file_index],
                            prepared.policy.context.clone(),
                            prepared.policy.deadline_ms,
                        ))
                        .or_default()
                        .push(index);
                }
            } else {
                groups
                    .entry((
                        indices,
                        prepared.policy.context.clone(),
                        prepared.policy.deadline_ms,
                    ))
                    .or_default()
                    .push(index);
            }
        }
        let mut snapshot = None;
        if !groups.is_empty() {
            let prepared = (|| -> Result<_> {
                let include: Vec<_> = groups
                    .keys()
                    .flat_map(|(_, context, _)| context.include.iter().cloned())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                match evidence {
                    EvidenceInput::Diff => Ok(None),
                    EvidenceInput::Live(supplied) => {
                        let mut captured = match supplied {
                            Some(snapshot) => snapshot.clone(),
                            None => {
                                crate::evidence::Snapshot::capture(&self.root, files, deadline)?
                            }
                        };
                        captured.capture_related(&self.root, &include, deadline)?;
                        Ok(Some(captured))
                    }
                    EvidenceInput::Revision(revision) => {
                        let paths: Vec<_> = groups
                            .keys()
                            .filter(|(_, policy, _)| {
                                policy.mode != crate::config::ContextMode::Diff
                                    || !policy.include.is_empty()
                            })
                            .flat_map(|(indices, _, _)| {
                                indices.iter().map(|i| files[*i].file.clone())
                            })
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        if paths.is_empty() {
                            return Ok(None);
                        }
                        crate::git::revision_snapshot(
                            &self.root, revision, &paths, &include, deadline,
                        )
                        .map(Some)
                    }
                }
            })();
            match prepared {
                Ok(captured) => snapshot = captured,
                Err(error) => return failed_outcome(started, error.code, error.message),
            }
        }
        let groups: Vec<_> = groups.into_iter().collect();
        let mut outcome = CheckOutcome {
            verdicts: Vec::new(),
            model_rules,
            calls: CallCounts {
                groups: groups.len(),
                ..Default::default()
            },
            usage: Usage::default(),
            model_latency_ms: 0,
            failures: Vec::new(),
        };
        let next = std::sync::atomic::AtomicUsize::new(0);
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            for _ in 0..groups.len().min(MAX_CONCURRENT_PROVIDER_CALLS) {
                let sender = sender.clone();
                let groups = &groups;
                let next = &next;
                let snapshot = snapshot.as_ref();
                let request = &request;
                scope.spawn(move || {
                    loop {
                        let job = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((key, rule_indices)) = groups.get(job) else {
                            break;
                        };
                        let group_deadline = deadline.min(started + Duration::from_millis(key.2));
                        let result = self.evaluate_group(
                            request,
                            key,
                            rule_indices,
                            snapshot,
                            group_deadline,
                        );
                        let _ = sender.send((job, result));
                    }
                });
            }
            drop(sender);
            for (job, result) in receiver {
                match result {
                    Ok(result) => {
                        outcome.calls.succeeded += 1;
                        outcome.calls.attempts += result.attempts;
                        add_usage(&mut outcome.usage, &result.usage);
                        outcome.verdicts.extend(result.verdicts);
                    }
                    Err(failure) => {
                        outcome.calls.attempts += failure.attempts;
                        let ((indices, _, _), rule_indices) = &groups[job];
                        outcome.failures.push(group_failure(
                            files,
                            indices,
                            &self.rules,
                            rule_indices,
                            failure.error.code,
                            failure.error.message,
                            failure.attempts,
                        ));
                    }
                }
            }
        });
        if let Some(snapshot) = snapshot.filter(|_| matches!(evidence, EvidenceInput::Live(_))) {
            let current = snapshot
                .ensure_current(&self.root, deadline)
                .and_then(|()| {
                    if self.rubric_inputs.iter().any(|(p, bytes)| {
                        &crate::paths::read_regular(p, crate::paths::MAX_FILE_READ_BYTES, false)
                            != bytes
                    }) {
                        return Err(OrdainError::new(
                            ErrorCode::Superseded,
                            "rubric changed during evaluation; finding withheld",
                        ));
                    }
                    let now = crate::config::ProjectConfig::load(&self.root)?;
                    if serde_json::to_value(now).ok() != serde_json::to_value(&self.config).ok() {
                        Err(OrdainError::new(
                            ErrorCode::Superseded,
                            "project policy changed during evaluation; finding withheld",
                        ))
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = current {
                outcome.verdicts.clear();
                outcome.failures.push(CheckFailure {
                    files: files.iter().map(|f| f.file.clone()).collect(),
                    rules: outcome.model_rules.clone(),
                    code: error.code,
                    message: error.message,
                    attempts: 0,
                });
            }
        }
        outcome.verdicts.sort_by(|a, b| {
            a.rule_id
                .cmp(&b.rule_id)
                .then_with(|| b.probability.total_cmp(&a.probability))
        });
        outcome.model_latency_ms = elapsed_ms(started);
        outcome
    }
}

fn group_failure(
    files: &[FileDiff],
    file_indices: &[usize],
    rules: &[PreparedRule],
    rule_indices: &[usize],
    code: ErrorCode,
    message: String,
    attempts: usize,
) -> CheckFailure {
    CheckFailure {
        files: file_indices
            .iter()
            .map(|index| files[*index].file.clone())
            .collect(),
        rules: rule_indices
            .iter()
            .map(|index| rules[*index].rule.id.clone())
            .collect(),
        code,
        message,
        attempts,
    }
}

fn planning_timeout(
    started: Instant,
    model_rules: Vec<String>,
    files: &[FileDiff],
) -> CheckOutcome {
    CheckOutcome {
        verdicts: Vec::new(),
        model_rules: model_rules.clone(),
        calls: CallCounts::default(),
        usage: Usage::default(),
        model_latency_ms: elapsed_ms(started),
        failures: vec![CheckFailure {
            files: files.iter().map(|file| file.file.clone()).collect(),
            rules: model_rules,
            code: ErrorCode::CheckTimeout,
            message: "the check exceeded its total time limit while preparing scopes".into(),
            attempts: 0,
        }],
    }
}

fn failed_outcome(started: Instant, code: ErrorCode, message: String) -> CheckOutcome {
    CheckOutcome {
        verdicts: Vec::new(),
        model_rules: Vec::new(),
        calls: CallCounts::default(),
        usage: Usage::default(),
        model_latency_ms: elapsed_ms(started),
        failures: vec![CheckFailure {
            files: Vec::new(),
            rules: Vec::new(),
            code,
            message,
            attempts: 0,
        }],
    }
}

fn state_for(
    phase: Phase,
    task: Option<&str>,
    files: &[FileDiff],
    indices: &[usize],
) -> CheckState {
    let single = phase == Phase::Edit && indices.len() == 1;
    let diff = if single {
        files[indices[0]].text.clone()
    } else {
        indices
            .iter()
            .map(|index| {
                let file = &files[*index];
                format!("--- a/{}\n+++ b/{}\n{}\n", file.file, file.file, file.text)
            })
            .collect()
    };
    CheckState {
        task: task.map(ToOwned::to_owned),
        file: single.then(|| files[indices[0]].file.clone()),
        files: (!single).then(|| {
            indices
                .iter()
                .map(|index| files[*index].file.clone())
                .collect()
        }),
        diff,
        sources: Vec::new(),
    }
}

fn add_usage(total: &mut Usage, next: &Usage) {
    total.input_tokens = sum_options(total.input_tokens, next.input_tokens);
    total.output_tokens = sum_options(total.output_tokens, next.output_tokens);
    total.cost_usd = sum_options(total.cost_usd, next.cost_usd);
}

fn sum_options<T>(a: Option<T>, b: Option<T>) -> Option<T>
where
    T: std::ops::Add<Output = T> + Copy + Default,
{
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or_default() + b.unwrap_or_default()),
    }
}

pub fn loudest_verdicts(verdicts: impl IntoIterator<Item = Verdict>) -> Vec<Verdict> {
    let mut best: HashMap<String, Verdict> = HashMap::new();
    for verdict in verdicts {
        match best.get(&verdict.rule_id) {
            Some(current) if current.probability >= verdict.probability => {}
            _ => {
                best.insert(verdict.rule_id.clone(), verdict);
            }
        }
    }
    best.into_values().collect()
}

pub fn merge_outcomes(outcomes: Vec<CheckOutcome>) -> CheckOutcome {
    let mut rules = Vec::new();
    let mut usage = Usage::default();
    let mut calls = CallCounts::default();
    let mut latency = 0;
    let mut verdicts = Vec::new();
    let mut failures = Vec::new();
    for outcome in outcomes {
        for rule in outcome.model_rules {
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
        calls.groups += outcome.calls.groups;
        calls.attempts += outcome.calls.attempts;
        calls.succeeded += outcome.calls.succeeded;
        latency = latency.max(outcome.model_latency_ms);
        add_usage(&mut usage, &outcome.usage);
        verdicts.extend(outcome.verdicts);
        failures.extend(outcome.failures);
    }
    CheckOutcome {
        verdicts: loudest_verdicts(verdicts),
        model_rules: rules,
        calls,
        usage,
        model_latency_ms: latency,
        failures,
    }
}

pub fn has_act(outcome: &CheckOutcome) -> bool {
    outcome
        .verdicts
        .iter()
        .any(|verdict| verdict.band == Band::Act)
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
