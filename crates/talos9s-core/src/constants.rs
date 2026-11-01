//! Shared constants for Talos and Kubernetes
//!
//! This module contains domain-specific constants like thresholds,
//! CRD names for addon detection, and other shared values.

/// Threshold for considering a pod to have "high" restarts
pub const HIGH_RESTART_THRESHOLD: i32 = 5;

/// Maximum log entries to retain in memory
pub const MAX_LOG_ENTRIES: usize = 5000;

/// Maximum packet capture size (40 MB)
pub const MAX_CAPTURE_SIZE: usize = 40 * 1024 * 1024;

// =============================================================================
// Addon Detection CRDs
// =============================================================================

/// cert-manager CRDs for detection
pub const CERT_MANAGER_CRDS: &[&str] = &[
    "certificates.cert-manager.io",
    "issuers.cert-manager.io",
    "clusterissuers.cert-manager.io",
];

/// external-secrets CRDs for detection
pub const EXTERNAL_SECRETS_CRDS: &[&str] = &[
    "externalsecrets.external-secrets.io",
    "secretstores.external-secrets.io",
];

/// Kyverno CRDs for detection
pub const KYVERNO_CRDS: &[&str] = &["clusterpolicies.kyverno.io", "policies.kyverno.io"];

/// ArgoCD CRDs for detection
pub const ARGOCD_CRDS: &[&str] = &["applications.argoproj.io", "appprojects.argoproj.io"];

/// Flux CRDs for detection
pub const FLUX_CRDS: &[&str] = &[
    "kustomizations.kustomize.toolkit.fluxcd.io",
    "gitrepositories.source.toolkit.fluxcd.io",
];

// =============================================================================
// Default Refresh Intervals
// =============================================================================

/// Default refresh intervals for components (in seconds)
pub mod refresh_intervals {
    /// Fast refresh for real-time data (network stats)
    pub const FAST: u64 = 2;

    /// Normal refresh for moderately changing data (processes, etcd)
    pub const NORMAL: u64 = 5;

    /// Slow refresh for stable data (workloads, diagnostics)
    pub const SLOW: u64 = 10;

    /// Very slow refresh for rarely changing data (lifecycle, security)
    pub const VERY_SLOW: u64 = 30;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thresholds() {
        const { assert!(HIGH_RESTART_THRESHOLD > 0) };
        const { assert!(MAX_LOG_ENTRIES > 0) };
        const { assert!(MAX_CAPTURE_SIZE > 0) };
    }

    #[test]
    fn test_crd_lists_not_empty() {
        assert!(!CERT_MANAGER_CRDS.is_empty());
        assert!(!EXTERNAL_SECRETS_CRDS.is_empty());
        assert!(!KYVERNO_CRDS.is_empty());
        assert!(!ARGOCD_CRDS.is_empty());
        assert!(!FLUX_CRDS.is_empty());
    }

    #[test]
    fn test_refresh_intervals_ordering() {
        const { assert!(refresh_intervals::FAST < refresh_intervals::NORMAL) };
        const { assert!(refresh_intervals::NORMAL < refresh_intervals::SLOW) };
        const { assert!(refresh_intervals::SLOW < refresh_intervals::VERY_SLOW) };
    }
}
