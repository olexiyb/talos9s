//! Network Stats component - displays network interface statistics for a node
//!
//! "Is the network the problem?"

mod capture;
mod connections;
mod file_viewer;
mod interfaces;
mod kubespan;
mod services;

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, TableState},
};
use std::collections::HashMap;
use std::time::Instant;
use talos_rs::{
    ConnectionCounts, ConnectionInfo, KubeSpanPeerStatus, NetDevRate, NetDevStats, NetstatFilter,
    NodeConnections, NodeNetworkStats, NodeServices, ServiceInfo, ServiceRestartResult,
    TalosClient, TalosError,
};
use talos9s_core::AsyncState;
use talos9s_core::constants::{CLOSE_WAIT_WARN, SYN_SENT_WARN, TIME_WAIT_WARN};
use tokio::task::JoinHandle;

/// How long one network call may take before it counts as failed
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run a Talos call with `CALL_TIMEOUT`, turning a timeout into an error
async fn with_timeout<T>(
    call: impl std::future::Future<Output = std::result::Result<T, TalosError>>,
) -> std::result::Result<T, TalosError> {
    match tokio::time::timeout(CALL_TIMEOUT, call).await {
        Ok(result) => result,
        Err(_) => Err(TalosError::Connection(format!(
            "request timed out after {}s",
            CALL_TIMEOUT.as_secs()
        ))),
    }
}

/// Results of one background stats fetch: interface stats, netstat, services
type StatsFetch = (
    std::result::Result<Vec<NodeNetworkStats>, TalosError>,
    std::result::Result<Vec<NodeConnections>, TalosError>,
    std::result::Result<Vec<NodeServices>, TalosError>,
);

/// A service restart's id and outcome
type RestartResult = (
    String,
    std::result::Result<Vec<ServiceRestartResult>, TalosError>,
);

/// A file viewer fetch: viewer, path, and content
type FileResult = (
    FileViewerType,
    &'static str,
    std::result::Result<String, TalosError>,
);

/// An opened packet capture stream
type CaptureResult = std::result::Result<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, TalosError>;

/// Network calls running in spawned tasks, so a node that stops answering
/// never blocks the event loop. Each is picked up on `Tick`.
#[derive(Default)]
struct NetworkTasks {
    stats: Option<JoinHandle<StatsFetch>>,
    restart: Option<JoinHandle<RestartResult>>,
    file: Option<JoinHandle<FileResult>>,
    capture: Option<JoinHandle<CaptureResult>>,
}

impl Drop for NetworkTasks {
    fn drop(&mut self) {
        // Results only feed this view. A restart already sent is left to finish.
        if let Some(t) = &self.stats {
            t.abort();
        }
        if let Some(t) = &self.file {
            t.abort();
        }
        if let Some(t) = &self.capture {
            t.abort();
        }
    }
}

/// Take a task's result once it has finished, without waiting
fn take_finished<T>(task: &mut Option<JoinHandle<T>>) -> Option<T> {
    if !task.as_ref().is_some_and(|t| t.is_finished()) {
        return None;
    }
    crate::task::poll_finished(task.take()?)?.ok()
}

/// Well-known Talos/Kubernetes service ports
fn port_to_service(port: u32) -> Option<&'static str> {
    talos9s_core::network::port_to_service_u32(port)
}

/// Ports whose LISTEN shows a Kubernetes component is up: all of them on a
/// control plane node (or when the role isn't known), only the kubelet's
/// on a worker
fn service_ports(controlplane: Option<bool>) -> &'static [(&'static str, u16)] {
    const ALL: [(&str, u16); 5] = [
        ("API", 6443),
        ("Etcd", 2379),
        ("Kubelet", 10250),
        ("Scheduler", 10259),
        ("Controller", 10257),
    ];
    match controlplane {
        Some(false) => &ALL[2..3],
        _ => &ALL,
    }
}

/// Whether a node's TCP state counts are worth a look (see the thresholds)
fn conn_warnings(cc: &ConnectionCounts) -> bool {
    cc.time_wait >= TIME_WAIT_WARN
        || cc.close_wait >= CLOSE_WAIT_WARN
        || cc.syn_sent >= SYN_SENT_WARN
}

const AUTO_REFRESH_INTERVAL_SECS: u64 = talos9s_core::constants::refresh_intervals::FAST;

/// Result of one KubeSpan fetch: whether it is enabled, and its peers
type KubeSpanFetch = (Option<bool>, Result<Vec<KubeSpanPeerStatus>, String>);

/// Sort order for device list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumString, strum::IntoStaticStr)]
pub enum SortBy {
    #[default]
    Traffic, // rx_bytes + tx_bytes descending
    Errors, // errors + dropped descending
}

impl SortBy {
    pub fn label(&self) -> &'static str {
        match self {
            SortBy::Traffic => "TRAFFIC",
            SortBy::Errors => "ERRORS",
        }
    }
}

/// View mode for the network component
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Interfaces, // Main view showing interfaces
    Connections, // Drill-down view showing connections
    KubeSpan,    // KubeSpan peer status
}

impl ViewMode {
    /// Get display label for the tab
    pub fn label(&self) -> &'static str {
        match self {
            ViewMode::Interfaces => "Interfaces",
            ViewMode::Connections => "Connections",
            ViewMode::KubeSpan => "KubeSpan",
        }
    }
}

/// Network view settings that carry over (see `NetworkStatsComponent::settings`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetworkSettings {
    pub sort_by: SortBy,
    pub conn_sort_by: ConnSortBy,
    pub listening_only: bool,
}

/// Sort order for connection list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumString, strum::IntoStaticStr)]
pub enum ConnSortBy {
    #[default]
    State, // Sort by connection state
    Port, // Sort by local port
}

/// Pending action requiring confirmation
#[derive(Debug, Clone)]
pub enum PendingAction {
    /// Restart a service (service_id, service_name)
    RestartService(String, String),
}

/// File viewer overlay type
#[derive(Debug, Clone, PartialEq)]
pub enum FileViewerType {
    /// DNS configuration (/etc/resolv.conf)
    DnsConfig,
    /// Routing table (/proc/net/route)
    RoutingTable,
}

/// File viewer overlay state
#[derive(Debug, Clone)]
pub struct FileViewerOverlay {
    /// Type of file being viewed
    pub viewer_type: FileViewerType,
    /// Title for the overlay
    pub title: String,
    /// Content lines
    pub lines: Vec<String>,
    /// Scroll offset
    pub scroll: usize,
}

/// Command output entry for the output pane
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Command description (e.g., "Restart apid")
    pub command: String,
    /// Output lines from the command
    pub lines: Vec<String>,
    /// When the command was executed
    pub timestamp: Instant,
    /// Whether the command succeeded
    pub success: bool,
}

/// Packet capture state
#[derive(Debug, Clone, Default)]
pub enum CaptureState {
    #[default]
    Idle,
    /// Capturing packets (interface, start time)
    Capturing(String, Instant),
}

/// Packet capture data holder
#[derive(Debug)]
pub struct CaptureData {
    /// State of the capture
    pub state: CaptureState,
    /// Captured pcap data
    pub data: Vec<u8>,
    /// Receiver for capture stream (Option to allow taking ownership)
    pub receiver: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    /// Whether to use BPF filter to exclude API traffic (port 50000)
    /// Recommended to avoid feedback loop on management interface
    pub use_bpf_filter: bool,
}

impl Default for CaptureData {
    fn default() -> Self {
        Self {
            state: CaptureState::default(),
            data: Vec::new(),
            receiver: None,
            use_bpf_filter: true, // ON by default to prevent feedback loops
        }
    }
}

/// Async-loaded network data
#[derive(Debug, Clone, Default)]
pub struct NetworkData {
    /// Current device statistics
    pub devices: Vec<NetDevStats>,
    /// Previous device stats (for rate calculation)
    pub prev_devices: HashMap<String, NetDevStats>,
    /// Calculated rates per device
    pub rates: HashMap<String, NetDevRate>,
    /// Time of last sample
    pub last_sample: Option<Instant>,

    /// Total RX rate (bytes/sec)
    pub total_rx_rate: u64,
    /// Total TX rate (bytes/sec)
    pub total_tx_rate: u64,
    /// Total errors across all devices
    pub total_errors: u64,
    /// Total dropped across all devices
    pub total_dropped: u64,
    /// Errors and drops per second right now, across all devices: the
    /// totals keep incidents long over, these flag what is happening now
    pub errors_per_sec: f64,
    pub dropped_per_sec: f64,

    /// Connection data from netstat
    pub connections: Vec<ConnectionInfo>,
    /// Connection counts by state
    pub conn_counts: ConnectionCounts,
    /// Service health status (port -> is_healthy)
    pub service_health: HashMap<u16, bool>,
    /// Service info from services API (service_id -> ServiceInfo)
    pub services: HashMap<String, ServiceInfo>,

    /// KubeSpan peer status data
    pub kubespan_peers: Vec<KubeSpanPeerStatus>,
    /// KubeSpan enabled status
    pub kubespan_enabled: Option<bool>,
    /// Why the last peer read failed; the peers keep their previous values
    pub kubespan_error: Option<String>,
}

/// Network stats component for viewing node network interfaces
pub struct NetworkStatsComponent {
    /// Node hostname
    hostname: String,
    /// Whether the node is a control plane node; `None` when not known
    controlplane: Option<bool>,
    /// Node address
    address: String,
    /// Talos context of the node's cluster, for talosctl calls
    talos_context: Option<String>,
    /// Custom talosconfig path (from --config)
    config_path: Option<String>,
    /// In-flight KubeSpan fetch: (enabled, peers)
    kubespan_task: Option<tokio::task::JoinHandle<KubeSpanFetch>>,
    /// When the last KubeSpan fetch started
    kubespan_started: Option<std::time::Instant>,

    /// Async state for loaded data
    state: AsyncState<NetworkData>,

    /// Selected device index
    selected: usize,
    /// Table state for rendering
    table_state: TableState,
    /// Where the tables and the breadcrumb were last drawn, for clicks
    hits: NetworkHits,
    /// Current sort order
    sort_by: SortBy,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Current view mode (Interfaces or Connections drill-down)
    view_mode: ViewMode,
    /// Selected interface name when in Connections view
    selected_interface: Option<String>,
    /// Filtered connections for the selected interface
    filtered_connections: Vec<ConnectionInfo>,
    /// Selected connection index (for Connections view)
    conn_selected: usize,
    /// Connection table state
    conn_table_state: TableState,
    /// Connection sort order
    conn_sort_by: ConnSortBy,
    /// Filter to listening only
    listening_only: bool,
    /// Show all connections (bypass interface filter)
    show_all_connections: bool,

    /// Visual selection anchor (for V mode) - stores connection index
    conn_visual: talos9s_core::VisualSelection,
    /// Viewport height for connection table (for page navigation)
    conn_viewport_height: u16,

    /// Pending action requiring confirmation
    pending_action: Option<PendingAction>,
    /// Status message to show (e.g., "Service restarted successfully")
    status_message: Option<(String, Instant)>,
    /// Service ID pending restart (to be executed in update)
    pending_restart_service: Option<String>,

    /// Show command output pane at bottom
    show_output_pane: bool,
    /// Command output history (most recent first)
    command_output: Option<CommandOutput>,

    /// File viewer overlay (DNS config, routing table)
    file_viewer: Option<FileViewerOverlay>,

    /// Packet capture state and data
    capture: CaptureData,

    /// Client for API calls
    client: Option<TalosClient>,

    /// Selected KubeSpan peer index
    kubespan_selected: usize,
    /// KubeSpan table state
    kubespan_table_state: TableState,

    /// In-flight network calls
    tasks: NetworkTasks,
}

impl Default for NetworkStatsComponent {
    fn default() -> Self {
        Self::new("".to_string(), "".to_string())
    }
}

impl NetworkStatsComponent {
    /// Sort orders and the listening-only filter, remembered between visits
    /// and runs
    pub fn settings(&self) -> NetworkSettings {
        NetworkSettings {
            sort_by: self.sort_by,
            conn_sort_by: self.conn_sort_by,
            listening_only: self.listening_only,
        }
    }

    /// The node's role: a worker runs no API server, etcd, scheduler or
    /// controller manager, so their ports aren't shown as down
    pub fn set_controlplane(&mut self, controlplane: Option<bool>) {
        self.controlplane = controlplane;
    }

    pub fn set_settings(&mut self, settings: NetworkSettings) {
        self.sort_by = settings.sort_by;
        self.conn_sort_by = settings.conn_sort_by;
        self.listening_only = settings.listening_only;
    }

    pub fn new(hostname: String, address: String) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        let mut conn_table_state = TableState::default();
        conn_table_state.select(Some(0));

        Self {
            hostname,
            controlplane: None,
            address,
            talos_context: None,
            config_path: None,
            kubespan_task: None,
            kubespan_started: None,
            state: AsyncState::new(),
            selected: 0,
            table_state,
            hits: NetworkHits::default(),
            sort_by: SortBy::Traffic,
            auto_refresh: true,
            view_mode: ViewMode::Interfaces,
            selected_interface: None,
            filtered_connections: Vec::new(),
            conn_selected: 0,
            conn_table_state,
            conn_sort_by: ConnSortBy::State,
            listening_only: false,
            show_all_connections: false,
            conn_visual: Default::default(),
            conn_viewport_height: 20, // Will be updated on draw
            pending_action: None,
            status_message: None,
            pending_restart_service: None,
            show_output_pane: false,
            command_output: None,
            file_viewer: None,
            capture: CaptureData::default(),
            client: None,
            kubespan_selected: 0,
            kubespan_table_state: {
                let mut state = TableState::default();
                state.select(Some(0));
                state
            },
            tasks: NetworkTasks::default(),
        }
    }

    /// Get a reference to the loaded data
    fn data(&self) -> Option<&NetworkData> {
        self.state.data()
    }

    /// Get a mutable reference to the loaded data
    fn data_mut(&mut self) -> Option<&mut NetworkData> {
        self.state.data_mut()
    }

    /// Set the client for API calls
    /// The node's KubeSpan peers as a view of its own (the menu's KubeSpan)
    pub fn new_kubespan(hostname: String, address: String) -> Self {
        let mut component = Self::new(hostname, address);
        component.view_mode = ViewMode::KubeSpan;
        component
    }

    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set the Talos context (and talosconfig path) the node belongs to, so
    /// talosctl calls target the right cluster
    pub fn set_talos_context(&mut self, context: Option<String>, config_path: Option<String>) {
        self.talos_context = context;
        self.config_path = config_path;
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Refresh network data from the node: start any requested action
    /// (service restart, file view, packet capture) and a stats fetch, each in
    /// a spawned task. Returns at once; `update(Tick)` applies the results.
    /// A stats fetch is in flight
    pub fn refreshing(&self) -> bool {
        self.tasks.stats.is_some()
    }

    pub fn refresh(&mut self) {
        let Some(client) = self.client.clone() else {
            self.set_error("No client configured".to_string());
            return;
        };

        self.perform_pending_restart();
        if self.file_viewer_needs_fetch() {
            self.fetch_file_content();
        }
        if self.needs_capture_start() {
            self.start_capture_async();
        }

        self.poll_tasks();
        if self.tasks.stats.is_some() {
            return;
        }

        self.state.start_loading();

        // Ensure we have data to update
        if self.state.data().is_none() {
            self.state.set_data(NetworkData::default());
        }

        // Fetch interface stats, netstat data, and services concurrently
        self.tasks.stats = Some(tokio::spawn(async move {
            tokio::join!(
                with_timeout(client.network_device_stats()),
                with_timeout(client.netstat(NetstatFilter::All)),
                with_timeout(client.services())
            )
        }));
    }

    /// Apply whatever background network calls have finished
    fn poll_tasks(&mut self) {
        if let Some(fetch) = take_finished(&mut self.tasks.stats) {
            self.apply_stats(fetch);
        }
        if let Some((service_id, result)) = take_finished(&mut self.tasks.restart) {
            self.apply_restart(service_id, result);
        }
        if let Some((viewer_type, path, result)) = take_finished(&mut self.tasks.file) {
            self.apply_file_content(viewer_type, path, result);
        }
        if let Some(result) = take_finished(&mut self.tasks.capture) {
            self.apply_capture_start(result);
        }
    }

    /// Apply a finished stats fetch
    fn apply_stats(&mut self, (dev_result, conn_result, svc_result): StatsFetch) {
        // Process interface stats
        match dev_result {
            Ok(stats) => {
                if let Some(node_data) = stats.into_iter().next() {
                    self.update_devices(node_data.devices);
                } else if let Some(data) = self.data_mut() {
                    data.devices.clear();
                    data.rates.clear();
                }
            }
            Err(e) => {
                self.set_error(format!(
                    "Failed to fetch network stats: {} (node: {})",
                    e, self.address
                ));
                return;
            }
        }

        // Process netstat data (don't fail if this errors, connection data is supplementary)
        match conn_result {
            Ok(conn_data) => {
                if let Some(node_conns) = conn_data.into_iter().next() {
                    self.update_connections(node_conns.connections);
                } else if let Some(data) = self.data_mut() {
                    data.connections.clear();
                    data.conn_counts = ConnectionCounts::default();
                }
            }
            Err(_) => {
                // Silently ignore netstat errors and timeouts - interface
                // data still useful
                if let Some(data) = self.data_mut() {
                    data.connections.clear();
                    data.conn_counts = ConnectionCounts::default();
                }
            }
        }

        // Process services data (don't fail if this errors)
        match svc_result {
            Ok(svc_data) => {
                if let Some(node_svcs) = svc_data.into_iter().next() {
                    if let Some(data) = self.data_mut() {
                        data.services = node_svcs
                            .services
                            .into_iter()
                            .map(|s| (s.id.clone(), s))
                            .collect();
                    }
                } else if let Some(data) = self.data_mut() {
                    data.services.clear();
                }
            }
            Err(_) => {
                // Silently ignore service errors
                if let Some(data) = self.data_mut() {
                    data.services.clear();
                }
            }
        }

        // Update service health based on connection data
        self.update_service_health();

        // KubeSpan data via talosctl, in the background
        self.poll_kubespan_data();

        // Reset selection if needed
        let device_count = self.data().map(|d| d.devices.len()).unwrap_or(0);
        if device_count > 0 && self.selected >= device_count {
            self.selected = 0;
        }
        self.table_state.select(Some(self.selected));

        self.state.mark_loaded();
    }
}

/// Areas drawn last frame, in screen coordinates
#[derive(Debug, Default, Clone, Copy)]
struct NetworkHits {
    /// Tables (their top line is the header) and the breadcrumb
    devices: Rect,
    connections: Rect,
    breadcrumb: Rect,
    /// Rows of each table
    device_rows: crate::ui_ext::RowHits,
    connection_rows: crate::ui_ext::RowHits,
    peer_rows: crate::ui_ext::RowHits,
}

fn contains(r: Rect, column: u16, row: u16) -> bool {
    column >= r.x && column < r.right() && row >= r.y && row < r.bottom()
}

/// Index of the fixed-width column at `x` in a table drawn at `area` with a
/// two-column highlight symbol and one column between columns
fn column_at(area: Rect, widths: &[u16], x: u16) -> Option<usize> {
    let mut left = area.x + 2;
    for (i, w) in widths.iter().enumerate() {
        if x >= left && x < left + w {
            return Some(i);
        }
        left += w + 1;
    }
    None
}

impl NetworkStatsComponent {
    /// Clicks: a row selects it, a sortable column header sorts by it,
    /// "Interfaces" in the connections breadcrumb goes back. A second click
    /// opens the row (the app sends Enter).
    fn click(&mut self, column: u16, row: u16) {
        match self.view_mode {
            ViewMode::Interfaces => {
                let area = self.hits.devices;
                if !contains(area, column, row) {
                    return;
                }
                if row == area.y {
                    // INTERFACE, RX RATE, TX RATE, RX ERR
                    match column_at(area, &[14, 12, 12, 8], column) {
                        Some(1) => self.sort_by = SortBy::Traffic,
                        Some(3) => self.sort_by = SortBy::Errors,
                        _ => return,
                    }
                    self.sort_devices();
                    return;
                }
                if let Some(i) = self.hits.device_rows.row_at(column, row) {
                    self.selected = i;
                    self.table_state.select(Some(i));
                }
            }
            ViewMode::Connections => {
                let crumb = self.hits.breadcrumb;
                if contains(crumb, column, row) && column < crumb.x + "Interfaces".len() as u16 {
                    self.exit_connections_view();
                    return;
                }
                let area = self.hits.connections;
                if !contains(area, column, row) {
                    return;
                }
                if row == area.y {
                    // PROTO, LOCAL, REMOTE, STATE
                    let sort = match column_at(area, &[6, 22, 24, 12], column) {
                        Some(1) => ConnSortBy::Port,
                        Some(3) => ConnSortBy::State,
                        _ => return,
                    };
                    self.conn_sort_by = sort;
                    self.conn_selected = 0;
                    self.conn_table_state.select(Some(0));
                    self.conn_visual.clear();
                    return;
                }
                if let Some(i) = self.hits.connection_rows.row_at(column, row) {
                    self.conn_selected = i;
                    self.conn_table_state.select(Some(i));
                    self.conn_visual.clear();
                }
            }
            ViewMode::KubeSpan => {
                if let Some(i) = self.hits.peer_rows.row_at(column, row) {
                    self.kubespan_selected = i;
                    self.kubespan_table_state.select(Some(i));
                }
            }
        }
    }
}

impl Component for NetworkStatsComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.click(mouse.column, mouse.row);
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Cyan));
        if self.file_viewer.is_some() {
            return Some(Line::from(vec![
                key("[j/k]"),
                Span::raw(" scroll  "),
                key("[g/G]"),
                Span::raw(" top/bottom  "),
                key("[Esc/q]"),
                Span::raw(" close"),
            ]));
        }
        if self.pending_action.is_some() {
            return Some(Line::from(vec![
                key("[y]"),
                Span::raw(" confirm  "),
                key("[n/Esc]"),
                Span::raw(" cancel"),
            ]));
        }
        let mut line = match self.view_mode {
            ViewMode::Interfaces => self.interfaces_key_hints(),
            ViewMode::Connections => self.connections_key_hints(),
            ViewMode::KubeSpan => Line::from(vec![
                key("[j/k]"),
                Span::raw(" navigate  "),
                key("[r]"),
                Span::raw(" refresh"),
            ]),
        };
        if self.show_output_pane {
            line.spans
                .extend([Span::raw("  "), key("[Esc]"), Span::raw(" close output")]);
        }
        Some(line)
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        // If file viewer is shown, handle its keys
        if self.file_viewer.is_some() {
            return self.handle_file_viewer_key(key);
        }

        // If output pane is shown, Esc closes it
        if self.show_output_pane && matches!(key.code, KeyCode::Esc) {
            self.show_output_pane = false;
            return Ok(None);
        }

        // If there's a pending confirmation, handle that first
        if self.pending_action.is_some() {
            return self.handle_confirmation_key(key);
        }

        match self.view_mode {
            ViewMode::Interfaces => self.handle_interfaces_key(key),
            ViewMode::Connections => self.handle_connections_key(key),
            ViewMode::KubeSpan => self.handle_kubespan_key(key),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Clear old status messages (after 3 seconds)
            if let Some((_, time)) = &self.status_message
                && time.elapsed() > std::time::Duration::from_secs(3)
            {
                self.status_message = None;
            }

            // Start a requested file view or capture without waiting for the
            // next refresh, and apply finished background calls
            if self.file_viewer_needs_fetch() {
                self.fetch_file_content();
            }
            if self.needs_capture_start() {
                self.start_capture_async();
            }
            self.poll_tasks();

            // Poll packet capture for new data
            if self.is_capturing() {
                self.poll_capture();
            }

            // Check for auto-refresh
            if self.auto_refresh
                && !self.state.is_loading()
                && let Some(last) = self.state.last_refresh()
            {
                let interval = std::time::Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
                if last.elapsed() >= interval {
                    return Ok(Some(Action::Refresh));
                }
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        if self.state.is_loading() && self.data().is_none() {
            let loading = Paragraph::new("Loading network stats...")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, area);
            return Ok(());
        }

        if let Some(err) = self.state.error() {
            let error =
                Paragraph::new(format!("Error: {}", err)).style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return Ok(());
        }

        // Split area for output pane if shown
        let (main_area, output_area) = if self.show_output_pane {
            let chunks = Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(10), // Output pane height
            ])
            .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };

        match self.view_mode {
            ViewMode::Interfaces => self.draw_interfaces_view(frame, main_area),
            ViewMode::Connections => self.draw_connections_view(frame, main_area),
            ViewMode::KubeSpan => self.draw_kubespan_view(frame, main_area),
        }

        // Draw output pane if shown
        if let Some(output_rect) = output_area {
            self.draw_output_pane(frame, output_rect);
        }

        // Draw confirmation dialog overlay if pending
        if let Some(ref action) = self.pending_action {
            self.draw_confirmation_dialog(frame, area, action);
        }

        // Draw status message overlay if present
        if let Some((ref msg, _)) = self.status_message {
            self.draw_status_message(frame, area, msg);
        }

        // Draw file viewer overlay if open
        if self.file_viewer.is_some() {
            self.draw_file_viewer(frame, area);
        }

        Ok(())
    }
}

#[cfg(test)]
mod click_tests {
    use super::*;

    /// Column hit-testing for the sortable headers
    #[test]
    fn columns_and_rows_under_the_mouse() {
        let area = Rect::new(31, 8, 100, 20);
        // The highlight symbol takes two columns, columns are one apart
        let widths = [6, 22, 24, 12];
        assert_eq!(column_at(area, &widths, 31), None);
        assert_eq!(column_at(area, &widths, 33), Some(0));
        assert_eq!(column_at(area, &widths, 40), Some(1));
        assert_eq!(column_at(area, &widths, 61), Some(1));
        assert_eq!(column_at(area, &widths, 62), None);
        assert_eq!(column_at(area, &widths, 63), Some(2));
        assert_eq!(column_at(area, &widths, 88), Some(3));
        assert_eq!(column_at(area, &widths, 100), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_show_only_the_kubelet_port() {
        let names = |cp| -> Vec<&str> { service_ports(cp).iter().map(|(n, _)| *n).collect() };
        assert_eq!(names(Some(false)), ["Kubelet"]);
        assert_eq!(names(Some(true)).len(), 5);
        assert_eq!(names(None).len(), 5);
    }

    #[test]
    fn busy_healthy_node_has_no_connection_warning() {
        // Seen on a healthy control plane node
        let busy = ConnectionCounts {
            established: 1375,
            listen: 67,
            time_wait: 839,
            close_wait: 2,
            syn_sent: 1,
            other: 345,
        };
        assert!(!conn_warnings(&busy));
        assert!(conn_warnings(&ConnectionCounts {
            close_wait: 150,
            ..busy.clone()
        }));
        assert!(conn_warnings(&ConnectionCounts {
            time_wait: 25_000,
            ..busy
        }));
    }
}
