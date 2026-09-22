use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::check::{CheckFailure, CheckOutcome, CheckRequest, EvaluationContext};
use crate::compile::{CompileTarget, compile_prompt, place_compile_skill, plan_compile};
use crate::credentials::{
    Credentials, GATEWAY_KEY_ENV, NO_KEY_HINT, TYPESAFE_KEY_ENV, find_credentials,
    project_env_path, save_key, user_env_path,
};
use crate::diff::split_diff;
use crate::error::{ErrorCode, OrdainError, Result};
use crate::events::read_events;
use crate::git::{is_ignored, working_tree_diff};
use crate::hosts::{Host, detect_hosts, install_host, uninstall_host};
use crate::model::{Band, Check, Event, FileDiff, Phase, RuleStatus};
use crate::paths::{
    MAX_DIFF_INPUT_CHARS, MAX_FILE_READ_BYTES, find_repo_root, global_dir, global_rubric_path,
    home_dir, ordain_dir, read_regular_text, rubric_path,
};
use crate::process::{ProcessOptions, run as run_process};
use crate::rubric::{
    RubricRead, bucket_counts, check_staleness, discover_global_sources, discover_project_sources,
    fill_source_hashes, find_lint_configs, load_rules, read_rubric, write_rubric,
};

pub fn login() -> Result<i32> {
    let root = current_root()?;
    let interactive = io::stdin().is_terminal();
    let provider = if interactive {
        choose(
            "Which key do you have?",
            &["TypeSafe API key", "Vercel AI Gateway key"],
        )?
    } else {
        0
    };
    let place = if interactive {
        choose(
            "Where should it live?",
            &["Every repo on this machine", "This repo only"],
        )?
    } else {
        0
    };
    let (name, prompt) = if provider == 0 {
        (TYPESAFE_KEY_ENV, "TypeSafe API key: ")
    } else {
        (GATEWAY_KEY_ENV, "Vercel AI Gateway key: ")
    };
    eprint!("{prompt}");
    io::stderr().flush()?;
    let key = if interactive {
        rpassword::read_password().map_err(OrdainError::from)?
    } else {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        line
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(OrdainError::new(ErrorCode::NoApiKey, "nothing was entered"));
    }
    let file = if place == 0 {
        user_env_path()
    } else {
        project_env_path(&root)
    };
    save_key(&file, name, key)?;
    println!(
        "{name} saved to {} (owner-only). Run ordain init next.",
        file.display()
    );
    if place == 1 && !is_ignored(&root, ".env.local") {
        eprintln!("warning: .env.local is not ignored by git; add it before committing");
    }
    Ok(0)
}

fn choose(title: &str, options: &[&str]) -> Result<usize> {
    eprintln!("{title}");
    for (index, option) in options.iter().enumerate() {
        eprintln!("  {}. {option}", index + 1);
    }
    eprint!("> ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    match line.trim().parse::<usize>() {
        Ok(value) if (1..=options.len()).contains(&value) => Ok(value - 1),
        _ => Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "invalid selection",
        )),
    }
}

pub fn integration_install(name: &str, project: bool, workspace: Option<&Path>) -> Result<i32> {
    let host = Host::parse(name)?;
    crate::hosts::validate_scope(host, project)?;
    if host == Host::Hermes && workspace.is_none() {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "Hermes requires --workspace /absolute/path/to/repository; select its profile with HERMES_HOME",
        ));
    }
    if host != Host::Hermes && workspace.is_some() {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "--workspace is only for Hermes",
        ));
    }
    let root = match workspace {
        Some(path) => fs::canonicalize(path)?,
        None => current_root()?,
    };
    let binary = std::env::current_exe()?;
    let installed = install_host(host, &root, project, &binary)?;
    let status = crate::hosts::integration_status(host, &root, project, &binary)?;
    if status.state != crate::hosts::IntegrationState::Current {
        return Err(OrdainError::new(
            ErrorCode::SettingsInvalid,
            "installation did not verify; run integration status",
        ));
    }
    println!(
        "{}: {} ({})",
        host.label(),
        installed.what,
        installed.target.display()
    );
    if let Some(message) = installed.afterwards {
        println!("{message}");
    }
    println!(
        "Project rubric and judge credentials are separate prerequisites; installation does not validate them."
    );
    Ok(0)
}

pub fn integration_status(name: Option<&str>, project: bool, json: bool) -> Result<i32> {
    let root = current_root()?;
    let binary = std::env::current_exe()?;
    let hosts = match name {
        Some(name) => vec![Host::parse(name)?],
        None => Host::ALL
            .into_iter()
            .filter(|h| !project || *h != Host::Hermes)
            .collect(),
    };
    let mut output = Vec::new();
    let mut invalid = false;
    for host in hosts {
        let item = match crate::hosts::integration_status(host, &root, project, &binary) {
            Ok(status) => serde_json::to_value(status)
                .map_err(|error| OrdainError::new(ErrorCode::Io, error.to_string()))?,
            Err(error) => {
                invalid = true;
                json!({"host": host.name(), "state": "error", "detail": error.message, "runtime": "not_verified"})
            }
        };
        if !json {
            println!(
                "{}: {} — {} (runtime not verified)",
                item["host"].as_str().unwrap_or(""),
                item["state"].as_str().unwrap_or(""),
                item.get("detail")
                    .or_else(|| item.get("target"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
        }
        output.push(item);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|error| OrdainError::new(ErrorCode::Io, error.to_string()))?
        );
    }
    Ok(if invalid { 2 } else { 0 })
}

pub fn init(names: &[String], project: bool) -> Result<i32> {
    let root = current_root()?;
    let credentials = find_credentials(&root);
    if credentials == Credentials::None {
        eprintln!("{NO_KEY_HINT}");
        return Ok(1);
    }
    let project_sources = discover_project_sources(&root);
    let global_sources = discover_global_sources();
    if project_sources.is_empty() && global_sources.is_empty() {
        eprintln!("No instruction files found. Add an AGENTS.md or CLAUDE.md first.");
        return Ok(1);
    }
    let mut hosts = selected_hosts(names)?;
    if names.is_empty() && project {
        hosts.retain(|host| *host != Host::Hermes);
    }
    for host in &hosts {
        crate::hosts::validate_scope(*host, project)?;
    }
    let binary = std::env::current_exe()?;
    let project_state = ordain_dir(&root);
    if fs::symlink_metadata(&project_state).is_ok_and(|metadata| !metadata.file_type().is_dir()) {
        return Err(OrdainError::new(
            ErrorCode::Io,
            format!(
                "{} must be a real directory, not a link or file",
                project_state.display()
            ),
        ));
    }
    fs::create_dir_all(&project_state)?;
    let ignore_path = project_state.join(".gitignore");
    let mut ignore_text = if ignore_path.exists() {
        read_regular_text(&ignore_path, MAX_FILE_READ_BYTES, false).ok_or_else(|| {
            OrdainError::new(
                ErrorCode::Io,
                format!("could not safely read {}", ignore_path.display()),
            )
        })?
    } else {
        String::new()
    };
    for entry in ["events.jsonl", "compile-skill.md"] {
        if !ignore_text.lines().any(|line| line.trim() == entry) {
            if !ignore_text.is_empty() && !ignore_text.ends_with('\n') {
                ignore_text.push('\n');
            }
            ignore_text.push_str(entry);
            ignore_text.push('\n');
        }
    }
    let mut ignore = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(ignore_path)?;
    ignore.write_all(ignore_text.as_bytes())?;
    let selftest_payload = serde_json::to_vec(&json!({
        "session_id":"ordain-init-selftest","cwd":root,"hook_event_name":"SessionStart","source":"startup"
    })).unwrap_or_default();
    let output = run_process(
        binary.to_string_lossy().as_ref(),
        &["__hook".into(), "session-start".into()],
        ProcessOptions {
            cwd: Some(&root),
            timeout: Duration::from_secs(15),
            input: Some(&selftest_payload),
            env: None,
            max_output: 1024 * 1024,
            inherit_output: false,
        },
    );
    if output.timed_out || output.status != Some(0) {
        return Err(OrdainError::new(
            ErrorCode::SettingsInvalid,
            format!(
                "the hook at {} did not run cleanly; nothing was enabled",
                binary.display()
            ),
        ));
    }
    println!(
        "{} key found in {}",
        credentials.label().unwrap_or("provider"),
        credentials.from().unwrap_or("unknown")
    );
    println!(
        "{} instruction files found",
        project_sources.len() + global_sources.len()
    );
    println!("hook self-test passed");
    for host in hosts {
        let installed = install_host(host, &root, project, &binary)?;
        println!(
            "{}: {} ({})",
            installed.host.label(),
            installed.what,
            installed.target.display()
        );
        if let Some(afterwards) = installed.afterwards {
            println!("{afterwards}");
        }
    }
    Ok(0)
}

fn selected_hosts(names: &[String]) -> Result<Vec<Host>> {
    if names.is_empty() {
        let found = detect_hosts();
        if found.is_empty() {
            return Err(OrdainError::new(
                ErrorCode::HostNotFound,
                "none of claude, codex, opencode is installed here; name one to install anyway",
            ));
        }
        return Ok(found);
    }
    let mut hosts = Vec::new();
    for name in names {
        let host = Host::parse(name)?;
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

pub fn uninstall(names: &[String], project: bool) -> Result<i32> {
    let root = current_root()?;
    let hosts = if names.is_empty() {
        Host::ALL
            .into_iter()
            .filter(|host| !project || *host != Host::Hermes)
            .collect()
    } else {
        selected_hosts(names)?
    };
    for host in &hosts {
        crate::hosts::validate_scope(*host, project)?;
    }
    let mut touched = Vec::new();
    for host in hosts {
        if uninstall_host(host, &root, project)? > 0 {
            touched.push(host.label());
        }
    }
    if touched.is_empty() {
        println!("No Ordain entries found.");
    } else {
        println!(
            "Removed Ordain from {}. Rubrics and credentials are untouched. Restart the host to unload already-running hooks.",
            touched.join(", ")
        );
    }
    Ok(0)
}

pub fn rubric(action: &str, global: bool) -> Result<i32> {
    if action != "validate" {
        return Err(OrdainError::new(
            ErrorCode::RubricInvalid,
            format!("unknown rubric command {action:?}; try: ordain rubric validate"),
        ));
    }
    let root = if global { home_dir() } else { current_root()? };
    let file = if global {
        global_rubric_path()
    } else {
        rubric_path(&root)
    };
    match read_rubric(&file) {
        RubricRead::Missing { .. } => Err(OrdainError::new(
            ErrorCode::RubricMissing,
            format!("{} does not exist yet", file.display()),
        )),
        RubricRead::Invalid { issues, .. } => {
            eprintln!("{} is invalid:", file.display());
            for issue in issues {
                eprintln!("- {issue}");
            }
            Ok(1)
        }
        RubricRead::Ok { mut rubric, .. } => {
            let missing = fill_source_hashes(&mut rubric, &root);
            let listed = rubric
                .sources
                .iter()
                .map(|source| source.path.as_str())
                .collect::<HashSet<_>>();
            let orphaned = rubric
                .rules
                .iter()
                .filter(|rule| !listed.contains(rule.source.path.as_str()))
                .map(|rule| format!("{} ({})", rule.id, rule.source.path))
                .collect::<Vec<_>>();
            write_rubric(&file, &rubric)?;
            let (buckets, statuses) = bucket_counts(&rubric.rules);
            println!("{}: {} rules", file.display(), rubric.rules.len());
            println!(
                "lint {} · model {} · deferred {} · unenforceable {}",
                buckets["lint"], buckets["model"], buckets["deferred"], buckets["unenforceable"]
            );
            println!(
                "statuses: {}",
                serde_json::to_string(&statuses).unwrap_or_default()
            );
            for path in &missing {
                eprintln!("missing source: {path}");
            }
            for rule in &orphaned {
                eprintln!("orphaned rule source: {rule}");
            }
            Ok(i32::from(!missing.is_empty() || !orphaned.is_empty()))
        }
    }
}

pub fn compile(print: bool, global: bool, tune: bool) -> Result<i32> {
    let root = current_root()?;
    let plan = plan_compile(&root);
    if !plan.invalid.is_empty() {
        eprintln!("Fix the rubric first:\n{}", plan.invalid.join("\n"));
        return Ok(1);
    }
    if plan.no_sources {
        return Err(OrdainError::new(
            ErrorCode::NoInstructionFiles,
            "found 0 instruction files; add an AGENTS.md",
        ));
    }
    let (targets, stats) = if tune {
        let file = if global {
            global_rubric_path()
        } else {
            rubric_path(&root)
        };
        let RubricRead::Ok { rubric, .. } = read_rubric(&file) else {
            return Err(OrdainError::new(
                ErrorCode::RubricMissing,
                "nothing to tune yet; compile first",
            ));
        };
        let candidates = if global {
            discover_global_sources()
        } else {
            discover_project_sources(&root)
        };
        let target_root = if global { home_dir() } else { root.clone() };
        let target = CompileTarget {
            which: if global { "global" } else { "project" },
            root: target_root.clone(),
            staleness: check_staleness(Some(&rubric), &candidates, &target_root),
            lint_configs: if global {
                Vec::new()
            } else {
                find_lint_configs(&root)
            },
            candidates,
        };
        (vec![target], Some(tune_stats(&root, &rubric.rules)))
    } else {
        if plan.targets.is_empty() {
            println!("Rubric is up to date. Nothing to compile.");
            return Ok(0);
        }
        (plan.targets, None)
    };
    let binary = std::env::current_exe()?;
    let skill = place_compile_skill(&root);
    let prompt = compile_prompt(&binary, &skill, &targets, stats.as_deref());
    if print {
        println!("{prompt}");
        return Ok(0);
    }
    let available = run_process(
        "claude",
        &["--version".into()],
        ProcessOptions {
            cwd: Some(&root),
            timeout: Duration::from_secs(5),
            input: None,
            env: None,
            max_output: 1024 * 1024,
            inherit_output: false,
        },
    )
    .status
        == Some(0);
    if !available {
        eprintln!("claude is not on PATH. Paste this into a Claude Code session in this repo:");
        println!("{prompt}");
        return Ok(0);
    }
    println!(
        "Starting a bounded headless Claude Code turn to {} the rubric.",
        if tune { "tune" } else { "compile" }
    );
    let mut args = vec![
        "-p".into(),
        prompt,
        "--permission-mode".into(),
        "acceptEdits".into(),
        "--allowedTools".into(),
        format!("Bash({} *)", binary.display()),
        "--add-dir".into(),
        global_dir().to_string_lossy().into_owned(),
    ];
    args.extend(discover_global_sources().into_iter().flat_map(|source| {
        vec![
            "--add-dir".into(),
            source
                .absolute
                .parent()
                .unwrap_or(&source.absolute)
                .to_string_lossy()
                .into_owned(),
        ]
    }));
    args.extend(["--output-format".into(), "text".into()]);
    let output = run_process(
        "claude",
        &args,
        ProcessOptions {
            cwd: Some(&root),
            timeout: Duration::from_secs(600),
            input: None,
            env: None,
            max_output: 1024,
            inherit_output: true,
        },
    );
    if output.timed_out {
        return Err(OrdainError::new(
            ErrorCode::ClaudeUnavailable,
            "claude exceeded the 10 minute compile limit",
        ));
    }
    if output.status != Some(0) {
        return Err(OrdainError::new(
            ErrorCode::ClaudeUnavailable,
            format!("claude exited with {:?}", output.status),
        ));
    }
    for target in targets {
        let file = if target.which == "global" {
            global_rubric_path()
        } else {
            rubric_path(&root)
        };
        match read_rubric(&file) {
            RubricRead::Ok { rubric, .. } => {
                println!("{}: {} rules compiled", file.display(), rubric.rules.len())
            }
            RubricRead::Missing { .. } => {
                eprintln!("Claude finished but {} was not written", file.display())
            }
            RubricRead::Invalid { issues, .. } => eprintln!(
                "Claude finished but {} is invalid: {}",
                file.display(),
                issues.join("; ")
            ),
        }
    }
    Ok(0)
}

fn tune_stats(root: &Path, rules: &[crate::model::Rule]) -> String {
    let history = read_events(root);
    let mut checks = HashMap::<String, usize>::new();
    let mut fired = HashMap::<String, usize>::new();
    for event in history.events {
        if let Event::Check { verdicts, .. } = event {
            for verdict in verdicts {
                *checks.entry(verdict.rule_id.clone()).or_default() += 1;
                if verdict.band == Band::Act {
                    *fired.entry(verdict.rule_id).or_default() += 1;
                }
            }
        }
    }
    let mut lines = vec!["Rewrite only weak/noisy rules. Statistics:".into()];
    if !history.status.complete {
        lines.push(format!(
            "Event history is incomplete (truncated={}, corrupt lines={}, unavailable={}); treat all statistics as partial.",
            history.status.truncated,
            history.status.corrupt_lines,
            history.status.unavailable.as_deref().unwrap_or("no")
        ));
    }
    lines.extend(
        rules
            .iter()
            .filter(|rule| matches!(rule.check, Check::Model { .. }))
            .map(|rule| {
                format!(
                    "  {}: status {:?}, median {}, fired {} of {}",
                    rule.id,
                    rule.status,
                    rule.calibration
                        .as_ref()
                        .map_or("n/a".into(), |c| format!("{:.2}", c.median)),
                    fired.get(&rule.id).copied().unwrap_or(0),
                    checks.get(&rule.id).copied().unwrap_or(0)
                )
            }),
    );
    lines.join("\n")
}

pub fn check(
    paths: &[String],
    diff_file: Option<&Path>,
    task: Option<&str>,
    phase: Option<Phase>,
    show_all: bool,
    json_output: bool,
) -> Result<i32> {
    let root = current_root()?;
    let loaded = load_rules(&root);
    if loaded.rules.is_empty() {
        return Err(OrdainError::new(
            ErrorCode::RubricMissing,
            "no rubric here or in $XDG_CONFIG_HOME/ordain; run ordain compile first",
        ));
    }
    if find_credentials(&root) == Credentials::None {
        return Err(OrdainError::new(ErrorCode::NoApiKey, NO_KEY_HINT));
    }
    let patch = if let Some(file) = diff_file {
        read_regular_text(file, MAX_DIFF_INPUT_CHARS as u64, true).ok_or_else(|| {
            OrdainError::new(ErrorCode::Io, format!("could not read {}", file.display()))
        })?
    } else {
        working_tree_diff(&root, paths)?
    };
    let files = split_diff(&patch)?;
    let context = EvaluationContext::new(&root, &loaded.rules, loaded.thresholds)?;
    let phases = phase.map_or_else(|| vec![Phase::Edit, Phase::Turn], |phase| vec![phase]);
    let mut sections = Vec::new();
    let mut spend = 0.0;
    for current in phases {
        if files.is_empty() {
            continue;
        }
        let request = CheckRequest {
            phase: current,
            file_diffs: &files,
            task,
            timeout: Duration::from_secs(15),
            retries: 2,
        };
        let outcome = if diff_file.is_some() {
            context.evaluate(request)
        } else {
            context.evaluate_live(request, None)
        };
        spend += outcome.usage.cost_usd.unwrap_or(0.0);
        sections.push(CheckSection::new(
            current,
            files.iter().map(|file| file.file.clone()).collect(),
            outcome,
        ));
    }
    let broken = sections.iter().any(CheckSection::has_act);
    let failed = sections.iter().any(|section| !section.failures.is_empty());
    if json_output {
        println!(
            "{}",
            json!({"root":root,"sections":sections,"spendUsd":spend,"all":show_all})
        );
    } else if files.is_empty() {
        println!("Nothing to check: no changed lines.");
    } else {
        println!("Ordain check: {} file(s), about ${spend:.6}", files.len());
        for section in &sections {
            print_section(section, show_all);
        }
    }
    Ok(if failed { 2 } else { i32::from(broken) })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckSection {
    phase: Phase,
    files: Vec<String>,
    model_rules: usize,
    calls: crate::check::CallCounts,
    latency_ms: u64,
    usage: crate::model::Usage,
    verdicts: Vec<crate::model::Verdict>,
    failures: Vec<CheckFailure>,
}

impl CheckSection {
    fn new(phase: Phase, files: Vec<String>, outcome: CheckOutcome) -> Self {
        Self {
            phase,
            files,
            model_rules: outcome.model_rules.len(),
            calls: outcome.calls,
            latency_ms: outcome.model_latency_ms,
            usage: outcome.usage,
            verdicts: outcome.verdicts,
            failures: outcome.failures,
        }
    }

    fn has_act(&self) -> bool {
        self.verdicts
            .iter()
            .any(|verdict| verdict.band == Band::Act)
    }
}

fn print_section(section: &CheckSection, show_all: bool) {
    println!("{}: {}", section.phase.as_str(), section.files.join(", "));
    for verdict in &section.verdicts {
        if show_all || verdict.band != Band::Clear {
            println!(
                "  {:<6} {:<32} {:.2}",
                format!("{:?}", verdict.band).to_lowercase(),
                verdict.rule_id,
                verdict.probability
            );
        }
    }
    for failure in &section.failures {
        println!("  ERROR {}: {}", failure.code.as_str(), failure.message);
    }
}

pub fn report(json_output: bool) -> Result<i32> {
    let root = current_root()?;
    let loaded = load_rules(&root);
    if loaded.rules.is_empty() {
        if json_output {
            println!("{}", json!({"root":root,"rules":[],"events":[]}));
        } else {
            eprintln!("No rubric found; run ordain compile first.");
        }
        return Ok(1);
    }
    let history = read_events(&root);
    let mut stats: HashMap<String, (usize, usize, usize)> = HashMap::new();
    let mut skipped = 0;
    let mut errors = 0;
    for event in &history.events {
        match event {
            Event::Check { verdicts, .. } => {
                for verdict in verdicts {
                    let row = stats.entry(verdict.rule_id.clone()).or_default();
                    row.0 += 1;
                    if verdict.band == Band::Act {
                        row.1 += 1;
                    } else if verdict.band == Band::Flag {
                        row.2 += 1;
                    }
                }
            }
            Event::Skip { .. } => skipped += 1,
            Event::Error { .. } => errors += 1,
            Event::CompileNeeded { .. } | Event::HistoryBoundary { .. } => {}
        }
    }
    let dead = loaded
        .rules
        .iter()
        .filter(|rule| {
            let row = stats.get(&rule.id).copied().unwrap_or_default();
            rule.status == RuleStatus::Active
                && matches!(rule.check, Check::Model { .. })
                && rule.calibration.as_ref().is_none_or(|c| c.median >= 0.25)
                && row.0 >= 20
                && row.1 == 0
                && row.2 == 0
        })
        .map(|rule| rule.id.clone())
        .collect::<Vec<_>>();
    if json_output {
        let stats = stats
            .into_iter()
            .map(|(id, (checks, fired, flagged))| {
                (id, json!({"checks":checks,"fired":fired,"flagged":flagged}))
            })
            .collect::<serde_json::Map<_, _>>();
        println!(
            "{}",
            json!({"root":root,"rules":loaded.rules,"stats":stats,"dead":dead,"events":history.events.len(),"history":history.status,"skipped":skipped,"errors":errors,"problems":loaded.problems})
        );
    } else {
        println!(
            "Ordain report: {} rules, {} events ({} skipped, {} errors)",
            loaded.rules.len(),
            history.events.len(),
            skipped,
            errors
        );
        if !history.status.complete {
            println!(
                "  history incomplete: truncated={}, corrupt lines={}, unavailable={}",
                history.status.truncated,
                history.status.corrupt_lines,
                history.status.unavailable.as_deref().unwrap_or("no")
            );
        }
        for rule in &loaded.rules {
            let (checks, fired, flagged) = stats.get(&rule.id).copied().unwrap_or_default();
            println!(
                "  {:<32} {:<13} {:<12} checks {checks}, fired {fired}, flagged {flagged}",
                rule.id,
                rule.check.kind(),
                format!("{:?}", rule.status).to_lowercase()
            );
        }
        if !dead.is_empty() {
            println!("Never deciding after 20+ checks: {}", dead.join(", "));
        }
        for problem in loaded.problems {
            eprintln!("warning: {problem}");
        }
    }
    Ok(0)
}

pub fn bench(runs: usize, json_output: bool) -> Result<i32> {
    if runs > 100 {
        return Err(OrdainError::new(
            ErrorCode::InvalidArguments,
            "bench is limited to 100 runs",
        ));
    }
    let root = current_root()?;
    if find_credentials(&root) == Credentials::None {
        return Err(OrdainError::new(ErrorCode::NoApiKey, NO_KEY_HINT));
    }
    let loaded = load_rules(&root);
    if loaded.rules.is_empty() {
        return Err(OrdainError::new(
            ErrorCode::RubricMissing,
            "no rubric to bench with; run ordain compile first",
        ));
    }
    let runs = runs.max(1);
    let context = EvaluationContext::new(&root, &loaded.rules, loaded.thresholds)?;
    let binary = std::env::current_exe()?;
    let startup = measure(runs, || {
        let started = Instant::now();
        let output = run_process(
            binary.to_string_lossy().as_ref(),
            &["__hook".into(), "session-start".into()],
            ProcessOptions {
                cwd: Some(&root),
                timeout: Duration::from_secs(5),
                input: Some(b"{}"),
                env: None,
                max_output: 1024,
                inherit_output: false,
            },
        );
        if output.timed_out || output.status != Some(0) {
            return Err(OrdainError::new(
                ErrorCode::CheckFailed,
                "benchmark startup hook did not complete",
            ));
        }
        Ok(started.elapsed().as_secs_f64() * 1000.0)
    })?;
    let small = "@@ -1 +1,3 @@\n-old\n+let value = load();\n+return value;";
    let large = (0..50)
        .map(|index| format!("+let step{index} = compute({index});"))
        .collect::<Vec<_>>()
        .join("\n");
    let edit = |text: &str| {
        checked_bench_outcome(context.evaluate(CheckRequest {
            phase: Phase::Edit,
            file_diffs: &[FileDiff {
                file: "src/ordain-bench-fixture.rs".into(),
                text: text.into(),
            }],
            task: Some("synthetic benchmark fixture"),
            timeout: Duration::from_secs(8),
            retries: 2,
        }))
    };
    let warm = edit(small)?;
    let small_times = measure(runs, || Ok(edit(small)?.model_latency_ms as f64))?;
    let large_once = edit(&large)?;
    let large_times = measure(runs, || Ok(edit(&large)?.model_latency_ms as f64))?;
    let turn_once = checked_bench_outcome(context.evaluate(CheckRequest {
        phase: Phase::Turn,
        file_diffs: &[FileDiff {
            file: "src/ordain-bench-fixture.rs".into(),
            text: large.clone(),
        }],
        task: Some("synthetic benchmark fixture"),
        timeout: Duration::from_secs(15),
        retries: 2,
    }))?;
    let turn_times = if turn_once.model_rules.is_empty() {
        Vec::new()
    } else {
        measure(runs, || {
            Ok(checked_bench_outcome(context.evaluate(CheckRequest {
                phase: Phase::Turn,
                file_diffs: &[FileDiff {
                    file: "src/ordain-bench-fixture.rs".into(),
                    text: large.clone(),
                }],
                task: Some("synthetic benchmark fixture"),
                timeout: Duration::from_secs(15),
                retries: 2,
            }))?
            .model_latency_ms as f64)
        })?
    };
    let synthetic_content = (0..50)
        .map(|index| format!("let step{index} = compute({index});"))
        .collect::<Vec<_>>()
        .join("\n");
    // Native hooks review files that actually exist. Keep benchmark writes out of the user's tree.
    let fixture = tempfile::tempdir()?;
    let fixture_root = fixture.path();
    fs::create_dir_all(fixture_root.join("src"))?;
    fs::create_dir_all(fixture_root.join(".ordain"))?;
    fs::write(
        fixture_root.join("src/ordain-bench-fixture.rs"),
        &synthetic_content,
    )?;
    fs::write(fixture_root.join(".ordain/rubric.json"), serde_json::to_vec(&json!({
        "version":1,"compiledAt":"benchmark","sources":[],"thresholds":loaded.thresholds,"rules":loaded.rules
    })).map_err(|e| OrdainError::new(ErrorCode::Io, e.to_string()))?)?;
    if let Some(config) = read_regular_text(
        &root.join(".ordain/config.toml"),
        crate::config::MAX_CONFIG_BYTES,
        false,
    ) {
        fs::write(fixture_root.join(".ordain/config.toml"), config)?;
    }
    let mut hook_env = HashMap::new();
    match find_credentials(&root) {
        Credentials::TypeSafe { key, .. } => {
            hook_env.insert("TYPESAFE_AI_API_KEY".into(), key);
        }
        Credentials::Gateway { key, .. } => {
            hook_env.insert("AI_GATEWAY_API_KEY".into(), key);
        }
        Credentials::None => {}
    }
    let hook_payload = serde_json::to_vec(&json!({
        "session_id":"ordain-bench","prompt_id":"synthetic-turn","cwd":fixture_root,
        "hook_event_name":"PostToolUse","tool_name":"Write",
        "tool_input":{"file_path":fixture_root.join("src/ordain-bench-fixture.rs"),"content":synthetic_content},
        "tool_response":{"originalFile":null,"structuredPatch":[]}
    }))
    .unwrap_or_default();
    let full_hook = measure(runs, || {
        let started = Instant::now();
        let output = run_process(
            binary.to_string_lossy().as_ref(),
            &["__hook".into(), "post-tool-use".into()],
            ProcessOptions {
                cwd: Some(fixture_root),
                timeout: Duration::from_secs(30),
                input: Some(&hook_payload),
                env: Some(&hook_env),
                max_output: 1024 * 1024,
                inherit_output: false,
            },
        );
        if output.timed_out || output.status != Some(0) || output.stdout_truncated {
            return Err(OrdainError::new(
                ErrorCode::CheckTimeout,
                "the full benchmark hook did not finish",
            ));
        }
        if !output.stdout.iter().all(u8::is_ascii_whitespace) {
            let response: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
                OrdainError::new(
                    ErrorCode::CheckFailed,
                    "benchmark hook returned invalid JSON",
                )
            })?;
            // Hooks keep exit 0 for host compatibility even when checking fails.
            if response.get("systemMessage").is_some() {
                return Err(OrdainError::new(
                    ErrorCode::CheckFailed,
                    "benchmark hook reported incomplete checking",
                ));
            }
        }
        Ok(started.elapsed().as_secs_f64() * 1000.0)
    });
    crate::state::clear_turn(&crate::state::turn_dir(
        fixture_root,
        "ordain-bench",
        Some("synthetic-turn"),
    ));
    let full_hook = full_hook?;
    let history = read_events(&root);
    let mut session_totals = HashMap::<String, (usize, u64, f64)>::new();
    for event in &history.events {
        if let Event::Check {
            session_id: Some(session_id),
            latency_ms,
            usage,
            ..
        } = event
            && session_id != "ordain-bench"
        {
            let total = session_totals.entry(session_id.clone()).or_default();
            total.0 += 1;
            total.1 += latency_ms;
            total.2 += usage
                .as_ref()
                .and_then(|usage| usage.cost_usd)
                .unwrap_or(0.0);
        }
    }
    let mut rows = vec![
        row("hook process start, no network", &startup),
        row("edit check, small synthetic diff", &small_times),
        row("edit check, large synthetic diff", &large_times),
    ];
    if !turn_times.is_empty() {
        rows.push(row("turn check, synthetic diff", &turn_times));
    }
    rows.push(row("whole PostToolUse hook, synthetic diff", &full_hook));
    let historical = if session_totals.is_empty() {
        Value::Null
    } else {
        let checks = session_totals
            .values()
            .map(|value| value.0 as f64)
            .collect::<Vec<_>>();
        let latency = session_totals
            .values()
            .map(|value| value.1 as f64)
            .collect::<Vec<_>>();
        let cost = session_totals
            .values()
            .map(|value| value.2)
            .collect::<Vec<_>>();
        json!({
            "count":session_totals.len(),"medianChecks":median_of(&checks),
            "medianLatencyMs":median_of(&latency),"medianCostUsd":median_of(&cost)
        })
    };
    let data = json!({
        "root":root,"runs":runs,"fixture":"synthetic test-only benchmark diff",
        "activeModelRules":loaded.rules.iter().filter(|rule| rule.status == RuleStatus::Active && matches!(rule.check,Check::Model{..})).count(),
        "rows":rows,
        "tokens":{"small":warm.usage.input_tokens,"large":large_once.usage.input_tokens,"turn":turn_once.usage.input_tokens},
        "cost":{"small":warm.usage.cost_usd,"large":large_once.usage.cost_usd,"turn":turn_once.usage.cost_usd},
        "perTurn":{"latencyMs":15.0*median_of(&full_hook)+median_of(&turn_times),"costUsd":15.0*large_once.usage.cost_usd.unwrap_or(0.0)+turn_once.usage.cost_usd.unwrap_or(0.0)},
        "sessions":historical,"history":history.status
    });
    if json_output {
        println!("{data}");
    } else {
        println!("Ordain bench (synthetic fixture, {runs} runs)");
        for row in data["rows"].as_array().into_iter().flatten() {
            println!(
                "  {:<38} median {:>7.1}ms p90 {:>7.1}ms",
                row["what"].as_str().unwrap_or(""),
                row["median"].as_f64().unwrap_or(0.0),
                row["p90"].as_f64().unwrap_or(0.0)
            );
        }
        println!(
            "Estimated cost: small ${:.6}, large ${:.6}, turn ${:.6}",
            warm.usage.cost_usd.unwrap_or(0.0),
            large_once.usage.cost_usd.unwrap_or(0.0),
            turn_once.usage.cost_usd.unwrap_or(0.0)
        );
    }
    Ok(0)
}

fn checked_bench_outcome(outcome: CheckOutcome) -> Result<CheckOutcome> {
    if let Some(failure) = outcome.failures.first() {
        Err(OrdainError::new(
            ErrorCode::CheckFailed,
            format!(
                "benchmark check failed ({}): {}",
                failure.code.as_str(),
                failure.message
            ),
        ))
    } else {
        Ok(outcome)
    }
}

fn measure<F>(runs: usize, mut operation: F) -> Result<Vec<f64>>
where
    F: FnMut() -> Result<f64>,
{
    (0..runs).map(|_| operation()).collect()
}

fn row(what: &str, values: &[f64]) -> Value {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let median = percentile(&values, 50.0);
    let p90 = percentile(&values, 90.0);
    json!({"what":what,"median":median,"p90":p90})
}

fn median_of(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        0.0
    } else if values.len().is_multiple_of(2) {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    } else {
        values[values.len() / 2]
    }
}

fn percentile(values: &[f64], percent: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let index = ((values.len() - 1) as f64 * percent / 100.0).round() as usize;
    values[index]
}

fn current_root() -> Result<PathBuf> {
    Ok(find_repo_root(&std::env::current_dir()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_bounded() {
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 90.0), 3.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }
}
