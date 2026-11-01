//! Error formatting utilities
//!
//! Provides user-friendly error message formatting for Talos API errors.

use talos_rs::TalosError;

/// Format a TalosError into a user-friendly message
///
/// Transforms technical error messages into actionable, human-readable text.
///
/// # Examples
///
/// ```
/// use talos9s_core::errors::format_talos_error;
/// use talos_rs::TalosError;
///
/// let error = TalosError::Connection("connection refused".to_string());
/// let message = format_talos_error(&error);
/// assert!(message.contains("refused"));
/// ```
pub fn format_talos_error(error: &TalosError) -> String {
    match error {
        TalosError::Connection(msg) => format_connection_error(msg),
        TalosError::Grpc(status) => format_grpc_error(status),
        TalosError::Transport(e) => format_transport_error(&e.to_string()),
        TalosError::Tls(msg) => {
            format!("TLS error: {} - check talosconfig credentials", msg)
        }
        TalosError::ConfigNotFound(path) => {
            format!("Config not found: {}", path)
        }
        TalosError::ConfigInvalid(msg) => {
            format!("Invalid config: {}", msg)
        }
        TalosError::ContextNotFound(ctx) => {
            format!("Context '{}' not found in talosconfig", ctx)
        }
        _ => error.to_string(),
    }
}

/// Format a connection error message
fn format_connection_error(msg: &str) -> String {
    let lower = msg.to_lowercase();
    if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
        "TLS/certificate error - check talosconfig credentials".to_string()
    } else if lower.contains("refused") {
        "Connection refused - is the node reachable?".to_string()
    } else if lower.contains("timeout") {
        "Connection timed out - node may be slow or unreachable".to_string()
    } else if lower.contains("dns") || lower.contains("resolve") {
        "DNS resolution failed - check node hostname/IP".to_string()
    } else {
        format!("Connection failed: {}", msg)
    }
}

/// Format a gRPC status error
fn format_grpc_error(status: &tonic::Status) -> String {
    let msg = status.message().to_lowercase();
    if msg.contains("unavailable") {
        "Service unavailable - node may be down".to_string()
    } else if msg.contains("permission denied") {
        "Permission denied - check RBAC/credentials".to_string()
    } else if msg.contains("unauthenticated") {
        "Authentication failed - check talosconfig".to_string()
    } else if msg.contains("deadline exceeded") || msg.contains("timeout") {
        "Request timed out".to_string()
    } else if msg.contains("not found") {
        "Resource not found".to_string()
    } else if msg.contains("already exists") {
        "Resource already exists".to_string()
    } else {
        format!("gRPC error: {}", status.message())
    }
}

/// Format a transport error message
fn format_transport_error(msg: &str) -> String {
    let lower = msg.to_lowercase();
    if lower.contains("refused") {
        "Connection refused - is the node reachable?".to_string()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "Connection timed out".to_string()
    } else if lower.contains("reset") {
        "Connection reset by peer".to_string()
    } else if lower.contains("broken pipe") {
        "Connection closed unexpectedly".to_string()
    } else {
        format!("Transport error: {}", msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_connection_error() {
        assert!(format_connection_error("connection refused").contains("refused"));
        assert!(format_connection_error("TLS handshake failed").contains("TLS"));
        assert!(format_connection_error("timeout").contains("timed out"));
    }
}
