//! Error types for talos-rs

use thiserror::Error;

/// Errors that can occur when interacting with Talos API
#[derive(Error, Debug)]
pub enum TalosError {
    /// Configuration file not found
    #[error("Config file not found: {0}")]
    ConfigNotFound(String),

    /// Failed to parse configuration
    #[error("Failed to parse config: {0}")]
    ConfigParse(#[from] serde_saphyr::Error),

    /// Invalid configuration
    #[error("Invalid config: {0}")]
    ConfigInvalid(String),

    /// Context not found in config
    #[error("Context not found: {0}")]
    ContextNotFound(String),

    /// No endpoints configured for context
    #[error("No endpoints configured for context: {0}")]
    NoEndpoints(String),

    /// Base64 decoding error
    #[error("Base64 decode error: {0}")]
    Base64Decode(#[from] base64::DecodeError),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// TLS error
    #[error("TLS error: {0}")]
    Tls(String),

    /// gRPC transport error
    #[error("Transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    /// gRPC status error
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    /// Connection failed
    #[error("Connection failed: {0}")]
    Connection(String),

    /// No home directory found
    #[error("Could not determine home directory")]
    NoHomeDirectory,
}

impl TalosError {
    /// Whether the node answered that the thing asked for doesn't exist,
    /// as opposed to not answering at all. A proxied multi-node reply puts
    /// the node's error in its metadata text.
    pub fn is_not_found(&self) -> bool {
        match self {
            TalosError::Grpc(status) => status.code() == tonic::Code::NotFound,
            TalosError::Connection(msg) => msg.contains("no such file or directory"),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_is_told_apart_from_failures() {
        assert!(TalosError::Grpc(tonic::Status::not_found("stat /x: no such file")).is_not_found());
        assert!(TalosError::Connection("stat /x: no such file or directory".into()).is_not_found());
        assert!(!TalosError::Grpc(tonic::Status::unavailable("connection refused")).is_not_found());
        assert!(!TalosError::Connection("timed out".into()).is_not_found());
    }
}
