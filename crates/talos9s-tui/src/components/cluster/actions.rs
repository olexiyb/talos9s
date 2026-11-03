//! The view each menu entry opens for the current selection

use super::*;

impl ClusterComponent {
    /// Processes view for the selection: the selected node, or every online
    /// node under the selected cluster / control plane / workers header
    pub(super) fn processes_action(&self) -> Option<Action> {
        let (title, nodes) = self.online_nodes()?;
        Some(Action::ShowProcesses(title, nodes))
    }

    /// Containers view for the selection, the same nodes as Processes
    pub(super) fn containers_action(&self) -> Option<Action> {
        let (title, nodes) = self.online_nodes()?;
        Some(Action::ShowContainers(title, nodes))
    }

    /// Operations for the selection: every node under it, offline ones too
    /// (uncordon a node that was shut down)
    pub(super) fn operations_action(&self) -> Option<Action> {
        let (cluster_idx, names) = self.selected_nodes()?;
        let cluster = self.clusters.get(cluster_idx)?;
        let nodes: Vec<crate::operations::OpNode> = names
            .into_iter()
            .map(|name| crate::operations::OpNode {
                address: cluster
                    .node_ips
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| name.clone()),
                // The tree's role, not "runs etcd now": an offline control
                // plane node runs nothing
                is_controlplane: cluster.is_controlplane(&name),
                hostname: name,
            })
            .collect();
        let (title, group) = match &self.selected_item {
            NodeListItem::ClusterHeader(_) => (cluster.name.clone(), true),
            NodeListItem::ControlPlaneHeader(_) => {
                (format!("{} control plane", cluster.name), true)
            }
            NodeListItem::WorkersHeader(_) => (format!("{} workers", cluster.name), true),
            _ => (nodes.first()?.hostname.clone(), false),
        };
        (!nodes.is_empty()).then_some(Action::ShowOperations(title, nodes, group))
    }

    /// The selected node, or the online nodes under the selected header,
    /// with the title for them; none when there are none
    fn online_nodes(&self) -> Option<(String, Vec<ProcessNode>)> {
        let (cluster_idx, names) = self.selected_nodes()?;
        let cluster = self.clusters.get(cluster_idx)?;
        let nodes: Vec<ProcessNode> = names
            .into_iter()
            .filter(|name| !cluster.offline.contains_key(name))
            .map(|name| ProcessNode {
                address: cluster
                    .node_ips
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| name.clone()),
                hostname: name,
            })
            .collect();
        let title = match &self.selected_item {
            NodeListItem::ClusterHeader(_) => cluster.name.clone(),
            NodeListItem::ControlPlaneHeader(_) => format!("{} control plane", cluster.name),
            NodeListItem::WorkersHeader(_) => format!("{} workers", cluster.name),
            _ => nodes.first()?.hostname.clone(),
        };
        (!nodes.is_empty()).then_some((title, nodes))
    }

    /// Online nodes of the selected header as group view nodes, with the
    /// header's title; `None` on a single node or an empty group
    pub(super) fn group_selection(&self) -> Option<(String, Vec<GroupNode>, Option<String>)> {
        let title = |cluster: &ClusterData| match &self.selected_item {
            NodeListItem::ClusterHeader(_) => Some(cluster.name.clone()),
            NodeListItem::ControlPlaneHeader(_) => Some(format!("{} control plane", cluster.name)),
            NodeListItem::WorkersHeader(_) => Some(format!("{} workers", cluster.name)),
            _ => None,
        };
        let (cluster_idx, names) = self.selected_nodes()?;
        let cluster = self.clusters.get(cluster_idx)?;
        let title = title(cluster)?;
        let address = |name: &String| {
            cluster
                .node_ips
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone())
        };
        let nodes: Vec<GroupNode> = names
            .iter()
            .filter(|name| !cluster.offline.contains_key(*name))
            .map(|name| GroupNode {
                address: address(name),
                role: if cluster.is_controlplane(name) {
                    "controlplane"
                } else {
                    "worker"
                }
                .to_string(),
                hostname: name.clone(),
            })
            .collect();
        // Workers fetch their kubeconfig from a control plane
        let cp_endpoint = self
            .controlplane_nodes_for(cluster_idx)
            .first()
            .map(|(_, v)| address(&v.node));
        (!nodes.is_empty()).then_some((title, nodes, cp_endpoint))
    }

    /// The selected node and its address
    pub(super) fn selected_node_address(&self) -> Option<(String, String)> {
        let node_name = self.current_node_name()?;
        let node_ip = self
            .node_ips()
            .get(&node_name)
            .cloned()
            .unwrap_or(node_name.clone());
        Some((node_name, node_ip))
    }

    /// Network view of the selected node, or of every node under a header
    pub(super) fn network_action(&self) -> Option<Action> {
        if let Some((name, ip)) = self.selected_node_address() {
            return Some(Action::ShowNetwork(name, ip));
        }
        let (title, nodes, cp) = self.group_selection()?;
        Some(Action::ShowGroup(GroupKind::Network, title, nodes, cp))
    }

    /// KubeSpan peers of the selected node, or the peer matrix of every node
    /// under a header
    pub(super) fn kubespan_action(&self) -> Option<Action> {
        if let Some((name, ip)) = self.selected_node_address() {
            return Some(Action::ShowKubeSpan(name, ip));
        }
        let (_, names) = self.selected_nodes()?;
        (!names.is_empty()).then_some(Action::ShowKubeSpanMatrix(names))
    }

    /// Containers, machine config or extensions of the selected node
    pub(super) fn node_info_action(&self, tab: NodeInfoTab) -> Option<Action> {
        let (name, ip) = self.selected_node_address()?;
        Some(Action::ShowNodeInfo(name, ip, tab))
    }

    /// Storage view of the selected node, or of every node under a header
    pub(super) fn storage_action(&self, mode: StorageViewMode) -> Option<Action> {
        if let Some((name, ip)) = self.selected_node_address() {
            return Some(Action::ShowStorage(name, ip, mode));
        }
        let (title, nodes, cp) = self.group_selection()?;
        let kind = match mode {
            StorageViewMode::Disks => GroupKind::Storage,
            StorageViewMode::Partitions => GroupKind::Partitions,
        };
        Some(Action::ShowGroup(kind, title, nodes, cp))
    }

    /// Diagnostics of the selected node, or of every node under a header
    pub(super) fn diagnostics_action(&self) -> Option<Action> {
        if let Some((name, ip)) = self.selected_node_address() {
            let role = self.current_node_role();
            // For worker nodes, provide a control plane endpoint to fetch kubeconfig from
            let cp_endpoint = if role == "worker" {
                self.get_controlplane_endpoint()
            } else {
                None
            };
            return Some(Action::ShowDiagnostics(name, ip, role, cp_endpoint));
        }
        let (title, nodes, cp) = self.group_selection()?;
        Some(Action::ShowGroup(GroupKind::Diagnostics, title, nodes, cp))
    }

    /// Workloads view for the selection: the whole cluster for a cluster
    /// header, otherwise only the pods on the selected node(s). Offline nodes
    /// stay in scope, since their pods are the ones most likely in trouble.
    pub(super) fn workloads_action(&self) -> Action {
        let selected = self
            .selected_nodes()
            .and_then(|(idx, names)| Some((self.clusters.get(idx)?, names)));
        match (selected, &self.selected_item) {
            (Some((cluster, _)), NodeListItem::ClusterHeader(_)) => {
                Action::ShowWorkloads(cluster.name.clone(), None)
            }
            (Some((cluster, names)), item) if !names.is_empty() => {
                let title = match item {
                    NodeListItem::ControlPlaneHeader(_) => {
                        format!("{} control plane", cluster.name)
                    }
                    NodeListItem::WorkersHeader(_) => format!("{} workers", cluster.name),
                    _ => names.first().cloned().unwrap_or_default(),
                };
                Action::ShowWorkloads(title, Some(names))
            }
            _ => Action::ShowWorkloads(
                self.current_context_name().unwrap_or("cluster").to_string(),
                None,
            ),
        }
    }

    /// Volumes view for the selection, scoped like `workloads_action`
    pub(super) fn volumes_action(&self) -> Action {
        match self.workloads_action() {
            Action::ShowWorkloads(title, nodes) => Action::ShowVolumes(title, nodes),
            other => other,
        }
    }

    /// Logs view for the selection: one node's services, or the nodes of a
    /// cluster / control plane / workers group streamed together. A group
    /// starts with etcd (control plane) or kubelet; the rest are a toggle
    /// away, and the kernel log is offered too.
    pub(super) fn logs_action(&self, only_service: Option<String>) -> Option<Action> {
        let (cluster_idx, names) = self.selected_nodes()?;
        let cluster = self.clusters.get(cluster_idx)?;
        let nodes: Vec<LogNode> = names
            .iter()
            .map(|name| LogNode {
                name: name.clone(),
                ip: cluster
                    .node_ips
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone()),
                services: self
                    .get_node_services_for(cluster_idx, name)
                    .map(|services| services.iter().map(|s| s.id.clone()).collect()),
                color: Some(self.node_color(cluster_idx, name)),
            })
            .collect();
        // Services of the nodes, in first-seen order
        let mut all_services: Vec<String> = Vec::new();
        for service in nodes.iter().flat_map(|n| n.services.iter().flatten()) {
            if !all_services.contains(service) {
                all_services.push(service.clone());
            }
        }

        let single = matches!(
            self.selected_item,
            NodeListItem::ControlPlaneNode(..) | NodeListItem::WorkerNode(..)
        );
        let (title, subtitle, active) = if single {
            if all_services.is_empty() {
                return None;
            }
            let node = nodes.first()?;
            let active = only_service.map_or_else(|| all_services.clone(), |s| vec![s]);
            (node.ip.clone(), self.current_node_role(), active)
        } else {
            if nodes.is_empty() {
                return None;
            }
            let title = match self.selected_item {
                NodeListItem::ControlPlaneHeader(_) => format!("{} control plane", cluster.name),
                NodeListItem::WorkersHeader(_) => format!("{} workers", cluster.name),
                _ => cluster.name.clone(),
            };
            let control_plane = matches!(self.selected_item, NodeListItem::ControlPlaneHeader(_));
            let preferred = if control_plane { "etcd" } else { "kubelet" };
            let first = if all_services.iter().any(|s| s == preferred) {
                preferred.to_string()
            } else {
                all_services
                    .first()
                    .cloned()
                    .unwrap_or_else(|| KERNEL_SERVICE.to_string())
            };
            (title, format!("{} nodes", nodes.len()), vec![first])
        };
        all_services.push(KERNEL_SERVICE.to_string());
        Some(Action::ShowMultiLogs(
            title,
            subtitle,
            nodes,
            active,
            all_services,
        ))
    }

    /// Navigate to the currently selected menu item (1-based index, 0 = on node)
    pub(super) fn navigate_to_selected_menu(&self) -> Option<Action> {
        let item = self
            .selected_menu_item
            .checked_sub(1)
            .and_then(|i| NavMenuItem::ALL.get(i))?;
        self.action_for(*item)
    }

    /// The view a menu item shows for the current selection. Views of a
    /// single node give a hint on a group instead of nothing.
    pub fn action_for(&self, item: NavMenuItem) -> Option<Action> {
        let node_only = |tab| {
            self.node_info_action(tab).or_else(|| {
                Some(Action::ShowHint(
                    item,
                    format!("Select a node to see its {}", item.label().to_lowercase()),
                ))
            })
        };
        // Cluster-wide views (Workloads, etcd, ...) don't wait for the nodes,
        // but they need the connection: while it's being made, the same
        // hint, so the app opens them once connected instead of showing
        // "Not connected" for good. The dashboard shows the connecting state
        // itself.
        if item != NavMenuItem::Dashboard && self.client().is_none() && self.nodes_pending() {
            return Some(Action::ShowHint(item, self.nodes_pending_hint()));
        }
        let action = match item {
            NavMenuItem::Dashboard => Some(Action::ShowDashboard),
            NavMenuItem::Logs => self.logs_action(None),
            NavMenuItem::Etcd => Some(Action::ShowEtcd),
            NavMenuItem::Network => self.network_action(),
            NavMenuItem::KubeSpan => self.kubespan_action(),
            NavMenuItem::Disks => self.storage_action(StorageViewMode::Disks),
            NavMenuItem::Partitions => self.storage_action(StorageViewMode::Partitions),
            NavMenuItem::Processes => self.processes_action(),
            NavMenuItem::Diagnostics => self.diagnostics_action(),
            NavMenuItem::Certs => Some(Action::ShowSecurity),
            NavMenuItem::Lifecycle => Some(Action::ShowLifecycle),
            NavMenuItem::Workloads => Some(self.workloads_action()),
            NavMenuItem::Volumes => Some(self.volumes_action()),
            NavMenuItem::Containers => self.containers_action(),
            NavMenuItem::Operations => self.operations_action(),
            NavMenuItem::Config => node_only(NodeInfoTab::Config),
            NavMenuItem::Extensions => node_only(NodeInfoTab::Extensions),
        };
        // Picked before the cluster's nodes are known (e.g. right after
        // start): say so; the app opens the view once they arrive
        action.or_else(|| {
            self.nodes_pending()
                .then(|| Action::ShowHint(item, self.nodes_pending_hint()))
        })
    }
}
