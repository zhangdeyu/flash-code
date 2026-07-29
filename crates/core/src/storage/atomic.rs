use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use super::StorageError;

/// Write `content` to `path` atomically using a temp file + rename.
pub(super) fn atomic_write(path: &Path, content: &str) -> Result<(), StorageError> {
    atomic_write_with(path, content, || Ok(()))
}

/// Write `content` to `path` atomically, invoking `before_rename` after the temp
/// file is flushed but before the rename. If `before_rename` fails the previous
/// file is left untouched and the temp file is removed.
pub(super) fn atomic_write_with<F>(
    path: &Path,
    content: &str,
    before_rename: F,
) -> Result<(), StorageError>
where
    F: FnOnce() -> io::Result<()>,
{
    let parent = path.parent().ok_or_else(|| {
        StorageError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write target has no parent directory",
        ))
    })?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp_path = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        super::timestamp_nanos()
    ));
    let result = (|| -> Result<(), StorageError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        before_rename()?;
        fs::rename(&temp_path, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _result = fs::remove_file(&temp_path);
    }
    result
}

/// Sync an existing file's metadata and data to disk.
pub(super) fn sync_file(path: &Path) -> Result<(), StorageError> {
    OpenOptions::new().read(true).open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn atomic_write_failure_should_preserve_previous_metadata() {
        let root = super::super::test_support::temp_dir("atomic_metadata_failure");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.json");
        atomic_write(&path, r#"{"status":"running"}"#).unwrap();

        let error = atomic_write_with(&path, r#"{"status":"failed"}"#, || {
            Err(io::Error::other("injected before rename"))
        })
        .unwrap_err();

        assert!(matches!(error, StorageError::Io(_)));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"status\":\"running\"}\n"
        );
        let names = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["session.json"]);
    }
}
