//! Shared constants for Talos and Kubernetes
//!
//! This module contains domain-specific constants like thresholds,
//! CRD names for addon detection, and other shared values.

/// Threshold for considering a pod to have "high" restarts
pub const HIGH_RESTART_THRESHOLD: i32 = 5;

/// Restarts count as "high" only while the last one is this recent: the
/// count never goes down, so old restarts would flag a pod forever
pub const RECENT_RESTART_SECS: i64 = 3600;

/// Days left on a certificate that are worth a look / urgent
pub const CERT_WARN_DAYS: i64 = 30;
pub const CERT_CRITICAL_DAYS: i64 = 7;

/// Clock offsets that are worth a look / a problem (seconds): etcd leases
/// and certificate checks start failing well before a few seconds
pub const CLOCK_WARN_SECS: f64 = 0.1;
pub const CLOCK_ERROR_SECS: f64 = 1.0;

/// etcd member checks
pub mod etcd {
    /// A defrag is worth it when at least this share of the DB file is free...
    pub const DEFRAG_MIN_FREE_PCT: i64 = 50;
    /// ...and that is at least this much space
    pub const DEFRAG_MIN_FREE_BYTES: i64 = 200 << 20;
    /// Committed but not applied entries worth a look; etcd itself rejects
    /// new requests ("too many requests") past 5000
    pub const APPLY_LAG_WARN: u64 = 1_000;
    pub const APPLY_LAG_ERROR: u64 = 5_000;
    /// Raft entries a member may trail the leader by. Statuses are read a
    /// few milliseconds apart, so a busy cluster shows small gaps even when
    /// healthy.
    pub const BEHIND_LEADER_WARN: u64 = 10_000;
    /// etcd's default `quota-backend-bytes`; past it etcd raises NOSPACE and
    /// stops accepting writes
    pub const DEFAULT_QUOTA_BYTES: i64 = 2 << 30;
    pub const QUOTA_WARN_PCT: i64 = 80;
}

/// TCP connections in one state that are worth a look on a node. Busy
/// healthy nodes hold hundreds of TIME_WAIT and the odd SYN_SENT; these
/// flag port exhaustion (ephemeral range ~28k), sockets an app never closes,
/// and many peers that don't answer.
pub const TIME_WAIT_WARN: usize = 20_000;
pub const CLOSE_WAIT_WARN: usize = 100;
pub const SYN_SENT_WARN: usize = 50;

/// A pod Pending for less than this is being scheduled or pulled, not stuck
pub const POD_PENDING_GRACE_SECS: i64 = 120;

/// Memory in use (percent) that is a warning / critical. One source for the
/// dashboard's colors and the diagnostics verdicts, so they never disagree.
pub const MEMORY_WARN_PERCENT: f64 = 80.0;
pub const MEMORY_CRITICAL_PERCENT: f64 = 90.0;

/// 1-minute load average per CPU core that is a warning / critical: load
/// means nothing without the core count (5 is idle on 32 cores)
pub const LOAD_WARN_PER_CORE: f64 = 1.5;
pub const LOAD_CRITICAL_PER_CORE: f64 = 2.0;

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
