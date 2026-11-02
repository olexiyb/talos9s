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
            format!("TLS error: {msg} - check talosconfig credentials")
        }
        TalosError::ConfigNotFound(path) => {
            format!("Config not found: {path}")
        }
        TalosError::ConfigInvalid(msg) => {
            format!("Invalid config: {msg}")
        }
        TalosError::ContextNotFound(ctx) => {
            format!("Context '{ctx}' not found in talosconfig")
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
        format!("Connection failed: {msg}")
    }
}

/// Format a gRPC status error by its code: the message text varies (an
/// Unavailable status may say "connection refused" or "dns error"), the
/// code doesn't. The message is kept where it tells what failed.
fn format_grpc_error(status: &tonic::Status) -> String {
    use tonic::Code;
    let detail = status.message();
    match status.code() {
        Code::Unavailable => format!("Service unavailable - node may be down ({detail})"),
        Code::PermissionDenied => "Permission denied - check RBAC/credentials".to_string(),
        Code::Unauthenticated => "Authentication failed - check talosconfig".to_string(),
        Code::DeadlineExceeded => "Request timed out".to_string(),
        Code::NotFound => format!("Not found: {detail}"),
        Code::AlreadyExists => "Resource already exists".to_string(),
        Code::Unimplemented => format!("Not supported by this Talos version: {detail}"),
        _ => format!("gRPC error: {detail}"),
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
        format!("Transport error: {msg}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_errors_by_code_not_text() {
        use tonic::Status;
        assert_eq!(
            format_grpc_error(&Status::unavailable("connection refused")),
            "Service unavailable - node may be down (connection refused)"
        );
        assert_eq!(
            format_grpc_error(&Status::permission_denied("not allowed")),
            "Permission denied - check RBAC/credentials"
        );
        assert_eq!(
            format_grpc_error(&Status::deadline_exceeded("")),
            "Request timed out"
        );
        assert_eq!(
            format_grpc_error(&Status::not_found("stat /x: no such file or directory")),
            "Not found: stat /x: no such file or directory"
        );
        // A message that merely mentions a code's words doesn't pick it
        assert_eq!(
            format_grpc_error(&Status::internal("permission denied on /dev/x")),
            "gRPC error: permission denied on /dev/x"
        );
    }

    #[test]
    fn grpc_errors_remaining_codes() {
        use tonic::Status;
        assert_eq!(
            format_grpc_error(&Status::unauthenticated("bad cert")),
            "Authentication failed - check talosconfig"
        );
        assert_eq!(
            format_grpc_error(&Status::already_exists("x")),
            "Resource already exists"
        );
        assert_eq!(
            format_grpc_error(&Status::unimplemented("method Foo")),
            "Not supported by this Talos version: method Foo"
        );
        assert_eq!(format_grpc_error(&Status::cancelled("")), "gRPC error: ");
    }

    #[test]
    fn test_format_connection_error() {
        assert!(format_connection_error("connection refused").contains("refused"));
        assert!(format_connection_error("TLS handshake failed").contains("TLS"));
        assert!(format_connection_error("timeout").contains("timed out"));
    }
}
