use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;

use crate::model::Event;
use crate::paths::{events_path, project_state_dir};

pub const MAX_EVENT_LOG_BYTES: u64 = 8 * 1024 * 1024;
const RETAIN_EVENT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStatus {
    pub complete: bool,
    pub truncated: bool,
    pub corrupt_lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

impl Default for HistoryStatus {
    fn default() -> Self {
        Self {
            complete: true,
            truncated: false,
            corrupt_lines: 0,
            unavailable: None,
        }
    }
}

pub struct EventHistory {
    pub events: Vec<Event>,
    pub status: HistoryStatus,
}

pub fn append_event(root: &Path, event: &Event) {
    if let Err(error) = append(root, event) {
        eprintln!("ordain: event history could not be updated: {error}");
    }
}

fn append(root: &Path, event: &Event) -> std::io::Result<()> {
    let directory = project_state_dir(root);
    if fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(std::io::Error::other("state directory is a symbolic link"));
    }
    crate::state::create_private_dirs(&directory)?;
    let mut output = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(events_path(root))?;
    let _lock = FileLock::exclusive(&output)?;
    let mut bytes = serde_json::to_vec(event).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_EVENT_LOG_BYTES - 1024 {
        return Err(std::io::Error::other(
            "one event exceeds the event-history safety limit",
        ));
    }

    let length = output.metadata()?.len();
    if length.saturating_add(bytes.len() as u64) <= MAX_EVENT_LOG_BYTES {
        output.seek(SeekFrom::End(0))?;
        output.write_all(&bytes)?;
        output.flush()?;
        return Ok(());
    }

    let boundary = Event::HistoryBoundary {
        at: Utc::now().to_rfc3339(),
        reason: "older events removed at the 8 MiB retention limit".into(),
    };
    let mut boundary = serde_json::to_vec(&boundary).map_err(std::io::Error::other)?;
    boundary.push(b'\n');
    let retention_budget = MAX_EVENT_LOG_BYTES
        .saturating_sub(boundary.len() as u64)
        .saturating_sub(bytes.len() as u64)
        .min(RETAIN_EVENT_BYTES);
    let retained = read_tail(&mut output, length, retention_budget)?;
    output.set_len(0)?;
    output.seek(SeekFrom::Start(0))?;
    output.write_all(&boundary)?;
    output.write_all(&retained)?;
    output.write_all(&bytes)?;
    output.flush()
}

fn read_tail(file: &mut File, length: u64, limit: u64) -> std::io::Result<Vec<u8>> {
    let start = length.saturating_sub(limit);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(limit.min(length) as usize);
    file.take(limit).read_to_end(&mut bytes)?;
    if start > 0 {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    Ok(bytes)
}

pub fn read_events(root: &Path) -> EventHistory {
    let path = events_path(root);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return EventHistory {
                events: Vec::new(),
                status: HistoryStatus::default(),
            };
        }
        Err(error) => {
            return unavailable(format!("history file could not be opened: {error}"));
        }
    };
    let _lock = match FileLock::shared(&file) {
        Ok(lock) => lock,
        Err(error) => return unavailable(format!("history file could not be locked: {error}")),
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return unavailable("history path is not a regular file".into()),
        Err(error) => return unavailable(format!("history metadata could not be read: {error}")),
    };
    let legacy_truncated = metadata.len() > MAX_EVENT_LOG_BYTES;
    let limit = if legacy_truncated {
        RETAIN_EVENT_BYTES
    } else {
        metadata.len()
    };
    let bytes = match read_tail(&mut file, metadata.len(), limit) {
        Ok(bytes) => bytes,
        Err(error) => return unavailable(format!("history file could not be read: {error}")),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return unavailable("history file is not valid UTF-8".into()),
    };
    let mut events = Vec::new();
    let mut corrupt_lines = 0;
    let mut boundary = false;
    for line in text.lines() {
        match serde_json::from_str::<Event>(line) {
            Ok(event) => {
                boundary |= matches!(event, Event::HistoryBoundary { .. });
                events.push(event);
            }
            Err(_) if !line.trim().is_empty() => corrupt_lines += 1,
            Err(_) => {}
        }
    }
    let truncated = legacy_truncated || boundary;
    EventHistory {
        events,
        status: HistoryStatus {
            complete: !truncated && corrupt_lines == 0,
            truncated,
            corrupt_lines,
            unavailable: None,
        },
    }
}

fn unavailable(message: String) -> EventHistory {
    EventHistory {
        events: Vec::new(),
        status: HistoryStatus {
            complete: false,
            truncated: false,
            corrupt_lines: 0,
            unavailable: Some(message),
        },
    }
}

struct FileLock {
    fd: i32,
}

impl FileLock {
    fn exclusive(file: &File) -> std::io::Result<Self> {
        Self::acquire(file, libc::LOCK_EX)
    }

    fn shared(file: &File) -> std::io::Result<Self> {
        Self::acquire(file, libc::LOCK_SH)
    }

    fn acquire(file: &File, mode: i32) -> std::io::Result<Self> {
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            // SAFETY: flock operates on the live descriptor borrowed for the lock lifetime.
            if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } == 0 {
                return Ok(Self {
                    fd: file.as_raw_fd(),
                });
            }
            let error = std::io::Error::last_os_error();
            let would_block = error
                .raw_os_error()
                .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN);
            if !would_block || Instant::now() >= deadline {
                return Err(error);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: this descriptor remains live until after the guard is dropped.
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use tempfile::tempdir;

    #[test]
    fn retention_is_bounded_and_visible() {
        let repo = tempdir().unwrap();
        let event = Event::Error {
            at: "fixture".into(),
            phase: "fixture".into(),
            session_id: None,
            code: "FIXTURE".into(),
            message: "x".repeat(16 * 1024),
            latency_ms: None,
        };
        for _ in 0..600 {
            append(repo.path(), &event).unwrap();
        }
        assert!(fs::metadata(events_path(repo.path())).unwrap().len() <= MAX_EVENT_LOG_BYTES);
        let history = read_events(repo.path());
        assert!(history.status.truncated);
        assert!(!history.status.complete);
        assert!(!history.events.is_empty());
    }

    #[test]
    fn corrupt_history_is_not_reported_as_empty_and_clean() {
        let repo = tempdir().unwrap();
        fs::create_dir_all(project_state_dir(repo.path())).unwrap();
        fs::write(events_path(repo.path()), b"not json\n").unwrap();
        let history = read_events(repo.path());
        assert_eq!(history.status.corrupt_lines, 1);
        assert!(!history.status.complete);
    }

    #[test]
    fn linked_history_is_neither_followed_nor_reported_as_clean() {
        let repo = tempdir().unwrap();
        fs::create_dir_all(project_state_dir(repo.path())).unwrap();
        let sentinel = repo.path().join("sentinel");
        fs::write(&sentinel, "unchanged").unwrap();
        symlink(&sentinel, events_path(repo.path())).unwrap();
        let event = Event::HistoryBoundary {
            at: "fixture".into(),
            reason: "fixture".into(),
        };
        assert!(append(repo.path(), &event).is_err());
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
        let history = read_events(repo.path());
        assert!(!history.status.complete);
        assert!(history.status.unavailable.is_some());
    }
}
