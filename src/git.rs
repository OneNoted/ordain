use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::diff::split_diff;
use crate::error::{ErrorCode, OrdainError, Result};
use crate::model::FileDiff;
use crate::paths::is_skipped_content_path;
use crate::process::{ProcessOptions, run};

pub const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_GIT_OUTPUT: usize = 64 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;

fn git_bytes(
    root: &Path,
    args: &[&str],
    timeout: Duration,
    env: Option<&HashMap<String, String>>,
    input: Option<&[u8]>,
    okay: &[i32],
) -> Result<Vec<u8>> {
    if timeout.is_zero() {
        return Err(OrdainError::new(
            ErrorCode::CheckTimeout,
            "the Git operation exceeded its time limit",
        ));
    }
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let output = run(
        "git",
        &args,
        ProcessOptions {
            cwd: Some(root),
            timeout,
            input,
            env,
            max_output: MAX_GIT_OUTPUT,
            inherit_output: false,
        },
    );
    if output.timed_out {
        return Err(OrdainError::new(
            ErrorCode::CheckTimeout,
            "the Git operation exceeded its time limit",
        ));
    }
    if output.stdout_truncated || output.stderr_truncated {
        return Err(OrdainError::new(
            ErrorCode::GitUnavailable,
            "Git output exceeded the 64 MiB safety limit",
        ));
    }
    if !output.status.is_some_and(|code| okay.contains(&code)) {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim().chars().take(500).collect::<String>();
        let message = if detail.is_empty() {
            "Git did not complete successfully".into()
        } else {
            format!("Git did not complete successfully: {detail}")
        };
        return Err(OrdainError::new(ErrorCode::GitUnavailable, message));
    }
    Ok(output.stdout)
}

fn git_text(
    root: &Path,
    args: &[&str],
    timeout: Duration,
    env: Option<&HashMap<String, String>>,
    input: Option<&[u8]>,
    okay: &[i32],
) -> Result<String> {
    String::from_utf8(git_bytes(root, args, timeout, env, input, okay)?).map_err(|_| {
        OrdainError::new(
            ErrorCode::GitUnavailable,
            "Git emitted text that is not valid UTF-8",
        )
    })
}

pub fn is_git_repo(root: &Path) -> bool {
    git_text(
        root,
        &["rev-parse", "--is-inside-work-tree"],
        Duration::from_secs(2),
        None,
        None,
        &[0],
    )
    .is_ok()
}

pub fn is_ignored(root: &Path, relative: &str) -> bool {
    if !is_git_repo(root) {
        return true;
    }
    // Fail closed: an uncertain secret-ignore check must not encourage installation.
    git_bytes(
        root,
        &["check-ignore", "-q", "--", relative],
        Duration::from_secs(2),
        None,
        None,
        &[0],
    )
    .is_ok()
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            OrdainError::new(
                ErrorCode::CheckTimeout,
                "the Git operation exceeded its time limit",
            )
        })
}

fn index_path(root: &Path, timeout: Duration) -> Result<PathBuf> {
    let raw = git_text(
        root,
        &["rev-parse", "--git-path", "index"],
        timeout,
        None,
        None,
        &[0],
    )?;
    let path = PathBuf::from(raw.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn copy_index_without_links(source: &Path, destination: &Path) -> Result<()> {
    let mut source_options = OpenOptions::new();
    source_options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut input = match source_options.open(source) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let metadata = input.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES {
        return Err(OrdainError::new(
            ErrorCode::GitUnavailable,
            "the Git index is not a regular file within the 256 MiB safety limit",
        ));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)?;
    let copied = std::io::copy(
        &mut std::io::Read::by_ref(&mut input).take(MAX_INDEX_BYTES + 1),
        &mut output,
    )?;
    if copied > MAX_INDEX_BYTES {
        return Err(OrdainError::new(
            ErrorCode::GitUnavailable,
            "the Git index grew beyond the 256 MiB safety limit",
        ));
    }
    output.flush()?;
    // Git uses the index mtime to detect racy same-size worktree edits.
    output.set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
    Ok(())
}

fn nul_paths(bytes: &[u8]) -> Result<Vec<String>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(ToOwned::to_owned)
                .map_err(|_| {
                    OrdainError::new(
                        ErrorCode::GitUnavailable,
                        "Git returned a pathname that is not valid UTF-8",
                    )
                })
        })
        .collect()
}

fn nul_input(paths: &[String]) -> Vec<u8> {
    let capacity = paths.iter().map(|path| path.len() + 1).sum();
    let mut bytes = Vec::with_capacity(capacity);
    for path in paths {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
    }
    bytes
}

/// Build a tree in a private, short-lived index without modifying the repository index.
/// The same exclusion policy used when parsing diffs is applied to tracked and untracked files.
pub fn snapshot_tree(root: &Path, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let scratch = crate::state::snapshot_scratch()?;
    let scratch_index = scratch.path().join("index");
    let real_index = index_path(root, remaining(deadline)?)?;
    copy_index_without_links(&real_index, &scratch_index)?;

    let listed = git_bytes(
        root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        remaining(deadline)?,
        None,
        None,
        &[0],
    )?;
    let all_paths = nul_paths(&listed)?;
    let (included, excluded): (Vec<_>, Vec<_>) = all_paths
        .into_iter()
        .partition(|path| !is_skipped_content_path(path, false));

    let mut environment = HashMap::new();
    environment.insert(
        "GIT_INDEX_FILE".into(),
        scratch_index.to_string_lossy().into_owned(),
    );
    if !excluded.is_empty() {
        git_bytes(
            root,
            &["update-index", "--force-remove", "-z", "--stdin"],
            remaining(deadline)?,
            Some(&environment),
            Some(&nul_input(&excluded)),
            &[0],
        )?;
    }
    if !included.is_empty() {
        // Paths are already allowlisted: tracked files plus nonignored untracked files,
        // minus our content exclusions. Git otherwise rejects tracked children of an
        // ignored directory even though they belong in the snapshot.
        git_bytes(
            root,
            &[
                "--literal-pathspecs",
                "add",
                "-A",
                "-f",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            remaining(deadline)?,
            Some(&environment),
            Some(&nul_input(&included)),
            &[0],
        )?;
    }
    let tree = git_text(
        root,
        &["write-tree"],
        remaining(deadline)?,
        Some(&environment),
        None,
        &[0],
    )?;
    let tree = tree.trim();
    if (tree.len() == 40 || tree.len() == 64) && tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(tree.to_owned())
    } else {
        Err(OrdainError::new(
            ErrorCode::GitUnavailable,
            "Git returned an invalid tree identifier",
        ))
    }
}

fn head_or_empty_tree(root: &Path, deadline: Instant) -> Result<String> {
    let head = git_text(
        root,
        &["rev-parse", "--verify", "HEAD"],
        remaining(deadline)?,
        None,
        None,
        &[0, 128],
    )?;
    let head = head.trim();
    if !head.is_empty() {
        return Ok(head.to_owned());
    }
    Ok(git_text(
        root,
        &["mktree"],
        remaining(deadline)?,
        None,
        Some(&[]),
        &[0],
    )?
    .trim()
    .to_owned())
}

pub fn working_tree_diff(root: &Path, paths: &[String]) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let base = head_or_empty_tree(root, deadline)?;
    let now = snapshot_tree(root, remaining(deadline)?)?;
    diff_trees_with_paths(root, &base, &now, paths, remaining(deadline)?)
}

pub fn diff_trees(root: &Path, base: &str, now: &str, timeout: Duration) -> Result<String> {
    diff_trees_with_paths(root, base, now, &[], timeout)
}

fn diff_trees_with_paths(
    root: &Path,
    base: &str,
    now: &str,
    paths: &[String],
    timeout: Duration,
) -> Result<String> {
    if base == now {
        return Ok(String::new());
    }
    let mut args = vec![
        "diff-tree",
        "-p",
        "-M",
        "--no-color",
        "--unified=3",
        base,
        now,
    ];
    if !paths.is_empty() {
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
    }
    git_text(root, &args, timeout, None, None, &[0])
}

pub fn blob_ids_at(
    root: &Path,
    tree: &str,
    files: &[String],
    timeout: Duration,
) -> Result<HashMap<String, String>> {
    if files.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(tree_entries(root, tree, files, timeout)?
        .into_iter()
        .filter(|(_, entry)| entry.kind == "blob")
        .map(|(path, entry)| (path, entry.id))
        .collect())
}

struct TreeEntry {
    mode: String,
    kind: String,
    id: String,
}

fn tree_entries(
    root: &Path,
    tree: &str,
    files: &[String],
    timeout: Duration,
) -> Result<HashMap<String, TreeEntry>> {
    let mut args = vec!["--literal-pathspecs", "ls-tree", "-r", "-z", tree, "--"];
    args.extend(files.iter().map(String::as_str));
    let output = git_bytes(root, &args, timeout, None, None, &[0])?;
    let mut ids = HashMap::new();
    for entry in output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
            return Err(OrdainError::new(
                ErrorCode::GitUnavailable,
                "Git returned a malformed tree entry",
            ));
        };
        let metadata = std::str::from_utf8(&entry[..tab]).map_err(|_| {
            OrdainError::new(
                ErrorCode::GitUnavailable,
                "Git returned invalid tree metadata",
            )
        })?;
        let name = std::str::from_utf8(&entry[tab + 1..]).map_err(|_| {
            OrdainError::new(
                ErrorCode::GitUnavailable,
                "Git returned a pathname that is not valid UTF-8",
            )
        })?;
        let fields = metadata.split_whitespace().collect::<Vec<_>>();
        let [mode, kind, id] = fields.as_slice() else {
            return Err(OrdainError::new(
                ErrorCode::GitUnavailable,
                "Git returned malformed tree metadata",
            ));
        };
        ids.insert(
            name.to_owned(),
            TreeEntry {
                mode: (*mode).into(),
                kind: (*kind).into(),
                id: (*id).into(),
            },
        );
    }
    Ok(ids)
}

/// Read immutable before/after images for a non-merge commit, including related
/// files from that revision. Missing objects are errors, not empty before-images.
pub fn revision_snapshot(
    root: &Path,
    revision: &str,
    files: &[String],
    include: &[String],
    deadline: Instant,
) -> Result<crate::evidence::Snapshot> {
    use crate::evidence::incomplete;
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(incomplete("historical evidence requires a full commit ID"));
    }
    let commit = git_text(
        root,
        &["cat-file", "commit", revision],
        remaining(deadline)?,
        None,
        None,
        &[0],
    )?;
    // Object headers retain parents at shallow boundaries, unlike revision traversal.
    let parents: Vec<_> = commit
        .lines()
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.strip_prefix("parent "))
        .collect();
    if parents.len() > 1 {
        return Err(incomplete(
            "merge commits have no unambiguous calibration before-image",
        ));
    }
    let after = tree_entries(
        root,
        revision,
        if include.is_empty() { files } else { &[] },
        remaining(deadline)?,
    )?;
    if after.len() > 20_000 {
        return Err(incomplete(
            "historical context discovery exceeded entry limit",
        ));
    }
    let mut selected: std::collections::BTreeSet<_> = files.iter().cloned().collect();
    if !include.is_empty() {
        let matcher = crate::paths::compile_globs(include).map_err(incomplete)?;
        selected.extend(after.keys().filter(|path| matcher.is_match(path)).cloned());
    }
    if selected.len() > 1000 {
        return Err(incomplete("too many historical context files"));
    }
    if selected.is_empty() {
        return Ok(crate::evidence::Snapshot::default());
    }
    let paths: Vec<_> = selected.into_iter().collect();
    let before = match parents.first() {
        Some(parent) => tree_entries(root, parent, &paths, remaining(deadline)?)?,
        None => HashMap::new(),
    };
    let mut snapshot = crate::evidence::Snapshot::default();
    let mut bytes = 0;
    for path in paths {
        if crate::paths::is_excluded_path(&path) {
            return Err(incomplete(format!(
                "excluded historical context path: {path}"
            )));
        }
        let old = revision_source(root, before.get(&path), deadline)?;
        let new = revision_source(root, after.get(&path), deadline)?;
        if old.is_none() && new.is_none() {
            return Err(incomplete(format!("no historical source for {path}")));
        }
        bytes += old.as_ref().map_or(0, String::len) + new.as_ref().map_or(0, String::len);
        if bytes > 16 * crate::config::MAX_CONTEXT_BYTES {
            return Err(incomplete(
                "historical evidence exceeds memory safety ceiling",
            ));
        }
        snapshot.insert(path, old, new);
    }
    Ok(snapshot)
}

fn revision_source(
    root: &Path,
    entry: Option<&TreeEntry>,
    deadline: Instant,
) -> Result<Option<String>> {
    use crate::evidence::incomplete;
    let Some(entry) = entry else { return Ok(None) };
    if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
        return Err(incomplete("historical context is not a regular file"));
    }
    let size = git_text(
        root,
        &["cat-file", "-s", &entry.id],
        remaining(deadline)?,
        None,
        None,
        &[0],
    )?;
    let size: usize = size
        .trim()
        .parse()
        .map_err(|_| incomplete("invalid historical blob size"))?;
    if size > crate::config::MAX_CONTEXT_BYTES {
        return Err(incomplete(
            "historical source exceeds per-file safety ceiling",
        ));
    }
    let text = git_text(
        root,
        &["cat-file", "blob", &entry.id],
        remaining(deadline)?,
        None,
        None,
        &[0],
    )?;
    Ok(Some(text))
}

#[derive(Debug, Clone)]
pub struct HistoryHunk {
    pub revision: String,
    pub subject: String,
    pub file: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct HistoryCommit {
    pub revision: String,
    pub subject: String,
    pub file_diffs: Vec<FileDiff>,
}

pub fn recent_history(
    root: &Path,
    want_hunks: usize,
    want_commits: usize,
) -> Result<(Vec<HistoryHunk>, Vec<HistoryCommit>)> {
    // An unborn branch has no revisions; other Git failures remain errors.
    let head = git_text(
        root,
        &["rev-parse", "--revs-only", "HEAD"],
        GIT_TIMEOUT,
        None,
        None,
        &[0],
    )?;
    if head.trim().is_empty() {
        // rev-parse also omits malformed refs; do not call corruption an empty history.
        git_text(
            root,
            &["show-ref", "--head"],
            GIT_TIMEOUT,
            None,
            None,
            &[0, 1],
        )?;
        return Ok((Vec::new(), Vec::new()));
    }
    let list = git_text(
        root,
        &[
            "log",
            "--no-merges",
            "-n",
            "60",
            "--format=%H%x1f%s",
            "HEAD",
        ],
        GIT_TIMEOUT,
        None,
        None,
        &[0],
    )?;
    let mut hunks = Vec::new();
    let mut commits = Vec::new();
    let mut seen_files = HashSet::new();
    for line in list.lines() {
        let Some((commit, subject)) = line.split_once('\x1f') else {
            continue;
        };
        if hunks.len() >= want_hunks && commits.len() >= want_commits {
            break;
        }
        let patch = git_text(
            root,
            &[
                "show",
                "--format=",
                "--unified=3",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--diff-filter=AM",
                commit,
            ],
            GIT_TIMEOUT,
            None,
            None,
            &[0],
        )?;
        let files = split_diff(&patch)?;
        if files.is_empty() {
            continue;
        }
        let size: usize = files.iter().map(|file| file.text.len()).sum();
        if commits.len() < want_commits && (200..=24_000).contains(&size) {
            commits.push(HistoryCommit {
                revision: commit.into(),
                subject: subject.into(),
                file_diffs: files.clone(),
            });
        }
        for file in files {
            if hunks.len() >= want_hunks {
                break;
            }
            let changed = file
                .text
                .lines()
                .filter(|line| {
                    (line.starts_with('+') && !line.starts_with("+++"))
                        || (line.starts_with('-') && !line.starts_with("---"))
                })
                .count();
            if changed >= 3 && file.text.len() <= 6_000 && seen_files.insert(file.file.clone()) {
                hunks.push(HistoryHunk {
                    revision: commit.into(),
                    subject: subject.into(),
                    file: file.file,
                    text: file.text,
                });
            }
        }
    }
    Ok((hunks, commits))
}

pub fn list_repo_files(root: &Path, paths: &[String]) -> Result<Vec<String>> {
    let mut args = vec![
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
        "--",
    ];
    if paths.is_empty() {
        args.push(".");
    } else {
        args.extend(paths.iter().map(String::as_str));
    }
    Ok(nul_paths(&git_bytes(
        root,
        &args,
        Duration::from_secs(20),
        None,
        None,
        &[0],
    )?)?
    .into_iter()
    .filter(|file| !is_skipped_content_path(file, true))
    .collect())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }

    fn repo() -> tempfile::TempDir {
        let repo = tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.name", "Fixture"]);
        git(
            repo.path(),
            &["config", "user.email", "fixture@example.test"],
        );
        repo
    }

    #[test]
    fn history_skips_unborn_branches_but_not_broken_repositories() {
        let repo = repo();
        let (hunks, commits) = recent_history(repo.path(), 20, 8).unwrap();
        assert!(hunks.is_empty() && commits.is_empty());
        fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/broken\n").unwrap();
        fs::write(repo.path().join(".git/refs/heads/broken"), "malformed\n").unwrap();
        assert!(recent_history(repo.path(), 20, 8).is_err());
        fs::remove_file(repo.path().join(".git/refs/heads/broken")).unwrap();
        fs::write(
            repo.path().join(".git/HEAD"),
            format!("{}\n", "a".repeat(40)),
        )
        .unwrap();
        assert!(recent_history(repo.path(), 20, 8).is_err());
        let non_repo = tempdir().unwrap();
        assert!(recent_history(non_repo.path(), 20, 8).is_err());
    }

    #[test]
    fn historical_sources_distinguish_roots_from_missing_parents_and_reject_links() {
        use crate::config::{ContextMode, ContextPolicy};
        let repo = repo();
        fs::write(repo.path().join("a.rs"), "old\n").unwrap();
        git(repo.path(), &["add", "a.rs"]);
        git(repo.path(), &["commit", "-qm", "root"]);
        let revision = |root: &Path| {
            git_text(root, &["rev-parse", "HEAD"], GIT_TIMEOUT, None, None, &[0])
                .unwrap()
                .trim()
                .to_owned()
        };
        let root_id = revision(repo.path());
        let snapshot = revision_snapshot(
            repo.path(),
            &root_id,
            &["a.rs".into()],
            &[],
            Instant::now() + GIT_TIMEOUT,
        )
        .unwrap();
        let diff = FileDiff {
            file: "a.rs".into(),
            text: "@@ -0,0 +1 @@\n+old\n".into(),
        };
        let policy = ContextPolicy {
            mode: ContextMode::ChangedFiles,
            ..ContextPolicy::default()
        };
        let sources = snapshot.select(&[&diff], &policy).unwrap();
        assert!(sources[0].before.is_none());
        assert_eq!(sources[0].after.as_ref().unwrap()[0].text, "old\n");
        fs::write(repo.path().join("a.rs"), "new\n").unwrap();
        symlink("a.rs", repo.path().join("link.rs")).unwrap();
        git(repo.path(), &["add", "a.rs", "link.rs"]);
        git(repo.path(), &["commit", "-qm", "next"]);
        let latest = revision(repo.path());
        let error = revision_snapshot(
            repo.path(),
            &latest,
            &["link.rs".into()],
            &[],
            Instant::now() + GIT_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ContextIncomplete);
        let shallow = tempdir().unwrap();
        git(
            shallow.path(),
            &[
                "clone",
                "-q",
                "--depth=1",
                &format!("file://{}", repo.path().display()),
                "copy",
            ],
        );
        let result = revision_snapshot(
            &shallow.path().join("copy"),
            &latest,
            &["a.rs".into()],
            &[],
            Instant::now() + GIT_TIMEOUT,
        );
        assert!(result.is_err(), "a shallow boundary is not a root commit");
    }

    #[test]
    fn working_tree_includes_staged_and_untracked_but_not_excluded_content() {
        let repo = repo();
        let global = tempdir().unwrap();
        let ignores = global.path().join("ignore");
        fs::write(&ignores, "a.rs\ndeleted.rs\nignored.rs\n.codex/\n").unwrap();
        git(
            repo.path(),
            &["config", "core.excludesFile", ignores.to_str().unwrap()],
        );
        fs::write(repo.path().join("a.rs"), "one\n").unwrap();
        fs::write(repo.path().join("deleted.rs"), "remove me\n").unwrap();
        fs::create_dir(repo.path().join(".codex")).unwrap();
        fs::write(repo.path().join(".codex/hooks.json"), "{}\n").unwrap();
        fs::write(repo.path().join(".env"), "SECRET=old\n").unwrap();
        fs::write(repo.path().join("Cargo.lock"), "old fixture\n").unwrap();
        fs::create_dir(repo.path().join(".ordain")).unwrap();
        fs::write(repo.path().join(".ordain/rubric.json"), "{}\n").unwrap();
        git(repo.path(), &["add", "-f", "."]);
        git(repo.path(), &["commit", "-qm", "init"]);
        fs::write(repo.path().join("a.rs"), "two\n").unwrap();
        fs::write(repo.path().join("b.rs"), "new\n").unwrap();
        fs::write(repo.path().join("ignored.rs"), "do not collect\n").unwrap();
        fs::write(
            repo.path().join(".codex/untracked.json"),
            "do not collect\n",
        )
        .unwrap();
        fs::remove_file(repo.path().join("deleted.rs")).unwrap();
        fs::write(repo.path().join(".env"), "SECRET=x\n").unwrap();
        fs::write(repo.path().join("Cargo.lock"), "fixture\n").unwrap();
        fs::write(repo.path().join(".ordain/rubric.json"), "{}\n").unwrap();
        // Delayed collection must still see same-size edits, including after pruning exclusions.
        std::thread::sleep(Duration::from_millis(1100));
        let tree = snapshot_tree(repo.path(), GIT_TIMEOUT).unwrap();
        let names = git_text(
            repo.path(),
            &["ls-tree", "-r", "--name-only", &tree],
            GIT_TIMEOUT,
            None,
            None,
            &[0],
        )
        .unwrap();
        assert!(names.lines().any(|name| name == ".codex/hooks.json"));
        assert!(!names.lines().any(|name| name == ".codex/untracked.json"));
        assert!(
            !names
                .lines()
                .any(|name| { matches!(name, ".env" | "Cargo.lock" | ".ordain/rubric.json") })
        );
        let files = split_diff(&working_tree_diff(repo.path(), &[]).unwrap()).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.file.as_str())
                .collect::<Vec<_>>(),
            ["a.rs", "b.rs", "deleted.rs"]
        );
    }

    #[test]
    fn snapshot_preserves_real_index_and_rejects_index_copy_symlinks() {
        let repo = repo();
        fs::write(repo.path().join("a.rs"), "one\n").unwrap();
        git(repo.path(), &["add", "a.rs"]);
        let index = index_path(repo.path(), GIT_TIMEOUT).unwrap();
        let before = fs::read(&index).unwrap();

        let home = tempdir().unwrap();
        let copy = home.path().join("copied-index");
        copy_index_without_links(&index, &copy).unwrap();
        assert_eq!(
            fs::metadata(&index).unwrap().modified().unwrap(),
            fs::metadata(copy).unwrap().modified().unwrap()
        );
        let sentinel = home.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        let linked_index = home.path().join("linked-index");
        symlink(&sentinel, &linked_index).unwrap();
        assert!(copy_index_without_links(&index, &linked_index).is_err());
        snapshot_tree(repo.path(), GIT_TIMEOUT).unwrap();

        assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
        assert_eq!(fs::read(index).unwrap(), before);
    }
}
