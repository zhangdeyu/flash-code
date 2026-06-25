use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::provider::Capability;

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1";
const DEFAULT_MODEL: &str = "deepseek-v4-pro";
const DEFAULT_MAX_CONTEXT: usize = 128_000;
const DEFAULT_MAX_OUTPUT: usize = 8_192;
const DEFAULT_REASONING_EFFORT: &str = "max";

#[derive(Debug, Deserialize)]
pub struct Config {
    pub api_key: String,

    #[serde(default = "default_base_url")]
    pub base_url: String,

    #[serde(default = "default_model")]
    pub model: String,

    #[serde(default = "default_max_context")]
    pub max_context: usize,

    #[serde(default = "default_max_output")]
    pub max_output: usize,

    #[serde(default = "default_reasoning_effort")]
    pub reasoning_effort: String,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}
fn default_model() -> String {
    DEFAULT_MODEL.to_owned()
}
fn default_max_context() -> usize {
    DEFAULT_MAX_CONTEXT
}
fn default_max_output() -> usize {
    DEFAULT_MAX_OUTPUT
}
fn default_reasoning_effort() -> String {
    DEFAULT_REASONING_EFFORT.to_owned()
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Err(Error::ConfigNotFound(path));
        }
        let content = std::fs::read_to_string(&path)?;
        let config: Self = toml::from_str(&content)?;
        if config.api_key.is_empty() {
            return Err(Error::Config(
                "api_key is empty in ~/.flash/config.toml".into(),
            ));
        }
        match config.reasoning_effort.as_str() {
            "high" | "max" => {}
            other => {
                return Err(Error::Config(format!(
                    "invalid reasoning_effort = \"{other}\"; expected \"high\" or \"max\""
                )));
            }
        }
        Ok(config)
    }

    pub fn config_path() -> Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("could not determine home directory".into()))?;
        Ok(home.join(".flash").join("config.toml"))
    }

    #[must_use]
    pub fn capability(&self) -> Capability {
        Capability {
            max_context: self.max_context,
            max_output: self.max_output,
        }
    }
}
