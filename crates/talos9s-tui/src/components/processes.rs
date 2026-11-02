//! Processes component - displays running processes on one or more nodes
//!
//! "Show me what's wrong in 5 seconds"
//!
//! Opened for a single node it shows that node's processes; opened for a
//! cluster or a control plane / workers group it merges the processes of every
//! node into one list with a NODE column, so the busiest processes of the whole
//! group are visible at once.

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use talos_rs::{CpuStat, MemInfo, ProcessInfo, ProcessState, TalosClient};
use talos9s_core::{AsyncState, format_bytes_compact};

const AUTO_REFRESH_INTERVAL_SECS: u64 = talos9s_core::constants::refresh_intervals::NORMAL;

/// Upper bound for each per-node API call in a refresh
const NODE_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// CPU % is the CPU time used between two samples, so the first load takes
/// a second one this long after the first before showing anything
const FIRST_SAMPLE_GAP: Duration = Duration::from_secs(1);

/// Sort order for process list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumString, strum::IntoStaticStr)]
pub enum SortBy {
    #[default]
    CpuPercent,
    CpuTime,
    Mem,
}

impl SortBy {
    pub fn label(&self) -> &'static str {
        match self {
            SortBy::CpuPercent => "CPU%",
            SortBy::CpuTime => "TIME",
            SortBy::Mem => "MEM",
        }
    }
}

/// Component mode
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Filtering,
}

/// A node whose processes are listed
#[derive(Debug, Clone)]
pub struct ProcessNode {
    pub hostname: String,
    pub address: String,
}

/// Memory, core count and load of a node the dashboard already has, so the
/// view asks the node only for its processes and CPU counters
#[derive(Debug, Clone)]
pub struct NodeBasics {
    pub meminfo: MemInfo,
    pub cpu_count: usize,
    pub load: (f64, f64, f64),
}

/// A process and the node it runs on. PIDs are only unique per node, so
/// processes are identified by `(node index, pid)`.
type ProcKey = (usize, i32);

#[derive(Debug, Clone)]
struct NodeProcess {
    /// Index into the component's `nodes`
    node: usize,
    info: ProcessInfo,
}

impl std::ops::Deref for NodeProcess {
    type Target = ProcessInfo;
    fn deref(&self) -> &ProcessInfo {
        &self.info
    }
}

impl NodeProcess {
    fn key(&self) -> ProcKey {
        (self.node, self.info.pid)
    }

    fn parent_key(&self) -> ProcKey {
        (self.node, self.info.ppid)
    }
}

/// State counts for summary bar
#[derive(Debug, Clone, Default)]
struct StateCounts {
    running: usize,
    sleeping: usize,
    disk_wait: usize,
    zombie: usize,
}

/// Display entry for tree view
#[derive(Debug, Clone)]
struct DisplayEntry {
    /// Index into processes vec
    process_idx: usize,
    /// Depth in tree (0 = root)
    depth: usize,
    /// Whether this is the last child at its level
    is_last: bool,
    /// Ancestry flags for drawing connectors (true = has more siblings below)
    ancestors_have_siblings: Vec<bool>,
}

/// Per-node system stats and CPU sampling state
#[derive(Debug, Clone, Default)]
struct NodeStats {
    /// Total memory on node (for percentage calc)
    total_memory: u64,
    /// Memory used on node
    memory_used: u64,
    /// Previous CPU stats (for calculating usage delta)
    prev_cpu_stat: Option<CpuStat>,
    /// Node CPU usage percentage, once two samples exist
    cpu_usage_percent: Option<f32>,
    /// Number of CPUs on node
    cpu_count: usize,
    /// Load average (1, 5, 15 min)
    load_avg: (f64, f64, f64),
    /// Previous CPU time per PID (for calculating per-process CPU %)
    prev_cpu_times: HashMap<i32, f64>,
    /// Time of last CPU measurement
    last_cpu_sample: Option<Instant>,
    /// Why the last process fetch for this node failed, if it did
    error: Option<String>,
}

impl NodeStats {
    fn memory_usage_percent(&self) -> f32 {
        if self.total_memory == 0 {
            0.0
        } else {
            self.memory_used as f32 / self.total_memory as f32 * 100.0
        }
    }
}

/// Loaded process data (wrapped by AsyncState)
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessesData {
    /// Processes of all nodes
    processes: Vec<NodeProcess>,
    /// Filtered process indices (into processes vec)
    filtered_indices: Vec<usize>,
    /// Display entries for tree view (with depth and connector info)
    display_entries: Vec<DisplayEntry>,
    /// State counts for summary
    state_counts: StateCounts,
    /// Stats per node, parallel to the component's `nodes`
    nodes: Vec<NodeStats>,
    /// Calculated CPU percentage per process
    cpu_percentages: HashMap<ProcKey, f32>,
}

impl ProcessesData {
    /// Only one sample so far: CPU % needs two, the next refresh comes soon
    fn cpu_pending(&self) -> bool {
        self.cpu_percentages.is_empty() && !self.processes.is_empty()
    }

    fn total_memory(&self) -> u64 {
        self.nodes.iter().map(|n| n.total_memory).sum()
    }

    fn cpu_count(&self) -> usize {
        self.nodes.iter().map(|n| n.cpu_count).sum()
    }

    /// Memory usage across all nodes (from meminfo, not the process sum)
    fn memory_usage_percent(&self) -> f32 {
        let total = self.total_memory();
        if total == 0 {
            return 0.0;
        }
        let used: u64 = self.nodes.iter().map(|n| n.memory_used).sum();
        used as f32 / total as f32 * 100.0
    }

    /// CPU usage across nodes, weighted by core count; `None` until sampled
    fn cpu_usage_percent(&self) -> Option<f32> {
        let sampled = self
            .nodes
            .iter()
            .filter_map(|n| n.cpu_usage_percent.map(|pct| (pct, n.cpu_count.max(1))));
        let (weighted, cores) = sampled.fold((0.0, 0), |(w, c), (pct, count)| {
            (w + pct * count as f32, c + count)
        });
        (cores > 0).then(|| weighted / cores as f32)
    }

    fn cpu_percent(&self, proc: &NodeProcess) -> f32 {
        self.cpu_percentages
            .get(&proc.key())
            .copied()
            .unwrap_or(0.0)
    }
}

/// One rendered row of the process table
struct ProcessRow {
    pid: i32,
    node: String,
    cpu: String,
    mem: String,
    state: String,
    command: String,
    cpu_color: Color,
    mem_color: Color,
    state_color: Color,
    interesting: bool,
}

/// What one node returned in a refresh
struct NodeFetch {
    processes: std::result::Result<Vec<ProcessInfo>, String>,
    meminfo: Option<MemInfo>,
    cpu_stat: Option<CpuStat>,
    cpu_count: Option<usize>,
    load_avg: Option<(f64, f64, f64)>,
}

/// Fetch processes and system stats of one node, each call bounded
async fn fetch_node(client: TalosClient, basics: Option<NodeBasics>) -> NodeFetch {
    let t = NODE_FETCH_TIMEOUT;
    // What the dashboard already has isn't asked again
    let ask = basics.is_none();
    let (procs, mem, stat, cpu, load) = tokio::join!(
        tokio::time::timeout(t, client.processes()),
        async {
            if ask {
                Some(tokio::time::timeout(t, client.memory()).await)
            } else {
                None
            }
        },
        tokio::time::timeout(t, client.system_stat()),
        async {
            if ask {
                Some(tokio::time::timeout(t, client.cpu_info()).await)
            } else {
                None
            }
        },
        async {
            if ask {
                Some(tokio::time::timeout(t, client.load_avg()).await)
            } else {
                None
            }
        },
    );
    let (mem, cpu, load) = (
        mem.and_then(Result::ok),
        cpu.and_then(Result::ok),
        load.and_then(Result::ok),
    );
    NodeFetch {
        processes: match procs {
            Ok(Ok(nodes)) => Ok(nodes
                .into_iter()
                .next()
                .map(|n| n.processes)
                .unwrap_or_default()),
            Ok(Err(e)) => Err(format!("Failed to fetch processes: {e}")),
            Err(_) => Err(format!("Request timed out after {}s", t.as_secs())),
        },
        meminfo: mem
            .and_then(std::result::Result::ok)
            .and_then(|m| m.into_iter().next())
            .and_then(|m| m.meminfo),
        cpu_stat: stat
            .ok()
            .and_then(std::result::Result::ok)
            .and_then(|s| s.into_iter().next())
            .map(|s| s.cpu_total),
        cpu_count: cpu
            .and_then(std::result::Result::ok)
            .and_then(|c| c.into_iter().next())
            .map(|c| c.cpu_count),
        load_avg: load
            .and_then(std::result::Result::ok)
            .and_then(|l| l.into_iter().next())
            .map(|l| (l.load1, l.load5, l.load15)),
    }
}

/// Processes component for viewing node processes
#[derive(Clone)]
pub struct ProcessesComponent {
    /// What is being viewed: a node name, or a cluster / group name
    title: String,
    /// Nodes whose processes are listed
    nodes: Vec<ProcessNode>,
    /// What the dashboard knows of each node (parallel to `nodes`)
    basics: Vec<Option<NodeBasics>>,

    /// Async state wrapping all process data
    state: AsyncState<ProcessesData>,

    /// Selected index in filtered list
    selected: usize,
    /// Table state for rendering
    table_state: TableState,
    /// Where the rows were drawn, for clicks
    rows: crate::ui_ext::RowHits,
    /// Current sort order
    sort_by: SortBy,
    /// Tree view enabled
    tree_view: bool,
    /// Tree root process (None = full tree from init, Some = subtree)
    tree_root: Option<ProcKey>,

    /// State filter (None = show all, Some(state) = show only that state)
    state_filter: Option<ProcessState>,

    /// Current mode
    mode: Mode,
    /// Filter input text
    filter_input: String,
    /// Active filter (applied)
    filter: Option<String>,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls; each node is targeted with `with_node`
    client: Option<TalosClient>,
}

impl Default for ProcessesComponent {
    fn default() -> Self {
        Self::new(String::new(), Vec::new())
    }
}

impl ProcessesComponent {
    /// Memory, cores and load the dashboard has for each node (by hostname),
    /// taken instead of asking the node; set before each refresh
    pub fn set_node_basics(&mut self, basics: impl Fn(&str) -> Option<NodeBasics>) {
        self.basics = self.nodes.iter().map(|n| basics(&n.hostname)).collect();
    }

    /// Sort order, remembered between visits and runs
    pub fn sort_by(&self) -> SortBy {
        self.sort_by
    }

    pub fn set_sort_by(&mut self, sort_by: SortBy) {
        self.sort_by = sort_by;
    }

    pub fn new(title: String, nodes: Vec<ProcessNode>) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        let mut state = AsyncState::new();
        state.start_loading();

        Self {
            title,
            basics: Vec::new(),
            nodes,
            state,
            selected: 0,
            table_state,
            rows: Default::default(),
            sort_by: SortBy::CpuPercent,
            tree_view: false,
            tree_root: None,
            state_filter: None,
            mode: Mode::Normal,
            filter_input: String::new(),
            filter: None,
            auto_refresh: true,
            client: None,
        }
    }

    /// Set the client for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Whether processes of more than one node are listed
    fn multi_node(&self) -> bool {
        self.nodes.len() > 1
    }

    fn node_name(&self, idx: usize) -> &str {
        self.nodes.get(idx).map_or("?", |n| n.hostname.as_str())
    }

    /// Helper to get data reference
    fn data(&self) -> Option<&ProcessesData> {
        self.state.data()
    }

    /// Run `f` on the loaded data with read access to the component's
    /// settings (sort order, filters, tree mode)
    fn with_data(&mut self, f: impl FnOnce(&Self, &mut ProcessesData)) {
        let Some(slot) = self.state.data_mut() else {
            return;
        };
        let mut data = std::mem::take(slot);
        f(self, &mut data);
        if let Some(slot) = self.state.data_mut() {
            *slot = data;
        }
    }

    /// Re-sort and re-filter after a setting changed
    fn resort(&mut self) {
        self.with_data(|this, data| {
            this.sort_processes_in_data(data);
            this.apply_filter_to_data(data);
        });
    }

    /// Re-filter after a filter changed
    fn apply_filter(&mut self) {
        self.with_data(ProcessesComponent::apply_filter_to_data);
    }

    /// Rebuild the flat/tree display list after the tree mode changed
    fn rebuild_display_list(&mut self) {
        self.with_data(ProcessesComponent::rebuild_display_list_into);
    }

    /// Refresh process data from every node concurrently
    pub async fn refresh(&mut self) -> Result<()> {
        let Some(client) = self.client.clone() else {
            self.set_error("No client configured".to_string());
            return Ok(());
        };

        self.state.start_loading();

        // Keep previous data: CPU deltas need the last sample, and a node that
        // fails this time keeps showing its last known processes.
        let mut data = self.state.take_data().unwrap_or_default();
        data.nodes.resize_with(self.nodes.len(), NodeStats::default);

        let started = Instant::now();
        let fetches = self.nodes.iter().enumerate().map(|(i, n)| {
            let basics = self.basics.get(i).cloned().flatten();
            fetch_node(client.with_node(&n.address), basics)
        });
        let results = futures::future::join_all(fetches).await;
        tracing::debug!(
            "processes: fetched {} node(s) in {:?}, {} failed",
            results.len(),
            started.elapsed(),
            results.iter().filter(|r| r.processes.is_err()).count()
        );

        let mut previous: Vec<NodeProcess> = std::mem::take(&mut data.processes);
        let mut cpu_percentages = HashMap::new();
        let now = Instant::now();

        for (idx, mut fetch) in results.into_iter().enumerate() {
            if let Some(basics) = self.basics.get(idx).cloned().flatten() {
                fetch.meminfo.get_or_insert(basics.meminfo);
                fetch.cpu_count.get_or_insert(basics.cpu_count);
                fetch.load_avg.get_or_insert(basics.load);
            }
            let Some(stats) = data.nodes.get_mut(idx) else {
                continue;
            };
            if let Some(meminfo) = fetch.meminfo {
                stats.total_memory = meminfo.mem_total;
                stats.memory_used = meminfo.mem_total.saturating_sub(meminfo.mem_available);
            }
            if let Some(count) = fetch.cpu_count {
                stats.cpu_count = count;
            }
            if let Some(load) = fetch.load_avg {
                stats.load_avg = load;
            }
            if let Some(curr) = fetch.cpu_stat {
                if let Some(prev) = &stats.prev_cpu_stat {
                    stats.cpu_usage_percent = Some(CpuStat::usage_percent_from(prev, &curr));
                }
                stats.prev_cpu_stat = Some(curr);
            }

            match fetch.processes {
                Ok(procs) => {
                    stats.error = None;
                    let elapsed = stats
                        .last_cpu_sample
                        .map(|t| now.duration_since(t).as_secs_f64())
                        .unwrap_or(0.0);
                    if stats.cpu_count > 0 {
                        for (pid, pct) in
                            process_cpu_percents(&stats.prev_cpu_times, &procs, elapsed)
                        {
                            cpu_percentages.insert((idx, pid), pct);
                        }
                    }
                    stats.prev_cpu_times = procs.iter().map(|p| (p.pid, p.cpu_time)).collect();
                    stats.last_cpu_sample = Some(now);
                    data.processes.extend(
                        procs
                            .into_iter()
                            .map(|info| NodeProcess { node: idx, info }),
                    );
                }
                Err(e) => {
                    let e = if self.multi_node() {
                        e
                    } else {
                        format!(
                            "{} (node: {})",
                            e,
                            self.nodes.get(idx).map_or("?", |n| n.address.as_str())
                        )
                    };
                    stats.error = Some(e);
                    // Keep this node's last known processes and CPU figures
                    for p in previous.extract_if(.., |p| p.node == idx) {
                        if let Some(&pct) = data.cpu_percentages.get(&p.key()) {
                            cpu_percentages.insert(p.key(), pct);
                        }
                        data.processes.push(p);
                    }
                }
            }
        }
        data.cpu_percentages = cpu_percentages;

        Self::calculate_state_counts_into(&mut data);
        self.sort_processes_in_data(&mut data);
        self.apply_filter_to_data(&mut data);

        // Reset selection if needed
        if !data.display_entries.is_empty() && self.selected >= data.display_entries.len() {
            self.selected = 0;
        }
        self.table_state.select(Some(self.selected));

        // Every node failed: report it, keeping the last data on screen
        let errors: Vec<String> = data
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| n.error.as_ref().map(|e| (i, e)))
            .map(|(i, e)| {
                if self.multi_node() {
                    format!("{}: {}", self.node_name(i), e)
                } else {
                    e.clone()
                }
            })
            .collect();
        let all_failed = !self.nodes.is_empty() && errors.len() == self.nodes.len();
        self.state.set_data(data);
        if all_failed {
            self.state.set_error(errors.join("; "));
            return Ok(());
        }
        Ok(())
    }

    /// Calculate state counts from processes (static method)
    fn calculate_state_counts_into(data: &mut ProcessesData) {
        data.state_counts = StateCounts::default();
        for proc in &data.processes {
            match proc.state {
                ProcessState::Running => data.state_counts.running += 1,
                ProcessState::Sleeping => data.state_counts.sleeping += 1,
                ProcessState::DiskSleep => data.state_counts.disk_wait += 1,
                ProcessState::Zombie => data.state_counts.zombie += 1,
                _ => {}
            }
        }
    }

    /// Sort processes based on current sort order (operates on data)
    fn sort_processes_in_data(&self, data: &mut ProcessesData) {
        match self.sort_by {
            // Until CPU % is known, CPU time puts the busy processes first
            SortBy::CpuPercent if data.cpu_pending() => {
                data.processes.sort_by(|a, b| {
                    b.cpu_time
                        .partial_cmp(&a.cpu_time)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortBy::CpuPercent => {
                let cpu_pcts = &data.cpu_percentages;
                let pct = |p: &NodeProcess| cpu_pcts.get(&p.key()).copied().unwrap_or(0.0);
                data.processes.sort_by(|a, b| {
                    pct(b)
                        .partial_cmp(&pct(a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortBy::CpuTime => {
                data.processes.sort_by(|a, b| {
                    b.cpu_time
                        .partial_cmp(&a.cpu_time)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortBy::Mem => {
                data.processes
                    .sort_by_key(|p| std::cmp::Reverse(p.resident_memory));
            }
        }
    }

    /// Apply current filter to processes (operates on data)
    fn apply_filter_to_data(&self, data: &mut ProcessesData) {
        let filter_lower = self.filter.as_ref().map(|f| f.to_lowercase());
        data.filtered_indices = data
            .processes
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                // Text filter: command, arguments, executable or node name
                let text_match = filter_lower.as_ref().is_none_or(|f| {
                    p.command.to_lowercase().contains(f)
                        || p.args.to_lowercase().contains(f)
                        || p.executable.to_lowercase().contains(f)
                        || (self.multi_node() && self.node_name(p.node).to_lowercase().contains(f))
                });

                // State filter
                let state_match = self.state_filter.as_ref().is_none_or(|state_filter| {
                    std::mem::discriminant(&p.state) == std::mem::discriminant(state_filter)
                });

                text_match && state_match
            })
            .map(|(i, _)| i)
            .collect();
        // Rebuild display list after filtering
        self.rebuild_display_list_into(data);
    }

    /// Rebuild the display list (either flat or tree view) - operates on data
    fn rebuild_display_list_into(&self, data: &mut ProcessesData) {
        if !self.tree_view {
            // Flat view - just use filtered indices directly
            data.display_entries = data
                .filtered_indices
                .iter()
                .map(|&idx| DisplayEntry {
                    process_idx: idx,
                    depth: 0,
                    is_last: false,
                    ancestors_have_siblings: vec![],
                })
                .collect();
            return;
        }

        // Tree view - build parent-child hierarchy, per node
        let key_to_idx: HashMap<ProcKey, usize> = data
            .processes
            .iter()
            .enumerate()
            .map(|(idx, p)| (p.key(), idx))
            .collect();
        let filtered: HashSet<usize> = data.filtered_indices.iter().copied().collect();

        let mut children_map: HashMap<ProcKey, Vec<usize>> = HashMap::new();
        let mut root_indices: Vec<usize> = Vec::new();

        // Determine which processes are descendants of tree_root (if set)
        let descendants: Option<HashSet<ProcKey>> = self.tree_root.map(|root| {
            let mut desc = HashSet::from([root]);
            // Keep adding children until no more found
            let mut changed = true;
            while changed {
                changed = false;
                for proc in &data.processes {
                    if desc.contains(&proc.parent_key()) && desc.insert(proc.key()) {
                        changed = true;
                    }
                }
            }
            desc
        });

        for &idx in &data.filtered_indices {
            let Some(proc) = data.processes.get(idx) else {
                continue;
            };

            // If subtree mode, skip processes not in the subtree
            if let Some(ref desc) = descendants
                && !desc.contains(&proc.key())
            {
                continue;
            }

            let parent = proc.parent_key();

            // Check if parent exists in our filtered list
            let parent_in_list = key_to_idx
                .get(&parent)
                .is_some_and(|parent_idx| filtered.contains(parent_idx));

            // In subtree mode, root is the tree_root process
            let is_root = if let Some(root) = self.tree_root {
                proc.key() == root
            } else {
                proc.ppid == 0 || proc.ppid == proc.pid || !parent_in_list
            };

            if !is_root
                && parent_in_list
                && descendants.as_ref().is_none_or(|d| d.contains(&parent))
            {
                children_map.entry(parent).or_default().push(idx);
            } else {
                root_indices.push(idx);
            }
        }

        // A ppid cycle has no root, so nothing in it would be shown: cut the
        // edge into its first unreached member and make that a root.
        fn reach(
            stack: &mut Vec<usize>,
            reached: &mut HashSet<usize>,
            processes: &[NodeProcess],
            children_map: &HashMap<ProcKey, Vec<usize>>,
        ) {
            while let Some(i) = stack.pop() {
                if reached.insert(i)
                    && let Some(process) = processes.get(i)
                    && let Some(children) = children_map.get(&process.key())
                {
                    stack.extend(children);
                }
            }
        }
        let mut reached = HashSet::new();
        let mut stack = root_indices.clone();
        reach(&mut stack, &mut reached, &data.processes, &children_map);
        for &idx in &data.filtered_indices {
            let Some(parent) = data.processes.get(idx).map(NodeProcess::parent_key) else {
                continue;
            };
            let is_child = children_map.get(&parent).is_some_and(|c| c.contains(&idx));
            if is_child && !reached.contains(&idx) {
                if let Some(c) = children_map.get_mut(&parent) {
                    c.retain(|&i| i != idx);
                }
                root_indices.push(idx);
                stack.push(idx);
                reach(&mut stack, &mut reached, &data.processes, &children_map);
            }
        }

        // Build display list recursively
        data.display_entries.clear();

        fn add_tree_entries(
            entries: &mut Vec<DisplayEntry>,
            processes: &[NodeProcess],
            children_map: &HashMap<ProcKey, Vec<usize>>,
            indices: &[usize],
            depth: usize,
            ancestors_have_siblings: Vec<bool>,
        ) {
            let count = indices.len();
            for (i, &idx) in indices.iter().enumerate() {
                let is_last = i == count - 1;

                entries.push(DisplayEntry {
                    process_idx: idx,
                    depth,
                    is_last,
                    ancestors_have_siblings: ancestors_have_siblings.clone(),
                });

                // Process children
                if let Some(children) = processes.get(idx).and_then(|p| children_map.get(&p.key()))
                {
                    let mut child_ancestors = ancestors_have_siblings.clone();
                    child_ancestors.push(!is_last);
                    add_tree_entries(
                        entries,
                        processes,
                        children_map,
                        children,
                        depth + 1,
                        child_ancestors,
                    );
                }
            }
        }

        add_tree_entries(
            &mut data.display_entries,
            &data.processes,
            &children_map,
            &root_indices,
            0,
            vec![],
        );
    }

    /// Get currently selected process
    fn selected_process(&self) -> Option<&NodeProcess> {
        self.data().and_then(|data| {
            data.display_entries
                .get(self.selected)
                .and_then(|entry| data.processes.get(entry.process_idx))
        })
    }

    /// Move the process cursor
    fn move_selection(&mut self, step: crate::ui_ext::Move) {
        let count = self.data().map(|d| d.display_entries.len()).unwrap_or(0);
        if let Some(i) = crate::ui_ext::moved(self.selected, count, step) {
            self.selected = i;
            self.table_state.select(Some(i));
        }
    }

    /// Back to the top of the list after the list changed shape
    fn reset_selection(&mut self) {
        self.selected = 0;
        self.table_state.select(Some(0));
    }

    /// Get color for process state
    fn state_color(state: &ProcessState) -> Color {
        match state {
            ProcessState::Running => Color::Green,
            ProcessState::Zombie => Color::Red,
            ProcessState::DiskSleep => Color::Yellow,
            ProcessState::Stopped => Color::Magenta,
            _ => Color::default(),
        }
    }

    /// Load average color relative to the node's core count, with the
    /// thresholds the dashboard and diagnostics use
    fn load_color(load: f64, cpu_count: usize) -> Color {
        use crate::ui_ext::HealthIndicatorExt;
        talos9s_core::load_health(load, Some(cpu_count)).color()
    }

    /// Draw the header
    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let data = match self.data() {
            Some(d) => d,
            None => return,
        };

        let sort_indicator = format!("[{}▼]", self.sort_by.label());
        let tree_indicator = if self.tree_view {
            if let Some(root) = self.tree_root {
                // Find root process name
                let root_name = data
                    .processes
                    .iter()
                    .find(|p| p.key() == root)
                    .map(|p| p.command.as_str())
                    .unwrap_or("?");
                format!("[TREE:{}/{}]", root.1, root_name)
            } else {
                "[TREE]".to_string()
            }
        } else {
            String::new()
        };
        let proc_count = format!("{} procs", data.display_entries.len());

        // System resources info (summed over nodes)
        let cpu_count = data.cpu_count();
        let cpu_info = if cpu_count > 0 {
            format!("{cpu_count} CPU")
        } else {
            String::new()
        };
        let total_memory = data.total_memory();
        let mem_info = if total_memory > 0 {
            format!("{} RAM", format_bytes_compact(total_memory))
        } else {
            String::new()
        };

        let mut spans = vec![
            Span::styled("Processes: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&self.title),
        ];
        match self.nodes.as_slice() {
            [node] => {
                spans.push(Span::styled(" (", Style::default().fg(Color::DarkGray)));
                spans.push(Span::raw(&node.address));
                spans.push(Span::styled(")", Style::default().fg(Color::DarkGray)));
            }
            nodes => spans.push(Span::styled(
                format!(" · {} nodes", nodes.len()),
                Style::default().fg(Color::DarkGray),
            )),
        }

        // Add system info
        if !cpu_info.is_empty() || !mem_info.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled("[", Style::default().fg(Color::DarkGray)));
            if !cpu_info.is_empty() {
                spans.push(Span::styled(cpu_info, Style::default().fg(Color::Cyan)));
            }
            if cpu_count > 0 && total_memory > 0 {
                spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            }
            if !mem_info.is_empty() {
                spans.push(Span::styled(mem_info, Style::default().fg(Color::Magenta)));
            }
            spans.push(Span::styled("]", Style::default().fg(Color::DarkGray)));
        }

        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            sort_indicator,
            Style::default().fg(Color::Cyan),
        ));

        if self.tree_view {
            spans.push(Span::raw(" "));
            let tree_color = if self.tree_root.is_some() {
                Color::Cyan
            } else {
                Color::Green
            };
            spans.push(Span::styled(
                tree_indicator,
                Style::default().fg(tree_color),
            ));
        }

        // State filter indicator
        if let Some(ref state_filter) = self.state_filter {
            spans.push(Span::raw(" "));
            let (label, color) = match state_filter {
                ProcessState::Zombie => ("ZOMBIE", Color::Red),
                ProcessState::DiskSleep => ("DISK-WAIT", Color::Yellow),
                _ => ("FILTER", Color::White),
            };
            spans.push(Span::styled(
                format!("[{label}]"),
                Style::default().fg(color),
            ));
        }

        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            proc_count,
            Style::default().fg(Color::DarkGray),
        ));

        let line = Line::from(spans);
        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }

    /// Generate a usage bar (10 chars) based on percentage
    fn usage_bar(percent: f32) -> (String, Color) {
        let filled = usize::from(
            talos9s_core::convert::float_to_u16(f64::from(percent / 10.0).round()).min(10),
        );
        let empty = 10 - filled;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
        // The dashboard's thresholds, so a node looks the same in both
        use crate::ui_ext::HealthIndicatorExt;
        let color = talos9s_core::memory_health(f64::from(percent)).color();
        (bar, color)
    }

    /// Draw the summary bar with CPU/MEM bars and state counts
    fn draw_summary_bar(&self, frame: &mut Frame, area: Rect) {
        let data = match self.data() {
            Some(d) => d,
            None => return,
        };

        // CPU bar (shows "--%" until two samples exist, since it needs a delta)
        let cpu_usage = data.cpu_usage_percent();
        let (cpu_bar, cpu_color) = Self::usage_bar(cpu_usage.unwrap_or(0.0));
        let cpu_pct = match cpu_usage {
            Some(pct) => format!("{pct:>3.0}%"),
            None => " --%".to_string(),
        };

        // Memory bar with actual percentage
        let mem_usage = data.memory_usage_percent();
        let (mem_bar, mem_color) = Self::usage_bar(mem_usage);
        let mem_pct = format!("{mem_usage:>3.0}%");

        let mut spans = vec![
            Span::raw("CPU "),
            Span::styled(cpu_bar, Style::default().fg(cpu_color)),
            Span::raw(format!(" {cpu_pct} ")),
            Span::raw("  MEM "),
            Span::styled(mem_bar, Style::default().fg(mem_color)),
            Span::raw(format!(" {mem_pct} ")),
            Span::raw("  "),
            Span::styled("Load: ", Style::default().add_modifier(Modifier::DIM)),
        ];

        // Load average: 1/5/15 min for one node, the 1 min load per node for
        // several (colored against each node's own core count)
        if let [stats] = data.nodes.as_slice() {
            let loads = [stats.load_avg.0, stats.load_avg.1, stats.load_avg.2];
            for load in loads {
                spans.push(Span::styled(
                    format!("{load:.2}"),
                    Style::default().fg(Self::load_color(load, stats.cpu_count)),
                ));
                spans.push(Span::raw(" "));
            }
        } else {
            for (idx, stats) in data.nodes.iter().enumerate() {
                spans.push(Span::styled(
                    format!("{} ", self.node_name(idx)),
                    Style::default().add_modifier(Modifier::DIM),
                ));
                spans.push(Span::styled(
                    format!("{:.2}", stats.load_avg.0),
                    Style::default().fg(Self::load_color(stats.load_avg.0, stats.cpu_count)),
                ));
                spans.push(Span::raw(" "));
            }
        }
        spans.push(Span::raw(" "));

        // Add state counts with colors
        spans.push(Span::styled(
            format!("R:{}", data.state_counts.running),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("S:{}", data.state_counts.sleeping),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("D:{}", data.state_counts.disk_wait),
            Style::default().fg(if data.state_counts.disk_wait > 0 {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("Z:{}", data.state_counts.zombie),
            Style::default().fg(if data.state_counts.zombie > 0 {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ));

        let line = Line::from(spans);
        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }

    /// Warnings worth a banner: zombies, high memory, nodes that failed
    fn warnings(&self) -> Vec<String> {
        let Some(data) = self.data() else {
            return Vec::new();
        };
        let mut warnings = Vec::new();

        // Check for zombie processes
        if data.state_counts.zombie > 0 {
            warnings.push(format!(
                "{} zombie process(es) detected",
                data.state_counts.zombie
            ));
        }

        // Node memory past the shared warning threshold (from meminfo), else
        // a single process using that much of its node's memory
        let prefix = |idx: usize| {
            if self.multi_node() {
                format!("{}: ", self.node_name(idx))
            } else {
                String::new()
            }
        };
        for (idx, stats) in data.nodes.iter().enumerate() {
            let usage = stats.memory_usage_percent();
            if talos9s_core::memory_health(f64::from(usage))
                != talos9s_core::HealthIndicator::Healthy
            {
                warnings.push(format!("{}High memory usage: {:.1}%", prefix(idx), usage));
            } else if stats.total_memory > 0
                && let Some(proc) = data.processes.iter().find(|p| {
                    p.node == idx
                        && p.resident_memory as f64 / stats.total_memory as f64 * 100.0
                            > talos9s_core::constants::MEMORY_WARN_PERCENT
                })
            {
                warnings.push(format!(
                    "{}Process '{}' using {:.1}% memory",
                    prefix(idx),
                    proc.command,
                    proc.resident_memory as f64 / stats.total_memory as f64 * 100.0
                ));
            }
        }

        // Nodes whose last fetch failed (when all failed, the error screen shows it)
        if self.multi_node() && self.state.error().is_none() {
            for (idx, stats) in data.nodes.iter().enumerate() {
                if let Some(e) = &stats.error {
                    warnings.push(format!(
                        "{}: {} (showing last known)",
                        self.node_name(idx),
                        e
                    ));
                }
            }
        }
        warnings
    }

    /// Draw warning banner
    fn draw_warning(&self, frame: &mut Frame, area: Rect, warnings: &[String]) {
        let warning_text = format!("⚠ {}", warnings.join(" • "));
        let para = Paragraph::new(warning_text).style(Style::default().fg(Color::Yellow));
        frame.render_widget(para, area);
    }

    /// Collect process row data for rendering (helper to limit borrow scope)
    fn collect_process_row_data(&self, area: Rect) -> Option<Vec<ProcessRow>> {
        let data = self.data()?;
        let tree_view = self.tree_view;
        let node_width = if self.multi_node() {
            self.node_column_width() + 1
        } else {
            0
        };
        let max_cmd_len = area.width.saturating_sub(45 + node_width) as usize;
        let max_mem = data
            .processes
            .iter()
            .map(|p| p.resident_memory)
            .max()
            .unwrap_or(0);

        Some(
            data.display_entries
                .iter()
                .filter_map(|entry| {
                    let proc = data.processes.get(entry.process_idx)?;

                    let cpu_pct = data.cpu_percent(proc);
                    let cpu_color = if cpu_pct > 50.0 {
                        Color::Red
                    } else if cpu_pct > 10.0 {
                        Color::Yellow
                    } else if cpu_pct > 0.1 {
                        Color::Green
                    } else {
                        Color::default()
                    };

                    let mem_color = if max_mem == 0 {
                        Color::default()
                    } else {
                        let ratio = proc.resident_memory as f64 / max_mem as f64;
                        if ratio > 0.7 {
                            Color::Red
                        } else if ratio > 0.3 {
                            Color::Yellow
                        } else {
                            Color::default()
                        }
                    };

                    let command = proc.display_command();

                    let tree_prefix = if !tree_view || entry.depth == 0 {
                        String::new()
                    } else {
                        let mut prefix = String::new();
                        for &has_sibling in &entry.ancestors_have_siblings {
                            if has_sibling {
                                prefix.push_str("│ ");
                            } else {
                                prefix.push_str("  ");
                            }
                        }
                        if entry.is_last {
                            prefix.push_str("└─");
                        } else {
                            prefix.push_str("├─");
                        }
                        prefix
                    };

                    let full_command = format!("{tree_prefix}{command}");
                    let command = if full_command.chars().count() > max_cmd_len {
                        let keep = max_cmd_len.saturating_sub(3);
                        format!("{}...", full_command.chars().take(keep).collect::<String>())
                    } else {
                        full_command
                    };

                    Some(ProcessRow {
                        pid: proc.pid,
                        node: self.node_name(proc.node).to_string(),
                        cpu: if data.cpu_pending() {
                            format!("{:>6}  {}", "…", proc.cpu_time_human())
                        } else {
                            format!("{:>5.1}%  {}", cpu_pct, proc.cpu_time_human())
                        },
                        mem: proc.resident_memory_human(),
                        state: proc.state.short().to_string(),
                        command,
                        cpu_color,
                        mem_color,
                        state_color: Self::state_color(&proc.state),
                        interesting: matches!(
                            proc.state,
                            ProcessState::Running | ProcessState::Zombie | ProcessState::DiskSleep
                        ),
                    })
                })
                .collect(),
        )
    }

    /// Width of the NODE column: the longest node name
    fn node_column_width(&self) -> u16 {
        self.nodes
            .iter()
            .map(|n| n.hostname.chars().count())
            .max()
            .unwrap_or(0)
            .max(4)
            .try_into()
            .unwrap_or(u16::MAX)
    }

    /// Draw the process table
    fn draw_process_table(&mut self, frame: &mut Frame, area: Rect) {
        // Collect row data using helper (borrow ends when helper returns)
        let row_data = match self.collect_process_row_data(area) {
            Some(data) => data,
            None => return,
        };
        let multi = self.multi_node();

        let rows: Vec<Row> = row_data
            .into_iter()
            .map(|row| {
                // Highlight interesting processes (running, zombie, disk-wait) with bold
                let base_style = if row.interesting {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let mut cells = vec![Cell::from(format!("{:>6}", row.pid)).style(base_style)];
                if multi {
                    cells.push(Cell::from(row.node).style(base_style.fg(Color::Cyan)));
                }
                cells.extend([
                    Cell::from(row.cpu).style(base_style.fg(row.cpu_color)),
                    Cell::from(format!("{:>8}", row.mem)).style(base_style.fg(row.mem_color)),
                    Cell::from(row.state).style(base_style.fg(row.state_color)),
                    Cell::from(row.command).style(base_style),
                ]);
                Row::new(cells)
            })
            .collect();

        // Build header with sort indicators
        let cpu_header = match self.sort_by {
            SortBy::CpuPercent => "CPU%▼  TIME",
            SortBy::CpuTime => "CPU%   TIME▼",
            _ => "CPU%   TIME",
        };
        let mem_header = if self.sort_by == SortBy::Mem {
            "MEM▼"
        } else {
            "MEM"
        };

        let mut header = vec![Cell::from("PID")];
        let mut widths = vec![Constraint::Length(7)]; // PID
        if multi {
            header.push(Cell::from("NODE"));
            widths.push(Constraint::Length(self.node_column_width()));
        }
        header.extend([
            Cell::from(cpu_header),
            Cell::from(mem_header),
            Cell::from("S"),
            Cell::from("COMMAND"),
        ]);
        widths.extend([
            Constraint::Length(14),     // CPU% + TIME
            Constraint::Length(9),      // MEM
            Constraint::Length(2),      // State
            Constraint::Percentage(55), // Command
        ]);
        let header = Row::new(header)
            .style(Style::default().add_modifier(Modifier::DIM))
            .bottom_margin(1);

        let row_count = rows.len();
        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(table, area, &mut self.table_state);
        self.rows =
            crate::ui_ext::RowHits::new(area, false, 2, self.table_state.offset(), row_count);
    }

    /// Draw the detail section for selected process
    fn draw_detail_section(&self, frame: &mut Frame, area: Rect) {
        let Some(proc) = self.selected_process() else {
            return;
        };
        let Some(data) = self.data() else {
            return;
        };

        let title = if self.multi_node() {
            format!(
                " {} (PID {}) on {} ",
                proc.command,
                proc.pid,
                self.node_name(proc.node)
            )
        } else {
            format!(" {} (PID {}) ", proc.command, proc.pid)
        };

        // Full command line with wrapping
        let full_cmd = proc.display_command();

        // Find parent process name (on the same node)
        let parent_name = data
            .processes
            .iter()
            .find(|p| p.key() == proc.parent_key())
            .map(|p| p.command.as_str())
            .unwrap_or("-");

        // Split area: command takes most space, stats at bottom
        let inner_area = {
            let block = Block::default().title(title.clone()).borders(Borders::TOP);
            block.inner(area)
        };

        let [cmd_area, stats_area] = Layout::vertical([
            Constraint::Min(3),    // Command (wrapped)
            Constraint::Length(2), // Stats lines (now 2 lines)
        ])
        .areas(inner_area);

        // Draw the block border
        let block = Block::default().title(title).borders(Borders::TOP);
        frame.render_widget(block, area);

        // Command paragraph with wrapping
        let cmd_para = Paragraph::new(full_cmd)
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false });
        frame.render_widget(cmd_para, cmd_area);

        // Stats lines
        let stats_line1 = Line::from(vec![
            Span::styled("State: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                proc.state.description(),
                Style::default().fg(Self::state_color(&proc.state)),
            ),
            Span::raw("  "),
            Span::styled("PPID: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(format!("{} ", proc.ppid)),
            Span::styled("(", Style::default().fg(Color::DarkGray)),
            Span::styled(parent_name, Style::default().fg(Color::Cyan)),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled("Threads: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(proc.threads.to_string()),
        ]);
        let stats_line2 = Line::from(vec![
            Span::styled("Virt: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(proc.virtual_memory_human()),
            Span::raw("  "),
            Span::styled("Res: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(proc.resident_memory_human()),
            Span::raw("  "),
            Span::styled("[y] yank", Style::default().fg(Color::DarkGray)),
        ]);
        let stats_para = Paragraph::new(vec![stats_line1, stats_line2]);
        frame.render_widget(stats_para, stats_area);
    }

    /// Draw the footer with keybindings
    fn key_hints_line(&self) -> Line<'static> {
        if self.mode == Mode::Filtering {
            return Line::from(vec![
                Span::raw(" Type to filter  "),
                Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" apply  "),
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" clear"),
            ]);
        }
        let auto_status = if self.auto_refresh {
            Span::styled("ON ", Style::default().fg(Color::Green))
        } else {
            Span::styled("OFF", Style::default().fg(Color::DarkGray))
        };

        let tree_status = if self.tree_view {
            if self.tree_root.is_some() {
                Span::styled("SUB", Style::default().fg(Color::Cyan))
            } else {
                Span::styled("ALL", Style::default().fg(Color::Green))
            }
        } else {
            Span::styled("OFF", Style::default().fg(Color::DarkGray))
        };

        // State filter status
        let filter_status = match &self.state_filter {
            Some(ProcessState::Zombie) => Span::styled(" [Z]", Style::default().fg(Color::Red)),
            Some(ProcessState::DiskSleep) => {
                Span::styled(" [D]", Style::default().fg(Color::Yellow))
            }
            _ => Span::raw(""),
        };

        Line::from(vec![
            Span::styled("[1]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cpu "),
            Span::styled("[2]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" mem "),
            Span::styled("[t]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" subtree "),
            Span::styled("[T]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" tree:"),
            tree_status,
            Span::raw(" "),
            Span::styled("[z]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" zombie "),
            Span::styled("[d]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" disk"),
            filter_status,
            Span::raw(" "),
            Span::styled("[/]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" filter "),
            Span::styled("[a]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" auto:"),
            auto_status,
        ])
    }

    /// Draw the filter input bar
    fn draw_filter_bar(&self, frame: &mut Frame, area: Rect) {
        let (match_count, total) = self
            .data()
            .map(|d| (d.display_entries.len(), d.processes.len()))
            .unwrap_or((0, 0));

        let line = Line::from(vec![
            Span::styled("Filter: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&self.filter_input),
            Span::styled("█", Style::default().fg(Color::Cyan)), // Cursor
            Span::raw("  "),
            Span::styled(
                format!("[{match_count}/{total}]"),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let para = Paragraph::new(line);
        frame.render_widget(para, area);
    }
}

impl Component for ProcessesComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if let Some(i) = self.rows.clicked(&mouse) {
            self.selected = i;
            self.table_state.select(Some(i));
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn captures_text(&self) -> bool {
        self.mode == Mode::Filtering
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Filtering => self.handle_filter_key(key),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Check for auto-refresh using AsyncState
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
            // The first screen shows at once with CPU % pending; the second
            // sample follows shortly, whatever auto-refresh says
            if self.data().is_some_and(ProcessesData::cpu_pending)
                && !self.state.is_loading()
                && self
                    .state
                    .elapsed_since_refresh()
                    .is_some_and(|t| t >= FIRST_SAMPLE_GAP)
            {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Check loading state
        if self.state.is_loading() && !self.state.has_data() {
            let loading =
                Paragraph::new("Loading processes...").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, area);
            return Ok(());
        }

        if let Some(err) = self.state.error() {
            let error =
                Paragraph::new(format!("Error: {err}")).style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return Ok(());
        }

        // Calculate layout - check for any warning condition
        let warnings = self.warnings();
        let has_warning = !warnings.is_empty();
        let warning_height = if has_warning { 1 } else { 0 };

        let [
            header_area,
            summary_area,
            warning_area,
            filter_area,
            table_area,
            detail_area,
        ] = Layout::vertical([
            Constraint::Length(1),              // Header
            Constraint::Length(1),              // Summary bar
            Constraint::Length(warning_height), // Warning (if any)
            Constraint::Length(1),              // Filter bar (in filter mode) or spacer
            Constraint::Min(5),                 // Process table
            Constraint::Length(7),              // Detail section (increased for wrapped cmd)
        ])
        .areas(area);

        self.draw_header(frame, header_area);
        self.draw_summary_bar(frame, summary_area);

        if has_warning {
            self.draw_warning(frame, warning_area, &warnings);
        }

        // Filter bar or spacer
        if self.mode == Mode::Filtering || self.filter.is_some() {
            self.draw_filter_bar(frame, filter_area);
        }

        self.draw_process_table(frame, table_area);
        self.draw_detail_section(frame, detail_area);

        Ok(())
    }
}

impl ProcessesComponent {
    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(crate::ui_ext::Move::Next);
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(crate::ui_ext::Move::Prev);
                Ok(None)
            }
            KeyCode::Char('g') => {
                self.move_selection(crate::ui_ext::Move::First);
                Ok(None)
            }
            KeyCode::Char('G') => {
                self.move_selection(crate::ui_ext::Move::Last);
                Ok(None)
            }
            KeyCode::Char('1') => {
                // Toggle between CPU% and CPU time
                self.sort_by = match self.sort_by {
                    SortBy::CpuPercent => SortBy::CpuTime,
                    SortBy::CpuTime => SortBy::CpuPercent,
                    _ => SortBy::CpuPercent,
                };
                self.resort();
                self.reset_selection();
                Ok(None)
            }
            KeyCode::Char('2') => {
                self.sort_by = SortBy::Mem;
                self.resort();
                self.reset_selection();
                Ok(None)
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Filtering;
                self.filter_input = self.filter.clone().unwrap_or_default();
                Ok(None)
            }
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
                Ok(None)
            }
            KeyCode::Char('t') => {
                // Toggle subtree from selected process
                if let Some(key) = self.selected_process().map(NodeProcess::key) {
                    if self.tree_view && self.tree_root == Some(key) {
                        // Already in subtree mode for this process - clear it
                        self.tree_view = false;
                        self.tree_root = None;
                    } else {
                        // Enter subtree mode for selected process
                        self.tree_root = Some(key);
                        self.tree_view = true;
                    }
                    self.rebuild_display_list();
                    self.reset_selection();
                }
                Ok(None)
            }
            KeyCode::Char('T') => {
                // Full tree from init (pid 1)
                self.tree_root = None;
                self.tree_view = !self.tree_view;
                self.rebuild_display_list();
                self.reset_selection();
                Ok(None)
            }
            KeyCode::Char('z') => {
                // Toggle zombie filter
                if self.state_filter == Some(ProcessState::Zombie) {
                    self.state_filter = None;
                } else {
                    self.state_filter = Some(ProcessState::Zombie);
                }
                self.apply_filter();
                self.reset_selection();
                Ok(None)
            }
            KeyCode::Char('d') => {
                // Toggle disk-wait filter
                if self.state_filter == Some(ProcessState::DiskSleep) {
                    self.state_filter = None;
                } else {
                    self.state_filter = Some(ProcessState::DiskSleep);
                }
                self.apply_filter();
                self.reset_selection();
                Ok(None)
            }
            KeyCode::Char('y') => {
                // Yank (copy) the selected process command to clipboard
                if let Some(proc) = self.selected_process() {
                    let full_cmd = proc.display_command().to_string();
                    // By characters: 50 bytes can end inside a multi-byte one
                    let preview: String = full_cmd.chars().take(50).collect();
                    if crate::clipboard::copy_to_clipboard(full_cmd).is_ok() {
                        tracing::info!("Copied to clipboard: {}...", preview);
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        use crate::ui_ext::{TextEdit, edit_text};
        match edit_text(&mut self.filter_input, &key) {
            TextEdit::Cancel => {
                self.mode = Mode::Normal;
                self.filter_input.clear();
                self.filter = None;
                self.apply_filter();
                self.reset_selection();
            }
            // Live filter as you type; Enter keeps it and stops typing
            TextEdit::Changed | TextEdit::Submit => {
                self.filter = (!self.filter_input.is_empty()).then(|| self.filter_input.clone());
                self.apply_filter();
                if key.code == KeyCode::Enter {
                    self.mode = Mode::Normal;
                    self.reset_selection();
                }
            }
            TextEdit::Ignored => {}
        }
        Ok(None)
    }
}

impl crate::components::BackgroundRefresh for ProcessesComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        self.state = refreshed.state;
        // Sort order, filters or tree mode may have changed while it ran
        self.resort();
        let len = self.data().map_or(0, |d| d.display_entries.len());
        if self.selected >= len {
            self.selected = len.saturating_sub(1);
        }
        self.table_state.select(Some(self.selected));
    }
}

/// Per-process CPU % from the CPU time delta since the previous sample.
/// Not divided by the core count, so (like htop) a process can exceed
/// 100%. A reused PID has less CPU time than its predecessor: clamp to 0.
fn process_cpu_percents(
    prev_cpu_times: &HashMap<i32, f64>,
    procs: &[ProcessInfo],
    elapsed: f64,
) -> Vec<(i32, f32)> {
    if elapsed <= 0.1 {
        return Vec::new();
    }
    procs
        .iter()
        .filter_map(|p| {
            let prev = prev_cpu_times.get(&p.pid)?;
            // f64 to f32 rounds to nearest (inf past the range): fine for a percentage
            #[allow(clippy::cast_possible_truncation)]
            let pct = ((p.cpu_time - prev) / elapsed * 100.0) as f32;
            Some((p.pid, pct.max(0.0)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `a` toggles auto-refresh, as in every view that has it
    #[test]
    fn a_toggles_auto_refresh() {
        use crate::components::Component;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut c = ProcessesComponent::new("t".into(), Vec::new());
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(c.auto_refresh);
        c.handle_key_event(a).unwrap();
        assert!(!c.auto_refresh);
        c.handle_key_event(a).unwrap();
        assert!(c.auto_refresh);
    }

    fn process(pid: i32, ppid: i32, command: &str, mem: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            state: ProcessState::Sleeping,
            threads: 1,
            cpu_time: 0.0,
            virtual_memory: 0,
            resident_memory: mem,
            command: command.to_string(),
            executable: String::new(),
            args: String::new(),
        }
    }

    /// Two nodes, both with PID 1 and PID 2: keys keep them apart
    fn two_nodes() -> ProcessesComponent {
        let node = |name: &str| ProcessNode {
            hostname: name.to_string(),
            address: String::new(),
        };
        let mut comp = ProcessesComponent::new("test".into(), vec![node("n1"), node("n2")]);
        let procs = [
            (0, process(1, 0, "init", 10)),
            (0, process(2, 1, "kubelet", 300)),
            (1, process(1, 0, "init", 20)),
            (1, process(2, 1, "etcd", 200)),
        ];
        comp.state.set_data(ProcessesData {
            processes: procs
                .into_iter()
                .map(|(node, info)| NodeProcess { node, info })
                .collect(),
            nodes: vec![NodeStats::default(), NodeStats::default()],
            ..Default::default()
        });
        comp.sort_by = SortBy::Mem;
        comp.resort();
        comp
    }

    fn listed(comp: &ProcessesComponent) -> Vec<(String, i32, usize)> {
        let data = comp.data().unwrap();
        data.display_entries
            .iter()
            .map(|e| {
                let p = &data.processes[e.process_idx];
                (comp.node_name(p.node).to_string(), p.pid, e.depth)
            })
            .collect()
    }

    #[test]
    fn merges_nodes_and_sorts_across_them() {
        let comp = two_nodes();
        assert_eq!(
            listed(&comp),
            vec![
                ("n1".into(), 2, 0),
                ("n2".into(), 2, 0),
                ("n2".into(), 1, 0),
                ("n1".into(), 1, 0),
            ]
        );
    }

    #[test]
    fn tree_keeps_each_node_separate() {
        let mut comp = two_nodes();
        comp.tree_view = true;
        comp.rebuild_display_list();
        // Each init has only its own node's child, despite equal PIDs
        assert_eq!(
            listed(&comp),
            vec![
                ("n2".into(), 1, 0),
                ("n2".into(), 2, 1),
                ("n1".into(), 1, 0),
                ("n1".into(), 2, 1),
            ]
        );

        comp.tree_root = Some((1, 1));
        comp.rebuild_display_list();
        assert_eq!(
            listed(&comp),
            vec![("n2".into(), 1, 0), ("n2".into(), 2, 1)]
        );
    }

    #[test]
    fn filter_matches_node_name() {
        let mut comp = two_nodes();
        comp.filter = Some("N2".into());
        comp.apply_filter();
        assert!(listed(&comp).iter().all(|(node, _, _)| node == "n2"));
        assert_eq!(listed(&comp).len(), 2);
    }

    #[test]
    fn cpu_percent_survives_pid_reuse_and_short_intervals() {
        let with_time = |pid, t| ProcessInfo {
            cpu_time: t,
            ..process(pid, 1, "p", 0)
        };
        let prev = HashMap::from([(10, 50.0), (11, 1.0)]);
        // PID 10 was reused by a fresh process: its CPU time went backwards
        let procs = [with_time(10, 0.2), with_time(11, 2.0), with_time(12, 9.0)];
        let mut pcts = process_cpu_percents(&prev, &procs, 2.0);
        pcts.sort_by_key(|(pid, _)| *pid);
        assert_eq!(pcts, vec![(10, 0.0), (11, 50.0)]);
        // Too short a window would blow small deltas up to huge numbers
        assert!(process_cpu_percents(&prev, &procs, 0.05).is_empty());
    }

    fn one_node(procs: Vec<ProcessInfo>) -> ProcessesComponent {
        let node = ProcessNode {
            hostname: "n1".to_string(),
            address: String::new(),
        };
        let mut comp = ProcessesComponent::new("test".into(), vec![node]);
        comp.state.set_data(ProcessesData {
            processes: procs
                .into_iter()
                .map(|info| NodeProcess { node: 0, info })
                .collect(),
            nodes: vec![NodeStats::default()],
            ..Default::default()
        });
        comp.sort_by = SortBy::Mem;
        comp.tree_view = true;
        comp.resort();
        comp
    }

    #[test]
    fn tree_shows_orphans_as_roots() {
        // PID 7's parent is not in the list (it exited between reads)
        let comp = one_node(vec![
            process(1, 0, "init", 20),
            process(7, 99, "orphan", 10),
            process(8, 7, "child", 5),
        ]);
        let rows: Vec<(i32, usize)> = listed(&comp).into_iter().map(|(_, p, d)| (p, d)).collect();
        assert_eq!(rows, vec![(1, 0), (7, 0), (8, 1)]);
    }

    #[test]
    fn tree_with_a_ppid_cycle_shows_every_process_once() {
        // A snapshot taken across PID reuse can make two processes each
        // other's parent; neither is then a root
        let comp = one_node(vec![
            process(1, 0, "init", 30),
            process(5, 6, "a", 20),
            process(6, 5, "b", 10),
            process(7, 7, "self", 5),
        ]);
        let mut pids: Vec<i32> = listed(&comp).into_iter().map(|(_, p, _)| p).collect();
        pids.sort();
        assert_eq!(pids, vec![1, 5, 6, 7]);
    }
}
