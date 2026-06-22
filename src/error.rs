use std::path::PathBuf;

/// All errors that can occur in flash-code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("operation cancelled")]
    Cancelled,

    #[error("provider error: {0}")]
    Provider(String),

    #[error("max turns exceeded")]
    MaxTurnsExceeded,

    #[error("config error: {0}")]
    Config(String),

    #[error("config file not found: {}", .0.display())]
    ConfigNotFound(PathBuf),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_display() {
        let err = Error::Cancelled;
        assert_eq!(err.to_string(), "operation cancelled");
    }

    #[test]
    fn provider_display() {
        let err = Error::Provider("rate limit".into());
        assert_eq!(err.to_string(), "provider error: rate limit");
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn from_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: Error = json_err.into();
        assert!(matches!(err, Error::Json(_)));
    }
}
// Error types
