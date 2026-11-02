//! Multi-service logs component - Stern-style interleaved log viewer
//!
//! Streams the chosen services from one node, or from several nodes at once
//! (a cluster or control plane / workers group), merged by timestamp with a
//! node column. The kernel log (dmesg) is offered as the pseudo-service
//! `kernel`. For watching only: nothing here decides health.

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};
use std::collections::{HashMap, HashSet};
use talos9s_core::AsyncState;
use talos9s_core::constants::MAX_LOG_ENTRIES;
use talos9s_core::convert::to_u16;
use talos9s_core::logparse::{self, LogLevel, bottom_start, match_ranges};

/// Maximum lines to process per tick during streaming
/// Higher = more responsive but could block UI if too high
const MAX_LINES_PER_TICK: usize = 500;

/// Pseudo-service for the kernel log (dmesg)
pub const KERNEL_SERVICE: &str = "kernel";

/// Widest node name shown in the node column
const NODE_COLUMN_MAX: usize = 14;

/// A node to stream logs from
#[derive(Debug, Clone, PartialEq)]
pub struct LogNode {
    pub name: String,
    /// Talos API address
    pub ip: String,
    /// Services it runs; `None` when not known yet (every service is tried)
    pub services: Option<Vec<String>>,
}

/// Where one (node, service) stream is
#[derive(Debug, Clone, PartialEq)]
enum StreamStatus {
    /// Requested, nothing received yet. Talos only answers a log request
    /// with its first line, so a quiet service waits here too.
    Waiting,
    Streaming,
    Failed(String),
    /// The node closed the stream
    Ended,
}

/// What a stream task reports
#[derive(Debug)]
enum StreamEvent {
    Line {
        node: usize,
        service: String,
        line: String,
    },
    Status {
        node: usize,
        service: String,
        status: StreamStatus,
    },
}

/// A node in the nodes pane
#[derive(Debug, Clone)]
struct NodeState {
    name: String,
    ip: String,
    services: Option<HashSet<String>>,
    color: Color,
    active: bool,
    entry_count: usize,
}

impl NodeState {
    /// Whether the node runs `service` (unknown counts as yes)
    fn runs(&self, service: &str) -> bool {
        service == KERNEL_SERVICE || self.services.as_ref().is_none_or(|s| s.contains(service))
    }
}

/// Color palette for services (deterministic assignment)
const SERVICE_COLORS: &[Color] = &[
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
];

/// The palette color for the `i`th service or node, cycling
fn service_color(i: usize) -> Color {
    SERVICE_COLORS
        .get(i % SERVICE_COLORS.len())
        .copied()
        .unwrap_or(Color::White)
}

/// Color of a level's badge
fn level_color(level: LogLevel) -> Color {
    match level {
        LogLevel::Error => Color::Red,
        LogLevel::Warn => Color::Yellow,
        LogLevel::Info => Color::Green,
        LogLevel::Debug => Color::DarkGray,
        LogLevel::Unknown => Color::White,
    }
}

/// A log entry from any service
#[derive(Debug, Clone)]
pub(crate) struct MultiLogEntry {
    /// Index of the node this entry came from
    node: usize,
    /// Service this entry came from
    service_id: String,
    /// Assigned color for this service
    service_color: Color,
    /// Parsed timestamp (display format)
    timestamp: String,
    /// Raw timestamp for sorting (if parseable)
    timestamp_sort: i64,
    /// Log level
    level: LogLevel,
    /// Log message
    message: String,
    /// Pre-computed lowercase for search
    search_text: String,
}

/// Service state for sidebar
#[derive(Debug, Clone)]
struct ServiceState {
    /// Service ID (e.g., "kubelet")
    id: String,
    /// Display color
    color: Color,
    /// Whether this service is active (showing logs)
    active: bool,
    /// Number of entries from this service
    entry_count: usize,
}

/// Search mode
#[derive(Debug, Clone, PartialEq)]
enum SearchMode {
    Off,
    Input,
    Active,
}

/// Which floating pane is open
#[derive(Debug, Clone, Copy, PartialEq)]
enum FloatingPane {
    None,
    Services,
    Levels,
    Nodes,
}

/// Which levels are shown, in panel order: Error, Warn, Info, Debug, Other.
/// Kept by the app so the selection survives leaving and reopening the view.
pub type LevelFilter = [bool; 5];

/// Show every level
pub const ALL_LEVELS: LevelFilter = [true; 5];

/// What a level hotkey does
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelHotkey {
    /// `0`: show every level
    All,
    /// `1`-`5`: show only this level
    Only(usize),
    /// Shift+`1`-`5`: add/remove this level
    Toggle(usize),
}

impl LevelHotkey {
    /// Map a key to a level hotkey. Shift+digit arrives as the shifted symbol
    /// on most terminals (US layout `!@#$%`), or as the digit with SHIFT set
    /// when the terminal reports modifiers (e.g. the kitty keyboard protocol).
    fn from_key(key: &KeyEvent) -> Option<Self> {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('0') if !shift => Some(Self::All),
            KeyCode::Char(c @ '1'..='5') => {
                let idx = c as usize - '1' as usize;
                Some(if shift {
                    Self::Toggle(idx)
                } else {
                    Self::Only(idx)
                })
            }
            KeyCode::Char(c) => ['!', '@', '#', '$', '%']
                .iter()
                .position(|&s| s == c)
                .map(Self::Toggle),
            _ => None,
        }
    }
}

/// Level filter state
#[derive(Debug, Clone)]
struct LevelState {
    level: LogLevel,
    active: bool,
    entry_count: usize,
}

/// Data for multi-logs component managed by AsyncState
#[derive(Debug, Clone, Default)]
pub(crate) struct MultiLogsData {
    /// All log entries (sorted by timestamp)
    pub(crate) entries: Vec<MultiLogEntry>,
    /// Filtered entries (indices into entries vec, only active services/levels)
    pub(crate) visible_indices: Vec<usize>,
}

/// Multi-service logs component
pub struct MultiLogsComponent {
    /// What is viewed: a node address, or a cluster / group name
    title: String,
    /// Node role (controlplane/worker), or the node count for a group
    subtitle: String,

    /// Nodes streamed from, in the nodes pane
    nodes: Vec<NodeState>,
    /// Selected node in the nodes pane
    selected_node: usize,
    nodes_state: ListState,

    /// All services available
    services: Vec<ServiceState>,
    /// Selected service in sidebar
    selected_service: usize,
    /// Sidebar list state
    sidebar_state: ListState,

    /// Level filters
    levels: Vec<LevelState>,
    /// Selected level in levels pane
    selected_level: usize,
    /// Levels list state
    levels_state: ListState,
    /// Entries of the open floating panel by line, for clicks
    panel_lines: crate::ui_ext::LineHits<usize>,

    /// Async state for log data (entries and visible indices)
    state: AsyncState<MultiLogsData>,

    /// Which floating pane is open
    floating_pane: FloatingPane,
    /// Scroll position in logs
    scroll: u16,
    /// Last known viewport height (for half-page scroll)
    viewport_height: u16,
    /// Following mode (auto-scroll to bottom)
    following: bool,

    /// Search mode
    search_mode: SearchMode,
    /// Search query
    search_query: String,
    /// Set of matching entry indices (into visible_indices)
    match_set: HashSet<usize>,
    /// Ordered matches for n/N navigation
    match_order: Vec<usize>,
    /// Current match index
    current_match: usize,

    /// Talos client for streaming
    client: Option<talos_rs::TalosClient>,
    /// Tail lines setting
    tail_lines: i32,
    /// Whether streaming is active
    streaming: bool,
    /// Channel the stream tasks report lines and status on
    stream_rx: Option<tokio::sync::mpsc::UnboundedReceiver<StreamEvent>>,
    stream_tx: Option<tokio::sync::mpsc::UnboundedSender<StreamEvent>>,
    /// Running stream task and its status, per (node index, service)
    streams: HashMap<(usize, String), (tokio::task::JoinHandle<()>, StreamStatus)>,
    /// Streams started before: a restart asks only for new lines, so the
    /// tail already shown isn't added again
    started: HashSet<(usize, String)>,
    /// Animation frame for pulsing indicator
    pulse_frame: u8,
    /// Whether to wrap long lines
    wrap: bool,
    /// Visual selection anchor (for V mode) - stores visible_index
    visual: talos9s_core::VisualSelection,
    /// Current cursor position in visible_indices (always tracked, moves with navigation)
    cursor: usize,
}

impl MultiLogsComponent {
    /// Create a new multi-logs component
    /// - active_services: services to initially show logs for
    /// - all_services: all available services (inactive ones shown greyed out in sidebar)
    pub fn new(
        title: String,
        subtitle: String,
        nodes: Vec<LogNode>,
        active_services: &[String],
        all_services: Vec<String>,
    ) -> Self {
        let nodes: Vec<NodeState> = nodes
            .into_iter()
            .enumerate()
            .map(|(i, n)| NodeState {
                name: n.name,
                ip: n.ip,
                services: n.services.map(|s| s.into_iter().collect()),
                color: service_color(i + 3),
                active: true,
                entry_count: 0,
            })
            .collect();
        let mut nodes_state = ListState::default();
        nodes_state.select(Some(0));

        // Build a set of active service IDs for quick lookup
        let active_set: std::collections::HashSet<&str> = active_services
            .iter()
            .map(std::string::String::as_str)
            .collect();

        // Assign colors to services deterministically
        let services: Vec<ServiceState> = all_services
            .into_iter()
            .enumerate()
            .map(|(i, id)| {
                let is_active = active_set.contains(id.as_str());
                ServiceState {
                    color: service_color(i),
                    active: is_active,
                    id,
                    entry_count: 0,
                }
            })
            .collect();

        let mut sidebar_state = ListState::default();
        sidebar_state.select(Some(0));

        // Initialize level filters (all active by default)
        let levels = vec![
            LevelState {
                level: LogLevel::Error,
                active: true,
                entry_count: 0,
            },
            LevelState {
                level: LogLevel::Warn,
                active: true,
                entry_count: 0,
            },
            LevelState {
                level: LogLevel::Info,
                active: true,
                entry_count: 0,
            },
            LevelState {
                level: LogLevel::Debug,
                active: true,
                entry_count: 0,
            },
            LevelState {
                level: LogLevel::Unknown,
                active: true,
                entry_count: 0,
            },
        ];
        let mut levels_state = ListState::default();
        levels_state.select(Some(0));

        Self {
            title,
            subtitle,
            nodes,
            selected_node: 0,
            nodes_state,
            services,
            selected_service: 0,
            sidebar_state,
            levels,
            selected_level: 0,
            levels_state,
            panel_lines: Default::default(),
            state: {
                let mut state = AsyncState::new();
                state.start_loading();
                state.set_data(MultiLogsData::default());
                state
            },
            floating_pane: FloatingPane::None, // Start with pane closed
            scroll: 0,
            viewport_height: 20, // Will be updated on first draw
            following: true,
            search_mode: SearchMode::Off,
            search_query: String::new(),
            match_set: HashSet::new(),
            match_order: Vec::new(),
            current_match: 0,
            // Streaming fields
            client: None,
            tail_lines: 500,
            streaming: false,
            stream_rx: None,
            stream_tx: None,
            streams: HashMap::new(),
            started: HashSet::new(),
            pulse_frame: 0,
            wrap: false,
            visual: Default::default(),
            cursor: 0,
        }
    }

    /// More than one node: show the node column and the nodes pane
    fn multi_node(&self) -> bool {
        self.nodes.len() > 1
    }

    /// Width of the node column (0 for a single node)
    fn node_column_width(&self) -> usize {
        if !self.multi_node() {
            return 0;
        }
        self.nodes
            .iter()
            .map(|n| n.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(NODE_COLUMN_MAX)
    }

    /// Streams in each state: (streaming, waiting, failed)
    fn stream_counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for (_, status) in self.streams.values() {
            match status {
                StreamStatus::Streaming => counts.0 += 1,
                StreamStatus::Waiting => counts.1 += 1,
                StreamStatus::Failed(_) => counts.2 += 1,
                StreamStatus::Ended => {}
            }
        }
        counts
    }

    /// Failed streams of a node, as `service: error`
    fn node_failures(&self, node: usize) -> Vec<String> {
        let mut failures: Vec<String> = self
            .streams
            .iter()
            .filter(|((n, _), _)| *n == node)
            .filter_map(|((_, service), (_, status))| match status {
                StreamStatus::Failed(e) => Some(format!("{service}: {e}")),
                _ => None,
            })
            .collect();
        failures.sort();
        failures
    }

    /// Set the Talos client for streaming
    pub fn set_client(&mut self, client: talos_rs::TalosClient, tail_lines: i32) {
        self.client = Some(client);
        self.tail_lines = tail_lines;
    }

    /// Get reference to data (if loaded)
    fn data(&self) -> Option<&MultiLogsData> {
        self.state.data()
    }

    /// Get mutable reference to data (if loaded)
    fn data_mut(&mut self) -> Option<&mut MultiLogsData> {
        self.state.data_mut()
    }

    /// Stream every active service from every active node that runs it.
    /// Streams already running are kept; only missing ones are started.
    pub fn start_streaming(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = match &self.stream_tx {
            Some(tx) if self.stream_rx.is_some() => tx.clone(),
            _ => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                self.stream_tx = Some(tx.clone());
                self.stream_rx = Some(rx);
                tx
            }
        };
        self.streaming = true;

        let mut wanted = Vec::new();
        for (ni, node) in self.nodes.iter().enumerate().filter(|(_, n)| n.active) {
            for service in self.services.iter().filter(|s| s.active) {
                let key = (ni, service.id.clone());
                if node.runs(&service.id) && !self.streams.contains_key(&key) {
                    wanted.push((key, node.ip.clone()));
                }
            }
        }
        for (key, ip) in wanted {
            // A restart only asks for new lines; the old ones are shown
            let tail = if self.started.contains(&key) {
                0
            } else {
                self.tail_lines
            };
            let task = tokio::spawn(stream_task(
                client.with_node(&ip),
                key.0,
                key.1.clone(),
                tail,
                tx.clone(),
            ));
            self.started.insert(key.clone());
            self.streams.insert(key, (task, StreamStatus::Waiting));
        }
    }

    /// Stop all log streams
    pub fn stop_streaming(&mut self) {
        self.streaming = false;
        for (task, _) in self.streams.values() {
            task.abort();
        }
        self.streams.clear();
        self.stream_tx = None;
        self.stream_rx = None;
    }

    /// Process incoming streamed log entries (call on tick)
    fn process_stream_entries(&mut self) {
        // First, drain the channel into a local vec to avoid borrow issues
        let events: Vec<StreamEvent> = {
            let rx = match &mut self.stream_rx {
                Some(r) => r,
                None => return,
            };

            // Up to MAX_LINES_PER_TICK a tick so drawing keeps up; the rest
            // waits in the channel for the next tick. Dropping it lost the
            // initial tails and status events (a stream stuck "connecting").
            let mut lines = Vec::with_capacity(MAX_LINES_PER_TICK);
            while lines.len() < MAX_LINES_PER_TICK {
                match rx.try_recv() {
                    Ok(item) => lines.push(item),
                    Err(_) => break,
                }
            }
            lines
        };

        if events.is_empty() {
            return;
        }
        // Something answered: no longer "Loading"
        if self.state.is_loading() {
            self.state.mark_loaded();
        }

        // Now parse entries (can access self freely)
        let mut new_entries: Vec<MultiLogEntry> = Vec::with_capacity(events.len());
        for event in events {
            match event {
                StreamEvent::Line {
                    node,
                    service,
                    line,
                } => {
                    let color = self.get_service_color(&service);
                    let mut entry = Self::parse_line(&line, &service, color);
                    entry.node = node;
                    if let Some(n) = self.nodes.get(node)
                        && self.nodes.len() > 1
                    {
                        entry.search_text =
                            format!("{} {}", n.name.to_lowercase(), entry.search_text);
                    }
                    new_entries.push(entry);
                }
                StreamEvent::Status {
                    node,
                    service,
                    status,
                } => {
                    if let Some((_, current)) = self.streams.get_mut(&(node, service)) {
                        *current = status;
                    }
                }
            }
        }
        if new_entries.is_empty() {
            return;
        }

        // Get data and add new entries
        let Some(data) = self.data_mut() else { return };

        // Add new entries
        data.entries.extend(new_entries);

        // Re-sort (could optimize with insertion sort for streaming)
        data.entries.sort_by_key(|e| e.timestamp_sort);

        // Enforce max entries (ring buffer) - this is the primary memory bound
        if data.entries.len() > MAX_LOG_ENTRIES {
            let excess = data.entries.len() - MAX_LOG_ENTRIES;
            data.entries.drain(0..excess);
            // Shrink capacity periodically to release memory
            if data.entries.capacity() > MAX_LOG_ENTRIES * 2 {
                data.entries.shrink_to(MAX_LOG_ENTRIES + 1000);
            }
        }

        // Update counts
        self.update_counts();

        // Rebuild visible indices
        self.rebuild_visible_indices();

        // Auto-scroll if following
        if self.following {
            self.scroll_to_bottom();
        }
    }

    /// Update service and level counts
    /// Service counts show total entries for each service
    /// Level counts show entries filtered by active services (so you see relevant breakdown)
    fn update_counts(&mut self) {
        // Compute counts from entries, then release borrow before mutating services/levels
        let (service_counts, level_counts, node_counts): (Vec<_>, Vec<_>, Vec<_>) = {
            let Some(data) = self.data() else { return };
            let entries = &data.entries;

            // Service counts: total entries for each service (not filtered)
            let service_counts: Vec<_> = self
                .services
                .iter()
                .map(|s| entries.iter().filter(|e| e.service_id == s.id).count())
                .collect();

            // Level counts: filtered by active services
            let active_services: HashSet<&str> = self
                .services
                .iter()
                .filter(|s| s.active)
                .map(|s| s.id.as_str())
                .collect();

            let level_counts: Vec<_> = self
                .levels
                .iter()
                .map(|l| {
                    entries
                        .iter()
                        .filter(|e| {
                            active_services.contains(e.service_id.as_str()) && e.level == l.level
                        })
                        .count()
                })
                .collect();

            let node_counts: Vec<_> = (0..self.nodes.len())
                .map(|ni| entries.iter().filter(|e| e.node == ni).count())
                .collect();

            (service_counts, level_counts, node_counts)
        };

        // Now apply the counts
        for (service, count) in self.services.iter_mut().zip(service_counts) {
            service.entry_count = count;
        }
        for (level, count) in self.levels.iter_mut().zip(level_counts) {
            level.entry_count = count;
        }
        for (node, count) in self.nodes.iter_mut().zip(node_counts) {
            node.entry_count = count;
        }
    }

    /// Get color for a service by ID
    fn get_service_color(&self, service_id: &str) -> Color {
        self.services
            .iter()
            .find(|s| s.id == service_id)
            .map(|s| s.color)
            .unwrap_or(Color::White)
    }

    /// Parse a log line into a MultiLogEntry
    fn parse_line(line: &str, service_id: &str, color: Color) -> MultiLogEntry {
        let parsed = logparse::parse(line, service_id == KERNEL_SERVICE);
        MultiLogEntry {
            node: 0,
            service_id: service_id.to_string(),
            service_color: color,
            timestamp: parsed.timestamp,
            timestamp_sort: parsed.timestamp_sort,
            level: parsed.level,
            message: parsed.message,
            search_text: line.trim().to_lowercase(),
        }
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Rebuild visible indices based on active services
    fn rebuild_visible_indices(&mut self) {
        // Clone to owned strings to avoid borrow issues
        let active_services: HashSet<String> = self
            .services
            .iter()
            .filter(|s| s.active)
            .map(|s| s.id.clone())
            .collect();

        let active_levels: HashSet<LogLevel> = self
            .levels
            .iter()
            .filter(|l| l.active)
            .map(|l| l.level)
            .collect();

        let active_nodes: Vec<bool> = self.nodes.iter().map(|n| n.active).collect();

        let total = {
            let Some(data) = self.data_mut() else { return };

            data.visible_indices = data
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    active_services.contains(&e.service_id)
                        && active_levels.contains(&e.level)
                        && active_nodes.get(e.node).copied().unwrap_or(true)
                })
                .map(|(i, _)| i)
                .collect();

            data.visible_indices.len()
        };

        // Clamp scroll to valid range (max is where last entry is at viewport bottom)
        let viewport = self.viewport_height as usize;
        let max_scroll = to_u16(total.saturating_sub(viewport));
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        // Update search if active
        if self.search_mode == SearchMode::Active {
            self.update_matches();
        }
    }

    /// Toggle service active state
    fn toggle_service(&mut self, index: usize) {
        let should_restart_streaming = if let Some(service) = self.services.get_mut(index) {
            let was_active = service.active;
            service.active = !service.active;
            // Check if we need to restart streaming (enabled a service while streaming)
            !was_active && service.active && self.streaming
        } else {
            false
        };

        self.update_counts(); // Level counts depend on active services
        self.rebuild_visible_indices();

        // Restart streaming to include the new service
        if should_restart_streaming {
            self.start_streaming();
        }
    }

    /// Set all services active
    fn activate_all(&mut self) {
        for service in &mut self.services {
            service.active = true;
        }
        self.update_counts();
        self.rebuild_visible_indices();
        if self.streaming {
            self.start_streaming();
        }
    }

    /// Set all services inactive
    fn deactivate_all(&mut self) {
        for service in &mut self.services {
            service.active = false;
        }
        self.update_counts();
        self.rebuild_visible_indices();
    }

    /// Count active services
    fn active_count(&self) -> usize {
        self.services.iter().filter(|s| s.active).count()
    }

    /// Show or hide a node's lines; showing one starts its streams
    fn toggle_node(&mut self, index: usize) {
        let Some(node) = self.nodes.get_mut(index) else {
            return;
        };
        node.active = !node.active;
        let start = node.active && self.streaming;
        self.rebuild_visible_indices();
        if start {
            self.start_streaming();
        }
    }

    /// Show or hide every node
    fn set_all_nodes(&mut self, active: bool) {
        for node in &mut self.nodes {
            node.active = active;
        }
        self.rebuild_visible_indices();
        if active && self.streaming {
            self.start_streaming();
        }
    }

    fn active_node_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.active).count()
    }

    /// Toggle level active state
    fn toggle_level(&mut self, index: usize) {
        if let Some(level) = self.levels.get_mut(index) {
            level.active = !level.active;
            self.rebuild_visible_indices();
        }
    }

    /// Set all levels active
    fn activate_all_levels(&mut self) {
        for level in &mut self.levels {
            level.active = true;
        }
        self.rebuild_visible_indices();
    }

    /// Set all levels inactive
    fn deactivate_all_levels(&mut self) {
        for level in &mut self.levels {
            level.active = false;
        }
        self.rebuild_visible_indices();
    }

    /// Show only the level at `index`
    fn show_only_level(&mut self, index: usize) {
        for (i, level) in self.levels.iter_mut().enumerate() {
            level.active = i == index;
        }
        self.rebuild_visible_indices();
    }

    /// Which levels are currently shown
    pub fn level_filter(&self) -> LevelFilter {
        let mut filter = [false; 5];
        for (active, level) in filter.iter_mut().zip(&self.levels) {
            *active = level.active;
        }
        filter
    }

    /// Restore a previously chosen level selection
    pub fn set_level_filter(&mut self, filter: LevelFilter) {
        for (level, active) in self.levels.iter_mut().zip(filter) {
            level.active = active;
        }
        self.rebuild_visible_indices();
    }

    /// Count active levels
    fn active_level_count(&self) -> usize {
        self.levels.iter().filter(|l| l.active).count()
    }

    /// Scroll to bottom and enable following
    /// Sets scroll so the last entries fill the viewport from the bottom
    /// Also moves cursor to the last entry
    fn scroll_to_bottom(&mut self) {
        let total = self.data().map(|d| d.visible_indices.len()).unwrap_or(0);
        let viewport = self.viewport_height as usize;
        // Position scroll so last entry is at bottom of viewport
        self.scroll = to_u16(total.saturating_sub(viewport));
        // Move cursor to last entry
        self.cursor = total.saturating_sub(1);
        self.following = true;
    }

    /// Check if a visible index is the cursor position
    fn is_cursor(&self, visible_idx: usize) -> bool {
        visible_idx == self.cursor
    }

    /// Format a log entry as a string
    fn format_entry(&self, entry: &MultiLogEntry) -> String {
        let ts = if entry.timestamp.is_empty() {
            "NO TIME"
        } else {
            &entry.timestamp
        };
        let node = match self.nodes.get(entry.node) {
            Some(n) if self.multi_node() => format!("{} ", n.name),
            _ => String::new(),
        };
        format!(
            "{} {}{} {} {}",
            ts,
            node,
            entry.service_id,
            entry.level.badge(),
            entry.message
        )
    }

    /// Yank (copy) the selected lines, or the current one
    fn yank_selection(&self) -> (bool, usize) {
        let Some(data) = self.data() else {
            return (false, 0);
        };
        let lines: Vec<String> = (self.visual.rows(self.cursor))
            .filter_map(|vi| {
                data.visible_indices
                    .get(vi)
                    .and_then(|&i| data.entries.get(i))
                    .map(|e| self.format_entry(e))
            })
            .collect();
        crate::clipboard::copy_lines(&lines)
    }

    /// Update search matches
    fn update_matches(&mut self) {
        self.match_set.clear();
        self.match_order.clear();
        self.current_match = 0;

        if self.search_query.is_empty() {
            return;
        }

        // Collect matches first, then release borrow before mutating
        let matches: Vec<usize> = {
            let Some(data) = self.data() else { return };
            let query_lower = self.search_query.to_lowercase();

            data.visible_indices
                .iter()
                .enumerate()
                .filter(|&(_, &entry_idx)| {
                    data.entries
                        .get(entry_idx)
                        .is_some_and(|e| e.search_text.contains(&query_lower))
                })
                .map(|(vi, _)| vi)
                .collect()
        };

        // Now apply matches
        for vi in matches {
            self.match_set.insert(vi);
            self.match_order.push(vi);
        }

        if let Some(&first) = self.match_order.first() {
            self.scroll = to_u16(first);
        }
    }

    /// Go to next match
    fn next_match(&mut self) {
        if self.match_order.is_empty() {
            return;
        }
        self.current_match = (self.current_match + 1) % self.match_order.len();
        if let Some(&vi) = self.match_order.get(self.current_match) {
            self.scroll = to_u16(vi);
        }
        self.following = false;
    }

    /// Go to previous match
    fn prev_match(&mut self) {
        if self.match_order.is_empty() {
            return;
        }
        self.current_match = if self.current_match == 0 {
            self.match_order.len() - 1
        } else {
            self.current_match - 1
        };
        if let Some(&vi) = self.match_order.get(self.current_match) {
            self.scroll = to_u16(vi);
        }
        self.following = false;
    }

    /// Clear search
    fn clear_search(&mut self) {
        self.search_mode = SearchMode::Off;
        self.search_query.clear();
        self.match_set.clear();
        self.match_order.clear();
        self.current_match = 0;
    }

    /// Check if a visible index is current match
    fn is_current_match(&self, visible_idx: usize) -> bool {
        if self.match_order.is_empty() {
            return false;
        }
        self.match_order.get(self.current_match) == Some(&visible_idx)
    }

    /// Check if a visible index matches search
    fn entry_matches(&self, visible_idx: usize) -> bool {
        self.match_set.contains(&visible_idx)
    }

    /// Render message with search highlighting
    // match_ranges returns ranges on char boundaries of `message`
    #[allow(clippy::string_slice)]
    fn render_message_with_highlight(&self, message: &str, is_current: bool) -> Vec<Span<'static>> {
        if self.search_query.is_empty() {
            return vec![Span::raw(message.to_string())];
        }

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut last_end = 0;
        let style = if is_current {
            Style::default().bg(Color::Yellow).fg(Color::Black).bold()
        } else {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        };
        for range in match_ranges(message, &self.search_query) {
            if range.start > last_end {
                spans.push(Span::raw(message[last_end..range.start].to_string()));
            }
            last_end = range.end;
            spans.push(Span::styled(message[range].to_string(), style));
        }
        if last_end < message.len() {
            spans.push(Span::raw(message[last_end..].to_string()));
        }

        if spans.is_empty() {
            vec![Span::raw(message.to_string())]
        } else {
            spans
        }
    }

    /// Draw the floating services panel
    fn draw_services_panel(&mut self, frame: &mut Frame, area: Rect) {
        // Filter services by search query if searching
        let query_lower = self.search_query.to_lowercase();
        let filtered_services: Vec<(usize, &ServiceState)> =
            if self.search_mode != SearchMode::Off && !self.search_query.is_empty() {
                self.services
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.id.to_lowercase().contains(&query_lower))
                    .collect()
            } else {
                self.services.iter().enumerate().collect()
            };

        if filtered_services.is_empty() {
            return; // Don't show panel if no services match
        }

        // Calculate panel size based on filtered service names
        let max_name_len = filtered_services
            .iter()
            .map(|(_, s)| s.id.len())
            .max()
            .unwrap_or(8);
        let panel_width = to_u16(
            (max_name_len + 4)
                .max(16)
                .min((area.width as usize).saturating_sub(4)),
        );
        let panel_height = to_u16((filtered_services.len() + 2).min(area.height as usize - 2));

        // Position in top-left with small margin
        let panel_area = Rect::new(area.x + 1, area.y, panel_width, panel_height);

        // Clear the background
        frame.render_widget(ratatui::widgets::Clear, panel_area);

        let items: Vec<ListItem> = filtered_services
            .iter()
            .map(|(_, s)| {
                let indicator = if s.active { "●" } else { "○" };
                let style = Style::default().fg(s.color);

                ListItem::new(Line::from(vec![
                    Span::styled(indicator, style),
                    Span::raw(" "),
                    Span::styled(&s.id, style),
                ]))
            })
            .collect();

        let border_style = Style::default().fg(Color::Cyan);

        let list = List::new(items)
            .block(
                Block::default()
                    .title("─ Services ")
                    .title_style(border_style)
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        // Adjust selected index if filtering
        let mut adjusted_state = ListState::default();
        if let Some(selected) = self.sidebar_state.selected() {
            // Find the position of the originally selected service in filtered list
            let adjusted_idx = filtered_services
                .iter()
                .position(|(orig_idx, _)| *orig_idx == selected)
                .unwrap_or(0);
            adjusted_state.select(Some(adjusted_idx));
        } else {
            adjusted_state.select(Some(0));
        }

        frame.render_stateful_widget(list, panel_area, &mut adjusted_state);
        let mut lines = crate::ui_ext::LineHits::new(
            panel_area.inner(ratatui::layout::Margin::new(1, 1)),
            adjusted_state.offset(),
        );
        for (index, _) in &filtered_services {
            lines.push(Some(*index));
        }
        self.panel_lines = lines;
    }

    /// Draw the levels filter panel
    fn draw_levels_panel(&mut self, frame: &mut Frame, area: Rect) {
        let panel_width = 20u16;
        let panel_height = to_u16((self.levels.len() + 2).min(area.height as usize - 2));

        // Position in top-left with small margin
        let panel_area = Rect::new(area.x + 1, area.y, panel_width, panel_height);

        // Clear the background
        frame.render_widget(ratatui::widgets::Clear, panel_area);

        let items: Vec<ListItem> = self
            .levels
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let indicator = if l.active { "●" } else { "○" };
                let style = Style::default().fg(level_color(l.level));

                ListItem::new(Line::from(vec![
                    Span::raw(format!("{} ", i + 1)).dim(),
                    Span::styled(indicator, style),
                    Span::raw(" "),
                    Span::styled(l.level.name(), style),
                    Span::raw(format!(" ({})", l.entry_count)).dim(),
                ]))
            })
            .collect();

        let border_style = Style::default().fg(Color::Cyan);

        let list = List::new(items)
            .block(
                Block::default()
                    .title("─ Levels ")
                    .title_style(border_style)
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(list, panel_area, &mut self.levels_state);
        self.panel_lines = panel_lines(panel_area, self.levels_state.offset(), self.levels.len());
    }

    /// Draw the nodes filter panel, with each node's stream problems
    fn draw_nodes_panel(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(ni, n)| {
                let indicator = if n.active { "●" } else { "○" };
                let style = Style::default().fg(n.color);
                let mut spans = vec![
                    Span::styled(indicator, style),
                    Span::raw(" "),
                    Span::styled(n.name.clone(), style),
                    Span::raw(format!(" ({})", n.entry_count)).dim(),
                ];
                let failures = self.node_failures(ni);
                if !failures.is_empty() {
                    spans.push(Span::styled(
                        format!("  ✗ {}", failures.join("; ")),
                        Style::default().fg(Color::Red),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let panel_width = (area.width.saturating_sub(4)).min(100);
        let panel_height = to_u16((self.nodes.len() + 2).min(area.height as usize - 2));
        let panel_area = Rect::new(area.x + 1, area.y, panel_width, panel_height);
        frame.render_widget(ratatui::widgets::Clear, panel_area);

        let border_style = Style::default().fg(Color::Cyan);
        let list = List::new(items)
            .block(
                Block::default()
                    .title("─ Nodes ")
                    .title_style(border_style)
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, panel_area, &mut self.nodes_state);
        self.panel_lines = panel_lines(panel_area, self.nodes_state.offset(), self.nodes.len());
    }

    /// Draw the logs area
    fn draw_logs(&self, frame: &mut Frame, area: Rect) {
        if self.state.is_loading() {
            let (_, waiting, failed) = self.stream_counts();
            let mut lines = vec![Line::from(
                Span::raw(format!(" Waiting for logs ({waiting} streams)...")).dim(),
            )];
            if failed > 0 {
                for (ni, node) in self.nodes.iter().enumerate() {
                    for failure in self.node_failures(ni) {
                        lines.push(Line::from(
                            Span::raw(format!(" {}: {failure}", node.name)).fg(Color::Red),
                        ));
                    }
                }
            }
            frame.render_widget(Paragraph::new(lines), area);
            return;
        }

        if let Some(error) = self.state.error() {
            let error_msg = Paragraph::new(vec![
                Line::from(vec![Span::raw(" Error: ").fg(Color::Red).bold()]),
                Line::from(vec![Span::raw(" "), Span::raw(error).fg(Color::White)]),
            ]);
            frame.render_widget(error_msg, area);
            return;
        }

        let Some(data) = self.data() else { return };

        if data.visible_indices.is_empty() {
            let msg = if self.active_count() == 0 || self.active_level_count() == 0 {
                " No services/levels selected. Press 's' or 'l' to open filters."
            } else {
                " No log entries"
            };
            let empty = Paragraph::new(Line::from(Span::raw(msg).dim()));
            frame.render_widget(empty, area);
            return;
        }

        let visible_height = area.height as usize;
        let content_width = area.width.saturating_sub(1) as usize; // -1 for scrollbar

        // indicator + time + node + service + level
        let node_width = self.node_column_width();
        let node_prefix = if node_width > 0 { node_width + 1 } else { 0 };
        let prefix_width = 1 + 8 + 1 + node_prefix + 12 + 1 + 3 + 1;
        // At least 1, or wrapping a narrow pane would never end
        let available = content_width.saturating_sub(prefix_width).max(1);
        let rows_of = |vi: usize| {
            let chars = data
                .visible_indices
                .get(vi)
                .and_then(|&i| data.entries.get(i))
                .map_or(0, |e| e.message.chars().count());
            if self.wrap {
                chars.div_ceil(available).max(1)
            } else {
                1
            }
        };

        // Safety clamp scroll to valid range (max is where last entry is at viewport bottom)
        let total = data.visible_indices.len();
        let max_start = total.saturating_sub(visible_height);
        // Following: the newest entries fill the pane from the bottom, counting
        // the rows wrapped entries take
        let at_bottom = self.following || self.scroll as usize >= max_start;
        let start = if at_bottom {
            bottom_start(total, visible_height, rows_of)
        } else {
            (self.scroll as usize).min(max_start)
        };
        let end = (start + visible_height).min(total);

        let mut lines: Vec<Line> = Vec::new();

        let shown = data.visible_indices.get(start..end).unwrap_or_default();
        for (vi, &entry_idx) in shown.iter().enumerate() {
            let visible_idx = start + vi;
            let Some(entry) = data.entries.get(entry_idx) else {
                continue;
            };
            let is_current_match = self.is_current_match(visible_idx);
            let is_match = self.entry_matches(visible_idx);
            let is_selected = self.visual.contains(visible_idx, self.cursor);

            let mut spans = Vec::new();
            let is_cursor = self.is_cursor(visible_idx);

            // Line style - highlight cursor and selected lines
            let line_style = if is_cursor {
                Style::default().bg(Color::DarkGray)
            } else if is_selected {
                Style::default().bg(Color::Rgb(60, 20, 60)) // Dark magenta
            } else {
                Style::default()
            };

            // Selection/match/cursor indicator
            if is_selected {
                spans.push(Span::styled("█", Style::default().fg(Color::Magenta)));
            } else if is_current_match {
                spans.push(Span::styled("▶", Style::default().fg(Color::Yellow)));
            } else if is_cursor {
                spans.push(Span::styled(">", Style::default().fg(Color::Cyan)));
            } else {
                spans.push(Span::raw(" "));
            }

            // Timestamp - use lighter color on highlighted lines
            let timestamp_color = if is_cursor || is_selected {
                Color::Gray
            } else {
                Color::DarkGray
            };
            if !entry.timestamp.is_empty() {
                spans.push(Span::styled(
                    format!("{:>8}", entry.timestamp),
                    Style::default().fg(timestamp_color),
                ));
            } else {
                spans.push(Span::styled(
                    " NO TIME",
                    Style::default().fg(Color::DarkGray).dim(),
                ));
            }
            spans.push(Span::raw(" "));

            // Node name (colored, fixed width) when streaming several
            if node_width > 0
                && let Some(node) = self.nodes.get(entry.node)
            {
                let name: String = node.name.chars().take(node_width).collect();
                spans.push(Span::styled(
                    format!("{name:<node_width$} "),
                    Style::default().fg(node.color),
                ));
            }

            // Service name (colored, fixed width)
            spans.push(Span::styled(
                format!("{:<12}", entry.service_id),
                Style::default().fg(entry.service_color),
            ));
            spans.push(Span::raw(" "));

            // Level badge
            let level_style = Style::default()
                .fg(Color::Black)
                .bg(level_color(entry.level))
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled(entry.level.badge(), level_style));
            spans.push(Span::raw(" "));

            // Message with optional highlighting
            let message_chars = entry.message.chars().count();
            if self.wrap && message_chars > available {
                // Wrap mode: split message into multiple lines
                let msg_chars: Vec<char> = entry.message.chars().collect();
                let mut chunk_start = 0;

                while chunk_start < msg_chars.len() {
                    let chunk_end = (chunk_start + available).min(msg_chars.len());
                    let chunk: String = msg_chars
                        .get(chunk_start..chunk_end)
                        .unwrap_or_default()
                        .iter()
                        .collect();

                    if chunk_start == 0 {
                        // First line: use the prefix spans we already built
                        if is_match && !self.search_query.is_empty() {
                            spans.extend(
                                self.render_message_with_highlight(&chunk, is_current_match),
                            );
                        } else {
                            spans.push(Span::raw(chunk));
                        }
                        lines.push(Line::from(spans.clone()).style(line_style));
                        spans.clear();
                    } else {
                        // Continuation lines: indent to align with message start
                        let indent = " ".repeat(prefix_width);
                        let mut cont_spans = vec![Span::raw(indent)];
                        if is_match && !self.search_query.is_empty() {
                            cont_spans.extend(
                                self.render_message_with_highlight(&chunk, is_current_match),
                            );
                        } else {
                            cont_spans.push(Span::raw(chunk));
                        }
                        lines.push(Line::from(cont_spans).style(line_style));
                    }
                    chunk_start = chunk_end;
                }
            } else if message_chars <= available {
                if is_match && !self.search_query.is_empty() {
                    spans.extend(
                        self.render_message_with_highlight(&entry.message, is_current_match),
                    );
                } else {
                    spans.push(Span::raw(entry.message.clone()));
                }
                lines.push(Line::from(spans).style(line_style));
            } else {
                // Truncate mode
                let truncated: String = entry
                    .message
                    .chars()
                    .take(available.saturating_sub(1))
                    .collect();
                if is_match && !self.search_query.is_empty() {
                    spans.extend(self.render_message_with_highlight(&truncated, is_current_match));
                } else {
                    spans.push(Span::raw(truncated));
                }
                spans.push(Span::raw("…").dim());
                lines.push(Line::from(spans).style(line_style));
            }
        }

        // Anchored to the bottom, the newest row is the last one drawn
        let skip = if at_bottom {
            lines.len().saturating_sub(visible_height)
        } else {
            0
        };
        let logs = Paragraph::new(lines).scroll((to_u16(skip), 0));
        frame.render_widget(logs, area);

        // Scrollbar
        if data.visible_indices.len() > visible_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"))
                .track_symbol(Some("│"))
                .thumb_symbol("█");
            let mut scrollbar_state = ScrollbarState::new(data.visible_indices.len())
                .position(self.scroll as usize)
                .viewport_content_length(visible_height);
            frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
        }
    }
}

/// Stream one service (or the kernel log) of one node into `tx` until the
/// stream ends or the task is aborted. `tail` lines of history come first.
async fn stream_task(
    client: talos_rs::TalosClient,
    node: usize,
    service: String,
    tail: i32,
    tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
) {
    let status = |status| StreamEvent::Status {
        node,
        service: service.clone(),
        status,
    };
    let line = |line| StreamEvent::Line {
        node,
        service: service.clone(),
        line,
    };

    // The kernel stream gives the whole ring buffer or only new lines, so
    // the history is a separate, bounded read of its last `tail` lines
    let mut last_history = None;
    if service == KERNEL_SERVICE && tail > 0 {
        let history = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            client.dmesg(false, false),
        )
        .await;
        match history {
            Ok(Ok(text)) => {
                let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                let skip = lines
                    .len()
                    .saturating_sub(usize::try_from(tail).unwrap_or(usize::MAX));
                if !lines.is_empty() {
                    let _ = tx.send(status(StreamStatus::Streaming));
                }
                for l in lines.iter().skip(skip) {
                    if tx.send(line(l.to_string())).is_err() {
                        return;
                    }
                }
                last_history = lines.last().map(std::string::ToString::to_string);
            }
            Ok(Err(e)) => {
                let _ = tx.send(status(StreamStatus::Failed(e.to_string())));
                return;
            }
            Err(_) => {
                let _ = tx.send(status(StreamStatus::Failed(
                    "timed out reading the kernel log".to_string(),
                )));
                return;
            }
        }
    }

    let stream = if service == KERNEL_SERVICE {
        client.dmesg_stream(true).await
    } else {
        client.logs_stream(&service, tail).await
    };
    let mut rx = match stream {
        Ok(rx) => rx,
        Err(e) => {
            let _ = tx.send(status(StreamStatus::Failed(
                talos9s_core::format_talos_error(&e),
            )));
            return;
        }
    };
    let mut first = true;
    while let Some(text) = rx.recv().await {
        if first {
            first = false;
            let _ = tx.send(status(StreamStatus::Streaming));
            // The follow stream may repeat the line the history ended with
            if last_history.as_deref() == Some(text.as_str()) {
                continue;
            }
        }
        if tx.send(line(text)).is_err() {
            return;
        }
    }
    let _ = tx.send(status(StreamStatus::Ended));
}

impl MultiLogsComponent {
    /// The keys that act in the current mode: typing a search, a search
    /// active, a floating menu open, a selection, or none of those
    fn key_hints_line(&self) -> Line<'static> {
        let footer_spans = if self.search_mode == SearchMode::Input {
            vec![
                Span::raw(" Type to search").dim(),
                Span::raw("  "),
                Span::raw("[Enter]").fg(Color::Yellow),
                Span::raw(" confirm").dim(),
                Span::raw("  "),
                Span::raw("[Esc]").fg(Color::Yellow),
                Span::raw(" cancel").dim(),
            ]
        } else if self.search_mode == SearchMode::Active {
            vec![
                Span::raw(" [n/N]").fg(Color::Yellow),
                Span::raw(" next/prev").dim(),
                Span::raw("  "),
                Span::raw("[/]").fg(Color::Yellow),
                Span::raw(" new search").dim(),
                Span::raw("  "),
                Span::raw("[Esc]").fg(Color::Yellow),
                Span::raw(" clear").dim(),
            ]
        } else if self.floating_pane != FloatingPane::None {
            let mut spans = vec![
                Span::raw(" [Space]").fg(Color::Yellow),
                Span::raw(" toggle").dim(),
                Span::raw("  "),
                Span::raw("[0]").fg(Color::Yellow),
                Span::raw(" all").dim(),
                Span::raw("  "),
                Span::raw("[n]").fg(Color::Yellow),
                Span::raw(" none").dim(),
                Span::raw("  "),
            ];
            // The level keys work everywhere in logs; they're listed here
            // to keep the main key line within 100 columns
            if self.floating_pane == FloatingPane::Levels {
                spans.extend([
                    Span::raw("[1-5]").fg(Color::Yellow),
                    Span::raw(" only").dim(),
                    Span::raw("  "),
                    Span::raw("[⇧1-5]").fg(Color::Yellow),
                    Span::raw(" ±level").dim(),
                    Span::raw("  "),
                ]);
            }
            spans.extend([
                Span::raw("[Esc/q]").fg(Color::Yellow),
                Span::raw(" close").dim(),
            ]);
            spans
        } else if self.visual.active() {
            // Visual mode footer
            vec![
                Span::raw(" [j/k]").fg(Color::Yellow),
                Span::raw(" extend").dim(),
                Span::raw(" "),
                Span::raw("[y]").fg(Color::Yellow),
                Span::raw(" yank").dim(),
                Span::raw(" "),
                Span::raw("[Esc/V]").fg(Color::Yellow),
                Span::raw(" cancel").dim(),
            ]
        } else {
            let wrap_text = if self.wrap { " nowrap" } else { " wrap" };
            // Grouped so the line fits a 100 column screen
            let (panes, pane_labels) = if self.multi_node() {
                (" [s/o/l]", " svcs/nodes/lvls")
            } else {
                (" [s/l]", " svcs/lvls")
            };
            let follow_text = if self.streaming {
                " follow/stop"
            } else {
                " follow/stream"
            };
            let spans = vec![
                Span::raw(panes).fg(Color::Yellow),
                Span::raw(pane_labels).dim(),
                Span::raw(" "),
                Span::raw("[/]").fg(Color::Yellow),
                Span::raw(" search").dim(),
                Span::raw(" "),
                Span::raw("[f/F]").fg(Color::Yellow),
                Span::raw(follow_text).dim(),
                Span::raw(" "),
                Span::raw("[w]").fg(Color::Yellow),
                Span::raw(wrap_text).dim(),
                Span::raw(" "),
                Span::raw("[V]").fg(Color::Yellow),
                Span::raw(" visual").dim(),
                Span::raw(" "),
                Span::raw("[y]").fg(Color::Yellow),
                Span::raw(" yank").dim(),
            ];
            spans
        };

        Line::from(footer_spans)
    }
}

/// Lines of a bordered floating panel listing entries `0..len` from `offset`
fn panel_lines(area: Rect, offset: usize, len: usize) -> crate::ui_ext::LineHits<usize> {
    let mut lines =
        crate::ui_ext::LineHits::new(area.inner(ratatui::layout::Margin::new(1, 1)), offset);
    for i in 0..len {
        lines.push(Some(i));
    }
    lines
}

impl Component for MultiLogsComponent {
    /// A click on an entry of the open panel selects and toggles it, like
    /// Space
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        let Some(i) = self.panel_lines.clicked(&mouse) else {
            return Ok(None);
        };
        match self.floating_pane {
            FloatingPane::Services => {
                self.selected_service = i;
                self.sidebar_state.select(Some(i));
                self.toggle_service(i);
            }
            FloatingPane::Levels => {
                self.selected_level = i;
                self.levels_state.select(Some(i));
                self.toggle_level(i);
            }
            FloatingPane::Nodes => {
                self.selected_node = i;
                self.nodes_state.select(Some(i));
                self.toggle_node(i);
            }
            FloatingPane::None => {}
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn captures_text(&self) -> bool {
        self.search_mode == SearchMode::Input
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        // Handle search input mode
        if self.search_mode == SearchMode::Input {
            use crate::ui_ext::{TextEdit, edit_text};
            match edit_text(&mut self.search_query, &key) {
                TextEdit::Changed => self.update_matches(),
                TextEdit::Submit if !self.search_query.is_empty() => {
                    self.search_mode = SearchMode::Active;
                }
                TextEdit::Submit | TextEdit::Cancel => self.clear_search(),
                TextEdit::Ignored => {}
            }
            return Ok(None);
        }

        if let Some(hotkey) = LevelHotkey::from_key(&key) {
            match hotkey {
                // 0 is "all" for the open pane, else for the levels
                LevelHotkey::All => match self.floating_pane {
                    FloatingPane::Services => self.activate_all(),
                    FloatingPane::Nodes => self.set_all_nodes(true),
                    FloatingPane::Levels | FloatingPane::None => self.activate_all_levels(),
                },
                LevelHotkey::Only(idx) => self.show_only_level(idx),
                LevelHotkey::Toggle(idx) => self.toggle_level(idx),
            }
            return Ok(None);
        }

        match key.code {
            // q is Esc here too (Esc never arrives in some terminals): close
            // what is open first, else back to the tree
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.visual.active() {
                    self.visual.clear();
                } else if self.search_mode == SearchMode::Active {
                    self.clear_search();
                } else if self.floating_pane != FloatingPane::None {
                    self.floating_pane = FloatingPane::None;
                } else {
                    return Ok(Some(Action::Back));
                }
                Ok(None)
            }

            // Open specific panes
            KeyCode::Char('s') => {
                self.floating_pane = if self.floating_pane == FloatingPane::Services {
                    FloatingPane::None
                } else {
                    FloatingPane::Services
                };
                Ok(None)
            }
            KeyCode::Char('o') if self.multi_node() => {
                self.floating_pane = if self.floating_pane == FloatingPane::Nodes {
                    FloatingPane::None
                } else {
                    FloatingPane::Nodes
                };
                Ok(None)
            }
            KeyCode::Char('l') if self.search_mode != SearchMode::Active => {
                self.floating_pane = if self.floating_pane == FloatingPane::Levels {
                    FloatingPane::None
                } else {
                    FloatingPane::Levels
                };
                Ok(None)
            }

            // Navigation - when a pane is open, navigate the pane; otherwise move cursor
            KeyCode::Up | KeyCode::Char('k') => {
                match self.floating_pane {
                    FloatingPane::Services => {
                        self.selected_service = self.selected_service.saturating_sub(1);
                        self.sidebar_state.select(Some(self.selected_service));
                    }
                    FloatingPane::Levels => {
                        self.selected_level = self.selected_level.saturating_sub(1);
                        self.levels_state.select(Some(self.selected_level));
                    }
                    FloatingPane::Nodes => {
                        self.selected_node = self.selected_node.saturating_sub(1);
                        self.nodes_state.select(Some(self.selected_node));
                    }
                    FloatingPane::None => {
                        // Always move cursor up
                        self.cursor = self.cursor.saturating_sub(1);
                        // Scroll viewport if cursor goes above visible area
                        if self.cursor < self.scroll as usize {
                            self.scroll = to_u16(self.cursor);
                        }
                        self.following = false;
                    }
                }
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.floating_pane {
                    FloatingPane::Services => {
                        self.selected_service =
                            (self.selected_service + 1).min(self.services.len().saturating_sub(1));
                        self.sidebar_state.select(Some(self.selected_service));
                    }
                    FloatingPane::Levels => {
                        self.selected_level =
                            (self.selected_level + 1).min(self.levels.len().saturating_sub(1));
                        self.levels_state.select(Some(self.selected_level));
                    }
                    FloatingPane::Nodes => {
                        self.selected_node =
                            (self.selected_node + 1).min(self.nodes.len().saturating_sub(1));
                        self.nodes_state.select(Some(self.selected_node));
                    }
                    FloatingPane::None => {
                        // Always move cursor down
                        let max_idx = self
                            .data()
                            .map(|d| d.visible_indices.len())
                            .unwrap_or(0)
                            .saturating_sub(1);
                        self.cursor = (self.cursor + 1).min(max_idx);
                        // Scroll viewport if cursor goes below visible area
                        let viewport = self.viewport_height as usize;
                        let visible_end = self.scroll as usize + viewport;
                        if self.cursor >= visible_end {
                            self.scroll = to_u16((self.cursor + 1).saturating_sub(viewport));
                        }
                        self.following = false;
                    }
                }
                Ok(None)
            }
            KeyCode::PageUp => {
                // Always move cursor
                self.cursor = self.cursor.saturating_sub(20);
                if self.cursor < self.scroll as usize {
                    self.scroll = to_u16(self.cursor);
                }
                self.following = false;
                Ok(None)
            }
            KeyCode::PageDown => {
                // Always move cursor
                let max_idx = self
                    .data()
                    .map(|d| d.visible_indices.len())
                    .unwrap_or(0)
                    .saturating_sub(1);
                self.cursor = (self.cursor + 20).min(max_idx);
                let viewport = self.viewport_height as usize;
                let visible_end = self.scroll as usize + viewport;
                if self.cursor >= visible_end {
                    self.scroll = to_u16((self.cursor + 1).saturating_sub(viewport));
                }
                self.following = false;
                Ok(None)
            }

            // Vim-style half-page scroll
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let half = (self.viewport_height / 2).max(1) as usize;
                // Always move cursor up by half page
                self.cursor = self.cursor.saturating_sub(half);
                // Scroll to keep cursor visible
                if self.cursor < self.scroll as usize {
                    self.scroll = to_u16(self.cursor);
                }
                self.following = false;
                Ok(None)
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let half = (self.viewport_height / 2).max(1) as usize;
                // Always move cursor down by half page
                let max_idx = self
                    .data()
                    .map(|d| d.visible_indices.len())
                    .unwrap_or(0)
                    .saturating_sub(1);
                self.cursor = (self.cursor + half).min(max_idx);
                // Scroll to keep cursor visible
                let viewport = self.viewport_height as usize;
                let visible_end = self.scroll as usize + viewport;
                if self.cursor >= visible_end {
                    self.scroll = to_u16((self.cursor + 1).saturating_sub(viewport));
                }
                self.following = false;
                Ok(None)
            }
            KeyCode::Home | KeyCode::Char('g') => {
                // Always move cursor to top
                self.cursor = 0;
                self.scroll = 0;
                self.following = false;
                Ok(None)
            }
            KeyCode::Char('G') => {
                // Always move cursor to bottom
                let len = self.data().map(|d| d.visible_indices.len()).unwrap_or(0);
                let max_idx = len.saturating_sub(1);
                self.cursor = max_idx;
                let viewport = self.viewport_height as usize;
                self.scroll = to_u16(len.saturating_sub(viewport));
                self.following = true;
                Ok(None)
            }

            // Toggle in current pane
            KeyCode::Char(' ') => {
                match self.floating_pane {
                    FloatingPane::Services => self.toggle_service(self.selected_service),
                    FloatingPane::Levels => self.toggle_level(self.selected_level),
                    FloatingPane::Nodes => self.toggle_node(self.selected_node),
                    FloatingPane::None => {}
                }
                Ok(None)
            }
            // Search
            KeyCode::Char('/') => {
                self.search_mode = SearchMode::Input;
                self.search_query.clear();
                self.match_set.clear();
                self.match_order.clear();
                Ok(None)
            }
            KeyCode::Char('n') => {
                if self.search_mode == SearchMode::Active {
                    self.next_match();
                } else {
                    // n = none (deactivate all) when a pane is open
                    match self.floating_pane {
                        FloatingPane::Services => self.deactivate_all(),
                        FloatingPane::Levels => self.deactivate_all_levels(),
                        FloatingPane::Nodes => self.set_all_nodes(false),
                        FloatingPane::None => {}
                    }
                }
                Ok(None)
            }
            KeyCode::Char('N') => {
                if self.search_mode == SearchMode::Active {
                    self.prev_match();
                }
                Ok(None)
            }

            // Follow mode
            KeyCode::Char('f') => {
                if self.following {
                    self.following = false;
                } else {
                    self.scroll_to_bottom();
                }
                Ok(None)
            }

            // Toggle streaming (Shift+F)
            KeyCode::Char('F') => {
                if self.streaming {
                    self.stop_streaming();
                } else {
                    self.start_streaming();
                }
                Ok(None)
            }

            // Toggle line wrapping
            KeyCode::Char('w') => {
                self.wrap = !self.wrap;
                Ok(None)
            }

            // Visual line selection mode
            KeyCode::Char('V') => {
                self.visual.toggle(self.cursor);
                Ok(None)
            }

            // Yank selection or current line to clipboard
            KeyCode::Char('y') => {
                let (success, count) = self.yank_selection();
                if success {
                    tracing::info!("Copied {} log entries to clipboard", count);
                }
                // Clear visual selection after yank
                self.visual.clear();
                Ok(None)
            }

            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Process any incoming streamed log entries
            if self.streaming {
                self.process_stream_entries();
                // Animate pulse indicator (cycles through 0-7)
                self.pulse_frame = (self.pulse_frame + 1) % 8;
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Layout: Header, Content (logs with floating services), optional search, footer
        let has_search_bar = self.search_mode != SearchMode::Off;

        let [header_area, content_area, search_area] = Layout::vertical([
            Constraint::Length(2),                         // Header
            Constraint::Min(0),                            // Content (logs + floating services)
            Constraint::Length(u16::from(has_search_bar)), // Search bar
        ])
        .areas(area);

        // Header with title matching mockup
        let follow_indicator = if self.following && self.streaming {
            // Pulsing LIVE indicator when streaming
            let pulse_color = match self.pulse_frame {
                0 | 4 => Color::Green,
                1 | 3 | 5 | 7 => Color::LightGreen,
                2 | 6 => Color::Rgb(100, 255, 100), // Brighter green
                _ => Color::Green,
            };
            Span::styled("● LIVE ", Style::default().fg(pulse_color).bold())
        } else if self.following {
            Span::styled("● LIVE ", Style::default().fg(Color::Green).bold())
        } else {
            Span::styled("○ PAUSED ", Style::default().fg(Color::DarkGray))
        };

        let nodes = if self.multi_node() {
            format!(", {}/{} nodes", self.active_node_count(), self.nodes.len())
        } else {
            String::new()
        };
        let mut header_spans = vec![
            Span::raw(if self.multi_node() {
                " Cluster Logs: "
            } else {
                " Multi-Service Logs: "
            })
            .bold()
            .fg(Color::Cyan),
            Span::raw(&self.title).fg(Color::White),
            Span::raw(format!(" ({})", self.subtitle)).dim(),
            Span::raw("  "),
            follow_indicator,
            Span::raw(format!(
                " [{}/{} svcs, {}/{} lvls{nodes}]",
                self.active_count(),
                self.services.len(),
                self.active_level_count(),
                self.levels.len()
            ))
            .dim(),
        ];
        let (_, waiting, failed) = self.stream_counts();
        if failed > 0 {
            let hint = if self.multi_node() { ", see [o]" } else { "" };
            header_spans.push(Span::styled(
                format!("  {failed} stream(s) failed{hint}"),
                Style::default().fg(Color::Red),
            ));
        }
        if waiting > 0 && self.streaming {
            header_spans.push(Span::raw(format!("  {waiting} waiting")).dim());
        }

        // Show match count when searching
        if !self.match_order.is_empty() {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("[{} matches]", self.match_order.len()),
                Style::default().fg(Color::Yellow).bold(),
            ));
        } else if self.search_mode != SearchMode::Off && !self.search_query.is_empty() {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                "[no matches]",
                Style::default().fg(Color::Red),
            ));
        }

        // Show visual selection indicator
        if let Some(range) = self.visual.range(self.cursor) {
            let count = range.count();
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("-- VISUAL ({count} lines) --"),
                Style::default().fg(Color::Magenta).bold(),
            ));
        }

        let header = Paragraph::new(Line::from(header_spans)).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(header, header_area);

        // Content area - logs, with floating services panel on top in Full mode
        // Update viewport height for half-page scrolling
        self.viewport_height = content_area.height;

        // Draw logs (no border)
        self.draw_logs(frame, content_area);

        // Draw floating panel on top based on which pane is open
        self.panel_lines = Default::default();
        match self.floating_pane {
            FloatingPane::Services => self.draw_services_panel(frame, content_area),
            FloatingPane::Levels => self.draw_levels_panel(frame, content_area),
            FloatingPane::Nodes => self.draw_nodes_panel(frame, content_area),
            FloatingPane::None => {}
        }

        // Search bar
        if has_search_bar {
            let cursor = if self.search_mode == SearchMode::Input {
                "█"
            } else {
                ""
            };

            // Match count on the right
            let match_info = if !self.match_order.is_empty() {
                format!("[{}/{}]", self.current_match + 1, self.match_order.len())
            } else {
                String::new()
            };

            let search_line = Line::from(vec![
                Span::styled(" /", Style::default().fg(Color::Yellow)),
                Span::raw(&self.search_query),
                Span::styled(cursor, Style::default().fg(Color::Yellow)),
            ]);
            frame.render_widget(Paragraph::new(search_line), search_area);

            // Render match count on the right side
            if !match_info.is_empty() {
                let match_para = Paragraph::new(Line::from(vec![
                    Span::styled(&match_info, Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                ]))
                .alignment(ratatui::layout::Alignment::Right);
                frame.render_widget(match_para, search_area);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn node(name: &str) -> LogNode {
        LogNode {
            name: name.to_string(),
            ip: format!("10.0.0.{}", name.len()),
            services: Some(vec!["etcd".to_string()]),
        }
    }

    /// A cluster view of etcd and the kernel on three nodes, fed lines as
    /// the stream tasks would send them
    fn cluster_logs(lines: &[(usize, &str, &str)]) -> MultiLogsComponent {
        let mut logs = MultiLogsComponent::new(
            "bc control plane".into(),
            "3 nodes".into(),
            vec![node("bc1"), node("bc2"), node("bc3")],
            &["etcd".into(), KERNEL_SERVICE.into()],
            vec!["etcd".into(), KERNEL_SERVICE.into()],
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        logs.stream_tx = Some(tx.clone());
        logs.stream_rx = Some(rx);
        logs.streaming = true;
        for (node, service, line) in lines {
            tx.send(StreamEvent::Line {
                node: *node,
                service: service.to_string(),
                line: line.to_string(),
            })
            .unwrap();
        }
        logs.update(Action::Tick).unwrap();
        logs
    }

    #[tokio::test]
    async fn a_burst_is_spread_over_ticks_not_dropped() {
        let lines: Vec<(usize, &str, String)> = (0..MAX_LINES_PER_TICK * 2 + 50)
            .map(|i| (0, "etcd", format!("2026-11-01T10:00:00Z line {i}")))
            .collect();
        let borrowed: Vec<(usize, &str, &str)> =
            lines.iter().map(|(n, s, l)| (*n, *s, l.as_str())).collect();
        let mut logs = cluster_logs(&[]);
        let tx = logs.stream_tx.clone().unwrap();
        logs.streams.insert(
            (1, "etcd".to_string()),
            (tokio::spawn(async {}), StreamStatus::Waiting),
        );
        for (node, service, line) in &borrowed {
            tx.send(StreamEvent::Line {
                node: *node,
                service: service.to_string(),
                line: line.to_string(),
            })
            .unwrap();
        }
        // A status after the burst must still arrive
        tx.send(StreamEvent::Status {
            node: 1,
            service: "etcd".to_string(),
            status: StreamStatus::Streaming,
        })
        .unwrap();
        for _ in 0..3 {
            logs.process_stream_entries();
        }
        assert_eq!(logs.data().unwrap().entries.len(), borrowed.len());
        assert_eq!(
            logs.streams.get(&(1, "etcd".to_string())).map(|(_, s)| s),
            Some(&StreamStatus::Streaming)
        );
    }

    #[test]
    fn following_with_wrap_keeps_the_newest_row_in_view() {
        let long = format!("2026-11-01T10:00:02Z {}", "x".repeat(2000));
        let mut logs = cluster_logs(&[
            (0, "etcd", "2026-11-01T10:00:00Z first"),
            (1, "etcd", "2026-11-01T10:00:01Z second"),
            (2, "etcd", &long),
            (0, "etcd", "2026-11-01T10:00:03Z newest line"),
        ]);
        logs.wrap = true;
        let screen = render(&mut logs);
        assert!(screen.contains("newest line"), "{screen}");
    }

    fn render(logs: &mut MultiLogsComponent) -> String {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(140, 14)).unwrap();
        terminal.draw(|f| logs.draw(f, f.area()).unwrap()).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Streams etcd and the kernel log from bc's nodes for a few seconds:
    /// `cargo test --lib live_cluster_logs -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_cluster_logs() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("TALOS9S_LIVE_CONTEXT").unwrap_or_else(|_| "bc".to_string());
        let ips: Vec<String> = std::env::var("TALOS9S_LIVE_NODES")
            .expect("set TALOS9S_LIVE_NODES=ip,ip,...")
            .split(',')
            .map(str::to_string)
            .collect();
        let client = talos_rs::TalosClient::from_named_context(&context)
            .await
            .unwrap();
        let nodes = ips
            .iter()
            .map(|ip| LogNode {
                name: ip.clone(),
                ip: ip.clone(),
                services: None,
            })
            .collect();
        let services = vec!["etcd".to_string(), KERNEL_SERVICE.to_string()];
        let mut logs =
            MultiLogsComponent::new(context, "live".into(), nodes, &services, services.clone());
        logs.set_client(client, 20);
        logs.start_streaming();
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            logs.update(Action::Tick).unwrap();
        }
        let data = logs.data().unwrap();
        for (ni, n) in logs.nodes.iter().enumerate() {
            let per: Vec<String> = ["etcd", KERNEL_SERVICE]
                .iter()
                .map(|svc| {
                    let count = data
                        .entries
                        .iter()
                        .filter(|e| e.node == ni && e.service_id == *svc)
                        .count();
                    let status = logs.streams.get(&(ni, svc.to_string())).map(|s| &s.1);
                    format!("{svc}={count} {status:?}")
                })
                .collect();
            println!("{}: {}", n.name, per.join(", "));
        }
        let mut kernel = data
            .entries
            .iter()
            .filter(|e| e.service_id == KERNEL_SERVICE);
        let no_time = kernel.clone().filter(|e| e.timestamp.is_empty()).count();
        println!("kernel entries without a time: {no_time}");
        if let Some(e) = kernel.next_back() {
            println!("last kernel: {} {:?} {}", e.timestamp, e.level, e.message);
        }
        let sorted = data
            .entries
            .windows(2)
            .all(|w| w[0].timestamp_sort <= w[1].timestamp_sort);
        println!("sorted: {sorted}, total {}", data.entries.len());
        logs.stop_streaming();
    }

    #[tokio::test]
    async fn merges_nodes_by_time_with_a_node_column() {
        let mut logs = cluster_logs(&[
            (
                1,
                "etcd",
                r#"{"level":"warn","ts":"2026-10-19T08:20:02.000Z","msg":"slow apply"}"#,
            ),
            (
                0,
                "etcd",
                r#"{"level":"info","ts":"2026-10-19T08:20:01.000Z","msg":"leader changed"}"#,
            ),
            (
                2,
                KERNEL_SERVICE,
                "kern:    info: [2026-10-19T08:20:03.000Z]: drbd link up",
            ),
        ]);
        let screen = render(&mut logs);
        assert!(
            screen.contains("Cluster Logs: bc control plane"),
            "{screen}"
        );
        assert!(screen.contains("3/3 nodes"), "{screen}");
        let positions: Vec<usize> = ["leader changed", "slow apply", "drbd link up"]
            .iter()
            .map(|m| screen.find(m).unwrap())
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{screen}");
        assert!(screen.contains("bc1 etcd"), "{screen}");
        assert!(screen.contains("bc3 kernel"), "{screen}");
        // The node filter is offered in the key line, not drawn in the view
        let hints = logs.key_hints().unwrap().to_string();
        assert!(hints.contains("[s/o/l] svcs/nodes"), "{hints}");
    }

    #[tokio::test]
    async fn nodes_pane_hides_a_nodes_lines_and_shows_failures() {
        let mut logs = cluster_logs(&[
            (
                0,
                "etcd",
                r#"{"level":"info","ts":"2026-10-19T08:20:01.000Z","msg":"from bc1"}"#,
            ),
            (
                1,
                "etcd",
                r#"{"level":"info","ts":"2026-10-19T08:20:02.000Z","msg":"from bc2"}"#,
            ),
        ]);
        // bc3's stream failed
        let tx = logs.stream_tx.clone().unwrap();
        logs.streams.insert(
            (2, "etcd".to_string()),
            (tokio::spawn(async {}), StreamStatus::Waiting),
        );
        tx.send(StreamEvent::Status {
            node: 2,
            service: "etcd".to_string(),
            status: StreamStatus::Failed("node unreachable".to_string()),
        })
        .unwrap();
        logs.update(Action::Tick).unwrap();

        for code in [KeyCode::Char('o'), KeyCode::Char('j'), KeyCode::Char(' ')] {
            logs.handle_key_event(key(code, KeyModifiers::NONE))
                .unwrap();
        }
        let screen = render(&mut logs);
        assert!(screen.contains("2/3 nodes"), "{screen}");
        assert!(screen.contains("1 stream(s) failed"), "{screen}");
        assert!(screen.contains("✗ etcd: node unreachable"), "{screen}");
        logs.handle_key_event(key(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        let screen = render(&mut logs);
        assert!(screen.contains("from bc1"), "{screen}");
        assert!(!screen.contains("from bc2"), "{screen}");
    }

    #[test]
    fn a_single_node_has_no_node_column() {
        let mut logs = MultiLogsComponent::new(
            "10.0.0.1".into(),
            "worker".into(),
            vec![node("w1")],
            &["kubelet".into()],
            vec!["kubelet".into()],
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        logs.stream_rx = Some(rx);
        tx.send(StreamEvent::Line {
            node: 0,
            service: "kubelet".to_string(),
            line: r#"{"level":"info","ts":"2026-10-19T08:20:01.000Z","msg":"synced"}"#.to_string(),
        })
        .unwrap();
        logs.process_stream_entries();
        let screen = render(&mut logs);
        assert!(screen.contains("Multi-Service Logs: 10.0.0.1"), "{screen}");
        assert!(screen.contains("synced"), "{screen}");
        assert!(!screen.contains("w1 "), "{screen}");
        assert!(!screen.contains("[o]"), "{screen}");
    }

    #[test]
    fn level_hotkeys() {
        let none = KeyModifiers::NONE;
        let shift = KeyModifiers::SHIFT;
        let hk = |code, m| LevelHotkey::from_key(&key(code, m));
        assert_eq!(hk(KeyCode::Char('0'), none), Some(LevelHotkey::All));
        assert_eq!(hk(KeyCode::Char('1'), none), Some(LevelHotkey::Only(0)));
        assert_eq!(hk(KeyCode::Char('5'), none), Some(LevelHotkey::Only(4)));
        assert_eq!(hk(KeyCode::Char('2'), shift), Some(LevelHotkey::Toggle(1)));
        assert_eq!(hk(KeyCode::Char('@'), shift), Some(LevelHotkey::Toggle(1)));
        assert_eq!(hk(KeyCode::Char('%'), none), Some(LevelHotkey::Toggle(4)));
        assert_eq!(hk(KeyCode::Char('6'), none), None);
        assert_eq!(hk(KeyCode::Char('q'), none), None);
    }

    #[test]
    fn level_filter_round_trips_and_hotkeys_combine() {
        let mut logs = MultiLogsComponent::new(
            "10.0.0.1".into(),
            "worker".into(),
            vec![node("w1")],
            &["kubelet".into()],
            vec!["kubelet".into()],
        );
        assert_eq!(logs.level_filter(), ALL_LEVELS);

        // 1 then Shift+2: errors + warnings
        logs.handle_key_event(key(KeyCode::Char('1'), KeyModifiers::NONE))
            .unwrap();
        logs.handle_key_event(key(KeyCode::Char('@'), KeyModifiers::SHIFT))
            .unwrap();
        let filter = logs.level_filter();
        assert_eq!(filter, [true, true, false, false, false]);

        let mut reopened = MultiLogsComponent::new(
            "10.0.0.1".into(),
            "worker".into(),
            vec![node("w1")],
            &["kubelet".into()],
            vec!["kubelet".into()],
        );
        reopened.set_level_filter(filter);
        assert_eq!(reopened.level_filter(), filter);

        reopened
            .handle_key_event(key(KeyCode::Char('0'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(reopened.level_filter(), ALL_LEVELS);
    }
}
