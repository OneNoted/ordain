use std::path::{Path, PathBuf};

use serde_json::Value;
use similar::TextDiff;

use crate::error::{ErrorCode, OrdainError, Result};
use crate::model::FileDiff;
use crate::paths::{MAX_DIFF_INPUT_CHARS, MAX_STATE_CHARS, read_regular_text};

#[derive(Debug, Clone)]
pub struct EditHunk {
    pub file_path: PathBuf,
    pub text: Option<String>,
    pub original: Option<String>,
    pub after: Option<String>,
}

pub fn unified_hunks(before: &str, after: &str) -> Option<String> {
    if before.len() + after.len() > MAX_DIFF_INPUT_CHARS {
        return None;
    }
    let patch = TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header("a", "b")
        .to_string();
    Some(
        patch
            .lines()
            .skip_while(|line| !line.starts_with("@@"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

pub fn unified_file_diff(relative: &str, before: &str, after: &str) -> Option<String> {
    let hunks = unified_hunks(before, after)?;
    if hunks.is_empty() {
        Some(String::new())
    } else {
        Some(format!("--- a/{relative}\n+++ b/{relative}\n{hunks}\n"))
    }
}

fn all_added(content: &str) -> String {
    let lines: Vec<_> = content
        .strip_suffix('\n')
        .unwrap_or(content)
        .split('\n')
        .collect();
    let count = if content.is_empty() { 0 } else { lines.len() };
    format!(
        "@@ -0,0 +1,{count} @@\n{}",
        if count == 0 {
            String::new()
        } else {
            lines
                .into_iter()
                .map(|line| format!("+{line}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    )
}

fn apply_replacement(mut content: String, old: &str, new: &str, all: bool) -> String {
    if all {
        content = content.replace(old, new);
    } else if let Some(index) = content.find(old) {
        content.replace_range(index..index + old.len(), new);
    }
    content
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name)?.as_str()
}

fn host_patch(response: Option<&Value>) -> Option<String> {
    // OpenCode persists unified text; Claude persists structured hunks.
    if let Some(diff) = response?.get("diff").and_then(Value::as_str)
        && diff.len() <= MAX_DIFF_INPUT_CHARS
        && (diff.starts_with("@@ ") || diff.contains("\n@@ "))
    {
        return Some(diff.to_owned());
    }
    let hunks = response?.get("structuredPatch")?.as_array()?;
    if hunks.is_empty() {
        return None;
    }
    let mut rendered = Vec::new();
    for hunk in hunks {
        let old_start = hunk.get("oldStart")?.as_i64()?;
        let old_lines = hunk.get("oldLines")?.as_i64()?;
        let new_start = hunk.get("newStart")?.as_i64()?;
        let new_lines = hunk.get("newLines")?.as_i64()?;
        let lines = hunk
            .get("lines")?
            .as_array()?
            .iter()
            .map(|line| line.as_str().map(ToOwned::to_owned))
            .collect::<Option<Vec<_>>>()?;
        rendered.push(format!(
            "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@\n{}",
            lines.join("\n")
        ));
    }
    Some(rendered.join("\n"))
}

pub fn edits_from_payload(raw: &Value) -> Vec<EditHunk> {
    parse_edits(raw, true)
}

/// Reconstruct only recorded evidence; today's working tree is not historical state.
pub fn edits_from_recorded_payload(raw: &Value) -> Vec<EditHunk> {
    parse_edits(raw, false)
}

fn parse_edits(raw: &Value, allow_working_tree: bool) -> Vec<EditHunk> {
    let Some(tool) = field(raw, "tool_name") else {
        return Vec::new();
    };
    let Some(input) = raw.get("tool_input") else {
        return Vec::new();
    };
    let response = raw.get("tool_response");
    match tool {
        "Edit" => {
            let (Some(path), Some(old), Some(new)) = (
                field(input, "file_path"),
                field(input, "old_string"),
                field(input, "new_string"),
            ) else {
                return Vec::new();
            };
            let all = input
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let original = response
                .and_then(|value| value.get("originalFile"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| {
                    if !allow_working_tree {
                        return None;
                    }
                    let current = read_regular_text(Path::new(path), 16 * 1024 * 1024, true)?;
                    Some(apply_replacement(current, new, old, all))
                });
            let after = original
                .as_ref()
                .map(|value| apply_replacement(value.clone(), old, new, all));
            vec![EditHunk {
                file_path: PathBuf::from(path),
                text: host_patch(response).or_else(|| unified_hunks(old, new)),
                original,
                after,
            }]
        }
        "Write" => {
            let (Some(path), Some(content)) = (field(input, "file_path"), field(input, "content"))
            else {
                return Vec::new();
            };
            let original_value = response.and_then(|value| value.get("originalFile"));
            let original = original_value
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let is_new = original_value.is_none_or(Value::is_null);
            vec![EditHunk {
                file_path: PathBuf::from(path),
                text: host_patch(response).or_else(|| {
                    if !allow_working_tree && original_value.is_none() {
                        None
                    } else if is_new {
                        (content.len() <= MAX_DIFF_INPUT_CHARS).then(|| all_added(content))
                    } else {
                        original
                            .as_ref()
                            .and_then(|before| unified_hunks(before, content))
                    }
                }),
                original,
                after: Some(content.to_owned()),
            }]
        }
        "MultiEdit" => {
            let Some(path) = field(input, "file_path") else {
                return Vec::new();
            };
            let edits = input
                .get("edits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut original = response
                .and_then(|value| value.get("originalFile"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if original.is_none() && allow_working_tree {
                original = read_regular_text(Path::new(path), 16 * 1024 * 1024, true);
                if let Some(value) = &mut original {
                    for edit in edits.iter().rev() {
                        if let (Some(old), Some(new)) =
                            (field(edit, "old_string"), field(edit, "new_string"))
                        {
                            *value = apply_replacement(
                                value.clone(),
                                new,
                                old,
                                edit.get("replace_all")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                            );
                        }
                    }
                }
            }
            let mut after = original.clone();
            let mut parts = Vec::new();
            for edit in &edits {
                let (Some(old), Some(new)) = (field(edit, "old_string"), field(edit, "new_string"))
                else {
                    continue;
                };
                if let Some(part) = unified_hunks(old, new) {
                    parts.push(part);
                }
                if let Some(value) = &mut after {
                    *value = apply_replacement(
                        value.clone(),
                        old,
                        new,
                        edit.get("replace_all")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    );
                }
            }
            vec![EditHunk {
                file_path: PathBuf::from(path),
                text: host_patch(response)
                    .or_else(|| (!parts.is_empty()).then(|| parts.join("\n"))),
                original,
                after,
            }]
        }
        "apply_patch" => {
            let (Some(cwd), Some(command)) = (field(raw, "cwd"), field(input, "command")) else {
                return Vec::new();
            };
            parse_apply_patch(command)
                .into_iter()
                .filter(|file| file.kind != PatchKind::Delete)
                .map(|file| EditHunk {
                    file_path: Path::new(cwd).join(file.moved_to.as_deref().unwrap_or(&file.path)),
                    text: Some(file.text),
                    original: None,
                    after: None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchKind {
    Add,
    Update,
    Delete,
}

struct PatchedFile {
    kind: PatchKind,
    path: String,
    moved_to: Option<String>,
    text: String,
}

fn parse_apply_patch(command: &str) -> Vec<PatchedFile> {
    let mut files = Vec::new();
    let mut open: Option<PatchedFile> = None;
    let close = |open: &mut Option<PatchedFile>, files: &mut Vec<PatchedFile>| {
        if let Some(file) = open.take() {
            files.push(file);
        }
    };
    for line in command.lines() {
        if line == "*** Begin Patch" || line == "*** End Patch" {
            close(&mut open, &mut files);
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            close(&mut open, &mut files);
            open = Some(PatchedFile {
                kind: PatchKind::Add,
                path: path.trim().into(),
                moved_to: None,
                text: String::new(),
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            close(&mut open, &mut files);
            open = Some(PatchedFile {
                kind: PatchKind::Update,
                path: path.trim().into(),
                moved_to: None,
                text: String::new(),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            close(&mut open, &mut files);
            files.push(PatchedFile {
                kind: PatchKind::Delete,
                path: path.trim().into(),
                moved_to: None,
                text: String::new(),
            });
        } else if let Some(target) = line.strip_prefix("*** Move to: ") {
            if let Some(file) = &mut open {
                file.moved_to = Some(target.trim().into());
            }
        } else if line == "*** End of File" {
        } else if let Some(file) = &mut open {
            let rendered = if let Some(context) = line.strip_prefix("@@") {
                let context = context.trim();
                if context.is_empty() {
                    "@@".into()
                } else {
                    format!("@@ {context}")
                }
            } else if file.kind == PatchKind::Add && !line.starts_with('+') {
                format!("+{line}")
            } else {
                line.into()
            };
            if !file.text.is_empty() {
                file.text.push('\n');
            }
            file.text.push_str(&rendered);
        }
    }
    close(&mut open, &mut files);
    files
}

pub fn split_diff(patch: &str) -> Result<Vec<FileDiff>> {
    let mut output = Vec::new();
    let mut old_name: Option<String> = None;
    let mut new_name: Option<String> = None;
    let mut hunks = Vec::new();
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            flush_diff(
                &mut output,
                &mut old_name,
                &mut new_name,
                &mut hunks,
                &mut in_hunk,
            );
        } else if !in_hunk && line.starts_with("--- ") {
            old_name = Some(parse_git_path(&line[4..])?);
        } else if !in_hunk && line.starts_with("+++ ") {
            new_name = Some(parse_git_path(&line[4..])?);
        } else if line.starts_with("@@") {
            in_hunk = true;
            hunks.push(line.to_owned());
        } else if in_hunk
            && (line.starts_with('+')
                || line.starts_with('-')
                || line.starts_with(' ')
                || line.starts_with("\\ No newline"))
        {
            hunks.push(line.to_owned());
        }
    }
    flush_diff(
        &mut output,
        &mut old_name,
        &mut new_name,
        &mut hunks,
        &mut in_hunk,
    );
    Ok(output)
}

fn flush_diff(
    output: &mut Vec<FileDiff>,
    old_name: &mut Option<String>,
    new_name: &mut Option<String>,
    hunks: &mut Vec<String>,
    in_hunk: &mut bool,
) {
    let selected = new_name
        .as_ref()
        .filter(|name| name.as_str() != "/dev/null")
        .or(old_name.as_ref());
    if let Some(name) = selected {
        let name = name
            .strip_prefix("a/")
            .or_else(|| name.strip_prefix("b/"))
            .unwrap_or(name);
        // Exclusion happens only after Git's pathname representation is decoded.
        if !hunks.is_empty() && !crate::paths::is_skipped_content_path(name, false) {
            output.push(FileDiff {
                file: name.to_owned(),
                text: hunks.join("\n"),
            });
        }
    }
    *old_name = None;
    *new_name = None;
    hunks.clear();
    *in_hunk = false;
}

fn parse_git_path(field: &str) -> Result<String> {
    if !field.starts_with('"') {
        let value = field.split('\t').next().unwrap_or_default();
        if value.is_empty() || value.bytes().any(|byte| byte == 0) {
            return Err(invalid_git_path());
        }
        return Ok(value.to_owned());
    }

    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 1;
    let mut closed = false;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index += 1;
                closed = true;
                break;
            }
            b'\\' => {
                index += 1;
                let Some(&escaped) = bytes.get(index) else {
                    return Err(invalid_git_path());
                };
                match escaped {
                    b'a' => decoded.push(0x07),
                    b'b' => decoded.push(0x08),
                    b't' => decoded.push(b'\t'),
                    b'n' => decoded.push(b'\n'),
                    b'v' => decoded.push(0x0b),
                    b'f' => decoded.push(0x0c),
                    b'r' => decoded.push(b'\r'),
                    b'\\' | b'"' => decoded.push(escaped),
                    b'0'..=b'7' => {
                        let mut value = escaped - b'0';
                        let mut digits = 1;
                        while digits < 3
                            && bytes
                                .get(index + 1)
                                .is_some_and(|byte| (b'0'..=b'7').contains(byte))
                        {
                            index += 1;
                            value = value * 8 + (bytes[index] - b'0');
                            digits += 1;
                        }
                        decoded.push(value);
                    }
                    _ => return Err(invalid_git_path()),
                }
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    let remainder = &field[index..];
    if !closed || !(remainder.is_empty() || remainder.starts_with('\t')) {
        return Err(invalid_git_path());
    }
    if decoded.contains(&0) {
        return Err(invalid_git_path());
    }
    String::from_utf8(decoded).map_err(|_| invalid_git_path())
}

fn invalid_git_path() -> OrdainError {
    OrdainError::new(
        ErrorCode::GitUnavailable,
        "git emitted an invalid quoted pathname",
    )
}

pub fn bound_state(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        (text.to_owned(), false)
    } else {
        let mut boundary = max;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        (
            format!(
                "{}\n[ordain: diff cut at {max} characters]",
                &text[..boundary]
            ),
            true,
        )
    }
}

pub fn default_bound_state(text: &str) -> (String, bool) {
    bound_state(text, MAX_STATE_CHARS)
}

pub fn file_as_chunks(content: &str, size: usize) -> Vec<String> {
    let lines: Vec<_> = content
        .strip_suffix('\n')
        .unwrap_or(content)
        .split('\n')
        .collect();
    if content.is_empty() {
        return Vec::new();
    }
    lines
        .chunks(size)
        .enumerate()
        .map(|(index, chunk)| {
            format!(
                "@@ -0,0 +{},{} @@\n{}",
                index * size + 1,
                chunk.len(),
                chunk
                    .iter()
                    .map(|line| format!("+{line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_multifile_patch_and_ignores_delete() {
        let value = serde_json::json!({
            "cwd":"/repo", "tool_name":"apply_patch",
            "tool_input":{"command":"*** Begin Patch\n*** Add File: a.rs\n+one\n*** Update File: b.rs\n*** Move to: c.rs\n@@\n-old\n+new\n*** Delete File: d.rs\n*** End Patch"}
        });
        let edits = edits_from_payload(&value);
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].file_path, PathBuf::from("/repo/a.rs"));
        assert_eq!(edits[1].file_path, PathBuf::from("/repo/c.rs"));
    }

    #[test]
    fn splits_git_patch_and_excludes_secrets() {
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/.env b/.env\n--- a/.env\n+++ b/.env\n@@ -0,0 +1 @@\n+KEY=x\n";
        let files = split_diff(patch).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file, "a.rs");
    }

    #[test]
    fn decodes_quoted_git_path_before_secret_filtering() {
        let patch = "diff --git x x\n--- \"a/n\\303\\244me/.env\"\n+++ \"b/n\\303\\244me/.env\"\n@@ -0,0 +1 @@\n+TOKEN=fixture\n";
        assert!(split_diff(patch).unwrap().is_empty());

        let patch = "diff --git x x\n--- /dev/null\n+++ \"b/n\\303\\244me/file.rs\"\n@@ -0,0 +1 @@\n+fn main() {}\n";
        assert_eq!(split_diff(patch).unwrap()[0].file, "näme/file.rs");
    }
}
