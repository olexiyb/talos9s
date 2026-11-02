//! Audit logging for node operations
//!
//! Provides persistent logging of all node operations for compliance and debugging.

use chrono::Local;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// Result of an audited operation
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AuditResult {
    /// Operation started
    Started,
    /// Operation step in progress
    InProgress,
    /// Operation completed successfully
    Success,
    /// Operation failed
    Failure,
    /// Operation was cancelled
    Cancelled,
}

impl AuditResult {
    fn as_str(&self) -> &'static str {
        match self {
            AuditResult::Started => "STARTED",
            AuditResult::InProgress => "IN_PROGRESS",
            AuditResult::Success => "SUCCESS",
            AuditResult::Failure => "FAILURE",
            AuditResult::Cancelled => "CANCELLED",
        }
    }
}

impl std::fmt::Display for AuditResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Audit logger for node operations
pub struct AuditLogger {
    /// Path to the audit log file
    log_path: PathBuf,
    /// Current cluster context
    cluster: String,
    /// Current user
    user: String,
    /// Whether logging is enabled
    enabled: bool,
}

/// Copy the audit log talos-pilot (which talos9s is forked from) kept in
/// `~/.talos-pilot/`, the first time talos9s logs. The old file is left in
/// place.
fn carry_over_legacy_log(home: &std::path::Path, log_path: &std::path::Path) {
    let legacy = home.join(".talos-pilot").join("audit.log");
    if !log_path.exists() && legacy.is_file() {
        let _ = fs::copy(&legacy, log_path);
    }
}

impl AuditLogger {
    /// Create a new audit logger
    ///
    /// Creates the ~/.talos9s directory if it doesn't exist.
    pub fn new(cluster: &str) -> Self {
        let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let talos_dir = home.join(".talos9s");
        let log_path = talos_dir.join("audit.log");

        // Get user from environment
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());

        // Try to create the directory
        let enabled = fs::create_dir_all(&talos_dir).is_ok();
        if enabled {
            carry_over_legacy_log(&home, &log_path);
        }

        Self {
            log_path,
            cluster: cluster.to_string(),
            user,
            enabled,
        }
    }

    /// Log an audit entry
    fn log(&self, operation: &str, node: &str, result: AuditResult, details: &str) {
        if !self.enabled {
            return;
        }

        // One entry per line: a newline in an error message must not
        // forge a second entry
        let one_line = |text: &str| text.replace(['\n', '\r'], " ");
        // Format: [timestamp] [cluster] [user] [operation] [node] [result] details
        let line = format!(
            "[{}] [{}] [{}] [{}] [{}] [{}] {}\n",
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            self.cluster,
            self.user,
            one_line(operation),
            one_line(node),
            result,
            one_line(details)
        );
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(e) = written {
            tracing::warn!("Failed to write audit log: {}", e);
        }
    }
}

/// Global audit logger instance
static AUDIT_LOGGER: std::sync::OnceLock<std::sync::Mutex<AuditLogger>> =
    std::sync::OnceLock::new();

/// Initialize the global audit logger
pub fn init_audit_logger(cluster: &str) {
    let logger = AuditLogger::new(cluster);
    let _ = AUDIT_LOGGER.set(std::sync::Mutex::new(logger));
}

/// Get the global audit logger
fn audit_logger() -> Option<std::sync::MutexGuard<'static, AuditLogger>> {
    AUDIT_LOGGER.get().and_then(|l| l.lock().ok())
}

/// Log an operation start (convenience function)
pub fn audit_start(operation: &str, node: &str, details: &str) {
    if let Some(logger) = audit_logger() {
        logger.log(operation, node, AuditResult::Started, details);
    }
}

/// Log an operation success (convenience function)
pub fn audit_success(operation: &str, node: &str, details: &str) {
    if let Some(logger) = audit_logger() {
        logger.log(operation, node, AuditResult::Success, details);
    }
}

/// Log an operation failure (convenience function)
pub fn audit_failure(operation: &str, node: &str, details: &str) {
    if let Some(logger) = audit_logger() {
        logger.log(operation, node, AuditResult::Failure, details);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn carries_over_talos_pilot_audit_log_once() {
        let home = tempfile::tempdir().unwrap();
        let legacy_dir = home.path().join(".talos-pilot");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("audit.log"), "old entry\n").unwrap();
        let new_dir = home.path().join(".talos9s");
        std::fs::create_dir_all(&new_dir).unwrap();
        let log = new_dir.join("audit.log");

        super::carry_over_legacy_log(home.path(), &log);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "old entry\n");
        assert!(legacy_dir.join("audit.log").exists());

        // An existing talos9s log is never overwritten
        std::fs::write(&log, "new entry\n").unwrap();
        super::carry_over_legacy_log(home.path(), &log);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "new entry\n");
    }

    use super::*;

    #[test]
    fn entries_stay_on_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let logger = AuditLogger {
            log_path: dir.path().join("audit.log"),
            cluster: "bc".into(),
            user: "me".into(),
            enabled: true,
        };
        logger.log(
            "REBOOT",
            "bc1",
            AuditResult::Failure,
            "failed:\n[x] [bc] [me] [REBOOT] [bc1] [SUCCESS] ok",
        );
        let log = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
        assert_eq!(log.lines().count(), 1);
        assert!(log.contains("] [bc] [me] [REBOOT] [bc1] [FAILURE] failed: [x]"));
    }

    #[test]
    fn test_audit_result_display() {
        assert_eq!(AuditResult::Started.to_string(), "STARTED");
        assert_eq!(AuditResult::Success.to_string(), "SUCCESS");
        assert_eq!(AuditResult::Failure.to_string(), "FAILURE");
    }
}
