use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use serde_json::json;

use crate::check::{CallCounts, CheckFailure, CheckRequest, EvaluationContext};
use crate::credentials::{Credentials, NO_KEY_HINT, find_credentials};
use crate::error::{ErrorCode, OrdainError, Result};
use crate::git::recent_history;
use crate::model::{Calibration, CalibrationVerdict, Check, Phase, RuleStatus, Thresholds};
use crate::paths::{find_repo_root, global_rubric_path, home_dir, rubric_path};
use crate::rubric::{require_valid, write_rubric};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    hunks: usize,
    median: f64,
    min: f64,
    max: f64,
    fired: usize,
    verdict: CalibrationVerdict,
}

const MAX_CALIBRATION_HUNKS: usize = 200;
const MAX_CALIBRATION_COMMITS: usize = 60;

pub fn run(
    global: bool,
    presets: bool,
    want_hunks: usize,
    want_commits: usize,
    json_output: bool,
) -> Result<i32> {
    if want_hunks > MAX_CALIBRATION_HUNKS || want_commits > MAX_CALIBRATION_COMMITS {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            format!(
                "calibration is limited to {MAX_CALIBRATION_HUNKS} hunks and {MAX_CALIBRATION_COMMITS} commits"
            ),
        ));
    }
    let repo_root = find_repo_root(&std::env::current_dir()?);
    let file = if presets {
        crate::presets::path(&repo_root)
    } else if global {
        global_rubric_path()
    } else {
        rubric_path(&repo_root)
    };
    let preset_file = presets
        .then(|| crate::presets::read(&repo_root))
        .transpose()?;
    let mut rubric = require_valid(&file)?;
    if presets {
        crate::presets::validate(&rubric)?;
        for rule in &mut rubric.rules {
            rule.origin = Some(crate::model::Origin::Preset);
        }
    }
    let thresholds = rubric.thresholds.unwrap_or_default();
    let mut rules = rubric
        .rules
        .iter()
        .filter(|rule| {
            matches!(rule.check, Check::Model { .. }) && rule.status != RuleStatus::Disabled
        })
        .cloned()
        .collect::<Vec<_>>();
    if rules.is_empty() {
        if json_output {
            println!(
                "{}",
                json!({"root":repo_root,"rows":[],"calls":CallCounts::default(),"spendUsd":0})
            );
        } else {
            println!("No model-checked rules to calibrate.");
        }
        return Ok(0);
    }
    for rule in &mut rules {
        rule.status = RuleStatus::Active;
    }
    let (hunks, commits) = recent_history(&repo_root, want_hunks, want_commits)?;
    if hunks.is_empty() && commits.is_empty() {
        if json_output {
            println!(
                "{}",
                json!({"root":repo_root,"rows":[],"calls":CallCounts::default(),"spendUsd":0,"skipped":"no usable history"})
            );
        } else {
            println!("No usable history in this repository yet; calibration skipped.");
        }
        return Ok(0);
    }
    if find_credentials(&repo_root) == Credentials::None {
        return Err(OrdainError::new(ErrorCode::NoApiKey, NO_KEY_HINT));
    }
    let context = EvaluationContext::new(&repo_root, &rules, thresholds)?;
    let mut samples = HashMap::<String, Vec<f64>>::new();
    let mut spend = 0.0;
    let mut calls = CallCounts::default();
    let mut failures = Vec::<CheckFailure>::new();
    for (index, hunk) in hunks.iter().enumerate() {
        if !json_output {
            eprintln!(
                "calibrate: hunk {} of {} {}",
                index + 1,
                hunks.len(),
                hunk.file
            );
        }
        let outcome = context.evaluate_revision(
            CheckRequest {
                phase: Phase::Edit,
                file_diffs: &[crate::model::FileDiff {
                    file: hunk.file.clone(),
                    text: hunk.text.clone(),
                }],
                task: Some(&hunk.subject),
                timeout: Duration::from_secs(8),
                retries: 2,
            },
            &hunk.revision,
        );
        record(&mut samples, &outcome.verdicts);
        spend += outcome.usage.cost_usd.unwrap_or(0.0);
        add_calls(&mut calls, &outcome.calls);
        failures.extend(outcome.failures);
    }
    for (index, commit) in commits.iter().enumerate() {
        if !json_output {
            eprintln!(
                "calibrate: commit {} of {} {}",
                index + 1,
                commits.len(),
                commit.subject
            );
        }
        let outcome = context.evaluate_revision(
            CheckRequest {
                phase: Phase::Turn,
                file_diffs: &commit.file_diffs,
                task: Some(&commit.subject),
                timeout: Duration::from_secs(15),
                retries: 2,
            },
            &commit.revision,
        );
        record(&mut samples, &outcome.verdicts);
        spend += outcome.usage.cost_usd.unwrap_or(0.0);
        add_calls(&mut calls, &outcome.calls);
        failures.extend(outcome.failures);
    }
    if !failures.is_empty() {
        if json_output {
            println!(
                "{}",
                json!({"root":repo_root,"calls":calls,"spendUsd":spend,"errors":failures,"rubricUpdated":false})
            );
        } else {
            eprintln!(
                "Calibration failed after {} request attempt(s); the existing rubric was not changed.",
                calls.attempts
            );
            for failure in &failures {
                eprintln!("  {}: {}", failure.code.as_str(), failure.message);
            }
        }
        return Ok(2);
    }
    let at = Utc::now().to_rfc3339();
    let mut rows = Vec::new();
    for rule in &mut rubric.rules {
        if !matches!(rule.check, Check::Model { .. }) || rule.status == RuleStatus::Disabled {
            continue;
        }
        let summary = summarize(
            samples.get(&rule.id).map(Vec::as_slice).unwrap_or_default(),
            thresholds,
        );
        rule.status = match summary.verdict {
            CalibrationVerdict::Weak => RuleStatus::Weak,
            CalibrationVerdict::Noisy => RuleStatus::Noisy,
            CalibrationVerdict::Decisive => RuleStatus::Active,
            CalibrationVerdict::Skipped => rule.status,
        };
        rule.calibration = Some(Calibration {
            at: at.clone(),
            hunks: summary.hunks,
            median: summary.median,
            min: summary.min,
            max: summary.max,
            fired: summary.fired,
            verdict: summary.verdict,
        });
        rows.push(json!({"id":rule.id,"when":rule.when,"status":rule.status,"summary":summary}));
    }
    if let Some(file) = preset_file {
        crate::presets::write(&file, &rubric)?;
    } else {
        write_rubric(&file, &rubric)?;
    }
    let weak = rubric
        .rules
        .iter()
        .filter(|rule| rule.status == RuleStatus::Weak)
        .map(|rule| rule.id.clone())
        .collect::<Vec<_>>();
    let noisy = rubric
        .rules
        .iter()
        .filter(|rule| rule.status == RuleStatus::Noisy)
        .map(|rule| rule.id.clone())
        .collect::<Vec<_>>();
    let root = if global {
        home_dir()
    } else {
        repo_root.clone()
    };
    if json_output {
        println!(
            "{}",
            json!({"root":root,"file":file,"hunks":hunks.len(),"commits":commits.len(),"calls":calls,"spendUsd":spend,"rows":rows,"weak":weak,"noisy":noisy})
        );
    } else {
        println!(
            "Ordain calibrate: {} rules, {} request attempts, about ${spend:.6}",
            rows.len(),
            calls.attempts
        );
        for row in rows {
            println!(
                "  {:<32} {:<8} median {:.2} range {:.2}..{:.2}",
                row["id"].as_str().unwrap_or(""),
                row["summary"]["verdict"].as_str().unwrap_or(""),
                row["summary"]["median"].as_f64().unwrap_or(0.0),
                row["summary"]["min"].as_f64().unwrap_or(0.0),
                row["summary"]["max"].as_f64().unwrap_or(0.0)
            );
        }
        if !weak.is_empty() {
            println!("Weak and switched off: {}", weak.join(", "));
        }
        if !noisy.is_empty() {
            println!("Noisy and switched off: {}", noisy.join(", "));
        }
    }
    Ok(0)
}

fn add_calls(total: &mut CallCounts, next: &CallCounts) {
    total.groups += next.groups;
    total.attempts += next.attempts;
    total.succeeded += next.succeeded;
}

fn record(samples: &mut HashMap<String, Vec<f64>>, verdicts: &[crate::model::Verdict]) {
    for verdict in verdicts {
        samples
            .entry(verdict.rule_id.clone())
            .or_default()
            .push(verdict.probability);
    }
}

fn summarize(probabilities: &[f64], thresholds: Thresholds) -> Summary {
    let mut values = probabilities.to_vec();
    values.sort_by(f64::total_cmp);
    let median = if values.is_empty() {
        0.0
    } else if values.len().is_multiple_of(2) {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    } else {
        values[values.len() / 2]
    };
    let min = values.first().copied().unwrap_or(0.0);
    let max = values.last().copied().unwrap_or(0.0);
    let fired = values
        .iter()
        .filter(|value| **value >= thresholds.act)
        .count();
    let verdict = if values.len() < 5 {
        CalibrationVerdict::Skipped
    } else if fired as f64 / values.len() as f64 >= 0.6 {
        CalibrationVerdict::Noisy
    } else if max < 0.7 && median >= 0.25 {
        CalibrationVerdict::Weak
    } else {
        CalibrationVerdict::Decisive
    };
    Summary {
        hunks: values.len(),
        median,
        min,
        max,
        fired,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_distinguishes_weak_noisy_and_decisive() {
        let t = Thresholds::default();
        assert_eq!(summarize(&[0.4; 5], t).verdict, CalibrationVerdict::Weak);
        assert_eq!(summarize(&[0.9; 5], t).verdict, CalibrationVerdict::Noisy);
        assert_eq!(
            summarize(&[0.01, 0.02, 0.03, 0.9, 0.95], t).verdict,
            CalibrationVerdict::Decisive
        );
        assert_eq!(summarize(&[0.4; 4], t).verdict, CalibrationVerdict::Skipped);
    }
}
