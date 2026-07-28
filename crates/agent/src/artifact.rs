use std::path::{Path, PathBuf};

use flash_core::{ArtifactLimits, ToolError, ToolErrorKind};

use crate::error::AgentError;
use crate::runtime::AgentRuntime;

impl<P> AgentRuntime<P>
where
    P: flash_provider::ChatProvider,
{
    pub(crate) async fn materialize_output(
        &self,
        session: &flash_core::storage::Session,
        call_id: &str,
        stream: &str,
        text: &str,
    ) -> Result<MaterializedOutput, AgentError> {
        if text.len() <= self.options.max_output_bytes {
            return Ok(MaterializedOutput {
                text: text.to_string(),
                artifact: None,
                truncated: false,
            });
        }
        let safe_call_id = sanitize_artifact_component(call_id);
        let artifact = format!("artifacts/{safe_call_id}.{stream}.txt");
        let path: PathBuf = session.path.join(&artifact);
        let artifact_dir = session.path.join("artifacts");
        let full_text = text.to_string();
        let artifact_text = full_text.clone();
        let limits = self.artifact_limits;
        tokio::task::spawn_blocking(move || {
            write_agent_artifact(&artifact_dir, &path, artifact_text.as_bytes(), limits)
        })
        .await
        .map_err(|error| {
            AgentError::Storage(flash_core::storage::StorageError::TaskJoin(
                error.to_string(),
            ))
        })?
        .map_err(AgentError::Tool)?;
        Ok(MaterializedOutput {
            text: format!(
                "{}\n[full output: {}]",
                truncate(&full_text, self.options.max_output_bytes),
                artifact
            ),
            artifact: Some(artifact),
            truncated: true,
        })
    }
}

fn write_agent_artifact(
    artifact_dir: &Path,
    path: &Path,
    bytes: &[u8],
    limits: ArtifactLimits,
) -> Result<(), ToolError> {
    use std::io::Write;

    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(ToolError::with_kind(
            ToolErrorKind::ResourceLimit,
            format!("artifact exceeded {} bytes", limits.max_file_bytes),
        ));
    }
    let mut used = 0_u64;
    for entry in std::fs::read_dir(artifact_dir).map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("failed to inspect artifact directory: {error}"),
        )
    })? {
        let entry = entry.map_err(|error| {
            ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to inspect artifact: {error}"),
            )
        })?;
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
            ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to inspect artifact: {error}"),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ToolError::with_kind(
                ToolErrorKind::PermissionDenied,
                "artifact directory contains an untrusted non-file entry",
            ));
        }
        used = used.saturating_add(metadata.len());
    }
    if used.saturating_add(bytes.len() as u64) > limits.max_session_bytes {
        return Err(ToolError::with_kind(
            ToolErrorKind::ResourceLimit,
            format!(
                "session artifacts exceeded {} bytes",
                limits.max_session_bytes
            ),
        ));
    }
    let mut file = std::fs::File::create_new(path).map_err(|error| {
        ToolError::with_kind(
            ToolErrorKind::Io,
            format!("failed to create artifact: {error}"),
        )
    })?;
    if let Err(error) = file.write_all(bytes) {
        let _result = std::fs::remove_file(path);
        return Err(ToolError::with_kind(
            ToolErrorKind::Io,
            format!("failed to write artifact: {error}"),
        ));
    }
    if let Err(error) = file.sync_all() {
        let _result = std::fs::remove_file(path);
        return Err(ToolError::with_kind(
            ToolErrorKind::Io,
            format!("failed to sync artifact: {error}"),
        ));
    }
    Ok(())
}

pub(crate) struct MaterializedOutput {
    pub(crate) text: String,
    pub(crate) artifact: Option<String>,
    pub(crate) truncated: bool,
}

pub(crate) fn sanitize_artifact_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let base = if sanitized.is_empty() {
        "call"
    } else {
        &sanitized
    };
    let hash = value
        .bytes()
        .fold(14_695_981_039_346_656_037_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        });
    format!("{base}_{hash:x}")
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let truncated = value
        .chars()
        .scan(0, |used, ch| {
            let len = ch.len_utf8();
            if *used + len > max_bytes {
                None
            } else {
                *used += len;
                Some(ch)
            }
        })
        .collect::<String>();
    format!("{truncated}...[truncated]")
}

#[cfg(test)]
mod tests {
    use super::sanitize_artifact_component;
    use crate::test_support::{
        only_session_path, prepared_workspace, temp_dir, DualOutputTool, LargeTool,
        LargeToolProvider,
    };
    use crate::{AgentError, AgentOptions, AgentRuntime};
    use flash_core::{
        ArtifactLimits, PermissionPolicy, StorageLimits, ToolError, ToolErrorKind, ToolRegistry,
    };
    use std::fs;

    #[tokio::test]
    async fn run_task_should_write_large_tool_output_to_artifact() {
        let root = temp_dir("artifact");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(LargeTool)).unwrap();
        let mut runtime = AgentRuntime::new(
            LargeToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 4,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "large").await.unwrap();

        let artifact_name = format!("{}.stdout.txt", sanitize_artifact_component("call_large"));
        let artifact = root
            .join(".flash")
            .join("sessions")
            .join(&run.session_id)
            .join("artifacts")
            .join(&artifact_name);
        assert!(artifact.exists());
        assert_eq!(fs::read_to_string(&artifact).unwrap(), "abcdef");

        let session_dir = root.join(".flash").join("sessions").join(&run.session_id);
        let events = fs::read_to_string(session_dir.join("events.jsonl")).unwrap();
        let messages = fs::read_to_string(session_dir.join("messages.jsonl")).unwrap();
        assert!(events.contains(&format!("[full output: artifacts/{artifact_name}]")));
        assert!(messages.contains(&format!("[full output: artifacts/{artifact_name}]")));
    }

    #[tokio::test]
    async fn artifact_file_limit_should_fail_and_finalize_session() {
        let root = prepared_workspace("artifact_file_limit");
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(LargeTool)).unwrap();
        let mut runtime = AgentRuntime::new(
            LargeToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 4,
                max_prompt_bytes: 200_000,
            },
        )
        .with_resource_limits(
            StorageLimits::default(),
            ArtifactLimits {
                max_file_bytes: 5,
                max_session_bytes: 10,
            },
        );

        let error = runtime.run_task(&root, "large").await.unwrap_err();
        let session = only_session_path(&root);

        assert!(matches!(
            error,
            AgentError::Tool(ToolError {
                kind: ToolErrorKind::ResourceLimit,
                ..
            })
        ));
        assert!(fs::read_to_string(session.join("session.json"))
            .unwrap()
            .contains("\"status\":\"failed\""));
        assert_eq!(fs::read_dir(session.join("artifacts")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn artifact_session_limit_should_include_multiple_streams() {
        let root = prepared_workspace("artifact_session_limit");
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DualOutputTool)).unwrap();
        let mut runtime = AgentRuntime::new(
            LargeToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 2,
                max_prompt_bytes: 200_000,
            },
        )
        .with_resource_limits(
            StorageLimits::default(),
            ArtifactLimits {
                max_file_bytes: 4,
                max_session_bytes: 6,
            },
        );

        let error = runtime.run_task(&root, "large").await.unwrap_err();
        let session = only_session_path(&root);

        assert!(matches!(
            error,
            AgentError::Tool(ToolError {
                kind: ToolErrorKind::ResourceLimit,
                ..
            })
        ));
        assert!(fs::read_to_string(session.join("session.json"))
            .unwrap()
            .contains("\"status\":\"failed\""));
        assert_eq!(fs::read_dir(session.join("artifacts")).unwrap().count(), 1);
    }

    #[test]
    fn artifact_name_should_not_allow_path_traversal() {
        let name = sanitize_artifact_component("../../outside/evil");

        assert!(!name.contains('/'));
        assert!(!name.contains(".."));
        assert!(name.starts_with("______outside_evil_"));
    }
}
