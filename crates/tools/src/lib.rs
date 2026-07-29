mod exec;
mod fs;
mod path_guard;
mod registry;

use serde::Deserialize;
use serde_json::Value;

use flash_core::{ToolError, ToolErrorKind};

pub use exec::{macos_sandbox_status, BashTool, SandboxStatus};
pub use fs::{EditTool, GlobTool, GrepTool, ListFilesTool, ReadTool, WriteTool};
pub use registry::{builtin_registry, builtin_registry_with_options};

/// Parse a JSON `Value` into a typed tool input, mapping deserialization
/// failures to a typed `InvalidInput` tool error.
pub(crate) fn parse_input<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::InvalidInput,
            format!("invalid tool input: {error}"),
        )
    })
}

#[cfg(test)]
mod test_support {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use flash_core::{CancellationToken, ToolContext};

    pub(crate) fn context(workspace_root: PathBuf) -> ToolContext {
        ToolContext {
            workspace_root,
            cancellation: CancellationToken::new(),
            artifact_dir: None,
            artifact_stem: None,
            artifact_limits: None,
        }
    }

    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flash_tools_{name}_{nanos}"))
    }
}
