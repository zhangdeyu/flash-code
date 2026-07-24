#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub supports_reasoning: bool,
    pub supports_tool_calls: bool,
    pub requires_reasoning_for_tool_turns: bool,
    pub supports_json_mode: bool,
    pub supports_prompt_cache_metrics: bool,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

pub trait Provider {
    fn name(&self) -> &str;

    fn capabilities(&self) -> &ModelCapabilities;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider {
        capabilities: ModelCapabilities,
    }

    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }

        fn capabilities(&self) -> &ModelCapabilities {
            &self.capabilities
        }
    }

    #[test]
    fn provider_should_expose_capabilities() {
        let provider = FakeProvider {
            capabilities: ModelCapabilities {
                supports_reasoning: true,
                supports_tool_calls: true,
                requires_reasoning_for_tool_turns: true,
                supports_json_mode: false,
                supports_prompt_cache_metrics: true,
                max_context_tokens: 64_000,
                max_output_tokens: 8_000,
            },
        };

        assert!(provider.capabilities().supports_tool_calls);
    }
}
