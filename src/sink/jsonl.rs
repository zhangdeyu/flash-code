use async_trait::async_trait;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::protocol::Event;
use crate::sink::EventSink;

/// Writes events as JSONL (one JSON object per line) to any `AsyncWrite` target.
///
/// Uses `tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>` because the
/// critical section contains `.await` calls (write + flush), so the guard must
/// be `Send`. Each `emit` flushes after writing so downstream pipe consumers
/// (e.g. `jq`) see events immediately rather than in block-buffered chunks.
pub struct JsonlSink {
    out: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
}

impl JsonlSink {
    /// Wrap any async writer (file, stdout, in-memory buffer, ...).
    pub fn new<W: AsyncWrite + Send + Unpin + 'static>(writer: W) -> Self {
        Self {
            out: tokio::sync::Mutex::new(Box::new(writer)),
        }
    }

    /// Convenience: write JSONL to process stdout. Production default.
    #[must_use]
    pub fn stdout() -> Self {
        Self::new(tokio::io::stdout())
    }
}

#[async_trait]
impl EventSink for JsonlSink {
    async fn emit(&self, event: Event) {
        // Unknown events are read-side only; never serialized back out.
        if matches!(event, Event::Unknown(_)) {
            return;
        }

        let line = match serde_json::to_string(&event) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("jsonl serialize failed: {e}");
                return;
            }
        };

        let mut out = self.out.lock().await;
        if let Err(e) = out.write_all(format!("{line}\n").as_bytes()).await {
            eprintln!("jsonl write failed: {e}");
            return;
        }
        // Flush per event so pipe consumers see lines without block buffering.
        if let Err(e) = out.flush().await {
            eprintln!("jsonl flush failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_known_event_as_jsonl_line() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_owned();
        let file = tokio::fs::File::create(&path).await.expect("open");
        let sink = JsonlSink::new(file);

        sink.emit(Event::SessionStarted {
            session_id: "s1".into(),
        })
        .await;

        // Flush by dropping the sink (releases the file)
        drop(sink);

        let content = tokio::fs::read_to_string(&path).await.expect("read");
        let line = content.trim();
        let parsed: serde_json::Value = serde_json::from_str(line).expect("valid json");
        assert_eq!(parsed["type"], "session_started");
        assert_eq!(parsed["session_id"], "s1");
    }

    #[tokio::test]
    async fn skips_unknown_event() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_owned();
        let file = tokio::fs::File::create(&path).await.expect("open");
        let sink = JsonlSink::new(file);

        sink.emit(Event::Unknown(serde_json::json!({"type": "future_thing"})))
            .await;

        drop(sink);

        let content = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(content.is_empty());
    }

    #[tokio::test]
    async fn flushes_each_event() {
        // Use an in-memory Vec to verify writes land synchronously per emit.
        let buf: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        // tokio::io::AsyncWrite is impl'd on tokio's wrappers; for the test we
        // use a tokio::fs::File via a temp path (cursor's Vec doesn't impl AsyncWrite directly).
        let tmp = tempfile::NamedTempFile::new().expect("temp");
        let path = tmp.path().to_owned();
        let _ = cursor;

        let file = tokio::fs::File::create(&path).await.expect("open");
        let sink = JsonlSink::new(file);

        sink.emit(Event::SessionStarted {
            session_id: "s1".into(),
        })
        .await;

        // Without explicit flush, this read may return empty under block buffering.
        // With per-emit flush, content should already be on disk.
        let content = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(
            content.contains("session_started"),
            "event not flushed: {content:?}"
        );

        drop(sink);
    }
}
