use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub max_context: usize,
    pub max_output: usize,
}

impl Capability {
    #[must_use]
    pub fn deepseek_v4_pro() -> Self {
        Self {
            max_context: 128_000,
            max_output: 8_192,
        }
    }
}
