pub mod console;
pub mod jsonl;
pub mod memory;

use async_trait::async_trait;

use crate::protocol::Event;

/// Trait for emitting events. All implementations must be `Send + Sync` so
/// `Arc<dyn EventSink>` can be shared across concurrent futures.
///
/// `emit` takes `&self` (not `&mut self`) — interior mutability is handled by
/// each implementation via its own Mutex.
///
/// Returns `()` rather than `Result`: sink write failures (e.g. disk full)
/// should not crash the tool loop; each implementation handles errors internally.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: Event);
}
