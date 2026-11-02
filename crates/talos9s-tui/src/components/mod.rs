//! Component system for talos9s TUI
//!
//! Based on the ratatui Component template pattern.

pub mod cluster;
pub mod containers;
pub mod diagnostics;
pub mod etcd;
pub mod group;
pub mod lifecycle;
pub mod multi_logs;
pub mod namespace_picker;
pub mod network;
pub mod node_info;
pub mod node_operations;
pub mod pod_logs;
pub mod processes;
pub mod rolling_operations;
pub mod security;
pub mod storage;
pub mod volumes;
pub mod wizard;
pub mod workloads;

pub use cluster::ClusterComponent;
pub use containers::ContainersComponent;
pub use diagnostics::DiagnosticsComponent;
pub use etcd::EtcdComponent;
pub use group::{GroupComponent, GroupKind, GroupNode};
pub use lifecycle::LifecycleComponent;
pub use multi_logs::{KERNEL_SERVICE, LogNode, MultiLogsComponent};
pub use network::NetworkStatsComponent;
pub use node_info::{NodeInfoComponent, NodeInfoTab};
pub use node_operations::NodeOperationsComponent;
pub use processes::{ProcessNode, ProcessesComponent};
pub use rolling_operations::RollingOperationsComponent;
pub use security::SecurityComponent;
pub use storage::{StorageComponent, StorageViewMode};
pub use volumes::VolumesComponent;
pub use wizard::WizardComponent;
pub use workloads::WorkloadHealthComponent;

use crate::action::Action;
use color_eyre::Result;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect, text::Line};

/// Trait for UI components
///
/// Components are modular, reusable UI elements that can handle events,
/// update their state, and render themselves.
pub trait Component {
    /// Initialize the component with the given area
    fn init(&mut self, _area: Rect) -> Result<()> {
        Ok(())
    }

    /// Handle key events and optionally produce actions
    fn handle_key_event(&mut self, _key: KeyEvent) -> Result<Option<Action>> {
        Ok(None)
    }

    /// Handle mouse events and optionally produce actions
    fn handle_mouse_event(&mut self, _mouse: MouseEvent) -> Result<Option<Action>> {
        Ok(None)
    }

    /// Whether the component is taking typed text (a filter or search box),
    /// so keys the app would otherwise use (layout keys) belong to it
    fn captures_text(&self) -> bool {
        false
    }

    /// Whether Left/Right currently mean something to the component (a
    /// horizontal scroll that isn't at its start, a Yes/No dialog), so the
    /// app leaves them to it instead of using Left to return to the tree
    fn uses_horizontal_keys(&self) -> bool {
        false
    }

    /// Whether the component switches its own tabs with Tab (a visible tab
    /// bar); otherwise Tab moves to the next window
    fn uses_tab_key(&self) -> bool {
        false
    }

    /// The keys that act in the component right now, shown on the screen's
    /// bottom line while the component has the keyboard. Views draw no key
    /// line of their own.
    fn key_hints(&self) -> Option<Line<'static>> {
        None
    }

    /// Update the component state based on an action
    fn update(&mut self, action: Action) -> Result<Option<Action>>;

    /// Render the component to the frame
    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()>;
}

/// A component whose refresh can run on a clone, off the event loop.
///
/// The app clones the component, runs `refresh()` on the clone in a spawned
/// task, and hands the finished clone back via [`adopt_refresh`] so the UI
/// keeps drawing and handling keys while slow API calls are in flight.
///
/// [`adopt_refresh`]: BackgroundRefresh::adopt_refresh
pub trait BackgroundRefresh: Clone + Send + 'static {
    /// Take the data `refreshed` loaded while keeping the UI state (selection,
    /// dialogs, scroll) the user may have changed during the refresh.
    fn adopt_refresh(&mut self, refreshed: Self);
}

#[cfg(test)]
mod layout_tests;
