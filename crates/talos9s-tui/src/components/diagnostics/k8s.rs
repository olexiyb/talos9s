//! Kubernetes client helper for diagnostics
//!
//! Creates a K8s client from Talos-provided kubeconfig.

use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::serde_json::json;
use kube::{
    Client, Config,
    api::{Api, EvictParams, ListParams, Patch, PatchParams},
};
use talos_rs::{DiscoveryMember, TalosClient};

/// Error type for K8s operations
#[derive(Debug, thiserror::Error)]
pub enum K8sError {
    #[error("Failed to get kubeconfig from Talos: {0}")]
    KubeconfigFetch(String),
    #[error("Failed to parse kubeconfig: {0}")]
    KubeconfigParse(String),
    #[error("Failed to create K8s client: {0}")]
    ClientCreate(String),
    #[error("K8s API error: {0}")]
    ApiError(String),
}

/// Source of the kubeconfig used to create the K8s client
#[derive(Debug, Clone)]
pub enum KubeconfigSource {
    /// From KUBECONFIG environment variable or default path (~/.kube/config),
    /// with the context used (for `kubectl --context` in suggested commands)
    Environment(Option<String>),
    /// Fetched from a specific Talos control plane node
    TalosNode(String),
    /// Source unknown or unavailable
    Unavailable(String),
}

/// How long fetching a kubeconfig and building a client may take
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// [`create_k8s_client_with_source`] bounded by [`CONNECT_TIMEOUT`], with
/// one error text for both failures. Pin `cp_node_ip` to a control plane
/// node of the cluster being viewed: an ambient KUBECONFIG may point at
/// another cluster.
pub async fn connect(
    talos_client: &TalosClient,
    cp_node_ip: Option<&str>,
) -> Result<(Client, KubeconfigSource), String> {
    match tokio::time::timeout(
        CONNECT_TIMEOUT,
        create_k8s_client_with_source(talos_client, cp_node_ip, None),
    )
    .await
    {
        Ok(Ok(created)) => {
            tracing::info!("K8s client created from: {:?}", created.1);
            Ok(created)
        }
        Ok(Err(e)) => {
            tracing::warn!("Failed to create K8s client: {}", e);
            Err(e.to_string())
        }
        Err(_) => {
            let e = format!(
                "timed out after {}s fetching the kubeconfig",
                CONNECT_TIMEOUT.as_secs()
            );
            tracing::warn!("Failed to create K8s client: {}", e);
            Err(e)
        }
    }
}

/// Create a Kubernetes client and return the source of the kubeconfig
///
/// Match-and-honor: when we can pin to a control plane node (`cp_node_ip`), that
/// node's kubeconfig identifies the cluster we launched against. We look through
/// **every** context of the ambient `KUBECONFIG` for one naming the same cluster
/// (matching CA) — not just its current context, since with several clusters
/// open at once only one can be current. A match is preferred: it may reach the
/// cluster by a more convenient endpoint (VIP/LB) with the user's own
/// credentials. Contexts naming other clusters are never used, so read views
/// never show — and drain/cordon never mutate — the wrong cluster. When no node
/// is available to pin to (both args `None`), we honor the ambient `KUBECONFIG`
/// per standard tooling conventions.
///
/// Sources, in order:
/// 1. Pinned control plane node kubeconfig, or a `KUBECONFIG` context with the same CA
/// 2. Provided kubeconfig client (a control plane used while diagnosing a worker)
/// 3. Ambient `KUBECONFIG` (env or default path)
/// 4. Main Talos client via VIP (may fail with multiple nodes configured)
///
/// # Arguments
/// * `talos_client` - The main Talos client (connected to VIP or endpoint)
/// * `cp_node_ip` - Optional control plane node IP to target for kubeconfig fetch
/// * `kubeconfig_client` - Optional pre-configured client targeting a control plane node
///
/// # Returns
/// A tuple of (Client, KubeconfigSource) on success
pub async fn create_k8s_client_with_source(
    talos_client: &TalosClient,
    cp_node_ip: Option<&str>,
    kubeconfig_client: Option<&TalosClient>,
) -> Result<(Client, KubeconfigSource), K8sError> {
    if let Some(node_ip) = cp_node_ip {
        tracing::debug!("Targeting control plane node {} for kubeconfig", node_ip);
        let node_client = talos_client.with_node(node_ip);
        match fetch_node_kubeconfig(&node_client).await {
            Ok(node_kubeconfig) => {
                // A KUBECONFIG context names the SAME cluster: prefer it.
                if let Some((ambient, context)) = current_context_cluster_ca(&node_kubeconfig)
                    .and_then(|ca| ambient_context_for_ca(&ca))
                {
                    match client_from_kubeconfig(ambient, &node_client).await {
                        Ok(client) => {
                            tracing::debug!(
                                "KUBECONFIG context {} matches the Talos cluster; using it",
                                context
                            );
                            return Ok((client, KubeconfigSource::Environment(Some(context))));
                        }
                        // e.g. exec/OIDC auth we can't run: use the node's kubeconfig.
                        Err(e) => tracing::debug!(
                            "KUBECONFIG context {} matches but is unusable ({}); \
                             using the node's kubeconfig",
                            context,
                            e
                        ),
                    }
                }
                // No matching context (or CAs can't be compared): the node's
                // kubeconfig is always the right cluster.
                return Ok((
                    client_from_kubeconfig(node_kubeconfig, &node_client).await?,
                    KubeconfigSource::TalosNode(node_ip.to_string()),
                ));
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to fetch kubeconfig from node {}: {} (falling back)",
                    node_ip,
                    e
                );
                // Fall through to try other methods
            }
        }
    }

    // Next, a provided kubeconfig client (e.g. a control plane node used to serve
    // the kubeconfig while diagnosing a worker) — also cluster-correct.
    if let Some(kc_client) = kubeconfig_client {
        tracing::debug!("Trying provided kubeconfig client");
        match fetch_kubeconfig_from_client(kc_client).await {
            Ok(client) => {
                return Ok((
                    client,
                    KubeconfigSource::TalosNode("control-plane".to_string()),
                ));
            }
            Err(e) => {
                tracing::warn!("Failed to fetch kubeconfig from provided client: {}", e);
            }
        }
    }

    // No control plane node to pin to: honor the ambient KUBECONFIG (env or
    // default path), respecting standard K8s tooling conventions.
    if let Ok(config) = Config::infer().await
        && let Ok(client) = Client::try_from(config)
    {
        tracing::debug!("Using kubeconfig from environment (KUBECONFIG or default path)");
        let context = kube::config::Kubeconfig::read()
            .ok()
            .and_then(|k| k.current_context);
        return Ok((client, KubeconfigSource::Environment(context)));
    }

    // Last resort: try the main client (may fail with multiple nodes configured)
    tracing::debug!("Trying main Talos client for kubeconfig (may fail with multiple nodes)");
    match fetch_kubeconfig_from_client(talos_client).await {
        Ok(client) => Ok((client, KubeconfigSource::TalosNode("vip".to_string()))),
        Err(e) => Err(e),
    }
}

/// Create a Kubernetes client by fetching the kubeconfig directly from a
/// specific Talos control plane node, bypassing the environment `KUBECONFIG`.
///
/// Unlike [`create_k8s_client_with_source`], this never consults the ambient
/// `KUBECONFIG`. That matters for node enumeration: we must talk to *this*
/// cluster's API server, and an environment kubeconfig can point at a
/// different cluster entirely — listing another cluster's nodes here would be
/// a silent false positive. The Talos-served kubeconfig is scoped to the node
/// we ask, so it always identifies the correct cluster.
pub async fn create_k8s_client_from_node(
    talos_client: &TalosClient,
    cp_node_ip: &str,
) -> Result<Client, K8sError> {
    let node_client = talos_client.with_node(cp_node_ip);
    fetch_kubeconfig_from_client(&node_client).await
}

/// Fetch and parse the kubeconfig a Talos node serves.
async fn fetch_node_kubeconfig(client: &TalosClient) -> Result<kube::config::Kubeconfig, K8sError> {
    let kubeconfig_yaml = client
        .kubeconfig()
        .await
        .map_err(|e| K8sError::KubeconfigFetch(e.to_string()))?;
    serde_saphyr::from_str(&kubeconfig_yaml).map_err(|e| K8sError::KubeconfigParse(e.to_string()))
}

/// Build a K8s client from an already-parsed kubeconfig.
///
/// `talos` is the client the kubeconfig came from; its node and endpoint
/// addresses are the fallback way to reach the API server when the
/// kubeconfig's hostname doesn't resolve from here.
async fn client_from_kubeconfig(
    mut kubeconfig: kube::config::Kubeconfig,
    talos: &TalosClient,
) -> Result<Client, K8sError> {
    reach_server_by_ip_if_unresolvable(&mut kubeconfig, &talos.known_addresses()).await;
    let config = Config::from_custom_kubeconfig(kubeconfig, &Default::default())
        .await
        .map_err(|e| K8sError::ClientCreate(e.to_string()))?;
    Client::try_from(config).map_err(|e| K8sError::ClientCreate(e.to_string()))
}

/// Fetch kubeconfig from a specific Talos client and create a K8s client
async fn fetch_kubeconfig_from_client(client: &TalosClient) -> Result<Client, K8sError> {
    client_from_kubeconfig(fetch_node_kubeconfig(client).await?, client).await
}

/// Upper bound for resolving the API server name and for each TCP probe
const SERVER_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Split a kubeconfig server URL into (host, port); IPv6 hosts lose brackets
fn server_host_port(server: &str) -> Option<(String, u16)> {
    let authority = server
        .strip_prefix("https://")
        .or_else(|| server.strip_prefix("http://"))
        .unwrap_or(server)
        .split('/')
        .next()?;
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest.split_once(']')?;
        let port = port
            .strip_prefix(':')
            .map_or(Some(443), |p| p.parse().ok())?;
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((authority.to_string(), 443)),
    }
}

/// Clusters often publish the API server under an internal DNS name (e.g.
/// `kube.prod.example.internal`) that only resolves inside their network or
/// over a split-DNS VPN. When that name doesn't resolve from here, point the
/// kubeconfig at the first of `candidates` (node / endpoint IPs) accepting TCP
/// on the API port, keeping the original name for TLS verification so the
/// server certificate is still checked against it.
async fn reach_server_by_ip_if_unresolvable(
    kubeconfig: &mut kube::config::Kubeconfig,
    candidates: &[String],
) {
    let Some(cluster) = kubeconfig
        .current_context
        .as_deref()
        .and_then(|name| kubeconfig.contexts.iter().find(|c| c.name == name))
        .and_then(|c| c.context.as_ref())
        .map(|c| c.cluster.clone())
        .and_then(|name| kubeconfig.clusters.iter_mut().find(|c| c.name == name))
        .and_then(|c| c.cluster.as_mut())
    else {
        return;
    };
    let Some((host, port)) = cluster.server.as_deref().and_then(server_host_port) else {
        return;
    };
    if host.parse::<std::net::IpAddr>().is_ok() {
        return;
    }
    let resolves = tokio::time::timeout(
        SERVER_PROBE_TIMEOUT,
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await
    .is_ok_and(|r| r.is_ok_and(|mut addrs| addrs.next().is_some()));
    if resolves {
        return;
    }

    for ip in candidates {
        let Ok(addr) = ip.parse::<std::net::IpAddr>() else {
            continue;
        };
        let connect = tokio::net::TcpStream::connect((addr, port));
        if matches!(
            tokio::time::timeout(SERVER_PROBE_TIMEOUT, connect).await,
            Ok(Ok(_))
        ) {
            let server = match addr {
                std::net::IpAddr::V4(v4) => format!("https://{v4}:{port}"),
                std::net::IpAddr::V6(v6) => format!("https://[{v6}]:{port}"),
            };
            tracing::info!(
                "Kubernetes API server {} does not resolve; reaching it via {}",
                host,
                server
            );
            cluster.server = Some(server);
            cluster.tls_server_name.get_or_insert(host);
            return;
        }
    }
    tracing::warn!(
        "Kubernetes API server {} does not resolve and none of {:?} accept connections on port {}",
        host,
        candidates,
        port
    );
}

/// The CA cert (base64 PEM) of the cluster a kubeconfig's current context points
/// at. Two kubeconfigs sharing this value describe the *same* cluster, whether
/// they reach it by a node IP or a VIP/load-balancer — so it's a reliable
/// same-cluster key that doesn't false-mismatch on endpoint differences.
///
/// Returns `None` when the kubeconfig has no current context, no matching
/// cluster, or a file-path CA rather than inline data — cases where we can't
/// cheaply compare, and so treat as "unknown" rather than risk a false mismatch.
fn current_context_cluster_ca(kubeconfig: &kube::config::Kubeconfig) -> Option<String> {
    context_cluster_ca(kubeconfig, kubeconfig.current_context.as_deref()?)
}

/// The inline CA cert of the cluster that context `ctx_name` points at.
fn context_cluster_ca(kubeconfig: &kube::config::Kubeconfig, ctx_name: &str) -> Option<String> {
    let ctx = kubeconfig.contexts.iter().find(|c| c.name == ctx_name)?;
    let cluster_name = &ctx.context.as_ref()?.cluster;
    let cluster = kubeconfig
        .clusters
        .iter()
        .find(|c| &c.name == cluster_name)?;
    cluster.cluster.as_ref()?.certificate_authority_data.clone()
}

/// The ambient `KUBECONFIG` (env var or default `~/.kube/config`) switched to a
/// context whose cluster has CA `ca`, plus that context's name. The current
/// context wins when it matches; otherwise the first matching context is used.
/// `None` when no kubeconfig is present/readable or no context matches.
fn ambient_context_for_ca(ca: &str) -> Option<(kube::config::Kubeconfig, String)> {
    let mut kubeconfig = kube::config::Kubeconfig::read().ok()?;
    let context = kubeconfig
        .current_context
        .iter()
        .chain(kubeconfig.contexts.iter().map(|c| &c.name))
        .find(|name| context_cluster_ca(&kubeconfig, name).as_deref() == Some(ca))?
        .clone();
    kubeconfig.current_context = Some(context.clone());
    Some((kubeconfig, context))
}

/// A cluster node as seen by the Kubernetes API.
///
/// Every node — control plane and worker — registers a `Node` object with the
/// API server independently of the Talos discovery service, which makes this
/// the reliable source of truth for the full roster when discovery is disabled.
#[derive(Debug, Clone, PartialEq)]
pub struct K8sNodeInfo {
    /// Node name (matches the Talos hostname).
    pub name: String,
    /// Primary internal IP, used to reach the node's Talos API. `None` if the
    /// node has no `InternalIP` address (then it cannot be queried).
    pub internal_ip: Option<String>,
    /// Whether the node carries a control-plane role label.
    pub is_control_plane: bool,
}

/// Extract roster info from a single Kubernetes `Node`.
///
/// Prefers the `InternalIP` address (the kubelet node IP, which is also the
/// Talos API address) and reads the role from the standard control-plane role
/// labels (`node-role.kubernetes.io/control-plane`, or the legacy `master`).
fn node_info_from(node: &Node) -> K8sNodeInfo {
    let name = node.metadata.name.clone().unwrap_or_default();

    let internal_ip = node
        .status
        .as_ref()
        .and_then(|s| s.addresses.as_ref())
        .and_then(|addrs| {
            addrs
                .iter()
                .find(|a| a.type_ == "InternalIP")
                .map(|a| a.address.clone())
        });

    let is_control_plane = node.metadata.labels.as_ref().is_some_and(|labels| {
        labels.contains_key("node-role.kubernetes.io/control-plane")
            || labels.contains_key("node-role.kubernetes.io/master")
    });

    K8sNodeInfo {
        name,
        internal_ip,
        is_control_plane,
    }
}

/// List all cluster nodes via the Kubernetes API.
///
/// This is the discovery-independent fallback for enumerating nodes (including
/// workers) when the Talos discovery service is disabled and
/// `talosctl get members` returns nothing.
pub async fn list_cluster_nodes(client: &Client) -> Result<Vec<K8sNodeInfo>, K8sError> {
    let nodes: Api<Node> = Api::all(client.clone());
    let list = nodes
        .list(&ListParams::default())
        .await
        .map_err(|e| K8sError::ApiError(format!("Failed to list nodes: {e}")))?;
    Ok(list.items.iter().map(node_info_from).collect())
}

/// Enumerate all cluster nodes (control plane and workers) through the
/// Kubernetes API and present them as [`DiscoveryMember`]s.
///
/// This is the shared fallback for when the Talos discovery service is disabled
/// and `talosctl get members` returns nothing. Every node registers with the
/// Kubernetes API independently of Talos discovery, so — unlike etcd
/// membership, which only ever lists control plane nodes — it can reveal
/// workers. The kubeconfig is fetched straight from `cp_ip` (a control plane
/// node) so we always query the correct cluster's API server, never an
/// unrelated one an ambient `KUBECONFIG` might point at.
pub async fn discovery_members_via_k8s(
    client: &TalosClient,
    cp_ip: &str,
) -> Result<Vec<DiscoveryMember>, K8sError> {
    let k8s_client = create_k8s_client_from_node(client, cp_ip).await?;
    let nodes = list_cluster_nodes(&k8s_client).await?;
    Ok(k8s_nodes_to_discovery_members(nodes))
}

/// Map Kubernetes node roster entries onto [`DiscoveryMember`]s.
///
/// Nodes without an internal IP are dropped: a node's Talos API is reached by
/// IP, so an IP-less node can neither be queried nor displayed. The
/// control-plane role label maps to the `controlplane`/`worker` machine type
/// that the node-classification logic keys on.
pub fn k8s_nodes_to_discovery_members(nodes: Vec<K8sNodeInfo>) -> Vec<DiscoveryMember> {
    nodes
        .into_iter()
        .filter_map(|n| {
            let ip = n.internal_ip?;
            Some(DiscoveryMember {
                id: n.name.clone(),
                addresses: vec![ip],
                hostname: n.name,
                machine_type: if n.is_control_plane {
                    "controlplane".to_string()
                } else {
                    "worker".to_string()
                },
                operating_system: String::new(),
            })
        })
        .collect()
}

/// Detected CNI information from K8s
#[derive(Debug, Clone, Default)]
pub struct CniInfo {
    /// Detected CNI type
    pub cni_type: super::types::CniType,
    /// CNI pods in kube-system
    pub pods: Vec<CniPodInfo>,
}

/// Information about a CNI pod
#[derive(Debug, Clone)]
pub struct CniPodInfo {
    /// Pod name
    pub name: String,
    /// Node the pod is running on
    pub node_name: Option<String>,
    /// Pod phase (Running, Pending, etc.)
    pub phase: String,
    /// Whether pod is ready
    pub ready: bool,
    /// Number of restarts
    pub restart_count: i32,
}

/// Detect CNI type by checking pods in kube-system namespace
pub async fn detect_cni_from_k8s(client: &Client) -> Result<CniInfo, K8sError> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), "kube-system");

    let pod_list = pods
        .list(&ListParams::default())
        .await
        .map_err(|e| K8sError::ApiError(e.to_string()))?;

    let mut cni_info = CniInfo::default();

    for pod in pod_list.items {
        let name = pod.metadata.name.clone().unwrap_or_default();
        let name_lower = name.to_lowercase();

        // Detect CNI type from pod names
        let is_cni_pod =
            if name_lower.starts_with("kube-flannel") || name_lower.starts_with("flannel") {
                cni_info.cni_type = super::types::CniType::Flannel;
                true
            } else if name_lower.starts_with("cilium") {
                cni_info.cni_type = super::types::CniType::Cilium;
                true
            } else if name_lower.starts_with("calico") || name_lower.starts_with("calico-node") {
                cni_info.cni_type = super::types::CniType::Calico;
                true
            } else {
                false
            };

        if is_cni_pod {
            let status = pod.status.as_ref();
            let phase = status
                .and_then(|s| s.phase.clone())
                .unwrap_or_else(|| "Unknown".to_string());

            // Check if pod is ready
            let ready = status
                .and_then(|s| s.conditions.as_ref())
                .map(|conditions| {
                    conditions
                        .iter()
                        .any(|c| c.type_ == "Ready" && c.status == "True")
                })
                .unwrap_or(false);

            // Get restart count from container statuses
            let restart_count = status
                .and_then(|s| s.container_statuses.as_ref())
                .map(|containers| containers.iter().map(|c| c.restart_count).sum())
                .unwrap_or(0);

            // Get node name from pod spec
            let node_name = pod.spec.as_ref().and_then(|s| s.node_name.clone());

            cni_info.pods.push(CniPodInfo {
                name,
                node_name,
                phase,
                ready,
                restart_count,
            });
        }
    }

    Ok(cni_info)
}

/// Information about an unhealthy pod
#[derive(Debug, Clone)]
pub struct UnhealthyPodInfo {
    /// Pod name
    pub name: String,
    /// Pod namespace
    pub namespace: String,
    /// Container state (e.g., "CrashLoopBackOff", "ImagePullBackOff")
    pub state: String,
    /// Number of restarts
    pub restart_count: i32,
    /// Last termination reason (if any)
    pub last_reason: Option<String>,
}

/// Pod health summary from K8s API
#[derive(Debug, Clone, Default)]
pub struct PodHealthInfo {
    /// Pods in CrashLoopBackOff
    pub crashing: Vec<UnhealthyPodInfo>,
    /// Pods in ImagePullBackOff
    pub image_pull_errors: Vec<UnhealthyPodInfo>,
    /// Pods stuck in Pending
    pub pending: Vec<UnhealthyPodInfo>,
    /// Total pod count
    pub total_pods: usize,
}

impl PodHealthInfo {
    /// Check if there are any unhealthy pods
    pub fn has_issues(&self) -> bool {
        !self.crashing.is_empty() || !self.image_pull_errors.is_empty()
    }

    /// Get summary message
    pub fn summary(&self) -> String {
        if self.crashing.is_empty() && self.image_pull_errors.is_empty() && self.pending.is_empty()
        {
            "All pods healthy".to_string()
        } else {
            let mut parts = Vec::new();
            if !self.crashing.is_empty() {
                parts.push(format!("{} crashing", self.crashing.len()));
            }
            if !self.image_pull_errors.is_empty() {
                parts.push(format!("{} image errors", self.image_pull_errors.len()));
            }
            if !self.pending.is_empty() {
                parts.push(format!("{} pending", self.pending.len()));
            }
            parts.join(", ")
        }
    }
}

/// Check pod health across all namespaces using K8s API
///
/// This is the authoritative way to check for crashing pods - no log parsing!
pub async fn check_pod_health(
    client: &Client,
    node: Option<&str>,
) -> Result<PodHealthInfo, K8sError> {
    let pods: Api<Pod> = Api::all(client.clone());
    // A node's own pods: the API filters them, instead of sending every pod
    // of the cluster (seconds on a big one)
    let params = match node {
        Some(node) => ListParams::default().fields(&format!("spec.nodeName={node}")),
        None => ListParams::default(),
    };

    let pod_list = pods
        .list(&params)
        .await
        .map_err(|e| K8sError::ApiError(e.to_string()))?;

    Ok(pod_health_from(
        &pod_list.items,
        chrono::Utc::now().timestamp(),
    ))
}

/// Unhealthy pods among `pods`, each listed once (under its first stuck
/// container). Pending counts once a pod has waited past the grace time;
/// `now` is seconds since the epoch.
fn pod_health_from(pods: &[Pod], now: i64) -> PodHealthInfo {
    use talos9s_core::constants::POD_PENDING_GRACE_SECS;
    let mut info = PodHealthInfo {
        total_pods: pods.len(),
        ..Default::default()
    };

    for pod in pods {
        let name = pod.metadata.name.clone().unwrap_or_default();
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();
        let status = pod.status.as_ref();

        let init = status
            .and_then(|s| s.init_container_statuses.as_deref())
            .unwrap_or_default();
        let containers = status
            .and_then(|s| s.container_statuses.as_deref())
            .unwrap_or_default();
        let stuck = init.iter().chain(containers).find_map(|cs| {
            let reason = cs.state.as_ref()?.waiting.as_ref()?.reason.clone()?;
            matches!(
                reason.as_str(),
                "CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull"
            )
            .then_some((cs, reason))
        });
        if let Some((cs, reason)) = stuck {
            let pod_info = UnhealthyPodInfo {
                name,
                namespace,
                state: reason.clone(),
                restart_count: cs.restart_count,
                last_reason: cs
                    .last_state
                    .as_ref()
                    .and_then(|s| s.terminated.as_ref())
                    .and_then(|t| t.reason.clone()),
            };
            if reason == "CrashLoopBackOff" {
                info.crashing.push(pod_info);
            } else {
                info.image_pull_errors.push(pod_info);
            }
            continue;
        }

        // Pending past the grace time: being scheduled or pulled is normal
        let pending = status.and_then(|s| s.phase.as_deref()) == Some("Pending");
        let age = pod
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|ts| now - ts.0.as_second());
        if pending && age.is_none_or(|a| a >= POD_PENDING_GRACE_SECS) {
            info.pending.push(UnhealthyPodInfo {
                name,
                namespace,
                state: "Pending".to_string(),
                restart_count: 0,
                last_reason: None,
            });
        }
    }

    info
}

/// Information about a PodDisruptionBudget
#[derive(Debug, Clone)]
pub struct PdbInfo {
    /// PDB name
    pub name: String,
    /// Namespace
    pub namespace: String,
    /// Current number of healthy pods
    pub current_healthy: i32,
    /// Desired number of healthy pods (minAvailable)
    pub desired_healthy: i32,
    /// Number of disruptions allowed
    pub disruptions_allowed: i32,
    /// Expected pods (total matching selector)
    pub expected_pods: i32,
    /// Whether this PDB would block a drain
    pub would_block_drain: bool,
}

/// PDB health summary
#[derive(Debug, Clone, Default)]
pub struct PdbHealthInfo {
    /// All PDBs in the cluster
    pub pdbs: Vec<PdbInfo>,
    /// PDBs that would block drain (disruptions_allowed == 0)
    pub blocking_pdbs: Vec<PdbInfo>,
}

impl PdbHealthInfo {
    /// Check if any PDBs would block a drain
    pub fn has_blocking_pdbs(&self) -> bool {
        !self.blocking_pdbs.is_empty()
    }

    /// Get summary message
    pub fn summary(&self) -> String {
        if self.pdbs.is_empty() {
            "No PDBs configured".to_string()
        } else if self.blocking_pdbs.is_empty() {
            format!("{} PDBs, all allow disruption", self.pdbs.len())
        } else {
            format!(
                "{} PDBs, {} would block drain",
                self.pdbs.len(),
                self.blocking_pdbs.len()
            )
        }
    }
}

/// Check PodDisruptionBudgets across all namespaces
///
/// Identifies PDBs that would block a node drain operation.
pub async fn check_pdb_health(client: &Client) -> Result<PdbHealthInfo, K8sError> {
    let pdbs: Api<PodDisruptionBudget> = Api::all(client.clone());

    let pdb_list = pdbs
        .list(&ListParams::default())
        .await
        .map_err(|e| K8sError::ApiError(e.to_string()))?;

    let mut info = PdbHealthInfo::default();

    for pdb in pdb_list.items {
        let name = pdb.metadata.name.clone().unwrap_or_default();
        let namespace = pdb.metadata.namespace.clone().unwrap_or_default();
        let status = pdb.status.as_ref();

        let current_healthy = status.and_then(|s| s.current_healthy).unwrap_or(0);
        let desired_healthy = status.and_then(|s| s.desired_healthy).unwrap_or(0);
        let disruptions_allowed = status.and_then(|s| s.disruptions_allowed).unwrap_or(0);
        let expected_pods = status.and_then(|s| s.expected_pods).unwrap_or(0);

        // A PDB blocks drain if disruptions_allowed is 0 and there are expected pods
        let would_block_drain = disruptions_allowed == 0 && expected_pods > 0;

        let pdb_info = PdbInfo {
            name,
            namespace,
            current_healthy,
            desired_healthy,
            disruptions_allowed,
            expected_pods,
            would_block_drain,
        };

        if would_block_drain {
            info.blocking_pdbs.push(pdb_info.clone());
        }
        info.pdbs.push(pdb_info);
    }

    Ok(info)
}

// ==================== Node Operations ====================

/// Result of a cordon operation
#[derive(Debug, Clone)]
pub struct CordonResult {
    /// Node name
    pub node: String,
    /// Whether the operation succeeded
    pub success: bool,
    /// Error message if failed
    pub error: Option<String>,
}

/// Result of a drain operation
#[derive(Debug, Clone)]
pub struct DrainResult {
    /// Node name
    pub node: String,
    /// Whether the operation succeeded
    pub success: bool,
    /// Number of pods evicted
    pub pods_evicted: usize,
    /// Pods that failed to evict
    pub failed_pods: Vec<String>,
    /// Pods that were force-deleted
    pub force_deleted_pods: Vec<String>,
    /// Error message if failed
    pub error: Option<String>,
}

/// Options for drain and reboot operations
#[derive(Debug, Clone)]
pub struct DrainOptions {
    /// Timeout per pod for PDB retry (seconds)
    pub per_pod_timeout_secs: u64,
    /// Grace period for pod termination (None = use pod's default)
    pub grace_period_secs: Option<i64>,
    /// Force delete pods that can't be evicted (unmanaged pods)
    pub force_delete_unmanaged: bool,
    /// Ignore DaemonSet pods
    pub ignore_daemonsets: bool,
    /// Delete pods with emptyDir volumes
    pub delete_emptydir_data: bool,
    /// Wait for node to become Ready after reboot
    pub wait_for_node_ready: bool,
    /// Timeout for waiting for node to become Ready (seconds)
    pub post_reboot_timeout_secs: u64,
    /// Uncordon node after it becomes Ready
    pub uncordon_after_reboot: bool,
}

impl Default for DrainOptions {
    fn default() -> Self {
        Self {
            per_pod_timeout_secs: 30,
            grace_period_secs: None, // Use pod's default
            force_delete_unmanaged: false,
            ignore_daemonsets: true,
            delete_emptydir_data: true,
            wait_for_node_ready: true,     // Safe default for production
            post_reboot_timeout_secs: 300, // 5 minutes
            uncordon_after_reboot: true,   // Auto-uncordon when healthy
        }
    }
}

/// Cordon a node (mark as unschedulable)
pub async fn cordon_node(client: &Client, node_name: &str) -> Result<CordonResult, K8sError> {
    let nodes: Api<Node> = Api::all(client.clone());

    // Patch the node to set unschedulable = true
    let patch = json!({
        "spec": {
            "unschedulable": true
        }
    });

    match nodes
        .patch(node_name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => Ok(CordonResult {
            node: node_name.to_string(),
            success: true,
            error: None,
        }),
        Err(e) => Ok(CordonResult {
            node: node_name.to_string(),
            success: false,
            error: Some(e.to_string()),
        }),
    }
}

/// Uncordon a node (mark as schedulable)
pub async fn uncordon_node(client: &Client, node_name: &str) -> Result<CordonResult, K8sError> {
    let nodes: Api<Node> = Api::all(client.clone());

    // Patch the node to set unschedulable = false
    let patch = json!({
        "spec": {
            "unschedulable": false
        }
    });

    match nodes
        .patch(node_name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => Ok(CordonResult {
            node: node_name.to_string(),
            success: true,
            error: None,
        }),
        Err(e) => Ok(CordonResult {
            node: node_name.to_string(),
            success: false,
            error: Some(e.to_string()),
        }),
    }
}

/// Progress callback type for drain operations
pub type DrainProgressCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Information about a pod to evict
#[derive(Debug, Clone, PartialEq)]
struct PodToEvict {
    namespace: String,
    name: String,
    /// Whether this pod is managed by a controller (ReplicaSet, Deployment, etc.)
    is_managed: bool,
}

/// Pods to evict from a node: everything except mirror pods and (with
/// `ignore_daemonsets`) DaemonSet pods. Pods with emptyDir volumes are an
/// error unless `delete_emptydir_data`: `Err` lists them as `ns/name`.
fn select_pods_to_evict(
    pods: &[Pod],
    options: &DrainOptions,
) -> std::result::Result<Vec<PodToEvict>, Vec<String>> {
    let mut pods_to_evict = Vec::new();
    let mut blocking = Vec::new();

    for pod in pods {
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        let namespace = pod.metadata.namespace.clone().unwrap_or_default();

        // Mirror pods are created by kubelet, not managed by the API server
        if pod
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"))
        {
            continue;
        }

        let owner_refs = pod.metadata.owner_references.as_ref();
        let is_daemonset_pod =
            owner_refs.is_some_and(|refs| refs.iter().any(|r| r.kind == "DaemonSet"));
        if is_daemonset_pod && options.ignore_daemonsets {
            continue;
        }
        let is_managed = owner_refs.is_some_and(|refs| {
            refs.iter().any(|r| {
                matches!(
                    r.kind.as_str(),
                    "ReplicaSet" | "Deployment" | "StatefulSet" | "Job" | "DaemonSet"
                )
            })
        });

        let has_emptydir = pod.spec.as_ref().is_some_and(|spec| {
            spec.volumes
                .as_ref()
                .is_some_and(|volumes| volumes.iter().any(|v| v.empty_dir.is_some()))
        });
        if has_emptydir && !options.delete_emptydir_data {
            blocking.push(format!("{namespace}/{pod_name}"));
            continue;
        }

        pods_to_evict.push(PodToEvict {
            namespace,
            name: pod_name,
            is_managed,
        });
    }

    if blocking.is_empty() {
        Ok(pods_to_evict)
    } else {
        Err(blocking)
    }
}

/// Drain a node with progress callback
pub async fn drain_node_with_progress(
    client: &Client,
    node_name: &str,
    options: &DrainOptions,
    progress_callback: Option<DrainProgressCallback>,
) -> Result<DrainResult, K8sError> {
    use kube::api::DeleteParams;

    let pods: Api<Pod> = Api::all(client.clone());

    // List pods on this node
    let list_params = ListParams::default().fields(&format!("spec.nodeName={}", node_name));

    let pod_list = pods
        .list(&list_params)
        .await
        .map_err(|e| K8sError::ApiError(format!("Failed to list pods: {}", e)))?;

    let pods_to_evict = match select_pods_to_evict(&pod_list.items, options) {
        Ok(pods) => pods,
        Err(blocking) => {
            // Like kubectl: refuse up front instead of skipping, so the node
            // isn't rebooted with pods still on it
            let msg = format!(
                "{} pod(s) with emptyDir data (local data would be lost): {}",
                blocking.len(),
                blocking.join(", ")
            );
            if let Some(ref cb) = progress_callback {
                cb(&msg);
            }
            return Ok(DrainResult {
                node: node_name.to_string(),
                success: false,
                pods_evicted: 0,
                failed_pods: blocking,
                force_deleted_pods: Vec::new(),
                error: Some(msg),
            });
        }
    };

    let total_pods = pods_to_evict.len();
    let mut evicted = 0;
    let mut failed_pods = Vec::new();
    let mut force_deleted_pods = Vec::new();

    // Report initial count
    if let Some(ref cb) = progress_callback {
        cb(&format!("Found {} pods to evict", total_pods));
    }

    // Calculate max attempts based on configurable timeout
    // Each attempt waits 2 seconds, so max_attempts = timeout / 2
    let max_attempts = (options.per_pod_timeout_secs / 2).max(1) as usize;

    // Eviction uses the pod's configured grace period by default.
    // Custom grace_period_secs is only applied to force-delete operations.
    let evict_params = EvictParams::default();

    // Evict pods one at a time, respecting PDBs
    // PDBs may temporarily block eviction, so we retry with backoff
    for (idx, pod_info) in pods_to_evict.into_iter().enumerate() {
        let namespace = &pod_info.namespace;
        let pod_name = &pod_info.name;
        let ns_pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

        // Report which pod we're evicting
        if let Some(ref cb) = progress_callback {
            cb(&format!(
                "Evicting {}/{} ({}/{})",
                namespace,
                pod_name,
                idx + 1,
                total_pods
            ));
        }

        let mut attempts = 0;
        let mut success = false;

        while attempts < max_attempts && !success {
            match ns_pods.evict(pod_name, &evict_params).await {
                Ok(_) => {
                    evicted += 1;
                    tracing::info!("Evicted pod {}/{}", namespace, pod_name);
                    success = true;

                    // Report success
                    if let Some(ref cb) = progress_callback {
                        cb(&format!(
                            "Evicted {}/{} ({}/{})",
                            namespace, pod_name, evicted, total_pods
                        ));
                    }

                    // Wait briefly for pod to start terminating before next eviction
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    let outcome = classify_eviction_error(&e);

                    // Pod already gone - count as success
                    if outcome == EvictionErrorKind::PodGone {
                        evicted += 1;
                        success = true;
                        if let Some(ref cb) = progress_callback {
                            cb(&format!(
                                "Pod gone {}/{} ({}/{})",
                                namespace, pod_name, evicted, total_pods
                            ));
                        }
                        continue;
                    }

                    // PDB blocking eviction - retry after delay
                    if outcome == EvictionErrorKind::BlockedByPdb {
                        attempts += 1;
                        tracing::debug!(
                            "PDB blocking eviction of {}/{}, attempt {}/{}",
                            namespace,
                            pod_name,
                            attempts,
                            max_attempts
                        );
                        // Report PDB wait
                        if let Some(ref cb) = progress_callback {
                            cb(&format!(
                                "Waiting for PDB: {}/{} (retry {}/{})",
                                namespace, pod_name, attempts, max_attempts
                            ));
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        continue;
                    }

                    // Other error - check if we can force delete
                    tracing::warn!("Failed to evict {}/{}: {}", namespace, pod_name, e);

                    // Try force delete if enabled and pod is unmanaged
                    if options.force_delete_unmanaged && !pod_info.is_managed {
                        if let Some(ref cb) = progress_callback {
                            cb(&format!(
                                "Force deleting unmanaged pod {}/{}",
                                namespace, pod_name
                            ));
                        }

                        let delete_params = if let Some(grace) = options.grace_period_secs {
                            DeleteParams::default().grace_period(grace as u32)
                        } else {
                            DeleteParams::default().grace_period(0)
                        };

                        match ns_pods.delete(pod_name, &delete_params).await {
                            Ok(_) => {
                                evicted += 1;
                                force_deleted_pods.push(format!("{}/{}", namespace, pod_name));
                                tracing::info!(
                                    "Force deleted unmanaged pod {}/{}",
                                    namespace,
                                    pod_name
                                );
                                success = true;
                                if let Some(ref cb) = progress_callback {
                                    cb(&format!(
                                        "Force deleted {}/{} ({}/{})",
                                        namespace, pod_name, evicted, total_pods
                                    ));
                                }
                            }
                            Err(del_err) => {
                                tracing::warn!(
                                    "Force delete failed for {}/{}: {}",
                                    namespace,
                                    pod_name,
                                    del_err
                                );
                                failed_pods.push(format!("{}/{}", namespace, pod_name));
                                if let Some(ref cb) = progress_callback {
                                    cb(&format!("Failed: {}/{}", namespace, pod_name));
                                }
                            }
                        }
                    } else {
                        failed_pods.push(format!("{}/{}", namespace, pod_name));
                        if let Some(ref cb) = progress_callback {
                            cb(&format!("Failed: {}/{}", namespace, pod_name));
                        }
                    }
                    break;
                }
            }
        }

        if !success && attempts >= max_attempts {
            tracing::warn!(
                "Timed out waiting for PDB to allow eviction of {}/{}",
                namespace,
                pod_name
            );

            // Try force delete on timeout if enabled and pod is unmanaged
            if options.force_delete_unmanaged && !pod_info.is_managed {
                if let Some(ref cb) = progress_callback {
                    cb(&format!(
                        "Force deleting unmanaged pod {}/{} after timeout",
                        namespace, pod_name
                    ));
                }

                let delete_params = if let Some(grace) = options.grace_period_secs {
                    DeleteParams::default().grace_period(grace as u32)
                } else {
                    DeleteParams::default().grace_period(0)
                };

                match ns_pods.delete(pod_name, &delete_params).await {
                    Ok(_) => {
                        evicted += 1;
                        force_deleted_pods.push(format!("{}/{}", namespace, pod_name));
                        tracing::info!(
                            "Force deleted unmanaged pod {}/{} after timeout",
                            namespace,
                            pod_name
                        );
                        if let Some(ref cb) = progress_callback {
                            cb(&format!(
                                "Force deleted {}/{} ({}/{})",
                                namespace, pod_name, evicted, total_pods
                            ));
                        }
                    }
                    Err(del_err) => {
                        tracing::warn!(
                            "Force delete failed for {}/{}: {}",
                            namespace,
                            pod_name,
                            del_err
                        );
                        failed_pods.push(format!("{}/{}", namespace, pod_name));
                        if let Some(ref cb) = progress_callback {
                            cb(&format!("Timeout: {}/{}", namespace, pod_name));
                        }
                    }
                }
            } else {
                failed_pods.push(format!("{}/{}", namespace, pod_name));
                if let Some(ref cb) = progress_callback {
                    cb(&format!("Timeout: {}/{}", namespace, pod_name));
                }
            }
        }
    }

    Ok(DrainResult {
        node: node_name.to_string(),
        success: failed_pods.is_empty(),
        pods_evicted: evicted,
        failed_pods,
        force_deleted_pods,
        error: None,
    })
}

// ==================== Post-Operation Verification ====================

/// Result of waiting for a node to become ready
#[derive(Debug, Clone)]
pub struct NodeReadyResult {
    /// Whether the node became ready
    pub success: bool,
    /// Time taken to become ready (seconds)
    pub time_taken_secs: u64,
    /// Error message if failed
    pub error: Option<String>,
}

/// Node condition status from Kubernetes
#[derive(Debug, Clone, PartialEq)]
pub enum NodeConditionStatus {
    Ready,
    NotReady,
    Unknown,
}

/// What a node reports: its Ready condition and the boot it is on
#[derive(Debug, Clone, PartialEq)]
pub struct NodeState {
    pub ready: NodeConditionStatus,
    /// `status.nodeInfo.bootID`; changes with every boot
    pub boot_id: Option<String>,
}

/// How long one node lookup may take while waiting on a reboot
const NODE_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Get the Ready condition and boot ID of a node
pub async fn get_node_state(client: &Client, node_name: &str) -> Result<NodeState, K8sError> {
    let nodes: Api<Node> = Api::all(client.clone());

    let node = tokio::time::timeout(NODE_LOOKUP_TIMEOUT, nodes.get(node_name))
        .await
        .map_err(|_| K8sError::ApiError("Timed out getting node".to_string()))?
        .map_err(|e| K8sError::ApiError(format!("Failed to get node: {}", e)))?;

    let status = node.status.as_ref();
    let ready = status
        .and_then(|s| s.conditions.as_ref())
        .and_then(|conditions| conditions.iter().find(|c| c.type_ == "Ready"))
        .map_or(NodeConditionStatus::Unknown, |c| match c.status.as_str() {
            "True" => NodeConditionStatus::Ready,
            "False" => NodeConditionStatus::NotReady,
            _ => NodeConditionStatus::Unknown,
        });
    let boot_id = status
        .and_then(|s| s.node_info.as_ref())
        .map(|info| info.boot_id.clone())
        .filter(|id| !id.is_empty());
    Ok(NodeState { ready, boot_id })
}

/// Boot ID of a node before a reboot, to tell the new boot from the old one
pub async fn get_node_boot_id(client: &Client, node_name: &str) -> Option<String> {
    get_node_state(client, node_name).await.ok()?.boot_id
}

/// Whether the node is back from the reboot: Ready on a new boot. Without
/// boot IDs, Ready after it was seen down. A Ready node on the old boot
/// hasn't rebooted yet: its condition is stale until kubelet stops reporting.
fn reboot_finished(boot_id_before: Option<&str>, seen_down: bool, state: &NodeState) -> bool {
    if state.ready != NodeConditionStatus::Ready {
        return false;
    }
    match (boot_id_before, state.boot_id.as_deref()) {
        (Some(before), Some(now)) => before != now,
        _ => seen_down,
    }
}

/// Progress callback for node readiness waiting
pub type NodeReadyProgressCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Wait for a node to come back Ready after a reboot.
///
/// `boot_id_before` is the node's boot ID from before the reboot request
/// (see [`get_node_boot_id`]); with it, success means Ready on a new boot.
/// Without it, success means Ready after the node was seen NotReady or gone.
pub async fn wait_for_node_ready(
    client: &Client,
    node_name: &str,
    timeout_secs: u64,
    boot_id_before: Option<String>,
    progress_callback: Option<NodeReadyProgressCallback>,
) -> Result<NodeReadyResult, K8sError> {
    let start = std::time::Instant::now();
    let poll_interval = tokio::time::Duration::from_secs(3);
    let report = |msg: &str| {
        if let Some(ref cb) = progress_callback {
            cb(msg);
        }
    };
    report("Waiting for node to begin rebooting...");

    let mut seen_down = false;
    loop {
        let elapsed = start.elapsed().as_secs();
        if elapsed >= timeout_secs {
            return Ok(NodeReadyResult {
                success: false,
                time_taken_secs: elapsed,
                error: Some(format!(
                    "Timed out waiting for node after {}s",
                    timeout_secs
                )),
            });
        }
        let remaining = timeout_secs - elapsed;

        match get_node_state(client, node_name).await {
            Ok(state) => {
                if reboot_finished(boot_id_before.as_deref(), seen_down, &state) {
                    report(&format!("Node is Ready (took {}s)", elapsed));
                    return Ok(NodeReadyResult {
                        success: true,
                        time_taken_secs: elapsed,
                        error: None,
                    });
                }
                let same_boot = boot_id_before.is_some() && state.boot_id == boot_id_before;
                if state.ready != NodeConditionStatus::Ready {
                    seen_down = true;
                }
                if state.ready == NodeConditionStatus::Ready && (same_boot || !seen_down) {
                    report(&format!(
                        "Waiting for node to begin rebooting ({}s remaining)",
                        remaining
                    ));
                } else {
                    let status = match state.ready {
                        NodeConditionStatus::NotReady => "NotReady",
                        NodeConditionStatus::Ready => "Ready",
                        NodeConditionStatus::Unknown => "Unknown",
                    };
                    report(&format!(
                        "Node status: {} ({}s remaining)",
                        status, remaining
                    ));
                }
            }
            Err(_) => {
                // Gone from the API or the API is down with it
                seen_down = true;
                report(&format!(
                    "Waiting for node to rejoin cluster ({}s remaining)",
                    remaining
                ));
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// How a failed pod eviction should be handled
#[derive(Debug, PartialEq, Eq)]
enum EvictionErrorKind {
    /// Pod no longer exists - nothing left to evict
    PodGone,
    /// A PodDisruptionBudget denied the eviction for now - retry later
    BlockedByPdb,
    /// Any other failure
    Other,
}

/// Classify an eviction error by its API status code rather than its message
/// text. The Eviction API answers 429 when a PodDisruptionBudget denies the
/// eviction and 404 when the pod is already gone.
fn classify_eviction_error(err: &kube::Error) -> EvictionErrorKind {
    match err {
        kube::Error::Api(resp) if resp.code == 404 => EvictionErrorKind::PodGone,
        kube::Error::Api(resp) if resp.code == 429 => EvictionErrorKind::BlockedByPdb,
        _ => EvictionErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    fn node_state(ready: NodeConditionStatus, boot_id: Option<&str>) -> NodeState {
        NodeState {
            ready,
            boot_id: boot_id.map(str::to_string),
        }
    }

    #[test]
    fn reboot_finished_needs_a_new_boot() {
        use NodeConditionStatus::*;
        // Still Ready on the old boot: not rebooted yet, even after the
        // disconnect window passed
        assert!(!reboot_finished(
            Some("a"),
            false,
            &node_state(Ready, Some("a"))
        ));
        assert!(!reboot_finished(
            Some("a"),
            true,
            &node_state(Ready, Some("a"))
        ));
        // New boot but not Ready yet
        assert!(!reboot_finished(
            Some("a"),
            true,
            &node_state(NotReady, Some("b"))
        ));
        // New boot and Ready, whether or not it was seen down
        assert!(reboot_finished(
            Some("a"),
            false,
            &node_state(Ready, Some("b"))
        ));
    }

    #[test]
    fn reboot_finished_without_boot_ids_needs_seen_down() {
        use NodeConditionStatus::*;
        assert!(!reboot_finished(None, false, &node_state(Ready, None)));
        assert!(reboot_finished(None, true, &node_state(Ready, None)));
        assert!(!reboot_finished(None, true, &node_state(Unknown, None)));
        // Boot ID only known afterwards: fall back to seen down
        assert!(!reboot_finished(None, false, &node_state(Ready, Some("b"))));
    }

    fn pod(name: &str, owner: Option<&str>, mirror: bool, emptydir: bool) -> Pod {
        k8s_openapi::serde_json::from_value(json!({
            "metadata": {
                "name": name,
                "namespace": "ns",
                "annotations": if mirror {
                    json!({"kubernetes.io/config.mirror": "x"})
                } else {
                    json!({})
                },
                "ownerReferences": owner.map(|kind| vec![json!({
                    "apiVersion": "apps/v1", "kind": kind, "name": "o", "uid": "u"
                })]).unwrap_or_default(),
            },
            "spec": {
                "containers": [{"name": "c"}],
                "volumes": if emptydir {
                    json!([{"name": "v", "emptyDir": {}}])
                } else {
                    json!([])
                },
            },
        }))
        .unwrap()
    }

    fn names(pods: &[PodToEvict]) -> Vec<&str> {
        pods.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn pod_health_lists_each_pod_once() {
        const NOW: i64 = 1_800_000_000;
        let created = |secs_ago: i64| {
            chrono::DateTime::from_timestamp(NOW - secs_ago, 0)
                .unwrap()
                .to_rfc3339()
        };
        let waiting = |reason: &str| {
            json!({"name": "c", "image": "i", "imageID": "", "ready": false, "restartCount": 4,
                   "state": {"waiting": {"reason": reason}}})
        };
        let pods: Vec<Pod> = [
            // Two crashing containers, one pod
            json!({"metadata": {"name": "two"},
                   "status": {"phase": "Running", "containerStatuses":
                       [waiting("CrashLoopBackOff"), waiting("CrashLoopBackOff")]}}),
            json!({"metadata": {"name": "init"},
                   "status": {"phase": "Pending", "initContainerStatuses":
                       [waiting("ImagePullBackOff")]}}),
            json!({"metadata": {"name": "new", "creationTimestamp": created(10)},
                   "status": {"phase": "Pending"}}),
            json!({"metadata": {"name": "stuck", "creationTimestamp": created(3600)},
                   "status": {"phase": "Pending"}}),
        ]
        .into_iter()
        .map(|v| k8s_openapi::serde_json::from_value(v).unwrap())
        .collect();
        let info = pod_health_from(&pods, NOW);
        let names = |list: &[UnhealthyPodInfo]| -> Vec<String> {
            list.iter().map(|p| p.name.clone()).collect()
        };
        assert_eq!(names(&info.crashing), ["two"]);
        assert_eq!(names(&info.image_pull_errors), ["init"]);
        assert_eq!(names(&info.pending), ["stuck"]);
        assert_eq!(info.total_pods, 4);
    }

    #[test]
    fn drain_skips_mirror_and_daemonset_pods() {
        let pods = [
            pod("mirror", None, true, false),
            pod("ds", Some("DaemonSet"), false, false),
            pod("app", Some("ReplicaSet"), false, false),
            pod("bare", None, false, false),
        ];
        let selected = select_pods_to_evict(&pods, &DrainOptions::default()).unwrap();
        assert_eq!(names(&selected), ["app", "bare"]);
        assert!(selected[0].is_managed);
        assert!(!selected[1].is_managed);

        let options = DrainOptions {
            ignore_daemonsets: false,
            ..Default::default()
        };
        let selected = select_pods_to_evict(&pods, &options).unwrap();
        assert_eq!(names(&selected), ["ds", "app", "bare"]);
    }

    #[test]
    fn drain_refuses_emptydir_pods_unless_allowed() {
        let pods = [
            pod("app", Some("ReplicaSet"), false, false),
            pod("cache", Some("ReplicaSet"), false, true),
        ];
        let selected = select_pods_to_evict(&pods, &DrainOptions::default()).unwrap();
        assert_eq!(names(&selected), ["app", "cache"]);

        let options = DrainOptions {
            delete_emptydir_data: false,
            ..Default::default()
        };
        assert_eq!(
            select_pods_to_evict(&pods, &options),
            Err(vec!["ns/cache".to_string()])
        );
    }

    #[test]
    fn server_host_port_parsing() {
        use super::server_host_port;
        assert_eq!(
            server_host_port("https://kube.prod.example.internal:6443"),
            Some(("kube.prod.example.internal".to_string(), 6443))
        );
        assert_eq!(
            server_host_port("https://10.0.0.1"),
            Some(("10.0.0.1".to_string(), 443))
        );
        assert_eq!(
            server_host_port("https://[fd00::1]:6443/"),
            Some(("fd00::1".to_string(), 6443))
        );
        assert_eq!(server_host_port("https://host:notaport"), None);
    }

    fn api_error(code: u16, message: &str) -> kube::Error {
        kube::Error::Api(
            kube::core::Status::failure(message, "")
                .with_code(code)
                .boxed(),
        )
    }

    #[test]
    fn eviction_errors_classified_by_status_code() {
        assert_eq!(
            classify_eviction_error(&api_error(404, "pods \"web\" not found")),
            EvictionErrorKind::PodGone
        );
        assert_eq!(
            classify_eviction_error(&api_error(
                429,
                "Cannot evict pod as it would violate the pod's disruption budget."
            )),
            EvictionErrorKind::BlockedByPdb
        );
        // Message text alone must not drive the decision
        assert_eq!(
            classify_eviction_error(&api_error(403, "user cannot evict: 404 in name")),
            EvictionErrorKind::Other
        );
    }

    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tokio::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::const_new(());

    /// Test that KUBECONFIG environment variable is respected.
    ///
    /// This test creates a valid kubeconfig file pointing to a non-existent cluster.
    /// If KUBECONFIG is respected, Config::infer() will find it and try to use it,
    /// resulting in a connection error (not a kubeconfig fetch error from Talos).
    ///
    /// Note: This test manipulates environment variables and must be run serially.
    #[tokio::test]
    async fn test_kubeconfig_env_is_tried_first() {
        let _guard = ENV_MUTEX.lock().await;

        // Create a valid kubeconfig file pointing to a non-existent cluster
        let kubeconfig_content = r#"
apiVersion: v1
kind: Config
clusters:
- cluster:
    server: https://127.0.0.1:64321
    insecure-skip-tls-verify: true
  name: test-cluster
contexts:
- context:
    cluster: test-cluster
    user: test-user
  name: test-context
current-context: test-context
users:
- name: test-user
  user:
    token: test-token
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(kubeconfig_content.as_bytes()).unwrap();
        let kubeconfig_path = temp_file.path().to_string_lossy().to_string();

        // Set KUBECONFIG to our temp file
        let old_kubeconfig = std::env::var("KUBECONFIG").ok();
        unsafe {
            std::env::set_var("KUBECONFIG", &kubeconfig_path);
        }

        // Try to infer config - this should succeed in finding the kubeconfig
        // (it will fail to connect, but that's expected)
        let config_result = Config::infer().await;

        // Restore original KUBECONFIG
        unsafe {
            if let Some(old_value) = old_kubeconfig {
                std::env::set_var("KUBECONFIG", old_value);
            } else {
                std::env::remove_var("KUBECONFIG");
            }
        }

        // The config should have been loaded (even though connection would fail)
        assert!(
            config_result.is_ok(),
            "Config::infer() should find our KUBECONFIG file"
        );

        // Verify it points to our test cluster
        let config = config_result.unwrap();
        assert_eq!(
            config.cluster_url.to_string(),
            "https://127.0.0.1:64321/",
            "Should use the cluster URL from our kubeconfig"
        );
    }

    /// Test that when KUBECONFIG points to a non-existent file, it falls back gracefully.
    ///
    /// Note: This test can't fully verify the Talos fallback without a real Talos client,
    /// but it verifies that the function handles missing KUBECONFIG correctly.
    #[tokio::test]
    async fn test_kubeconfig_fallback_on_missing_file() {
        let _guard = ENV_MUTEX.lock().await;

        // Set KUBECONFIG to a non-existent file
        let old_kubeconfig = std::env::var("KUBECONFIG").ok();
        unsafe {
            std::env::set_var("KUBECONFIG", "/non/existent/path/kubeconfig");
        }

        // Try to infer config - this should fail
        let config_result = Config::infer().await;

        // Restore original KUBECONFIG
        unsafe {
            if let Some(old_value) = old_kubeconfig {
                std::env::set_var("KUBECONFIG", old_value);
            } else {
                std::env::remove_var("KUBECONFIG");
            }
        }

        // Config::infer() should fail when kubeconfig doesn't exist
        // This triggers the fallback to Talos API in create_k8s_client_with_kubeconfig_source
        assert!(
            config_result.is_err(),
            "Config::infer() should fail with non-existent kubeconfig"
        );
    }

    /// `node_info_from` must pick the `InternalIP` (the Talos API address) and
    /// read the control-plane role from the standard role labels. This is the
    /// extraction the discovery-disabled worker fallback relies on.
    #[test]
    fn node_info_extracts_internal_ip_and_control_plane_role() {
        // Control plane node: role label present; InternalIP chosen even when
        // other address types precede it.
        let cp: Node = k8s_openapi::serde_json::from_value(k8s_openapi::serde_json::json!({
            "metadata": {
                "name": "cp-1",
                "labels": { "node-role.kubernetes.io/control-plane": "" }
            },
            "status": {
                "addresses": [
                    { "type": "Hostname", "address": "cp-1" },
                    { "type": "InternalIP", "address": "10.0.0.1" }
                ]
            }
        }))
        .unwrap();
        let info = node_info_from(&cp);
        assert_eq!(info.name, "cp-1");
        assert_eq!(info.internal_ip.as_deref(), Some("10.0.0.1"));
        assert!(info.is_control_plane);

        // Worker node: no role label -> not control plane.
        let worker: Node = k8s_openapi::serde_json::from_value(k8s_openapi::serde_json::json!({
            "metadata": { "name": "worker-1", "labels": {} },
            "status": { "addresses": [ { "type": "InternalIP", "address": "10.0.0.2" } ] }
        }))
        .unwrap();
        let info = node_info_from(&worker);
        assert!(!info.is_control_plane);
        assert_eq!(info.internal_ip.as_deref(), Some("10.0.0.2"));

        // Legacy `master` label also counts as control plane; no InternalIP -> None.
        let legacy: Node = k8s_openapi::serde_json::from_value(k8s_openapi::serde_json::json!({
            "metadata": { "name": "old-cp", "labels": { "node-role.kubernetes.io/master": "" } },
            "status": { "addresses": [] }
        }))
        .unwrap();
        let info = node_info_from(&legacy);
        assert!(info.is_control_plane);
        assert_eq!(info.internal_ip, None);
    }

    /// `current_context_cluster_ca` extracts the CA of the current context's
    /// cluster — the same-cluster key used to decide whether an ambient
    /// KUBECONFIG matches the launched Talos context. Equal CAs => same cluster
    /// (honor KUBECONFIG); different CAs => different cluster (ignore it).
    #[test]
    fn cluster_ca_extracted_from_current_context() {
        let yaml = |ca: &str, server: &str| {
            format!(
                "apiVersion: v1\nkind: Config\n\
                 current-context: ctx\n\
                 clusters:\n\
                 - name: c1\n  cluster:\n    server: {server}\n    certificate-authority-data: {ca}\n\
                 contexts:\n- name: ctx\n  context: {{cluster: c1, user: u1}}\n\
                 users:\n- name: u1\n  user: {{token: t}}\n"
            )
        };

        // Same CA but different server endpoints (node IP vs VIP) => same cluster.
        let node: kube::config::Kubeconfig =
            serde_saphyr::from_str(&yaml("CA_ALPHA", "https://10.6.0.2:6443")).unwrap();
        let vip: kube::config::Kubeconfig =
            serde_saphyr::from_str(&yaml("CA_ALPHA", "https://10.6.0.1:6443")).unwrap();
        assert_eq!(
            current_context_cluster_ca(&node).as_deref(),
            Some("CA_ALPHA")
        );
        assert_eq!(
            current_context_cluster_ca(&node),
            current_context_cluster_ca(&vip),
            "same CA, different endpoint => matched as same cluster"
        );

        // Different CA => different cluster.
        let other: kube::config::Kubeconfig =
            serde_saphyr::from_str(&yaml("CA_BETA", "https://192.0.2.9:6443")).unwrap();
        assert_ne!(
            current_context_cluster_ca(&node),
            current_context_cluster_ca(&other)
        );

        // No current context / no inline CA => None (treated as "can't compare",
        // never a false mismatch).
        let empty: kube::config::Kubeconfig =
            serde_saphyr::from_str("apiVersion: v1\nkind: Config\n").unwrap();
        assert_eq!(current_context_cluster_ca(&empty), None);
    }

    /// Kubernetes roster entries map onto discovery members, dropping IP-less
    /// nodes (unreachable over the Talos API) and mapping the control-plane flag
    /// to the machine type the UI classifies on.
    #[test]
    fn k8s_nodes_map_to_discovery_members_dropping_ipless() {
        let members = k8s_nodes_to_discovery_members(vec![
            K8sNodeInfo {
                name: "cp1".into(),
                internal_ip: Some("10.0.0.1".into()),
                is_control_plane: true,
            },
            K8sNodeInfo {
                name: "w1".into(),
                internal_ip: Some("10.0.0.2".into()),
                is_control_plane: false,
            },
            // No InternalIP -> unreachable over the Talos API -> dropped.
            K8sNodeInfo {
                name: "ghost".into(),
                internal_ip: None,
                is_control_plane: false,
            },
        ]);
        assert_eq!(members.len(), 2, "IP-less node dropped");
        assert_eq!(members[0].hostname, "cp1");
        assert_eq!(members[0].machine_type, "controlplane");
        assert_eq!(members[0].addresses, vec!["10.0.0.1".to_string()]);
        assert_eq!(members[1].hostname, "w1");
        assert_eq!(members[1].machine_type, "worker");
    }
}
