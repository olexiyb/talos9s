//! Cluster data: what a refresh fetches from the nodes and how it is assembled

use super::*;

/// What one node answered in a cluster refresh
#[derive(Clone)]
pub(super) struct NodeFetch {
    /// `Err` when the node didn't answer (the error, or none on a timeout)
    pub(super) version: Result<Vec<VersionInfo>, Option<String>>,
    pub(super) services: Option<Vec<NodeServices>>,
    pub(super) memory: Option<Vec<NodeMemory>>,
    pub(super) load_avg: Option<Vec<NodeLoadAvg>>,
    pub(super) cpu_info: Option<Vec<NodeCpuInfo>>,
    pub(super) system_stat: Option<Vec<NodeSystemStat>>,
    pub(super) mounts: Option<Vec<NodeMounts>>,
}

pub(super) use talos9s_core::stats::most_common;

/// Simple etcd status for header display
#[derive(Debug, Clone, Default)]
pub(super) struct EtcdSummary {
    /// Voters that answered the status call; `None` when it failed
    pub(super) healthy: Option<usize>,
    /// Voting members
    pub(super) total: usize,
    /// Whether etcd has quorum; `None` when unknown
    pub(super) has_quorum: Option<bool>,
}
/// A known node that did not answer the last refresh (e.g. rebooting)
#[derive(Debug, Clone)]
pub(super) struct OfflineNode {
    /// When the node first failed to answer
    pub(super) since: std::time::Instant,
    /// Role it had while it was online, used when no source lists it any more
    pub(super) controlplane: bool,
}

/// Version entry standing in for a node that did not answer, so it keeps its
/// place in the node list
pub(super) fn offline_version(node: &str) -> VersionInfo {
    VersionInfo {
        node: node.to_string(),
        version: String::new(),
        sha: String::new(),
        built: String::new(),
        go_version: String::new(),
        os: String::new(),
        arch: String::new(),
        platform: String::new(),
    }
}

/// Each member's first address, by hostname
pub(super) fn node_ips_of(members: &[DiscoveryMember]) -> HashMap<String, String> {
    members
        .iter()
        .filter_map(|m| Some((m.hostname.clone(), m.addresses.first()?.clone())))
        .collect()
}

/// A per-node answer from the Talos API, named by the node it came from
pub(super) trait FromNode {
    fn node(&self) -> &str;
}

macro_rules! from_node {
    ($($t:ty),*) => {
        $(impl FromNode for $t {
            fn node(&self) -> &str {
                &self.node
            }
        })*
    };
}
from_node!(
    NodeServices,
    NodeMemory,
    NodeLoadAvg,
    NodeSystemStat,
    NodeMounts,
    NodeCpuInfo
);

/// The answer for `node`. A single-node reply carries no node name, which
/// matches the unnamed node.
pub(super) fn for_node<'a, T: FromNode>(items: &'a [T], node: &str) -> Option<&'a T> {
    items
        .iter()
        .find(|i| i.node() == node || (i.node().is_empty() && node.is_empty()))
}

/// Per-cluster data storage
#[derive(Clone, Default)]
pub(super) struct ClusterData {
    /// Context/cluster name
    pub(super) name: String,
    /// Talos client for this cluster
    pub(super) client: Option<TalosClient>,
    /// Connection state
    pub(super) connected: bool,
    /// Error message if connection failed
    pub(super) error: Option<String>,
    /// Configured endpoints for this context (shown in connection-error details)
    pub(super) endpoints: Vec<String>,
    /// talosconfig context, kept so a failed connection can be retried
    pub(super) context: Option<talos_rs::Context>,
    /// True until the first connect + refresh attempt has finished
    pub(super) loading: bool,
    /// Version info from nodes
    pub(super) versions: Vec<VersionInfo>,
    /// Services from nodes
    pub(super) services: Vec<NodeServices>,
    /// Memory info from nodes
    pub(super) memory: Vec<NodeMemory>,
    /// Load average from nodes
    pub(super) load_avg: Vec<NodeLoadAvg>,
    /// CPU info from nodes
    pub(super) cpu_info: Vec<NodeCpuInfo>,
    /// System stats (boot time) from nodes
    pub(super) system_stat: Vec<NodeSystemStat>,
    /// Filesystem usage from nodes
    pub(super) mounts: Vec<NodeMounts>,
    /// The last refresh fetched the node table's columns and the Kubernetes
    /// data too (only the cluster being looked at does; the rest fetch what
    /// the tree's status dots need)
    pub(super) detailed: bool,
    /// Etcd members (control plane nodes only)
    pub(super) etcd_members: Vec<EtcdMemberInfo>,
    /// Discovery members (ALL cluster nodes)
    pub(super) discovery_members: Vec<DiscoveryMember>,
    /// Etcd summary for header
    pub(super) etcd_summary: Option<EtcdSummary>,
    /// Node hostname to IP mapping
    pub(super) node_ips: HashMap<String, String>,
    /// Whether this cluster accordion is expanded
    pub(super) expanded: bool,
    /// Whether control plane group is expanded
    pub(super) controlplane_expanded: bool,
    /// Whether workers group is expanded
    pub(super) workers_expanded: bool,
    /// Non-fatal warning when worker discovery is unavailable, so the node list
    /// (control-plane-only in that case) isn't silently misleading.
    pub(super) discovery_warning: Option<String>,
    /// Kubernetes client, created on first use and reused across refreshes
    pub(super) k8s_client: Option<kube::Client>,
    /// ZFS pools per node name (from OpenEBS ZFSNode resources)
    pub(super) zfs_pools: HashMap<String, Vec<ZfsPool>>,
    /// Kubelet version per lowercased Kubernetes node name
    pub(super) kubelet_versions: HashMap<String, String>,
    /// LINSTOR state when the cluster runs Piraeus, `None` otherwise
    pub(super) linstor: Option<LinstorStatus>,
    /// Known nodes that did not answer the last refresh, by node name. They
    /// stay in `versions` (as placeholders) so they show as offline.
    pub(super) offline: HashMap<String, OfflineNode>,
}

impl ClusterData {
    /// This cluster, as it was before a refresh that did not finish within
    /// [`CLUSTER_REFRESH_TIMEOUT`]. The client is dropped so the retry dials
    /// a fresh connection instead of waiting on a dead one again. Data shown
    /// so far stays; a first load turns into an error instead of
    /// "connecting…".
    pub(super) fn refresh_timed_out(mut self) -> Self {
        tracing::warn!(
            "Refresh of {} did not finish within {:?}; reconnecting",
            self.name,
            CLUSTER_REFRESH_TIMEOUT
        );
        let first_load = !self.connected;
        self.client = None;
        self.loading = false;
        if first_load {
            self.connected = false;
            self.error = Some(format!(
                "No answer within {}s — retrying",
                CLUSTER_REFRESH_TIMEOUT.as_secs()
            ));
        }
        self
    }

    /// A refresh that failed outright (its task panicked)
    pub(super) fn refresh_failed(&mut self, error: String) {
        self.loading = false;
        if !self.connected {
            self.error = Some(error);
        }
    }

    /// Refresh this cluster's data: etcd, roster, per-node stats.
    ///
    /// Operates only on this `ClusterData`, so it can run on a clone in a
    /// background task while the UI keeps drawing (see `start_background_refresh`).
    pub(super) async fn refresh(
        &mut self,
        config_path: Option<&str>,
        detailed: bool,
        progress: Option<&tokio::sync::watch::Sender<Option<ClusterData>>>,
    ) {
        let cluster = self;
        cluster.loading = false;

        // Not connected yet (startup) or the last attempt failed: (re)connect
        if cluster.client.is_none()
            && let Some(ctx) = &cluster.context
        {
            match TalosClient::from_context(ctx).await {
                Ok(client) => {
                    cluster.client = Some(client);
                    cluster.connected = true;
                    cluster.error = None;
                }
                Err(e) => {
                    cluster.error = Some(format!("Could not connect — {}", root_cause(&e)));
                    cluster.connected = false;
                }
            }
        }

        let Some(client) = &cluster.client else {
            return;
        };

        // Clone client to avoid borrow issues
        let client = client.clone();

        // Nodes shown before this refresh, with address and role, so one that
        // stops answering (e.g. rebooting) is kept as offline, not dropped.
        let previous: Vec<(String, String, bool)> = cluster
            .versions
            .iter()
            .filter_map(|v| {
                let ip = cluster.node_ips.get(&v.node)?.clone();
                Some((v.node.clone(), ip, cluster.is_controlplane(&v.node)))
            })
            .collect();

        // First, fetch etcd members via gRPC
        let started = std::time::Instant::now();
        let members = match tokio::time::timeout(MEMBERS_TIMEOUT, client.etcd_members()).await {
            Ok(members) => members,
            Err(_) => {
                tracing::warn!(
                    "{}: no answer within {:?}; reconnecting",
                    cluster.name,
                    MEMBERS_TIMEOUT
                );
                // Keep what is shown; the next refresh dials anew
                cluster.client = None;
                return;
            }
        };
        match members {
            Ok(members) => {
                cluster.node_ips = members
                    .iter()
                    .filter_map(|m| Some((m.hostname.clone(), m.ip_address()?)))
                    .collect();
                cluster.etcd_members = members;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch etcd members for {}: {}", cluster.name, e);
                cluster.etcd_members.clear();
            }
        }

        // Try to get discovery members (ALL nodes including workers)
        // Use context-aware async function with retry to avoid blocking and to use correct certificates
        // Pass etcd member IPs as fallback in case VIP-based discovery fails
        let context_name = cluster.name.clone();
        let fallback_ips: Vec<String> = cluster
            .etcd_members
            .iter()
            .filter_map(talos_rs::EtcdMemberInfo::ip_address)
            .collect();
        tracing::debug!("{}: etcd members in {:?}", cluster.name, started.elapsed());
        let started = std::time::Instant::now();
        let discovered =
            get_discovery_members_with_retry(&context_name, config_path, &fallback_ips).await;
        tracing::debug!("{}: discovery in {:?}", cluster.name, started.elapsed());
        match discovered {
            Ok(mut members) if !members.is_empty() => {
                let started = std::time::Instant::now();
                prefer_reachable_addresses(&client, &mut members, &cluster.known_good_ips()).await;
                tracing::debug!("{}: address probe in {:?}", cluster.name, started.elapsed());
                cluster.node_ips = node_ips_of(&members);
                cluster.discovery_members = members;
                cluster.discovery_warning = None;
            }
            outcome => {
                // Talos discovery gave us nothing usable. It's an optional service
                // and commonly disabled, so this is not fatal — log the reason.
                match &outcome {
                    Ok(_) => tracing::warn!(
                        "Discovery returned no members for {} (discovery service likely disabled)",
                        cluster.name
                    ),
                    Err(e) => tracing::warn!(
                        "Failed to fetch discovery members for {} after retries: {}",
                        cluster.name,
                        e
                    ),
                }

                // Only rebuild the roster when we have none cached. If discovery
                // succeeded on an earlier refresh, keep those members for
                // resilience rather than dropping workers on a transient failure.
                if cluster.discovery_members.is_empty() {
                    // Fall back to the Kubernetes API, which knows every node —
                    // control plane AND worker — independently of Talos discovery.
                    // This is what makes workers show up when discovery is disabled;
                    // etcd membership alone would leave the list control-plane-only.
                    // Prefer an etcd member IP (a real control plane node) for the
                    // kubeconfig fetch; fall back to the configured endpoint IP.
                    let cp_ip = cluster.control_plane_ip();

                    match cp_ip {
                        Some(cp_ip) => {
                            match k8s::discovery_members_via_k8s(&client, &cp_ip).await {
                                Ok(mut members) if !members.is_empty() => {
                                    prefer_reachable_addresses(
                                        &client,
                                        &mut members,
                                        &cluster.known_good_ips(),
                                    )
                                    .await;
                                    cluster.node_ips = node_ips_of(&members);
                                    let count = members.len();
                                    cluster.discovery_members = members;
                                    cluster.discovery_warning = None;
                                    tracing::info!(
                                        "Enumerated {} node(s) via the Kubernetes API for {} \
                                     (Talos discovery unavailable)",
                                        count,
                                        cluster.name
                                    );
                                }
                                Ok(_) => {
                                    cluster.discovery_warning = Some(
                                        "Worker nodes unavailable: Talos discovery returned no \
                                     members and the Kubernetes API reported no nodes."
                                            .to_string(),
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Kubernetes node enumeration failed for {}: {}",
                                        cluster.name,
                                        e
                                    );
                                    cluster.discovery_warning = Some(format!(
                                        "Worker nodes may be missing: Talos discovery is unavailable \
                                     and the Kubernetes API could not be reached ({e})."
                                    ));
                                }
                            }
                        }
                        None => {
                            cluster.discovery_warning = Some(
                                "Worker nodes unavailable: cluster discovery returned no members \
                                 (the discovery service may be disabled)."
                                    .to_string(),
                            );
                        }
                    }
                }
            }
        }

        // Determine which nodes to query
        let mut nodes_to_query: Vec<(String, String)> = if !cluster.discovery_members.is_empty() {
            cluster
                .discovery_members
                .iter()
                .filter_map(|m| {
                    m.addresses.first().map(|ip| {
                        let name = if !m.hostname.is_empty() {
                            m.hostname.clone()
                        } else {
                            ip.clone()
                        };
                        (name, ip.clone())
                    })
                })
                .collect()
        } else if !cluster.etcd_members.is_empty() {
            cluster
                .etcd_members
                .iter()
                .filter_map(|m| {
                    m.ip_address().map(|ip| {
                        let name = if !m.hostname.is_empty() {
                            m.hostname.clone()
                        } else {
                            ip.clone()
                        };
                        (name, ip)
                    })
                })
                .collect()
        } else if !cluster.versions.is_empty() {
            // Tertiary fallback: use nodes from previous version queries
            tracing::debug!(
                "Using version info as fallback node source for {}",
                cluster.name
            );
            cluster
                .versions
                .iter()
                .map(|v| {
                    let ip = talos_rs::host_of(&v.node).to_string();
                    (v.node.clone(), ip)
                })
                .collect()
        } else {
            tracing::warn!(
                "No node sources available for {}, skipping queries",
                cluster.name
            );
            Vec::new()
        };

        // Also query etcd members discovery doesn't list (a rebooting control
        // plane can drop out of discovery while it is still an etcd member) and
        // recently seen nodes nothing lists any more, so they show as offline.
        let listed = |nodes: &[(String, String)], name: &str, ip: &str| {
            nodes.iter().any(|(n, i)| n == name || i == ip)
        };
        for m in &cluster.etcd_members {
            if let Some(ip) = m.ip_address()
                && !m.hostname.is_empty()
                && !listed(&nodes_to_query, &m.hostname, &ip)
            {
                nodes_to_query.push((m.hostname.clone(), ip));
            }
        }
        for (name, ip, _) in &previous {
            let expired = cluster
                .offline
                .get(name)
                .is_some_and(|o| o.since.elapsed() > OFFLINE_NODE_GRACE);
            if !expired && !listed(&nodes_to_query, name, ip) {
                nodes_to_query.push((name.clone(), ip.clone()));
            }
        }
        for (name, ip) in &nodes_to_query {
            cluster
                .node_ips
                .entry(name.clone())
                .or_insert_with(|| ip.clone());
        }

        // Query the nodes, all at once, and alongside them the etcd summary
        // and (for the cluster being looked at) the Kubernetes data. Every
        // node answers what the tree's status dot needs (version, services,
        // memory); the node table's other columns only when `detailed`.
        // Snapshots go out as answers arrive, so the dashboard lists the
        // nodes at once and fills in each one when it answers instead of
        // waiting for the slowest.
        let before = cluster.clone();
        let mut results: Vec<Option<NodeFetch>> = vec![None; nodes_to_query.len()];
        let publish = |cluster: &mut ClusterData, results: &[Option<NodeFetch>]| {
            cluster.assemble_nodes(&before, &nodes_to_query, results, &previous, detailed);
            if let Some(progress) = progress {
                progress.send_replace(Some(cluster.clone()));
            }
        };
        publish(cluster, &results);

        let started = std::time::Instant::now();
        let mut pending: FuturesUnordered<_> = nodes_to_query
            .iter()
            .enumerate()
            .map(|(i, (_, ip))| {
                let node_client = client.with_node(ip);
                async move {
                    let version =
                        match tokio::time::timeout(NODE_PROBE_TIMEOUT, node_client.version()).await
                        {
                            Ok(Ok(versions)) => Ok(versions),
                            Ok(Err(e)) => Err(Some(e.to_string())),
                            Err(_) => Err(None),
                        };
                    let mut fetch = NodeFetch {
                        version,
                        services: None,
                        memory: None,
                        load_avg: None,
                        cpu_info: None,
                        system_stat: None,
                        mounts: None,
                    };
                    // Not answering: the remaining calls would only time out too
                    if fetch.version.is_err() {
                        return (i, fetch);
                    }
                    let (services, memory) =
                        tokio::join!(node_client.services(), node_client.memory());
                    fetch.services = services.ok();
                    fetch.memory = memory.ok();
                    if detailed {
                        let (load_avg, cpu_info, system_stat, mounts) = tokio::join!(
                            node_client.load_avg(),
                            node_client.cpu_info(),
                            node_client.system_stat(),
                            node_client.mounts()
                        );
                        fetch.load_avg = load_avg.ok();
                        fetch.cpu_info = cpu_info.ok();
                        fetch.system_stat = system_stat.ok();
                        fetch.mounts = mounts.ok();
                    }
                    (i, fetch)
                }
            })
            .collect();

        // etcd health for the cluster row (all control planes, by IP since
        // hostnames may not resolve)
        let cp_ips: Vec<String> = cluster
            .etcd_members
            .iter()
            .filter_map(talos_rs::EtcdMemberInfo::ip_address)
            .collect();
        let etcd_members = cluster.etcd_members.clone();
        let etcd_client = client.clone();
        let etcd = async move {
            if cp_ips.is_empty() {
                return None;
            }
            // A failed or timed-out call is unknown health, not zero healthy
            let statuses =
                tokio::time::timeout(MEMBERS_TIMEOUT, etcd_client.etcd_status_for_nodes(&cp_ips))
                    .await
                    .ok()
                    .and_then(std::result::Result::ok);
            Some(talos9s_core::EtcdQuorum::assess(
                &etcd_members,
                statuses.as_deref(),
            ))
        };

        // ZFS pool capacity, kubelet versions and LINSTOR nodes from the
        // Kubernetes API. Bounded so an unreachable kube-apiserver can't stall
        // the refresh; on failure the previous values are kept.
        let cp_ip = cluster.control_plane_ip();
        let k8s_client = cluster.k8s_client.clone();
        let k8s_talos = client.clone();
        let k8s = async move {
            let started = std::time::Instant::now();
            let outcome = tokio::time::timeout(K8S_QUERY_TIMEOUT, async {
                let k8s = match k8s_client {
                    Some(k8s) => k8s,
                    None => {
                        k8s::create_k8s_client_with_source(&k8s_talos, cp_ip.as_deref(), None)
                            .await
                            .ok()?
                            .0
                    }
                };
                let data = tokio::join!(
                    fetch_zfs_pools(&k8s),
                    fetch_kubelet_versions(&k8s),
                    fetch_linstor(&k8s)
                );
                Some((k8s, data))
            })
            .await;
            (outcome, started.elapsed())
        };
        tokio::pin!(etcd, k8s);
        let mut etcd_done = false;
        let mut k8s_done = !detailed;
        let mut last_publish = std::time::Instant::now();
        let mut unpublished = false;
        loop {
            // At most a few redraws a second, however many nodes answer; the
            // final state comes with the finished job
            let done = pending.is_empty() && etcd_done && k8s_done;
            let next_publish = tokio::time::Instant::from_std(last_publish + PROGRESS_INTERVAL);
            tokio::select! {
                _ = tokio::time::sleep_until(next_publish), if unpublished && !done => {
                    publish(cluster, &results);
                    last_publish = std::time::Instant::now();
                    unpublished = false;
                }
                Some((i, fetch)) = pending.next() => {
                    results[i] = Some(fetch);
                    unpublished = true;
                    if pending.is_empty() {
                        tracing::debug!(
                            "{}: {} nodes answered in {:?} (detailed: {})",
                            cluster.name,
                            results.len(),
                            started.elapsed(),
                            detailed
                        );
                    }
                }
                statuses = &mut etcd, if !etcd_done => {
                    etcd_done = true;
                    if let Some(quorum) = statuses {
                        cluster.etcd_summary = Some(EtcdSummary {
                            healthy: quorum.healthy,
                            total: quorum.voters,
                            has_quorum: quorum.has_quorum(),
                        });
                    }
                    unpublished = true;
                }
                (outcome, took) = &mut k8s, if !k8s_done => {
                    k8s_done = true;
                    tracing::debug!("{}: Kubernetes stage took {:?}", cluster.name, took);
                    cluster.apply_k8s_data(outcome);
                    unpublished = true;
                }
                else => break,
            }
        }
        cluster.assemble_nodes(&before, &nodes_to_query, &results, &previous, detailed);

        // Reflect real reachability so the UI shows a clear error instead of an
        // empty/blank cluster when no endpoint can be reached. Healthy clusters
        // have versions/etcd/discovery data; a transient failure keeps the
        // previously cached data, so this only trips when there is nothing at all.
        {
            // Offline placeholders don't count: they are nodes that didn't answer
            let has_any_data = cluster.versions.len() > cluster.offline.len()
                || !cluster.etcd_members.is_empty()
                || !cluster.discovery_members.is_empty();
            if has_any_data {
                cluster.connected = true;
                cluster.error = None;
            } else {
                cluster.connected = false;
                cluster.error = Some(
                    "Unable to reach any configured Talos endpoint. Check network \
                     connectivity and the endpoints in your talosconfig."
                        .to_string(),
                );
            }
        }
        cluster.detailed = detailed;
    }

    /// Store what the Kubernetes stage fetched; anything that failed keeps
    /// its previous value
    #[allow(clippy::type_complexity)]
    pub(super) fn apply_k8s_data(
        &mut self,
        outcome: Result<
            Option<(
                kube::Client,
                (
                    Result<HashMap<String, Vec<ZfsPool>>, kube::Error>,
                    Result<HashMap<String, String>, kube::Error>,
                    Result<Option<LinstorStatus>, kube::Error>,
                ),
            )>,
            tokio::time::error::Elapsed,
        >,
    ) {
        match outcome {
            Ok(Some((k8s, (zfs, kubelets, linstor)))) => {
                self.k8s_client = Some(k8s);
                match linstor {
                    Ok(status) => self.linstor = status,
                    Err(e) => tracing::debug!("LINSTOR unavailable for {}: {}", self.name, e),
                }
                match zfs {
                    Ok(pools) => self.zfs_pools = pools,
                    Err(e) => tracing::debug!("ZFS pools unavailable for {}: {}", self.name, e),
                }
                match kubelets {
                    Ok(versions) => self.kubelet_versions = versions,
                    Err(e) => {
                        tracing::debug!("Kubelet versions unavailable for {}: {}", self.name, e);
                    }
                }
            }
            Ok(None) => tracing::debug!("Kubernetes API unavailable for {}", self.name),
            Err(_) => tracing::debug!("Kubernetes query timed out for {}", self.name),
        }
    }

    /// Node data from what the nodes answered so far, in roster order. A node
    /// that hasn't answered yet keeps what `before` had for it, or gets a
    /// placeholder row so it is listed from the start; one that didn't
    /// answer at all is offline. Columns not fetched in a light refresh keep
    /// their previous values.
    pub(super) fn assemble_nodes(
        &mut self,
        before: &ClusterData,
        nodes: &[(String, String)],
        results: &[Option<NodeFetch>],
        previous: &[(String, String, bool)],
        detailed: bool,
    ) {
        fn keep<T: Clone>(from: &[T], node: &str, node_of: impl Fn(&T) -> &str) -> Vec<T> {
            from.iter()
                .filter(|x| node_of(x) == node)
                .cloned()
                .collect()
        }
        fn named<T>(mut items: Vec<T>, node: &str, set: impl Fn(&mut T, String)) -> Vec<T> {
            for item in &mut items {
                set(item, node.to_string());
            }
            items
        }
        let mut versions = Vec::new();
        let mut services = Vec::new();
        let mut memory = Vec::new();
        let mut load_avg = Vec::new();
        let mut cpu_info = Vec::new();
        let mut system_stat = Vec::new();
        let mut mounts = Vec::new();
        let mut offline = HashMap::new();
        for ((node, _), result) in nodes.iter().zip(results) {
            let node = node.as_str();
            let kept_details = |load_avg: &mut Vec<NodeLoadAvg>,
                                cpu_info: &mut Vec<NodeCpuInfo>,
                                system_stat: &mut Vec<NodeSystemStat>,
                                mounts: &mut Vec<NodeMounts>| {
                load_avg.extend(keep(&before.load_avg, node, |x| &x.node));
                cpu_info.extend(keep(&before.cpu_info, node, |x| &x.node));
                system_stat.extend(keep(&before.system_stat, node, |x| &x.node));
                mounts.extend(keep(&before.mounts, node, |x| &x.node));
            };
            match result {
                // Not answered yet: what was shown before, or a placeholder
                None => {
                    let shown = keep(&before.versions, node, |v| &v.node);
                    if shown.is_empty() {
                        versions.push(offline_version(node));
                    }
                    versions.extend(shown);
                    services.extend(keep(&before.services, node, |x| &x.node));
                    memory.extend(keep(&before.memory, node, |x| &x.node));
                    kept_details(&mut load_avg, &mut cpu_info, &mut system_stat, &mut mounts);
                    if let Some(o) = before.offline.get(node) {
                        offline.insert(node.to_string(), o.clone());
                    }
                }
                Some(NodeFetch {
                    version: Err(error),
                    ..
                }) => {
                    if let Some(e) = error {
                        tracing::debug!("Node {} is offline: {}", node, e);
                    }
                    let since = before
                        .offline
                        .get(node)
                        .map_or_else(std::time::Instant::now, |o| o.since);
                    let controlplane = previous
                        .iter()
                        .find(|(n, _, _)| n == node)
                        .map_or_else(|| before.is_controlplane(node), |p| p.2);
                    offline.insert(
                        node.to_string(),
                        OfflineNode {
                            since,
                            controlplane,
                        },
                    );
                    versions.push(offline_version(node));
                }
                Some(fetch) => {
                    if let Ok(nv) = &fetch.version {
                        versions.extend(named(nv.clone(), node, |v, n| v.node = n));
                    }
                    if let Some(x) = &fetch.services {
                        services.extend(named(x.clone(), node, |v, n| v.node = n));
                    }
                    if let Some(x) = &fetch.memory {
                        memory.extend(named(x.clone(), node, |v, n| v.node = n));
                    }
                    if detailed {
                        if let Some(x) = &fetch.load_avg {
                            load_avg.extend(named(x.clone(), node, |v, n| v.node = n));
                        }
                        if let Some(x) = &fetch.cpu_info {
                            cpu_info.extend(named(x.clone(), node, |v, n| v.node = n));
                        }
                        if let Some(x) = &fetch.system_stat {
                            system_stat.extend(named(x.clone(), node, |v, n| v.node = n));
                        }
                        if let Some(x) = &fetch.mounts {
                            mounts.extend(named(x.clone(), node, |v, n| v.node = n));
                        }
                    } else {
                        kept_details(&mut load_avg, &mut cpu_info, &mut system_stat, &mut mounts);
                    }
                }
            }
        }
        self.versions = versions;
        self.services = services;
        self.memory = memory;
        self.load_avg = load_avg;
        self.cpu_info = cpu_info;
        self.system_stat = system_stat;
        self.mounts = mounts;
        self.offline = offline;
    }

    /// Whether a node is a control plane node.
    ///
    /// Discovery `machineType` is authoritative when available; then etcd
    /// membership (only control planes run etcd), the role remembered for an
    /// offline node, and finally the etcd service. Keeping these in one place
    /// ensures the control-plane and worker groups stay complementary.
    pub(super) fn is_controlplane(&self, node: &str) -> bool {
        let key = talos_rs::host_of(node);
        if let Some(member) = self.discovery_members.iter().find(|m| {
            m.hostname.eq_ignore_ascii_case(node)
                || m.hostname.eq_ignore_ascii_case(key)
                || m.addresses.iter().any(|a| a == node || a == key)
        }) {
            return member.machine_type.eq_ignore_ascii_case("controlplane");
        }
        if self
            .etcd_members
            .iter()
            .any(|m| m.hostname.eq_ignore_ascii_case(key))
        {
            return true;
        }
        if let Some(offline) = self.offline.get(node) {
            return offline.controlplane;
        }
        self.services
            .iter()
            .find(|s| s.node == node)
            .is_some_and(|s| s.services.iter().any(|svc| svc.id == "etcd"))
    }

    /// Addresses known to reach their node: etcd member IPs (etcd peers talk
    /// over them) and the addresses chosen on the previous refresh. Tried first
    /// so a healthy refresh needs a single probe per node.
    pub(super) fn known_good_ips(&self) -> Vec<String> {
        self.etcd_members
            .iter()
            .filter_map(talos_rs::EtcdMemberInfo::ip_address)
            .chain(
                self.discovery_members
                    .iter()
                    .filter_map(|m| m.addresses.first().cloned()),
            )
            .collect()
    }

    /// Best control plane node IP for fetching a cluster-correct kubeconfig.
    /// Prefers an etcd member (etcd runs only on control planes), then a roster
    /// `controlplane` member, then the configured endpoint.
    pub(super) fn control_plane_ip(&self) -> Option<String> {
        self.etcd_members
            .iter()
            .find_map(talos_rs::EtcdMemberInfo::ip_address)
            .or_else(|| {
                self.discovery_members
                    .iter()
                    .find(|m| m.machine_type == "controlplane")
                    .and_then(|m| m.addresses.first().cloned())
            })
            .or_else(|| {
                self.endpoints
                    .first()
                    .map(|e| talos_rs::host_of(e).to_string())
            })
    }
}

/// Background `get_node_network_info` calls, keyed by (context, node name)
/// Stats of one node, fetched in the background for the selected node.
/// A call that failed or timed out is `None`, keeping the previous values.
pub(super) struct NodeStatsFetch {
    /// Context of the cluster the node belongs to
    pub(super) context: String,
    pub(super) node: String,
    pub(super) services: Option<Vec<NodeServices>>,
    pub(super) memory: Option<Vec<NodeMemory>>,
    pub(super) load_avg: Option<Vec<NodeLoadAvg>>,
    pub(super) system_stat: Option<Vec<NodeSystemStat>>,
    pub(super) mounts: Option<Vec<NodeMounts>>,
}

/// How long each selected-node stats call may take
pub(super) const NODE_STATS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) type NetworkFetch = tokio::task::JoinHandle<
    Vec<(
        (String, String),
        Result<NodeNetworkInfo, talos_rs::TalosError>,
        Option<NodeClock>,
    )>,
>;

/// A node's clock against its NTP server, from the Talos Time API
#[derive(Debug, Clone, PartialEq)]
pub(super) struct NodeClock {
    /// Node time minus the NTP server's (seconds, positive = node ahead)
    pub(super) offset_secs: f64,
    pub(super) server: String,
}

/// Upper bound for one node's Time API call
pub(super) const CLOCK_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Ask a node for its time and its NTP server's. `None` unless both are
/// known (the client library reads a missing time as a zero offset).
pub(super) async fn fetch_node_clock(client: TalosClient) -> Option<NodeClock> {
    let times = tokio::time::timeout(CLOCK_QUERY_TIMEOUT, client.time())
        .await
        .ok()?
        .ok()?;
    let time = times.into_iter().next()?;
    let (local, remote) = (time.local_time?, time.remote_time?);
    let offset_secs = match local.duration_since(remote) {
        Ok(ahead) => ahead.as_secs_f64(),
        Err(behind) => -behind.duration().as_secs_f64(),
    };
    Some(NodeClock {
        offset_secs,
        server: time.server,
    })
}

/// Upper bound for the kube-apiserver queries (ZFS pools, kubelet versions,
/// LINSTOR nodes) in a cluster refresh
pub(super) const K8S_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Kubelet version of every Kubernetes node, keyed by lowercased node name
pub(super) async fn fetch_kubelet_versions(
    client: &kube::Client,
) -> Result<HashMap<String, String>, kube::Error> {
    let nodes: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(client.clone());
    Ok(nodes
        .list(&kube::api::ListParams::default())
        .await?
        .items
        .into_iter()
        .filter_map(|node| {
            let version = node.status?.node_info?.kubelet_version;
            Some((node.metadata.name?.to_lowercase(), version))
        })
        .collect())
}
