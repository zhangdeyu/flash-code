use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use flash_core::{ArtifactLimits, ToolContext, ToolError, ToolErrorKind};

#[derive(Clone)]
pub(super) struct OutputArtifact {
    path: PathBuf,
    pub(super) reference: String,
    limits: ArtifactLimits,
    usage: ArtifactUsage,
}

pub(super) struct ShellArtifacts {
    pub(super) stdout: OutputArtifact,
    pub(super) stderr: OutputArtifact,
}

pub(super) struct CapturedOutput {
    pub(super) preview: Vec<u8>,
    pub(super) truncated: bool,
    pub(super) artifact: Option<String>,
}

#[derive(Clone)]
struct ArtifactUsage {
    used: Arc<Mutex<u64>>,
}

impl ArtifactUsage {
    fn reserve(&self, bytes: u64, max_total: u64) -> Result<(), ToolError> {
        let mut used = self
            .used
            .lock()
            .map_err(|_| ToolError::with_kind(ToolErrorKind::Internal, "artifact budget lock"))?;
        if used.saturating_add(bytes) > max_total {
            return Err(ToolError::with_kind(
                ToolErrorKind::ResourceLimit,
                format!("session artifacts exceeded {max_total} bytes"),
            ));
        }
        *used += bytes;
        Ok(())
    }

    fn release(&self, bytes: u64) {
        if let Ok(mut used) = self.used.lock() {
            *used = used.saturating_sub(bytes);
        }
    }
}

pub(super) fn output_artifacts(context: &ToolContext) -> Result<Option<ShellArtifacts>, ToolError> {
    let (Some(dir), Some(stem)) = (&context.artifact_dir, &context.artifact_stem) else {
        return Ok(None);
    };
    if stem.is_empty()
        || !stem
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(ToolError::with_kind(
            ToolErrorKind::InvalidInput,
            "invalid artifact stem",
        ));
    }
    let limits = context.artifact_limits.unwrap_or_default();
    let usage = ArtifactUsage {
        used: Arc::new(Mutex::new(artifact_directory_usage(dir)?)),
    };
    Ok(Some(ShellArtifacts {
        stdout: OutputArtifact {
            path: dir.join(format!("{stem}.stdout.txt")),
            reference: format!("artifacts/{stem}.stdout.txt"),
            limits,
            usage: usage.clone(),
        },
        stderr: OutputArtifact {
            path: dir.join(format!("{stem}.stderr.txt")),
            reference: format!("artifacts/{stem}.stderr.txt"),
            limits,
            usage,
        },
    }))
}

fn artifact_directory_usage(dir: &Path) -> Result<u64, ToolError> {
    let mut total = 0_u64;
    for entry in fs::read_dir(dir).map_err(|error| {
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
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
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
        total = total.saturating_add(metadata.len());
    }
    Ok(total)
}

struct ArtifactWriter {
    file: File,
    artifact: OutputArtifact,
    written: u64,
}

impl ArtifactWriter {
    fn create(artifact: OutputArtifact) -> Result<Self, ToolError> {
        let file = File::create_new(&artifact.path).map_err(|error| {
            ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to create shell output artifact: {error}"),
            )
        })?;
        Ok(Self {
            file,
            artifact,
            written: 0,
        })
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), ToolError> {
        let bytes_len = bytes.len() as u64;
        if self.written.saturating_add(bytes_len) > self.artifact.limits.max_file_bytes {
            return Err(self.fail(format!(
                "artifact exceeded {} bytes",
                self.artifact.limits.max_file_bytes
            )));
        }
        if let Err(error) = self
            .artifact
            .usage
            .reserve(bytes_len, self.artifact.limits.max_session_bytes)
        {
            self.cleanup();
            return Err(error);
        }
        if let Err(error) = self.file.write_all(bytes) {
            self.artifact.usage.release(bytes_len);
            self.cleanup();
            return Err(ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to write shell output artifact: {error}"),
            ));
        }
        self.written += bytes_len;
        Ok(())
    }

    fn fail(&mut self, message: String) -> ToolError {
        self.cleanup();
        ToolError::with_kind(ToolErrorKind::ResourceLimit, message)
    }

    fn cleanup(&mut self) {
        self.artifact.usage.release(self.written);
        self.written = 0;
        let _result = fs::remove_file(&self.artifact.path);
    }
}

pub(super) fn read_limited(
    mut reader: impl Read,
    max_output_bytes: usize,
    artifact: Option<OutputArtifact>,
) -> Result<CapturedOutput, ToolError> {
    let mut preview = Vec::with_capacity(max_output_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    let mut artifact_writer: Option<ArtifactWriter> = None;
    let mut artifact_reference = None;
    loop {
        let read = reader.read(&mut buffer).map_err(|error| {
            ToolError::with_kind(
                ToolErrorKind::Io,
                format!("failed to read shell output: {error}"),
            )
        })?;
        if read == 0 {
            break;
        }
        let remaining = max_output_bytes.saturating_sub(preview.len());
        let keep = read.min(remaining);
        preview.extend_from_slice(&buffer[..keep]);
        if let Some(writer) = artifact_writer.as_mut() {
            writer.write_all(&buffer[..read])?;
        } else if keep < read {
            truncated = true;
            if let Some(artifact) = &artifact {
                let mut writer = ArtifactWriter::create(artifact.clone())?;
                writer.write_all(&preview)?;
                writer.write_all(&buffer[keep..read])?;
                artifact_reference = Some(artifact.reference.clone());
                artifact_writer = Some(writer);
            }
        }
    }
    Ok(CapturedOutput {
        preview,
        truncated,
        artifact: artifact_reference,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, temp_dir};

    #[cfg(unix)]
    #[test]
    fn shell_artifact_should_reject_symlink_entries() {
        let root = temp_dir("shell_artifact_symlink");
        let artifact_dir = root.join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        std::os::unix::fs::symlink("/tmp", artifact_dir.join("untrusted")).unwrap();
        let mut context = context(root);
        context.artifact_dir = Some(artifact_dir);
        context.artifact_stem = Some("call".to_string());
        context.artifact_limits = Some(ArtifactLimits::default());

        let error = match output_artifacts(&context) {
            Ok(_) => panic!("symlink artifact entry should be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind, ToolErrorKind::PermissionDenied);
    }
}
