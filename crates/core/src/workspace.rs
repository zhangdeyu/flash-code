use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum WorkspaceError {
    CurrentDir(std::io::Error),
    Canonicalize {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CurrentDir(error) => {
                write!(formatter, "failed to read current directory: {error}")
            }
            Self::Canonicalize { path, source } => {
                write!(
                    formatter,
                    "failed to canonicalize `{}`: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

pub fn discover_workspace_root(start: Option<&Path>) -> Result<PathBuf, WorkspaceError> {
    let cwd = match start {
        Some(path) => path.to_path_buf(),
        None => env::current_dir().map_err(WorkspaceError::CurrentDir)?,
    };
    let canonical = cwd
        .canonicalize()
        .map_err(|source| WorkspaceError::Canonicalize {
            path: cwd.clone(),
            source,
        })?;
    Ok(find_git_root(&canonical).unwrap_or(canonical))
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn discover_workspace_root_should_use_git_root() {
        let root = temp_dir("git_root");
        let nested = root.join("a/b");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(&nested).unwrap();

        let discovered = discover_workspace_root(Some(&nested)).unwrap();

        assert_eq!(discovered, root.canonicalize().unwrap());
    }

    #[test]
    fn discover_workspace_root_should_use_cwd_without_git() {
        let root = temp_dir("plain_root");
        fs::create_dir_all(&root).unwrap();

        let discovered = discover_workspace_root(Some(&root)).unwrap();

        assert_eq!(discovered, root.canonicalize().unwrap());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("flash_core_{name}_{nanos}"))
    }
}
