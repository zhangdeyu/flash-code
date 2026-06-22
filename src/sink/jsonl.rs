use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::protocol::Event;
use crate::sink::EventSink;

/// Writes events as JSONL (one JSON object per line) to a file.
///
/// Uses `tokio::sync::Mutex<tokio::fs::File>` because the critical section
/// contains `write_all(...).await` — the guard crosses an await point, so
/// `std::sync::Mutex` would not compile (its guard is not `Send`).
pub struct JsonlSink {
    file: tokio::sync::Mutex<tokio::fs::File>,
}

impl JsonlSink {
    #[must_use]
    pub fn new(file: tokio::fs::File) -> Self {
        Self {
            file: tokio::sync::Mutex::new(file),
        }
    }
}

#[async_trait]
impl EventSink for JsonlSink {
    async fn emit(&self, event: Event) {
        // Unknown events are never written — they only appear on the read side
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

        let mut file = self.file.lock().await;
        if let Err(e) = file.write_all(format!("{line}\n").as_bytes()).await {
            eprintln!("jsonl write failed: {e}");
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
}
// JsonlSink: JSONL file output
