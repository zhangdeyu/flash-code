use std::path::PathBuf;

use crate::protocol::Message;
use crate::tool::ToolRegistry;

pub const STATIC_TEMPLATE: &str = include_str!("../prompts/system_default.md");

pub struct EnvironmentSnapshot {
    pub os: String,
    pub shell: String,
    pub cwd: PathBuf,
    pub date: String,
}

impl EnvironmentSnapshot {
    #[must_use]
    pub fn capture(cwd: PathBuf) -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            shell: std::env::var("SHELL").unwrap_or_else(|_| "sh".into()),
            cwd,
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        }
    }
}

#[must_use]
pub fn render_environment(env: &EnvironmentSnapshot, registry: &ToolRegistry) -> String {
    let tool_list = registry
        .specs()
        .iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "# Environment\nOS: {}\nShell: {}\nCWD: {}\nDate: {}\n\n# Available tools\n{}",
        env.os,
        env.shell,
        env.cwd.display(),
        env.date,
        tool_list,
    )
}

#[must_use]
pub fn build_system_prompt(
    static_template: &str,
    env: &EnvironmentSnapshot,
    registry: &ToolRegistry,
) -> Vec<Message> {
    let dynamic = render_environment(env, registry);
    vec![
        Message::system_text(static_template),
        Message::system_text(dynamic),
    ]
}
