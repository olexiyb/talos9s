//! Lifecycle component - displays version status, time sync, and config drift
//!
//! Provides a consolidated view of cluster lifecycle operations.

use crate::action::Action;
use crate::components::Component;
use crate::components::diagnostics::k8s::{
    KubeconfigSource, PdbHealthInfo, PodHealthInfo, check_pdb_health, check_pod_health,
    discovery_members_via_k8s,
};
use crate::reachability::prefer_reachable_addresses;
use crate::ui_ext::HealthIndicatorExt;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use kube::Client;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState},
};
use std::time::Duration;
use talos_rs::{
    DiscoveryMember, EtcdMemberInfo, NodeTimeInfo, TalosClient, TalosConfig, VersionInfo,
    get_discovery_members_with_retry,
};
use talos9s_core::{AsyncState, EtcdQuorum, HasHealth, HealthIndicator, SelectableList};

const AUTO_REFRESH_INTERVAL_SECS: u64 = talos9s_core::constants::refresh_intervals::VERY_SLOW;

/// How long one Talos, Kubernetes or `talosctl` call may take; a dead node
/// must not hold up the refresh
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Discovery retries internally, so it gets longer
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// `call` bounded by `limit`; a timeout is an error like any other
async fn bounded<T, E: std::fmt::Display>(
    limit: Duration,
    call: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> std::result::Result<T, String> {
    match tokio::time::timeout(limit, call).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => Err(format!("timed out after {}s", limit.as_secs())),
    }
}

/// Query Talos version and time-sync info for every roster node individually.
///
/// The base client's single `version()` / `time()` only cover the endpoint it
/// targets, so this fans out across the whole roster (control plane and
/// workers) to build complete lists for the version / config-drift table. Each
/// entry is keyed by the node hostname (falling back to its IP) so versions and
/// time-sync rows line up. Returns empty vecs if nothing could be queried,
/// letting callers keep whatever they already had.
async fn enumerate_roster_node_info(
    client: &TalosClient,
    roster: &[DiscoveryMember],
) -> (Vec<VersionInfo>, Vec<NodeTimeInfo>) {
    let mut versions = Vec::new();
    let mut times = Vec::new();
    for member in roster {
        let Some(ip) = member.addresses.first() else {
            continue;
        };
        let name = if member.hostname.is_empty() {
            ip.clone()
        } else {
            member.hostname.clone()
        };
        let node_client = client.with_node(ip);
        if let Ok(mut vs) = bounded(CALL_TIMEOUT, node_client.version()).await {
            for v in &mut vs {
                v.node = name.clone();
            }
            versions.extend(vs);
        }
        if let Ok(mut ts) = bounded(CALL_TIMEOUT, node_client.time()).await {
            for t in &mut ts {
                t.node = name.clone();
            }
            times.extend(ts);
        }
    }
    (versions, times)
}

/// Resolve the IP to target for per-node `talosctl` calls (e.g. config-hash).
///
/// Prefers the roster address for `node` (matched by hostname or address) so
/// hostname-named nodes still resolve to a reachable IP; falls back to `node`
/// itself with any port stripped.
fn roster_ip_for(roster: &[DiscoveryMember], node: &str) -> String {
    roster
        .iter()
        .find(|m| m.hostname == node || m.addresses.iter().any(|a| a == node))
        .and_then(|m| m.addresses.first().cloned())
        .unwrap_or_else(|| talos_rs::host_of(node).to_string())
}

/// Node lifecycle status
#[derive(Debug, Clone)]
pub struct NodeStatus {
    /// Node hostname
    pub hostname: String,
    /// Talos version
    pub version: String,
    /// Config hash (placeholder - needs MachineConfig API)
    pub config_hash: Option<String>,
    /// Time sync status (placeholder - needs TimeStatus API)
    pub time_synced: Option<bool>,
    /// Node ready status
    pub ready: bool,
    /// Platform (container, metal, etc.)
    pub platform: String,
}

/// Alert item
#[derive(Debug, Clone)]
pub struct Alert {
    /// Alert severity
    pub severity: AlertSeverity,
    /// Alert message
    pub message: String,
}

#[derive(Debug, Clone, Copy)]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
}

impl HasHealth for AlertSeverity {
    fn health(&self) -> HealthIndicator {
        match self {
            AlertSeverity::Info => HealthIndicator::Info,
            AlertSeverity::Warning => HealthIndicator::Warning,
            AlertSeverity::Error => HealthIndicator::Error,
        }
    }
}

impl AlertSeverity {
    fn indicator(&self) -> (&'static str, Color) {
        self.health().symbol_and_color()
    }
}

/// etcd quorum information for pre-operation checks
#[derive(Debug, Clone, Default)]
pub struct EtcdQuorumInfo {
    /// Voting etcd members
    pub total_members: usize,
    /// Voters that answered the status call; `None` when unknown
    pub healthy_members: Option<usize>,
    /// How many more voters can go down before quorum is lost; `None` when
    /// unknown
    pub can_lose: Option<usize>,
    /// Whether quorum holds; `None` when unknown
    pub has_quorum: Option<bool>,
}

impl From<EtcdQuorum> for EtcdQuorumInfo {
    fn from(q: EtcdQuorum) -> Self {
        EtcdQuorumInfo {
            total_members: q.voters,
            healthy_members: q.healthy,
            can_lose: q.can_lose(),
            has_quorum: q.has_quorum(),
        }
    }
}

impl HasHealth for EtcdQuorumInfo {
    /// A node can be taken down only while one more can be lost
    fn health(&self) -> HealthIndicator {
        match (self.has_quorum, self.can_lose) {
            (Some(false), _) => HealthIndicator::Error,
            (Some(true), Some(0)) => HealthIndicator::Warning,
            (Some(true), Some(_)) => HealthIndicator::Healthy,
            _ => HealthIndicator::Unknown,
        }
    }
}

impl EtcdQuorumInfo {
    /// Get summary message
    pub fn summary(&self) -> String {
        if self.total_members == 0 {
            return "No etcd members found".to_string();
        }
        match (self.healthy_members, self.has_quorum, self.can_lose) {
            (Some(healthy), Some(true), Some(can_lose)) => format!(
                "{}/{} members, can lose {}",
                healthy, self.total_members, can_lose
            ),
            (Some(healthy), _, _) => format!(
                "{}/{} members healthy (quorum at risk)",
                healthy, self.total_members
            ),
            _ => format!("{} members, health unknown", self.total_members),
        }
    }
}

/// Pre-operation health check results
#[derive(Debug, Clone, Default)]
pub struct PreOpChecks {
    /// Pod health info
    pub pod_health: Option<PodHealthInfo>,
    /// PDB health info
    pub pdb_health: Option<PdbHealthInfo>,
    /// etcd quorum info
    pub etcd_quorum: Option<EtcdQuorumInfo>,
    /// Worst of the checks; a check that couldn't run is unknown, not passed
    pub overall: HealthIndicator,
}

impl PreOpChecks {
    fn new(
        pod_health: Option<PodHealthInfo>,
        pdb_health: Option<PdbHealthInfo>,
        etcd_quorum: Option<EtcdQuorumInfo>,
    ) -> Self {
        let pod = pod_health.as_ref().map_or(HealthIndicator::Unknown, |p| {
            if p.has_issues() {
                HealthIndicator::Warning
            } else {
                HealthIndicator::Healthy
            }
        });
        let pdb = pdb_health.as_ref().map_or(HealthIndicator::Unknown, |p| {
            if p.has_blocking_pdbs() {
                HealthIndicator::Warning
            } else {
                HealthIndicator::Healthy
            }
        });
        let etcd = etcd_quorum
            .as_ref()
            .map_or(HealthIndicator::Unknown, |e| e.health());
        PreOpChecks {
            pod_health,
            pdb_health,
            etcd_quorum,
            overall: pod.worst(pdb).worst(etcd),
        }
    }
}

/// Loaded lifecycle data (wrapped by AsyncState)
#[derive(Debug, Clone, Default)]
pub struct LifecycleData {
    /// Context name (cluster identifier)
    pub context_name: String,
    /// Talos versions per node
    pub versions: Vec<VersionInfo>,
    /// Time sync info per node
    pub time_info: Vec<NodeTimeInfo>,
    /// Node statuses with selection
    pub node_statuses: SelectableList<NodeStatus>,
    /// Alerts
    pub alerts: Vec<Alert>,
    /// Discovery members (from talosctl get members)
    pub discovery_members: Vec<DiscoveryMember>,
    /// Pre-operation health checks
    pub pre_op_checks: PreOpChecks,
}

/// Lifecycle component for viewing version and lifecycle status
#[derive(Clone)]
pub struct LifecycleComponent {
    /// Async state wrapping all lifecycle data
    state: AsyncState<LifecycleData>,

    /// Table state for rendering (synced with node_statuses selection)
    table_state: TableState,
    /// Where the rows were drawn, for clicks
    rows: crate::ui_ext::RowHits,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls
    client: Option<TalosClient>,

    /// K8s client for pod/PDB checks (reusable, not part of loaded data)
    k8s_client: Option<Client>,

    /// Source of the kubeconfig (for display in UI)
    k8s_source: Option<KubeconfigSource>,

    /// Custom config file path (from --config flag)
    config_path: Option<String>,
}

impl Default for LifecycleComponent {
    fn default() -> Self {
        Self::new("".to_string(), None)
    }
}

impl LifecycleComponent {
    pub fn new(context_name: String, config_path: Option<String>) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        // Initialize with context name in the data
        let initial_data = LifecycleData {
            context_name,
            ..Default::default()
        };
        let mut state = AsyncState::new();
        state.set_data(initial_data);

        Self {
            state,
            table_state,
            rows: Default::default(),
            auto_refresh: true,
            client: None,
            k8s_client: None,
            k8s_source: None,
            config_path,
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

    /// Helper to get data reference
    fn data(&self) -> Option<&LifecycleData> {
        self.state.data()
    }

    /// Helper to get mutable data reference
    fn data_mut(&mut self) -> Option<&mut LifecycleData> {
        self.state.data_mut()
    }

    /// Refresh lifecycle data
    pub async fn refresh(&mut self) -> Result<()> {
        self.state.start_loading();

        let Some(client) = self.client.clone() else {
            self.state.set_error("No client configured");
            return Ok(());
        };

        // Get or create data
        let mut data = self.state.take_data().unwrap_or_default();

        // Fetch version information.
        //
        // Non-fatal: a version RPC failure must NOT abort the refresh. The K8s,
        // etcd, and pod/PDB pre-operation checks below use independent data
        // sources and can still succeed. Previously this early-returned after
        // set_data() (which clears the just-set error), leaving every section
        // stuck on "unknown"/"unavailable" with no error and no further fetches.
        let version_error = match bounded(CALL_TIMEOUT, client.version()).await {
            Ok(versions) => {
                // Get context name from talosconfig if not already set
                if !versions.is_empty() && data.context_name.is_empty() {
                    let config_result = match &self.config_path {
                        Some(path) => {
                            let path_buf = std::path::PathBuf::from(path);
                            TalosConfig::load_from(&path_buf)
                        }
                        None => TalosConfig::load_default(),
                    };
                    if let Ok(config) = config_result {
                        data.context_name = config.context;
                    }
                }

                data.versions = versions;
                None
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to fetch versions: {} (continuing with etcd/K8s pre-op checks)",
                    e
                );
                // Preserve any cached versions for display; do not abort.
                Some(format!("Failed to fetch versions: {}", e))
            }
        };

        // Fetch time sync status
        match bounded(CALL_TIMEOUT, client.time()).await {
            Ok(times) => {
                data.time_info = times;
            }
            Err(e) => {
                // Time fetch failure is not fatal - just log it
                tracing::warn!("Failed to fetch time status: {}", e);
                data.time_info.clear();
            }
        }

        // Used for the Kubernetes fallback, reachability hints and the quorum
        // check, so fetched once
        let etcd_members = bounded(CALL_TIMEOUT, client.etcd_members()).await;

        // Fetch discovery members using context-aware async version
        let context_name = if !data.context_name.is_empty() {
            data.context_name.clone()
        } else {
            let config_result = match &self.config_path {
                Some(path) => {
                    let path_buf = std::path::PathBuf::from(path);
                    TalosConfig::load_from(&path_buf)
                }
                None => TalosConfig::load_default(),
            };
            config_result
                .map(|config| config.context)
                .unwrap_or_default()
        };

        if !context_name.is_empty() {
            // Use version info nodes as fallback in case VIP-based discovery fails
            let fallback_ips: Vec<String> = data
                .versions
                .iter()
                .map(|v| talos_rs::host_of(&v.node).to_string())
                .collect();
            match bounded(
                DISCOVERY_TIMEOUT,
                get_discovery_members_with_retry(
                    &context_name,
                    self.config_path.as_deref(),
                    &fallback_ips,
                ),
            )
            .await
            {
                Ok(members) if !members.is_empty() => {
                    data.discovery_members = members;
                }
                outcome => {
                    match &outcome {
                        Ok(_) => tracing::debug!(
                            "Talos discovery returned no members (trying the Kubernetes API)"
                        ),
                        Err(e) => tracing::debug!(
                            "Discovery fetch failed after retries: {} (trying the Kubernetes API)",
                            e
                        ),
                    }
                    // Discovery is disabled or unreachable. Fall back to the
                    // Kubernetes API, which knows every node independently of
                    // Talos discovery, so the version / config-drift table covers
                    // workers too — not just the endpoint. Mirrors the cluster
                    // node-list fallback.
                    if data.discovery_members.is_empty() {
                        let cp_ip = etcd_members
                            .as_ref()
                            .ok()
                            .and_then(|ms| ms.iter().find_map(|m| m.ip_address()))
                            .or_else(|| fallback_ips.first().cloned());
                        if let Some(cp_ip) = cp_ip {
                            match bounded(CALL_TIMEOUT, discovery_members_via_k8s(&client, &cp_ip))
                                .await
                            {
                                Ok(members) if !members.is_empty() => {
                                    tracing::info!(
                                        "Enumerated {} node(s) via the Kubernetes API for \
                                         lifecycle (Talos discovery unavailable)",
                                        members.len()
                                    );
                                    data.discovery_members = members;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    tracing::debug!("Kubernetes node enumeration failed: {}", e)
                                }
                            }
                        }
                    }
                }
            }

            // Enumerate version + time-sync info for every roster node so the
            // table covers workers, not just the endpoint the base client
            // targets. Keep the previously fetched data if the roster query
            // yields nothing (graceful degradation).
            let hints: Vec<String> = etcd_members
                .as_ref()
                .map(|ms| ms.iter().filter_map(|m| m.ip_address()).collect())
                .unwrap_or_default();
            // Probes are bounded per address; this bounds the whole pass
            let _ = tokio::time::timeout(
                CALL_TIMEOUT,
                prefer_reachable_addresses(&client, &mut data.discovery_members, &hints),
            )
            .await;
            let (roster_versions, roster_times) =
                enumerate_roster_node_info(&client, &data.discovery_members).await;
            if !roster_versions.is_empty() {
                data.versions = roster_versions;
                data.time_info = roster_times;
            }
        }

        // Fetch each node's config hash for drift detection, concurrently and
        // in this cluster's context. Resolve the IP from the roster so this
        // still works when the node is named by hostname (e.g. workers
        // enumerated via the Kubernetes API).
        let talos_context = (!context_name.is_empty()).then_some(context_name.as_str());
        let config_path = self.config_path.as_deref();
        let config_hashes: Vec<Option<String>> =
            futures::future::join_all(data.versions.iter().map(|v| {
                let node_addr = roster_ip_for(&data.discovery_members, &v.node);
                async move {
                    bounded(
                        CALL_TIMEOUT,
                        talos_rs::get_machine_config_for_node(
                            talos_context,
                            &node_addr,
                            config_path,
                        ),
                    )
                    .await
                    .ok()
                    .map(|config| config.version)
                }
            }))
            .await;

        // Build node statuses combining version and time info
        let statuses: Vec<NodeStatus> = data
            .versions
            .iter()
            .zip(config_hashes)
            .map(|(v, node_config_hash)| {
                // Look up time sync status for this node
                let time_synced = data
                    .time_info
                    .iter()
                    .find(|t| t.node == v.node)
                    .map(|t| t.synced);

                NodeStatus {
                    hostname: v.node.clone(),
                    version: v.version.clone(),
                    config_hash: node_config_hash,
                    time_synced,
                    ready: true, // Assume ready for now
                    platform: v.platform.clone(),
                }
            })
            .collect();

        // Update items, preserving selection if possible
        data.node_statuses.update_items(statuses);
        self.table_state
            .select(Some(data.node_statuses.selected_index()));

        // Fetch pre-operation health checks
        self.fetch_pre_op_checks_into(&mut data, &client, etcd_members)
            .await;

        // Generate alerts
        Self::generate_alerts_into(&mut data);

        // Store the data (this clears any prior error).
        self.state.set_data(data);

        // If the version RPC failed AND nothing else could be gathered, surface
        // the error instead of a silent all-"unknown" screen. If any pre-op
        // check succeeded we keep the partial view (graceful degradation).
        if let Some(err) = version_error {
            let nothing_loaded = self.data().map(Self::gathered_nothing).unwrap_or(true);
            if nothing_loaded {
                self.state.set_error(err);
            }
        }
        Ok(())
    }

    /// True when a refresh produced no usable data at all (no versions, no node
    /// rows, and no pre-operation check succeeded). Used to decide whether a
    /// version-fetch failure should surface as an error banner rather than a
    /// silent all-"unknown" screen. Kept pure so it can be unit-tested.
    fn gathered_nothing(data: &LifecycleData) -> bool {
        data.versions.is_empty()
            && data.node_statuses.is_empty()
            && data.pre_op_checks.pod_health.is_none()
            && data.pre_op_checks.pdb_health.is_none()
            && data.pre_op_checks.etcd_quorum.is_none()
    }

    /// Fetch pre-operation health checks into data
    async fn fetch_pre_op_checks_into(
        &mut self,
        data: &mut LifecycleData,
        client: &TalosClient,
        etcd_members: std::result::Result<Vec<EtcdMemberInfo>, String>,
    ) {
        // Only control plane nodes run etcd: fetch the kubeconfig from one
        let cp_node_ip = etcd_members
            .as_ref()
            .ok()
            .and_then(|ms| ms.first())
            .and_then(|m| m.ip_address());

        if self.k8s_client.is_none() {
            match crate::components::diagnostics::k8s::connect(client, cp_node_ip.as_deref()).await
            {
                Ok((k8s, source)) => {
                    self.k8s_client = Some(k8s);
                    self.k8s_source = Some(source);
                }
                Err(e) => self.k8s_source = Some(KubeconfigSource::Unavailable(e)),
            }
        }

        let etcd_quorum = match etcd_members {
            Ok(members) => {
                // Target the status call by IP: hostnames may not resolve
                let cp_ips: Vec<String> = members.iter().filter_map(|m| m.ip_address()).collect();
                let statuses = bounded(CALL_TIMEOUT, client.etcd_status_for_nodes(&cp_ips))
                    .await
                    .inspect_err(|e| tracing::warn!("Failed to fetch etcd status: {}", e))
                    .ok();
                Some(EtcdQuorumInfo::from(EtcdQuorum::assess(
                    &members,
                    statuses.as_deref(),
                )))
            }
            Err(e) => {
                tracing::warn!("Failed to fetch etcd members: {}", e);
                None
            }
        };

        let (pod_health, pdb_health) = if let Some(k8s) = &self.k8s_client {
            let (pods, pdbs) = tokio::join!(
                bounded(CALL_TIMEOUT, check_pod_health(k8s, None)),
                bounded(CALL_TIMEOUT, check_pdb_health(k8s)),
            );
            (pods.ok(), pdbs.ok())
        } else {
            (None, None)
        };

        data.pre_op_checks = PreOpChecks::new(pod_health, pdb_health, etcd_quorum);
    }

    /// Generate alerts based on current state (static method operating on data)
    fn generate_alerts_into(data: &mut LifecycleData) {
        data.alerts.clear();

        // Check for version mismatches
        let versions: Vec<&str> = data.versions.iter().map(|v| v.version.as_str()).collect();
        let unique_versions: std::collections::HashSet<_> = versions.iter().collect();
        if unique_versions.len() > 1 {
            data.alerts.push(Alert {
                severity: AlertSeverity::Warning,
                message: "Version mismatch detected across nodes".to_string(),
            });
        }

        // Check for time sync issues
        let unsynced_nodes: Vec<&str> = data
            .node_statuses
            .items()
            .iter()
            .filter(|n| n.time_synced == Some(false))
            .map(|n| n.hostname.as_str())
            .collect();
        if !unsynced_nodes.is_empty() {
            data.alerts.push(Alert {
                severity: AlertSeverity::Warning,
                message: format!("Time not synced on: {}", unsynced_nodes.join(", ")),
            });
        }

        // Check for config drift (compare hashes across nodes)
        let config_hashes: Vec<(&str, Option<&str>)> = data
            .node_statuses
            .items()
            .iter()
            .map(|n| (n.hostname.as_str(), n.config_hash.as_deref()))
            .collect();

        let unique_hashes: std::collections::HashSet<_> =
            config_hashes.iter().filter_map(|(_, h)| *h).collect();

        if unique_hashes.len() > 1 {
            // Config drift detected - list nodes with different hashes
            let drift_details: Vec<String> = config_hashes
                .iter()
                .filter_map(|(hostname, hash)| {
                    hash.map(|h| format!("{}:{}", talos_rs::host_of(hostname), h))
                })
                .collect();
            data.alerts.push(Alert {
                severity: AlertSeverity::Warning,
                message: format!("Config drift detected: {}", drift_details.join(", ")),
            });
        } else if let Some(hash) = unique_hashes.iter().next() {
            // All nodes have same config hash
            data.alerts.push(Alert {
                severity: AlertSeverity::Info,
                message: format!("Config version: {} (all nodes in sync)", hash),
            });
        }

        // Check discovery service health
        if !data.discovery_members.is_empty() {
            let expected_nodes = data.node_statuses.len();
            let discovered_nodes = data.discovery_members.len();

            if discovered_nodes < expected_nodes {
                data.alerts.push(Alert {
                    severity: AlertSeverity::Warning,
                    message: format!(
                        "Discovery: {}/{} nodes visible (some nodes may not be discoverable)",
                        discovered_nodes, expected_nodes
                    ),
                });
            } else {
                data.alerts.push(Alert {
                    severity: AlertSeverity::Info,
                    message: format!(
                        "Discovery: {}/{} nodes healthy",
                        discovered_nodes, expected_nodes
                    ),
                });
            }
        }
    }

    /// Get current Talos version (from first node)
    fn current_talos_version(&self) -> &str {
        self.data()
            .and_then(|d| d.versions.first())
            .map(|v| v.version.as_str())
            .unwrap_or("unknown")
    }

    /// Get K8s version support range for current Talos
    fn k8s_support_range(&self) -> &str {
        // Talos version to K8s support mapping
        // See: https://www.talos.dev/latest/introduction/support-matrix/
        let version = self.current_talos_version();
        if version.starts_with("v1.12") {
            "v1.30 - v1.35"
        } else if version.starts_with("v1.11") {
            "v1.29 - v1.34"
        } else if version.starts_with("v1.10") {
            "v1.28 - v1.33"
        } else if version.starts_with("v1.9") {
            "v1.27 - v1.32"
        } else if version.starts_with("v1.8") {
            "v1.26 - v1.31"
        } else if version.starts_with("v1.7") {
            "v1.25 - v1.30"
        } else if version.starts_with("v1.6") {
            "v1.24 - v1.29"
        } else {
            "unknown"
        }
    }

    /// Navigation: select next node
    fn select_next(&mut self) {
        let new_index = if let Some(data) = self.data_mut() {
            data.node_statuses.select_next();
            Some(data.node_statuses.selected_index())
        } else {
            None
        };
        if let Some(idx) = new_index {
            self.table_state.select(Some(idx));
        }
    }

    /// Navigation: select previous node
    fn select_prev(&mut self) {
        let new_index = if let Some(data) = self.data_mut() {
            data.node_statuses.select_prev();
            Some(data.node_statuses.selected_index())
        } else {
            None
        };
        if let Some(idx) = new_index {
            self.table_state.select(Some(idx));
        }
    }

    /// Draw the cluster versions section
    fn draw_versions_section(&self, frame: &mut Frame, area: Rect) {
        let version = self.current_talos_version();
        let k8s_range = self.k8s_support_range();

        // Get platform from first node
        let platform = self
            .data()
            .and_then(|d| d.versions.first())
            .map(|v| v.platform.as_str())
            .unwrap_or("unknown");

        let lines = vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Talos Version",
                Style::default().fg(Color::Yellow),
            )]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("|-", Style::default().fg(Color::DarkGray)),
                Span::raw(" Current:   "),
                Span::styled(version, Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("|-", Style::default().fg(Color::DarkGray)),
                Span::raw(" Platform:  "),
                Span::styled(platform, Style::default().fg(Color::Cyan)),
            ]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Kubernetes Support",
                Style::default().fg(Color::Yellow),
            )]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("|-", Style::default().fg(Color::DarkGray)),
                Span::raw(" Supported: "),
                Span::styled(k8s_range, Style::default().fg(Color::White)),
            ]),
        ];

        let block = Block::default()
            .title(" Cluster Versions ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw the node status table
    fn draw_node_table(&mut self, frame: &mut Frame, area: Rect) {
        let header_cells = ["Node", "Version", "Config", "Time Sync", "Status"]
            .iter()
            .map(|h| {
                ratatui::widgets::Cell::from(*h).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            });
        let header = Row::new(header_cells).height(1);

        let node_items: Vec<NodeStatus> = self
            .data()
            .map(|d| d.node_statuses.items().to_vec())
            .unwrap_or_default();

        let rows = node_items.iter().map(|node| {
            let config_cell = match &node.config_hash {
                Some(hash) => Span::styled(
                    &hash[..8.min(hash.len())],
                    Style::default().fg(Color::White),
                ),
                None => Span::styled("-", Style::default().fg(Color::Gray)),
            };

            let time_cell = match node.time_synced {
                Some(true) => Span::styled("synced", Style::default().fg(Color::Green)),
                Some(false) => Span::styled("not synced", Style::default().fg(Color::Yellow)),
                None => Span::styled("-", Style::default().fg(Color::Gray)),
            };

            let status_cell = if node.ready {
                Span::styled("Ready", Style::default().fg(Color::Green))
            } else {
                Span::styled("NotReady", Style::default().fg(Color::Red))
            };

            Row::new(vec![
                ratatui::widgets::Cell::from(node.hostname.clone())
                    .style(Style::default().fg(Color::White)),
                ratatui::widgets::Cell::from(node.version.clone())
                    .style(Style::default().fg(Color::Green)),
                ratatui::widgets::Cell::from(config_cell),
                ratatui::widgets::Cell::from(time_cell),
                ratatui::widgets::Cell::from(status_cell),
            ])
        });

        let widths = [
            Constraint::Min(20),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(10),
        ];

        let block = Block::default()
            .title(" Node Status ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let row_count = rows.len();
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(table, area, &mut self.table_state);
        self.rows =
            crate::ui_ext::RowHits::new(area, true, 1, self.table_state.offset(), row_count);
    }

    /// Draw the alerts section
    fn draw_alerts_section(&self, frame: &mut Frame, area: Rect) {
        let alerts: Vec<Alert> = self.data().map(|d| d.alerts.clone()).unwrap_or_default();

        let lines: Vec<Line> = alerts
            .iter()
            .map(|alert| {
                let (indicator, color) = alert.severity.indicator();
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(indicator, Style::default().fg(color)),
                    Span::raw(" "),
                    Span::raw(&alert.message),
                ])
            })
            .collect();

        let block = Block::default()
            .title(" Alerts ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw the pre-operation health checks section
    fn draw_pre_op_checks(&self, frame: &mut Frame, area: Rect) {
        let pre_op_checks = self
            .data()
            .map(|d| d.pre_op_checks.clone())
            .unwrap_or_default();

        let mut lines = Vec::new();

        // Overall status indicator
        let (overall_indicator, overall_color) = pre_op_checks.overall.symbol_and_color();
        let overall_text = match pre_op_checks.overall {
            HealthIndicator::Healthy => "All checks passed",
            HealthIndicator::Unknown => "Some checks could not run",
            HealthIndicator::Error => "Some checks failed",
            _ => "Some checks have warnings",
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(overall_indicator, Style::default().fg(overall_color)),
            Span::raw(" "),
            Span::styled(overall_text, Style::default().fg(overall_color)),
        ]));

        // etcd quorum check
        if let Some(ref etcd) = pre_op_checks.etcd_quorum {
            let (indicator, color) = etcd.health().symbol_and_color();
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(" etcd: "),
                Span::styled(etcd.summary(), Style::default().fg(color)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("?", Style::default().fg(Color::DarkGray)),
                Span::raw(" etcd: "),
                Span::styled("unavailable", Style::default().fg(Color::DarkGray)),
            ]));
        }

        // K8s source info (show where kubeconfig came from)
        let k8s_source_text = match &self.k8s_source {
            Some(KubeconfigSource::Environment(_)) => "via KUBECONFIG".to_string(),
            Some(KubeconfigSource::TalosNode(node)) => format!("via node {}", node),
            Some(KubeconfigSource::Unavailable(err)) => {
                // Truncate long error messages
                let short_err = talos9s_core::truncate_string(err, 43);
                format!("unavailable: {}", short_err)
            }
            None => "not initialized".to_string(),
        };

        let k8s_color = match &self.k8s_source {
            Some(KubeconfigSource::Environment(_)) | Some(KubeconfigSource::TalosNode(_)) => {
                Color::Green
            }
            _ => Color::DarkGray,
        };

        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                if self.k8s_client.is_some() {
                    "✓"
                } else {
                    "?"
                },
                Style::default().fg(k8s_color),
            ),
            Span::raw(" K8s: "),
            Span::styled(k8s_source_text, Style::default().fg(k8s_color)),
        ]));

        // Pod health check
        if let Some(ref pods) = pre_op_checks.pod_health {
            let has_issues = pods.has_issues();
            let (indicator, color) = if !has_issues && pods.pending.is_empty() {
                ("✓", Color::Green)
            } else if !has_issues {
                ("◐", Color::Yellow)
            } else {
                ("✗", Color::Red)
            };

            let running = pods.total_pods
                - pods.crashing.len()
                - pods.image_pull_errors.len()
                - pods.pending.len();
            let summary = if has_issues || !pods.pending.is_empty() {
                format!("{} running, {}", running, pods.summary())
            } else {
                format!("{} pods healthy", pods.total_pods)
            };

            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(" Pods: "),
                Span::styled(summary, Style::default().fg(color)),
            ]));
        } else {
            // Show hint about why pods are unavailable
            let hint = match &self.k8s_source {
                Some(KubeconfigSource::Unavailable(_)) => "set KUBECONFIG",
                None => "loading...",
                _ => "K8s API error",
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("?", Style::default().fg(Color::DarkGray)),
                Span::raw(" Pods: "),
                Span::styled(
                    format!("unavailable ({})", hint),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }

        // PDB check
        if let Some(ref pdbs) = pre_op_checks.pdb_health {
            let (indicator, color) = if !pdbs.has_blocking_pdbs() {
                ("✓", Color::Green)
            } else {
                ("◐", Color::Yellow)
            };

            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(" PDBs: "),
                Span::styled(pdbs.summary(), Style::default().fg(color)),
            ]));
        } else {
            // Show hint about why PDBs are unavailable
            let hint = match &self.k8s_source {
                Some(KubeconfigSource::Unavailable(_)) => "set KUBECONFIG",
                None => "loading...",
                _ => "K8s API error",
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("?", Style::default().fg(Color::DarkGray)),
                Span::raw(" PDBs: "),
                Span::styled(
                    format!("unavailable ({})", hint),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }

        let block = Block::default()
            .title(" Pre-Operation Checks ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw the footer
    fn key_hints_line(&self) -> Line<'static> {
        let spans = vec![
            Span::styled("[j/k]", Style::default().fg(Color::Cyan)),
            Span::raw(" navigate  "),
            Span::styled("[r]", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh"),
        ];

        Line::from(spans)
    }
}

impl Component for LifecycleComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if let Some(i) = self.rows.clicked(&mouse) {
            if let Some(data) = self.data_mut() {
                data.node_statuses.select(i);
            }
            self.table_state.select(Some(i));
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next();
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_prev();
                Ok(None)
            }
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Check for auto-refresh using AsyncState
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Check loading state (show loading only if no data yet)
        let has_nodes = self
            .data()
            .map(|d| !d.node_statuses.is_empty())
            .unwrap_or(false);

        if self.state.is_loading() && !has_nodes {
            let loading = Paragraph::new("Loading lifecycle status...")
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

        // Get context name from data
        let context_name = self
            .data()
            .map(|d| d.context_name.clone())
            .unwrap_or_default();

        // Layout
        let chunks = Layout::vertical([
            Constraint::Length(1),  // Header
            Constraint::Length(10), // Versions section
            Constraint::Length(7),  // Pre-Operation Checks (4 lines + border + padding)
            Constraint::Min(6),     // Node table
            Constraint::Length(6),  // Alerts (3 lines + border)
        ])
        .split(area);

        // Header
        let header = Paragraph::new(format!(" Lifecycle │ {}", context_name)).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(header, chunks[0]);

        // Versions section
        self.draw_versions_section(frame, chunks[1]);

        // Pre-Operation Checks
        self.draw_pre_op_checks(frame, chunks[2]);

        // Node table
        self.draw_node_table(frame, chunks[3]);

        // Alerts
        self.draw_alerts_section(frame, chunks[4]);

        Ok(())
    }
}

impl crate::components::BackgroundRefresh for LifecycleComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        // The node selection lives inside the data; carry the live one over
        let selected = self.data().map(|d| d.node_statuses.selected_index());
        self.state = refreshed.state;
        self.k8s_client = refreshed.k8s_client;
        self.k8s_source = refreshed.k8s_source;
        let selected = match (self.state.data_mut(), selected) {
            (Some(data), Some(idx)) => {
                data.node_statuses.select(idx);
                Some(data.node_statuses.selected_index())
            }
            (Some(data), None) => Some(data.node_statuses.selected_index()),
            _ => None,
        };
        if let Some(idx) = selected {
            self.table_state.select(Some(idx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The roster IP resolver maps a hostname- or IP-named node back to a
    /// reachable IP, so per-node calls (config-hash) work for workers
    /// enumerated via the Kubernetes API. Unknown nodes fall back to themselves
    /// with any port stripped. Regression guard for the discovery-disabled
    /// lifecycle enumeration.
    #[test]
    fn roster_ip_for_resolves_hostname_and_falls_back() {
        let roster = vec![DiscoveryMember {
            id: "w1".into(),
            addresses: vec!["10.0.0.2".into()],
            hostname: "worker-1".into(),
            machine_type: "worker".into(),
            operating_system: String::new(),
        }];
        // By hostname -> IP.
        assert_eq!(roster_ip_for(&roster, "worker-1"), "10.0.0.2");
        // By address -> same IP.
        assert_eq!(roster_ip_for(&roster, "10.0.0.2"), "10.0.0.2");
        // Unknown node with a port -> itself, port stripped.
        assert_eq!(roster_ip_for(&roster, "10.9.9.9:50000"), "10.9.9.9");
        // Empty roster -> fall back to the node itself.
        assert_eq!(roster_ip_for(&[], "cp-1"), "cp-1");
    }

    /// A totally empty refresh result (e.g. version RPC failed and no pre-op
    /// check succeeded) must be reported as "nothing gathered" so the caller
    /// surfaces an error instead of a silent all-"unknown" screen. This is the
    /// regression guard for issue #2 (lifecycle view stuck on "unknown").
    #[test]
    fn gathered_nothing_is_true_for_empty_data() {
        let data = LifecycleData::default();
        assert!(LifecycleComponent::gathered_nothing(&data));
    }

    /// If any single pre-op check succeeded, the refresh produced usable data,
    /// so we keep the partial view rather than replacing it with an error
    /// (graceful degradation — the whole point of not aborting on version()).
    #[test]
    fn gathered_nothing_is_false_when_etcd_quorum_present() {
        let mut data = LifecycleData::default();
        data.pre_op_checks.etcd_quorum = Some(EtcdQuorumInfo {
            total_members: 3,
            healthy_members: Some(3),
            can_lose: Some(1),
            has_quorum: Some(true),
        });
        assert!(!LifecycleComponent::gathered_nothing(&data));
    }

    fn quorum(voters: usize, healthy: Option<usize>) -> EtcdQuorumInfo {
        let has_quorum = healthy.map(|h| h > voters / 2);
        EtcdQuorumInfo {
            total_members: voters,
            healthy_members: healthy,
            can_lose: healthy.map(|h| h.saturating_sub(voters / 2 + 1)),
            has_quorum,
        }
    }

    #[test]
    fn etcd_quorum_health() {
        assert_eq!(quorum(3, Some(3)).health(), HealthIndicator::Healthy);
        // Quorum holds but one more loss breaks it
        assert_eq!(quorum(3, Some(2)).health(), HealthIndicator::Warning);
        assert_eq!(quorum(3, Some(1)).health(), HealthIndicator::Error);
        // Status call failed: unknown, never assumed healthy
        assert_eq!(quorum(3, None).health(), HealthIndicator::Unknown);
        assert_eq!(quorum(3, None).summary(), "3 members, health unknown");
    }

    #[test]
    fn pre_op_checks_that_could_not_run_are_not_passed() {
        let checks = PreOpChecks::new(None, None, Some(quorum(3, Some(3))));
        assert_eq!(checks.overall, HealthIndicator::Unknown);
        let checks = PreOpChecks::new(None, None, None);
        assert_eq!(checks.overall, HealthIndicator::Unknown);
    }

    #[test]
    fn pre_op_checks_worst_wins() {
        let pods = PodHealthInfo::default();
        let pdbs = PdbHealthInfo::default();
        let checks = PreOpChecks::new(
            Some(pods.clone()),
            Some(pdbs.clone()),
            Some(quorum(3, Some(3))),
        );
        assert_eq!(checks.overall, HealthIndicator::Healthy);
        let checks = PreOpChecks::new(Some(pods), Some(pdbs), Some(quorum(3, Some(1))));
        assert_eq!(checks.overall, HealthIndicator::Error);
    }

    /// Versions present (the happy path) is likewise not "nothing".
    #[test]
    fn gathered_nothing_is_false_when_versions_present() {
        let mut data = LifecycleData::default();
        data.versions.push(VersionInfo {
            node: "10.5.0.2".to_string(),
            version: "v1.12.1".to_string(),
            sha: String::new(),
            built: String::new(),
            go_version: String::new(),
            os: String::new(),
            arch: String::new(),
            platform: "container".to_string(),
        });
        assert!(!LifecycleComponent::gathered_nothing(&data));
    }
}
