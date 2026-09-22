use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::check::{CheckRequest, EvaluationContext};
use crate::credentials::{Credentials, NO_KEY_HINT, find_credentials};
use crate::diff::{bound_state, edits_from_recorded_payload};
use crate::error::{ErrorCode, OrdainError, Result};
use crate::hosts::Host;
use crate::model::{Band, Check, FileDiff, Phase, Rule, Verdict};
use crate::paths::{
    MAX_TASK_CHARS, find_repo_root, home_dir, is_excluded_path, read_regular_text, relative_to_root,
};
use crate::process::{ProcessOptions, run as run_process};
use crate::rubric::load_rules;

const MAX_REPLAY_CONCURRENCY: usize = 16;
const MAX_REPLAY_SESSIONS: usize = 1_000;
const MAX_REPLAY_EDITS: usize = 10_000;
const MAX_REPLAY_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DISCOVERY_ENTRIES: usize = 100_000;

#[derive(Debug, Clone)]
struct ReplayEdit {
    input: Value,
}

#[derive(Debug, Clone)]
struct ReplayTurn {
    index: usize,
    prompt: Option<String>,
    edits: Vec<ReplayEdit>,
}

#[derive(Debug, Clone)]
struct ReplaySession {
    file: PathBuf,
    cwd: PathBuf,
    turns: Vec<ReplayTurn>,
    coverage: TraceCoverage,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceCoverage {
    tool_calls: usize,
    other_tool_calls: usize,
    failed_edits: usize,
    unmatched_edits: usize,
    unreconstructable_edits: usize,
    malformed_records: usize,
}

impl TraceCoverage {
    fn incomplete(&self) -> bool {
        self.unmatched_edits + self.unreconstructable_edits + self.malformed_records > 0
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EditResult {
    session: String,
    turn: usize,
    file: String,
    verdicts: Vec<Verdict>,
    cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnResult {
    session: String,
    turn: usize,
    files: Vec<String>,
    verdicts: Vec<Verdict>,
    cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct DriftRow {
    label: &'static str,
    edits: usize,
    broken: usize,
    rate: f64,
}

#[derive(Debug, Serialize)]
struct RuleTally {
    rule: String,
    phase: Phase,
    broken: usize,
    flagged: usize,
    of: usize,
    example: String,
}

pub fn run(
    agent_or_path: Option<&str>,
    more_paths: &[String],
    repo: Option<&Path>,
    concurrency: usize,
    max_sessions: Option<usize>,
    keep_diffs: bool,
    json_output: bool,
) -> Result<i32> {
    if concurrency == 0 || concurrency > MAX_REPLAY_CONCURRENCY {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            format!("replay concurrency must be between 1 and {MAX_REPLAY_CONCURRENCY}"),
        ));
    }
    if max_sessions.is_some_and(|max| max == 0 || max > MAX_REPLAY_SESSIONS) {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            format!("--max-sessions must be between 1 and {MAX_REPLAY_SESSIONS}"),
        ));
    }
    let first = agent_or_path.ok_or_else(|| OrdainError::new(ErrorCode::HostUnknown, "name the agent whose sessions to replay: ordain replay claude|codex|opencode [--repo <path>]"))?;
    let (host, paths) = match Host::parse(first) {
        Ok(host) => (
            host,
            more_paths.iter().map(PathBuf::from).collect::<Vec<_>>(),
        ),
        Err(_) => {
            let mut paths = vec![PathBuf::from(first)];
            paths.extend(more_paths.iter().map(PathBuf::from));
            (Host::Claude, paths)
        }
    };
    let start = repo.unwrap_or(Path::new("."));
    let root = find_repo_root(start);
    if find_credentials(&root) == Credentials::None {
        return Err(OrdainError::new(ErrorCode::NoApiKey, NO_KEY_HINT));
    }
    let loaded = load_rules(&root);
    if loaded.rules.is_empty() {
        return Err(OrdainError::new(
            ErrorCode::RubricMissing,
            format!(
                "no rubric in {} or $XDG_CONFIG_HOME/ordain; run ordain compile there first",
                root.display()
            ),
        ));
    }
    let session_limit = max_sessions.unwrap_or(MAX_REPLAY_SESSIONS);
    let mut sessions = sessions_for(host, &root, &paths, session_limit)?;
    let discovered = sessions.len();
    sessions.retain(|session| inside(&root, &session.cwd));
    let outside_repo = discovered - sessions.len();
    if let Some(max) = max_sessions {
        sessions.truncate(max);
    }
    let edit_count = sessions
        .iter()
        .flat_map(|session| &session.turns)
        .map(|turn| turn.edits.len())
        .sum::<usize>();
    if edit_count > MAX_REPLAY_EDITS {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            format!(
                "replay selected {edit_count} edits; narrow the paths or sessions to at most {MAX_REPLAY_EDITS} edits"
            ),
        ));
    }
    let started = Instant::now();
    let context = EvaluationContext::new(&root, &loaded.rules, loaded.thresholds)?;
    let (mut edits, mut turns, omitted_files) = replay_sessions(
        &sessions,
        &context,
        concurrency.max(1),
        keep_diffs,
        !json_output,
    );
    edits.sort_by(|a, b| {
        a.session
            .cmp(&b.session)
            .then(a.turn.cmp(&b.turn))
            .then(a.file.cmp(&b.file))
    });
    turns.sort_by(|a, b| a.session.cmp(&b.session).then(a.turn.cmp(&b.turn)));
    let drift = drift_by_turn(&edits);
    let tallies = tally_rules(&edits, &turns, &loaded.rules);
    let spend: f64 = edits.iter().map(|result| result.cost_usd).sum::<f64>()
        + turns.iter().map(|result| result.cost_usd).sum::<f64>();
    let errors = edits.iter().filter(|result| result.error.is_some()).count()
        + turns.iter().filter(|result| result.error.is_some()).count();
    let verdicts: usize = edits
        .iter()
        .map(|result| result.verdicts.len())
        .sum::<usize>()
        + turns
            .iter()
            .map(|result| result.verdicts.len())
            .sum::<usize>();
    let incomplete =
        errors > 0 || verdicts == 0 || sessions.iter().any(|session| session.coverage.incomplete());
    let coverage = json!({
        "discoveredSessions": discovered, "outsideRepoSessions": outside_repo,
        "reconstructedEdits": edit_count, "fileEditResults": edits.len(),
        "omittedFileEdits": omitted_files,
        "notApplicableFileEdits": edits.iter().filter(|result| result.error.is_none() && result.verdicts.is_empty()).count(),
        "verdicts": verdicts, "evaluationErrors": errors, "incomplete": incomplete,
        "scope": "Recorded edits only; other tool calls and shell changes are not reconstructed. Turn evidence is accumulated recorded edits, not a verified final-tree diff.",
        "traces": sessions.iter().map(|session| json!({
            "file": session.file, "turns": session.turns.len(), "counts": session.coverage,
        })).collect::<Vec<_>>()
    });
    if json_output {
        println!(
            "{}",
            json!({"root":root,"host":host.name(),"sessions":sessions.len(),"edits":edit_count,"coverage":coverage,"spendUsd":spend,"elapsedMs":started.elapsed().as_millis(),"drift":drift,"byRule":tallies,"editResults":edits,"turnResults":turns})
        );
    } else {
        println!(
            "Ordain replay: {} · {} sessions · {} edits · {:.1}s · about ${spend:.6}",
            host.label(),
            sessions.len(),
            edit_count,
            started.elapsed().as_secs_f64()
        );
        println!("By rule:");
        for tally in &tallies {
            println!(
                "  {:<32} {:?} broken {} flagged {} of {}{}",
                tally.rule,
                tally.phase,
                tally.broken,
                tally.flagged,
                tally.of,
                if tally.example.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", tally.example)
                }
            );
        }
        println!("Drift:");
        for row in &drift {
            println!(
                "  {:<20} {} broken of {} ({:.1}%)",
                row.label,
                row.broken,
                row.edits,
                row.rate * 100.0
            );
        }
        println!("Coverage: {coverage}");
    }
    if incomplete {
        eprintln!(
            "Replay incomplete: no verdicts, unreconstructed edit evidence, or evaluation failures; not a clean result."
        );
    }
    Ok(if incomplete { 2 } else { 0 })
}

fn sessions_for(
    host: Host,
    root: &Path,
    paths: &[PathBuf],
    limit: usize,
) -> Result<Vec<ReplaySession>> {
    match host {
        Host::Hermes => Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "Hermes transcript replay is not supported",
        )),
        Host::Claude => {
            let defaults = [home_dir()
                .join(".claude/projects")
                .join(root.to_string_lossy().replace(['/', '.'], "-"))];
            let selected = if paths.is_empty() {
                defaults.as_slice()
            } else {
                paths
            };
            let mut sessions = Vec::new();
            let mut edits = 0;
            for target in selected {
                if sessions.len() >= limit {
                    break;
                }
                let metadata = fs::metadata(target).map_err(|error| {
                    OrdainError::new(ErrorCode::Io, format!("{}: {error}", target.display()))
                })?;
                if metadata.is_file() {
                    let session = parse_claude(target, MAX_REPLAY_EDITS - edits)?;
                    edits += edit_count(&session);
                    sessions.push(session);
                } else {
                    let remaining = limit - sessions.len();
                    let mut files = Vec::with_capacity(remaining.min(128));
                    for (scanned, entry) in fs::read_dir(target)?.enumerate() {
                        if scanned >= MAX_DISCOVERY_ENTRIES || files.len() >= remaining {
                            break;
                        }
                        let path = entry?.path();
                        if path.extension().is_some_and(|ext| ext == "jsonl") {
                            files.push(path);
                        }
                    }
                    files.sort();
                    for file in files {
                        let session = parse_claude(&file, MAX_REPLAY_EDITS - edits)?;
                        edits += edit_count(&session);
                        sessions.push(session);
                    }
                }
            }
            Ok(sessions)
        }
        Host::Codex => {
            let defaults = [home_dir().join(".codex/sessions")];
            let directories = if paths.is_empty() {
                defaults.as_slice()
            } else {
                paths
            };
            let mut seen = std::collections::HashSet::new();
            let mut sessions = Vec::new();
            let mut edits = 0;
            for (scanned, entry) in directories
                .iter()
                .flat_map(|directory| WalkDir::new(directory).follow_links(false))
                .enumerate()
            {
                if scanned >= MAX_DISCOVERY_ENTRIES {
                    return Err(OrdainError::new(
                        ErrorCode::InvalidArguments,
                        format!(
                            "replay discovery exceeded {MAX_DISCOVERY_ENTRIES} filesystem entries"
                        ),
                    ));
                }
                let entry =
                    entry.map_err(|error| OrdainError::new(ErrorCode::Io, error.to_string()))?;
                if sessions.len() >= limit {
                    break;
                }
                let name = entry.file_name().to_string_lossy();
                if entry.file_type().is_file()
                    && name.starts_with("rollout-")
                    && name.ends_with(".jsonl")
                {
                    if !seen.insert(fs::canonicalize(entry.path())?) {
                        continue;
                    }
                    let session = parse_codex(entry.path(), MAX_REPLAY_EDITS - edits)?;
                    edits += edit_count(&session);
                    sessions.push(session);
                }
            }
            sessions.sort_by(|a, b| a.file.cmp(&b.file));
            Ok(sessions)
        }
        Host::Opencode => {
            let db = paths
                .first()
                .cloned()
                .unwrap_or_else(|| home_dir().join(".local/share/opencode/opencode.db"));
            let mut sessions = parse_opencode(root, &db, limit, MAX_REPLAY_EDITS)?;
            sessions.sort_by(|a, b| a.file.cmp(&b.file));
            sessions.truncate(limit);
            Ok(sessions)
        }
    }
}

fn edit_count(session: &ReplaySession) -> usize {
    session.turns.iter().map(|turn| turn.edits.len()).sum()
}

fn replay_edit_limit() -> OrdainError {
    OrdainError::new(
        ErrorCode::InvalidArguments,
        format!("replay is limited to {MAX_REPLAY_EDITS} edits"),
    )
}

fn parse_claude(file: &Path, max_edits: usize) -> Result<ReplaySession> {
    let text = read_regular_text(file, MAX_REPLAY_TRANSCRIPT_BYTES, false).ok_or_else(|| {
        OrdainError::new(
            ErrorCode::Io,
            format!(
                "{} is not a regular UTF-8 transcript under 64 MiB",
                file.display()
            ),
        )
    })?;
    let mut turns = Vec::<ReplayTurn>::new();
    let mut uses = HashMap::<String, (String, Value, usize)>::new();
    let mut cwd = None;
    let mut turn_index = 0;
    let mut edit_total = 0;
    let mut coverage = TraceCoverage::default();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            coverage.malformed_records += 1;
            continue;
        };
        if cwd.is_none() {
            cwd = entry.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        }
        let kind = entry.get("type").and_then(Value::as_str);
        let message = entry.get("message");
        if kind == Some("user") && entry.get("isMeta").and_then(Value::as_bool) != Some(true) {
            if let Some(prompt) = prompt_text(message.and_then(|value| value.get("content"))) {
                if turns.len() >= MAX_REPLAY_EDITS {
                    return Err(replay_edit_limit());
                }
                turn_index += 1;
                turns.push(ReplayTurn {
                    index: turn_index,
                    prompt: Some(crate::paths::capture_task(&prompt)),
                    edits: Vec::new(),
                });
            }
            let response = entry.get("toolUseResult").cloned();
            for part in message
                .and_then(|value| value.get("content"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let Some(id) = part.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                let Some((name, input, turn)) = uses.remove(id) else {
                    continue;
                };
                if part.get("is_error").and_then(Value::as_bool) == Some(true) {
                    coverage.failed_edits += 1;
                    continue;
                }
                let payload =
                    hook_payload(file, cwd.as_deref(), &name, id, input, response.clone());
                if valid_edit_payload(&payload) {
                    let target = turns.iter().position(|item| item.index == turn);
                    if let Some(target) = target {
                        if edit_total >= max_edits {
                            return Err(replay_edit_limit());
                        }
                        turns[target].edits.push(ReplayEdit { input: payload });
                        edit_total += 1;
                    } else {
                        coverage.unreconstructable_edits += 1;
                    }
                } else {
                    coverage.unreconstructable_edits += 1;
                }
            }
        } else if kind == Some("assistant") {
            for part in message
                .and_then(|value| value.get("content"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = part.get("name").and_then(Value::as_str);
                if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                    coverage.tool_calls += 1;
                    if !matches!(name, Some("Edit" | "Write" | "MultiEdit")) {
                        coverage.other_tool_calls += 1;
                    }
                }
                if part.get("type").and_then(Value::as_str) == Some("tool_use")
                    && matches!(name, Some("Edit" | "Write" | "MultiEdit"))
                    && let Some(id) = part.get("id").and_then(Value::as_str)
                {
                    if uses.len() >= MAX_REPLAY_EDITS {
                        return Err(replay_edit_limit());
                    }
                    uses.insert(
                        id.into(),
                        (
                            name.unwrap().into(),
                            part.get("input").cloned().unwrap_or(Value::Null),
                            turn_index,
                        ),
                    );
                }
            }
        }
    }
    turns.retain(|turn| !turn.edits.is_empty());
    coverage.unmatched_edits = uses.len();
    Ok(ReplaySession {
        file: file.into(),
        cwd: cwd.unwrap_or_else(|| PathBuf::from(".")),
        turns,
        coverage,
    })
}

fn prompt_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return (!text.trim().is_empty()).then(|| text.into());
    }
    let parts = value.as_array()?;
    if parts
        .iter()
        .any(|part| part.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        return None;
    }
    let text = bounded_join(
        parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str)),
        MAX_TASK_CHARS,
    );
    (!text.trim().is_empty()).then_some(text)
}

fn bounded_join<'a>(values: impl IntoIterator<Item = &'a str>, limit: usize) -> String {
    let mut output = String::with_capacity(limit.min(1024));
    for value in values {
        let separator = usize::from(!output.is_empty());
        if output
            .len()
            .saturating_add(separator)
            .saturating_add(value.len())
            > limit
        {
            return crate::paths::INCOMPLETE_TASK.into();
        }
        if separator != 0 {
            output.push('\n');
        }
        output.push_str(value);
    }
    output
}

fn hook_payload(
    file: &Path,
    cwd: Option<&Path>,
    name: &str,
    id: &str,
    input: Value,
    response: Option<Value>,
) -> Value {
    let mut payload = json!({"session_id":file.file_stem().unwrap_or_default().to_string_lossy(),"cwd":cwd.unwrap_or(Path::new(".")).to_string_lossy(),"hook_event_name":"PostToolUse","tool_name":name,"tool_use_id":id,"tool_input":input});
    if let Some(response) = response {
        payload["tool_response"] = response;
    }
    payload
}

fn valid_edit_payload(payload: &Value) -> bool {
    let hunks = edits_from_recorded_payload(payload);
    !hunks.is_empty() && hunks.iter().all(|hunk| hunk.text.is_some())
}

fn parse_codex(file: &Path, max_edits: usize) -> Result<ReplaySession> {
    let text = read_regular_text(file, MAX_REPLAY_TRANSCRIPT_BYTES, false).ok_or_else(|| {
        OrdainError::new(
            ErrorCode::Io,
            format!(
                "{} is not a regular UTF-8 transcript under 64 MiB",
                file.display()
            ),
        )
    })?;
    let mut turns = Vec::<ReplayTurn>::new();
    let mut pending = HashMap::<String, (usize, String)>::new();
    let mut cwd = None;
    let mut index = 0;
    let mut edit_total = 0;
    let mut coverage = TraceCoverage::default();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            coverage.malformed_records += 1;
            continue;
        };
        let payload = &entry["payload"];
        if entry["type"] == "session_meta" && cwd.is_none() {
            cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from);
        }
        if entry["type"] != "response_item" {
            continue;
        }
        let tool_call = matches!(
            payload["type"].as_str(),
            Some("custom_tool_call" | "function_call")
        );
        let name = payload["name"]
            .as_str()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("");
        if tool_call {
            coverage.tool_calls += 1;
            if name != "apply_patch" {
                coverage.other_tool_calls += 1;
            }
        }
        if payload["type"] == "message" && payload["role"] == "user" {
            let text = bounded_join(
                payload
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "input_text")
                    .filter_map(|part| part.get("text").and_then(Value::as_str)),
                MAX_TASK_CHARS,
            );
            const INJECTED: &[&str] = &[
                "# AGENTS.md instructions",
                "<environment_context>",
                "<permissions instructions>",
                "<turn_aborted>",
                "\n# Files mentioned by the user",
            ];
            if !text.trim().is_empty() && !INJECTED.iter().any(|prefix| text.starts_with(prefix)) {
                if turns.len() >= MAX_REPLAY_EDITS {
                    return Err(replay_edit_limit());
                }
                index += 1;
                turns.push(ReplayTurn {
                    index,
                    prompt: Some(crate::paths::capture_task(&text)),
                    edits: Vec::new(),
                });
            }
        } else if tool_call && name == "apply_patch" {
            let arguments = payload
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            let command = payload.get("input").and_then(Value::as_str).or_else(|| {
                let arguments = arguments.as_ref()?;
                ["input", "patch", "command"]
                    .iter()
                    .find_map(|name| arguments.get(name).and_then(Value::as_str))
            });
            if let (Some(id), Some(command)) =
                (payload.get("call_id").and_then(Value::as_str), command)
            {
                if pending.len() >= MAX_REPLAY_EDITS {
                    return Err(replay_edit_limit());
                }
                pending.insert(id.into(), (index, command.into()));
            } else {
                coverage.unreconstructable_edits += 1;
            }
        } else if matches!(
            payload["type"].as_str(),
            Some("custom_tool_call_output" | "function_call_output")
        ) && let Some(id) = payload.get("call_id").and_then(Value::as_str)
            && let Some((turn_index, command)) = pending.remove(id)
        {
            match patch_succeeded(payload.get("output").and_then(Value::as_str).unwrap_or("")) {
                Some(true) => {}
                Some(false) => {
                    coverage.failed_edits += 1;
                    continue;
                }
                None => {
                    coverage.unreconstructable_edits += 1;
                    continue;
                }
            }
            let value = json!({"session_id":file.file_stem().unwrap_or_default().to_string_lossy(),"cwd":cwd.as_deref().unwrap_or(Path::new(".")).to_string_lossy(),"hook_event_name":"PostToolUse","tool_name":"apply_patch","tool_use_id":id,"tool_input":{"command":command}});
            let target = turns.iter().position(|turn| turn.index == turn_index);
            if !valid_edit_payload(&value) {
                coverage.unreconstructable_edits += 1;
                continue;
            }
            if let Some(target) = target {
                if edit_total >= max_edits {
                    return Err(replay_edit_limit());
                }
                turns[target].edits.push(ReplayEdit { input: value });
                edit_total += 1;
            } else {
                coverage.unreconstructable_edits += 1;
            }
        }
    }
    turns.retain(|turn| !turn.edits.is_empty());
    coverage.unmatched_edits = pending.len();
    Ok(ReplaySession {
        file: file.into(),
        cwd: cwd.unwrap_or_else(|| PathBuf::from(".")),
        turns,
        coverage,
    })
}

// Only confirmed outcomes may enter historical evidence. A missing/unknown result
// is not success, including when a session was interrupted after issuing a patch.
fn patch_succeeded(output: &str) -> Option<bool> {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        if let Some(code) = value
            .pointer("/metadata/exit_code")
            .or_else(|| value.get("exit_code"))
            .and_then(Value::as_i64)
        {
            return Some(code == 0);
        }
        if let Some(text) = value.get("output").and_then(Value::as_str) {
            return patch_succeeded(text);
        }
    }
    let text = output.trim().to_ascii_lowercase();
    if text.starts_with("success") || text.starts_with("done!") {
        Some(true)
    } else if text.starts_with("apply_patch failed")
        || text.starts_with("apply_patch verification failed")
        || text.starts_with("error")
    {
        Some(false)
    } else {
        None
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenCodeRow {
    session_id: String,
    directory: String,
    message_id: String,
    role: String,
    part: String,
}

fn parse_opencode(
    root: &Path,
    db: &Path,
    max_sessions: usize,
    max_edits: usize,
) -> Result<Vec<ReplaySession>> {
    if !db.exists() {
        return Err(OrdainError::new(
            ErrorCode::RubricMissing,
            format!("no OpenCode database at {}", db.display()),
        ));
    }
    let sql = "select s.id as sessionId, s.directory as directory, m.id as messageId, json_extract(m.data, '$.role') as role, p.data as part from part p join message m on m.id = p.message_id join session s on s.id = m.session_id order by s.id, m.time_created, m.id, p.id limit 50001";
    let output = run_process(
        "sqlite3",
        &[
            "-json".into(),
            "-readonly".into(),
            db.to_string_lossy().into_owned(),
            sql.into(),
        ],
        ProcessOptions {
            cwd: None,
            timeout: Duration::from_secs(30),
            input: None,
            env: None,
            max_output: 64 * 1024 * 1024,
            inherit_output: false,
        },
    );
    if output.status.is_none() {
        return Err(OrdainError::new(
            ErrorCode::GitUnavailable,
            "sqlite3 is not on PATH, and OpenCode sessions live in SQLite",
        ));
    }
    if output.status != Some(0) {
        return Err(OrdainError::new(
            ErrorCode::GitUnavailable,
            format!("sqlite3 could not read {}", db.display()),
        ));
    }
    if output.stdout_truncated {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "OpenCode replay query exceeded the 64 MiB output limit",
        ));
    }
    let rows: Vec<OpenCodeRow> = if output.stdout.iter().all(u8::is_ascii_whitespace) {
        Vec::new()
    } else {
        serde_json::from_slice(&output.stdout).map_err(|_| {
            OrdainError::new(ErrorCode::GitUnavailable, "sqlite3 returned invalid JSON")
        })?
    };
    if rows.len() > 50_000 {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "OpenCode replay exceeds the 50,000-row discovery limit",
        ));
    }
    let mut sessions = HashMap::<String, (ReplaySession, usize, Option<String>)>::new();
    let mut edits = 0;
    for row in rows {
        let directory = PathBuf::from(&row.directory);
        if !inside(root, &directory) {
            continue;
        }
        if !sessions.contains_key(&row.session_id) && sessions.len() >= max_sessions {
            continue;
        }
        let state = sessions.entry(row.session_id.clone()).or_insert_with(|| {
            (
                ReplaySession {
                    file: PathBuf::from(&row.session_id),
                    cwd: directory.clone(),
                    turns: Vec::new(),
                    coverage: TraceCoverage::default(),
                },
                0,
                None,
            )
        });
        let Ok(part) = serde_json::from_str::<Value>(&row.part) else {
            state.0.coverage.malformed_records += 1;
            continue;
        };
        if part["type"] == "tool" {
            state.0.coverage.tool_calls += 1;
            if !matches!(part["tool"].as_str(), Some("edit" | "write")) {
                state.0.coverage.other_tool_calls += 1;
            } else if part["state"]["status"] == "error" {
                state.0.coverage.failed_edits += 1;
            } else if part["state"]["status"] != "completed" {
                state.0.coverage.unmatched_edits += 1;
            }
        }
        if row.role == "user" {
            if state.2.as_deref() != Some(&row.message_id) {
                state.2 = Some(row.message_id);
                state.1 += 1;
                state.0.turns.push(ReplayTurn {
                    index: state.1,
                    prompt: None,
                    edits: Vec::new(),
                });
            }
            if part["type"] == "text"
                && let Some(text) = part.get("text").and_then(Value::as_str)
                && !text.trim().is_empty()
                && !text.starts_with("Ordain:")
                && let Some(turn) = state.0.turns.last_mut()
            {
                turn.prompt = Some(crate::paths::capture_task(text));
            }
        } else if part["type"] == "tool" && part["state"]["status"] == "completed" {
            let Some(tool) = part.get("tool").and_then(Value::as_str) else {
                continue;
            };
            if !matches!(tool, "edit" | "write") {
                continue;
            }
            let input = &part["state"]["input"];
            let Some(file) = input.get("filePath").and_then(Value::as_str) else {
                continue;
            };
            let file = if Path::new(file).is_absolute() {
                PathBuf::from(file)
            } else {
                directory.join(file)
            };
            let base = json!({"session_id":row.session_id,"cwd":row.directory,"hook_event_name":"PostToolUse","tool_use_id":part.get("callID").and_then(Value::as_str).unwrap_or("")});
            let mut payload = base;
            if tool == "edit" {
                payload["tool_name"] = json!("Edit");
                payload["tool_input"] = json!({"file_path":file,"old_string":input.get("oldString").and_then(Value::as_str).unwrap_or(""),"new_string":input.get("newString").and_then(Value::as_str).unwrap_or(""),"replace_all":input.get("replaceAll").and_then(Value::as_bool).unwrap_or(false)});
            } else {
                payload["tool_name"] = json!("Write");
                payload["tool_input"] = json!({"file_path":file,"content":input.get("content").and_then(Value::as_str).unwrap_or("")});
                payload["tool_response"] = part["state"]["metadata"].clone();
            }
            if !valid_edit_payload(&payload) {
                state.0.coverage.unreconstructable_edits += 1;
                continue;
            }
            if state.0.turns.is_empty() {
                state.0.turns.push(ReplayTurn {
                    index: 0,
                    prompt: None,
                    edits: Vec::new(),
                });
            }
            state
                .0
                .turns
                .last_mut()
                .unwrap()
                .edits
                .push(ReplayEdit { input: payload });
            edits += 1;
            if edits > max_edits {
                return Err(replay_edit_limit());
            }
        }
    }
    Ok(sessions
        .into_values()
        .map(|(mut session, _, _)| {
            session.turns.retain(|turn| !turn.edits.is_empty());
            session
        })
        .collect())
}

fn replay_sessions(
    sessions: &[ReplaySession],
    context: &EvaluationContext,
    concurrency: usize,
    keep_diffs: bool,
    progress: bool,
) -> (Vec<EditResult>, Vec<TurnResult>, usize) {
    // Parallelize independent turns, not edits within a turn: evidence order must
    // not depend on provider latency. Per-file aggregation stays bounded as we go.
    let jobs: Vec<_> = sessions
        .iter()
        .flat_map(|session| session.turns.iter().map(move |turn| (session, turn)))
        .collect();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let omitted = AtomicUsize::new(0);
    let results = Mutex::new((Vec::new(), Vec::new()));
    thread::scope(|scope| {
        for _ in 0..concurrency.min(jobs.len()) {
            scope.spawn(|| {
                while let Some(&(session, turn)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let session_name = session.file.to_string_lossy().into_owned();
                    let session_root = find_repo_root(&session.cwd);
                    let mut turn_files = std::collections::BTreeMap::<String, String>::new();
                    let mut edits = Vec::new();
                    for edit in &turn.edits {
                        for hunk in edits_from_recorded_payload(&edit.input) {
                            let relative = relative_to_root(&session_root, &hunk.file_path)
                                .filter(|path| !is_excluded_path(path));
                            let Some((relative, text)) = relative
                                .zip(hunk.text)
                                .filter(|(_, text)| !text.trim().is_empty())
                            else {
                                omitted.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };
                            let file_diff = FileDiff {
                                file: relative.clone(),
                                text: bound_state(&text, 24_000).0,
                            };
                            append_bounded(
                                turn_files.entry(relative.clone()).or_default(),
                                &file_diff.text,
                                8_000,
                            );
                            let outcome = context.evaluate(CheckRequest {
                                phase: Phase::Edit,
                                file_diffs: std::slice::from_ref(&file_diff),
                                task: turn.prompt.as_deref(),
                                timeout: Duration::from_secs(8),
                                retries: 2,
                            });
                            edits.push(EditResult {
                                session: session_name.clone(),
                                turn: turn.index,
                                file: relative,
                                error: failures_text(&outcome.failures),
                                verdicts: outcome.verdicts,
                                cost_usd: outcome.usage.cost_usd.unwrap_or(0.0),
                                diff: keep_diffs.then_some(file_diff.text),
                                task: keep_diffs.then(|| turn.prompt.clone()).flatten(),
                            });
                        }
                    }
                    let diffs: Vec<_> = turn_files
                        .into_iter()
                        .map(|(file, text)| FileDiff { file, text })
                        .collect();
                    let turn_result = if diffs.is_empty() {
                        None
                    } else {
                        let outcome = context.evaluate(CheckRequest {
                            phase: Phase::Turn,
                            file_diffs: &diffs,
                            task: turn.prompt.as_deref(),
                            timeout: Duration::from_secs(15),
                            retries: 2,
                        });
                        Some(TurnResult {
                            session: session_name,
                            turn: turn.index,
                            files: diffs.into_iter().map(|diff| diff.file).collect(),
                            error: failures_text(&outcome.failures),
                            verdicts: outcome.verdicts,
                            cost_usd: outcome.usage.cost_usd.unwrap_or(0.0),
                        })
                    };
                    let mut results = results.lock().unwrap();
                    results.0.extend(edits);
                    results.1.extend(turn_result);
                    drop(results);
                    let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if progress {
                        eprintln!("replay: {completed} of {} turns", jobs.len());
                    }
                }
            });
        }
    });
    let (edits, turns) = results.into_inner().unwrap();
    (edits, turns, omitted.into_inner())
}

fn append_bounded(output: &mut String, text: &str, limit: usize) {
    if output.len() >= limit {
        return;
    }
    if !output.is_empty() {
        output.push('\n');
    }
    // Keep the same visible truncation marker used by individual edit checks.
    output.push_str(&bound_state(text, limit.saturating_sub(output.len())).0);
}

fn failures_text(failures: &[crate::check::CheckFailure]) -> Option<String> {
    (!failures.is_empty()).then(|| {
        failures
            .iter()
            .map(|failure| format!("{}: {}", failure.code.as_str(), failure.message))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

fn drift_by_turn(edits: &[EditResult]) -> Vec<DriftRow> {
    [
        ("turns 1 to 5", 1, 5),
        ("turns 6 to 15", 6, 15),
        ("turns 16 and later", 16, usize::MAX),
    ]
    .into_iter()
    .map(|(label, from, to)| {
        let items = edits
            .iter()
            .filter(|result| {
                result.error.is_none()
                    && !result.verdicts.is_empty()
                    && result.turn >= from
                    && result.turn <= to
            })
            .collect::<Vec<_>>();
        let broken = items
            .iter()
            .filter(|result| {
                result
                    .verdicts
                    .iter()
                    .any(|verdict| verdict.band == Band::Act)
            })
            .count();
        DriftRow {
            label,
            edits: items.len(),
            broken,
            rate: if items.is_empty() {
                0.0
            } else {
                broken as f64 / items.len() as f64
            },
        }
    })
    .collect()
}

fn tally_rules(edits: &[EditResult], turns: &[TurnResult], rules: &[Rule]) -> Vec<RuleTally> {
    let mut rows = Vec::new();
    for rule in rules {
        if !matches!(rule.check, Check::Model { .. }) {
            continue;
        }
        let phase = rule.when.unwrap_or(Phase::Edit);
        let values: Vec<(&str, &[Verdict])> = if phase == Phase::Edit {
            edits
                .iter()
                .map(|result| (result.file.as_str(), result.verdicts.as_slice()))
                .collect()
        } else {
            turns
                .iter()
                .map(|result| {
                    (
                        result.files.first().map(String::as_str).unwrap_or(""),
                        result.verdicts.as_slice(),
                    )
                })
                .collect()
        };
        let mut row = RuleTally {
            rule: rule.id.clone(),
            phase,
            broken: 0,
            flagged: 0,
            of: 0,
            example: String::new(),
        };
        for (file, verdicts) in values {
            if let Some(verdict) = verdicts.iter().find(|verdict| verdict.rule_id == rule.id) {
                row.of += 1;
                if verdict.band == Band::Act {
                    row.broken += 1;
                    if row.example.is_empty() {
                        row.example = file.into()
                    }
                } else if verdict.band == Band::Flag {
                    row.flagged += 1
                }
            }
        }
        if row.of > 0 {
            rows.push(row)
        }
    }
    rows.sort_by(|a, b| b.broken.cmp(&a.broken).then(b.flagged.cmp(&a.flagged)));
    rows
}

fn inside(root: &Path, directory: &Path) -> bool {
    directory.strip_prefix(root).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn claude_replay_uses_successful_recorded_edits_not_current_files() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("a.rs");
        fs::write(&target, "TODAYS_UNRELATED_CONTENT").unwrap();
        let trace = dir.path().join("claude.jsonl");
        let lines = [
            json!({"cwd":dir.path(),"type":"user","message":{"content":"first turn"}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","name":"Edit","id":"edit","input":{"file_path":target,"old_string":"old","new_string":"new"}},
                {"type":"tool_use","name":"Write","id":"failed","input":{"file_path":target,"content":"never written"}},
                {"type":"tool_use","name":"Write","id":"unknown","input":{"file_path":target,"content":"unknown prior state"}},
                {"type":"tool_use","name":"Edit","id":"pending","input":{"file_path":target,"old_string":"x","new_string":"y"}},
                {"type":"tool_use","name":"Bash","id":"shell","input":{"command":"true"}}
            ]}}),
            json!({"type":"user","message":{"content":"second turn"}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"edit","is_error":false}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"failed","is_error":true}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"unknown","is_error":false}]}}),
        ];
        let mut text = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        text.push_str("\n{invalid JSON");
        fs::write(&trace, text).unwrap();
        let session = parse_claude(&trace, 10).unwrap();
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].index, 1);
        let hunks = edits_from_recorded_payload(&session.turns[0].edits[0].input);
        assert!(hunks[0].original.is_none());
        assert!(
            !hunks[0]
                .text
                .as_ref()
                .unwrap()
                .contains("TODAYS_UNRELATED_CONTENT")
        );
        assert_eq!(session.coverage.tool_calls, 5);
        assert_eq!(session.coverage.failed_edits, 1);
        assert_eq!(session.coverage.unmatched_edits, 1);
        assert_eq!(session.coverage.unreconstructable_edits, 1);
        assert_eq!(session.coverage.malformed_records, 1);
        assert_eq!(session.coverage.other_tool_calls, 1);
    }

    #[test]
    fn codex_parser_skips_failed_patches_and_injected_context() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("rollout-fixture.jsonl");
        let lines = [
            json!({"type":"session_meta","payload":{"cwd":"/r"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions..."}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"do it"}]}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"1","input":"*** Begin Patch\n*** Add File: a.rs\n+x\n*** End Patch"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"1","output":"Success"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"2","input":"*** Begin Patch\n*** Add File: b.rs\n+x\n*** End Patch"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"2","output":"apply_patch failed"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"functions.apply_patch","call_id":"structured","arguments":json!({"input":"*** Begin Patch\n*** Add File: b.rs\n+safe\n*** End Patch"}).to_string()}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"structured","output":json!({"metadata":{"exit_code":0},"output":"applied"}).to_string()}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"shell","arguments":"{}"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"pending","input":"*** Begin Patch\n*** Add File: c.rs\n+unknown\n*** End Patch"}}),
        ];
        fs::write(
            &file,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let session = parse_codex(&file, MAX_REPLAY_EDITS).unwrap();
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].edits.len(), 2);
        assert_eq!(session.turns[0].prompt.as_deref(), Some("do it"));
        assert_eq!(session.coverage.failed_edits, 1);
        assert_eq!(session.coverage.unmatched_edits, 1);
        assert_eq!(session.coverage.other_tool_calls, 1);
        assert_eq!(patch_succeeded(""), None);
        assert_eq!(
            patch_succeeded(r#"{"metadata":{"exit_code":1}}"#),
            Some(false)
        );
    }
}
