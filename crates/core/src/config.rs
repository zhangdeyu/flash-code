use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::tools::ApprovalMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub provider_default: String,
    pub deepseek_base_url: String,
    pub deepseek_api_key_env: String,
    pub deepseek_model: String,
    pub deepseek_reasoning_effort: String,
    pub approval_mode: ApprovalMode,
    pub max_turns: u32,
    pub shell_timeout_secs: u64,
    pub shell_max_output_bytes: usize,
    pub allow_network: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider_default: "deepseek".to_string(),
            deepseek_base_url: "https://api.deepseek.com".to_string(),
            deepseek_api_key_env: "DEEPSEEK_API_KEY".to_string(),
            deepseek_model: "deepseek-v4-flash".to_string(),
            deepseek_reasoning_effort: "high".to_string(),
            approval_mode: ApprovalMode::Confirm,
            max_turns: 50,
            shell_timeout_secs: 120,
            shell_max_output_bytes: 200_000,
            allow_network: false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigOverrides {
    pub model: Option<String>,
    pub approval_mode: Option<ApprovalMode>,
}

impl Config {
    pub fn load(
        user_config: Option<&Path>,
        workspace_config: Option<&Path>,
        env: &BTreeMap<String, String>,
        overrides: &ConfigOverrides,
    ) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        if let Some(path) = user_config {
            config.apply_file(path)?;
        }
        if let Some(path) = workspace_config {
            config.apply_file(path)?;
        }
        config.apply_env(env);
        config.apply_overrides(overrides);
        Ok(config)
    }

    fn apply_file(&mut self, path: &Path) -> Result<(), ConfigError> {
        if !path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(path).map_err(ConfigError::Read)?;
        self.apply_toml_like(&content)
    }

    fn apply_env(&mut self, env: &BTreeMap<String, String>) {
        if let Some(model) = env.get("FLASH_MODEL") {
            self.deepseek_model.clone_from(model);
        }
        if let Some(mode) = env
            .get("FLASH_APPROVAL_MODE")
            .and_then(|value| parse_approval_mode(value))
        {
            self.approval_mode = mode;
        }
    }

    fn apply_overrides(&mut self, overrides: &ConfigOverrides) {
        if let Some(model) = &overrides.model {
            self.deepseek_model.clone_from(model);
        }
        if let Some(mode) = overrides.approval_mode {
            self.approval_mode = mode;
        }
    }

    fn apply_toml_like(&mut self, content: &str) -> Result<(), ConfigError> {
        let mut section = String::new();
        for raw_line in content.lines() {
            let line = raw_line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_string();
                continue;
            }
            let Some((raw_key, raw_value)) = line.split_once('=') else {
                return Err(ConfigError::Parse(line.to_string()));
            };
            let key = raw_key.trim();
            let value = parse_value(raw_value.trim());
            self.apply_value(&section, key, &value)?;
        }
        Ok(())
    }

    fn apply_value(&mut self, section: &str, key: &str, value: &str) -> Result<(), ConfigError> {
        match (section, key) {
            ("provider", "default") => self.provider_default = value.to_string(),
            ("provider", "model") => self.deepseek_model = value.to_string(),
            ("providers.deepseek", "base_url") => self.deepseek_base_url = value.to_string(),
            ("providers.deepseek", "api_key_env") => {
                self.deepseek_api_key_env = value.to_string();
            }
            ("providers.deepseek", "default_model") => self.deepseek_model = value.to_string(),
            ("providers.deepseek", "reasoning_effort") => {
                self.deepseek_reasoning_effort = value.to_string();
            }
            ("agent", "approval_mode") => {
                self.approval_mode = parse_approval_mode(value).ok_or_else(|| {
                    ConfigError::Parse(format!("invalid approval_mode `{value}`"))
                })?;
            }
            ("agent", "max_turns") => {
                self.max_turns = value
                    .parse()
                    .map_err(|_| ConfigError::Parse(format!("invalid max_turns `{value}`")))?;
            }
            ("tools", "allow_network") => {
                self.allow_network = parse_bool(value).ok_or_else(|| {
                    ConfigError::Parse(format!("invalid allow_network `{value}`"))
                })?;
            }
            ("tools.shell", "timeout_secs") => {
                self.shell_timeout_secs = value.parse().map_err(|_| {
                    ConfigError::Parse(format!("invalid shell timeout_secs `{value}`"))
                })?;
            }
            ("tools.shell", "max_output_bytes") => {
                self.shell_max_output_bytes = value.parse().map_err(|_| {
                    ConfigError::Parse(format!("invalid shell max_output_bytes `{value}`"))
                })?;
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read(std::io::Error),
    Parse(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "failed to read config: {error}"),
            Self::Parse(message) => write!(formatter, "failed to parse config: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

pub fn parse_approval_mode(value: &str) -> Option<ApprovalMode> {
    match value {
        "confirm" => Some(ApprovalMode::Confirm),
        "yolo" => Some(ApprovalMode::Yolo),
        "human" => Some(ApprovalMode::Human),
        _ => None,
    }
}

fn parse_value(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(trimmed)
        .to_string()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn load_should_apply_config_precedence() {
        let dir = temp_dir("config_precedence");
        let user = dir.join("user.toml");
        let workspace = dir.join("workspace.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &user,
            "[providers.deepseek]\ndefault_model = \"user-model\"\n",
        )
        .unwrap();
        fs::write(&workspace, "[provider]\nmodel = \"workspace-model\"\n").unwrap();
        let env = BTreeMap::from([("FLASH_MODEL".to_string(), "env-model".to_string())]);
        let overrides = ConfigOverrides {
            model: Some("cli-model".to_string()),
            approval_mode: None,
        };

        let config = Config::load(Some(&user), Some(&workspace), &env, &overrides).unwrap();

        assert_eq!(config.deepseek_model, "cli-model");
    }

    #[test]
    fn load_should_apply_workspace_over_user_config() {
        let dir = temp_dir("config_workspace");
        let user = dir.join("user.toml");
        let workspace = dir.join("workspace.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&user, "[agent]\napproval_mode = \"human\"\n").unwrap();
        fs::write(&workspace, "[agent]\napproval_mode = \"yolo\"\n").unwrap();

        let config = Config::load(
            Some(&user),
            Some(&workspace),
            &BTreeMap::new(),
            &ConfigOverrides::default(),
        )
        .unwrap();

        assert_eq!(config.approval_mode, ApprovalMode::Yolo);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flash_core_{name}_{nanos}"))
    }
}
