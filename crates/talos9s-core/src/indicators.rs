//! Health and status indicators for consistent UI representation
//!
//! This module provides unified types for representing health states,
//! eliminating duplication across TUI components.

use serde::{Deserialize, Serialize};

/// Universal health/status indicator
///
/// Represents the health state of any entity (node, service, check, etc.)
/// with consistent visual representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum HealthIndicator {
    /// Fully healthy/operational
    Healthy,
    /// Degraded but functional
    Warning,
    /// Failed or critical error
    Error,
    /// Pending or transitioning state
    Pending,
    /// Informational (neutral)
    Info,
    /// State cannot be determined
    #[default]
    Unknown,
}

impl HealthIndicator {
    /// Unicode symbol for this status
    ///
    /// Returns a single character that visually represents the state.
    pub fn symbol(&self) -> &'static str {
        match self {
            HealthIndicator::Healthy => "●",
            HealthIndicator::Warning => "◐",
            HealthIndicator::Error => "✗",
            HealthIndicator::Pending => "○",
            HealthIndicator::Info => "○",
            HealthIndicator::Unknown => "?",
        }
    }

    /// Human-readable label for this status
    pub fn label(&self) -> &'static str {
        match self {
            HealthIndicator::Healthy => "Healthy",
            HealthIndicator::Warning => "Warning",
            HealthIndicator::Error => "Error",
            HealthIndicator::Pending => "Pending",
            HealthIndicator::Info => "Info",
            HealthIndicator::Unknown => "Unknown",
        }
    }

    /// Check if this represents a healthy state
    pub fn is_healthy(&self) -> bool {
        matches!(self, HealthIndicator::Healthy)
    }

    /// Check if this represents a warning or worse
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            HealthIndicator::Warning | HealthIndicator::Error | HealthIndicator::Unknown
        )
    }

    /// Get severity level (for sorting/prioritization)
    ///
    /// Higher numbers = more severe
    pub fn severity(&self) -> u8 {
        match self {
            HealthIndicator::Healthy => 0,
            HealthIndicator::Info => 1,
            HealthIndicator::Pending => 2,
            HealthIndicator::Unknown => 3,
            HealthIndicator::Warning => 4,
            HealthIndicator::Error => 5,
        }
    }

    /// Return the more severe of two indicators
    pub fn worst(self, other: HealthIndicator) -> HealthIndicator {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }
}

impl std::fmt::Display for HealthIndicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// Trait for types that can report their health status
///
/// Implement this trait to provide consistent health reporting
/// across different entity types.
pub trait HasHealth {
    /// Return the current health indicator
    fn health(&self) -> HealthIndicator;

    /// Check if the entity is in a healthy state
    fn is_healthy(&self) -> bool {
        self.health().is_healthy()
    }

    /// Check if the entity needs attention
    fn needs_attention(&self) -> bool {
        self.health().needs_attention()
    }
}

/// Connection state indicator
///
/// Represents the connection state of a node or service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ConnectionState {
    /// Fully connected and responsive
    Connected,
    /// Partially connected or some issues
    Partial,
    /// Disconnected or unreachable
    Disconnected,
    /// Connection state unknown
    #[default]
    Unknown,
}

impl ConnectionState {
    /// Unicode symbol for this state
    pub fn symbol(&self) -> &'static str {
        match self {
            ConnectionState::Connected => "●",
            ConnectionState::Partial => "◐",
            ConnectionState::Disconnected => "○",
            ConnectionState::Unknown => "?",
        }
    }

    /// Human-readable label
    pub fn label(&self) -> &'static str {
        match self {
            ConnectionState::Connected => "Connected",
            ConnectionState::Partial => "Partial",
            ConnectionState::Disconnected => "Disconnected",
            ConnectionState::Unknown => "Unknown",
        }
    }
}

impl From<ConnectionState> for HealthIndicator {
    fn from(state: ConnectionState) -> Self {
        match state {
            ConnectionState::Connected => HealthIndicator::Healthy,
            ConnectionState::Partial => HealthIndicator::Warning,
            ConnectionState::Disconnected => HealthIndicator::Error,
            ConnectionState::Unknown => HealthIndicator::Unknown,
        }
    }
}

/// Quorum state for clustered services (etcd, etc.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QuorumState {
    /// Cluster has full quorum - all members healthy
    Healthy,
    /// Cluster is degraded but has quorum
    Degraded { healthy: usize, total: usize },
    /// Cluster has lost quorum - critical
    NoQuorum { healthy: usize, total: usize },
    /// Unknown state (loading or error)
    #[default]
    Unknown,
}

impl QuorumState {
    /// Calculate quorum state from member counts
    pub fn from_counts(healthy: usize, total: usize) -> Self {
        if total == 0 {
            return QuorumState::Unknown;
        }

        let required = (total / 2) + 1;

        if healthy == total {
            QuorumState::Healthy
        } else if healthy >= required {
            QuorumState::Degraded { healthy, total }
        } else {
            QuorumState::NoQuorum { healthy, total }
        }
    }

    /// Check if quorum is maintained
    pub fn has_quorum(&self) -> bool {
        matches!(self, QuorumState::Healthy | QuorumState::Degraded { .. })
    }

    /// Get display text and color hint for the quorum state
    pub fn display(&self) -> (&'static str, &'static str) {
        match self {
            QuorumState::Healthy => ("HEALTHY", "green"),
            QuorumState::Degraded { .. } => ("DEGRADED", "yellow"),
            QuorumState::NoQuorum { .. } => ("NO QUORUM", "red"),
            QuorumState::Unknown => ("UNKNOWN", "gray"),
        }
    }
}

impl HasHealth for QuorumState {
    fn health(&self) -> HealthIndicator {
        match self {
            QuorumState::Healthy => HealthIndicator::Healthy,
            QuorumState::Degraded { .. } => HealthIndicator::Warning,
            QuorumState::NoQuorum { .. } => HealthIndicator::Error,
            QuorumState::Unknown => HealthIndicator::Unknown,
        }
    }
}

/// Safety status for operations that may have risks
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SafetyStatus {
    /// Operation is safe to proceed
    #[default]
    Safe,
    /// Operation has warnings but can proceed
    Warning(String),
    /// Operation is unsafe and should not proceed
    Unsafe(String),
    /// Safety status is unknown (still loading)
    Unknown,
}

impl SafetyStatus {
    /// Get the reason if unsafe or warning
    pub fn reason(&self) -> Option<&str> {
        match self {
            SafetyStatus::Safe | SafetyStatus::Unknown => None,
            SafetyStatus::Warning(reason) | SafetyStatus::Unsafe(reason) => Some(reason),
        }
    }
}

impl HasHealth for SafetyStatus {
    fn health(&self) -> HealthIndicator {
        match self {
            SafetyStatus::Safe => HealthIndicator::Healthy,
            SafetyStatus::Warning(_) => HealthIndicator::Warning,
            SafetyStatus::Unsafe(_) => HealthIndicator::Error,
            SafetyStatus::Unknown => HealthIndicator::Unknown,
        }
    }
}

/// Health of a node's memory use (percent in use)
pub fn memory_health(used_percent: f64) -> HealthIndicator {
    use crate::constants::{MEMORY_CRITICAL_PERCENT, MEMORY_WARN_PERCENT};
    if used_percent > MEMORY_CRITICAL_PERCENT {
        HealthIndicator::Error
    } else if used_percent > MEMORY_WARN_PERCENT {
        HealthIndicator::Warning
    } else {
        HealthIndicator::Healthy
    }
}

/// Health of a node's 1-minute load average for its core count; `Unknown`
/// without the core count, since load alone can't be judged
pub fn load_health(load1: f64, cpu_count: Option<usize>) -> HealthIndicator {
    use crate::constants::{LOAD_CRITICAL_PER_CORE, LOAD_WARN_PER_CORE};
    let Some(cores) = cpu_count.filter(|c| *c > 0) else {
        return HealthIndicator::Unknown;
    };
    let per_core = load1 / cores as f64;
    if per_core > LOAD_CRITICAL_PER_CORE {
        HealthIndicator::Error
    } else if per_core > LOAD_WARN_PER_CORE {
        HealthIndicator::Warning
    } else {
        HealthIndicator::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_and_load_thresholds() {
        assert_eq!(memory_health(70.0), HealthIndicator::Healthy);
        assert_eq!(memory_health(85.0), HealthIndicator::Warning);
        assert_eq!(memory_health(95.0), HealthIndicator::Error);
        // Load 5.5 is fine on 32 cores, too much on 2
        assert_eq!(load_health(5.5, Some(32)), HealthIndicator::Healthy);
        assert_eq!(load_health(3.5, Some(2)), HealthIndicator::Warning);
        assert_eq!(load_health(5.0, Some(2)), HealthIndicator::Error);
        assert_eq!(load_health(5.0, None), HealthIndicator::Unknown);
    }

    #[test]
    fn test_health_indicator_severity() {
        assert!(HealthIndicator::Error.severity() > HealthIndicator::Warning.severity());
        assert_eq!(
            HealthIndicator::Healthy.worst(HealthIndicator::Warning),
            HealthIndicator::Warning
        );
    }

    #[test]
    fn test_health_indicator_worst() {
        assert_eq!(
            HealthIndicator::Healthy.worst(HealthIndicator::Error),
            HealthIndicator::Error
        );
        assert_eq!(
            HealthIndicator::Error.worst(HealthIndicator::Healthy),
            HealthIndicator::Error
        );
    }

    #[test]
    fn test_quorum_state_from_counts() {
        // Empty cluster
        assert!(matches!(
            QuorumState::from_counts(0, 0),
            QuorumState::Unknown
        ));

        // 3-node cluster
        assert!(matches!(
            QuorumState::from_counts(3, 3),
            QuorumState::Healthy
        ));
        assert!(matches!(
            QuorumState::from_counts(2, 3),
            QuorumState::Degraded { .. }
        ));
        assert!(matches!(
            QuorumState::from_counts(1, 3),
            QuorumState::NoQuorum { .. }
        ));

        // 5-node cluster
        assert!(matches!(
            QuorumState::from_counts(5, 5),
            QuorumState::Healthy
        ));
        assert!(matches!(
            QuorumState::from_counts(3, 5),
            QuorumState::Degraded { .. }
        ));
        assert!(matches!(
            QuorumState::from_counts(2, 5),
            QuorumState::NoQuorum { .. }
        ));
    }

    #[test]
    fn test_has_health_trait() {
        let quorum = QuorumState::Healthy;
        assert!(quorum.is_healthy());
        assert!(!quorum.needs_attention());

        let degraded = QuorumState::Degraded {
            healthy: 2,
            total: 3,
        };
        assert!(!degraded.is_healthy());
        assert!(degraded.needs_attention());
    }

    #[test]
    fn test_safety_status() {
        let safe = SafetyStatus::Safe;
        assert!(matches!(safe, SafetyStatus::Safe));
        assert!(safe.reason().is_none());

        let unsafe_op = SafetyStatus::Unsafe("Would lose quorum".to_string());
        assert!(!matches!(unsafe_op, SafetyStatus::Safe));
        assert_eq!(unsafe_op.reason(), Some("Would lose quorum"));
    }
}
