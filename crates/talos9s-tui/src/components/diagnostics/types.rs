//! Shared types for the diagnostics system
//!
//! Core types (CheckStatus, CniType, etc.) are imported from talos9s-core.
//! This module contains UI-specific types for fixes and diagnostic context.

// Re-export core types for use by diagnostics modules
pub use talos9s_core::{
    CheckCategory, CheckStatus, CniInfo, CniPodInfo, CniType, PodHealthInfo, UnhealthyPodInfo,
};

/// What fixes an issue. talos9s never runs it: the dialog shows the command
/// and copies it, and the user runs it.
#[derive(Debug, Clone)]
pub enum FixAction {
    /// Add a kernel module via the machine config
    AddKernelModule(String),
    /// Restart a service
    RestartService(String),
    /// Run a command on the host (for Docker environments)
    HostCommand {
        command: String,
        description: String,
    },
}

impl FixAction {
    /// Get a short description of the fix
    pub fn description(&self) -> String {
        match self {
            FixAction::AddKernelModule(name) => format!("Add {} kernel module", name),
            FixAction::RestartService(name) => format!("Restart {} service", name),
            FixAction::HostCommand { description, .. } => description.clone(),
        }
    }

    /// Whether the fix takes effect only after a reboot
    pub fn requires_reboot(&self) -> bool {
        matches!(self, FixAction::AddKernelModule(_))
    }

    /// Check if this is a host command (run outside Talos)
    pub fn is_host_command(&self) -> bool {
        matches!(self, FixAction::HostCommand { .. })
    }

    /// The command that applies the fix on `node`, for the user to run.
    /// With a `context`, `talosctl` targets that cluster, not its default.
    pub fn command(&self, context: Option<&str>, node: &str) -> String {
        let node = talos_rs::host_of(node);
        let talosctl = match context {
            Some(context) => format!("talosctl --context {context} -n {node}"),
            None => format!("talosctl -n {node}"),
        };
        match self {
            FixAction::AddKernelModule(name) => format!(
                "{talosctl} patch machineconfig --patch \
                 '{{\"machine\":{{\"kernel\":{{\"modules\":[{{\"name\":\"{name}\"}}]}}}}}}'"
            ),
            FixAction::RestartService(service) => format!("{talosctl} service {service} restart"),
            FixAction::HostCommand { command, .. } => command.clone(),
        }
    }
}

/// A diagnostic fix with description
#[derive(Debug, Clone)]
pub struct DiagnosticFix {
    /// Description of what this fix does
    pub description: String,
    /// The action to take
    pub action: FixAction,
}

/// A single diagnostic check result
#[derive(Debug, Clone)]
pub struct DiagnosticCheck {
    /// Unique identifier for this check
    pub id: String,
    /// Display name of the check
    pub name: String,
    /// Current status
    pub status: CheckStatus,
    /// Status message (e.g., "2.1 GB / 4.0 GB")
    pub message: String,
    /// Additional details (shown when selected)
    pub details: Option<String>,
    /// Available fix if status is Fail or Warn
    pub fix: Option<DiagnosticFix>,
}

impl DiagnosticCheck {
    /// Create a new check with Pass status
    pub fn pass(id: &str, name: &str, message: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            status: CheckStatus::Pass,
            message: message.to_string(),
            details: None,
            fix: None,
        }
    }

    /// Create a new check with Fail status and optional fix
    pub fn fail(id: &str, name: &str, message: &str, fix: Option<DiagnosticFix>) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            status: CheckStatus::Fail,
            message: message.to_string(),
            details: None,
            fix,
        }
    }

    /// Create a new check with Warn status
    pub fn warn(id: &str, name: &str, message: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            status: CheckStatus::Warn,
            message: message.to_string(),
            details: None,
            fix: None,
        }
    }

    /// Create a new check with Unknown status
    pub fn unknown(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            status: CheckStatus::Unknown,
            message: "Unknown".to_string(),
            details: None,
            fix: None,
        }
    }

    /// Set details for this check
    pub fn with_details(mut self, details: &str) -> Self {
        self.details = Some(details.to_string());
        self
    }

    /// Set a fix for this check
    pub fn with_fix(mut self, fix: DiagnosticFix) -> Self {
        self.fix = Some(fix);
        self
    }
}

/// The fix shown in the dialog
#[derive(Debug, Clone)]
pub struct PendingAction {
    /// The check this action is for
    pub check_id: String,
    /// The fix to show
    pub fix: DiagnosticFix,
}

/// Context passed to diagnostic providers
#[derive(Debug, Clone)]
pub struct DiagnosticContext {
    /// Platform type (e.g., "container", "metal", "aws")
    pub platform: String,
    /// Whether running in a container environment
    pub is_container: bool,
    /// Detected CNI type
    pub cni_type: CniType,
    /// Node role (controlplane or worker)
    pub node_role: String,
    /// Node hostname (used as container name in Docker)
    pub hostname: String,
    /// Node endpoint (IP address for talosctl commands)
    pub node_endpoint: Option<String>,
    /// Talos context of the node's cluster (`--context` for talosctl commands)
    pub talos_context: Option<String>,
    /// Custom talosconfig path (`--talosconfig` for talosctl commands)
    pub config_path: Option<String>,
    /// CNI information from K8s API (if available)
    pub cni_info: Option<CniInfo>,
    /// Pod health information from K8s API (if available)
    pub pod_health: Option<PodHealthInfo>,
    /// Number of CPU cores (for load threshold scaling)
    pub cpu_count: Option<usize>,
    /// K8s client error (if client creation failed)
    pub k8s_error: Option<String>,
}

impl DiagnosticContext {
    pub fn new() -> Self {
        Self {
            platform: String::new(),
            is_container: false,
            cni_type: CniType::Unknown,
            node_role: String::new(),
            hostname: String::new(),
            node_endpoint: None,
            talos_context: None,
            config_path: None,
            cni_info: None,
            pod_health: None,
            cpu_count: None,
            k8s_error: None,
        }
    }
}

impl Default for DiagnosticContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_commands_target_the_cluster_and_node() {
        assert_eq!(
            FixAction::RestartService("kubelet".into()).command(Some("bc"), "10.0.0.1:50000"),
            "talosctl --context bc -n 10.0.0.1 service kubelet restart"
        );
        assert_eq!(
            FixAction::AddKernelModule("br_netfilter".into()).command(None, "[fd00::1]:50000"),
            "talosctl -n fd00::1 patch machineconfig --patch \
             '{\"machine\":{\"kernel\":{\"modules\":[{\"name\":\"br_netfilter\"}]}}}'"
        );
        let host = FixAction::HostCommand {
            command: "sudo modprobe br_netfilter".into(),
            description: "Load br_netfilter".into(),
        };
        assert_eq!(
            host.command(Some("bc"), "10.0.0.1"),
            "sudo modprobe br_netfilter"
        );
    }
}
