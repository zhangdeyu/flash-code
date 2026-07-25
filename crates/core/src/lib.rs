pub mod config;
pub mod protocol;
pub mod storage;
pub mod tools;
pub mod workspace;

pub use config::{Config, ConfigOverrides};
pub use protocol::{ContentBlock, Event, Message, Outcome, Role, SessionStatus, ToolResultStatus};
pub use storage::{
    append_assistant_message, append_event, append_tool_result_message, append_user_message,
    create_session, init_workspace, replay_events,
};
pub use tools::{
    PermissionDecision, PermissionPolicy, Tool, ToolContext, ToolDescriptor, ToolError,
    ToolExitStatus, ToolOutput, ToolRegistry, ToolRisk,
};
pub use workspace::{discover_workspace_root, WorkspaceError};
