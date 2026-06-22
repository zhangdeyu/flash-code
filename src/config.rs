use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Error, Result};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o";

#[derive(Debug, Deserialize)]
pub struct Config {
    pub api_key: String,

    #[serde(default = "default_base_url")]
    pub base_url: String,

    #[serde(default = "default_model")]
    pub model: String,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}

fn default_model() -> String {
    DEFAULT_MODEL.to_owned()
}

impl Config {
    /// Load configuration from `~/.flash/config.toml`.
    ///
    /// Returns a clear error if the file does not exist or api_key is missing.
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

        Ok(config)
    }

    /// Returns the path to the config file: `~/.flash/config.toml`
    pub fn config_path() -> Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| {
            Error::Config("could not determine home directory".into())
        })?;
        Ok(home.join(".flash").join("config.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
            api_key = "sk-test-123"
            base_url = "https://custom.api.com/v1"
            model = "gpt-3.5-turbo"
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert_eq!(config.api_key, "sk-test-123");
        assert_eq!(config.base_url, "https://custom.api.com/v1");
        assert_eq!(config.model, "gpt-3.5-turbo");
    }

    #[test]
    fn parse_minimal_config_uses_defaults() {
        let toml_str = r#"
            api_key = "sk-minimal"
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert_eq!(config.api_key, "sk-minimal");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
    }

    #[test]
    fn missing_api_key_fails() {
        let toml_str = r#"
            base_url = "https://example.com"
        "#;
        let result = toml::from_str::<Config>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn config_path_is_under_home() {
        let path = Config::config_path().expect("should resolve");
        assert!(path.ends_with(".flash/config.toml"));
    }
}
// Configuration: ~/.flash/config.toml
