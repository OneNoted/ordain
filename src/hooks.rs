use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::Value;

use crate::check::{CheckFailure, CheckOutcome, CheckRequest, EvaluationContext, merge_outcomes};
use crate::compile::{compile_prompt, place_compile_skill, plan_compile};
use crate::credentials::{Credentials, find_credentials};
use crate::diff::{edits_from_payload, split_diff, unified_file_diff};
use crate::events::append_event;
use crate::git::{blob_ids_at, diff_trees, is_git_repo, snapshot_tree};
use crate::model::{Event, FileDiff, HookCommon, HookOutput, Phase, Rule, Verdict};
use crate::paths::{
    MAX_FILE_READ_BYTES, find_repo_root, is_excluded_path, read_regular, read_regular_text,
    relative_to_root,
};
use crate::rubric::load_rules;
use crate::state::{
    BaselineStatus, CheckedEdit, blob_id, block_count, clear_turn, edits_cover_file,
    has_turn_state, increment_block, increment_stop_checks, mark_baseline, prune_old_turns,
    read_baseline, read_baseline_status, read_blocked_files, read_checked, read_file_starts,
    read_prompt, record_blocked_file, record_checked, record_file_start, stop_check_count,
    turn_dir, write_baseline, write_prompt,
};

const EDIT_TIMEOUT: Duration = Duration::from_secs(8);
const TURN_TIMEOUT: Duration = Duration::from_secs(15);
const TURN_START_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_GIT_TIMEOUT: Duration = Duration::from_secs(8);
const STOP_FALLBACK_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_EDITS_PER_TOOL_USE: usize = 16;

pub fn handle(name: &str, raw: Value, binary: &Path) -> HookOutput {
    match name {
        "session-start" => handle_session_start(raw, binary),
        "turn-start" => handle_turn_start(raw),
        "post-tool-use" => handle_post_tool_use(raw),
        "stop" => handle_stop(raw),
        _ => HookOutput::Silent,
    }
}

fn common(raw: &Value, expected: &str) -> Option<HookCommon> {
    let parsed: HookCommon = serde_json::from_value(raw.clone()).ok()?;
    (parsed.hook_event_name == expected).then_some(parsed)
}

fn handle_session_start(raw: Value, binary: &Path) -> HookOutput {
    let Some(input) = common(&raw, "SessionStart") else {
        return HookOutput::Silent;
    };
    let root = find_repo_root(Path::new(&input.cwd));
    prune_old_turns(&root);
    let plan = plan_compile(&root);
    let mut notices = Vec::new();
    if matches!(find_credentials(&root), Credentials::None) {
        notices.push("Ordain: no API key was found, so edits are not being checked. Run ordain login, or put TYPESAFE_AI_API_KEY in the environment or a .env at the repo root, then start a new session.".to_owned());
    }
    notices.extend(plan.invalid.iter().map(|problem| {
        format!("Ordain: rubric could not be read and is being ignored: {problem}")
    }));
    let system_message = (!notices.is_empty()).then(|| notices.join("\n"));
    if plan.targets.is_empty() {
        return system_message.map_or(HookOutput::Silent, HookOutput::incomplete);
    }
    append_event(
        &root,
        &Event::CompileNeeded {
            at: Utc::now().to_rfc3339(),
            session_id: Some(input.session_id.clone()),
            reason: plan
                .targets
                .iter()
                .map(|target| format!("{}: {:?}", target.which, target.staleness))
                .collect::<Vec<_>>()
                .join(", "),
            sources: plan
                .targets
                .iter()
                .flat_map(|target| target.candidates.iter().map(|source| source.path.clone()))
                .collect(),
        },
    );
    let skill = place_compile_skill(&root);
    HookOutput::SessionContext {
        additional_context: compile_prompt(binary, &skill, &plan.targets, None),
        system_message,
    }
}

fn handle_turn_start(raw: Value) -> HookOutput {
    let Some(input) = common(&raw, "UserPromptSubmit") else {
        return HookOutput::Silent;
    };
    let root = find_repo_root(Path::new(&input.cwd));
    let directory = turn_dir(&root, &input.session_id, input.turn_id());
    if input.turn_id().is_none() {
        clear_turn(&directory);
    }
    if let Some(prompt) = input.prompt {
        write_prompt(&directory, &prompt);
    }
    if !is_git_repo(&root) {
        return HookOutput::Silent;
    }
    mark_baseline(&directory, BaselineStatus::Pending);
    match snapshot_tree(&root, TURN_START_TIMEOUT) {
        Ok(tree) => {
            write_baseline(&directory, &tree);
            mark_baseline(&directory, BaselineStatus::Ok);
        }
        Err(error) => {
            mark_baseline(&directory, BaselineStatus::Failed);
            append_event(
                &root,
                &Event::Error {
                    at: Utc::now().to_rfc3339(),
                    phase: "turn-start".into(),
                    session_id: Some(input.session_id),
                    code: error.code.as_str().into(),
                    message: error.message,
                    latency_ms: None,
                },
            );
        }
    }
    HookOutput::Silent
}

fn handle_post_tool_use(raw: Value) -> HookOutput {
    let Some(input) = common(&raw, "PostToolUse") else {
        return HookOutput::Silent;
    };
    if !matches!(
        raw.get("tool_name").and_then(Value::as_str),
        Some("Edit" | "Write" | "MultiEdit" | "apply_patch")
    ) {
        return HookOutput::Silent;
    }
    let started = Instant::now();
    let at = Utc::now().to_rfc3339();
    let all = edits_from_payload(&raw);
    let root = find_repo_root(Path::new(&input.cwd));
    let edits = all
        .into_iter()
        .filter_map(|edit| {
            let relative = relative_to_root(&root, &edit.file_path)?;
            (!is_excluded_path(&relative)).then_some(edit)
        })
        .collect::<Vec<_>>();
    if edits.is_empty() {
        return HookOutput::Silent;
    }
    if edits.len() > MAX_EDITS_PER_TOOL_USE {
        append_event(
            &root,
            &Event::Skip {
                at,
                phase: Phase::Edit,
                session_id: Some(input.session_id),
                reason: format!(
                    "tool use contained {} edits; per-tool checks are limited to {MAX_EDITS_PER_TOOL_USE} and the complete turn remains eligible for Stop checking",
                    edits.len()
                ),
                files: None,
            },
        );
        return HookOutput::Silent;
    }
    let directory = turn_dir(&root, &input.session_id, input.turn_id());
    for edit in &edits {
        record_file_start(&directory, &edit.file_path, edit.original.as_deref());
    }
    let loaded = load_rules(&root);
    debug_problems(&loaded.problems);
    if loaded.rules.is_empty() {
        return HookOutput::Silent;
    }
    let mut checkable = Vec::new();
    for mut edit in edits {
        let Some(relative) = relative_to_root(&root, &edit.file_path) else {
            continue;
        };
        let Some(text) = edit.text.take() else {
            append_event(
                &root,
                &Event::Skip {
                    at: at.clone(),
                    phase: Phase::Edit,
                    session_id: Some(input.session_id.clone()),
                    reason: "diff too large to compute in time".into(),
                    files: Some(vec![relative]),
                },
            );
            continue;
        };
        if !text.trim().is_empty() {
            if edit.after.is_none()
                && raw.get("tool_name").and_then(Value::as_str) == Some("apply_patch")
                && let Ok(Some(after)) = crate::evidence::read_source(&root, &relative)
            {
                let previous = crate::state::read_observed_file(&directory, &relative);
                // Retain the observation even if judging fails; it establishes no
                // coverage and is validated again before use by a later hook.
                let _ = crate::state::record_observed_file(&directory, &relative, &after);
                if let Ok(before) =
                    crate::evidence::reconstruct_native_before(&after, &text, previous.as_deref())
                {
                    edit.original = Some(before);
                    edit.after = Some(after);
                }
            }
            checkable.push((edit, relative, text));
        }
    }
    if checkable.is_empty() {
        return HookOutput::Silent;
    }
    let files = checkable
        .iter()
        .map(|(_, file, _)| file.clone())
        .collect::<Vec<_>>();
    if matches!(find_credentials(&root), Credentials::None) {
        append_event(
            &root,
            &Event::Skip {
                at,
                phase: Phase::Edit,
                session_id: Some(input.session_id),
                reason: "no api key".into(),
                files: Some(files),
            },
        );
        return HookOutput::Silent;
    }
    let context = match EvaluationContext::new(&root, &loaded.rules, loaded.thresholds) {
        Ok(context) => context,
        Err(error) => {
            append_event(
                &root,
                &Event::Error {
                    at,
                    phase: "edit".into(),
                    session_id: Some(input.session_id),
                    code: error.code.as_str().into(),
                    message: error.message.clone(),
                    latency_ms: Some(elapsed_ms(started)),
                },
            );
            return HookOutput::incomplete(format!(
                "Ordain could not run the edit check: {}",
                error.message
            ));
        }
    };
    let task =
        last_user_prompt(input.transcript_path.as_deref()).or_else(|| read_prompt(&directory));
    let mut snapshot = crate::evidence::Snapshot::default();
    let mut complete_snapshot = true;
    let diffs: Vec<_> = checkable
        .iter()
        .map(|(edit, file, diff)| {
            if let Some(after) = &edit.after {
                snapshot.insert(file.clone(), edit.original.clone(), Some(after.clone()));
            } else {
                complete_snapshot = false;
            }
            FileDiff {
                file: file.clone(),
                text: diff.clone(),
            }
        })
        .collect();
    let outcome = context.evaluate_live(
        CheckRequest {
            phase: Phase::Edit,
            file_diffs: &diffs,
            task: task.as_deref(),
            timeout: EDIT_TIMEOUT,
            retries: 0,
        },
        complete_snapshot.then_some(&snapshot),
    );
    // Incomplete evaluations cannot establish edit-chain coverage for Stop.
    if outcome.failures.is_empty() {
        for (edit, relative, _) in &checkable {
            if let Some(after) = &edit.after {
                record_checked(
                    &directory,
                    &CheckedEdit {
                        path: relative.clone(),
                        before: edit.original.as_ref().map(|s| blob_id(s.as_bytes())),
                        after: blob_id(after.as_bytes()),
                        policy_revision: context.revision(),
                    },
                );
            }
        }
    }
    HookReview {
        root: &root,
        directory: &directory,
        input: &input,
        phase: Phase::Edit,
        files: &files,
        started,
    }
    .deliver(&context, outcome)
}

struct HookReview<'a> {
    root: &'a Path,
    directory: &'a Path,
    input: &'a HookCommon,
    phase: Phase,
    files: &'a [String],
    started: Instant,
}
impl HookReview<'_> {
    fn deliver(self, context: &EvaluationContext, mut outcome: CheckOutcome) -> HookOutput {
        let at = Utc::now().to_rfc3339();
        let by_id: HashMap<_, _> = context.rules().map(|r| (r.id.as_str(), r)).collect();
        let mut acting = Vec::new();
        let mut flagged = Vec::new();
        let mut capped = Vec::new();
        let mut steering = Vec::new();
        if self.phase == Phase::Turn {
            let rules: Vec<_> = outcome
                .verdicts
                .iter()
                .filter(|v| v.action == crate::config::Action::Steer)
                .map(|v| v.rule_id.clone())
                .collect();
            if !rules.is_empty() {
                outcome.failures.push(CheckFailure {
                    files: self.files.to_vec(), rules,
                    code: crate::error::ErrorCode::UnsupportedDelivery,
                    message: "Edit-time steering was detected by the turn-end fallback. This host phase cannot deliver nonblocking agent context; the finding is recorded, not delivered as steering.".into(),
                    attempts: 0,
                });
            }
        }
        let failures: Vec<_> = outcome
            .failures
            .iter()
            .filter(|f| f.code != crate::error::ErrorCode::UnsupportedDelivery)
            .map(|f| format!("{}: {}", f.code.as_str(), f.message))
            .collect();
        let unsupported: Vec<_> = outcome
            .failures
            .iter()
            .filter(|f| f.code == crate::error::ErrorCode::UnsupportedDelivery)
            .map(|f| format!("{}: {}", f.code.as_str(), f.message))
            .collect();
        for failure in &outcome.failures {
            append_event(
                self.root,
                &Event::Error {
                    at: at.clone(),
                    phase: self.phase.as_str().into(),
                    session_id: Some(self.input.session_id.clone()),
                    code: failure.code.as_str().into(),
                    message: failure.message.clone(),
                    latency_ms: Some(elapsed_ms(self.started)),
                },
            );
        }
        let exhausted = block_count(self.directory, "total") >= context.limits().max_repairs;
        // Multiple file-local verdicts for one rule deliver one instruction, with its actual evidence retained in the event.
        for verdict in crate::check::loudest_verdicts(outcome.verdicts.clone()) {
            let Some(rule) = by_id.get(verdict.rule_id.as_str()) else {
                continue;
            };
            let policy = context.policy(&rule.id).expect("prepared rule has policy");
            let exhausted = exhausted
                || block_count(self.directory, &format!("rule:{}", rule.id)) >= policy.max_repairs;
            match verdict.action {
                crate::config::Action::Steer if self.phase == Phase::Turn => {
                    flagged.push(((*rule).clone(), verdict));
                }
                crate::config::Action::Block | crate::config::Action::Steer if !exhausted => {
                    increment_block(self.directory, &format!("rule:{}", rule.id));
                    if verdict.action == crate::config::Action::Block {
                        acting.push(((*rule).clone(), verdict));
                    } else {
                        steering.push(((*rule).clone(), verdict));
                    }
                }
                crate::config::Action::Notice => flagged.push(((*rule).clone(), verdict)),
                crate::config::Action::Block | crate::config::Action::Steer => {
                    capped.push(rule.id.clone())
                }
                crate::config::Action::Record => {}
            }
        }
        if !acting.is_empty() || !steering.is_empty() {
            increment_block(self.directory, "total");
            for file in self.files {
                record_blocked_file(self.directory, file);
            }
        }
        append_event(
            self.root,
            &Event::Check {
                at,
                phase: self.phase,
                session_id: Some(self.input.session_id.clone()),
                prompt_id: self.input.turn_id().map(ToOwned::to_owned),
                files: self.files.to_vec(),
                rules: outcome.model_rules.len(),
                latency_ms: elapsed_ms(self.started),
                model_latency_ms: Some(outcome.model_latency_ms),
                usage: Some(outcome.usage),
                verdicts: outcome.verdicts,
                blocked: !acting.is_empty(),
                suppressed_rules: capped.clone(),
                steered_rules: steering.iter().map(|(r, _)| r.id.clone()).collect(),
            },
        );
        let incomplete = (!failures.is_empty()).then(|| format!(
            "Ordain could not judge every applicable rule; no clean result was recorded for {} group(s): {}",
            failures.len(), failures.join("; ")
        ));
        let mut notice = combined_notice(self.phase, &flagged, self.files, incomplete.as_deref());
        if !unsupported.is_empty() {
            let message = unsupported.join("; ");
            notice = Some(notice.map_or_else(|| message.clone(), |n| format!("{n}\n{message}")));
        }
        if !capped.is_empty() {
            let message = format!(
                "Ordain: Repair limit reached for {}. Findings recorded, but no further repair requested.",
                capped.join(", ")
            );
            notice = Some(notice.map_or_else(|| message.clone(), |n| format!("{n}\n{message}")));
        }
        let advisory = steering
            .iter()
            .map(|(rule, verdict)| {
                format!(
                    "- Rule {} from {}: {} (confidence {:.2})",
                    rule.id, rule.source.path, rule.text, verdict.probability
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !acting.is_empty() {
            let mut reason = repair_reason(self.phase, &acting, self.files);
            if !advisory.is_empty() {
                reason.push_str(&format!(
                    "\nAlso consider these advisory findings:\n{advisory}"
                ));
            }
            if let Some(incomplete) = &incomplete {
                reason.push_str(&format!("\n{incomplete}"));
            }
            HookOutput::Block {
                reason,
                system_message: notice,
            }
        } else if !advisory.is_empty() || incomplete.is_some() {
            let mut context = Vec::new();
            if !advisory.is_empty() {
                context.push(format!(
                    "Ordain review: your edit succeeded. Consider these possible rule violations before continuing:\n{advisory}"
                ));
            }
            context.extend(incomplete);
            HookOutput::SessionContext {
                additional_context: context.join("\n"),
                system_message: notice,
            }
        } else {
            notice.map_or(HookOutput::Silent, |system_message| HookOutput::Notice {
                system_message,
            })
        }
    }
}

enum TurnDiff {
    Complete {
        files: Vec<String>,
        diffs: Vec<FileDiff>,
        start_ids: Option<HashMap<String, Option<String>>>,
        end_ids: Option<HashMap<String, String>>,
    },
    Incomplete {
        reason: String,
        missing: Vec<String>,
    },
}

fn turn_diff(root: &Path, directory: &Path) -> TurnDiff {
    if matches!(
        read_baseline_status(directory),
        Some(BaselineStatus::Failed | BaselineStatus::Pending)
    ) {
        return TurnDiff::Incomplete {
            reason: "git could not snapshot the working tree at turn start".into(),
            missing: Vec::new(),
        };
    }
    if let Some(baseline) = read_baseline(directory) {
        let started = Instant::now();
        let now = match snapshot_tree(root, STOP_GIT_TIMEOUT) {
            Ok(now) => now,
            Err(error) => return incomplete_git(error),
        };
        let Some(remaining) = STOP_GIT_TIMEOUT.checked_sub(started.elapsed()) else {
            return TurnDiff::Incomplete {
                reason: "git could not snapshot the working tree in time".into(),
                missing: Vec::new(),
            };
        };
        let patch = match diff_trees(root, &baseline, &now, remaining) {
            Ok(patch) => patch,
            Err(error) => return incomplete_git(error),
        };
        let diffs = match split_diff(&patch) {
            Ok(diffs) => diffs,
            Err(error) => return incomplete_git(error),
        };
        let files = diffs
            .iter()
            .map(|diff| diff.file.clone())
            .collect::<Vec<_>>();
        let Some(remaining) = STOP_GIT_TIMEOUT
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
        else {
            return TurnDiff::Incomplete {
                reason: "git could not snapshot the working tree in time".into(),
                missing: Vec::new(),
            };
        };
        let ids = match blob_ids_at(root, &baseline, &files, remaining) {
            Ok(ids) => files
                .iter()
                .map(|file| (file.clone(), ids.get(file).cloned()))
                .collect(),
            Err(error) => return incomplete_git(error),
        };
        let Some(remaining) = STOP_GIT_TIMEOUT.checked_sub(started.elapsed()) else {
            return TurnDiff::Incomplete {
                reason: "snapshot identity check exceeded its deadline".into(),
                missing: files,
            };
        };
        let end_ids = match blob_ids_at(root, &now, &files, remaining) {
            Ok(ids) => ids,
            Err(error) => return incomplete_git(error),
        };
        return TurnDiff::Complete {
            files,
            diffs,
            start_ids: Some(ids),
            end_ids: Some(end_ids),
        };
    }
    let deadline = Instant::now() + STOP_FALLBACK_TIMEOUT;
    let mut diffs = Vec::new();
    let mut missing = Vec::new();
    let mut start_ids = HashMap::new();
    for start in read_file_starts(directory) {
        if Instant::now() >= deadline {
            missing.push(relative_to_root(root, Path::new(&start.path)).unwrap_or(start.path));
            continue;
        }
        let Some(relative) = relative_to_root(root, Path::new(&start.path)) else {
            continue;
        };
        if !start.original_complete {
            missing.push(relative);
            continue;
        }
        if is_excluded_path(&relative) {
            continue;
        }
        let after = read_regular_text(Path::new(&start.path), MAX_FILE_READ_BYTES, false);
        if after.is_none() && Path::new(&start.path).exists() {
            missing.push(relative);
            continue;
        }
        if start.original == after {
            continue;
        }
        let Some(patch) = unified_file_diff(
            &relative,
            start.original.as_deref().unwrap_or(""),
            after.as_deref().unwrap_or(""),
        ) else {
            missing.push(relative);
            continue;
        };
        match split_diff(&patch) {
            Ok(parsed) => diffs.extend(parsed),
            Err(error) => return incomplete_git(error),
        }
        start_ids.insert(
            relative,
            start.original.as_ref().map(|text| blob_id(text.as_bytes())),
        );
    }
    if missing.is_empty() {
        TurnDiff::Complete {
            files: diffs.iter().map(|diff| diff.file.clone()).collect(),
            diffs,
            start_ids: Some(start_ids),
            end_ids: None,
        }
    } else {
        TurnDiff::Incomplete {
            reason: "some files changed this turn could not be diffed in time".into(),
            missing,
        }
    }
}

fn incomplete_git(error: crate::error::OrdainError) -> TurnDiff {
    TurnDiff::Incomplete {
        reason: format!("{}: {}", error.code.as_str(), error.message),
        missing: Vec::new(),
    }
}

fn handle_stop(raw: Value) -> HookOutput {
    let Some(input) = common(&raw, "Stop") else {
        return HookOutput::Silent;
    };
    let started = Instant::now();
    let at = Utc::now().to_rfc3339();
    let root = find_repo_root(Path::new(&input.cwd));
    let directory = turn_dir(&root, &input.session_id, input.turn_id());
    let finish = |output: HookOutput| {
        if !matches!(
            output,
            HookOutput::Block { .. } | HookOutput::SessionContext { .. }
        ) {
            clear_turn(&directory);
        }
        output
    };
    if !has_turn_state(&directory) {
        return finish(HookOutput::Silent);
    }
    let loaded = load_rules(&root);
    debug_problems(&loaded.problems);
    if loaded.rules.is_empty() {
        return finish(HookOutput::Silent);
    }
    let turn = turn_diff(&root, &directory);
    let TurnDiff::Complete {
        files,
        diffs,
        start_ids,
        end_ids,
    } = turn
    else {
        let TurnDiff::Incomplete { reason, missing } = turn else {
            unreachable!()
        };
        append_event(
            &root,
            &Event::Skip {
                at,
                phase: Phase::Turn,
                session_id: Some(input.session_id),
                reason: format!("turn diff incomplete: {reason}"),
                files: Some(missing),
            },
        );
        return finish(HookOutput::incomplete(format!(
            "Ordain could not complete the turn check: {reason}. No clean result was recorded."
        )));
    };
    if files.is_empty() {
        return finish(HookOutput::Silent);
    }
    if matches!(find_credentials(&root), Credentials::None) {
        append_event(
            &root,
            &Event::Skip {
                at,
                phase: Phase::Turn,
                session_id: Some(input.session_id),
                reason: "no api key".into(),
                files: Some(files),
            },
        );
        return finish(HookOutput::Silent);
    }
    let context = match EvaluationContext::new(&root, &loaded.rules, loaded.thresholds) {
        Ok(context) => context,
        Err(error) => {
            append_event(
                &root,
                &Event::Error {
                    at,
                    phase: "turn".into(),
                    session_id: Some(input.session_id),
                    code: error.code.as_str().into(),
                    message: error.message.clone(),
                    latency_ms: Some(elapsed_ms(started)),
                },
            );
            return finish(HookOutput::incomplete(format!(
                "Ordain could not run the turn check: {}",
                error.message
            )));
        }
    };
    if stop_check_count(&directory) >= context.limits().max_stop_checks {
        return finish(HookOutput::Notice {
            system_message:
                "Ordain: turn-check limit reached; remaining changes were not rechecked.".into(),
        });
    }
    increment_stop_checks(&directory);
    let revision = context.revision();
    let checked: Vec<_> = read_checked(&directory)
        .into_iter()
        .filter(|edit| edit.policy_revision == revision)
        .collect();
    let blocked = read_blocked_files(&directory);
    let unchecked = diffs
        .iter()
        .enumerate()
        .filter(|file| {
            let file = file.1;
            if blocked.contains(&file.file) {
                return true;
            }
            let Some(start_ids) = &start_ids else {
                return true;
            };
            let Some(now) = read_regular(&root.join(&file.file), MAX_FILE_READ_BYTES, false) else {
                return true;
            };
            let now = blob_id(&now);
            let start = start_ids.get(&file.file).and_then(|id| id.as_deref());
            !edits_cover_file(
                start,
                &checked
                    .iter()
                    .filter(|edit| edit.path == file.file)
                    .cloned()
                    .collect::<Vec<_>>(),
                &now,
            )
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let task =
        last_user_prompt(input.transcript_path.as_deref()).or_else(|| read_prompt(&directory));
    let snapshot = match crate::evidence::Snapshot::capture(
        &root,
        &diffs,
        Instant::now() + Duration::from_secs(2),
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            append_event(
                &root,
                &Event::Error {
                    at,
                    phase: "turn".into(),
                    session_id: Some(input.session_id.clone()),
                    code: error.code.as_str().into(),
                    message: error.message.clone(),
                    latency_ms: Some(elapsed_ms(started)),
                },
            );
            return finish(HookOutput::incomplete(format!(
                "Ordain could not capture turn evidence: {}",
                error.message
            )));
        }
    };
    let check_deadline = Instant::now() + TURN_TIMEOUT;
    if end_ids.as_ref().is_some_and(|ids| {
        snapshot.files.iter().any(|(file, source)| {
            ids.get(file)
                != source
                    .after
                    .as_deref()
                    .map(|text| blob_id(text.as_bytes()))
                    .as_ref()
        })
    }) {
        let message = "Ordain: working tree changed after the turn snapshot; check superseded, no verdict delivered.";
        append_event(
            &root,
            &Event::Error {
                at,
                phase: "turn".into(),
                session_id: Some(input.session_id.clone()),
                code: crate::error::ErrorCode::Superseded.as_str().into(),
                message: message.into(),
                latency_ms: Some(elapsed_ms(started)),
            },
        );
        return finish(HookOutput::incomplete(message));
    }
    let turn_outcome = context.evaluate_live(
        CheckRequest {
            phase: Phase::Turn,
            file_diffs: &diffs,
            task: task.as_deref(),
            timeout: TURN_TIMEOUT,
            retries: 0,
        },
        Some(&snapshot),
    );
    let mut outcomes = vec![turn_outcome];
    if !unchecked.is_empty() {
        // Whole edit scope: a changeset rule must not lose cross-file relationships.
        outcomes.push(context.evaluate_live(
            CheckRequest {
                phase: Phase::Edit,
                file_diffs: &diffs,
                task: task.as_deref(),
                timeout: check_deadline.saturating_duration_since(Instant::now()),
                retries: 0,
            },
            Some(&snapshot),
        ));
    }
    let mut outcome = merge_outcomes(outcomes);
    if let Err(error) = snapshot.ensure_current(&root, check_deadline) {
        outcome.verdicts.clear();
        outcome.failures.push(CheckFailure {
            files: files.clone(),
            rules: outcome.model_rules.clone(),
            code: error.code,
            message: error.message,
            attempts: 0,
        });
    }
    finish(
        HookReview {
            root: &root,
            directory: &directory,
            input: &input,
            phase: Phase::Turn,
            files: &files,
            started,
        }
        .deliver(&context, outcome),
    )
}

fn last_user_prompt(path: Option<&str>) -> Option<String> {
    let path = Path::new(path?);
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let start = metadata.len().saturating_sub(512 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(512 * 1024).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    for line in text.lines().rev() {
        if !line.contains("\"type\":\"user\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("user")
            || value.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(content) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(str::trim)
        else {
            continue;
        };
        if !content.is_empty() {
            return Some(crate::paths::capture_task(content));
        }
    }
    None
}

fn repair_reason(phase: Phase, violations: &[(Rule, Verdict)], files: &[String]) -> String {
    let subject = if phase == Phase::Edit {
        "your edit succeeded, but the resulting code appears to violate"
    } else {
        "the changes in this turn appear to violate"
    };
    let lines = violations
        .iter()
        .map(|(rule, verdict)| {
            let where_ = rule.source.line.map_or_else(
                || rule.source.path.clone(),
                |line| format!("{} line {line}", rule.source.path),
            );
            let text = rule.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let text = if text.chars().count() > 220 {
                format!("{}...", text.chars().take(217).collect::<String>())
            } else {
                text
            };
            let evidence = verdict.answer.as_ref().map_or_else(
                || format!(" Confidence: {:.2}.", verdict.probability),
                |answer| format!(" Judged: {answer}. Confidence: {:.2}.", verdict.probability),
            );
            format!("- Rule {:?} from {where_}: {:?}.{evidence}", rule.id, text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let target = if files.len() == 1 {
        files[0].clone()
    } else {
        format!("{} files ({})", files.len(), files.join(", "))
    };
    let ask = if phase == Phase::Edit {
        format!("Repair {target} now, then continue with the task.")
    } else {
        format!("Repair {target} before you finish. Keep the fix to what the rule asks.")
    };
    format!(
        "Ordain: {subject} {}.\n{lines}\n{ask}",
        if violations.len() == 1 {
            "a repository rule".into()
        } else {
            format!("{} repository rules", violations.len())
        }
    )
}

fn flag_notice(phase: Phase, flagged: &[(Rule, Verdict)], files: &[String]) -> String {
    let list = flagged
        .iter()
        .map(|(rule, verdict)| format!("{} {:.2}", rule.id, verdict.probability))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Ordain: uncertain about {list} on {} ({}). Not sent to the agent. Use ordain report for details.",
        files.join(", "),
        phase.as_str()
    )
}

fn combined_notice(
    phase: Phase,
    flagged: &[(Rule, Verdict)],
    files: &[String],
    incomplete: Option<&str>,
) -> Option<String> {
    let mut notices = Vec::new();
    if !flagged.is_empty() {
        notices.push(flag_notice(phase, flagged, files));
    }
    if let Some(incomplete) = incomplete {
        notices.push(incomplete.to_owned());
    }
    (!notices.is_empty()).then(|| notices.join("\n"))
}

fn debug_problems(problems: &[String]) {
    if std::env::var_os("ORDAIN_DEBUG").is_some() {
        for problem in problems {
            eprintln!("ordain: {problem}");
        }
    }
}
fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_message_has_provenance_and_action() {
        let rule: Rule = serde_json::from_value(serde_json::json!({
            "id":"raw-error", "text":"Never show raw errors", "source":{"path":"AGENTS.md","line":12},
            "when":"edit", "check":{"type":"model","question":{"type":"boolean","instructions":"?"}}
        })).unwrap();
        let verdict = Verdict {
            action: crate::config::Action::Block,
            policy_revision: String::new(),
            evidence: None,
            rule_id: rule.id.clone(),
            probability: 0.9,
            band: crate::model::Band::Act,
            answer: None,
        };
        let violations = [(rule, verdict)];
        let files = ["src/a.rs".into()];
        let text = repair_reason(Phase::Edit, &violations, &files);
        assert!(text.starts_with(
            "Ordain: your edit succeeded, but the resulting code appears to violate a repository rule."
        ));
        assert!(text.contains("AGENTS.md line 12"));
        assert!(text.contains("Confidence: 0.90."));
        assert!(text.contains("Repair src/a.rs now"));
        let turn = repair_reason(Phase::Turn, &violations, &files);
        assert!(!turn.contains("your edit succeeded"));
        assert!(turn.contains("before you finish"));
    }
}
