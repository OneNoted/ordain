use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{ErrorCode, OrdainError, Result};
use crate::paths::{MAX_FILE_READ_BYTES, global_dir, read_regular_text};

pub const TYPESAFE_KEY_ENV: &str = "TYPESAFE_AI_API_KEY";
pub const GATEWAY_KEY_ENV: &str = "AI_GATEWAY_API_KEY";
pub const NO_KEY_HINT: &str = "No API key found. Run \"ordain login\" with your TypeSafe key, or put TYPESAFE_AI_API_KEY in the environment or a .env file at the repo root.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    TypeSafe { key: String, from: String },
    Gateway { key: String, from: String },
    None,
}

impl Credentials {
    pub fn from(&self) -> Option<&str> {
        match self {
            Self::TypeSafe { from, .. } | Self::Gateway { from, .. } => Some(from),
            Self::None => None,
        }
    }

    pub fn label(&self) -> Option<&'static str> {
        match self {
            Self::TypeSafe { .. } => Some("TypeSafe"),
            Self::Gateway { .. } => Some("Vercel AI Gateway"),
            Self::None => None,
        }
    }
}

pub fn user_env_path() -> PathBuf {
    global_dir().join(".env")
}

pub fn project_env_path(root: &Path) -> PathBuf {
    root.join(".env.local")
}

pub fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for raw in text.lines() {
        let mut line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim_start();
        }
        let Some((name, raw_value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !matches!(name, TYPESAFE_KEY_ENV | GATEWAY_KEY_ENV) {
            continue;
        }
        let mut value = raw_value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        if !value.is_empty() {
            vars.insert(name.to_owned(), value.to_owned());
        }
    }
    vars
}

fn pick(vars: &HashMap<String, String>, from: impl Into<String>) -> Credentials {
    let from = from.into();
    if let Some(key) = vars.get(TYPESAFE_KEY_ENV) {
        return Credentials::TypeSafe {
            key: key.clone(),
            from,
        };
    }
    if let Some(key) = vars.get(GATEWAY_KEY_ENV) {
        return Credentials::Gateway {
            key: key.clone(),
            from,
        };
    }
    Credentials::None
}

pub fn find_credentials(root: &Path) -> Credentials {
    let environment = [TYPESAFE_KEY_ENV, GATEWAY_KEY_ENV]
        .into_iter()
        .filter_map(|name| {
            env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .map(|value| (name.to_owned(), value))
        })
        .collect();
    let found = pick(&environment, "the environment");
    if found != Credentials::None {
        return found;
    }
    for (file, label) in [
        (root.join(".env.local"), ".env.local".to_owned()),
        (root.join(".env"), ".env".to_owned()),
        (user_env_path(), user_env_path().display().to_string()),
    ] {
        if let Some(text) = read_regular_text(&file, MAX_FILE_READ_BYTES, false) {
            let found = pick(&parse_env_file(&text), label);
            if found != Credentials::None {
                return found;
            }
        }
    }
    Credentials::None
}

pub fn upsert_env_line(text: &str, name: &str, value: &str) -> String {
    let replacement = format!("{name}={value}");
    let mut lines: Vec<String> = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .filter(|line| !line.is_empty() || !text.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let prefix = format!("{name}=");
    let export_prefix = format!("export {name}");
    if let Some(index) = lines.iter().position(|line| {
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        compact.starts_with(&prefix) || compact.starts_with(&export_prefix.replace(' ', ""))
    }) {
        lines[index] = replacement;
    } else {
        lines.push(replacement);
    }
    format!("{}\n", lines.join("\n"))
}

pub fn save_key(file: &Path, name: &str, key: &str) -> Result<()> {
    let parent = file
        .parent()
        .ok_or_else(|| OrdainError::new(ErrorCode::KeyFileUnwritable, "key file has no parent"))?;
    if fs::symlink_metadata(parent).is_ok_and(|metadata| !metadata.file_type().is_dir()) {
        return Err(unwritable(file, "parent is not a plain directory"));
    }
    fs::create_dir_all(parent).map_err(|_| unwritable(file, "parent could not be created"))?;
    match fs::symlink_metadata(file) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(unwritable(file, "is not a plain file"));
        }
        Ok(_) => {
            let existing = read_regular_text(file, MAX_FILE_READ_BYTES, false)
                .ok_or_else(|| unwritable(file, "could not be read"))?;
            let text = upsert_env_line(&existing, name, key);
            let mut output = OpenOptions::new()
                .write(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(file)
                .map_err(|_| unwritable(file, "could not be written"))?;
            output
                .write_all(text.as_bytes())
                .map_err(|_| unwritable(file, "could not be written"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(file)
                .map_err(|_| unwritable(file, "could not be created"))?;
            output
                .write_all(upsert_env_line("", name, key).as_bytes())
                .map_err(|_| unwritable(file, "could not be written"))?;
        }
        Err(_) => return Err(unwritable(file, "could not be inspected")),
    }
    fs::set_permissions(file, fs::Permissions::from_mode(0o600))
        .map_err(|_| unwritable(file, "permissions could not be secured"))?;
    Ok(())
}

fn unwritable(file: &Path, why: &str) -> OrdainError {
    OrdainError::new(
        ErrorCode::KeyFileUnwritable,
        format!("{} {why}, so the key was not written", file.display()),
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_only_provider_keys_and_preserves_env_file() {
        let parsed = parse_env_file(
            "# comment\nexport TYPESAFE_AI_API_KEY=\"abc\"\nOTHER=1\nAI_GATEWAY_API_KEY='g'\n",
        );
        assert_eq!(parsed[TYPESAFE_KEY_ENV], "abc");
        assert_eq!(parsed[GATEWAY_KEY_ENV], "g");
        assert_eq!(
            upsert_env_line(
                "DATABASE_URL=x\nexport TYPESAFE_AI_API_KEY = old",
                TYPESAFE_KEY_ENV,
                "new"
            ),
            "DATABASE_URL=x\nTYPESAFE_AI_API_KEY=new\n"
        );
    }

    #[test]
    fn refuses_symlinked_key_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "SAFE=1\n").unwrap();
        let link = dir.path().join(".env.local");
        symlink(&target, &link).unwrap();
        let error = save_key(&link, TYPESAFE_KEY_ENV, "secret").unwrap_err();
        assert_eq!(error.code, ErrorCode::KeyFileUnwritable);
        assert_eq!(fs::read_to_string(target).unwrap(), "SAFE=1\n");
    }
}
