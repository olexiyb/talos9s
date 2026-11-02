//! talos9s-core: Core business logic for talos9s
//!
//! This crate contains domain types, shared utilities, and business logic
//! for the talos9s TUI. It is intentionally kept independent of any
//! TUI framework to enable:
//!
//! - Unit testing without UI dependencies
//! - Reuse in CLI tools or other consumers
//! - Clear separation between business logic and presentation
//!
//! # Modules
//!
//! - [`types`] - Core domain types (Cluster, Node, Service, etc.)
//! - [`indicators`] - Health and status indicators for consistent UI representation
//! - [`formatting`] - Utilities for formatting bytes, durations, percentages, etc.
//! - [`selection`] - Generic selection logic for list-based UI components
//! - [`async_state`] - Async component state management (loading, error, refresh)
//! - [`errors`] - Error formatting utilities for user-friendly messages
//! - [`network`] - Network analysis utilities (port mapping, connection classification)
//! - [`diagnostics`] - Diagnostic types for health checks and CNI detection
//! - [`stats`] - Summaries over node values (common version, capacity)
//! - [`version`] - Release version comparison and the update-check schedule
//! - [`constants`] - Shared constants (thresholds, CRD names, refresh intervals)

pub mod async_state;
pub mod constants;
pub mod convert;
pub mod diagnostics;
pub mod errors;
pub mod etcd;
pub mod formatting;
pub mod indicators;
pub mod logparse;
pub mod network;
pub mod selection;
pub mod stats;
pub mod version;

// Re-export commonly used items at crate root
pub use async_state::*;
pub use diagnostics::*;
pub use errors::*;
pub use etcd::*;
pub use formatting::*;
pub use indicators::*;
pub use selection::*;

// Network is not re-exported at root to avoid name conflicts
// Use talos9s_core::network::* explicitly
