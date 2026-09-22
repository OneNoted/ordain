//! Read-modify-replace user configuration without truncating the original.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::error::{ErrorCode, OrdainError, Result};
use crate::paths::MAX_FILE_READ_BYTES;

pub(crate) struct ConfigFile {
    path: PathBuf,
    original: Option<String>,
}

impl ConfigFile {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let original = read(path)?;
        Ok(Self {
            path: path.into(),
            original,
        })
    }

    pub(crate) fn text(&self) -> &str {
        self.original.as_deref().unwrap_or("")
    }

    pub(crate) fn exists(&self) -> bool {
        self.original.is_some()
    }

    pub(crate) fn write(&self, text: &str) -> Result<()> {
        if self.original.as_deref() == Some(text) {
            return Ok(());
        }
        let parent = self.path.parent().expect("configuration has a parent");
        fs::create_dir_all(parent)?;
        let mut staged = NamedTempFile::new_in(parent)?;
        staged.write_all(text.as_bytes())?;
        if self.original.is_some() {
            // Do not use rename to bypass an existing file's write permissions.
            let existing = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&self.path)?;
            staged
                .as_file()
                .set_permissions(existing.metadata()?.permissions())?;
        }
        staged.as_file().sync_all()?;
        // Detect changes since parsing. This is not a lock against arbitrary host writers.
        if read(&self.path)? != self.original {
            return Err(OrdainError::new(
                ErrorCode::SettingsInvalid,
                format!("{} changed during update; retry", self.path.display()),
            ));
        }
        staged.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn read(path: &Path) -> Result<Option<String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A dangling link is not an absent configuration.
            if fs::symlink_metadata(path).is_ok() {
                return Err(invalid(path));
            }
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_FILE_READ_BYTES {
        return Err(invalid(path));
    }
    let mut text = String::new();
    file.take(MAX_FILE_READ_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_READ_BYTES {
        return Err(invalid(path));
    }
    Ok(Some(text))
}

fn invalid(path: &Path) -> OrdainError {
    OrdainError::new(
        ErrorCode::SettingsInvalid,
        format!(
            "{} must be a bounded, unlinked regular configuration file",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn replacement_preserves_original_on_conflict_and_rejects_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        fs::write(&path, "original").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let snapshot = ConfigFile::read(&path).unwrap();
        fs::write(&path, "outside edit").unwrap();
        assert!(snapshot.write("replacement").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "outside edit");
        ConfigFile::read(&path)
            .unwrap()
            .write("complete replacement")
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let alias = dir.path().join("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(ConfigFile::read(&path).is_err());
        fs::remove_file(&alias).unwrap();
        symlink(&path, &alias).unwrap();
        assert!(ConfigFile::read(&alias).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "complete replacement");
    }
}
