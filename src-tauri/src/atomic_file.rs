use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

pub fn write(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Storage("atomic file has no parent directory".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| AppError::Storage(error.to_string()))?;
    #[cfg(unix)]
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| AppError::Storage(error.to_string()))?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| AppError::Storage(error.to_string()))?;
    temporary
        .write_all(bytes)
        .and_then(|_| temporary.as_file_mut().sync_all())
        .map_err(|error| AppError::Storage(error.to_string()))?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::Storage(error.to_string()))?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| AppError::Storage(error.error.to_string()))
}

pub async fn write_async(path: PathBuf, bytes: Vec<u8>) -> AppResult<()> {
    tokio::task::spawn_blocking(move || write(&path, &bytes))
        .await
        .map_err(|error| AppError::Internal(error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::write;

    #[test]
    fn atomically_replaces_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("value.json");
        write(&path, b"first").unwrap();
        write(&path, b"second").unwrap();
        assert_eq!(b"second", std::fs::read(path).unwrap().as_slice());
    }
}
