//! The node tree: its rows, selection, folding and navigation

use super::*;

impl ClusterComponent {
    /// Talos address of every node in the active cluster, by node name
    pub fn active_node_ips(&self) -> HashMap<String, String> {
        self.node_ips().clone()
    }

    /// Whether a node is a control plane node (see `ClusterData::is_controlplane`)
    pub(super) fn node_is_controlplane(&self, cluster_idx: usize, node: &str) -> bool {
        self.clusters
            .get(cluster_idx)
            .is_some_and(|c| c.is_controlplane(node))
    }

    /// Whether `node` of the active cluster is a control plane node; `None`
    /// without an active cluster
    pub fn is_controlplane(&self, node: &str) -> Option<bool> {
        self.clusters
            .get(self.active_cluster)
            .map(|c| c.is_controlplane(node))
    }

    /// How long a node has not been answering, or `None` when it is online
    pub(super) fn node_offline_for(
        &self,
        cluster_idx: usize,
        node: &str,
    ) -> Option<std::time::Duration> {
        self.clusters
            .get(cluster_idx)?
            .offline
            .get(node)
            .map(|o| o.since.elapsed())
    }

    /// Tree row for a node: health symbol and color, plus an "offline" note
    pub(super) fn node_row_status(
        &self,
        cluster_idx: usize,
        node: &str,
    ) -> (&'static str, Color, String) {
        if let Some(down) = self.node_offline_for(cluster_idx, node) {
            return (
                "○",
                Color::Red,
                format!("  offline {}", format_uptime(down.as_secs())),
            );
        }
        // Listed but not answered yet: unknown, not healthy
        if self.get_node_memory_for(cluster_idx, node).is_none()
            && self.get_node_services_for(cluster_idx, node).is_none()
        {
            return ("●", Color::DarkGray, String::new());
        }
        let mem_pct = self
            .get_node_memory_for(cluster_idx, node)
            .map(|m| f64::from(m.usage_percent()));
        let services = self.get_node_services_for(cluster_idx, node);
        if node_row_ok(mem_pct, services.map(Vec::as_slice)) {
            ("●", Color::Green, String::new())
        } else {
            ("◐", Color::Yellow, String::new())
        }
    }

    /// The node's colour: its place in the cluster's tree (control plane,
    /// then workers), so it stays the same whichever group is selected
    pub(crate) fn node_color(&self, cluster_idx: usize, node: &str) -> Color {
        let position = self
            .controlplane_nodes_for(cluster_idx)
            .into_iter()
            .chain(self.worker_nodes_for(cluster_idx))
            .position(|(_, v)| v.node == node)
            .unwrap_or(0);
        crate::components::multi_logs::node_color(position)
    }

    /// Get control plane nodes for a cluster (discovery machineType, else etcd service)
    pub(super) fn controlplane_nodes_for(&self, cluster_idx: usize) -> Vec<(usize, &VersionInfo)> {
        let Some(cluster) = self.clusters.get(cluster_idx) else {
            return Vec::new();
        };
        cluster
            .versions
            .iter()
            .enumerate()
            .filter(|(_, v)| self.node_is_controlplane(cluster_idx, &v.node))
            .collect()
    }

    /// Get worker nodes for a cluster (every node that isn't a control plane node)
    pub(super) fn worker_nodes_for(&self, cluster_idx: usize) -> Vec<(usize, &VersionInfo)> {
        let Some(cluster) = self.clusters.get(cluster_idx) else {
            return Vec::new();
        };
        cluster
            .versions
            .iter()
            .enumerate()
            .filter(|(_, v)| !self.node_is_controlplane(cluster_idx, &v.node))
            .collect()
    }

    /// Build the visible list of items based on expand/collapse state
    pub(super) fn visible_items(&self) -> Vec<NodeListItem> {
        let mut items = Vec::new();

        for (cluster_idx, cluster) in self.clusters.iter().enumerate() {
            // Cluster header
            items.push(NodeListItem::ClusterHeader(cluster_idx));

            if cluster.expanded {
                let cp_nodes = self.controlplane_nodes_for(cluster_idx);
                let worker_nodes = self.worker_nodes_for(cluster_idx);

                // Control plane section
                if !cp_nodes.is_empty() {
                    items.push(NodeListItem::ControlPlaneHeader(cluster_idx));
                    if cluster.controlplane_expanded {
                        for (i, _) in cp_nodes.iter().enumerate() {
                            items.push(NodeListItem::ControlPlaneNode(cluster_idx, i));
                        }
                    }
                }

                // Workers section
                if !worker_nodes.is_empty() {
                    items.push(NodeListItem::WorkersHeader(cluster_idx));
                    if cluster.workers_expanded {
                        for (i, _) in worker_nodes.iter().enumerate() {
                            items.push(NodeListItem::WorkerNode(cluster_idx, i));
                        }
                    }
                }
            }
        }

        items
    }

    /// Get the cluster index for the currently selected item
    pub(super) fn selected_cluster_index(&self) -> usize {
        match &self.selected_item {
            NodeListItem::ClusterHeader(idx) => *idx,
            NodeListItem::ControlPlaneHeader(idx) => *idx,
            NodeListItem::ControlPlaneNode(idx, _) => *idx,
            NodeListItem::WorkersHeader(idx) => *idx,
            NodeListItem::WorkerNode(idx, _) => *idx,
        }
    }

    /// Navigate to the next item in the node list
    pub(super) fn navigate_down(&mut self) {
        let items = self.visible_items();
        if items.is_empty() {
            return;
        }
        let current_pos = items
            .iter()
            .position(|i| i == &self.selected_item)
            .unwrap_or(0);
        let Some(next) = items.get(current_pos + 1).or(items.last()) else {
            return;
        };
        self.selected_item = next.clone();
        // Update active cluster
        self.active_cluster = self.selected_cluster_index();
    }

    /// Navigate to the previous item in the node list
    pub(super) fn navigate_up(&mut self) {
        let items = self.visible_items();
        if items.is_empty() {
            return;
        }
        let current_pos = items
            .iter()
            .position(|i| i == &self.selected_item)
            .unwrap_or(0);
        let Some(prev) = items.get(current_pos.saturating_sub(1)) else {
            return;
        };
        self.selected_item = prev.clone();
        // Update active cluster
        self.active_cluster = self.selected_cluster_index();
    }

    /// Toggle expand/collapse of the currently selected group header
    pub(super) fn toggle_expand(&mut self) {
        match &self.selected_item {
            NodeListItem::ClusterHeader(idx) => {
                if let Some(cluster) = self.clusters.get_mut(*idx) {
                    cluster.expanded = !cluster.expanded;
                }
            }
            NodeListItem::ControlPlaneHeader(idx) => {
                if let Some(cluster) = self.clusters.get_mut(*idx) {
                    cluster.controlplane_expanded = !cluster.controlplane_expanded;
                }
            }
            NodeListItem::WorkersHeader(idx) => {
                if let Some(cluster) = self.clusters.get_mut(*idx) {
                    cluster.workers_expanded = !cluster.workers_expanded;
                }
            }
            _ => {} // No action for node items
        }
    }

    /// Node names covered by the current selection: the selected node, or
    /// every node under the selected cluster / control plane / workers header.
    pub(super) fn selected_nodes(&self) -> Option<(usize, Vec<String>)> {
        let names = |nodes: Vec<(usize, &VersionInfo)>| -> Vec<String> {
            nodes.into_iter().map(|(_, v)| v.node.clone()).collect()
        };
        match &self.selected_item {
            NodeListItem::ClusterHeader(idx) => {
                let mut nodes = names(self.controlplane_nodes_for(*idx));
                nodes.extend(names(self.worker_nodes_for(*idx)));
                Some((*idx, nodes))
            }
            NodeListItem::ControlPlaneHeader(idx) => {
                Some((*idx, names(self.controlplane_nodes_for(*idx))))
            }
            NodeListItem::WorkersHeader(idx) => Some((*idx, names(self.worker_nodes_for(*idx)))),
            NodeListItem::ControlPlaneNode(idx, _) | NodeListItem::WorkerNode(idx, _) => {
                Some((*idx, vec![self.current_node_name()?]))
            }
        }
    }
    /// Reset the selection to its cluster header if a refresh removed the
    /// selected node (e.g. it went down), so the cursor never points at nothing.
    pub(super) fn ensure_selection_visible(&mut self) {
        if !self.visible_items().contains(&self.selected_item) {
            self.selected_item = NodeListItem::ClusterHeader(self.active_cluster);
        }
    }

    /// Get the currently selected service ID
    pub fn selected_service_id(&self) -> Option<String> {
        let node_name = self.current_node_name()?;
        self.get_node_services(&node_name)
            .and_then(|services| services.get(self.selected_service))
            .map(|s| s.id.clone())
    }

    /// Get a reference to the client for the active cluster
    pub fn client(&self) -> Option<&TalosClient> {
        self.clusters.get(self.active_cluster)?.client.as_ref()
    }

    /// Best control plane node IP for the active cluster, used to fetch a
    /// cluster-correct kubeconfig (see [`k8s::create_k8s_client_with_source`]).
    ///
    /// Prefers an etcd member address (etcd only runs on control plane nodes, so
    /// this is unambiguously a control plane), then a discovery/roster member
    /// tagged `controlplane`, then the configured endpoint. `None` only when the
    /// cluster has no known address at all.
    pub fn control_plane_ip(&self) -> Option<String> {
        self.clusters.get(self.active_cluster)?.control_plane_ip()
    }

    /// Get context name for active cluster
    pub fn current_context_name(&self) -> Option<&str> {
        self.clusters
            .get(self.active_cluster)
            .map(|c| c.name.as_str())
    }

    /// Get config path
    pub fn config_path(&self) -> Option<&str> {
        self.config_path.as_deref()
    }

    /// Get node_ips for active cluster
    pub(super) fn node_ips(&self) -> &HashMap<String, String> {
        static EMPTY: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
        self.clusters
            .get(self.active_cluster)
            .map(|c| &c.node_ips)
            .unwrap_or_else(|| EMPTY.get_or_init(HashMap::new))
    }

    /// Get a control plane node IP from the active cluster
    /// Used to fetch kubeconfig when diagnosing worker nodes
    pub(super) fn get_controlplane_endpoint(&self) -> Option<String> {
        let cp_nodes = self.controlplane_nodes_for(self.active_cluster);
        if let Some((_, node)) = cp_nodes.first() {
            self.node_ips().get(&node.node).cloned()
        } else {
            None
        }
    }

    /// Get service count for current node
    pub(super) fn current_service_count(&self) -> usize {
        let Some(node_name) = self.current_node_name() else {
            return 0;
        };
        self.get_node_services(&node_name)
            .map(std::vec::Vec::len)
            .unwrap_or(0)
    }

    /// Get all service IDs for current node
    pub(super) fn current_service_ids(&self) -> Vec<String> {
        let Some(node_name) = self.current_node_name() else {
            return Vec::new();
        };
        self.get_node_services(&node_name)
            .map(|services| services.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default()
    }

    /// Get current node IP/name based on selected_item
    pub(super) fn current_node_name(&self) -> Option<String> {
        match &self.selected_item {
            NodeListItem::ControlPlaneNode(cluster_idx, node_idx) => self
                .controlplane_nodes_for(*cluster_idx)
                .get(*node_idx)
                .map(|(_, v)| v.node.clone()),
            NodeListItem::WorkerNode(cluster_idx, node_idx) => self
                .worker_nodes_for(*cluster_idx)
                .get(*node_idx)
                .map(|(_, v)| v.node.clone()),
            _ => None, // Headers don't have a node name
        }
    }

    /// Determine the selected node's role.
    ///
    /// The selection already reflects how the node was grouped (which now uses
    /// discovery machineType with an etcd-service fallback), so read that
    /// directly; only fall back to the service heuristic for non-node selections.
    pub(super) fn current_node_role(&self) -> String {
        match &self.selected_item {
            NodeListItem::ControlPlaneNode(..) => return "controlplane".to_string(),
            NodeListItem::WorkerNode(..) => return "worker".to_string(),
            _ => {}
        }
        let service_ids = self.current_service_ids();
        if service_ids.iter().any(|s| s == "etcd") {
            "controlplane".to_string()
        } else {
            "worker".to_string()
        }
    }

    /// The selected cluster is still connecting or has listed no nodes yet
    pub(super) fn nodes_pending(&self) -> bool {
        self.clusters
            .get(self.active_cluster)
            .is_some_and(|c| c.loading || c.versions.is_empty())
    }

    /// Identity of the selected tree row (cluster, group or node), for
    /// following the selection
    pub fn target_key(&self) -> String {
        let cluster = self.cluster_key();
        match &self.selected_item {
            NodeListItem::ClusterHeader(_) => cluster,
            NodeListItem::ControlPlaneHeader(_) => format!("{cluster}/control-plane"),
            NodeListItem::WorkersHeader(_) => format!("{cluster}/workers"),
            NodeListItem::ControlPlaneNode(..) | NodeListItem::WorkerNode(..) => {
                format!("{cluster}/{}", self.current_node_name().unwrap_or_default())
            }
        }
    }

    /// What the selection is, for a view's title: a node, a group or the
    /// cluster (always the cluster for cluster-wide views)
    pub fn target_title(&self, cluster_wide: bool) -> String {
        let cluster = self.cluster_key();
        if cluster_wide {
            return cluster;
        }
        match &self.selected_item {
            NodeListItem::ClusterHeader(_) => cluster,
            NodeListItem::ControlPlaneHeader(_) => format!("{cluster} control plane"),
            NodeListItem::WorkersHeader(_) => format!("{cluster} workers"),
            NodeListItem::ControlPlaneNode(..) | NodeListItem::WorkerNode(..) => {
                self.current_node_name().unwrap_or(cluster)
            }
        }
    }

    /// Name of the cluster the selection belongs to
    pub fn cluster_key(&self) -> String {
        let idx = self.selected_cluster_index();
        self.clusters
            .get(idx)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    }

    /// The current tree selection, to restore with [`restore_selection`]
    ///
    /// [`restore_selection`]: Self::restore_selection
    pub fn selection(&self) -> TreeSelection {
        TreeSelection(self.selected_item.clone())
    }

    /// Put back the tree as the last run left it: folds, auto-refresh and
    /// the selected cluster now, a group or node once its nodes are known.
    /// A remembered cluster wins over the talosconfig's current context.
    pub fn apply_saved(&mut self, state: &crate::state::UiState) {
        for folds in &state.tree {
            if let Some(c) = self.clusters.iter_mut().find(|c| c.name == folds.cluster) {
                c.expanded = folds.expanded;
                c.controlplane_expanded = folds.control_plane;
                c.workers_expanded = folds.workers;
            }
        }
        if let Some(auto) = state.auto_refresh {
            self.auto_refresh = auto;
        }
        let Some(selection) = &state.selection else {
            return;
        };
        let Some(idx) = self
            .clusters
            .iter()
            .position(|c| c.name == selection.cluster)
        else {
            return;
        };
        self.active_cluster = idx;
        self.selected_item = NodeListItem::ClusterHeader(idx);
        if let Some(cluster) = self.clusters.get_mut(idx) {
            cluster.expanded = true;
        }
        if selection.group.is_some() || selection.node.is_some() {
            self.pending_selection = Some(selection.clone());
        }
    }

    /// Select the remembered group or node once the cluster lists nodes;
    /// give up when the finished refresh doesn't have it
    pub(super) fn resolve_pending_selection(&mut self) {
        let Some(selection) = self.pending_selection.clone() else {
            return;
        };
        let Some(idx) = self
            .clusters
            .iter()
            .position(|c| c.name == selection.cluster)
        else {
            self.pending_selection = None;
            return;
        };
        let refreshing = self
            .background_refresh
            .get(idx)
            .is_some_and(std::option::Option::is_some);
        let Some(cluster) = self.clusters.get(idx) else {
            self.pending_selection = None;
            return;
        };
        if cluster.versions.is_empty() {
            if !refreshing && !cluster.loading {
                self.pending_selection = None;
            }
            return;
        }
        self.active_cluster = idx;
        let found = match (&selection.node, selection.group.as_deref()) {
            (Some(node), _) => self.select_node(node),
            (None, Some("control-plane")) if !self.controlplane_nodes_for(idx).is_empty() => {
                self.selected_item = NodeListItem::ControlPlaneHeader(idx);
                true
            }
            (None, Some("workers")) if !self.worker_nodes_for(idx).is_empty() => {
                self.selected_item = NodeListItem::WorkersHeader(idx);
                true
            }
            _ => false,
        };
        if found && let Some(cluster) = self.clusters.get_mut(idx) {
            cluster.expanded = true;
        }
        if found || !refreshing {
            self.pending_selection = None;
        }
    }

    /// The user moved the selection: forget the one remembered from the
    /// last run
    pub fn selection_moved(&mut self, before: &TreeSelection) {
        if self.selected_item != before.0 {
            self.pending_selection = None;
        }
    }

    /// The selected row by name, to remember (the remembered one while it
    /// is still waiting for the nodes)
    pub fn saved_selection(&self) -> Option<crate::state::Selection> {
        if let Some(pending) = &self.pending_selection {
            return Some(pending.clone());
        }
        let idx = self.selected_cluster_index();
        let cluster = self.clusters.get(idx)?.name.clone();
        let (group, node) = match &self.selected_item {
            NodeListItem::ClusterHeader(_) => (None, None),
            NodeListItem::ControlPlaneHeader(_) => (Some("control-plane".to_string()), None),
            NodeListItem::WorkersHeader(_) => (Some("workers".to_string()), None),
            NodeListItem::ControlPlaneNode(..) | NodeListItem::WorkerNode(..) => {
                (None, Some(self.current_node_name()?))
            }
        };
        Some(crate::state::Selection {
            cluster,
            group,
            node,
        })
    }

    /// Folds of every listed cluster, to remember
    pub fn saved_folds(&self) -> Vec<crate::state::Folds> {
        self.clusters
            .iter()
            .map(|c| crate::state::Folds {
                cluster: c.name.clone(),
                expanded: c.expanded,
                control_plane: c.controlplane_expanded,
                workers: c.workers_expanded,
            })
            .collect()
    }

    /// Memory, core count and load the dashboard has for a node of the
    /// active cluster, all three or nothing
    pub fn node_basics(&self, node: &str) -> Option<crate::components::processes::NodeBasics> {
        let idx = self.active_cluster;
        let meminfo = self.get_node_memory_for(idx, node)?.clone();
        let cpu_count = self.get_node_cpu_info_for(idx, node)?.cpu_count;
        let load = self.get_node_load_avg_for(idx, node)?;
        Some(crate::components::processes::NodeBasics {
            meminfo,
            cpu_count,
            load: (load.load1, load.load5, load.load15),
        })
    }

    /// A newer release found by the daily check, or none
    pub fn set_run_status(&mut self, status: Option<String>) {
        self.run_status = status;
    }

    pub fn set_newer_version(&mut self, version: Option<String>) {
        self.newer_version = version;
    }

    pub fn auto_refresh(&self) -> bool {
        self.auto_refresh
    }

    pub fn restore_selection(&mut self, selection: TreeSelection) {
        self.selected_item = selection.0;
        self.active_cluster = self.selected_cluster_index();
    }

    /// What Esc in the view returns to, shown in the key line (a group view
    /// a node was opened from)
    pub fn set_view_modal(&mut self, modal: bool) {
        self.view_modal = modal;
    }

    pub fn set_back_hint(&mut self, hint: Option<String>) {
        self.back_hint = hint;
    }

    /// Select the node named `name` in the active cluster (e.g. opened from
    /// a group view), unfolding its group; false when it isn't listed
    pub fn select_node(&mut self, name: &str) -> bool {
        let idx = self.active_cluster;
        if !self.reselect_node(idx, name) {
            return false;
        }
        let cp_group = matches!(self.selected_item, NodeListItem::ControlPlaneNode(..));
        if let Some(cluster) = self.clusters.get_mut(idx) {
            cluster.expanded = true;
            if cp_group {
                cluster.controlplane_expanded = true;
            } else {
                cluster.workers_expanded = true;
            }
        }
        true
    }

    /// Remember a diagnostics run's result for a node of the selected cluster
    pub fn set_diagnostics_summary(&mut self, node: String, summary: DiagnosticsSummary) {
        self.diagnostics_summaries
            .insert((self.cluster_key(), node), summary);
    }
}
