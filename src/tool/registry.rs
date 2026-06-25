use std::collections::HashMap;
use std::sync::Arc;

use super::Tool;
use crate::protocol::ToolSpec;

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool. Panics on duplicate name (startup-time error).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_owned();
        assert!(
            !self.tools.contains_key(&name),
            "duplicate tool name: {name}"
        );
        self.tools.insert(name, tool);
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
