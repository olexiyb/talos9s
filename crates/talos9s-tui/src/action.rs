//! Actions for the talos9s TUI
//!
//! Actions represent events that can modify application state.

/// Actions that can be dispatched in the application
#[derive(Debug, Clone)]
pub enum Action {
    // Navigation
    Quit,
    Back,

    // View transitions
    /// Show logs of one or more nodes: (title, subtitle, nodes,
    /// active_service_ids, all_service_ids)
    ShowMultiLogs(
        String,
        String,
        Vec<crate::components::LogNode>,
        Vec<String>,
        Vec<String>,
    ),
    /// Show etcd cluster status
    ShowEtcd,
    /// Show processes for a node: (hostname, address)
    /// Processes of one or more nodes: (title, nodes)
    ShowProcesses(String, Vec<crate::components::ProcessNode>),
    /// Containers of the selected node, or every node under a header:
    /// (title, nodes)
    ShowContainers(String, Vec<crate::components::ProcessNode>),
    /// Show network stats for a node: (hostname, address)
    ShowNetwork(String, String),
    /// Show diagnostics for a node: (hostname, address, role, controlplane_endpoint)
    /// The controlplane_endpoint is used for fetching kubeconfig when diagnosing worker nodes
    ShowDiagnostics(String, String, String, Option<String>),
    /// Show security/certificates view
    ShowSecurity,
    /// Show lifecycle/version view
    ShowLifecycle,
    /// Show workload health view: (title, nodes to limit it to, or `None`
    /// for the whole cluster)
    ShowWorkloads(String, Option<Vec<String>>),
    /// Show volumes view (LINSTOR, ZFS LocalPV): (title, nodes to limit it to, or `None`
    /// for the whole cluster)
    ShowVolumes(String, Option<Vec<String>>),
    /// Show storage/disks view for a node: (hostname, address)
    /// Disks or partitions of a node: (hostname, address, tab)
    ShowStorage(String, String, crate::components::StorageViewMode),
    /// Diagnostics, storage or network of every node under a header:
    /// (kind, title, nodes, a control plane address for workers' kubeconfig)
    ShowGroup(
        crate::components::GroupKind,
        String,
        Vec<crate::components::GroupNode>,
        Option<String>,
    ),
    /// Machine config or extensions of a node: (hostname,
    /// address, tab to open)
    ShowNodeInfo(String, String, crate::components::NodeInfoTab),
    /// A node's KubeSpan peers: (hostname, address)
    ShowKubeSpan(String, String),
    /// The KubeSpan peer matrix of a group of nodes (by name)
    ShowKubeSpanMatrix(Vec<String>),
    /// The dashboard (cluster and node details) on the right
    ShowDashboard,
    /// A menu view that has nothing to show for the selection, with why
    ShowHint(crate::components::cluster::NavMenuItem, String),
    /// Move the keyboard from the tree into the view on the right
    FocusView,
    /// A view warped into namespaces and prefixes (empty: all), to remember
    /// per cluster
    WarpNamespace(Vec<String>),
    /// Suspend the TUI and open this file in the log viewer (`v`)
    OpenInViewer(std::path::PathBuf),
    /// Show node operations overlay: (hostname, address, is_controlplane)
    ShowNodeOperations(String, String, bool),
    /// Show rolling operations overlay with node list: Vec<(hostname, address, is_controlplane)>
    ShowRollingOperations(Vec<(String, String, bool)>),

    // UI state
    Tick,
    Resize(u16, u16),
    Refresh,

    // Wizard actions
    /// Generate config in wizard
    WizardGenConfig,
    /// Apply config in wizard
    WizardApplyConfig,
    /// Bootstrap cluster in wizard
    WizardBootstrap,
    /// Retry after error in wizard
    WizardRetry,
    /// Wizard complete - transition to secure mode (context_name)
    WizardComplete(Option<String>),
}
