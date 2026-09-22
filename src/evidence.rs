//! Immutable source evidence. Historical diffs never borrow today's working tree.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{ContextMode, ContextPolicy, MAX_CONTEXT_BYTES};
use crate::error::{ErrorCode, OrdainError, Result};
use crate::model::FileDiff;
use crate::paths::{compile_globs, is_excluded_path, read_regular_text};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRange {
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEvidence {
    pub file: String,
    pub before: Option<Vec<SourceRange>>,
    pub after: Option<Vec<SourceRange>>,
    pub complete_file: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub snapshot: String,
    pub mode: ContextMode,
    pub files: Vec<String>,
    pub complete_files: bool,
    pub request_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub files: BTreeMap<String, std::sync::Arc<FileSnapshot>>,
    related_patterns: Vec<String>,
    related_paths: BTreeSet<String>,
}

impl Snapshot {
    /// Caller-provided complete tool evidence, checked against disk before delivery.
    pub fn insert(&mut self, file: String, before: Option<String>, after: Option<String>) {
        self.files
            .insert(file, std::sync::Arc::new(FileSnapshot { before, after }));
    }

    /// Capture a current-worktree diff. Do not call this for replay/imported patches.
    pub fn capture(root: &Path, diffs: &[FileDiff], deadline: Instant) -> Result<Self> {
        let mut snapshot = Self::default();
        let mut bytes = 0;
        for diff in diffs {
            check_deadline(deadline)?;
            let after = read_source(root, &diff.file)?;
            let patch = format!("--- a\n+++ b\n{}\n", diff.text);
            let parsed = diffy::Patch::from_str(&patch);
            if parsed.is_err() && !diff.text.starts_with("@@ -") {
                let before = reverse_native_patch(after.as_deref().unwrap_or(""), &diff.text)?;
                bytes += before.len() + after.as_ref().map_or(0, String::len);
                if bytes > 16 * MAX_CONTEXT_BYTES {
                    return Err(incomplete("snapshot exceeds memory safety ceiling"));
                }
                snapshot.insert(diff.file.clone(), Some(before), after);
                continue;
            }
            let patch = parsed.map_err(|_| {
                incomplete(format!(
                    "{}: patch lacks reconstructible source ranges",
                    diff.file
                ))
            })?;
            let before =
                diffy::apply(after.as_deref().unwrap_or(""), &patch.reverse()).map_err(|_| {
                    incomplete(format!("{}: diff does not match captured file", diff.file))
                })?;
            // Applying the reverse must not relocate hunks to some other matching text.
            let lines: Vec<_> = after
                .as_deref()
                .unwrap_or("")
                .split_inclusive('\n')
                .collect();
            for hunk in patch.hunks() {
                let expected: String = hunk
                    .lines()
                    .iter()
                    .filter_map(|line| match line {
                        diffy::Line::Context(s) | diffy::Line::Insert(s) => Some(*s),
                        _ => None,
                    })
                    .collect();
                let range = hunk.new_range();
                let start = range.start().saturating_sub(1);
                if start > lines.len()
                    || start.saturating_add(range.len()) > lines.len()
                    || lines[start..start + range.len()].concat() != expected
                {
                    return Err(incomplete(format!(
                        "{}: captured hunk positions changed",
                        diff.file
                    )));
                }
            }
            bytes += before.len() + after.as_ref().map_or(0, String::len);
            if bytes > 16 * MAX_CONTEXT_BYTES {
                return Err(incomplete("snapshot exceeds memory safety ceiling"));
            }
            snapshot.insert(diff.file.clone(), Some(before), after);
        }
        snapshot.ensure_current(root, deadline)?;
        Ok(snapshot)
    }

    pub fn capture_related(
        &mut self,
        root: &Path,
        patterns: &[String],
        deadline: Instant,
    ) -> Result<()> {
        let paths = related_paths(root, patterns, deadline)?;
        let mut bytes: usize = self
            .files
            .values()
            .map(|f| {
                f.before.as_ref().map_or(0, String::len) + f.after.as_ref().map_or(0, String::len)
            })
            .sum();
        if bytes > 16 * MAX_CONTEXT_BYTES {
            return Err(incomplete("snapshot exceeds memory safety ceiling"));
        }
        for path in &paths {
            if self.files.contains_key(path) {
                continue;
            }
            check_deadline(deadline)?;
            let after = read_source(root, path)?
                .ok_or_else(|| incomplete(format!("required related file disappeared: {path}")))?;
            bytes += after.len();
            if bytes > 16 * MAX_CONTEXT_BYTES {
                return Err(incomplete("related evidence exceeds memory safety ceiling"));
            }
            self.insert(path.clone(), None, Some(after));
        }
        self.related_patterns = patterns.to_vec();
        self.related_paths = paths;
        self.ensure_current(root, deadline)
    }

    pub fn ensure_current(&self, root: &Path, deadline: Instant) -> Result<()> {
        for (path, snapshot) in &self.files {
            check_deadline(deadline)?;
            if read_source(root, path)? != snapshot.after {
                return Err(OrdainError::new(
                    ErrorCode::Superseded,
                    format!("{path} changed after evidence was captured; finding withheld"),
                ));
            }
        }
        if !self.related_patterns.is_empty()
            && related_paths(root, &self.related_patterns, deadline)? != self.related_paths
        {
            return Err(OrdainError::new(
                ErrorCode::Superseded,
                "related-file selection changed; finding withheld",
            ));
        }
        Ok(())
    }

    pub fn select(
        &self,
        files: &[&FileDiff],
        policy: &ContextPolicy,
    ) -> Result<Vec<SourceEvidence>> {
        let mut selected: BTreeSet<_> = files.iter().map(|f| f.file.clone()).collect();
        let matcher = if policy.include.is_empty() {
            globset::GlobSet::empty()
        } else {
            compile_globs(&policy.include).map_err(incomplete)?
        };
        if !policy.include.is_empty() {
            for pattern in &policy.include {
                let one = compile_globs(std::slice::from_ref(pattern)).map_err(incomplete)?;
                if !self.files.keys().any(|p| one.is_match(p)) {
                    return Err(incomplete(format!(
                        "required context pattern has no captured match: {pattern}"
                    )));
                }
            }
            selected.extend(self.files.keys().filter(|p| matcher.is_match(p)).cloned());
        }
        let mut output = Vec::new();
        for path in selected {
            let Some(file) = self.files.get(&path) else {
                return Err(incomplete(format!("no captured source for {path}")));
            };
            let changed = files.iter().any(|f| f.file == path);
            if policy.mode == ContextMode::Diff && changed && !matcher.is_match(&path) {
                continue;
            }
            let ranges = policy.mode == ContextMode::ChangedRanges && changed;
            let (before, after) = if ranges {
                let patch = diffy::create_patch(
                    file.before.as_deref().unwrap_or(""),
                    file.after.as_deref().unwrap_or(""),
                );
                let old: Vec<_> = patch.hunks().iter().map(|h| h.old_range()).collect();
                let new: Vec<_> = patch.hunks().iter().map(|h| h.new_range()).collect();
                (
                    file.before
                        .as_deref()
                        .map(|s| excerpts(s, &old, policy.surrounding_lines)),
                    file.after
                        .as_deref()
                        .map(|s| excerpts(s, &new, policy.surrounding_lines)),
                )
            } else {
                (
                    file.before.as_deref().map(full),
                    file.after.as_deref().map(full),
                )
            };
            output.push(SourceEvidence {
                file: path,
                before,
                after,
                complete_file: !ranges,
            });
        }
        Ok(output)
    }
}

fn full(text: &str) -> Vec<SourceRange> {
    vec![SourceRange {
        start_line: 1,
        end_line: text.lines().count(),
        text: text.into(),
    }]
}

fn excerpts(text: &str, hunks: &[diffy::HunkRange], context: usize) -> Vec<SourceRange> {
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let mut ranges = Vec::<std::ops::Range<usize>>::new();
    for hunk in hunks {
        let start = hunk
            .start()
            .saturating_sub(1)
            .saturating_sub(context)
            .min(lines.len());
        let end = hunk
            .start()
            .saturating_sub(1)
            .saturating_add(hunk.len())
            .saturating_add(context)
            .min(lines.len());
        if let Some(last) = ranges.last_mut()
            && last.end >= start
        {
            last.end = last.end.max(end);
        } else {
            ranges.push(start..end);
        }
    }
    ranges
        .into_iter()
        .map(|range| SourceRange {
            start_line: range.start + 1,
            end_line: range.end,
            text: lines[range].concat(),
        })
        .collect()
}

pub fn fingerprint(diff: &str, sources: &[SourceEvidence]) -> String {
    let mut hash = Sha256::new();
    hash.update(diff);
    hash.update(serde_json::to_vec(sources).expect("source evidence serializes"));
    hex::encode(hash.finalize())
}

pub fn read_source(root: &Path, relative: &str) -> Result<Option<String>> {
    if is_excluded_path(relative)
        || Path::new(relative)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(incomplete(format!(
            "excluded or unsafe context path: {relative}"
        )));
    }
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(incomplete(format!("symlink context path: {relative}")));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            _ => {}
        }
    }
    read_regular_text(&path, MAX_CONTEXT_BYTES as u64, false)
        .map(Some)
        .ok_or_else(|| {
            incomplete(format!(
                "context file is not bounded regular UTF-8: {relative}"
            ))
        })
}

fn related_paths(root: &Path, patterns: &[String], deadline: Instant) -> Result<BTreeSet<String>> {
    if patterns.is_empty() {
        return Ok(BTreeSet::new());
    }
    let globs = compile_globs(patterns).map_err(incomplete)?;
    let mut paths = BTreeSet::new();
    for (count, entry) in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".ordain" | "target" | "node_modules")
                )
        })
        .enumerate()
    {
        check_deadline(deadline)?;
        if count >= 20_000 {
            return Err(incomplete(
                "context discovery exceeded entry limit; narrow includes",
            ));
        }
        let entry = entry.map_err(|error| incomplete(error.to_string()))?;
        let relative = entry.path().strip_prefix(root).expect("walk rooted");
        if globs.is_match(relative) && !entry.file_type().is_dir() {
            let relative = crate::paths::posix(relative);
            if is_excluded_path(&relative) || entry.file_type().is_symlink() {
                return Err(incomplete(format!(
                    "required context includes excluded path: {relative}"
                )));
            }
            paths.insert(relative);
            if paths.len() > 1000 {
                return Err(incomplete("too many related context files"));
            }
        }
    }
    for pattern in patterns {
        let one = compile_globs(std::slice::from_ref(pattern)).map_err(incomplete)?;
        if !paths.iter().any(|p| one.is_match(p)) {
            return Err(incomplete(format!(
                "required context pattern has no match: {pattern}"
            )));
        }
    }
    Ok(paths)
}

pub fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(OrdainError::new(
            ErrorCode::CheckTimeout,
            "evidence collection exceeded deadline",
        ))
    } else {
        Ok(())
    }
}
pub fn incomplete(message: impl Into<String>) -> OrdainError {
    OrdainError::new(ErrorCode::ContextIncomplete, message)
}

/// V4A omits line coordinates. Accept a prior observation only if forwarding the
/// patch reproduces the entire current file; otherwise require unique post-images.
pub fn reconstruct_native_before(
    after: &str,
    patch: &str,
    previous: Option<&str>,
) -> Result<String> {
    let chunks = native_chunks(patch)?;
    if let Some(previous) = previous {
        let mut candidate = previous.to_owned();
        let valid = chunks.iter().all(|(old, new)| {
            if old.is_empty() {
                return false;
            }
            let mut matches = candidate.match_indices(old);
            let Some((position, _)) = matches.next() else {
                return false;
            };
            if matches.next().is_some() {
                return false;
            }
            candidate.replace_range(position..position + old.len(), new);
            true
        });
        // A cached observation is only a candidate, never proof: shell edits and
        // overlapping hooks may have invalidated it since it was recorded.
        if valid && candidate == after {
            return Ok(previous.to_owned());
        }
    }
    reverse_native_chunks(after, chunks)
}

fn reverse_native_patch(after: &str, patch: &str) -> Result<String> {
    reconstruct_native_before(after, patch, None)
}

fn native_chunks(patch: &str) -> Result<Vec<(String, String)>> {
    let mut chunks = Vec::<(String, String)>::new();
    let mut old = String::new();
    let mut new = String::new();
    for line in patch.lines() {
        if line.starts_with("@@") {
            if !old.is_empty() || !new.is_empty() {
                chunks.push((std::mem::take(&mut old), std::mem::take(&mut new)));
            }
            continue;
        }
        let (kind, text) = line
            .split_at_checked(1)
            .ok_or_else(|| incomplete("empty V4A patch line"))?;
        if matches!(kind, " " | "-") {
            old.push_str(text);
            old.push('\n');
        }
        if matches!(kind, " " | "+") {
            new.push_str(text);
            new.push('\n');
        }
        if !matches!(kind, " " | "+" | "-") {
            return Err(incomplete("unsupported V4A patch record"));
        }
    }
    if !old.is_empty() || !new.is_empty() {
        chunks.push((old, new));
    }
    Ok(chunks)
}

fn reverse_native_chunks(after: &str, chunks: Vec<(String, String)>) -> Result<String> {
    let mut before = after.to_owned();
    for (mut old, mut new) in chunks.into_iter().rev() {
        if new.is_empty() {
            return Err(incomplete("V4A deletion has no unique post-image anchor"));
        }
        if !before.contains(&new)
            && !after.ends_with('\n')
            && before.ends_with(new.trim_end_matches('\n'))
        {
            new.pop();
            if old.ends_with('\n') {
                old.pop();
            }
        }
        let positions: Vec<_> = before
            .match_indices(&new)
            .map(|(at, _)| at)
            .take(2)
            .collect();
        if positions.len() != 1 {
            return Err(incomplete(
                "V4A post-image is absent or ambiguous; turn snapshot required",
            ));
        }
        before.replace_range(positions[0]..positions[0] + new.len(), &old);
    }
    Ok(before)
}
