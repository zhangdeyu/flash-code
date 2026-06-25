use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::message::MessageId;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Auto,
    Overflow,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub id: String,
    pub summary: String,
    pub tail_start_id: MessageId,
    pub trigger: CompactionTrigger,
    pub created_at: SystemTime,
}

impl Compaction {
    #[must_use]
    pub fn new(
        summary: String,
        tail_start_id: MessageId,
        trigger: CompactionTrigger,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            summary,
            tail_start_id,
            trigger,
            created_at: SystemTime::now(),
        }
    }
}
