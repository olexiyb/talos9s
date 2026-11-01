//! Diagnostic types for health checks and CNI detection
//!
//! This module provides domain types for the diagnostics system,
//! separated from UI concerns.

use crate::{HasHealth, HealthIndicator};
use serde::{Deserialize, Serialize};

/// Status of a diagnostic check
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CheckStatus {
    /// Check passed
    Pass,
    /// Warning condition
    Warn,
    /// Check failed - action may be available
    Fail,
    /// Status unknown or still checking
    #[default]
    Unknown,
    /// Currently checking
    Checking,
}

impl HasHealth for CheckStatus {
    fn health(&self) -> HealthIndicator {
        match self {
            CheckStatus::Pass => HealthIndicator::Healthy,
            CheckStatus::Warn => HealthIndicator::Warning,
            CheckStatus::Fail => HealthIndicator::Error,
            CheckStatus::Unknown => HealthIndicator::Unknown,
            CheckStatus::Checking => HealthIndicator::Pending,
        }
    }
}

/// Category of diagnostic checks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckCategory {
    System,
    Kubernetes,
    Services,
    Cni,
    Addons,
}

impl CheckCategory {
    /// Get the display title for this category
    pub fn title(&self) -> &'static str {
        match self {
            CheckCategory::System => "System Health",
            CheckCategory::Kubernetes => "Kubernetes Components",
            CheckCategory::Services => "Services",
            CheckCategory::Cni => "CNI",
            CheckCategory::Addons => "Addons",
        }
    }
}

/// Detected CNI type
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CniType {
    #[default]
    Unknown,
    Flannel,
    Cilium,
    Calico,
    None,
}

impl CniType {
    /// Get the display name for this CNI type
    pub fn name(&self) -> &'static str {
        match self {
            CniType::Unknown => "Unknown",
            CniType::Flannel => "Flannel",
            CniType::Cilium => "Cilium",
            CniType::Calico => "Calico",
            CniType::None => "None",
        }
    }
}

/// Information about a CNI pod
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniPodInfo {
    /// Pod name
    pub name: String,
    /// Node the pod is running on
    pub node_name: Option<String>,
    /// Pod phase (Running, Pending, etc.)
    pub phase: String,
    /// Whether pod is ready
    pub ready: bool,
    /// Number of restarts
    pub restart_count: i32,
}

/// CNI information from K8s API
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CniInfo {
    /// Detected CNI type
    pub cni_type: CniType,
    /// CNI pods in kube-system
    pub pods: Vec<CniPodInfo>,
}

impl CniInfo {
    /// Check if all CNI pods are healthy
    pub fn are_pods_healthy(&self) -> bool {
        if self.pods.is_empty() {
            return false;
        }
        self.pods
            .iter()
            .all(|pod| pod.phase == "Running" && pod.ready)
    }

    /// Get summary of CNI pod health
    pub fn pod_health_summary(&self) -> String {
        if self.pods.is_empty() {
            return "No CNI pods found".to_string();
        }

        let total = self.pods.len();
        let healthy = self
            .pods
            .iter()
            .filter(|p| p.phase == "Running" && p.ready)
            .count();
        let total_restarts: i32 = self.pods.iter().map(|p| p.restart_count).sum();

        if healthy == total && total_restarts == 0 {
            format!("{}/{} pods healthy", healthy, total)
        } else if healthy == total {
            format!(
                "{}/{} pods healthy ({} restarts)",
                healthy, total, total_restarts
            )
        } else {
            format!("{}/{} pods healthy", healthy, total)
        }
    }
}

/// Information about an unhealthy pod (from K8s API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnhealthyPodInfo {
    /// Pod name
    pub name: String,
    /// Pod namespace
    pub namespace: String,
    /// Container state (e.g., "CrashLoopBackOff", "ImagePullBackOff")
    pub state: String,
    /// Number of restarts
    pub restart_count: i32,
}

/// Pod health information from K8s API
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PodHealthInfo {
    /// Pods in CrashLoopBackOff
    pub crashing: Vec<UnhealthyPodInfo>,
    /// Pods in ImagePullBackOff
    pub image_pull_errors: Vec<UnhealthyPodInfo>,
    /// Total pod count
    pub total_pods: usize,
}

impl PodHealthInfo {
    /// Check if there are any unhealthy pods
    pub fn has_issues(&self) -> bool {
        !self.crashing.is_empty() || !self.image_pull_errors.is_empty()
    }

    /// Get summary message
    pub fn summary(&self) -> String {
        if self.crashing.is_empty() && self.image_pull_errors.is_empty() {
            "All pods healthy".to_string()
        } else {
            let mut parts = Vec::new();
            if !self.crashing.is_empty() {
                parts.push(format!("{} crashing", self.crashing.len()));
            }
            if !self.image_pull_errors.is_empty() {
                parts.push(format!("{} image errors", self.image_pull_errors.len()));
            }
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_status_health() {
        assert_eq!(CheckStatus::Pass.health(), HealthIndicator::Healthy);
        assert_eq!(CheckStatus::Warn.health(), HealthIndicator::Warning);
        assert_eq!(CheckStatus::Fail.health(), HealthIndicator::Error);
        assert_eq!(CheckStatus::Unknown.health(), HealthIndicator::Unknown);
        assert_eq!(CheckStatus::Checking.health(), HealthIndicator::Pending);
    }

    #[test]
    fn test_check_category_title() {
        assert_eq!(CheckCategory::System.title(), "System Health");
        assert_eq!(CheckCategory::Cni.title(), "CNI");
    }

    #[test]
    fn test_cni_type_name() {
        assert_eq!(CniType::Flannel.name(), "Flannel");
        assert_eq!(CniType::Unknown.name(), "Unknown");
    }

    #[test]
    fn test_cni_info_health() {
        let info = CniInfo::default();
        assert!(!info.are_pods_healthy()); // Empty = not healthy

        let healthy_info = CniInfo {
            cni_type: CniType::Flannel,
            pods: vec![CniPodInfo {
                name: "flannel-1".to_string(),
                node_name: Some("node1".to_string()),
                phase: "Running".to_string(),
                ready: true,
                restart_count: 0,
            }],
        };
        assert!(healthy_info.are_pods_healthy());
    }

    #[test]
    fn test_pod_health_info() {
        let healthy = PodHealthInfo::default();
        assert!(!healthy.has_issues());
        assert_eq!(healthy.summary(), "All pods healthy");

        let unhealthy = PodHealthInfo {
            crashing: vec![UnhealthyPodInfo {
                name: "app-1".to_string(),
                namespace: "default".to_string(),
                state: "CrashLoopBackOff".to_string(),
                restart_count: 5,
            }],
            image_pull_errors: vec![],
            total_pods: 10,
        };
        assert!(unhealthy.has_issues());
        assert_eq!(unhealthy.summary(), "1 crashing");
    }
}
