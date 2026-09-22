use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

pub const MAX_FILE_READ_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_DIFF_INPUT_CHARS: usize = 1_000_000;
pub const MAX_STATE_CHARS: usize = 24_000;
pub const MAX_TASK_CHARS: usize = 64 * 1024;
pub const INCOMPLETE_TASK: &str = "[Ordain: task context exceeded capture limit]";
pub fn capture_task(text: &str) -> String {
    if text.chars().take(MAX_TASK_CHARS + 1).count() > MAX_TASK_CHARS {
        INCOMPLETE_TASK.into()
    } else {
        text.into()
    }
}

pub fn home_dir() -> PathBuf {
    env::var_os("ORDAIN_HOME_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// User configuration only; persistent state has a separate XDG location.
pub fn global_dir() -> PathBuf {
    xdg_path(
        "XDG_CONFIG_HOME",
        ".config",
        xdg::BaseDirectories::new().get_config_home(),
    )
}

pub fn state_dir() -> PathBuf {
    xdg_path(
        "XDG_STATE_HOME",
        ".local/state",
        xdg::BaseDirectories::new().get_state_home(),
    )
}

pub fn cache_dir() -> PathBuf {
    xdg_path(
        "XDG_CACHE_HOME",
        ".cache",
        xdg::BaseDirectories::new().get_cache_home(),
    )
}

fn xdg_path(variable: &str, fallback: &str, standard: Option<PathBuf>) -> PathBuf {
    // Keep explicit isolated-home invocations isolated, while honouring absolute XDG overrides.
    let absolute_override = env::var_os(variable).is_some_and(|p| Path::new(&p).is_absolute());
    let directory = if env::var_os("ORDAIN_HOME_DIR").is_some() && !absolute_override {
        home_dir().join(fallback)
    } else {
        standard.unwrap_or_else(|| home_dir().join(fallback))
    };
    directory.join("ordain")
}

pub fn project_state_dir(root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let identity = hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes()));
    state_dir().join("projects").join(identity)
}

pub fn global_rubric_path() -> PathBuf {
    global_dir().join("global.json")
}

pub fn sessions_dir() -> PathBuf {
    state_dir().join("sessions")
}

pub fn ordain_dir(root: &Path) -> PathBuf {
    root.join(".ordain")
}

pub fn rubric_path(root: &Path) -> PathBuf {
    ordain_dir(root).join("rubric.json")
}

pub fn events_path(root: &Path) -> PathBuf {
    project_state_dir(root).join("events.jsonl")
}

pub fn find_repo_root(start: &Path) -> PathBuf {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent().unwrap_or(start).to_path_buf()
    };
    dir = fs::canonicalize(&dir).unwrap_or(dir);
    let mut fallback = None;
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if fallback.is_none()
            && [".ordain", "AGENTS.md", "CLAUDE.md"]
                .iter()
                .any(|marker| dir.join(marker).exists())
        {
            fallback = Some(dir.clone());
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent.to_path_buf();
    }
    fallback.unwrap_or_else(|| start.to_path_buf())
}

pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

pub fn resolve_source_path(root: &Path, source: &str) -> PathBuf {
    if source.starts_with('~') {
        expand_home(source)
    } else {
        root.join(source)
    }
}

pub fn to_source_path(root: &Path, absolute: &Path) -> String {
    let home = home_dir();
    if root != home
        && let Ok(relative) = absolute.strip_prefix(root)
        && !relative.as_os_str().is_empty()
    {
        return posix(relative);
    }
    if let Ok(relative) = absolute.strip_prefix(&home)
        && !relative.as_os_str().is_empty()
    {
        return format!("~/{}", posix(relative));
    }
    posix(absolute)
}

pub fn canonical_source_path(root: &Path, source: &str) -> String {
    let absolute = resolve_source_path(root, source);
    let clean = lexical_normalize(&absolute);
    to_source_path(root, &clean)
}

pub fn relative_to_root(root: &Path, absolute: &Path) -> Option<String> {
    let path = if absolute.is_absolute() {
        lexical_normalize(absolute)
    } else {
        lexical_normalize(&root.join(absolute))
    };
    path.strip_prefix(root).ok().map(posix)
}

pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn posix(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub fn is_ordain_owned(relative: &str) -> bool {
    relative == ".ordain" || relative.starts_with(".ordain/")
}

pub fn is_secret_file(relative: &str) -> bool {
    let name = Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name == ".env"
        || name.starts_with(".env.")
        || name == ".envrc"
        || name.ends_with(".pem")
        || name.ends_with(".key")
}

pub fn is_excluded_path(relative: &str) -> bool {
    is_ordain_owned(relative) || is_secret_file(relative)
}

pub fn compile_globs(globs: &[String]) -> Result<GlobSet, String> {
    if globs.is_empty() {
        return Err("at least one glob is required".into());
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in globs {
        let glob = Glob::new(if pattern == "**/*" { "**" } else { pattern })
            .map_err(|error| format!("{pattern:?}: {error}"))?;
        builder.add(glob);
    }
    builder.build().map_err(|error| error.to_string())
}

pub fn is_skipped_content_path(relative: &str, audit: bool) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    if matches!(
        name,
        "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
            | "Cargo.lock"
            | "go.sum"
    ) {
        return true;
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    matches!(
        extension,
        "map"
            | "svg"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "ico"
            | "woff"
            | "woff2"
            | "ttf"
            | "pdf"
            | "lock"
            | "snap"
    ) || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || (audit && extension == "jsonl")
        || is_excluded_path(relative)
}

pub fn read_regular(path: &Path, max_bytes: u64, follow_symlinks: bool) -> Option<Vec<u8>> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NONBLOCK);
    if !follow_symlinks {
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    let maximum = usize::try_from(max_bytes).unwrap_or(usize::MAX.saturating_sub(1));
    let initial = usize::try_from(metadata.len().min(max_bytes).min(64 * 1024)).ok()?;
    let mut bytes = Vec::with_capacity(initial);
    use std::io::Read;
    let mut file = file;
    let mut buffer = [0_u8; 8192];
    loop {
        let remaining = maximum.saturating_add(1).saturating_sub(bytes.len());
        if remaining == 0 {
            return None;
        }
        let chunk = remaining.min(buffer.len());
        let read = file.read(&mut buffer[..chunk]).ok()?;
        if read == 0 {
            break;
        }
        bytes.try_reserve_exact(read).ok()?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    Some(bytes)
}

pub fn read_regular_text(path: &Path, max_bytes: u64, follow_symlinks: bool) -> Option<String> {
    String::from_utf8(read_regular(path, max_bytes, follow_symlinks)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_and_generated_files_are_excluded() {
        assert!(is_excluded_path("a/.env.local"));
        assert!(is_excluded_path(".ordain/events.jsonl"));
        assert!(is_skipped_content_path("Cargo.lock", false));
        assert!(!is_skipped_content_path("src/main.rs", false));
    }

    #[test]
    fn glob_scopes_include_dotfiles() {
        assert!(
            compile_globs(&["**/*.ts".into()])
                .unwrap()
                .is_match("a/.hidden/x.ts")
        );
        assert!(
            compile_globs(&["src/**".into()])
                .unwrap()
                .is_match("src/a.rs")
        );
        assert!(
            !compile_globs(&["src/**".into()])
                .unwrap()
                .is_match("tests/a.rs")
        );
        assert!(compile_globs(&["[".into()]).is_err());
    }
}
