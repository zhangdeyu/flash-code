#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageLimits {
    pub max_event_bytes: usize,
    pub max_jsonl_bytes: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            max_event_bytes: 1024 * 1024,
            max_jsonl_bytes: 50 * 1024 * 1024,
        }
    }
}
