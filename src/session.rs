use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::engine::run_loop::{run_loop, ApprovalMode};
use crate::error::Result;
use crate::protocol::Message;
use crate::provider::Provider;
use crate::sink::EventSink;
use crate::tool::Tool;

/// Holds state across multiple user turns within a single session.
///
/// `history` persists across `send()` calls — it is not a local variable of
/// `run_loop`, so it survives Done/Cancelled returns.
pub struct Session {
    pub session_id: String,
    pub system: Vec<Message>,
    pub history: Vec<Message>,
    pub cancel: CancellationToken,
    pub sink: Arc<dyn EventSink>,
}

impl Session {
    /// Create a new session with the given system messages and event sink.
    #[must_use]
    pub fn new(
        session_id: String,
        system: Vec<Message>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            session_id,
            system,
            history: Vec::new(),
            cancel: CancellationToken::new(),
            sink,
        }
    }

    /// Send a user message and run the agent loop until Done or Cancelled.
    ///
    /// On each call:
    /// 1. Push the user message into history
    /// 2. Reset the cancellation token (previous cancel doesn't affect this turn)
    /// 3. Run the agent loop
    pub async fn send(
        &mut self,
        user_input: String,
        mode: ApprovalMode,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        approval_rx: &mut mpsc::Receiver<bool>,
    ) -> Result<()> {
        self.history.push(Message::user(user_input));
        self.cancel = CancellationToken::new();
        run_loop(self, mode, provider, tools, approval_rx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::memory::MemorySink;

    #[test]
    fn new_session_has_empty_history() {
        let sink = Arc::new(MemorySink::new());
        let session = Session::new("s1".into(), vec![], sink);
        assert!(session.history.is_empty());
        assert_eq!(session.session_id, "s1");
    }

    #[test]
    fn cancel_token_is_not_cancelled_initially() {
        let sink = Arc::new(MemorySink::new());
        let session = Session::new("s1".into(), vec![], sink);
        assert!(!session.cancel.is_cancelled());
    }
}
// Session: holds state across turns
