use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::Sha256;

use crate::paths::{
    MAX_DIFF_INPUT_CHARS, MAX_STATE_CHARS, MAX_TASK_CHARS, read_regular, read_regular_text,
    state_dir,
};

const STATE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn safe(part: &str) -> String {
    let cleaned = part
        .chars()
        .take(40)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!(
        "{}-{}",
        if cleaned.is_empty() { "id" } else { &cleaned },
        &short_hash(part)[..16]
    )
}

fn short_hash(value: &str) -> String {
    let digest = <Sha256 as sha2::Digest>::digest(value.as_bytes());
    hex::encode(digest)[..24].to_owned()
}

pub fn blob_id(content: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", content.len()).as_bytes());
    hasher.update(content);
    hex::encode(hasher.finalize())
}

pub fn turn_dir(root: &Path, session: &str, prompt: Option<&str>) -> PathBuf {
    crate::paths::project_state_dir(root)
        .join("sessions")
        .join(safe(session))
        .join(safe(prompt.unwrap_or("turn")))
}

fn write_private(path: &Path, contents: &[u8], create_new: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dirs(parent)?;
    }
    let mut options = OpenOptions::new();
    options
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let mut output = options.open(path)?;
    output.write_all(contents)
}

pub fn create_private_dirs(path: &Path) -> std::io::Result<()> {
    if !safe_state_ancestry(path, true) {
        return Err(std::io::Error::other(
            "Ordain state path contains a link or non-directory",
        ));
    }
    fs::create_dir_all(path)?;
    if !safe_state_ancestry(path, false) {
        return Err(std::io::Error::other(
            "Ordain state path contains a link or non-directory",
        ));
    }
    let owner_root = state_dir();
    let mut current = Some(path);
    while let Some(directory) = current {
        if !directory.starts_with(&owner_root) {
            break;
        }
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        current = directory.parent();
    }
    Ok(())
}

pub fn snapshot_scratch() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("ordain-snapshot-")
        .tempdir()
}

fn safe_state_ancestry(path: &Path, missing_ok: bool) -> bool {
    let root = state_dir();
    if !path.starts_with(&root) {
        return false;
    }
    let mut current = root;
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.file_type().is_dir() => return false,
            Ok(_) => {}
            Err(error) if missing_ok && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
        if current == path {
            return true;
        }
        let Some(component) = path
            .strip_prefix(&current)
            .ok()
            .and_then(|remaining| remaining.components().next())
        else {
            return false;
        };
        current.push(component.as_os_str());
    }
}

fn create_once(path: &Path, contents: &[u8]) -> bool {
    write_private(path, contents, true).is_ok()
}

fn count_prefix(directory: &Path, prefix: &str) -> usize {
    if !safe_state_ancestry(directory, false) {
        return 0;
    }
    fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .count()
}

fn increment(directory: &Path, prefix: &str) -> usize {
    for _ in 0..32 {
        let next = count_prefix(directory, prefix) + 1;
        if create_once(&directory.join(format!("{prefix}{next}")), b"") {
            return next;
        }
    }
    count_prefix(directory, prefix)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStart {
    pub path: String,
    pub original: Option<String>,
    #[serde(default = "true_value")]
    pub original_complete: bool,
}

fn true_value() -> bool {
    true
}

pub fn record_file_start(directory: &Path, path: &Path, original: Option<&str>) {
    let original_complete = original.is_none_or(|text| text.len() <= MAX_DIFF_INPUT_CHARS);
    let record = FileStart {
        path: path.to_string_lossy().into_owned(),
        original: original
            .filter(|text| text.len() <= MAX_DIFF_INPUT_CHARS)
            .map(ToOwned::to_owned),
        original_complete,
    };
    if let Ok(bytes) = serde_json::to_vec(&record) {
        create_once(
            &directory
                .join("files")
                .join(format!("{}.json", short_hash(&record.path))),
            &bytes,
        );
    }
}

fn read_records<T: for<'de> Deserialize<'de>>(directory: &Path) -> Vec<T> {
    if !safe_state_ancestry(directory, false) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            read_regular(&entry.path(), (MAX_DIFF_INPUT_CHARS + 4096) as u64, false)
        })
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect()
}

pub fn read_file_starts(directory: &Path) -> Vec<FileStart> {
    read_records(&directory.join("files"))
}

/// Last observed contents are candidates for reconstructing a later native edit.
/// Callers must validate the complete forward patch against the current file.
pub fn read_observed_file(directory: &Path, relative: &str) -> Option<String> {
    let directory = directory.join("observed");
    if !safe_state_ancestry(&directory, false) {
        return None;
    }
    read_regular_text(
        &directory.join(short_hash(relative)),
        crate::config::MAX_CONTEXT_BYTES as u64,
        false,
    )
}

pub fn record_observed_file(
    directory: &Path,
    relative: &str,
    content: &str,
) -> std::io::Result<()> {
    if content.len() > crate::config::MAX_CONTEXT_BYTES {
        return Err(std::io::Error::other(
            "observed source exceeds capture limit",
        ));
    }
    let directory = directory.join("observed");
    create_private_dirs(&directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary.write_all(content.as_bytes())?;
    temporary.persist(directory.join(short_hash(relative)))?;
    Ok(())
}

pub fn record_blocked_file(directory: &Path, relative: &str) {
    create_once(
        &directory.join("blocked").join(short_hash(relative)),
        relative.as_bytes(),
    );
}

pub fn read_blocked_files(directory: &Path) -> std::collections::HashSet<String> {
    let blocked = directory.join("blocked");
    if !safe_state_ancestry(&blocked, false) {
        return std::collections::HashSet::new();
    }
    fs::read_dir(blocked)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| read_regular_text(&entry.path(), MAX_STATE_CHARS as u64, false))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckedEdit {
    pub path: String,
    pub before: Option<String>,
    pub after: String,
    #[serde(default)]
    pub policy_revision: String,
}

pub fn record_checked(directory: &Path, record: &CheckedEdit) {
    if let Ok(bytes) = serde_json::to_vec(record) {
        create_once(
            &directory.join("checked").join(format!(
                "{}.json",
                short_hash(&String::from_utf8_lossy(&bytes))
            )),
            &bytes,
        );
    }
}

pub fn read_checked(directory: &Path) -> Vec<CheckedEdit> {
    read_records(&directory.join("checked"))
}

fn block_prefix(key: &str) -> String {
    format!("{}.", short_hash(key))
}

pub fn block_count(directory: &Path, key: &str) -> usize {
    count_prefix(&directory.join("blocks"), &block_prefix(key))
}

pub fn increment_block(directory: &Path, key: &str) -> usize {
    increment(&directory.join("blocks"), &block_prefix(key))
}

pub fn stop_check_count(directory: &Path) -> usize {
    count_prefix(&directory.join("stops"), "stop.")
}

pub fn increment_stop_checks(directory: &Path) -> usize {
    increment(&directory.join("stops"), "stop.")
}

pub fn write_prompt(directory: &Path, prompt: &str) {
    let bounded = crate::paths::capture_task(prompt);
    create_once(&directory.join("prompt"), bounded.as_bytes());
}

pub fn read_prompt(directory: &Path) -> Option<String> {
    if !safe_state_ancestry(directory, false) {
        return None;
    }
    let prompt = read_regular_text(&directory.join("prompt"), MAX_TASK_CHARS as u64 * 4, false)?;
    let prompt = prompt.trim();
    (!prompt.is_empty()).then(|| crate::paths::capture_task(prompt))
}

pub fn write_baseline(directory: &Path, tree: &str) {
    create_once(&directory.join("baseline"), tree.as_bytes());
}

pub fn read_baseline(directory: &Path) -> Option<String> {
    if !safe_state_ancestry(directory, false) {
        return None;
    }
    let tree = read_regular_text(&directory.join("baseline"), 128, false)?;
    let tree = tree.trim();
    ((tree.len() == 40 || tree.len() == 64) && tree.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| tree.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineStatus {
    Pending,
    Ok,
    Failed,
}

pub fn mark_baseline(directory: &Path, status: BaselineStatus) {
    let text = match status {
        BaselineStatus::Pending => "pending",
        BaselineStatus::Ok => "ok",
        BaselineStatus::Failed => "failed",
    };
    let _ = write_private(&directory.join("baseline-status"), text.as_bytes(), false);
}

pub fn read_baseline_status(directory: &Path) -> Option<BaselineStatus> {
    if !safe_state_ancestry(directory, false) {
        return None;
    }
    match read_regular_text(&directory.join("baseline-status"), 16, false)?.trim() {
        "pending" => Some(BaselineStatus::Pending),
        "ok" => Some(BaselineStatus::Ok),
        "failed" => Some(BaselineStatus::Failed),
        _ => None,
    }
}

pub fn has_turn_state(directory: &Path) -> bool {
    !read_file_starts(directory).is_empty()
        || read_baseline(directory).is_some()
        || read_baseline_status(directory).is_some()
}

pub fn clear_turn(directory: &Path) {
    if safe_state_ancestry(directory, false) {
        let _ = fs::remove_dir_all(directory);
    }
}

pub fn prune_old_turns(root: &Path) {
    let sessions = crate::paths::project_state_dir(root).join("sessions");
    if !safe_state_ancestry(&sessions, false) {
        return;
    }
    let Ok(entries) = fs::read_dir(&sessions) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_type()
            .is_ok_and(|file_type| !file_type.is_dir() || file_type.is_symlink())
        {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > STATE_MAX_AGE);
        if old && safe_state_ancestry(&entry.path(), false) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

pub fn edits_cover_file(start: Option<&str>, edits: &[CheckedEdit], now: &str) -> bool {
    let mut unused = edits.to_vec();
    let mut at = start.map(ToOwned::to_owned);
    while at.as_deref() != Some(now) {
        let Some(index) = unused.iter().position(|edit| edit.before == at) else {
            return false;
        };
        at = Some(unused.remove(index).after);
    }
    true
}

#[cfg(test)]
mod tests {

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn first_file_record_wins_and_counters_do_not_overwrite() {
        let home = tempdir().unwrap();
        let directory = turn_dir(home.path(), "s", Some("p"));
        record_file_start(&directory, Path::new("/r/a"), Some("one"));
        record_file_start(&directory, Path::new("/r/a"), Some("two"));
        assert_eq!(
            read_file_starts(&directory)[0].original.as_deref(),
            Some("one")
        );
        assert_eq!(increment_block(&directory, "x"), 1);
        assert_eq!(increment_block(&directory, "x"), 2);
        assert_eq!(block_count(&directory, "x"), 2);
        write_prompt(&directory, &"p".repeat(MAX_TASK_CHARS + 50));
        assert_eq!(
            read_prompt(&directory).unwrap(),
            crate::paths::INCOMPLETE_TASK
        );
        assert!(
            directory
                .components()
                .all(|component| component.as_os_str().len() < 128)
        );
        assert_eq!(
            fs::metadata(state_dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("prompt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(crate::paths::project_state_dir(home.path())).unwrap();
    }

    #[test]
    fn checked_edits_must_chain() {
        let edits = vec![
            CheckedEdit {
                path: "a".into(),
                before: None,
                after: "1".into(),
                policy_revision: String::new(),
            },
            CheckedEdit {
                path: "a".into(),
                before: Some("1".into()),
                after: "2".into(),
                policy_revision: String::new(),
            },
        ];
        assert!(edits_cover_file(None, &edits, "2"));
        assert!(!edits_cover_file(None, &edits[..1], "2"));
    }
}
