use super::compaction::Compaction;
use super::content_block::ContentBlock;
use super::message::{Message, MessageId, Role};
use super::micro::{apply_micro_compact, MicroCompactPolicy, MicroCompactResult};

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("tail_start_id {0} not found in history")]
    TailNotFound(MessageId),

    #[error("tail_start_id {0} points to assistant message; must be user or tool")]
    TailNotOnTurnBoundary(MessageId),

    #[error("compaction tail must move forward; new pos {new} <= prev pos {prev}")]
    NonMonotonicCompaction { new: usize, prev: usize },

    #[error("tail cannot be the last message; nothing left to keep")]
    EmptyTail,

    #[error(
        "tool_results call_ids {got:?} do not match preceding assistant tool_uses {expected:?}"
    )]
    ToolResultMismatch {
        expected: Vec<String>,
        got: Vec<String>,
    },

    #[error("no preceding assistant message before tool_results")]
    NoPrecedingAssistant,
}

pub struct History {
    messages: Vec<Message>,
    compactions: Vec<Compaction>,
    micro_policy: MicroCompactPolicy,
}

impl History {
    #[must_use]
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            compactions: Vec::new(),
            micro_policy: MicroCompactPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_micro_policy(policy: MicroCompactPolicy) -> Self {
        Self {
            messages: Vec::new(),
            compactions: Vec::new(),
            micro_policy: policy,
        }
    }

    pub fn push_user(&mut self, blocks: Vec<ContentBlock>) -> MessageId {
        let m = Message::user(blocks);
        let id = m.id.clone();
        self.messages.push(m);
        id
    }

    pub fn push_assistant(&mut self, blocks: Vec<ContentBlock>) -> MessageId {
        let m = Message::assistant(blocks);
        let id = m.id.clone();
        self.messages.push(m);
        id
    }

    pub fn push_tool_results(
        &mut self,
        results: Vec<ContentBlock>,
    ) -> Result<MessageId, HistoryError> {
        let assistant = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .ok_or(HistoryError::NoPrecedingAssistant)?;

        let mut expected: Vec<String> = assistant.tool_use_call_ids();
        let mut got: Vec<String> = results
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        let mut e_sorted = expected.clone();
        let mut g_sorted = got.clone();
        e_sorted.sort();
        g_sorted.sort();
        if e_sorted != g_sorted {
            expected.sort();
            got.sort();
            return Err(HistoryError::ToolResultMismatch { expected, got });
        }

        let m = Message::tool_results(results);
        let id = m.id.clone();
        self.messages.push(m);
        Ok(id)
    }

    pub fn record_compaction(&mut self, c: Compaction) -> Result<(), HistoryError> {
        let pos = self
            .messages
            .iter()
            .position(|m| m.id == c.tail_start_id)
            .ok_or_else(|| HistoryError::TailNotFound(c.tail_start_id.clone()))?;

        let role = self.messages[pos].role;
        if !matches!(role, Role::User | Role::Tool) {
            return Err(HistoryError::TailNotOnTurnBoundary(c.tail_start_id.clone()));
        }

        if pos >= self.messages.len() - 1 {
            return Err(HistoryError::EmptyTail);
        }

        if let Some(prev) = self.compactions.last() {
            let prev_pos = self
                .messages
                .iter()
                .position(|m| m.id == prev.tail_start_id)
                .expect("prev compaction tail invariant");
            if pos <= prev_pos {
                return Err(HistoryError::NonMonotonicCompaction {
                    new: pos,
                    prev: prev_pos,
                });
            }
        }

        self.compactions.push(c);
        Ok(())
    }

    /// Replay-friendly unchecked push. Internal use only (Event replay).
    #[allow(dead_code)]
    pub(crate) fn push_message_unchecked(&mut self, m: Message) {
        self.messages.push(m);
    }

    /// Replay-friendly unchecked compaction insert. Internal use only.
    #[allow(dead_code)]
    pub(crate) fn push_compaction_unchecked(&mut self, c: Compaction) {
        self.compactions.push(c);
    }

    #[must_use]
    pub fn raw_messages(&self) -> &[Message] {
        &self.messages
    }

    #[must_use]
    pub fn last_compaction(&self) -> Option<&Compaction> {
        self.compactions.last()
    }

    #[must_use]
    pub fn compactions(&self) -> &[Compaction] {
        &self.compactions
    }

    /// Messages strictly before `tail_start_id` (for compaction summary input).
    /// If a prior compaction exists, its summary is prepended as a Summary message
    /// so the summarizer can merge it via the `<previous-summary>` block.
    #[must_use]
    pub fn messages_before(&self, tail_id: &MessageId) -> Vec<Message> {
        let pos = self
            .messages
            .iter()
            .position(|m| &m.id == tail_id)
            .unwrap_or(self.messages.len());
        let mut out: Vec<Message> = Vec::new();
        if let Some(prev) = self.compactions.last() {
            out.push(Message::summary(prev.summary.clone()));
        }
        out.extend_from_slice(&self.messages[..pos]);
        out
    }

    /// Pure compaction slice (no micro). For tests / debugging.
    #[must_use]
    pub fn compact_slice(&self) -> Vec<Message> {
        match self.compactions.last() {
            None => self.messages.clone(),
            Some(c) => {
                let idx = self
                    .messages
                    .iter()
                    .position(|m| m.id == c.tail_start_id)
                    .expect("tail_start_id invariant");
                let mut v = Vec::with_capacity(self.messages.len() - idx + 1);
                v.push(Message::summary(c.summary.clone()));
                v.extend_from_slice(&self.messages[idx..]);
                v
            }
        }
    }

    /// Projection: compact_slice + micro redact.
    #[must_use]
    pub fn to_prompt_messages(&self) -> Vec<Message> {
        let mut out = self.compact_slice();
        let _ = apply_micro_compact(&mut out, &self.micro_policy);
        out
    }

    /// Projection that also returns the MicroCompact effect (for emitting events).
    #[must_use]
    pub fn project_with_micro(&self) -> (Vec<Message>, MicroCompactResult) {
        let mut out = self.compact_slice();
        let r = apply_micro_compact(&mut out, &self.micro_policy);
        (out, r)
    }

    /// Heuristic token estimate based on JSON serialization length / 4.
    #[must_use]
    pub fn estimate_tokens(&self) -> usize {
        self.to_prompt_messages()
            .iter()
            .map(estimate_message_tokens)
            .sum()
    }

    #[must_use]
    pub fn micro_policy(&self) -> &MicroCompactPolicy {
        &self.micro_policy
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn estimate_message_tokens(msg: &Message) -> usize {
    serde_json::to_string(msg).map(|s| s.len() / 4).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::text(text)]
    }

    #[test]
    fn push_and_project_no_compaction() {
        let mut h = History::new();
        h.push_user(user("hi"));
        h.push_assistant(vec![ContentBlock::text("hello")]);
        let p = h.to_prompt_messages();
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn tool_result_mismatch_is_error() {
        let mut h = History::new();
        h.push_user(user("go"));
        h.push_assistant(vec![ContentBlock::ToolUse {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }]);
        // wrong call_id
        let bad = ContentBlock::tool_result("c2", vec![ContentBlock::text("x")], false).unwrap();
        let r = h.push_tool_results(vec![bad]);
        assert!(matches!(r, Err(HistoryError::ToolResultMismatch { .. })));
    }

    #[test]
    fn record_compaction_invariants() {
        let mut h = History::new();
        let u1 = h.push_user(user("a"));
        h.push_assistant(vec![ContentBlock::text("b")]);
        let _u2 = h.push_user(user("c"));
        h.push_assistant(vec![ContentBlock::text("d")]);

        // tail not found
        let bogus = Compaction::new(
            "s".into(),
            "no-such".into(),
            super::super::compaction::CompactionTrigger::Auto,
        );
        assert!(matches!(
            h.record_compaction(bogus),
            Err(HistoryError::TailNotFound(_))
        ));

        // tail on assistant boundary -> error
        let asst_id = h.raw_messages()[1].id.clone();
        let c2 = Compaction::new(
            "s".into(),
            asst_id,
            super::super::compaction::CompactionTrigger::Auto,
        );
        assert!(matches!(
            h.record_compaction(c2),
            Err(HistoryError::TailNotOnTurnBoundary(_))
        ));

        // empty tail (last message)
        let last_id = h.raw_messages().last().unwrap().id.clone();
        let c3 = Compaction::new(
            "s".into(),
            last_id,
            super::super::compaction::CompactionTrigger::Auto,
        );
        // last message is assistant in this fixture; that error short-circuits first
        let _ = h.record_compaction(c3);

        // valid: user 2 as tail
        let user2 = h.raw_messages()[2].id.clone();
        let c4 = Compaction::new(
            "summary".into(),
            user2,
            super::super::compaction::CompactionTrigger::Auto,
        );
        h.record_compaction(c4).unwrap();

        // monotonic: re-using u1 should fail
        let c5 = Compaction::new(
            "s2".into(),
            u1,
            super::super::compaction::CompactionTrigger::Auto,
        );
        assert!(matches!(
            h.record_compaction(c5),
            Err(HistoryError::NonMonotonicCompaction { .. })
        ));
    }

    #[test]
    fn projection_idempotent() {
        let mut h = History::new();
        h.push_user(user("x"));
        h.push_assistant(vec![ContentBlock::text("y")]);
        let a = h.to_prompt_messages();
        let b = h.to_prompt_messages();
        assert_eq!(a.len(), b.len());
    }
}
