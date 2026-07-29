use std::fs;
use std::path::Path;

use flash_core::ToolError;

pub mod edit;
pub mod glob;
pub mod grep;
pub mod list;
pub mod read;
pub mod write;

pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list::ListFilesTool;
pub use read::ReadTool;
pub use write::WriteTool;

/// Walk the workspace tree, visiting every file. Skips `.git`, `target` and
/// `.flash` directories.
pub(super) fn visit_files(root: &Path, visit: &mut impl FnMut(&Path)) -> Result<(), ToolError> {
    for entry in fs::read_dir(root)
        .map_err(|error| ToolError::new(format!("failed to read dir: {error}")))?
    {
        let entry =
            entry.map_err(|error| ToolError::new(format!("failed to read dir: {error}")))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == ".git" || name == "target" || name == ".flash" {
            continue;
        }
        if path.is_dir() {
            visit_files(&path, visit)?;
        } else {
            visit(&path);
        }
    }
    Ok(())
}
