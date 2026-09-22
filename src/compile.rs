use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::model::Rubric;
use crate::paths::{global_rubric_path, home_dir, ordain_dir, resolve_source_path, rubric_path};
use crate::rubric::{
    RubricRead, SourceCandidate, SourceOrigin, Staleness, check_staleness, describe_staleness,
    discover_global_sources, discover_project_sources, find_lint_configs, read_rubric,
};

#[derive(Debug, Clone)]
pub struct CompileTarget {
    pub which: &'static str,
    pub root: PathBuf,
    pub candidates: Vec<SourceCandidate>,
    pub staleness: Staleness,
    pub lint_configs: Vec<String>,
}

pub struct CompilePlan {
    pub targets: Vec<CompileTarget>,
    pub invalid: Vec<String>,
    pub no_sources: bool,
}

pub fn plan_compile(root: &Path) -> CompilePlan {
    let mut targets = Vec::new();
    let mut invalid = Vec::new();
    let mut project = discover_project_sources(root);
    let project_read = read_rubric(&rubric_path(root));
    if let RubricRead::Invalid { path, issues } = &project_read {
        invalid.push(format!(
            "{}: {}",
            path.display(),
            issues
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let project_rubric = match &project_read {
        RubricRead::Ok { rubric, .. } => Some(rubric),
        _ => None,
    };
    retain_recorded_sources(&mut project, project_rubric, root);
    let project_staleness = check_staleness(project_rubric, &project, root);
    if project.iter().any(|source| source.required)
        && project_staleness != Staleness::Fresh
        && !matches!(project_read, RubricRead::Invalid { .. })
    {
        targets.push(CompileTarget {
            which: "project",
            root: root.to_path_buf(),
            candidates: project.clone(),
            staleness: project_staleness,
            lint_configs: find_lint_configs(root),
        });
    }
    let mut global = discover_global_sources();
    let global_read = read_rubric(&global_rubric_path());
    if let RubricRead::Invalid { path, issues } = &global_read {
        invalid.push(format!(
            "{}: {}",
            path.display(),
            issues
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let global_rubric = match &global_read {
        RubricRead::Ok { rubric, .. } => Some(rubric),
        _ => None,
    };
    retain_recorded_sources(&mut global, global_rubric, &home_dir());
    let global_staleness = check_staleness(global_rubric, &global, &home_dir());
    if !global.is_empty()
        && global_staleness != Staleness::Fresh
        && !matches!(global_read, RubricRead::Invalid { .. })
    {
        targets.push(CompileTarget {
            which: "global",
            root: home_dir(),
            candidates: global.clone(),
            staleness: global_staleness,
            lint_configs: Vec::new(),
        });
    }
    CompilePlan {
        targets,
        invalid,
        no_sources: project.is_empty() && global.is_empty(),
    }
}

fn retain_recorded_sources(
    candidates: &mut Vec<SourceCandidate>,
    rubric: Option<&Rubric>,
    root: &Path,
) {
    let Some(rubric) = rubric else { return };
    for source in &rubric.sources {
        let absolute = resolve_source_path(root, &source.path);
        if candidates
            .iter()
            .any(|candidate| candidate.absolute == absolute)
            || !absolute.is_file()
        {
            continue;
        }
        candidates.push(SourceCandidate {
            path: source.path.clone(),
            absolute,
            scope: source.scope.clone().unwrap_or_else(|| "**/*".into()),
            required: true,
            origin: SourceOrigin::Recorded,
        });
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
}

pub fn place_compile_skill(root: &Path) -> PathBuf {
    let target = ordain_dir(root).join("compile-skill.md");
    let result = (|| -> std::io::Result<()> {
        let parent = target.parent().expect("skill target has parent");
        if fs::symlink_metadata(parent).is_ok_and(|metadata| !metadata.file_type().is_dir()) {
            return Err(std::io::Error::other("unsafe compile-skill directory"));
        }
        fs::create_dir_all(parent)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&target)?;
        output.write_all(include_bytes!("../assets/compile-skill.md"))
    })();
    if result.is_ok() {
        target
    } else {
        PathBuf::from("<embedded Ordain compile skill>")
    }
}

pub fn compile_prompt(
    binary: &Path,
    skill: &Path,
    targets: &[CompileTarget],
    tune: Option<&str>,
) -> String {
    let head = if tune.is_some() {
        "Ordain is installed here. Some rubric rules never fire or fire on everything. Rewrite only the named rules before starting the user's request."
    } else {
        "Ordain is installed here and its rubric is missing or out of date. Compile it before starting the user's request."
    };
    let mut parts = vec![
        head.to_owned(),
        if skill.exists() {
            format!(
                "Read {} and follow it through validation and calibration.",
                skill.display()
            )
        } else {
            format!(
                "Follow this embedded compilation procedure through validation and calibration:\n\n{}",
                include_str!("../assets/compile-skill.md")
            )
        },
        format!("Run the Ordain CLI as: {} <command>", binary.display()),
    ];
    for target in targets {
        let file = if target.which == "project" {
            rubric_path(&target.root)
        } else {
            global_rubric_path()
        };
        let mut lines = vec![
            format!(
                "{} rubric: {} ({})",
                target.which,
                file.display(),
                describe_staleness(&target.staleness)
            ),
            format!("  root: {}", target.root.display()),
        ];
        lines.extend(target.candidates.iter().map(|candidate| {
            format!(
                "  source: {} (rules apply to {}{})",
                candidate.path,
                candidate.scope,
                if candidate.required {
                    ""
                } else {
                    "; include only imperative rules"
                }
            )
        }));
        if !target.lint_configs.is_empty() {
            lines.push(format!(
                "  lint configs for overlap names: {}",
                target.lint_configs.join(", ")
            ));
        }
        parts.push(lines.join("\n"));
    }
    if let Some(stats) = tune {
        parts.push(stats.to_owned());
    }
    parts.push("When finished, report the four bucket counts and any weak/noisy rules, then continue with the user's request.".into());
    parts.join("\n\n")
}
