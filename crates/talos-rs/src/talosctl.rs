//! Talosctl command execution
//!
//! Provides functions to execute talosctl commands and parse their output.
//! This is necessary because the COSI State API is not exposed externally
//! through apid - talosctl connects directly to machined via Unix socket.

use crate::error::TalosError;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Execute a talosctl command asynchronously and return stdout
async fn exec_talosctl_async(args: &[&str]) -> Result<String, TalosError> {
    let output = tokio::process::Command::new("talosctl")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(TalosError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TalosError::Connection(format!(
            "talosctl failed: {}",
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Volume encryption status from VolumeStatus resource
#[derive(Debug, Clone)]
pub struct VolumeStatus {
    /// Volume ID (e.g., "STATE", "EPHEMERAL")
    pub id: String,
    /// Encryption provider type
    pub encryption_provider: Option<String>,
    /// Volume phase
    pub phase: String,
    /// Pretty size
    pub size: String,
    /// Filesystem type
    pub filesystem: Option<String>,
    /// Mount location
    pub mount_location: Option<String>,
}

/// Disk information from Disks.block.talos.dev resource
#[derive(Debug, Clone)]
pub struct DiskInfo {
    /// Disk ID (e.g., "sda", "nvme0n1")
    pub id: String,
    /// Device path (e.g., "/dev/sda")
    pub dev_path: String,
    /// Size in bytes
    pub size: u64,
    /// Human-readable size (e.g., "500 GB")
    pub size_pretty: String,
    /// Disk model
    pub model: Option<String>,
    /// Disk serial number
    pub serial: Option<String>,
    /// Transport type (e.g., "sata", "nvme", "virtio", "usb")
    pub transport: Option<String>,
    /// Whether the disk is rotational (HDD) or not (SSD/NVMe)
    pub rotational: bool,
    /// Whether the disk is read-only
    pub readonly: bool,
    /// Whether the disk is a CD-ROM
    pub cdrom: bool,
    /// World Wide ID
    pub wwid: Option<String>,
    /// Bus path
    pub bus_path: Option<String>,
}

/// Machine config info from MachineConfig resource
#[derive(Debug, Clone)]
pub struct MachineConfigInfo {
    /// Config version (resource version, acts as hash)
    pub version: String,
    /// Machine type
    pub machine_type: Option<String>,
}

/// A node's machine config documents, as text
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MachineConfigText {
    /// The config in effect (`v1alpha1` resource)
    pub active: String,
    /// Its resource version and last update
    pub version: String,
    pub updated: Option<String>,
    /// The config saved for the next boot (`persistent` resource) differs:
    /// a change was staged and waits for a reboot
    pub staged_differs: bool,
}

/// KubeSpan settings that decide how it combines with a CNI
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KubeSpanSettings {
    pub enabled: bool,
    /// Pod and service networks are routed over KubeSpan (needed by
    /// Cilium's native routing)
    pub advertise_kubernetes_networks: bool,
}

/// A system extension installed on a node (ExtensionStatus resource)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtensionInfo {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    /// Image file in the node's extensions directory, e.g. `0.sqsh`
    pub image: String,
}

/// KubeSpan peer status from KubeSpanPeerStatus resource
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KubeSpanPeerStatus {
    /// Peer ID (usually the node name or public key)
    pub id: String,
    /// Peer label/hostname
    pub label: String,
    /// Endpoint address (IP:port)
    pub endpoint: Option<String>,
    /// Peer state (e.g., "up", "down", "unknown")
    pub state: String,
    /// Last handshake time
    pub last_handshake: Option<String>,
    /// Received bytes
    pub rx_bytes: u64,
    /// Transmitted bytes
    pub tx_bytes: u64,
}

/// Discovery member from Members resource
#[derive(Debug, Clone)]
pub struct DiscoveryMember {
    /// Member ID (node ID)
    pub id: String,
    /// Member addresses
    pub addresses: Vec<String>,
    /// Hostname
    pub hostname: String,
    /// Machine type (controlplane, worker)
    pub machine_type: String,
    /// Operating system
    pub operating_system: String,
}

/// Volume status of a node: `talosctl --context <context> -n <node> get
/// volumestatus`, bounded like every resource read
pub async fn get_volume_status_for_node(
    context: &str,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<Vec<VolumeStatus>, TalosError> {
    let output = get_resource_yaml(Some(context), node_ip, config_path, &["volumestatus"]).await?;
    parse_volume_status_yaml(&output)
}

/// Kinds of the encryption keys of a volume (`static`, `nodeID`, `kms`,
/// `tpm`), from its VolumeConfig: VolumeStatus only says `luks2`. Only the
/// kinds are read; a static key's passphrase is never kept.
pub async fn get_volume_key_types_for_node(
    context: &str,
    node_ip: &str,
    config_path: Option<&str>,
    volume: &str,
) -> Result<Vec<String>, TalosError> {
    let output = get_resource_yaml(
        Some(context),
        node_ip,
        config_path,
        &["volumeconfig", volume],
    )
    .await?;
    Ok(parse_key_types(&output))
}

fn parse_key_types(yaml: &str) -> Vec<String> {
    resource_documents::<serde_json::Value>(yaml)
        .flat_map(|doc| {
            doc.pointer("/spec/encryption/keys")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|key| key.get("type").and_then(scalar_to_string))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Disks of a node: `talosctl --context <context> -n <node> get disks`,
/// bounded like every resource read
pub async fn get_disks_for_node(
    context: &str,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<Vec<DiskInfo>, TalosError> {
    let output = get_resource_yaml(Some(context), node_ip, config_path, &["disks"]).await?;
    parse_disks_yaml(&output)
}

// ============================================================================
// Insecure Mode Functions
// ============================================================================
// These functions connect to Talos nodes without TLS client certificates.
// Used for maintenance mode nodes that haven't been bootstrapped yet.

/// Get disks from a node in insecure mode (no TLS client auth)
///
/// Executes: talosctl get disks --insecure -n <endpoint> -o yaml
///
/// This is useful for pre-bootstrap scenarios where you want to see
/// what disks are available before writing the machine configuration.
pub async fn get_disks_insecure(endpoint: &str) -> Result<Vec<DiskInfo>, TalosError> {
    let output =
        exec_talosctl_async(&["get", "disks", "--insecure", "-n", endpoint, "-o", "yaml"]).await?;
    parse_disks_yaml(&output)
}

/// Get volume status from a node in insecure mode (no TLS client auth)
///
/// Executes: talosctl get volumestatus --insecure -n <endpoint> -o yaml
pub async fn get_volume_status_insecure(endpoint: &str) -> Result<Vec<VolumeStatus>, TalosError> {
    let output = exec_talosctl_async(&[
        "get",
        "volumestatus",
        "--insecure",
        "-n",
        endpoint,
        "-o",
        "yaml",
    ])
    .await?;
    parse_volume_status_yaml(&output)
}

/// Result of generating Talos configuration
#[derive(Debug, Clone)]
pub struct GenConfigResult {
    /// Path to generated controlplane.yaml
    pub controlplane_path: String,
    /// Path to generated worker.yaml
    pub worker_path: String,
    /// Path to generated talosconfig
    pub talosconfig_path: String,
    /// Output directory
    pub output_dir: String,
}

/// Generate Talos machine configuration
///
/// Executes: talosctl gen config <cluster-name> <endpoint> --output-dir <dir> [--force]
///
/// This generates controlplane.yaml, worker.yaml, and talosconfig in the output directory.
pub async fn gen_config(
    cluster_name: &str,
    kubernetes_endpoint: &str,
    output_dir: &str,
    additional_sans: Option<&[&str]>,
    force: bool,
) -> Result<GenConfigResult, TalosError> {
    let mut args = vec!["gen", "config", cluster_name, kubernetes_endpoint];

    args.push("--output-dir");
    args.push(output_dir);

    // Add additional SANs if provided
    let sans_joined: String;
    if let Some(sans) = additional_sans
        && !sans.is_empty()
    {
        sans_joined = sans.join(",");
        args.push("--additional-sans");
        args.push(&sans_joined);
    }

    if force {
        args.push("--force");
    }

    exec_talosctl_async(&args).await?;

    Ok(GenConfigResult {
        controlplane_path: format!("{}/controlplane.yaml", output_dir),
        worker_path: format!("{}/worker.yaml", output_dir),
        talosconfig_path: format!("{}/talosconfig", output_dir),
        output_dir: output_dir.to_string(),
    })
}

/// Result of applying configuration in insecure mode
#[derive(Debug, Clone)]
pub struct InsecureApplyResult {
    /// Whether the apply was successful
    pub success: bool,
    /// Output message
    pub message: String,
}

/// Apply configuration to a node in insecure mode
///
/// Executes: talosctl apply-config --insecure -n <endpoint> -f <config_path>
///
/// This applies a machine configuration to a node in maintenance mode.
/// The node will install Talos and reboot.
pub async fn apply_config_insecure(
    endpoint: &str,
    config_path: &str,
) -> Result<InsecureApplyResult, TalosError> {
    let output = exec_talosctl_async(&[
        "apply-config",
        "--insecure",
        "-n",
        endpoint,
        "-f",
        config_path,
    ])
    .await;

    match output {
        Ok(msg) => Ok(InsecureApplyResult {
            success: true,
            message: if msg.trim().is_empty() {
                "Configuration applied successfully. Node will install and reboot.".to_string()
            } else {
                msg
            },
        }),
        Err(e) => Ok(InsecureApplyResult {
            success: false,
            message: format!("Failed to apply config: {}", e),
        }),
    }
}

/// Upper bound for one `talosctl get members` run. A request that apid has to
/// forward to an unreachable node otherwise hangs for its ~20s dial timeout,
/// which stalls startup since the first refresh runs behind the connect screen.
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Host part of a talosconfig endpoint (`host`, `host:port`, `[v6]` or `[v6]:port`).
fn endpoint_host(endpoint: &str) -> &str {
    if let Some(rest) = endpoint.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match endpoint.split_once(':') {
        // A bare IPv6 address has several colons and no port
        Some((host, port)) if !port.contains(':') => host,
        _ => endpoint,
    }
}

/// Get discovery members for a context (async, non-blocking)
///
/// Executes, for each configured endpoint until one succeeds:
/// talosctl --context <context> -e <endpoint> -n <endpoint> get members -o yaml
///
/// Pinning `-e` to the same node as `-n` makes each endpoint answer for itself.
/// Without it talosctl picks any endpoint, which then has to forward the
/// request to `-n` — and nodes frequently cannot reach each other's public
/// endpoint IPs, so that hop hangs until apid's dial timeout.
///
/// If `config_path` is provided, loads config from that path instead of the default.
pub async fn get_discovery_members_for_context(
    context: &str,
    config_path: Option<&str>,
) -> Result<Vec<DiscoveryMember>, TalosError> {
    let config = match config_path {
        Some(path) => {
            let path_buf = std::path::PathBuf::from(path);
            crate::TalosConfig::load_from(&path_buf)?
        }
        None => crate::TalosConfig::load_default()?,
    };
    let ctx = config
        .contexts
        .get(context)
        .ok_or_else(|| TalosError::ContextNotFound(context.to_string()))?;

    let mut last_error = TalosError::NoEndpoints(context.to_string());
    for endpoint in &ctx.endpoints {
        let host = endpoint_host(endpoint);
        if host.is_empty() {
            continue;
        }
        match get_discovery_members_for_node_async(context, Some(endpoint), host, config_path).await
        {
            Ok(members) => return Ok(members),
            Err(e) => {
                tracing::debug!("Discovery via endpoint {} failed: {}", endpoint, e);
                last_error = e;
            }
        }
    }
    Err(last_error)
}

/// Get discovery members for a specific node IP using context certificates (async).
///
/// This allows querying a specific control plane node directly instead of going through the VIP.
///
/// Executes: talosctl --context <context> [--talosconfig <path>] [-e <endpoint>] -n <node> get members -o yaml
///
/// The `--talosconfig` flag is essential for users who run with `--config <path>`:
/// without it talosctl reads the default `~/.talos/config`, where the context
/// (and its certs) may not exist, so discovery fails and the node list silently
/// degrades to control-plane-only (the "worker nodes missing" reports).
async fn get_discovery_members_for_node_async(
    context: &str,
    endpoint: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<Vec<DiscoveryMember>, TalosError> {
    let args = members_command_args(context, endpoint, node_ip, config_path);
    let output = tokio::time::timeout(DISCOVERY_TIMEOUT, exec_talosctl_async(&args))
        .await
        .map_err(|_| {
            TalosError::Connection(format!(
                "talosctl get members for {} timed out after {}s",
                node_ip,
                DISCOVERY_TIMEOUT.as_secs()
            ))
        })??;
    parse_discovery_members_yaml(&output)
}

/// Build the `talosctl ... get members` argument list.
///
/// Extracted as a pure function so the `--talosconfig` handling can be
/// unit-tested: omitting it made talosctl read the default `~/.talos/config`,
/// which breaks `--config <path>` users (their context isn't there) and
/// silently degraded the node list to control-plane-only (workers missing).
fn members_command_args<'a>(
    context: &'a str,
    endpoint: Option<&'a str>,
    node_ip: &'a str,
    config_path: Option<&'a str>,
) -> Vec<&'a str> {
    let mut args = vec!["--context", context];
    if let Some(path) = config_path {
        args.push("--talosconfig");
        args.push(path);
    }
    if let Some(endpoint) = endpoint {
        args.push("-e");
        args.push(endpoint);
    }
    args.extend_from_slice(&["-n", node_ip, "get", "members", "-o", "yaml"]);
    args
}

/// Get discovery members with automatic retry and fallback to specific nodes.
///
/// First tries the VIP endpoint (via context), then falls back to querying
/// individual control plane nodes directly if the VIP fails.
///
/// This handles transient "no request forwarding" errors that occur when
/// the VIP routes to a node that can't forward the request.
pub async fn get_discovery_members_with_retry(
    context: &str,
    config_path: Option<&str>,
    fallback_node_ips: &[String],
) -> Result<Vec<DiscoveryMember>, TalosError> {
    // First, try the VIP-based approach with retries
    // VIP_MAX_RETRIES=2 means 3 total attempts (initial + 2 retries)
    const VIP_MAX_RETRIES: u32 = 2;
    const BASE_DELAY_MS: u64 = 100;

    let mut last_error = None;

    for attempt in 0..=VIP_MAX_RETRIES {
        match get_discovery_members_for_context(context, config_path).await {
            Ok(members) => return Ok(members),
            Err(e) => {
                last_error = Some(e);
                if attempt < VIP_MAX_RETRIES {
                    let delay_ms = BASE_DELAY_MS * (1 << attempt);
                    tracing::debug!(
                        "Discovery fetch via VIP attempt {} failed, retrying in {}ms",
                        attempt + 1,
                        delay_ms
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    // VIP failed, try fallback nodes directly
    // Shuffle to distribute load and avoid repeatedly hitting a problematic node first
    if !fallback_node_ips.is_empty() {
        tracing::debug!(
            "VIP-based discovery failed, trying {} fallback nodes directly",
            fallback_node_ips.len()
        );

        let mut shuffled_ips: Vec<&String> = fallback_node_ips.iter().collect();
        fastrand::shuffle(&mut shuffled_ips);

        for node_ip in shuffled_ips {
            match get_discovery_members_for_node_async(context, None, node_ip, config_path).await {
                Ok(members) => {
                    tracing::debug!(
                        "Successfully fetched discovery members from fallback node {}",
                        node_ip
                    );
                    return Ok(members);
                }
                Err(e) => {
                    tracing::debug!("Fallback node {} failed: {}", node_ip, e);
                    last_error = Some(e);
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| TalosError::NoEndpoints(context.to_string())))
}

/// Network summary for a node, as shown by the Talos dashboard
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeNetworkInfo {
    /// Node addresses with prefix length, excluding Kubernetes pod addresses
    pub addresses: Vec<String>,
    /// Default-route gateways
    pub gateways: Vec<String>,
    /// Configured DNS servers
    pub dns_servers: Vec<String>,
    /// `NetworkStatus.connectivityReady`, `None` if it could not be read
    pub connectivity_ready: Option<bool>,
    /// The Tailscale extension's `tailscale0` link, `None` when the node
    /// has none (or it could not be read)
    pub tailscale: Option<TailscaleLink>,
    /// KubeSpan peers as this node sees them; `None` if they could not be
    /// read, empty when KubeSpan is off
    pub kubespan_peers: Option<Vec<KubeSpanPeerStatus>>,
}

/// State of a node's `tailscale0` link, from Talos network resources
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TailscaleLink {
    /// Link is up (a tun device reports no operational state, so this is
    /// `linkState`)
    pub up: bool,
    pub mtu: Option<u64>,
    /// Tailnet addresses, link-local left out
    pub addresses: Vec<String>,
    /// Routes via `tailscale0` to tailnet hosts (`100.64.0.0/10`,
    /// `fd7a:115c:a1e0::/48`): roughly the peers this node knows
    pub peer_routes: usize,
    /// Other routes via `tailscale0`: subnet routes advertised by peers
    pub subnet_routes: Vec<String>,
}

/// Upper bound for each per-node `talosctl get` (see [`get_resource_yaml`])
const NODE_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Upper bound for reading the full machine config
const MACHINE_CONFIG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Run `talosctl [--context c] get <resource> [id] -o yaml` against one node,
/// with a timeout.
///
/// Always pass the context of the cluster `node_ip` belongs to: without it
/// talosctl goes through the *current* context's endpoint, which for a node in
/// another cluster hangs ~20s on apid's proxy dial before failing.
async fn get_resource_yaml(
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
    resource: &[&str],
) -> Result<String, TalosError> {
    get_resource_yaml_within(NODE_QUERY_TIMEOUT, context, node_ip, config_path, resource).await
}

/// [`get_resource_yaml`] with its own time limit, for large resources
async fn get_resource_yaml_within(
    timeout: std::time::Duration,
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
    resource: &[&str],
) -> Result<String, TalosError> {
    let mut args = Vec::new();
    if let Some(context) = context {
        args.extend_from_slice(&["--context", context]);
    }
    if let Some(path) = config_path {
        args.extend_from_slice(&["--talosconfig", path]);
    }
    args.extend_from_slice(&["-n", node_ip, "get"]);
    args.extend_from_slice(resource);
    args.extend_from_slice(&["-o", "yaml"]);

    tokio::time::timeout(timeout, exec_talosctl_async(&args))
        .await
        .map_err(|_| {
            TalosError::Connection(format!(
                "talosctl get {} timed out after {}s",
                resource.join(" "),
                timeout.as_secs()
            ))
        })?
}

/// Addresses etcd advertises and listens on, from the EtcdSpec resource
/// (what Talos derived from `cluster.etcd.advertisedSubnets/listenSubnets`)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EtcdSpecInfo {
    pub advertised: Vec<String>,
    pub listen_peer: Vec<String>,
    pub listen_client: Vec<String>,
}

/// Get the etcd spec of a control plane node in `context` (async, with timeout)
pub async fn get_etcd_spec_for_node(
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<EtcdSpecInfo, TalosError> {
    let output = get_resource_yaml(context, node_ip, config_path, &["etcdspec"]).await?;
    parse_etcd_spec_yaml(&output)
}

fn parse_etcd_spec_yaml(yaml_str: &str) -> Result<EtcdSpecInfo, TalosError> {
    let spec = yaml_specs(yaml_str)
        .next()
        .ok_or_else(|| TalosError::ConfigInvalid("no EtcdSpec in talosctl output".to_string()))?;
    let addresses = |key: &str| -> Vec<String> {
        spec.get(key)
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(EtcdSpecInfo {
        advertised: addresses("advertisedAddresses"),
        listen_peer: addresses("listenPeerAddresses"),
        listen_client: addresses("listenClientAddresses"),
    })
}

/// Get KubeSpan peer status for a node in `context` (async, with timeout)
pub async fn get_kubespan_peers_for_node(
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<Vec<KubeSpanPeerStatus>, TalosError> {
    let output = get_resource_yaml(context, node_ip, config_path, &["kubespanpeerstatus"]).await?;
    parse_kubespan_peers_yaml(&output)
}

/// Whether KubeSpan is enabled on a node in `context` (async, with timeout).
/// Any failure reads as "not enabled", like [`is_kubespan_enabled`].
pub async fn is_kubespan_enabled_for_node(
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
) -> bool {
    get_resource_yaml(context, node_ip, config_path, &["kubespanconfig"])
        .await
        .is_ok_and(|output| parse_kubespan_settings(&output).enabled)
}

/// KubeSpan settings of a node, from its KubeSpanConfig resource
///
/// Executes: `talosctl -n <node> get kubespanconfig -o yaml`
pub async fn get_kubespan_settings_for_node(
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<KubeSpanSettings, TalosError> {
    let output = get_resource_yaml(context, node_ip, config_path, &["kubespanconfig"]).await?;
    Ok(parse_kubespan_settings(&output))
}

/// Get the machine config resource of a node in `context` (async, with timeout)
pub async fn get_machine_config_for_node(
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<MachineConfigInfo, TalosError> {
    let output = get_resource_yaml(context, node_ip, config_path, &["machineconfig"]).await?;
    parse_machine_config_yaml(&output)
}

/// The node's machine config as text (it holds secrets: mask before
/// showing it)
///
/// Executes: `talosctl -n <node> get machineconfig -o yaml`
pub async fn get_machine_config_text_for_node(
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<MachineConfigText, TalosError> {
    // Megabytes with inline manifests (a CNI's), and twice: active and stored
    let output = get_resource_yaml_within(
        MACHINE_CONFIG_TIMEOUT,
        context,
        node_ip,
        config_path,
        &["machineconfig"],
    )
    .await?;
    parse_machine_config_text(&output)
}

/// System extensions installed on the node
///
/// Executes: `talosctl -n <node> get extensions -o yaml`
pub async fn get_extensions_for_node(
    context: Option<&str>,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<Vec<ExtensionInfo>, TalosError> {
    let output = get_resource_yaml(context, node_ip, config_path, &["extensions"]).await?;
    Ok(parse_extensions_yaml(&output))
}

/// Get the node's addresses, gateways, DNS servers and connectivity status
///
/// Executes (concurrently) against `node_ip`:
/// `get nodeaddress current-no-k8s`, `get routestatus`, `get resolvers`,
/// `get networkstatus`. Fields whose query fails are left empty / `None`;
/// an error is returned only if every query fails.
pub async fn get_node_network_info(
    context: &str,
    node_ip: &str,
    config_path: Option<&str>,
) -> Result<NodeNetworkInfo, TalosError> {
    let (addresses, routes, resolvers, status, ts_link, address_status, kubespan) = tokio::join!(
        get_resource_yaml(
            Some(context),
            node_ip,
            config_path,
            &["nodeaddress", "current-no-k8s"]
        ),
        get_resource_yaml(Some(context), node_ip, config_path, &["routestatus"]),
        get_resource_yaml(Some(context), node_ip, config_path, &["resolvers"]),
        get_resource_yaml(Some(context), node_ip, config_path, &["networkstatus"]),
        get_resource_yaml(
            Some(context),
            node_ip,
            config_path,
            &["linkstatus", TAILSCALE_LINK]
        ),
        get_resource_yaml(Some(context), node_ip, config_path, &["addressstatus"]),
        get_resource_yaml(Some(context), node_ip, config_path, &["kubespanpeerstatus"]),
    );

    if let (Err(e), Err(_), Err(_), Err(_)) = (&addresses, &routes, &resolvers, &status) {
        return Err(TalosError::Connection(e.to_string()));
    }

    // No tailscale0 link (the get fails with NotFound): no Tailscale
    let tailscale = ts_link.ok().and_then(|link| {
        parse_tailscale_link(
            &link,
            address_status.as_deref().unwrap_or(""),
            routes.as_deref().unwrap_or(""),
        )
    });

    Ok(NodeNetworkInfo {
        tailscale,
        kubespan_peers: kubespan
            .ok()
            .and_then(|y| parse_kubespan_peers_yaml(&y).ok()),
        addresses: addresses
            .map(|y| parse_spec_string_list(&y, "addresses"))
            .unwrap_or_default(),
        gateways: routes
            .map(|y| parse_default_gateways(&y))
            .unwrap_or_default(),
        dns_servers: resolvers
            .map(|y| parse_spec_string_list(&y, "dnsServers"))
            .unwrap_or_default(),
        connectivity_ready: status.ok().and_then(|y| parse_connectivity_ready(&y)),
    })
}

/// Iterate over the `spec` of each document in multi-document talosctl YAML
fn yaml_specs(yaml_str: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    resource_documents::<serde_json::Value>(yaml_str)
        .filter_map(|mut doc| doc.get_mut("spec").map(serde_json::Value::take))
}

/// Collect a string list field from the first document's spec
fn parse_spec_string_list(yaml_str: &str, field: &str) -> Vec<String> {
    yaml_specs(yaml_str)
        .next()
        .and_then(|spec| {
            spec.get(field)?.as_array().map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Gateways of default routes (empty destination) in the main table
fn parse_default_gateways(yaml_str: &str) -> Vec<String> {
    let mut gateways: Vec<String> = Vec::new();
    for spec in yaml_specs(yaml_str) {
        let field = |name: &str| spec.get(name).and_then(|v| v.as_str()).unwrap_or("");
        let gateway = field("gateway");
        if field("dst").is_empty()
            && field("table") == "main"
            && !gateway.is_empty()
            && !gateways.iter().any(|g| g == gateway)
        {
            gateways.push(gateway.to_string());
        }
    }
    gateways
}

/// Link created by the Tailscale system extension
const TAILSCALE_LINK: &str = "tailscale0";

/// `tailscale0` state from its LinkStatus, its addresses from AddressStatus
/// and its routes from RouteStatus documents
fn parse_tailscale_link(link: &str, addresses: &str, routes: &str) -> Option<TailscaleLink> {
    let spec = yaml_specs(link).next()?;
    let on_link = |spec: &serde_json::Value, key: &str| {
        spec.get(key).and_then(|v| v.as_str()) == Some(TAILSCALE_LINK)
    };
    let addresses = yaml_specs(addresses)
        .filter(|spec| on_link(spec, "linkName"))
        .filter_map(|spec| spec.get("address")?.as_str().map(str::to_string))
        .filter(|a| !a.starts_with("fe80:"))
        .collect();
    let mut peer_routes = 0;
    let mut subnet_routes = Vec::new();
    for spec in yaml_specs(routes).filter(|spec| on_link(spec, "outLinkName")) {
        let Some(dst) = spec.get("dst").and_then(|v| v.as_str()) else {
            continue;
        };
        if dst.is_empty() || dst.starts_with("fe80:") {
            continue;
        }
        if is_tailnet_address(dst) {
            peer_routes += 1;
        } else if !subnet_routes.iter().any(|r| r == dst) {
            subnet_routes.push(dst.to_string());
        }
    }
    Some(TailscaleLink {
        up: spec
            .get("linkState")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        mtu: spec.get("mtu").and_then(|v| v.as_u64()),
        addresses,
        peer_routes,
        subnet_routes,
    })
}

/// Whether a route destination is a tailnet address: in `100.64.0.0/10`
/// (CGNAT) or Tailscale's `fd7a:115c:a1e0::/48`
fn is_tailnet_address(dst: &str) -> bool {
    if dst.starts_with("fd7a:115c:a1e0:") {
        return true;
    }
    let ip = dst.split('/').next().unwrap_or(dst);
    match ip.parse::<std::net::Ipv4Addr>() {
        Ok(ip) => {
            let [a, b, ..] = ip.octets();
            a == 100 && (64..128).contains(&b)
        }
        Err(_) => false,
    }
}

/// `connectivityReady` from the NetworkStatus resource
fn parse_connectivity_ready(yaml_str: &str) -> Option<bool> {
    yaml_specs(yaml_str).find_map(|spec| spec.get("connectivityReady").and_then(|v| v.as_bool()))
}

/// `spec.enabled` and `spec.advertiseKubernetesNetworks` of KubeSpanConfig;
/// both false when the resource is absent (KubeSpan not configured)
fn parse_kubespan_settings(output: &str) -> KubeSpanSettings {
    resource_documents::<serde_json::Value>(output)
        .find_map(|doc| {
            let spec = doc.get("spec")?;
            let flag = |key: &str| spec.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
            Some(KubeSpanSettings {
                enabled: flag("enabled"),
                advertise_kubernetes_networks: flag("advertiseKubernetesNetworks"),
            })
        })
        .unwrap_or_default()
}

/// A COSI resource document; only `metadata.id` and `spec` are read
#[derive(Deserialize)]
struct Resource<S> {
    metadata: Option<ResourceMetadata>,
    spec: Option<S>,
}

#[derive(Deserialize)]
struct ResourceMetadata {
    id: Option<String>,
}

/// `(metadata.id, spec)` of each resource in `talosctl get … -o yaml`
/// output. Resources without an id are skipped, and so are documents that
/// don't fit `S`, so one odd resource doesn't hide the others.
fn resources<S: DeserializeOwned + Default>(
    yaml_str: &str,
) -> impl Iterator<Item = (String, S)> + '_ {
    resource_documents::<Resource<S>>(yaml_str).filter_map(|doc| {
        let id = doc.metadata?.id.filter(|id| !id.is_empty())?;
        Some((id, doc.spec.unwrap_or_default()))
    })
}

/// For fields whose shape varies across Talos versions (or isn't what the
/// name suggests): an unexpected shape reads as absent instead of
/// dropping the whole document.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// `spec` of VolumeStatuses.block.talos.dev
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct VolumeStatusSpec {
    encryption_provider: Option<String>,
    phase: Option<String>,
    pretty_size: Option<String>,
    /// directory, partition, symlink, …
    #[serde(rename = "type")]
    volume_type: Option<String>,
    filesystem: Option<String>,
    mount_location: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    mount_spec: Option<MountSpec>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MountSpec {
    target_path: Option<String>,
}

/// Parse volume status YAML output from talosctl
fn parse_volume_status_yaml(yaml_str: &str) -> Result<Vec<VolumeStatus>, TalosError> {
    Ok(resources::<VolumeStatusSpec>(yaml_str)
        .map(|(id, spec)| {
            // Volumes without a size or filesystem (directories, symlinks)
            // show their type instead
            let size = spec
                .pretty_size
                .or_else(|| spec.volume_type.clone())
                .unwrap_or_default();
            let filesystem = spec.filesystem.or_else(|| spec.volume_type.clone());
            let mount_location = spec
                .mount_location
                .or_else(|| spec.mount_spec.and_then(|m| m.target_path))
                .or_else(|| id.starts_with('/').then(|| id.clone()));
            VolumeStatus {
                encryption_provider: spec.encryption_provider,
                phase: spec.phase.unwrap_or_else(|| "unknown".to_string()),
                size,
                filesystem,
                mount_location,
                id,
            }
        })
        .collect())
}

/// `spec` of Disks.block.talos.dev (snake_case keys, unlike most resources)
#[derive(Deserialize, Default)]
struct DiskSpec {
    dev_path: Option<String>,
    size: Option<u64>,
    human_size: Option<String>,
    pretty_size: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    transport: Option<String>,
    rotational: Option<bool>,
    readonly: Option<bool>,
    cdrom: Option<bool>,
    wwid: Option<String>,
    bus_path: Option<String>,
}

/// Parse disks YAML output from talosctl
fn parse_disks_yaml(yaml_str: &str) -> Result<Vec<DiskInfo>, TalosError> {
    let non_empty = |s: Option<String>| s.filter(|s| !s.is_empty());
    Ok(resources::<DiskSpec>(yaml_str)
        .map(|(id, spec)| DiskInfo {
            id,
            dev_path: spec.dev_path.unwrap_or_default(),
            size: spec.size.unwrap_or(0),
            size_pretty: spec.human_size.or(spec.pretty_size).unwrap_or_default(),
            model: non_empty(spec.model),
            serial: non_empty(spec.serial),
            transport: non_empty(spec.transport),
            rotational: spec.rotational.unwrap_or(false),
            readonly: spec.readonly.unwrap_or(false),
            cdrom: spec.cdrom.unwrap_or(false),
            wwid: non_empty(spec.wwid),
            bus_path: non_empty(spec.bus_path),
        })
        .collect())
}

/// Parse machine config YAML output from talosctl.
///
/// `talosctl get machineconfig -o yaml` emits MULTIPLE documents (typically the
/// `persistent` on-disk config and the active `v1alpha1` config), so we must
/// parse each document separately — a single `from_str` over the whole blob
/// fails with "deserializing from YAML containing more than one document is not
/// supported", leaving the config-drift column blank. We prefer the canonical
/// `v1alpha1` document and fall back to the first document that carries a
/// version.
fn parse_machine_config_yaml(yaml_str: &str) -> Result<MachineConfigInfo, TalosError> {
    let mut fallback: Option<MachineConfigInfo> = None;

    for doc in resource_documents::<serde_json::Value>(yaml_str) {
        let metadata = doc.get("metadata");

        // `version` is a resource revision — usually a number, occasionally a
        // string — so accept either scalar form.
        let version = metadata
            .and_then(|m| m.get("version"))
            .and_then(scalar_to_string)
            .filter(|s| !s.is_empty());
        let Some(version) = version else {
            continue;
        };

        let id = metadata
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // The real config puts `spec` as a serialized string, so `machine.type`
        // is only reachable when `spec` is a mapping (as in tests); otherwise it
        // stays `None`, which is fine — callers only rely on `version` here.
        let machine_type = doc
            .get("spec")
            .and_then(|s| s.get("machine"))
            .and_then(|m| m.get("type"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let info = MachineConfigInfo {
            version,
            machine_type,
        };

        if id == "v1alpha1" {
            return Ok(info);
        }
        fallback.get_or_insert(info);
    }

    fallback.ok_or_else(|| {
        TalosError::Connection("no machineconfig document with a version found".to_string())
    })
}

/// Render a YAML scalar (`version` may be a number or a string) as a String.
fn scalar_to_string(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        Some(s.to_string())
    } else if let Some(n) = v.as_u64() {
        Some(n.to_string())
    } else if let Some(n) = v.as_i64() {
        Some(n.to_string())
    } else {
        v.as_f64().map(|n| n.to_string())
    }
}

/// Top-level documents of `talosctl get … -o yaml` output. Documents nested
/// in a block scalar (a machine config's) are indented, so they stay whole.
/// A document that doesn't parse as `T` is skipped.
fn resource_documents<T: DeserializeOwned>(yaml_str: &str) -> impl Iterator<Item = T> + '_ {
    yaml_str
        .split("\n---\n")
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .filter_map(|d| serde_saphyr::from_str::<T>(d).ok())
}

/// The `v1alpha1` (active) and `persistent` machine config resources
fn parse_machine_config_text(yaml_str: &str) -> Result<MachineConfigText, TalosError> {
    let mut active = None;
    let mut persistent = None;
    for doc in resource_documents::<serde_json::Value>(yaml_str) {
        let metadata = doc.get("metadata");
        let field = |key: &str| metadata.and_then(|m| m.get(key)).and_then(scalar_to_string);
        let Some(spec) = doc.get("spec").and_then(|s| s.as_str()) else {
            continue;
        };
        match field("id").as_deref() {
            Some("v1alpha1") => {
                active = Some(MachineConfigText {
                    active: spec.to_string(),
                    version: field("version").unwrap_or_default(),
                    updated: field("updated"),
                    staged_differs: false,
                })
            }
            Some("persistent") => persistent = Some(spec.to_string()),
            _ => {}
        }
    }
    let mut config = active.ok_or_else(|| {
        TalosError::ConfigInvalid("no v1alpha1 machine config in the output".into())
    })?;
    config.staged_differs = persistent.is_some_and(|p| p != config.active);
    Ok(config)
}

/// ExtensionStatus resources, in the order the node loads them
fn parse_extensions_yaml(yaml_str: &str) -> Vec<ExtensionInfo> {
    resource_documents::<serde_json::Value>(yaml_str)
        .filter_map(|doc| {
            let spec = doc.get("spec")?;
            let meta = spec.get("metadata");
            let field = |key: &str| {
                meta.and_then(|m| m.get(key))
                    .and_then(scalar_to_string)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default()
            };
            Some(ExtensionInfo {
                name: field("name"),
                version: field("version"),
                author: field("author"),
                description: field("description"),
                image: spec
                    .get("image")
                    .and_then(scalar_to_string)
                    .unwrap_or_default(),
            })
        })
        .filter(|e| !e.name.is_empty())
        .collect()
}

/// `spec` of KubeSpanPeerStatuses.kubespan.talos.dev
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct KubeSpanPeerSpec {
    label: Option<String>,
    endpoint: Option<String>,
    state: Option<String>,
    last_handshake_time: Option<String>,
    receive_bytes: Option<u64>,
    transmit_bytes: Option<u64>,
}

/// Parse KubeSpan peer status YAML output from talosctl
fn parse_kubespan_peers_yaml(yaml_str: &str) -> Result<Vec<KubeSpanPeerStatus>, TalosError> {
    Ok(resources::<KubeSpanPeerSpec>(yaml_str)
        .map(|(id, spec)| KubeSpanPeerStatus {
            label: spec.label.unwrap_or_else(|| id.clone()),
            id,
            endpoint: spec.endpoint,
            state: spec.state.unwrap_or_else(|| "unknown".to_string()),
            last_handshake: spec.last_handshake_time,
            rx_bytes: spec.receive_bytes.unwrap_or(0),
            tx_bytes: spec.transmit_bytes.unwrap_or(0),
        })
        .collect())
}

/// `spec` of Members.cluster.talos.dev
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MemberSpec {
    addresses: Option<Vec<String>>,
    hostname: Option<String>,
    machine_type: Option<String>,
    operating_system: Option<String>,
}

/// Parse discovery members YAML output from talosctl
fn parse_discovery_members_yaml(yaml_str: &str) -> Result<Vec<DiscoveryMember>, TalosError> {
    Ok(resources::<MemberSpec>(yaml_str)
        .map(|(id, spec)| DiscoveryMember {
            id,
            addresses: spec.addresses.unwrap_or_default(),
            hostname: spec.hostname.unwrap_or_default(),
            machine_type: spec.machine_type.unwrap_or_else(|| "unknown".to_string()),
            operating_system: spec.operating_system.unwrap_or_default(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn volume_key_types_leave_secrets_out() {
        let yaml = "node: 10.0.0.1\nmetadata:\n    id: STATE\nspec:\n    encryption:\n        provider: luks2\n        keys:\n            - slot: 0\n              type: tpm\n            - slot: 1\n              type: static\n              staticPassphrase: hunter2\n";
        assert_eq!(super::parse_key_types(yaml), ["tpm", "static"]);
        assert!(super::parse_key_types("spec:\n    phase: ready\n").is_empty());
    }

    /// `talosctl get machineconfig -o yaml`: the stored config first, then
    /// the active one, each a multi-document config in a block scalar
    fn machine_config_output(persistent_hostname: &str) -> String {
        let config = |hostname: &str| {
            format!(
                "    version: v1alpha1\n    machine:\n        type: controlplane\n    \
                 ---\n    apiVersion: v1alpha1\n    kind: HostnameConfig\n    \
                 hostname: {hostname}\n"
            )
        };
        format!(
            "node: 10.0.0.1\nmetadata:\n    namespace: config\n    \
             type: MachineConfigs.config.talos.dev\n    id: persistent\n    version: 3\n\
             spec: |\n{}---\nnode: 10.0.0.1\nmetadata:\n    namespace: config\n    \
             type: MachineConfigs.config.talos.dev\n    id: v1alpha1\n    version: 4\n    \
             updated: 2026-10-31T07:25:48Z\nspec: |\n{}",
            config(persistent_hostname),
            config("bc1")
        )
    }

    #[test]
    fn parses_active_machine_config_and_staged_change() {
        let config = super::parse_machine_config_text(&machine_config_output("bc1")).unwrap();
        assert_eq!(config.version, "4");
        assert_eq!(config.updated.as_deref(), Some("2026-10-31T07:25:48Z"));
        assert!(config.active.starts_with("version: v1alpha1\n"));
        // The nested document separator stays inside the config
        assert!(
            config
                .active
                .contains("\n---\napiVersion: v1alpha1\nkind: HostnameConfig")
        );
        assert!(!config.staged_differs);

        let staged = super::parse_machine_config_text(&machine_config_output("bc9")).unwrap();
        assert!(staged.staged_differs);
        assert!(super::parse_machine_config_text("").is_err());
    }

    #[test]
    fn parses_kubespan_settings() {
        let yaml = "node: 10.0.0.1
metadata:
    namespace: config
    type: KubeSpanConfigs.kubespan.talos.dev
    id: kubespan
spec:
    enabled: true
    clusterId: x
    forceRouting: true
    advertiseKubernetesNetworks: true
    mtu: 1420
";
        assert_eq!(
            super::parse_kubespan_settings(yaml),
            super::KubeSpanSettings {
                enabled: true,
                advertise_kubernetes_networks: true
            }
        );
        assert_eq!(
            super::parse_kubespan_settings(""),
            super::KubeSpanSettings::default()
        );
    }

    #[test]
    fn parses_extensions() {
        let yaml = "node: 10.0.0.1
metadata:
    namespace: runtime
    type: ExtensionStatuses.runtime.talos.dev
    id: 0
spec:
    image: 0.sqsh
    metadata:
        name: drbd
        version: 9.2.14-v1.11.5
        author: Sidero Labs
        description: |
            This system extension provides DRBD kernel modules.
        compatibility:
            talos:
                version: '>= v1.11.0'
---
node: 10.0.0.1
metadata:
    namespace: runtime
    type: ExtensionStatuses.runtime.talos.dev
    id: 1
spec:
    image: 1.sqsh
    metadata:
        name: zfs
        version: 2.4.4-v1.14.5
        author: Aenix, Enix
        description: ZFS
";
        let extensions = super::parse_extensions_yaml(yaml);
        assert_eq!(extensions.len(), 2);
        assert_eq!(extensions[0].name, "drbd");
        assert_eq!(extensions[0].version, "9.2.14-v1.11.5");
        assert_eq!(extensions[0].author, "Sidero Labs");
        assert_eq!(
            extensions[0].description,
            "This system extension provides DRBD kernel modules."
        );
        assert_eq!(extensions[0].image, "0.sqsh");
        assert_eq!(extensions[1].name, "zfs");
        assert!(super::parse_extensions_yaml("").is_empty());
    }

    #[test]
    fn parses_etcd_spec() {
        let yaml = "node: 10.22.1.3
metadata:
    namespace: etcd
    type: EtcdSpecs.etcd.talos.dev
    id: etcd
spec:
    name: kpc3
    advertisedAddresses:
        - 100.100.0.6
    listenPeerAddresses:
        - 100.100.0.6
        - 116.202.55.89
    listenClientAddresses:
        - 127.0.0.1
        - 100.100.0.6
        - 116.202.55.89
    image: registry.k8s.io/etcd:v3.7.2
";
        let spec = parse_etcd_spec_yaml(yaml).unwrap();
        assert_eq!(spec.advertised, ["100.100.0.6"]);
        assert_eq!(spec.listen_peer, ["100.100.0.6", "116.202.55.89"]);
        assert_eq!(
            spec.listen_client,
            ["127.0.0.1", "100.100.0.6", "116.202.55.89"]
        );
        assert!(parse_etcd_spec_yaml("").is_err());
    }

    use super::*;

    #[test]
    fn parses_tailscale_link_addresses_and_routes() {
        let link = "spec:\n    mtu: 1280\n    operationalState: unknown\n    linkState: true\n";
        let addresses = "\
spec:\n    address: 100.64.0.14/32\n    linkName: tailscale0\n---
spec:\n    address: fe80::89ef:d775:4055:c2da/64\n    linkName: tailscale0\n---
spec:\n    address: 37.27.227.99/26\n    linkName: enp0s31f6\n";
        let routes = "\
spec:\n    dst: 100.64.0.20/32\n    outLinkName: tailscale0\n    table: \"52\"\n---
spec:\n    dst: fd7a:115c:a1e0::14/128\n    outLinkName: tailscale0\n---
spec:\n    dst: 10.107.0.0/16\n    outLinkName: tailscale0\n---
spec:\n    dst: \"\"\n    gateway: 37.27.227.65\n    outLinkName: enp0s31f6\n";
        let ts = parse_tailscale_link(link, addresses, routes).unwrap();
        assert!(ts.up);
        assert_eq!(ts.mtu, Some(1280));
        assert_eq!(ts.addresses, ["100.64.0.14/32"]);
        assert_eq!(ts.peer_routes, 2);
        assert_eq!(ts.subnet_routes, ["10.107.0.0/16"]);
        assert!(parse_tailscale_link("", addresses, routes).is_none());
    }

    #[test]
    fn tailnet_addresses() {
        assert!(is_tailnet_address("100.64.0.1/32"));
        assert!(is_tailnet_address("100.127.255.1/32"));
        assert!(!is_tailnet_address("100.128.0.1/32"));
        assert!(!is_tailnet_address("10.10.0.0/16"));
        assert!(is_tailnet_address("fd7a:115c:a1e0::e/128"));
    }

    #[test]
    fn test_parse_volume_status() {
        let yaml = r#"
node: 10.5.0.2
metadata:
    namespace: runtime
    type: VolumeStatuses.block.talos.dev
    id: STATE
    version: "1"
    phase: running
spec:
    phase: ready
    location: /dev/sda6
    encryptionProvider: luks2
    filesystem: xfs
    mountLocation: /system/state
    prettySize: 100 MiB
---
node: 10.5.0.2
metadata:
    namespace: runtime
    type: VolumeStatuses.block.talos.dev
    id: EPHEMERAL
    version: "1"
    phase: running
spec:
    phase: ready
    location: /dev/sda5
    filesystem: xfs
    mountLocation: /var
    prettySize: 10 GiB
"#;

        let volumes = parse_volume_status_yaml(yaml).unwrap();
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].id, "STATE");
        assert_eq!(volumes[0].encryption_provider, Some("luks2".to_string()));
        assert_eq!(volumes[1].id, "EPHEMERAL");
        assert_eq!(volumes[1].encryption_provider, None);
        assert_eq!(volumes[0].phase, "ready");
        assert_eq!(volumes[0].size, "100 MiB");
        assert_eq!(volumes[0].filesystem.as_deref(), Some("xfs"));
        assert_eq!(volumes[0].mount_location.as_deref(), Some("/system/state"));
    }

    #[test]
    fn volume_status_fallbacks() {
        // Directory and symlink volumes have no size, filesystem or
        // mountLocation; the middle document doesn't fit the spec at all
        let yaml = "node: 10.5.0.2
metadata:
    namespace: runtime
    type: VolumeStatuses.block.talos.dev
    id: /var/lib
    version: 1
    phase: running
spec:
    phase: ready
    type: directory
    mountSpec:
        targetPath: /var/lib
        selinuxLabel: system_u:object_r:var_lib_t:s0
---
node: 10.5.0.2
metadata:
    id: BROKEN
spec:
    phase: [not, a, string]
---
node: 10.5.0.2
metadata:
    namespace: runtime
    type: VolumeStatuses.block.talos.dev
    id: /var/log/audit
spec:
    type: symlink
    mountSpec: unexpected
---
node: 10.5.0.2
metadata:
    namespace: runtime
    type: VolumeStatuses.block.talos.dev
    id: META
spec: {}
";
        let volumes = parse_volume_status_yaml(yaml).unwrap();
        let ids: Vec<_> = volumes.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["/var/lib", "/var/log/audit", "META"]);

        assert_eq!(volumes[0].size, "directory");
        assert_eq!(volumes[0].filesystem.as_deref(), Some("directory"));
        assert_eq!(volumes[0].mount_location.as_deref(), Some("/var/lib"));

        // An odd mountSpec reads as absent: the id, a path, is the mount
        assert_eq!(volumes[1].phase, "unknown");
        assert_eq!(volumes[1].mount_location.as_deref(), Some("/var/log/audit"));

        assert_eq!(volumes[2].size, "");
        assert_eq!(volumes[2].filesystem, None);
        assert_eq!(volumes[2].mount_location, None);
    }

    #[test]
    fn kubespan_enabled_reads_spec() {
        let config = |enabled: bool| {
            format!(
                "node: 10.0.0.1\nmetadata:\n    namespace: config\n    \
                 type: KubeSpanConfigs.kubespan.talos.dev\n    id: kubespan\n\
                 spec:\n    enabled: {enabled}\n    forceRouting: true\n"
            )
        };
        assert!(parse_kubespan_settings(&config(true)).enabled);
        assert!(!parse_kubespan_settings(&config(false)).enabled);
        assert!(!parse_kubespan_settings("").enabled);
    }

    #[test]
    fn test_parse_machine_config() {
        let yaml = r#"
node: 10.5.0.2
metadata:
    namespace: config
    type: MachineConfigs.config.talos.dev
    id: v1alpha1
    version: "5"
spec:
    machine:
        type: controlplane
"#;

        let config = parse_machine_config_yaml(yaml).unwrap();
        assert_eq!(config.version, "5");
        assert_eq!(config.machine_type, Some("controlplane".to_string()));
    }

    /// Regression: real `talosctl get machineconfig -o yaml` returns MULTIPLE
    /// documents (persistent + v1alpha1) with a numeric `version` and a
    /// serialized-string `spec`. The old single-document parse failed outright
    /// ("more than one document is not supported"), blanking the config column.
    /// We must parse each document, handle a numeric version, and prefer the
    /// canonical v1alpha1 config over persistent.
    #[test]
    fn test_parse_machine_config_multi_document() {
        let yaml = r#"node: 10.6.0.2
metadata:
    namespace: config
    type: MachineConfigs.config.talos.dev
    id: persistent
    version: 7
spec: "version: v1alpha1\nmachine:\n    type: controlplane\n"
---
node: 10.6.0.2
metadata:
    namespace: config
    type: MachineConfigs.config.talos.dev
    id: v1alpha1
    version: 3
spec: "version: v1alpha1\nmachine:\n    type: controlplane\n"
"#;

        let config = parse_machine_config_yaml(yaml).unwrap();
        // Numeric version rendered as a string, taken from the canonical
        // v1alpha1 document (not persistent's 7).
        assert_eq!(config.version, "3");
        // spec is a serialized string here, so machine.type is not a mapping.
        assert_eq!(config.machine_type, None);
    }

    #[test]
    fn test_parse_disks() {
        let yaml = r#"
node: 172.20.0.5
metadata:
    namespace: runtime
    type: Disks.block.talos.dev
    id: sda
    version: 1
    owner: block.DisksController
    phase: running
spec:
    dev_path: /dev/sda
    size: 10485760000
    human_size: 10 GB
    io_size: 512
    sector_size: 512
    readonly: false
    cdrom: false
    model: QEMU HARDDISK
    modalias: scsi:t-0x00
    bus_path: /pci0000:00/0000:00:07.0/virtio4/host1/target1:0:0/1:0:0:0
    sub_system: /sys/class/block
    transport: virtio
    rotational: true
---
node: 172.20.0.5
metadata:
    namespace: runtime
    type: Disks.block.talos.dev
    id: nvme0n1
    version: 1
    owner: block.DisksController
    phase: running
spec:
    dev_path: /dev/nvme0n1
    size: 256060514304
    human_size: 256 GB
    io_size: 4096
    sector_size: 512
    readonly: false
    cdrom: false
    model: Samsung SSD 970 EVO Plus
    serial: S4EVNG0N123456
    wwid: nvme.144d-5334455...
    bus_path: /pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/nvme0n1
    sub_system: /sys/class/block
    transport: nvme
    rotational: false
"#;

        let disks = parse_disks_yaml(yaml).unwrap();
        assert_eq!(disks.len(), 2);

        // First disk - virtio HDD
        assert_eq!(disks[0].id, "sda");
        assert_eq!(disks[0].dev_path, "/dev/sda");
        assert_eq!(disks[0].size, 10485760000);
        assert_eq!(disks[0].size_pretty, "10 GB");
        assert_eq!(disks[0].model, Some("QEMU HARDDISK".to_string()));
        assert_eq!(disks[0].transport, Some("virtio".to_string()));
        assert!(disks[0].rotational);
        assert!(!disks[0].readonly);

        // Second disk - NVMe SSD
        assert_eq!(disks[1].id, "nvme0n1");
        assert_eq!(disks[1].dev_path, "/dev/nvme0n1");
        assert_eq!(disks[1].size_pretty, "256 GB");
        assert_eq!(disks[1].model, Some("Samsung SSD 970 EVO Plus".to_string()));
        assert_eq!(disks[1].serial, Some("S4EVNG0N123456".to_string()));
        assert_eq!(disks[1].transport, Some("nvme".to_string()));
        assert!(!disks[1].rotational);
    }

    #[test]
    fn test_parse_discovery_members() {
        let yaml = r#"
node: 192.168.9.11
metadata:
    namespace: cluster
    type: Members.cluster.talos.dev
    id: 3xKYjp
    version: 1
    owner: cluster.DiscoveryService
    phase: running
spec:
    nodeId: 3xKYjp
    addresses:
        - 192.168.9.11
    hostname: cp-1
    machineType: controlplane
    operatingSystem: Talos (v1.9.2)
---
node: 192.168.9.11
metadata:
    namespace: cluster
    type: Members.cluster.talos.dev
    id: 4yLZkq
    version: 1
    owner: cluster.DiscoveryService
    phase: running
spec:
    nodeId: 4yLZkq
    addresses:
        - 192.168.9.21
        - 10.244.0.1
    hostname: worker-1
    machineType: worker
    operatingSystem: Talos (v1.9.2)
"#;

        let members = parse_discovery_members_yaml(yaml).unwrap();
        assert_eq!(members.len(), 2);

        // Control plane node
        assert_eq!(members[0].id, "3xKYjp");
        assert_eq!(members[0].hostname, "cp-1");
        assert_eq!(members[0].machine_type, "controlplane");
        assert_eq!(members[0].addresses, vec!["192.168.9.11"]);
        assert!(members[0].operating_system.contains("Talos"));

        // Worker node with multiple addresses
        assert_eq!(members[1].id, "4yLZkq");
        assert_eq!(members[1].hostname, "worker-1");
        assert_eq!(members[1].machine_type, "worker");
        assert_eq!(members[1].addresses.len(), 2);
        assert!(members[1].addresses.contains(&"192.168.9.21".to_string()));
    }

    #[test]
    fn test_parse_discovery_members_empty() {
        let yaml = "";
        let members = parse_discovery_members_yaml(yaml).unwrap();
        assert!(members.is_empty());
    }

    #[test]
    fn members_args_include_talosconfig_when_config_path_set() {
        let args = members_command_args("mycluster", None, "10.0.0.1", Some("/etc/talos/config"));
        assert_eq!(
            args,
            vec![
                "--context",
                "mycluster",
                "--talosconfig",
                "/etc/talos/config",
                "-n",
                "10.0.0.1",
                "get",
                "members",
                "-o",
                "yaml",
            ]
        );
    }

    #[test]
    fn members_args_pin_endpoint_to_node() {
        let args = members_command_args("kprod", Some("5.9.183.236"), "5.9.183.236", None);
        assert_eq!(
            args,
            vec![
                "--context",
                "kprod",
                "-e",
                "5.9.183.236",
                "-n",
                "5.9.183.236",
                "get",
                "members",
                "-o",
                "yaml",
            ]
        );
    }

    #[test]
    fn endpoint_host_strips_port_and_brackets() {
        assert_eq!(endpoint_host("10.0.0.1"), "10.0.0.1");
        assert_eq!(endpoint_host("10.0.0.1:50000"), "10.0.0.1");
        assert_eq!(endpoint_host("talos.example.com:443"), "talos.example.com");
        assert_eq!(endpoint_host("[2a01::1]:50000"), "2a01::1");
        assert_eq!(endpoint_host("[2a01::1]"), "2a01::1");
        assert_eq!(endpoint_host("2a01::1"), "2a01::1");
    }

    #[test]
    fn members_args_omit_talosconfig_when_none() {
        let args = members_command_args("mycluster", None, "10.0.0.1", None);
        assert!(!args.contains(&"--talosconfig"));
        assert_eq!(&args[..2], &["--context", "mycluster"]);
    }

    #[test]
    fn test_parse_discovery_members_invalid_yaml() {
        // Should skip invalid documents and not panic
        let yaml = r#"
not valid yaml: [
---
node: 192.168.9.11
metadata:
    namespace: cluster
    type: Members.cluster.talos.dev
    id: valid
spec:
    hostname: valid-host
    machineType: controlplane
"#;
        let members = parse_discovery_members_yaml(yaml).unwrap();
        // Should have parsed the valid document
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, "valid");
    }

    #[test]
    fn network_info_parsing() {
        let addresses = "node: 65.21.17.62\nmetadata:\n    id: current-no-k8s\nspec:\n    addresses:\n        - 65.21.17.62/26\n        - 100.64.0.8/32\n    sortAlgorithm: v1\n";
        assert_eq!(
            parse_spec_string_list(addresses, "addresses"),
            vec!["65.21.17.62/26", "100.64.0.8/32"]
        );

        let routes = "spec:\n    dst: 10.44.2.0/24\n    gateway: 10.44.2.147\n    table: main\n---\nspec:\n    dst: \"\"\n    gateway: \"\"\n    table: \"180\"\n---\nspec:\n    dst: \"\"\n    gateway: 65.21.17.1\n    table: main\n---\nspec:\n    dst: \"\"\n    gateway: 65.21.17.1\n    table: main\n";
        assert_eq!(parse_default_gateways(routes), vec!["65.21.17.1"]);

        let resolvers = "spec:\n    dnsServers:\n        - 185.12.64.1\n        - 1.1.1.1\n";
        assert_eq!(
            parse_spec_string_list(resolvers, "dnsServers"),
            vec!["185.12.64.1", "1.1.1.1"]
        );

        let status = "spec:\n    addressReady: true\n    connectivityReady: false\n";
        assert_eq!(parse_connectivity_ready(status), Some(false));
        assert_eq!(parse_connectivity_ready("spec: {}\n"), None);
    }

    #[test]
    fn parses_kubespan_peers_from_talosctl_yaml() {
        let yaml = "node: 192.0.2.1
metadata:
    namespace: kubespan
    type: KubeSpanPeerStatuses.kubespan.talos.dev
    id: AAAAkeyOne+/=
    created: 2026-10-16T09:19:28Z
spec:
    endpoint: 192.0.2.2:51820
    label: node2
    state: up
    receiveBytes: 1213241208180
    transmitBytes: 1847078950984
    lastHandshakeTime: 2026-10-26T20:52:13.708617698Z
    lastUsedEndpoint: 192.0.2.2:51820
    lastEndpointChange: 2026-10-16T09:19:30Z
---
node: 192.0.2.1
metadata:
    id: BBBBkeyTwo=
spec:
    label: node3
    state: down
    receiveBytes: 0
    transmitBytes: 0
";
        let peers = parse_kubespan_peers_yaml(yaml).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].id, "AAAAkeyOne+/=");
        assert_eq!(peers[0].label, "node2");
        assert_eq!(peers[0].endpoint.as_deref(), Some("192.0.2.2:51820"));
        assert_eq!(peers[0].state, "up");
        assert_eq!(peers[0].rx_bytes, 1_213_241_208_180);
        assert_eq!(peers[0].tx_bytes, 1_847_078_950_984);
        assert_eq!(
            peers[0].last_handshake.as_deref(),
            Some("2026-10-26T20:52:13.708617698Z")
        );
        assert_eq!(peers[1].state, "down");
        assert_eq!(peers[1].endpoint, None);
    }
}
