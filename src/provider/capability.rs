use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub max_context: usize,
    pub max_output: usize,
    pub supports_reasoning: bool,
}

impl Capability {
    #[must_use]
    pub fn openai_gpt4o() -> Self {
        Self {
            max_context: 128_000,
            max_output: 16_384,
            supports_reasoning: false,
        }
    }

    #[must_use]
    pub fn openai_o_series(max_context: usize, max_output: usize) -> Self {
        Self {
            max_context,
            max_output,
            supports_reasoning: true,
        }
    }
}
