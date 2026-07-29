use std::path::{Component, Path, PathBuf};

use flash_core::{ToolError, ToolErrorKind};

/// Resolve a workspace-relative path that must already exist, verifying it stays
/// inside the workspace after canonicalization.
pub(crate) fn workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let root = root.canonicalize().map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("invalid workspace root: {error}"),
        )
    })?;
    let path = root.join(input.trim());
    let canonical = path.canonicalize().map_err(|error| {
        ToolError::with_kind(ToolErrorKind::Io, format!("invalid path: {error}"))
    })?;
    if !canonical.starts_with(root) {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "path escapes workspace",
        ));
    }
    Ok(canonical)
}

/// Resolve an existing workspace path for editing.
pub(crate) fn existing_workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let path = writable_workspace_path(root, input)?;
    path.canonicalize()
        .map_err(|error| ToolError::with_kind(ToolErrorKind::Io, format!("invalid path: {error}")))
}

/// Resolve a workspace-relative path that may not yet exist, verifying its
/// parent is inside the workspace and rejecting `..` and absolute escapes.
pub(crate) fn writable_workspace_path(root: &Path, input: &str) -> Result<PathBuf, ToolError> {
    let root = root.canonicalize().map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("invalid workspace root: {error}"),
        )
    })?;
    let relative = Path::new(input.trim());
    if relative.is_absolute() {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "path escapes workspace",
        ));
    }
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "path escapes workspace",
        ));
    }
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::with_kind(ToolErrorKind::InvalidInput, "invalid path"))?
        .canonicalize()
        .map_err(|error| {
            ToolError::with_kind(ToolErrorKind::Io, format!("invalid parent path: {error}"))
        })?;
    if !parent.starts_with(root) {
        return Err(ToolError::with_kind(
            ToolErrorKind::PermissionDenied,
            "path escapes workspace",
        ));
    }
    Ok(path)
}
