//! Cluster component - displays cluster overview with nodes

mod actions;
mod data;
mod details;
mod frame;
mod group_table;
mod helpers;
mod keys;
mod nodes_pane;
mod refresh;
mod stats;
#[cfg(test)]
mod tests;
mod tree;

use crate::action::Action;
use crate::components::diagnostics::{DiagnosticsSummary, k8s};
use crate::components::{
    Component, GroupKind, GroupNode, KERNEL_SERVICE, LogNode, NodeInfoTab, ProcessNode,
    StorageViewMode,
};
use crate::linstor::{LinstorNode, LinstorStatus, StoragePool, fetch_linstor};
use crate::reachability::prefer_reachable_addresses;
use crate::ui_ext::HealthIndicatorExt;
use crate::zfs::{ZfsPool, fetch_zfs_pools};
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use data::*;
use futures::stream::{FuturesUnordered, StreamExt};
use helpers::*;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Widget},
};
use std::collections::HashMap;
use talos_rs::{
    DiscoveryMember, EtcdMemberInfo, MemInfo, NodeCpuInfo, NodeLoadAvg, NodeMemory, NodeMounts,
    NodeNetworkInfo, NodeServices, NodeSystemStat, ServiceInfo, TalosClient, TalosConfig,
    VersionInfo, get_discovery_members_with_retry,
};
use talos9s_core::constants::{CLOCK_ERROR_SECS, CLOCK_WARN_SECS};

/// Which pane is currently focused
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumString, strum::IntoStaticStr)]
pub enum FocusedPane {
    #[default]
    Nodes,
    Menu,
    Services,
}

/// Narrowest the nodes pane gets, so a short list keeps its look
const NODES_PANE_MIN_WIDTH: u16 = 30;

/// A tree selection to come back to (opaque to the app)
#[derive(Debug, Clone, PartialEq)]
pub struct TreeSelection(NodeListItem);

/// What a click in the nodes pane lands on
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTarget {
    /// A node, group or cluster row
    Row,
    /// A menu entry
    Menu,
    /// Empty space, the title, or an error line
    None,
}

/// Width of the nodes pane on the left of the workspace
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
pub enum PaneWidth {
    /// Sized to the node list and menu
    Full,
    /// Node names and menu keys only
    Compact,
    /// Not shown: the view gets the whole width
    Hidden,
}

/// The compact nodes pane: about the longest node name, within these bounds
const COMPACT_PANE_MIN_WIDTH: u16 = 14;
const COMPACT_PANE_MAX_WIDTH: u16 = 22;
/// Columns the view keeps before the nodes pane turns compact on its own
const VIEW_MIN_WIDTH: u16 = 100;

/// Navigation menu items for quick screen access
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumString, strum::IntoStaticStr)]
pub enum NavMenuItem {
    Dashboard,
    Logs,
    Etcd,
    Network,
    KubeSpan,
    Disks,
    Partitions,
    Processes,
    Diagnostics,
    Certs,
    Lifecycle,
    Workloads,
    Volumes,
    Containers,
    Config,
    Extensions,
}

impl NavMenuItem {
    pub const ALL: [NavMenuItem; 16] = [
        NavMenuItem::Dashboard,
        NavMenuItem::Logs,
        NavMenuItem::Etcd,
        NavMenuItem::Network,
        NavMenuItem::KubeSpan,
        NavMenuItem::Disks,
        NavMenuItem::Partitions,
        NavMenuItem::Processes,
        NavMenuItem::Diagnostics,
        NavMenuItem::Certs,
        NavMenuItem::Lifecycle,
        NavMenuItem::Workloads,
        NavMenuItem::Volumes,
        NavMenuItem::Containers,
        NavMenuItem::Config,
        NavMenuItem::Extensions,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            NavMenuItem::Dashboard => "Dashboard",
            NavMenuItem::Logs => "Logs",
            NavMenuItem::Etcd => "etcd",
            NavMenuItem::Network => "Network",
            NavMenuItem::KubeSpan => "KubeSpan",
            NavMenuItem::Disks => "Disks",
            NavMenuItem::Partitions => "Partitions",
            NavMenuItem::Processes => "Processes",
            NavMenuItem::Diagnostics => "Diagnostics",
            NavMenuItem::Certs => "Certificates",
            NavMenuItem::Lifecycle => "Lifecycle",
            NavMenuItem::Workloads => "Workloads",
            NavMenuItem::Volumes => "Volumes",
            NavMenuItem::Containers => "Containers",
            NavMenuItem::Config => "Machine config",
            NavMenuItem::Extensions => "Extensions",
        }
    }

    fn hotkey(&self) -> &'static str {
        match self {
            NavMenuItem::Dashboard => "D",
            NavMenuItem::Logs => "L",
            NavMenuItem::Etcd => "e",
            NavMenuItem::Network => "n",
            NavMenuItem::KubeSpan => "K",
            NavMenuItem::Disks => "s",
            NavMenuItem::Partitions => "t",
            NavMenuItem::Processes => "p",
            NavMenuItem::Diagnostics => "d",
            NavMenuItem::Certs => "c",
            NavMenuItem::Lifecycle => "y",
            NavMenuItem::Workloads => "w",
            NavMenuItem::Volumes => "v",
            NavMenuItem::Containers => "C",
            NavMenuItem::Config => "m",
            NavMenuItem::Extensions => "x",
        }
    }
}

/// Represents a selectable item in the node list (header or node)
#[derive(Debug, Clone, PartialEq)]
enum NodeListItem {
    /// Cluster header (cluster_idx)
    ClusterHeader(usize),
    /// Control plane group header (cluster_idx)
    ControlPlaneHeader(usize),
    /// A control plane node (cluster_idx, node_idx within controlplane_nodes)
    ControlPlaneNode(usize, usize),
    /// Workers group header (cluster_idx)
    WorkersHeader(usize),
    /// A worker node (cluster_idx, node_idx within worker_nodes)
    WorkerNode(usize, usize),
}

/// How long a node that no source lists any more (not in discovery or etcd)
/// is still shown as offline before it is dropped from the list. Nodes that
/// discovery or etcd still list stay as offline for as long as they are listed.
const OFFLINE_NODE_GRACE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Upper bound for the per-node Version call that decides whether a node is
/// online; an unreachable node otherwise waits out apid's ~20s proxy dial.
const NODE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound for one cluster's background refresh. Most calls in it have
/// their own timeout, but a request on a connection that died silently (e.g.
/// across a laptop sleep) can wait forever, and a refresh that never returns
/// is never retried: the cluster would show "connecting…" for good.
const CLUSTER_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Shown for a view picked before the cluster's nodes are known
pub const LOADING_NODES_HINT: &str = "Loading the cluster's nodes…";

/// How often a running cluster refresh hands what it has so far to the
/// dashboard
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Bound on the first call of a refresh. A connection that died silently
/// (network change, tailnet switch) or stalled makes it hang; dropping the
/// client then redials within [`RECONNECT_INTERVAL`] instead of waiting out
/// [`CLUSTER_REFRESH_TIMEOUT`] while views keep using the dead connection.
const MEMBERS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How soon a cluster without a connection is dialled again
const RECONNECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Cluster component showing overview with node list
pub struct ClusterComponent {
    /// All clusters from talosconfig
    clusters: Vec<ClusterData>,
    /// Currently active cluster index (for operations)
    active_cluster: usize,
    /// Currently selected service index within the node
    selected_service: usize,
    /// Last refresh time
    last_refresh: Option<std::time::Instant>,
    /// Which pane is currently focused
    focused_pane: FocusedPane,
    /// Currently selected navigation menu item
    selected_menu_item: usize,
    /// Auto-refresh enabled
    auto_refresh: bool,
    /// Last auto-refresh time for selected node
    last_auto_refresh: Option<std::time::Instant>,
    /// Currently selected item in the node list
    selected_item: NodeListItem,
    /// Custom config file path (from --config flag)
    config_path: Option<String>,
    /// Specific context to use (from --context flag)
    context_filter: Option<String>,
    /// In-flight background refresh per cluster, same order as `clusters`
    /// (see `start_background_refresh`)
    background_refresh: Vec<Option<tokio::task::JoinHandle<ClusterData>>>,
    /// What each running refresh has fetched so far, adopted while it runs
    refresh_progress: Vec<Option<tokio::sync::watch::Receiver<Option<ClusterData>>>>,
    /// When each cluster's last background refresh finished
    cluster_refreshed_at: Vec<Option<std::time::Instant>>,
    /// Network summary per (context, node name), with the time of the last fetch attempt
    network_info: HashMap<(String, String), (std::time::Instant, Option<NodeNetworkInfo>)>,
    /// In-flight network summary fetch for the selected node(s) (see `poll_network_info`)
    network_fetch: Option<NetworkFetch>,
    /// Clock offset per (context, node name), fetched with the network summary
    clock_info: HashMap<(String, String), NodeClock>,
    /// In-flight stats fetch for the selected node (see `refresh_selected_node`)
    node_stats_fetch: Option<tokio::task::JoinHandle<NodeStatsFetch>>,
    /// Screen regions from the last draw, for mapping mouse clicks
    hits: std::cell::RefCell<HitMap>,
    /// The view on the right has the keyboard, not the tree (set per draw)
    view_focused: bool,
    /// Last of Nodes / Menu that had focus, to return to from a view or the
    /// dashboard's services list (set per draw)
    tree_pane: FocusedPane,
    /// Where Esc in the view goes back to, when not just the tree
    back_hint: Option<String>,
    /// Group or node remembered from the last run, selected once the
    /// cluster lists its nodes (unless the user moved first)
    pending_selection: Option<crate::state::Selection>,
    /// A newer talos9s release, for the top line
    newer_version: Option<String>,
    /// First tree row shown, when the tree is taller than its pane
    tree_scroll: usize,
    /// Scroll of the node pane (lines), the node it belongs to, and how far
    /// it can go (0 when everything fits; set per draw)
    details_scroll: usize,
    details_scroll_node: String,
    details_max_scroll: usize,
    /// Last diagnostics result per (cluster, node), shown on the dashboard
    diagnostics_summaries: HashMap<(String, String), DiagnosticsSummary>,
}

/// Where clickable things were drawn on the last frame
#[derive(Default)]
struct HitMap {
    /// The whole nodes pane (tree and menu, with borders)
    tree: Rect,
    /// Node list area and the item drawn on each of its rows (`None` for
    /// error/warning lines)
    node_list: Rect,
    node_rows: Vec<Option<NodeListItem>>,
    /// Navigation menu area; row 0 is the separator, row `i` is item `i`
    menu: Rect,
    /// Details pane area
    details: Rect,
    /// Services list area in the node details (row 0 is its heading) and the
    /// number of services drawn
    /// Services rows visible on screen, the index of the first of them, and
    /// how many services there are
    services: Option<(Rect, usize, usize)>,
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.x + area.width && row >= area.y && row < area.y + area.height
}

impl Default for ClusterComponent {
    fn default() -> Self {
        Self::new(None, None)
    }
}
impl ClusterComponent {
    pub fn new(config_path: Option<String>, context_filter: Option<String>) -> Self {
        Self {
            clusters: Vec::new(),
            active_cluster: 0,
            selected_service: 0,
            last_refresh: None,
            focused_pane: FocusedPane::Nodes,
            selected_menu_item: 0,
            auto_refresh: true,
            last_auto_refresh: None,
            selected_item: NodeListItem::ClusterHeader(0),
            config_path,
            context_filter,
            background_refresh: Vec::new(),
            refresh_progress: Vec::new(),
            cluster_refreshed_at: Vec::new(),
            network_info: HashMap::new(),
            network_fetch: None,
            clock_info: HashMap::new(),
            node_stats_fetch: None,
            hits: std::cell::RefCell::new(HitMap::default()),
            view_focused: false,
            tree_pane: FocusedPane::Nodes,
            back_hint: None,
            pending_selection: None,
            newer_version: None,
            tree_scroll: 0,
            details_scroll: 0,
            details_scroll_node: String::new(),
            details_max_scroll: 0,
            diagnostics_summaries: HashMap::new(),
        }
    }
}
