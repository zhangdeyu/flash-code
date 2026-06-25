use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::engine::{run_loop, ApprovalCallback, CompactionPolicy, RunContext};
use crate::error::Result;
use crate::protocol::{ContentBlock, History, Message};
use crate::provider::Provider;
use crate::sink::EventSink;
use crate::system_prompt::{build_system_prompt, EnvironmentSnapshot, STATIC_TEMPLATE};
use crate::tool::{ApprovalMode, ToolRegistry};

pub struct AgentConfig {
    pub approval_mode: ApprovalMode,
    pub cwd: PathBuf,
    pub tool_timeout: Duration,
    pub tool_max_output_bytes: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            approval_mode: ApprovalMode::Default,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            tool_timeout: Duration::from_secs(120),
            tool_max_output_bytes: 64 * 1024,
        }
    }
}

pub struct Session {
    pub session_id: String,
    pub system: Vec<Message>,
    pub history: History,
    pub cancel_root: CancellationToken,
    pub sink: Arc<dyn EventSink>,
}

impl Session {
    #[must_use]
    pub fn new(session_id: String, sink: Arc<dyn EventSink>) -> Self {
        Self {
            session_id,
            system: Vec::new(),
            history: History::new(),
            cancel_root: CancellationToken::new(),
            sink,
        }
    }

    /// Send a user message and run the agent loop.
    pub async fn send(
        &mut self,
        user_input: Vec<ContentBlock>,
        provider: &dyn Provider,
        registry: &ToolRegistry,
        compaction_policy: &dyn CompactionPolicy,
        approval_callback: ApprovalCallback,
        config: &AgentConfig,
    ) -> Result<()> {
        let cancel = self.cancel_root.child_token();

        let env = EnvironmentSnapshot::capture(config.cwd.clone());
        self.system = build_system_prompt(STATIC_TEMPLATE, &env, registry);

        let ctx = RunContext {
            session_id: self.session_id.clone(),
            approval_mode: config.approval_mode,
            cwd: config.cwd.clone(),
            tool_timeout: config.tool_timeout,
            tool_max_output_bytes: config.tool_max_output_bytes,
            cancel,
            sink: self.sink.clone(),
        };

        run_loop(
            self,
            user_input,
            provider,
            registry,
            compaction_policy,
            approval_callback,
            &ctx,
        )
        .await
    }

    pub fn cancel_current(&self) {
        self.cancel_root.cancel();
    }
}
