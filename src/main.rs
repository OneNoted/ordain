use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use ordain::commands;
use ordain::events::append_event;
use ordain::hooks;
use ordain::model::{Event, HookOutput, Phase};
use ordain::paths::find_repo_root;
use serde_json::Value;

#[derive(Parser)]
#[command(
    name = "ordain",
    version,
    about = "Enforce repository instructions on coding-agent edits"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Login,
    Config {
        #[arg(default_value = "validate")]
        action: String,
        #[arg(long)]
        rule: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Init(HostArgs),
    /// Manage native agent integrations independently of project setup.
    Integration {
        #[command(subcommand)]
        command: IntegrationCommand,
    },
    /// Select project-owned snapshots of optional rule packs.
    Preset {
        #[command(subcommand)]
        command: PresetCommand,
    },
    Compile(CompileArgs),
    Tune(CompileArgs),
    Rubric(RubricArgs),
    Calibrate(CalibrateArgs),
    Check(CheckArgs),
    Audit(AuditArgs),
    Report(JsonArgs),
    Replay(ReplayArgs),
    Bench(BenchArgs),
    Uninstall(HostArgs),
    #[command(name = "__hook", hide = true)]
    Hook {
        name: String,
    },
}

#[derive(Subcommand)]
enum PresetCommand {
    List,
    Show {
        name: String,
    },
    Add {
        name: String,
    },
    /// Preview replacement of a selected pack, including local tuning.
    Update {
        name: String,
        #[arg(long)]
        apply: bool,
    },
    Remove {
        name: String,
    },
    Validate,
}

#[derive(Subcommand)]
enum IntegrationCommand {
    Install {
        host: String,
        #[arg(long)]
        project: bool,
        /// Repository checked by Hermes (required for Hermes installation).
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Uninstall {
        host: String,
        #[arg(long)]
        project: bool,
    },
    Status {
        host: Option<String>,
        #[arg(long)]
        project: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
struct HostArgs {
    hosts: Vec<String>,
    #[arg(long)]
    project: bool,
}

#[derive(Args, Clone)]
struct CompileArgs {
    #[arg(long)]
    print: bool,
    #[arg(long)]
    global: bool,
}

#[derive(Args)]
struct RubricArgs {
    #[arg(default_value = "validate")]
    action: String,
    #[arg(long)]
    global: bool,
}

#[derive(Args)]
struct CalibrateArgs {
    #[arg(long, conflicts_with = "presets")]
    global: bool,
    #[arg(long)]
    presets: bool,
    #[arg(long, default_value_t = 20)]
    hunks: usize,
    #[arg(long, default_value_t = 8)]
    commits: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum PhaseArg {
    All,
    Edit,
    Turn,
}

#[derive(Args)]
struct CheckArgs {
    paths: Vec<String>,
    #[arg(long)]
    diff: Option<PathBuf>,
    #[arg(long)]
    task: Option<String>,
    #[arg(long, value_enum, default_value_t = PhaseArg::All)]
    phase: PhaseArg,
    #[arg(long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct AuditArgs {
    paths: Vec<String>,
    #[arg(long, default_value_t = 3)]
    concurrency: usize,
    #[arg(long)]
    max_files: Option<usize>,
    #[arg(long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct JsonArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ReplayArgs {
    agent_or_path: Option<String>,
    paths: Vec<String>,
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long, default_value_t = 3)]
    concurrency: usize,
    #[arg(long)]
    max_sessions: Option<usize>,
    #[arg(long)]
    diffs: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct BenchArgs {
    #[arg(long, default_value_t = 5)]
    runs: usize,
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => {
            let mut command = <Cli as clap::CommandFactory>::command();
            command
                .print_help()
                .map(|_| {
                    println!();
                    0
                })
                .map_err(Into::into)
        }
        Some(Command::Login) => commands::login(),
        Some(Command::Config { action, rule, json }) => {
            ordain::config::run(&action, rule.as_deref(), json)
        }
        Some(Command::Init(args)) => commands::init(&args.hosts, args.project),
        Some(Command::Integration { command }) => match command {
            IntegrationCommand::Install {
                host,
                project,
                workspace,
            } => commands::integration_install(&host, project, workspace.as_deref()),
            IntegrationCommand::Uninstall { host, project } => {
                commands::uninstall(&[host], project)
            }
            IntegrationCommand::Status {
                host,
                project,
                json,
            } => commands::integration_status(host.as_deref(), project, json),
        },
        Some(Command::Preset { command }) => {
            let (action, name, apply) = match &command {
                PresetCommand::List => ("list", None, false),
                PresetCommand::Show { name } => ("show", Some(name.as_str()), false),
                PresetCommand::Add { name } => ("add", Some(name.as_str()), false),
                PresetCommand::Update { name, apply } => ("update", Some(name.as_str()), *apply),
                PresetCommand::Remove { name } => ("remove", Some(name.as_str()), false),
                PresetCommand::Validate => ("validate", None, false),
            };
            ordain::presets::run(action, name, apply)
        }
        Some(Command::Compile(args)) => commands::compile(args.print, args.global, false),
        Some(Command::Tune(args)) => commands::compile(args.print, args.global, true),
        Some(Command::Rubric(args)) => commands::rubric(&args.action, args.global),
        Some(Command::Calibrate(args)) => ordain::calibration::run(
            args.global,
            args.presets,
            args.hunks,
            args.commits,
            args.json,
        ),
        Some(Command::Check(args)) => commands::check(
            &args.paths,
            args.diff.as_deref(),
            args.task.as_deref(),
            match args.phase {
                PhaseArg::All => None,
                PhaseArg::Edit => Some(Phase::Edit),
                PhaseArg::Turn => Some(Phase::Turn),
            },
            args.all,
            args.json,
        ),
        Some(Command::Audit(args)) => ordain::audit::run(
            &args.paths,
            args.concurrency,
            args.max_files,
            args.all,
            args.json,
        ),
        Some(Command::Report(args)) => commands::report(args.json),
        Some(Command::Replay(args)) => ordain::replay::run(
            args.agent_or_path.as_deref(),
            &args.paths,
            args.repo.as_deref(),
            args.concurrency,
            args.max_sessions,
            args.diffs,
            args.json,
        ),
        Some(Command::Bench(args)) => commands::bench(args.runs, args.json),
        Some(Command::Uninstall(args)) => commands::uninstall(&args.hosts, args.project),
        Some(Command::Hook { name }) => return run_hook(&name),
    };
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            eprintln!("ordain: {}: {}", error.code.as_str(), error.message);
            ExitCode::from(2)
        }
    }
}

fn run_hook(name: &str) -> ExitCode {
    let (event, budget) = match name {
        "session-start" => ("SessionStart", Duration::from_secs(8)),
        "turn-start" => ("UserPromptSubmit", Duration::from_secs(8)),
        "post-tool-use" => ("PostToolUse", Duration::from_secs(18)),
        "stop" => ("Stop", Duration::from_secs(28)),
        _ => return ExitCode::SUCCESS,
    };
    let started = Instant::now();
    let (input_sender, input_receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut input = Vec::new();
        let mut stdin = io::stdin().take(1024 * 1024 + 1);
        let value = if stdin.read_to_end(&mut input).is_ok() && input.len() <= 1024 * 1024 {
            Some(input)
        } else {
            None
        };
        let _ = input_sender.send(value);
    });
    let input = match input_receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(Some(input)) => input,
        _ => return ExitCode::SUCCESS,
    };
    let raw: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    let timeout_context = raw
        .get("cwd")
        .and_then(Value::as_str)
        .zip(raw.get("session_id").and_then(Value::as_str))
        .map(|(cwd, session)| {
            (
                find_repo_root(std::path::Path::new(cwd)),
                session.to_owned(),
            )
        });
    let (sender, receiver) = mpsc::channel();
    let name = name.to_owned();
    let worker_name = name.clone();
    let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ordain"));
    thread::spawn(move || {
        let output = std::panic::catch_unwind(|| hooks::handle(&worker_name, raw, &binary))
            .unwrap_or(HookOutput::Silent);
        let _ = sender.send(output);
    });
    let remaining = budget
        .checked_sub(started.elapsed())
        .unwrap_or(Duration::ZERO);
    match receiver.recv_timeout(remaining) {
        Ok(output) => {
            if let Some(value) = output.to_protocol(event)
                && let Ok(text) = serde_json::to_string(&value)
            {
                println!("{text}");
            }
        }
        Err(_) => {
            if let Some((root, session_id)) = timeout_context {
                append_event(
                    &root,
                    &Event::Error {
                        at: Utc::now().to_rfc3339(),
                        phase: name.clone(),
                        session_id: Some(session_id),
                        code: "HOOK_DEADLINE".into(),
                        message: format!("{name} did not finish within its hard deadline"),
                        latency_ms: Some(
                            started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                        ),
                    },
                );
            }
            if std::env::var_os("ORDAIN_DEBUG").is_some() && io::stderr().is_terminal() {
                eprintln!("ordain: {name}: hook deadline reached");
            }
        }
    }
    ExitCode::SUCCESS
}
