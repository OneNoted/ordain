use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::check::{CheckOutcome, CheckRequest, EvaluationContext, loudest_verdicts};
use crate::credentials::{Credentials, NO_KEY_HINT, find_credentials};
use crate::diff::file_as_chunks;
use crate::error::{ErrorCode, OrdainError, Result};
use crate::git::{is_git_repo, list_repo_files};
use crate::model::{Band, FileDiff, Phase, Verdict};
use crate::paths::{MAX_DIFF_INPUT_CHARS, find_repo_root, read_regular_text};
use crate::rubric::load_rules;

const AUDIT_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK_LINES: usize = 150;
const MAX_AUDIT_CONCURRENCY: usize = 16;
const MAX_AUDIT_FILES: usize = 10_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditFileResult {
    file: String,
    verdicts: Vec<Verdict>,
    rules: usize,
    latency_ms: u64,
    cost_usd: f64,
    chunks: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunks_failed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug)]
struct Judged {
    outcomes: Vec<CheckOutcome>,
    failed: Vec<(String, String)>,
    chunks: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuleTally {
    rule_id: String,
    broken: Vec<String>,
    flagged: Vec<String>,
    checked: usize,
}

pub fn run(
    paths: &[String],
    concurrency: usize,
    max_files: Option<usize>,
    show_all: bool,
    json_output: bool,
) -> Result<i32> {
    if concurrency == 0 || concurrency > MAX_AUDIT_CONCURRENCY {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            format!("audit concurrency must be between 1 and {MAX_AUDIT_CONCURRENCY}"),
        ));
    }
    if max_files.is_some_and(|max| max == 0 || max > MAX_AUDIT_FILES) {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            format!("--max-files must be between 1 and {MAX_AUDIT_FILES}"),
        ));
    }
    let root = find_repo_root(&std::env::current_dir()?);
    if find_credentials(&root) == Credentials::None {
        return Err(OrdainError::new(ErrorCode::NoApiKey, NO_KEY_HINT));
    }
    if !is_git_repo(&root) {
        return Err(OrdainError::new(
            ErrorCode::GitUnavailable,
            "audit walks the files git knows about, and this is not a git repository",
        ));
    }
    let loaded = load_rules(&root);
    if loaded.rules.is_empty() {
        return Err(OrdainError::new(
            ErrorCode::RubricMissing,
            "no rubric here or in $XDG_CONFIG_HOME/ordain; run ordain compile first",
        ));
    }
    let context = EvaluationContext::new(&root, &loaded.rules, loaded.thresholds)?;
    let candidates = list_repo_files(&root, paths)?;
    let (mut files, too_big, out_of_scope) = auditable_files(&root, &candidates, &context);
    if max_files.is_none() && files.len() > MAX_AUDIT_FILES {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            format!(
                "audit matched {} files; narrow the paths or set --max-files (maximum {MAX_AUDIT_FILES})",
                files.len()
            ),
        ));
    }
    if let Some(max) = max_files {
        files.truncate(max);
    }
    let started = Instant::now();
    let results = audit_files(&root, &files, &context, concurrency.max(1), !json_output);
    let tallies = tally_by_rule(&results);
    let spend: f64 = results.iter().map(|result| result.cost_usd).sum();
    let broken = tallies.iter().any(|tally| !tally.broken.is_empty());
    let failed = results.iter().any(|result| result.error.is_some());
    if json_output {
        println!(
            "{}",
            json!({
                "root":root,"files":files.len(),"skipped":{"tooBig":too_big,"outOfScope":out_of_scope},
                "spendUsd":spend,"elapsedMs":started.elapsed().as_millis(),"byRule":tallies,"byFile":results
            })
        );
    } else {
        println!(
            "Ordain audit: {} files, {} out of scope, {} too large, {:.1}s, about ${spend:.6}",
            files.len(),
            out_of_scope,
            too_big.len(),
            started.elapsed().as_secs_f64()
        );
        println!("By rule:");
        for tally in &tallies {
            if show_all || !tally.broken.is_empty() || !tally.flagged.is_empty() {
                println!(
                    "  {:<32} broken {:>3}, flagged {:>3}, checked {:>3}",
                    tally.rule_id,
                    tally.broken.len(),
                    tally.flagged.len(),
                    tally.checked
                );
            }
        }
        println!("By file:");
        for result in &results {
            let acts = result
                .verdicts
                .iter()
                .filter(|verdict| verdict.band == Band::Act)
                .count();
            let flags = result
                .verdicts
                .iter()
                .filter(|verdict| verdict.band == Band::Flag)
                .count();
            if show_all || acts > 0 || flags > 0 || result.error.is_some() {
                println!(
                    "  {:<48} broken {acts}, flagged {flags}{}",
                    result.file,
                    result
                        .error
                        .as_ref()
                        .map_or(String::new(), |error| format!(" · ERROR: {error}"))
                );
            }
        }
    }
    Ok(if failed { 2 } else { i32::from(broken) })
}

fn auditable_files(
    root: &Path,
    files: &[String],
    context: &EvaluationContext,
) -> (Vec<String>, Vec<String>, usize) {
    let base = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut kept = Vec::new();
    let mut too_big = Vec::new();
    let mut out_of_scope = 0;
    for file in files {
        if !context.has_applicable_rule(Phase::Edit, file) {
            out_of_scope += 1;
            continue;
        }
        let Ok(real) = fs::canonicalize(root.join(file)) else {
            continue;
        };
        if !real.starts_with(&base) || !real.is_file() {
            continue;
        }
        if fs::metadata(&real).is_ok_and(|metadata| metadata.len() > MAX_DIFF_INPUT_CHARS as u64) {
            too_big.push(file.clone());
            continue;
        }
        kept.push(file.clone());
    }
    (kept, too_big, out_of_scope)
}

fn audit_files(
    root: &Path,
    files: &[String],
    context: &EvaluationContext,
    concurrency: usize,
    progress: bool,
) -> Vec<AuditFileResult> {
    let queue = Arc::new(Mutex::new(
        files.iter().cloned().enumerate().collect::<Vec<_>>(),
    ));
    let results = Arc::new(Mutex::new(
        Vec::<(usize, AuditFileResult, Option<Judged>)>::new(),
    ));
    let done = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..concurrency.min(files.len().max(1)) {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let done = &done;
            scope.spawn(move || loop {
                let item = queue.lock().expect("audit queue poisoned").pop();
                let Some((index, file)) = item else { break };
                let real = root.join(&file);
                let (result, judged) = match read_regular_text(&real, MAX_DIFF_INPUT_CHARS as u64, false) {
                    Some(content) => {
                        let judged = judge(&file, &file_as_chunks(&content, CHUNK_LINES), context, 3);
                        (summarize(&file, &judged), Some(judged))
                    }
                    None => (AuditFileResult { file: file.clone(), verdicts: Vec::new(), rules: 0, latency_ms: 0, cost_usd: 0.0, chunks: 0, chunks_failed: None, error: Some("not a regular file inside the repository, or could not be read".into()) }, None),
                };
                results.lock().expect("audit results poisoned").push((index, result, judged));
                let count = done.fetch_add(1, Ordering::Relaxed) + 1;
                if progress { eprintln!("audit: {count} of {} files", files.len()); }
            });
        }
    });
    let mut with_judged = Arc::try_unwrap(results)
        .expect("audit results still shared")
        .into_inner()
        .expect("audit results poisoned");
    // Retry rejected chunks serially after the concurrent burst.
    for (_, result, judged) in &mut with_judged {
        let Some(judged) = judged else { continue };
        if judged.failed.is_empty() {
            continue;
        }
        let texts = judged
            .failed
            .iter()
            .map(|(text, _)| text.clone())
            .collect::<Vec<_>>();
        let retried = judge(&result.file, &texts, context, 3);
        judged.outcomes.extend(retried.outcomes);
        judged.failed = retried.failed;
        *result = summarize(&result.file, judged);
    }
    with_judged.sort_by_key(|(index, _, _)| *index);
    with_judged
        .into_iter()
        .map(|(_, result, _)| result)
        .collect()
}

fn judge(file: &str, chunks: &[String], context: &EvaluationContext, retries: usize) -> Judged {
    let mut outcomes = Vec::new();
    let mut failed = Vec::new();
    for text in chunks {
        let outcome = context.evaluate(CheckRequest {
            phase: Phase::Edit,
            file_diffs: &[FileDiff {
                file: file.into(),
                text: text.clone(),
            }],
            task: None,
            timeout: AUDIT_TIMEOUT,
            retries,
        });
        if !outcome.failures.is_empty() {
            failed.push((
                text.clone(),
                outcome
                    .failures
                    .iter()
                    .map(|failure| format!("{}: {}", failure.code.as_str(), failure.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        outcomes.push(outcome);
    }
    Judged {
        outcomes,
        failed,
        chunks: chunks.len(),
    }
}

fn summarize(file: &str, judged: &Judged) -> AuditFileResult {
    let error = judged.failed.last().map(|(_, error)| {
        format!(
            "{} of {} chunks not judged: {error}",
            judged.failed.len(),
            judged.chunks
        )
    });
    AuditFileResult {
        file: file.into(),
        verdicts: loudest_verdicts(
            judged
                .outcomes
                .iter()
                .flat_map(|outcome| outcome.verdicts.clone()),
        ),
        rules: judged
            .outcomes
            .first()
            .map_or(0, |outcome| outcome.model_rules.len()),
        latency_ms: judged
            .outcomes
            .iter()
            .map(|outcome| outcome.model_latency_ms)
            .sum(),
        cost_usd: judged
            .outcomes
            .iter()
            .map(|outcome| outcome.usage.cost_usd.unwrap_or(0.0))
            .sum(),
        chunks: judged.chunks,
        chunks_failed: (!judged.failed.is_empty()).then_some(judged.failed.len()),
        error,
    }
}

fn tally_by_rule(results: &[AuditFileResult]) -> Vec<RuleTally> {
    let mut tallies = HashMap::<String, RuleTally>::new();
    for result in results {
        for verdict in &result.verdicts {
            let tally = tallies
                .entry(verdict.rule_id.clone())
                .or_insert_with(|| RuleTally {
                    rule_id: verdict.rule_id.clone(),
                    broken: Vec::new(),
                    flagged: Vec::new(),
                    checked: 0,
                });
            tally.checked += 1;
            if verdict.band == Band::Act {
                tally.broken.push(result.file.clone());
            } else if verdict.band == Band::Flag {
                tally.flagged.push(result.file.clone());
            }
        }
    }
    let mut tallies = tallies.into_values().collect::<Vec<_>>();
    tallies.sort_by(|a, b| {
        b.broken
            .len()
            .cmp(&a.broken.len())
            .then_with(|| b.flagged.len().cmp(&a.flagged.len()))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    tallies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_preserve_line_offsets() {
        let text = (0..301)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = file_as_chunks(&text, 150);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[1].starts_with("@@ -0,0 +151,150 @@"));
    }
}
