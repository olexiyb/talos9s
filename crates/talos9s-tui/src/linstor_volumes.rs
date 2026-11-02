//! LINSTOR volumes and the DRBD state behind them
//!
//! LINSTOR's view of a volume (`linstor resource list`) can say every replica
//! is UpToDate while the kernel disagrees: a DRBD device that never grew
//! after a resize, or blocks still out of sync on an "Established" link.
//! So besides LINSTOR's resource view, each node's DRBD state is read from the
//! kernel through the Talos API:
//!
//! - `/sys/block/drbd<minor>/size`: the device size actually in use
//! - DRBD debugfs `resources/<res>/connections/<peer>/<vol>/proc_drbd`:
//!   connection / disk state and `oos:` (KiB out of sync) per peer
//! - DRBD debugfs `resources/<res>/connections/<peer>/debug`: `ap_in_flight`
//! - the Mounts API on the Primary's node: size of the filesystem on the
//!   DRBD device, which a resize must grow too
//! - the kernel log (Dmesg API), only on nodes with a StandAlone link, for
//!   why DRBD stopped reconnecting: whether a split-brain caused the
//!   StandAlone the kernel reports now
//!
//! debugfs is marked "format will change without notice"; anything that
//! can't be read or parsed is reported as unknown, never as a problem.

use crate::linstor::{
    controller_base, get_json, items, lenient, lenient_items, lenient_map, present,
};
use futures::StreamExt;
use k8s_openapi::serde_json::Value;
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Write as _;
use talos_rs::{MountUsage, TalosClient};
use talos9s_core::constants::{
    FS_OVERHEAD_FIXED_KIB, FS_OVERHEAD_PERCENT, USAGE_ERROR_PCT, USAGE_WARNING_PCT,
};
use talos9s_core::{HasHealth, HealthIndicator};

/// Volumes of the cluster's LINSTOR resources, with each replica's DRBD
/// state as LINSTOR reports it
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LinstorVolume {
    /// Resource (definition) name, e.g. `pvc-…`
    pub resource: String,
    pub volume_number: u32,
    /// `namespace/name` of the PVC, from the CSI driver's properties
    pub pvc: Option<String>,
    /// Size of the volume definition (what LINSTOR thinks it is)
    pub size_kib: Option<u64>,
    /// Resource group, and how many diskful replicas it places
    pub resource_group: Option<String>,
    pub place_count: Option<u32>,
    /// The definition is being deleted (`DELETE` flag)
    pub deleting: bool,
    /// One per node the resource is placed on, sorted by node
    pub replicas: Vec<Replica>,
}

impl LinstorVolume {
    /// Replicas holding data (not diskless tie-breakers or clients)
    pub fn diskful_replicas(&self) -> usize {
        self.replicas.iter().filter(|r| !r.diskless).count()
    }

    /// Filesystem used and size (KiB) where the volume is mounted
    pub fn usage_kib(&self) -> Option<(u64, u64)> {
        self.replicas.iter().find_map(|r| {
            let d = r.drbd.as_ref()?;
            Some((d.fs_used_kib?, d.fs_size_kib?))
        })
    }
}

/// One node's replica of a volume
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Replica {
    pub node: String,
    /// Replicated through DRBD. Without it (a STORAGE-only resource, e.g. a
    /// single local volume for an app that replicates itself) LINSTOR
    /// reports no disk state and there is no DRBD state to check.
    pub drbd_layer: bool,
    /// DRBD Primary (the volume is mounted on this node)
    pub in_use: bool,
    /// No local disk (tie-breaker or diskless client)
    pub diskless: bool,
    /// e.g. `UpToDate`, `Outdated`, `Inconsistent`, `Diskless`
    pub disk_state: String,
    pub minor: Option<u32>,
    /// Peers as LINSTOR reports them
    pub peers: Vec<PeerLink>,
    /// What the node's kernel reports, when it could be read
    pub drbd: Option<DrbdObserved>,
}

/// A replica's link to one peer, as LINSTOR reports it
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerLink {
    pub peer: String,
    pub connected: bool,
    /// Connection message, e.g. `Connected`, `Connecting`, `StandAlone`
    pub message: String,
    /// e.g. `Established`, `SyncSource`, `SyncTarget`
    pub replication: Option<String>,
}

/// DRBD state read from a node's kernel
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DrbdObserved {
    /// Device size from `/sys/block`
    pub size_kib: Option<u64>,
    /// Size and used space of the filesystem mounted from the device
    /// (Mounts API), only where it is mounted (the Primary) and not for raw
    /// block volumes
    pub fs_size_kib: Option<u64>,
    pub fs_used_kib: Option<u64>,
    /// I/O frozen on the device (the `s` flag in debugfs `proc_drbd`):
    /// quorum lost with `on-no-quorum suspend-io`, fencing, or by hand.
    /// Writes of the app using it hang until it resumes.
    pub io_suspended: Option<bool>,
    /// Per peer, from debugfs
    pub peers: HashMap<String, PeerObserved>,
}

/// DRBD state of one peer device, from debugfs
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerObserved {
    /// Connection state (`cs:`), e.g. `Established`
    pub cs: Option<String>,
    /// KiB this node has marked out of sync with the peer (`oos:`)
    pub oos_kib: Option<u64>,
    /// Application writes in flight to the peer (`ap_in_flight`)
    pub ap_in_flight_kib: Option<u64>,
    /// Progress of a running resync or verify with the peer
    pub sync: Option<SyncProgress>,
    /// The link is StandAlone because DRBD detected a split-brain it could
    /// not resolve, per the kernel log. `false` when the log doesn't show
    /// it, e.g. rotated out of the ring buffer.
    pub split_brain: bool,
}

impl PeerObserved {
    /// KiB out of sync with the peer, `None` when nothing is (or unknown)
    pub fn out_of_sync_kib(&self) -> Option<u64> {
        self.oos_kib.filter(|&oos| oos > 0)
    }
}

impl Replica {
    /// What the node's kernel reports about the link to `peer`
    pub fn observed(&self, peer: &str) -> Option<&PeerObserved> {
        self.drbd.as_ref()?.peers.get(peer)
    }
}

/// A resync (or online verify) in progress, from the `sync'ed:` /
/// `finish:` lines DRBD adds to debugfs `proc_drbd` while one runs
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SyncProgress {
    /// Online verify rather than resync
    pub verify: bool,
    pub percent: f32,
    /// Estimated seconds left; `None` when DRBD shows `∞`: the out-of-sync
    /// blocks aren't shrinking under the current writes
    pub finish_secs: Option<u64>,
    /// Speed over the last ~18 s (KiB/s)
    pub speed_kib: Option<u64>,
    /// No progress for over 3 minutes
    pub stalled: bool,
}

impl SyncProgress {
    /// e.g. `resync 37.0%, 109 MiB/s, ~4m 12s left`
    pub fn describe(&self) -> String {
        let what = if self.verify { "verify" } else { "resync" };
        let mut text = format!("{what} {:.1}%", self.percent);
        if let Some(speed) = self.speed_kib {
            let _ = write!(text, ", {}/s", format_kib(speed));
        }
        if self.stalled {
            text.push_str(", stalled for over 3 min");
        } else {
            match self.finish_secs {
                Some(secs) => {
                    let _ = write!(text, ", ~{} left", format_eta(secs));
                }
                None => text.push_str(", not converging under the current writes"),
            }
        }
        text
    }
}

/// Seconds as `1h 05m`, `4m 12s` or `9s`
fn format_eta(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Resync progress from a debugfs `proc_drbd` file, `None` when no resync
/// or verify is running (or the lines can't be parsed):
///
/// ```text
///     [===>................] sync'ed: 18.4% (9012/11048)M
///     finish: 0:04:12 speed: 112,345 (110,000 -- 98,000) want: 250,000 K/sec
/// ```
fn parse_sync_progress(content: &str) -> Option<SyncProgress> {
    let mut words = content.split_whitespace().peekable();
    let mut progress = None::<SyncProgress>;
    while let Some(word) = words.next() {
        match word {
            "sync'ed:" | "verified:" => {
                let percent = words.next()?.strip_suffix('%')?.parse().ok()?;
                progress = Some(SyncProgress {
                    verify: word == "verified:",
                    percent,
                    ..Default::default()
                });
            }
            "finish:" => {
                let p = progress.as_mut()?;
                let value = words.next()?;
                p.finish_secs = if value == "∞" {
                    None
                } else {
                    let parts: Vec<u64> = value
                        .split(':')
                        .map(|n| n.parse().ok())
                        .collect::<Option<_>>()?;
                    match parts[..] {
                        [h, m, s] => Some(h * 3600 + m * 60 + s),
                        _ => return None,
                    }
                };
            }
            "speed:" => {
                let p = progress.as_mut()?;
                p.speed_kib = words.next()?.replace(',', "").parse().ok();
            }
            "(stalled)" => progress.as_mut()?.stalled = true,
            _ => {}
        }
    }
    progress
}

/// How serious an [`Issue`] is. Its own type rather than `HealthIndicator`
/// because the order matters: issues sort worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

impl HasHealth for Severity {
    fn health(&self) -> HealthIndicator {
        match self {
            Severity::Error => HealthIndicator::Error,
            Severity::Warning => HealthIndicator::Warning,
        }
    }
}

/// A problem found on a volume
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub severity: Severity,
    pub text: String,
}

/// The LINSTOR satellite pod running on a node, where `drbdadm` can be run
#[derive(Debug, Clone, PartialEq)]
pub struct SatellitePod {
    pub namespace: String,
    pub name: String,
}

/// A suggested manual fix for a volume: what it does, and the commands
#[derive(Debug, Clone, PartialEq)]
pub struct Remedy {
    pub summary: String,
    pub commands: Vec<String>,
}

/// Label of the satellite pods created by the Piraeus operator
const SATELLITE_LABEL: &str = "app.kubernetes.io/component=linstor-satellite";
/// Container of a satellite pod that has `drbdadm`
const SATELLITE_CONTAINER: &str = "linstor-satellite";

/// Satellite pod per node name. Empty when they can't be listed; fix
/// suggestions then name the pod as a placeholder.
pub async fn fetch_satellites(client: &kube::Client) -> HashMap<String, SatellitePod> {
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::all(client.clone());
    let params = kube::api::ListParams::default().labels(SATELLITE_LABEL);
    match pods.list(&params).await {
        Ok(list) => list
            .items
            .into_iter()
            .filter_map(|pod| {
                let node = pod.spec?.node_name?;
                Some((
                    node,
                    SatellitePod {
                        namespace: pod.metadata.namespace?,
                        name: pod.metadata.name?,
                    },
                ))
            })
            .collect(),
        Err(e) => {
            tracing::debug!("LINSTOR satellite pods unavailable: {}", e);
            HashMap::new()
        }
    }
}

/// LINSTOR volumes of the cluster, `None` when it doesn't run Piraeus.
/// `Err` when the Kubernetes or LINSTOR API can't be queried.
pub async fn fetch_volumes(client: &kube::Client) -> Result<Option<Vec<LinstorVolume>>, String> {
    let base = match controller_base(client).await.map_err(|e| e.to_string())? {
        None => return Ok(None),
        Some(base) => base?,
    };
    let resources_path = format!("{base}/v1/view/resources");
    let definitions_path = format!("{base}/v1/resource-definitions?with_volume_definitions=true");
    let groups_path = format!("{base}/v1/resource-groups");
    let (resources, definitions, groups) = tokio::join!(
        get_json(client, &resources_path),
        get_json(client, &definitions_path),
        get_json(client, &groups_path),
    );
    let mut volumes = volumes_from(&resources?, &definitions?);
    // Without the groups the replica count is shown without a target
    match groups {
        Ok(groups) => apply_place_counts(&mut volumes, &groups),
        Err(e) => tracing::debug!("LINSTOR resource groups unavailable: {}", e),
    }
    Ok(Some(volumes))
}

/// Set each volume's `place_count` from its resource group's select filter
pub fn apply_place_counts(volumes: &mut [LinstorVolume], groups: &Value) {
    let counts: HashMap<String, u32> = items::<GroupJson>(groups)
        .map(|g| {
            (
                g.name,
                u32::try_from(g.select_filter.place_count).unwrap_or(u32::MAX),
            )
        })
        .collect();
    for volume in volumes {
        volume.place_count = volume
            .resource_group
            .as_deref()
            .and_then(|g| counts.get(g).copied());
    }
}

/// One entry of `/v1/resource-groups`; groups without a place count are
/// skipped
#[derive(Deserialize)]
struct GroupJson {
    name: String,
    select_filter: SelectFilterJson,
}

#[derive(Deserialize)]
struct SelectFilterJson {
    place_count: u64,
}

/// One entry of `/v1/resource-definitions?with_volume_definitions=true`
#[derive(Deserialize)]
struct DefinitionJson {
    name: String,
    #[serde(default, deserialize_with = "lenient_map")]
    props: HashMap<String, Value>,
    #[serde(default, deserialize_with = "lenient")]
    resource_group_name: Option<String>,
    #[serde(default, deserialize_with = "lenient_items")]
    flags: Vec<String>,
    #[serde(default, deserialize_with = "lenient_items")]
    volume_definitions: Vec<VolumeDefinitionJson>,
}

#[derive(Deserialize)]
struct VolumeDefinitionJson {
    volume_number: u64,
    size_kib: u64,
}

/// One entry of `/v1/view/resources`: a resource on one node
#[derive(Deserialize)]
struct ResourceJson {
    name: String,
    node_name: String,
    #[serde(default, deserialize_with = "lenient")]
    state: Option<ResourceStateJson>,
    #[serde(default, deserialize_with = "lenient_items")]
    flags: Vec<String>,
    #[serde(default, deserialize_with = "lenient")]
    layer_object: Option<LayerObjectJson>,
    #[serde(default, deserialize_with = "lenient_items")]
    volumes: Vec<VolumeJson>,
}

#[derive(Deserialize)]
struct ResourceStateJson {
    #[serde(default, deserialize_with = "lenient")]
    in_use: Option<bool>,
}

#[derive(Deserialize)]
struct LayerObjectJson {
    /// Any `drbd` entry means the resource has a DRBD layer
    #[serde(default, deserialize_with = "present")]
    drbd: Option<DrbdLayerJson>,
}

#[derive(Deserialize, Default)]
struct DrbdLayerJson {
    #[serde(default, deserialize_with = "lenient_map")]
    connections: HashMap<String, ConnectionJson>,
}

#[derive(Deserialize, Default)]
struct ConnectionJson {
    #[serde(default, deserialize_with = "lenient")]
    connected: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    message: Option<String>,
}

/// One volume of a resource; volumes without a number are skipped
#[derive(Deserialize)]
struct VolumeJson {
    volume_number: u64,
    #[serde(default, deserialize_with = "lenient")]
    provider_kind: Option<String>,
    #[serde(default, deserialize_with = "lenient_items")]
    layer_data_list: Vec<LayerDataJson>,
    #[serde(default, deserialize_with = "lenient")]
    state: Option<VolumeStateJson>,
}

#[derive(Deserialize)]
struct LayerDataJson {
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    layer_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    data: Option<LayerDataDataJson>,
}

#[derive(Deserialize)]
struct LayerDataDataJson {
    #[serde(default, deserialize_with = "lenient")]
    drbd_volume_definition: Option<DrbdVolumeDefinitionJson>,
}

#[derive(Deserialize)]
struct DrbdVolumeDefinitionJson {
    #[serde(default, deserialize_with = "lenient")]
    minor_number: Option<u64>,
}

#[derive(Deserialize)]
struct VolumeStateJson {
    #[serde(default, deserialize_with = "lenient")]
    disk_state: Option<String>,
    #[serde(default, deserialize_with = "lenient_map")]
    replication_states: HashMap<String, ReplicationJson>,
}

#[derive(Deserialize, Default)]
struct ReplicationJson {
    #[serde(default, deserialize_with = "lenient")]
    replication_state: Option<String>,
}

/// A volume without replicas, with what its definition (if any) says
fn volume_of(resource: &str, number: u64, definition: Option<&DefinitionJson>) -> LinstorVolume {
    let mut volume = LinstorVolume {
        resource: resource.to_string(),
        volume_number: u32::try_from(number).unwrap_or(u32::MAX),
        ..Default::default()
    };
    let Some(rd) = definition else {
        return volume;
    };
    let prop = |key: &str| rd.props.get(key).and_then(Value::as_str);
    volume.pvc = match (
        prop("Aux/csi.storage.k8s.io/pvc/namespace"),
        prop("Aux/csi.storage.k8s.io/pvc/name"),
    ) {
        (Some(ns), Some(name)) => Some(format!("{ns}/{name}")),
        _ => None,
    };
    volume.resource_group = rd.resource_group_name.clone();
    volume.deleting = rd.flags.iter().any(|f| f == "DELETE");
    // The last definition of a number wins, as LINSTOR's own map would
    volume.size_kib = rd
        .volume_definitions
        .iter()
        .rev()
        .find(|vd| vd.volume_number == number)
        .map(|vd| vd.size_kib);
    volume
}

/// Group LINSTOR's resource view (one entry per resource and node) into
/// volumes, with sizes and PVC names from the resource definitions
pub fn volumes_from(resources: &Value, definitions: &Value) -> Vec<LinstorVolume> {
    let defs: HashMap<String, DefinitionJson> = items::<DefinitionJson>(definitions)
        .map(|rd| (rd.name.clone(), rd))
        .collect();

    let mut volumes: HashMap<(String, u64), LinstorVolume> = HashMap::new();
    for res in items::<ResourceJson>(resources) {
        let (name, node) = (res.name, res.node_name);
        let in_use = res.state.and_then(|s| s.in_use).unwrap_or(false);
        let drbd = res.layer_object.and_then(|l| l.drbd);
        let connections: Vec<(String, bool, String)> = drbd
            .iter()
            .flat_map(|d| &d.connections)
            .map(|(peer, c)| {
                (
                    peer.clone(),
                    c.connected.unwrap_or(false),
                    c.message.clone().unwrap_or_default(),
                )
            })
            .collect();

        for vol in res.volumes {
            let number = vol.volume_number;
            let diskless = res.flags.iter().any(|f| f.contains("DISKLESS"))
                || vol.provider_kind.as_deref() == Some("DISKLESS");
            let drbd_layer_data = vol
                .layer_data_list
                .iter()
                .find(|l| l.layer_type.as_deref() == Some("DRBD"));
            let drbd_layer = drbd.is_some() || drbd_layer_data.is_some();
            let minor = drbd_layer_data
                .and_then(|l| {
                    l.data
                        .as_ref()?
                        .drbd_volume_definition
                        .as_ref()?
                        .minor_number
                })
                .map(|m| u32::try_from(m).unwrap_or(u32::MAX));
            let (disk_state, replication) = match vol.state {
                Some(s) => (s.disk_state, s.replication_states),
                None => (None, HashMap::new()),
            };
            let mut peers: Vec<PeerLink> = connections
                .iter()
                .map(|(peer, connected, message)| PeerLink {
                    peer: peer.clone(),
                    connected: *connected,
                    message: message.clone(),
                    replication: replication
                        .get(peer)
                        .and_then(|r| r.replication_state.clone()),
                })
                .collect();
            peers.sort_by(|a, b| a.peer.cmp(&b.peer));

            let volume = volumes
                .entry((name.clone(), number))
                .or_insert_with(|| volume_of(&name, number, defs.get(&name)));
            volume.replicas.push(Replica {
                node: node.clone(),
                drbd_layer,
                in_use,
                diskless,
                disk_state: disk_state.unwrap_or_else(|| "Unknown".to_string()),
                minor,
                peers,
                drbd: None,
            });
        }
    }

    let mut volumes: Vec<LinstorVolume> = volumes.into_values().collect();
    for v in &mut volumes {
        v.replicas.sort_by(|a, b| a.node.cmp(&b.node));
    }
    volumes.sort_by(|a, b| {
        a.pvc
            .is_none()
            .cmp(&b.pvc.is_none())
            .then_with(|| a.pvc.cmp(&b.pvc))
            .then_with(|| a.resource.cmp(&b.resource))
            .then_with(|| a.volume_number.cmp(&b.volume_number))
    });
    volumes
}

/// How many kernel reads run at once across all nodes
const OBSERVE_CONCURRENCY: usize = 24;
/// Upper bound for one Talos file read
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One file to read from a node for a replica
enum Probe {
    Size,
    PeerDevice(String),
    PeerConnection(String),
}

/// Fill in `Replica::drbd` from each node's kernel, for replicas on nodes in
/// `node_ips` (node name -> Talos address). Replicas on other nodes, or whose
/// reads all fail, stay `None`.
pub async fn observe_drbd(
    talos: &TalosClient,
    node_ips: &HashMap<String, String>,
    volumes: &mut [LinstorVolume],
) {
    let mut probes = Vec::new();
    for (vi, volume) in volumes.iter().enumerate() {
        for (ri, replica) in volume.replicas.iter().enumerate() {
            let Some(ip) = node_ips.get(&replica.node) else {
                continue;
            };
            let client = talos.with_node(ip);
            let debugfs = format!("/sys/kernel/debug/drbd/resources/{}", volume.resource);
            if let Some(minor) = replica.minor {
                probes.push((
                    (vi, ri),
                    Probe::Size,
                    client.clone(),
                    format!("/sys/block/drbd{minor}/size"),
                ));
            }
            for peer in &replica.peers {
                let conn = format!("{debugfs}/connections/{}", peer.peer);
                probes.push((
                    (vi, ri),
                    Probe::PeerDevice(peer.peer.clone()),
                    client.clone(),
                    format!("{conn}/{}/proc_drbd", volume.volume_number),
                ));
                probes.push((
                    (vi, ri),
                    Probe::PeerConnection(peer.peer.clone()),
                    client.clone(),
                    format!("{conn}/debug"),
                ));
            }
        }
    }

    let reads = futures::stream::iter(probes)
        .map(|(at, probe, client, path)| async move {
            let content = tokio::time::timeout(READ_TIMEOUT, client.read_file(&path))
                .await
                .ok()
                .and_then(std::result::Result::ok);
            (at, probe, content)
        })
        .buffer_unordered(OBSERVE_CONCURRENCY)
        .collect::<Vec<_>>();
    let results = reads.await;

    for ((vi, ri), probe, content) in results {
        let Some(content) = content else {
            continue;
        };
        let Some(replica) = volumes.get_mut(vi).and_then(|v| v.replicas.get_mut(ri)) else {
            continue;
        };
        let observed = replica.drbd.get_or_insert_with(DrbdObserved::default);
        match probe {
            // 512-byte sectors
            Probe::Size => observed.size_kib = content.trim().parse::<u64>().ok().map(|s| s / 2),
            Probe::PeerDevice(peer) => {
                let parsed = parse_proc_drbd(&content);
                // Per device; every peer's file reports the same
                if let Some(suspended) = parsed.suspended {
                    observed.io_suspended = Some(observed.io_suspended == Some(true) || suspended);
                }
                let entry = observed.peers.entry(peer).or_default();
                entry.cs = parsed.cs;
                entry.oos_kib = parsed.oos_kib;
                entry.sync = parse_sync_progress(&content);
            }
            Probe::PeerConnection(peer) => {
                observed.peers.entry(peer).or_default().ap_in_flight_kib =
                    parse_ap_in_flight(&content);
            }
        }
    }

    // Why a link is StandAlone is only in the kernel log; read it only on
    // the nodes that have one
    let mut nodes: Vec<&String> = volumes
        .iter()
        .flat_map(|v| &v.replicas)
        .filter(|r| r.peers.iter().any(|p| standalone(r, &p.peer)))
        .map(|r| &r.node)
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    let reads = nodes.into_iter().filter_map(|node| {
        let client = talos.with_node(node_ips.get(node)?);
        Some(async move {
            let log = tokio::time::timeout(DMESG_TIMEOUT, client.dmesg(false, false))
                .await
                .ok()?
                .ok()?;
            Some((node.clone(), split_brain_links(&log)))
        })
    });
    let split_brains: HashMap<String, Vec<(String, String)>> = futures::future::join_all(reads)
        .await
        .into_iter()
        .flatten()
        .collect();
    for volume in volumes.iter_mut() {
        for replica in &mut volume.replicas {
            let Some(links) = split_brains.get(&replica.node) else {
                continue;
            };
            let Some(observed) = &mut replica.drbd else {
                continue;
            };
            for (peer, seen) in &mut observed.peers {
                seen.split_brain = links
                    .iter()
                    .any(|(res, p)| res == &volume.resource && p == peer);
            }
        }
    }
}

/// Upper bound for reading a node's kernel log
const DMESG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// LINSTOR's replication state of the replica's link to `peer`, e.g.
/// `Established`, `SyncSource`; `None` when LINSTOR reports none
fn linstor_replication<'a>(replica: &'a Replica, peer: &str) -> Option<&'a str> {
    replica
        .peers
        .iter()
        .find(|p| p.peer == peer)
        .and_then(|p| p.replication.as_deref())
}

/// The link to `peer` is Established as far as issues go: the kernel says
/// so and LINSTOR doesn't say otherwise.
///
/// The kernel is the authority (LINSTOR lags it), so LINSTOR reporting no
/// replication state doesn't make out-of-sync bits on a kernel-Established
/// link any less stale. LINSTOR reporting a resync does: the bits are then
/// shown as resyncing, a warning, rather than an error that might be wrong.
fn established_per_kernel(replica: &Replica, peer: &str, seen: &PeerObserved) -> bool {
    seen.cs.as_deref() == Some("Established")
        && linstor_replication(replica, peer).is_none_or(|s| s == "Established")
}

/// The link to `peer` is Established by both the kernel and LINSTOR.
///
/// Stricter than [`established_per_kernel`] on purpose: this gates the
/// `drbdadm` commands suggested for stale bits, which are offered only for
/// provably safe cases, so a missing LINSTOR state counts as not proven.
fn established_confirmed(replica: &Replica, peer: &str, seen: &PeerObserved) -> bool {
    seen.cs.as_deref() == Some("Established")
        && linstor_replication(replica, peer) == Some("Established")
}

/// The replica's link to `peer` is StandAlone (DRBD stopped trying to
/// connect), per LINSTOR or the kernel
fn standalone(replica: &Replica, peer: &str) -> bool {
    let kernel = replica.observed(peer).and_then(|p| p.cs.as_deref());
    kernel == Some("StandAlone")
        || replica
            .peers
            .iter()
            .any(|p| p.peer == peer && !p.connected && p.message == "StandAlone")
}

/// `(resource, peer)` links a node's kernel log shows as StandAlone because
/// of an unresolved split-brain, from Talos kernel log lines such as
/// `kern: err: [ts]: drbd pvc-1 bc2: Split-Brain detected but unresolved,
/// dropping connection!` and `… drbd pvc-1 bc2: conn( Disconnecting ->
/// StandAlone ) [disconnected]`.
///
/// Only the link's latest StandAlone counts, and only when no other
/// connection change came between it and the split-brain: a split-brain
/// resolved by a later reconnect, or a StandAlone from a later manual
/// `disconnect`, is not reported.
// The only slice is past the ASCII prefix "drbd", just checked
#[allow(clippy::string_slice)]
fn split_brain_links(log: &str) -> Vec<(String, String)> {
    // Per link: split-brain seen since its last connection attempt, and
    // whether its latest StandAlone followed one
    let mut pending: HashMap<(String, String), bool> = HashMap::new();
    let mut result: HashMap<(String, String), bool> = HashMap::new();
    for line in log.lines() {
        let Some(rest) = line.split_once("]: drbd ").map(|(_, r)| r) else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let Some(resource) = words.next() else {
            continue;
        };
        if resource.ends_with(':') {
            // Resource-level line, no peer
            continue;
        }
        let resource = resource.split('/').next().unwrap_or(resource);
        // The peer is the first word ending in ':', after the minor
        // (`drbd1002`) on device lines; the minor alone means no peer
        let Some(peer) = words.find_map(|w| w.strip_suffix(':')) else {
            continue;
        };
        if peer.starts_with("drbd") && peer[4..].bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some((_, text)) = rest.split_once(&format!("{peer}: ")) else {
            continue;
        };
        let link = (resource.to_string(), peer.to_string());
        if text.starts_with("Split-Brain detected") && text.contains("unresolved") {
            pending.insert(link, true);
        } else if let Some(change) = text.strip_prefix("conn( ") {
            let Some((from, to)) = change.split_once(" -> ") else {
                continue;
            };
            let to = to.split_whitespace().next().unwrap_or_default();
            if to == "StandAlone" {
                let after_split_brain = pending.remove(&link).unwrap_or(false);
                result.insert(link, after_split_brain);
            } else if from == "StandAlone" || matches!(to, "Unconnected" | "Connecting") {
                // Left StandAlone, or tried connecting again
                pending.remove(&link);
                result.remove(&link);
            }
        }
    }
    let mut links: Vec<_> = result
        .into_iter()
        .filter(|(_, split_brain)| *split_brain)
        .map(|(link, _)| link)
        .collect();
    links.sort();
    links
}

/// A link reported broken by one or both ends
struct BrokenLink<'a> {
    /// The pair, sorted
    a: &'a str,
    b: &'a str,
    /// Each reporting end and its state, StandAlone ends first
    ends: Vec<(&'a str, &'a str)>,
    split_brain: bool,
}

impl BrokenLink<'_> {
    fn standalone(&self) -> bool {
        self.ends.iter().any(|(_, state)| *state == "StandAlone")
    }

    fn text(&self) -> String {
        let (a, b) = (self.a, self.b);
        let first = self.ends.first().map(|(_, s)| *s);
        let states = if let Some(first) = first
            && self.ends.iter().all(|(_, s)| *s == first)
        {
            first.to_string()
        } else {
            self.ends
                .iter()
                .map(|(node, state)| format!("{state} on {node}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if self.split_brain {
            format!("{a} ↔ {b}: split-brain, {states}: DRBD won't reconnect by itself")
        } else if self.standalone() {
            format!("{a} ↔ {b}: {states}: DRBD won't reconnect by itself")
        } else {
            format!("{a} ↔ {b}: {states}")
        }
    }
}

/// Links not connected, once per pair, sorted
fn broken_links(volume: &LinstorVolume) -> Vec<BrokenLink<'_>> {
    let mut links: Vec<BrokenLink> = Vec::new();
    for r in volume.replicas.iter().filter(|r| r.drbd_layer) {
        for p in r.peers.iter().filter(|p| !p.connected) {
            let (a, b) = if r.node < p.peer {
                (r.node.as_str(), p.peer.as_str())
            } else {
                (p.peer.as_str(), r.node.as_str())
            };
            let state = if standalone(r, &p.peer) {
                "StandAlone"
            } else {
                p.message.as_str()
            };
            let split_brain = r.observed(&p.peer).is_some_and(|seen| seen.split_brain);
            let Some(link) = (match links.iter().position(|l| l.a == a && l.b == b) {
                Some(i) => links.get_mut(i),
                None => {
                    links.push(BrokenLink {
                        a,
                        b,
                        ends: Vec::new(),
                        split_brain: false,
                    });
                    links.last_mut()
                }
            }) else {
                continue;
            };
            link.ends.push((r.node.as_str(), state));
            link.split_brain |= split_brain;
        }
    }
    for link in &mut links {
        link.ends
            .sort_by_key(|(node, state)| (*state != "StandAlone", *node));
    }
    links.sort_by(|x, y| (x.a, x.b).cmp(&(y.a, y.b)));
    links
}

/// Mounted filesystems per node name, one Mounts call per node, each
/// bounded. A node whose call fails is left out.
pub async fn fetch_mounts(
    talos: &TalosClient,
    node_ips: &HashMap<String, String>,
    nodes: &[String],
) -> HashMap<String, Vec<MountUsage>> {
    let calls = nodes.iter().filter_map(|node| {
        let client = talos.with_node(node_ips.get(node)?);
        Some(async move {
            let mounts = tokio::time::timeout(READ_TIMEOUT, client.mounts())
                .await
                .ok()?
                .ok()?;
            Some((
                node.clone(),
                mounts.into_iter().flat_map(|m| m.mounts).collect(),
            ))
        })
    });
    futures::future::join_all(calls)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Nodes whose mounts the LINSTOR volumes need: where they are in use
pub fn linstor_mount_nodes(volumes: &[LinstorVolume]) -> Vec<String> {
    volumes
        .iter()
        .flat_map(|v| &v.replicas)
        .filter(|r| r.in_use && r.drbd_layer)
        .map(|r| r.node.clone())
        .collect()
}

/// Set the filesystem size and usage of each in-use replica from the mount
/// of its DRBD device. A device mounted several times (pod mount, subPaths)
/// has the same size everywhere.
pub fn apply_mounts(volumes: &mut [LinstorVolume], mounts: &HashMap<String, Vec<MountUsage>>) {
    for volume in volumes {
        for replica in volume.replicas.iter_mut().filter(|r| r.in_use) {
            let (Some(minor), Some(node_mounts)) = (replica.minor, mounts.get(&replica.node))
            else {
                continue;
            };
            let device = format!("/dev/drbd{minor}");
            if let Some(m) = node_mounts.iter().find(|m| m.filesystem == device) {
                let observed = replica.drbd.get_or_insert_with(DrbdObserved::default);
                observed.fs_size_kib = Some(m.size / 1024);
                observed.fs_used_kib = Some(m.size.saturating_sub(m.available) / 1024);
            }
        }
    }
}

/// How serious a filesystem fill level (percent) is, `None` when fine.
/// The volumes table colors its Used column by this too.
pub fn usage_severity(pct: f64) -> Option<Severity> {
    if pct >= USAGE_ERROR_PCT {
        Some(Severity::Error)
    } else if pct >= USAGE_WARNING_PCT {
        Some(Severity::Warning)
    } else {
        None
    }
}

/// An issue for a filesystem filling up on `node`
pub fn usage_issue(node: &str, used_kib: u64, size_kib: u64) -> Option<Issue> {
    if size_kib == 0 {
        return None;
    }
    let pct = used_kib as f64 / size_kib as f64 * 100.0;
    let severity = usage_severity(pct)?;
    Some(Issue {
        severity,
        text: format!(
            "{node}: filesystem {pct:.0}% full ({} of {})",
            format_kib(used_kib),
            format_kib(size_kib)
        ),
    })
}

/// What a debugfs `proc_drbd` file says about a peer device
#[derive(Debug, PartialEq, Default)]
struct ProcDrbd {
    cs: Option<String>,
    oos_kib: Option<u64>,
    /// First of the state flags: `s` suspended, `r` running
    suspended: Option<bool>,
}

/// `cs:`, `oos:` and the suspended flag from a debugfs `proc_drbd` file, e.g.
/// `1015: cs:Established ro:Secondary/Secondary ds:UpToDate/UpToDate C r-----`
/// followed by `ns:0 nr:0 … oos:0`
fn parse_proc_drbd(content: &str) -> ProcDrbd {
    let field = |key: &str| {
        content
            .split_whitespace()
            .find_map(|t| t.strip_prefix(key))
            .map(str::to_string)
    };
    // The flags are the last word of the first line, six characters
    let suspended = content
        .lines()
        .next()
        .filter(|l| l.contains("cs:"))
        .and_then(|l| l.split_whitespace().last())
        .filter(|f| f.len() == 6)
        .and_then(|f| match f.as_bytes().first() {
            Some(b's') => Some(true),
            Some(b'r') => Some(false),
            _ => None,
        });
    ProcDrbd {
        cs: field("cs:"),
        oos_kib: field("oos:").and_then(|v| v.parse().ok()),
        suspended,
    }
}

/// `ap_in_flight` in KiB from a debugfs connection `debug` file
/// (`ap_in_flight: 0 KiB (0 sectors)`)
fn parse_ap_in_flight(content: &str) -> Option<u64> {
    content.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("ap_in_flight:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// In-flight writes beyond this are a wrapped-around (underflowed) counter
const AP_IN_FLIGHT_IMPLAUSIBLE_KIB: u64 = 1 << 40;

/// Problems of a volume, worst first
pub fn volume_issues(volume: &LinstorVolume) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut push = |severity, text: String| {
        if !issues.iter().any(|i: &Issue| i.text == text) {
            issues.push(Issue { severity, text });
        }
    };

    let syncing = |r: &Replica| {
        r.peers.iter().any(|p| {
            p.replication
                .as_deref()
                .is_some_and(|s| s.starts_with("Sync") || s.starts_with("PausedSync"))
        })
    };

    if let Some(wanted) = volume.place_count {
        let have = volume.diskful_replicas();
        if have < wanted as usize {
            push(
                Severity::Error,
                format!("only {have} of {wanted} replicas hold data"),
            );
        }
    }
    if let Some(node) = volume.replicas.iter().find(|r| r.in_use).map(|r| &r.node)
        && let Some((used, size)) = volume.usage_kib()
        && let Some(issue) = usage_issue(node, used, size)
    {
        push(issue.severity, issue.text);
    }

    // Both ends report a broken link; name the pair once
    for link in broken_links(volume) {
        push(Severity::Error, link.text());
    }

    for r in &volume.replicas {
        if !r.drbd_layer {
            continue;
        }
        if !r.diskless && r.disk_state != "UpToDate" {
            match r.disk_state.as_str() {
                "Inconsistent" if syncing(r) => push(
                    Severity::Warning,
                    format!("{}: Inconsistent (syncing)", r.node),
                ),
                // Passed through while attaching or connecting
                "Attaching" | "Negotiating" | "Detaching" => push(
                    Severity::Warning,
                    format!("{}: disk {}", r.node, r.disk_state),
                ),
                // Not reported (satellite not answering): unknown, not broken
                "" | "Unknown" | "DUnknown" => {
                    push(Severity::Warning, format!("{}: disk state unknown", r.node));
                }
                _ => push(
                    Severity::Error,
                    format!("{}: disk {}", r.node, r.disk_state),
                ),
            }
        }
        for p in r.peers.iter().filter(|p| p.connected) {
            if let Some(rep) = p.replication.as_deref()
                && rep != "Established"
            {
                push(
                    Severity::Warning,
                    format!("{} → {}: {}", r.node, p.peer, rep),
                );
            }
        }

        let Some(observed) = &r.drbd else {
            continue;
        };
        if observed.io_suspended == Some(true) {
            push(
                Severity::Error,
                format!(
                    "{}: I/O suspended (quorum lost, fencing or by hand): writes hang",
                    r.node
                ),
            );
        }
        if let (Some(actual), Some(expected)) = (observed.size_kib, volume.size_kib)
            && actual < expected
        {
            push(
                Severity::Error,
                format!(
                    "{}: DRBD device {} < LINSTOR {}",
                    r.node,
                    format_kib(actual),
                    format_kib(expected)
                ),
            );
        }
        // A resize must also grow the filesystem (the 2026-10-17 incident's
        // XFS stayed at 5.9 GiB on an 11 GiB device); briefly smaller while
        // a resize runs, hence a warning
        if let (Some(fs), Some(device)) = (observed.fs_size_kib, observed.size_kib)
            && fs + FS_OVERHEAD_FIXED_KIB + device * FS_OVERHEAD_PERCENT / 100 < device
        {
            push(
                Severity::Warning,
                format!(
                    "{}: filesystem {} on a {} DRBD device: not grown after a resize",
                    r.node,
                    format_kib(fs),
                    format_kib(device)
                ),
            );
        }
        let mut peers: Vec<_> = observed.peers.iter().collect();
        peers.sort_by(|a, b| a.0.cmp(b.0));
        for (peer, seen) in peers {
            if let Some(oos) = seen.out_of_sync_kib() {
                let established = established_per_kernel(r, peer, seen);
                let peer_is_primary = volume.replicas.iter().any(|p| &p.node == peer && p.in_use);
                if !established {
                    let progress = seen
                        .sync
                        .as_ref()
                        .map_or_else(|| "resyncing".to_string(), SyncProgress::describe);
                    push(
                        Severity::Warning,
                        format!(
                            "{} → {peer}: {} out of sync ({progress})",
                            r.node,
                            format_kib(oos)
                        ),
                    );
                } else if !r.in_use && peer_is_primary {
                    // LINBIT/drbd#153: with stale bits toward the Primary, a
                    // short loss of all links can make the Primary the
                    // SyncTarget, which forces it Secondary and fails its I/O
                    push(
                        Severity::Error,
                        format!(
                            "{} holds {} out-of-sync bits toward Primary {peer}: a link loss \
                             could make the Primary a SyncTarget (LINBIT/drbd#153)",
                            r.node,
                            format_kib(oos)
                        ),
                    );
                } else {
                    push(
                        Severity::Error,
                        format!("{} → {peer}: {} out of sync", r.node, format_kib(oos)),
                    );
                }
            }
            if seen
                .ap_in_flight_kib
                .is_some_and(|ap| ap >= AP_IN_FLIGHT_IMPLAUSIBLE_KIB)
            {
                push(
                    Severity::Error,
                    format!("{} → {peer}: in-flight counter underflowed", r.node),
                );
            }
        }
    }

    issues.sort_by_key(|i| i.severity);
    issues
}

/// Suggested manual fixes for stale out-of-sync bits on established links
/// and for StandAlone links (see [`standalone_remedies`]), with each command
/// run in the satellite pod of the node it must run on.
///
/// - Bits between the Primary and a peer: from the Primary, optionally verify
///   the peer (read-only), then `invalidate-remote` it, so the peer copies
///   the whole volume from the Primary. Never the other way round: making the
///   Primary the SyncTarget is the LINBIT/drbd#153 failure.
/// - Bits between two Secondaries: reconnect just that link from one of
///   them, never touching the Primary (as home-operations/miroir#500 does).
///   A reconnect with equal UUIDs drops the bits without copying data.
///
/// Nothing is suggested for stale bits while a resync is running or without
/// a Primary. `kubectl` is the command pinned to the cluster's context.
pub fn remediation(
    volume: &LinstorVolume,
    satellites: &HashMap<String, SatellitePod>,
    kubectl: &str,
) -> Vec<Remedy> {
    let mut remedies = stale_bit_remedies(volume, satellites, kubectl);
    remedies.extend(standalone_remedies(volume, satellites, kubectl));
    remedies
}

/// `res:peer`, or `res:peer/vol` past the first volume, for `drbdadm`
fn drbdadm_target(volume: &LinstorVolume, peer: &str) -> String {
    if volume.volume_number == 0 {
        format!("{}:{peer}", volume.resource)
    } else {
        format!("{}:{peer}/{}", volume.resource, volume.volume_number)
    }
}

/// `drbdadm` run in the satellite pod of `node`
fn drbdadm_on(
    satellites: &HashMap<String, SatellitePod>,
    kubectl: &str,
    node: &str,
    drbdadm: &str,
) -> String {
    match satellites.get(node) {
        Some(pod) => format!(
            "{kubectl} -n {} exec {} -c {SATELLITE_CONTAINER} -- drbdadm {drbdadm}",
            pod.namespace, pod.name
        ),
        None => format!(
            "{kubectl} -n piraeus-datastore exec <linstor-satellite pod on {node}> \
             -c {SATELLITE_CONTAINER} -- drbdadm {drbdadm}"
        ),
    }
}

/// DRBD's guide to resolving a split-brain by hand
const SPLIT_BRAIN_GUIDE: &str =
    "https://linbit.com/drbd-user-guide/drbd-guide-9_0-en/#s-resolve-split-brain";

/// Fixes for StandAlone links, which DRBD never reconnects by itself.
///
/// Commands only when it can't lose data: both ends Secondary, each
/// UpToDate (or diskless), and a Primary Established to both with nothing
/// out of sync either way. Both then hold the Primary's data, so:
/// 1. `connect` the StandAlone ends: with the same current UUID the link
///    comes back without a resync (DRBD's `no-sync by rule=reconnected`)
/// 2. if it drops back to StandAlone (split-brain on their histories),
///    discard one end's copy: it resyncs the differing blocks from the other
///
/// Otherwise (the Primary on the link, no Primary, an end behind) only a
/// note: which side's writes to keep is the operator's call.
fn standalone_remedies(
    volume: &LinstorVolume,
    satellites: &HashMap<String, SatellitePod>,
    kubectl: &str,
) -> Vec<Remedy> {
    let exec = |node: &str, drbdadm: &str| drbdadm_on(satellites, kubectl, node, drbdadm);
    let target = |peer: &str| drbdadm_target(volume, peer);
    let replica = |node: &str| volume.replicas.iter().find(|r| r.node == node);
    let primary = volume.replicas.iter().find(|r| r.in_use);
    let oos = |from: &Replica, to: &str| from.observed(to).and_then(|p| p.oos_kib).unwrap_or(0);
    // `node` holds exactly the Primary's data
    let in_sync_with = |primary: &Replica, node: &str| {
        let Some(r) = replica(node) else {
            return false;
        };
        let link = primary.peers.iter().any(|p| {
            p.peer == node && p.connected && p.replication.as_deref() == Some("Established")
        });
        (r.diskless || r.disk_state == "UpToDate")
            && link
            && oos(primary, node) == 0
            && oos(r, &primary.node) == 0
    };

    let mut remedies = Vec::new();
    for link in broken_links(volume)
        .into_iter()
        .filter(BrokenLink::standalone)
    {
        let (a, b) = (link.a, link.b);
        let reason = match primary {
            None => Some("no Primary to compare them with".to_string()),
            Some(p) if p.node == a || p.node == b => Some(format!(
                "Primary {} is one end, and its writes since the split may be the ones to keep",
                p.node
            )),
            Some(p) if p.diskless || p.disk_state != "UpToDate" => {
                Some(format!("Primary {} is not UpToDate", p.node))
            }
            Some(p) => [a, b]
                .into_iter()
                .find(|n| !in_sync_with(p, n))
                .map(|n| format!("{n} is not UpToDate and in sync with Primary {}", p.node)),
        };
        if let Some(reason) = reason {
            remedies.push(Remedy {
                summary: format!(
                    "Reconnect {a} ↔ {b} by hand: {reason}, so which side's data to discard \
                     is your call ({SPLIT_BRAIN_GUIDE})"
                ),
                commands: Vec::new(),
            });
            continue;
        }
        let primary = primary.map_or("", |p| p.node.as_str());
        let standalone_ends: Vec<&str> = link
            .ends
            .iter()
            .filter(|(_, state)| *state == "StandAlone")
            .map(|(node, _)| *node)
            .collect();
        let other = |node: &str| if node == a { b } else { a };
        remedies.push(Remedy {
            summary: format!(
                "Reconnect Secondaries {a} ↔ {b}: both match Primary {primary}, so this \
                 resyncs nothing"
            ),
            commands: standalone_ends
                .iter()
                .map(|n| exec(n, &format!("connect {}", target(other(n)))))
                .collect(),
        });
        let diskful = [a, b]
            .into_iter()
            .all(|n| replica(n).is_some_and(|r| !r.diskless));
        if diskful && let Some(victim) = standalone_ends.first() {
            let keep = other(victim);
            remedies.push(Remedy {
                summary: format!(
                    "Only if it drops back to StandAlone (split-brain): discard {victim}'s \
                     copy, it resyncs the differing blocks from {keep}"
                ),
                commands: vec![
                    exec(
                        victim,
                        &format!("connect --discard-my-data {}", target(keep)),
                    ),
                    exec(keep, &format!("connect {}", target(victim))),
                ],
            });
        }
    }
    remedies
}

/// Fixes for stale out-of-sync bits on established links
fn stale_bit_remedies(
    volume: &LinstorVolume,
    satellites: &HashMap<String, SatellitePod>,
    kubectl: &str,
) -> Vec<Remedy> {
    let Some(primary) = volume.replicas.iter().find(|r| r.in_use) else {
        return Vec::new();
    };
    let target = |peer: &str| drbdadm_target(volume, peer);
    let exec = |node: &str, drbdadm: &str| drbdadm_on(satellites, kubectl, node, drbdadm);

    // Established links with stale bits: (holder, peer)
    let mut stale: Vec<(&str, &str)> = Vec::new();
    for r in volume.replicas.iter().filter(|r| r.drbd_layer) {
        let Some(observed) = &r.drbd else {
            continue;
        };
        for (peer, seen) in &observed.peers {
            if established_confirmed(r, peer, seen) && seen.out_of_sync_kib().is_some() {
                stale.push((r.node.as_str(), peer.as_str()));
            }
        }
    }

    let mut via_primary: Vec<&str> = Vec::new();
    let mut between_secondaries: Vec<(&str, &str)> = Vec::new();
    for (holder, peer) in stale {
        if holder == primary.node {
            via_primary.push(peer);
        } else if peer == primary.node {
            via_primary.push(holder);
        } else if holder < peer {
            between_secondaries.push((holder, peer));
        } else {
            between_secondaries.push((peer, holder));
        }
    }
    via_primary.sort_unstable();
    via_primary.dedup();
    between_secondaries.sort_unstable();
    between_secondaries.dedup();

    let size = volume
        .size_kib
        .map(format_kib)
        .unwrap_or_else(|| "the volume".to_string());
    let mut remedies = Vec::new();
    for peer in via_primary {
        remedies.push(Remedy {
            summary: format!(
                "Resync {peer} from Primary {}: full copy of {size}; {peer} is Inconsistent \
                 until it finishes. Verify is optional and read-only.",
                primary.node
            ),
            commands: vec![
                exec(&primary.node, &format!("verify {}", target(peer))),
                exec(
                    &primary.node,
                    &format!("invalidate-remote {}", target(peer)),
                ),
            ],
        });
    }
    for (a, b) in between_secondaries {
        remedies.push(Remedy {
            summary: format!(
                "Reconnect Secondaries {a} ↔ {b} only (not the Primary): clears the bits \
                 without copying data"
            ),
            commands: vec![
                exec(a, &format!("disconnect {}", target(b))),
                exec(a, &format!("connect {}", target(b))),
            ],
        });
    }
    remedies
}

/// KiB as a human size in binary units, e.g. `11.0 GiB`, `924 KiB`.
///
/// Not `talos9s_core::format_bytes`: that labels binary sizes `KB`/`GB`,
/// while DRBD, LINSTOR and the remedies here speak KiB/GiB, and storage
/// sizes are compared against those tools' output.
pub fn format_kib(kib: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = kib as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{kib} KiB")
    } else {
        format!("{value:.1} {}", UNITS.get(unit).unwrap_or(&"PiB"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::serde_json::json;

    fn resource(node: &str, in_use: bool, disk: &str, peers: &[(&str, bool, &str)]) -> Value {
        let connections: serde_json::Map<String, Value> = peers
            .iter()
            .map(|(p, connected, _)| {
                (
                    p.to_string(),
                    json!({"connected": connected, "message": if *connected {"Connected"} else {"Connecting"}}),
                )
            })
            .collect();
        let replication: serde_json::Map<String, Value> = peers
            .iter()
            .map(|(p, _, rep)| (p.to_string(), json!({"replication_state": rep})))
            .collect();
        json!({
            "name": "pvc-1", "node_name": node,
            "state": {"in_use": in_use},
            "layer_object": {"drbd": {"connections": connections}},
            "volumes": [{
                "volume_number": 0, "provider_kind": "ZFS_THIN",
                "state": {"disk_state": disk, "replication_states": replication},
                "layer_data_list": [{"type": "DRBD",
                    "data": {"drbd_volume_definition": {"minor_number": 1002}}}]
            }]
        })
    }

    use k8s_openapi::serde_json;

    fn definitions() -> Value {
        json!([{
            "name": "pvc-1",
            "props": {"Aux/csi.storage.k8s.io/pvc/namespace": "monitoring",
                      "Aux/csi.storage.k8s.io/pvc/name": "vmsingle"},
            "volume_definitions": [{"volume_number": 0, "size_kib": 11 * 1024 * 1024}]
        }])
    }

    #[test]
    fn groups_replicas_into_volumes() {
        let resources = json!([
            resource("bc2", false, "UpToDate", &[("bc1", true, "Established")]),
            resource("bc1", true, "UpToDate", &[("bc2", true, "Established")]),
        ]);
        let volumes = volumes_from(&resources, &definitions());
        assert_eq!(volumes.len(), 1);
        let v = &volumes[0];
        assert_eq!(v.pvc.as_deref(), Some("monitoring/vmsingle"));
        assert_eq!(v.size_kib, Some(11 * 1024 * 1024));
        assert_eq!(v.replicas[0].node, "bc1");
        assert!(v.replicas[0].in_use);
        assert_eq!(v.replicas[0].minor, Some(1002));
        assert!(volume_issues(v).is_empty());
    }

    /// A single local volume without DRBD (STORAGE layer only) reports no
    /// disk state; that is not a problem
    #[test]
    fn storage_only_volume_has_no_issues() {
        let resources = json!([{
            "name": "pvc-1", "node_name": "khn1",
            "layer_object": {"type": "STORAGE"},
            "volumes": [{"volume_number": 0, "provider_kind": "FILE_THIN",
                         "layer_data_list": [{"type": "STORAGE", "data": {}}]}]
        }]);
        let volume = volumes_from(&resources, &definitions()).remove(0);
        assert!(!volume.replicas[0].drbd_layer);
        assert!(volume_issues(&volume).is_empty());
    }

    #[test]
    fn transitional_and_unreported_disk_states_are_not_errors() {
        for (disk, text) in [
            ("Attaching", "bc2: disk Attaching"),
            ("Negotiating", "bc2: disk Negotiating"),
            ("DUnknown", "bc2: disk state unknown"),
            ("Unknown", "bc2: disk state unknown"),
        ] {
            let resources = json!([
                resource("bc1", true, "UpToDate", &[("bc2", true, "Established")]),
                resource("bc2", false, disk, &[("bc1", true, "Established")]),
            ]);
            let issues = volume_issues(&volumes_from(&resources, &definitions())[0]);
            assert_eq!(
                issues,
                [Issue {
                    severity: Severity::Warning,
                    text: text.to_string()
                }],
                "{disk}"
            );
        }

        // No disk_state at all is unknown too
        let mut missing = resource("bc2", false, "UpToDate", &[("bc1", true, "Established")]);
        missing["volumes"][0]["state"]
            .as_object_mut()
            .unwrap()
            .remove("disk_state");
        let resources = json!([
            resource("bc1", true, "UpToDate", &[("bc2", true, "Established")]),
            missing,
        ]);
        let issues = volume_issues(&volumes_from(&resources, &definitions())[0]);
        assert!(
            issues.iter().all(|i| i.severity == Severity::Warning),
            "{issues:?}"
        );
    }

    #[test]
    fn paused_sync_counts_as_syncing() {
        let resources = json!([
            resource("bc1", true, "UpToDate", &[("bc3", true, "PausedSyncS")]),
            resource(
                "bc3",
                false,
                "Inconsistent",
                &[("bc1", true, "PausedSyncT")]
            ),
        ]);
        let issues = volume_issues(&volumes_from(&resources, &definitions())[0]);
        assert!(
            issues.iter().all(|i| i.severity == Severity::Warning),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.text == "bc3: Inconsistent (syncing)")
        );
    }

    #[test]
    fn odd_resources_degrade_per_item() {
        let resources = json!([
            {"name": "pvc-1"},
            "garbage",
            {
                "name": "pvc-1", "node_name": "bc1",
                "state": "?",
                "flags": [1, "DRBD_DISKLESS"],
                "layer_object": {"drbd": null},
                "volumes": [
                    {"provider_kind": "ZFS_THIN"},
                    {"volume_number": 0,
                     "layer_data_list": [7, {"type": "DRBD", "data": "?"}]}
                ]
            },
            {
                "name": "pvc-1", "node_name": "bc2",
                "layer_object": {"drbd": {"connections": {"bc1": "?", "bc3": {"connected": true}}}},
                "volumes": [{"volume_number": 0, "state": {
                    "disk_state": 9,
                    "replication_states": {"bc1": {"replication_state": "Established"}, "bc3": 1}
                }}]
            }
        ]);
        let definitions = json!([
            {"name": "pvc-1", "props": "?", "flags": ["DELETE", 2],
             "resource_group_name": 3,
             "volume_definitions": [{"volume_number": 0}, {"volume_number": 0, "size_kib": 8}]}
        ]);
        let volumes = volumes_from(&resources, &definitions);
        assert_eq!(volumes.len(), 1);
        let v = &volumes[0];
        assert_eq!(
            (v.pvc.as_deref(), v.resource_group.as_deref()),
            (None, None)
        );
        assert!(v.deleting);
        assert_eq!(v.size_kib, Some(8));
        assert_eq!(v.replicas.len(), 2);

        // `drbd: null` still means a DRBD layer; a bad layer entry no minor
        let bc1 = &v.replicas[0];
        assert!(bc1.drbd_layer && bc1.diskless && !bc1.in_use);
        assert_eq!((bc1.disk_state.as_str(), bc1.minor), ("Unknown", None));
        assert!(bc1.peers.is_empty());

        // A malformed connection is kept, as not connected
        let bc2 = &v.replicas[1];
        assert_eq!(bc2.disk_state, "Unknown");
        assert_eq!(
            bc2.peers,
            [
                PeerLink {
                    peer: "bc1".to_string(),
                    connected: false,
                    message: String::new(),
                    replication: Some("Established".to_string()),
                },
                PeerLink {
                    peer: "bc3".to_string(),
                    connected: true,
                    message: String::new(),
                    replication: None,
                },
            ]
        );

        let mut volumes = volumes;
        volumes[0].resource_group = Some("rg".to_string());
        let groups = json!([
            {"name": "rg", "select_filter": {}},
            {"name": "rg", "select_filter": {"place_count": 3}},
        ]);
        apply_place_counts(&mut volumes, &groups);
        assert_eq!(volumes[0].place_count, Some(3));
        apply_place_counts(&mut volumes, &json!([{"name": "rg"}]));
        assert_eq!(volumes[0].place_count, None);
    }

    #[test]
    fn linstor_states_become_issues() {
        let resources = json!([
            resource(
                "bc1",
                true,
                "UpToDate",
                &[("bc2", false, "Off"), ("bc3", true, "SyncSource")]
            ),
            resource("bc2", false, "Outdated", &[("bc1", false, "Off")]),
            resource("bc3", false, "Inconsistent", &[("bc1", true, "SyncTarget")]),
        ]);
        let issues = volume_issues(&volumes_from(&resources, &definitions())[0]);
        let texts: Vec<&str> = issues.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.contains(&"bc1 ↔ bc2: Connecting"));
        assert!(texts.contains(&"bc2: disk Outdated"));
        assert!(texts.contains(&"bc3: Inconsistent (syncing)"));
        // The broken link is reported once, though both ends see it
        assert_eq!(texts.iter().filter(|t| t.contains("↔")).count(), 1);
        assert_eq!(issues[0].severity, Severity::Error);
    }

    /// The 2026-10-17 incident: LINSTOR all UpToDate, DRBD on the Primary
    /// still at the old size
    #[test]
    fn drbd_smaller_than_linstor_is_an_error() {
        let resources = json!([resource(
            "bc1",
            true,
            "UpToDate",
            &[("bc2", true, "Established")]
        )]);
        let mut volume = volumes_from(&resources, &definitions()).remove(0);
        assert!(volume_issues(&volume).is_empty());
        volume.replicas[0].drbd = Some(DrbdObserved {
            fs_size_kib: None,
            fs_used_kib: None,
            io_suspended: None,
            size_kib: Some(6 * 1024 * 1024),
            peers: HashMap::new(),
        });
        let issues = volume_issues(&volume);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Error);
        assert!(issues[0].text.starts_with("bc1: DRBD device"));
    }

    #[test]
    fn missing_replica_and_full_filesystem_are_issues() {
        let resources = json!([
            resource("bc1", true, "UpToDate", &[("bc2", true, "Established")]),
            resource("bc2", false, "UpToDate", &[("bc1", true, "Established")]),
        ]);
        let mut volume = volumes_from(&resources, &definitions()).remove(0);
        let groups = json!([{"name": "sc-1", "select_filter": {"place_count": 3}}]);
        volume.resource_group = Some("sc-1".to_string());
        apply_place_counts(std::slice::from_mut(&mut volume), &groups);
        assert_eq!(volume.place_count, Some(3));
        let issues = volume_issues(&volume);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].text, "only 2 of 3 replicas hold data");

        volume.place_count = Some(2);
        let gib = 1024 * 1024;
        volume.replicas[0].drbd = Some(DrbdObserved {
            size_kib: Some(11 * gib + 8),
            fs_size_kib: Some(10 * gib),
            fs_used_kib: Some(9 * gib),
            io_suspended: None,
            peers: HashMap::new(),
        });
        let issues = volume_issues(&volume);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Warning);
        assert!(issues[0].text.starts_with("bc1: filesystem 90% full"));
        assert_eq!(volume.usage_kib(), Some((9 * gib, 10 * gib)));
    }

    #[test]
    fn parses_resync_progress() {
        let syncing = "1001: cs:SyncSource ro:Primary/Secondary ds:UpToDate/Inconsistent C r-----\n    ns:0 nr:0 dw:0 dr:0 al:0 bm:0 lo:0 pe:[0;0] ua:0 ap:[0;0] ep:1 wo:2 oos:9228288\n\t[===>................] sync'ed: 18.4% (9012/11048)M\n\tfinish: 0:04:12 speed: 112,345 (110,000 -- 98,000) K/sec\n\t 0% sector pos: 0/23068672\n";
        let progress = parse_sync_progress(syncing).unwrap();
        assert_eq!(
            progress,
            SyncProgress {
                verify: false,
                percent: 18.4,
                finish_secs: Some(252),
                speed_kib: Some(112_345),
                stalled: false,
            }
        );
        assert_eq!(
            progress.describe(),
            "resync 18.4%, 109.7 MiB/s, ~4m 12s left"
        );

        let stuck = "\t[>....................] verified:  2.0% (100/102)K\n\tfinish: ∞ speed: 0 (0 -- 0) want: 250,000 K/sec (stalled)\n";
        let progress = parse_sync_progress(stuck).unwrap();
        assert!(progress.verify && progress.stalled);
        assert_eq!(progress.finish_secs, None);
        assert_eq!(
            progress.describe(),
            "verify 2.0%, 0 KiB/s, stalled for over 3 min"
        );

        assert_eq!(parse_sync_progress("1015: cs:Established oos:0"), None);
        assert_eq!(format_eta(3_900), "1h 05m");
        assert_eq!(format_eta(9), "9s");
    }

    #[test]
    fn suspended_io_is_an_error() {
        let resources = json!([
            resource("bc1", true, "UpToDate", &[("bc2", true, "Established")]),
            resource("bc2", false, "UpToDate", &[("bc1", true, "Established")]),
        ]);
        let mut volume = volumes_from(&resources, &definitions()).remove(0);
        volume.replicas[0].drbd = Some(DrbdObserved {
            io_suspended: Some(false),
            ..Default::default()
        });
        assert!(volume_issues(&volume).is_empty());
        volume.replicas[0].drbd.as_mut().unwrap().io_suspended = Some(true);
        let issues = volume_issues(&volume);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Error);
        assert!(issues[0].text.starts_with("bc1: I/O suspended"));
    }

    #[test]
    fn filesystem_not_grown_is_a_warning() {
        let resources = json!([resource("bc1", true, "UpToDate", &[])]);
        let mut volume = volumes_from(&resources, &definitions()).remove(0);
        let gib = 1024 * 1024;
        let with_fs = |fs: u64, device: u64| DrbdObserved {
            fs_size_kib: Some(fs),
            fs_used_kib: None,
            io_suspended: None,
            size_kib: Some(device),
            peers: HashMap::new(),
        };
        // Healthy overheads measured on real volumes
        for (fs, device) in [
            (856_678, gib),         // 836.6 MiB on 1 GiB
            (241_664, 300 * 1024),  // 236 MiB on 300 MiB
            (10_380_902, 10 * gib), // 9.9 GiB on 10 GiB
        ] {
            volume.size_kib = Some(device);
            volume.replicas[0].drbd = Some(with_fs(fs, device));
            assert!(volume_issues(&volume).is_empty(), "{fs} on {device}");
        }
        // The incident: XFS at 5.9 GiB on the grown 11 GiB device
        volume.size_kib = Some(11 * gib);
        volume.replicas[0].drbd = Some(with_fs(6_186_598, 11 * gib + 8));
        let issues = volume_issues(&volume);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Warning);
        assert!(issues[0].text.contains("not grown"));
    }

    #[test]
    fn out_of_sync_on_an_established_link_is_an_error() {
        let resources = json!([resource(
            "bc1",
            true,
            "UpToDate",
            &[("bc2", true, "Established")]
        )]);
        let mut volume = volumes_from(&resources, &definitions()).remove(0);
        let observed = |oos, ap| DrbdObserved {
            fs_size_kib: None,
            fs_used_kib: None,
            io_suspended: None,
            size_kib: Some(11 * 1024 * 1024 + 8),
            peers: HashMap::from([(
                "bc2".to_string(),
                PeerObserved {
                    cs: Some("Established".to_string()),
                    oos_kib: Some(oos),
                    ap_in_flight_kib: Some(ap),
                    sync: None,
                    split_brain: false,
                },
            )]),
        };
        volume.replicas[0].drbd = Some(observed(0, 0));
        assert!(volume_issues(&volume).is_empty());

        volume.replicas[0].drbd = Some(observed(4096, 9_223_372_036_854_775_800));
        let issues = volume_issues(&volume);
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().all(|i| i.severity == Severity::Error));
        assert!(issues[0].text.contains("out of sync"));
        assert!(issues[1].text.contains("underflowed"));
    }

    /// The bc aftermath: Primary bc1, bits bc1↔bc2 and bc4→bc2
    fn vmsingle_with_stale_bits() -> LinstorVolume {
        let links = |peers: [&'static str; 2]| -> Vec<(&'static str, bool, &'static str)> {
            peers.iter().map(|p| (*p, true, "Established")).collect()
        };
        let resources = json!([
            resource("bc1", true, "UpToDate", &links(["bc2", "bc4"])),
            resource("bc2", false, "UpToDate", &links(["bc1", "bc4"])),
            resource("bc4", false, "UpToDate", &links(["bc1", "bc2"])),
        ]);
        let mut volume = volumes_from(&resources, &definitions()).remove(0);
        let seen = |oos: &[(&str, u64)]| DrbdObserved {
            fs_size_kib: None,
            fs_used_kib: None,
            io_suspended: None,
            size_kib: Some(11 * 1024 * 1024 + 8),
            peers: oos
                .iter()
                .map(|(peer, oos)| {
                    (
                        peer.to_string(),
                        PeerObserved {
                            cs: Some("Established".to_string()),
                            oos_kib: Some(*oos),
                            ap_in_flight_kib: Some(0),
                            sync: None,
                            split_brain: false,
                        },
                    )
                })
                .collect(),
        };
        volume.replicas[0].drbd = Some(seen(&[("bc2", 924), ("bc4", 0)]));
        volume.replicas[1].drbd = Some(seen(&[("bc1", 924), ("bc4", 0)]));
        volume.replicas[2].drbd = Some(seen(&[("bc1", 0), ("bc2", 48)]));
        volume
    }

    #[test]
    fn secondary_bits_toward_the_primary_name_drbd_153() {
        let issues = volume_issues(&vmsingle_with_stale_bits());
        let texts: Vec<&str> = issues.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.iter().any(|t| {
            t.starts_with("bc2 holds 924 KiB out-of-sync bits toward Primary bc1")
                && t.contains("LINBIT/drbd#153")
        }));
        assert!(texts.contains(&"bc1 → bc2: 924 KiB out of sync"));
        assert!(texts.contains(&"bc4 → bc2: 48 KiB out of sync"));
    }

    #[test]
    fn suggests_resync_from_the_primary_and_secondary_reconnect() {
        let satellites = HashMap::from([(
            "bc1".to_string(),
            SatellitePod {
                namespace: "piraeus-datastore".to_string(),
                name: "linstor-satellite.bc1-kxfwm".to_string(),
            },
        )]);
        let remedies = remediation(
            &vmsingle_with_stale_bits(),
            &satellites,
            "kubectl --context bc",
        );
        assert_eq!(remedies.len(), 2);
        assert!(
            remedies[0]
                .summary
                .starts_with("Resync bc2 from Primary bc1")
        );
        let exec = "kubectl --context bc -n piraeus-datastore exec linstor-satellite.bc1-kxfwm \
             -c linstor-satellite --";
        assert_eq!(
            remedies[0].commands,
            [
                format!("{exec} drbdadm verify pvc-1:bc2"),
                format!("{exec} drbdadm invalidate-remote pvc-1:bc2"),
            ]
        );
        // bc2 ↔ bc4 is between Secondaries: reconnect from bc2, pod unknown
        assert!(
            remedies[1]
                .summary
                .starts_with("Reconnect Secondaries bc2 ↔ bc4")
        );
        assert!(remedies[1].commands[0].contains("<linstor-satellite pod on bc2>"));
        assert!(remedies[1].commands[0].ends_with("drbdadm disconnect pvc-1:bc4"));
        assert!(remedies[1].commands.iter().all(|c| !c.contains("bc1")));
    }

    /// The 2026-10-31 autokuma-data case: Primary bc1 in sync with both
    /// Secondaries, bc2 ↔ bc4 dropped by a split-brain on their histories;
    /// bc2 already retried (`Connecting`), bc4 still StandAlone
    fn secondaries_split_brain() -> LinstorVolume {
        let established = |p: &'static str| (p, true, "Established");
        let resources = json!([
            resource(
                "bc1",
                true,
                "UpToDate",
                &[established("bc2"), established("bc4")]
            ),
            resource(
                "bc2",
                false,
                "UpToDate",
                &[established("bc1"), ("bc4", false, "Off")]
            ),
            resource(
                "bc4",
                false,
                "UpToDate",
                &[established("bc1"), ("bc2", false, "Off")]
            ),
        ]);
        let mut volume = volumes_from(&resources, &definitions()).remove(0);
        let peer = |cs: &str, split_brain: bool| PeerObserved {
            cs: Some(cs.to_string()),
            oos_kib: Some(0),
            split_brain,
            ..Default::default()
        };
        let observed = |peers: [(&str, PeerObserved); 2]| DrbdObserved {
            peers: peers.into_iter().map(|(n, p)| (n.to_string(), p)).collect(),
            ..Default::default()
        };
        volume.replicas[0].drbd = Some(observed([
            ("bc2", peer("Established", false)),
            ("bc4", peer("Established", false)),
        ]));
        volume.replicas[1].drbd = Some(observed([
            ("bc1", peer("Established", false)),
            ("bc4", peer("Connecting", false)),
        ]));
        volume.replicas[2].drbd = Some(observed([
            ("bc1", peer("Established", false)),
            ("bc2", peer("StandAlone", true)),
        ]));
        volume.replicas[2].peers[1].message = "StandAlone".to_string();
        volume
    }

    #[test]
    fn finds_split_brains_behind_the_latest_standalone() {
        let log = "\
kern:    info: [2026-10-31T08:25:01.10Z]: drbd pvc-1 bc2: conn( Unconnected -> Connecting ) [connecting]
kern:    info: [2026-10-31T08:25:01.20Z]: drbd pvc-1/0 drbd1002 bc2: uuid_compare()=split-brain-disconnect by rule=history-both
kern:   alert: [2026-10-31T08:25:01.21Z]: drbd pvc-1/0 drbd1002 bc2: Split-Brain detected but unresolved, dropping connection!
kern:    info: [2026-10-31T08:25:01.22Z]: drbd pvc-1 bc2: conn( Connected -> Disconnecting ) [disconnected]
kern:    info: [2026-10-31T08:25:01.23Z]: drbd pvc-1 bc2: conn( Disconnecting -> StandAlone ) [disconnected]
kern:   alert: [2026-10-31T08:26:00.00Z]: drbd pvc-2 bc1: Split-Brain detected but unresolved, dropping connection!
kern:    info: [2026-10-31T08:26:00.01Z]: drbd pvc-2 bc1: conn( Disconnecting -> StandAlone ) [disconnected]
kern:    info: [2026-10-31T08:34:01.00Z]: drbd pvc-2 bc1: conn( StandAlone -> Unconnected ) [connect]
kern:   alert: [2026-10-31T08:40:00.00Z]: drbd pvc-3 bc1: Split-Brain detected but unresolved, dropping connection!
kern:    info: [2026-10-31T08:40:00.01Z]: drbd pvc-3 bc1: conn( Disconnecting -> Unconnected ) [disconnected]
kern:    info: [2026-10-31T08:40:00.02Z]: drbd pvc-3 bc1: conn( Unconnected -> Connecting ) [connecting]
kern:    info: [2026-10-31T08:50:00.00Z]: drbd pvc-3 bc1: conn( Connecting -> StandAlone ) [disconnect]
kern:    info: [2026-10-31T08:50:00.01Z]: drbd pvc-3/0 drbd1003: disk( UpToDate -> Outdated )
";
        // pvc-2 reconnected since; pvc-3's StandAlone is a later disconnect
        assert_eq!(
            split_brain_links(log),
            [("pvc-1".to_string(), "bc2".to_string())]
        );
        assert!(split_brain_links("").is_empty());
    }

    #[test]
    fn standalone_link_is_one_issue_naming_split_brain() {
        let mut volume = secondaries_split_brain();
        let texts = |v: &LinstorVolume| -> Vec<String> {
            volume_issues(v).into_iter().map(|i| i.text).collect()
        };
        assert_eq!(
            texts(&volume),
            [
                "bc2 ↔ bc4: split-brain, StandAlone on bc4, Connecting on bc2: \
              DRBD won't reconnect by itself"
            ]
        );
        // Without the kernel log's word for it, only what the state shows
        let bc4 = volume.replicas[2].drbd.as_mut().unwrap();
        bc4.peers.get_mut("bc2").unwrap().split_brain = false;
        assert_eq!(
            texts(&volume),
            ["bc2 ↔ bc4: StandAlone on bc4, Connecting on bc2: DRBD won't reconnect by itself"]
        );
    }

    #[test]
    fn reconnects_standalone_secondaries_in_sync_with_the_primary() {
        let remedies = remediation(&secondaries_split_brain(), &HashMap::new(), "kubectl");
        assert_eq!(remedies.len(), 2);
        assert!(
            remedies[0]
                .summary
                .starts_with("Reconnect Secondaries bc2 ↔ bc4")
        );
        // Only the StandAlone end needs it; bc2 is already connecting
        assert_eq!(remedies[0].commands.len(), 1);
        assert!(remedies[0].commands[0].contains("<linstor-satellite pod on bc4>"));
        assert!(remedies[0].commands[0].ends_with("drbdadm connect pvc-1:bc2"));
        assert!(remedies[1].summary.starts_with("Only if it drops back"));
        assert!(remedies[1].commands[0].contains("<linstor-satellite pod on bc4>"));
        assert!(remedies[1].commands[0].ends_with("drbdadm connect --discard-my-data pvc-1:bc2"));
        assert!(remedies[1].commands[1].contains("<linstor-satellite pod on bc2>"));
        assert!(remedies[1].commands[1].ends_with("drbdadm connect pvc-1:bc4"));
        // The Primary is never touched
        assert!(
            remedies
                .iter()
                .flat_map(|r| &r.commands)
                .all(|c| !c.contains("bc1"))
        );
    }

    #[test]
    fn no_commands_for_a_split_brain_with_the_primary_or_a_secondary_behind() {
        let mut volume = secondaries_split_brain();
        volume.replicas[0].in_use = false;
        volume.replicas[1].in_use = true;
        let remedies = remediation(&volume, &HashMap::new(), "kubectl");
        assert_eq!(remedies.len(), 1);
        assert!(remedies[0].summary.contains("Primary bc2 is one end"));
        assert!(remedies[0].commands.is_empty());

        let mut volume = secondaries_split_brain();
        let bc1 = volume.replicas[0].drbd.as_mut().unwrap();
        bc1.peers.get_mut("bc4").unwrap().oos_kib = Some(48);
        let remedies: Vec<_> = remediation(&volume, &HashMap::new(), "kubectl")
            .into_iter()
            .filter(|r| r.summary.contains("bc2 ↔ bc4"))
            .collect();
        assert_eq!(remedies.len(), 1);
        assert!(
            remedies[0]
                .summary
                .contains("bc4 is not UpToDate and in sync with Primary bc1")
        );
        assert!(remedies[0].commands.is_empty());
    }

    #[test]
    fn no_suggestion_while_resyncing() {
        let mut volume = vmsingle_with_stale_bits();
        for r in &mut volume.replicas {
            for p in &mut r.peers {
                p.replication = Some("SyncSource".to_string());
            }
        }
        assert!(remediation(&volume, &HashMap::new(), "kubectl").is_empty());
    }

    /// The kernel says Established but LINSTOR reports no replication state:
    /// the stale bits are still an error, but no commands are suggested
    #[test]
    fn stale_bits_without_linstor_state_are_reported_but_not_fixed() {
        let mut volume = vmsingle_with_stale_bits();
        for r in &mut volume.replicas {
            for p in &mut r.peers {
                p.replication = None;
            }
        }
        let issues = volume_issues(&volume);
        assert!(
            issues
                .iter()
                .any(|i| { i.severity == Severity::Error && i.text.contains("LINBIT/drbd#153") })
        );
        assert!(remediation(&volume, &HashMap::new(), "kubectl").is_empty());
    }

    #[test]
    fn parses_debugfs() {
        let proc_drbd = "1015: cs:Established ro:Secondary/Secondary ds:UpToDate/UpToDate C r-----\n    ns:0 nr:0 dw:0 dr:7168 al:0 bm:2 lo:1 pe:[0;0] ua:0 ap:[0;0] ep:1 wo:2 oos:12\n";
        assert_eq!(
            parse_proc_drbd(proc_drbd),
            ProcDrbd {
                cs: Some("Established".to_string()),
                oos_kib: Some(12),
                suspended: Some(false),
            }
        );
        let frozen = proc_drbd.replace("C r-----", "C s----Q");
        assert_eq!(parse_proc_drbd(&frozen).suspended, Some(true));
        assert_eq!(parse_proc_drbd("garbage"), ProcDrbd::default());

        let debug = "flags: 0x180800 : RECONNECT\n            ap_in_flight: 16 KiB (32 sectors)\n            rs_in_flight: 0 KiB (0 sectors)\n";
        assert_eq!(parse_ap_in_flight(debug), Some(16));
        assert_eq!(parse_ap_in_flight("nothing"), None);
    }

    #[test]
    fn formats_kib() {
        assert_eq!(format_kib(924), "924 KiB");
        assert_eq!(format_kib(11 * 1024 * 1024), "11.0 GiB");
        assert_eq!(format_kib(1536), "1.5 MiB");
    }

    /// Scan a real cluster: the current KUBECONFIG context and the Talos
    /// context named by `TALOS9S_LIVE_CONTEXT`, e.g.
    /// `TALOS9S_LIVE_CONTEXT=bc cargo test live_scan -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_scan() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("TALOS9S_LIVE_CONTEXT").expect("set TALOS9S_LIVE_CONTEXT");
        let kube = kube::Client::try_default().await.unwrap();
        let talos = TalosClient::from_named_context(&context).await.unwrap();
        let nodes: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(kube.clone());
        let node_ips: HashMap<String, String> = nodes
            .list(&Default::default())
            .await
            .unwrap()
            .items
            .into_iter()
            .filter_map(|n| {
                let ip = n
                    .status?
                    .addresses?
                    .into_iter()
                    .find(|a| a.type_ == "InternalIP")?
                    .address;
                Some((n.metadata.name?, ip))
            })
            .collect();
        let started = std::time::Instant::now();
        let mut volumes = fetch_volumes(&kube).await.unwrap().expect("no LINSTOR");
        let fetched = started.elapsed();
        observe_drbd(&talos, &node_ips, &mut volumes).await;
        let mut zfs = crate::zfs::fetch_zfs_volumes(&kube)
            .await
            .unwrap()
            .unwrap_or_default();
        let mut mount_nodes = linstor_mount_nodes(&volumes);
        mount_nodes.extend(zfs.iter().map(|z| z.node.clone()));
        mount_nodes.sort();
        mount_nodes.dedup();
        let mounts = fetch_mounts(&talos, &node_ips, &mount_nodes).await;
        apply_mounts(&mut volumes, &mounts);
        crate::zfs::apply_zfs_mounts(&mut zfs, &mounts);
        let pvs = crate::orphans::fetch_pv_index(&kube).await.unwrap();
        for v in volumes.iter().filter(|v| v.pvc.is_some()) {
            if let Some(o) = pvs.orphan(&v.resource) {
                println!("orphan LINSTOR {} ({:?}): {o:?}", v.resource, v.pvc);
            }
        }
        for v in &zfs {
            if let Some(o) = pvs.orphan(&v.name) {
                println!("orphan ZFS {} on {}: {o:?}", v.name, v.node);
            }
        }
        let place: Vec<String> = volumes
            .iter()
            .map(|v| format!("{}/{:?}", v.diskful_replicas(), v.place_count))
            .collect();
        let mut place_summary: HashMap<&String, usize> = HashMap::new();
        for p in &place {
            *place_summary.entry(p).or_default() += 1;
        }
        println!("replicas/place_count: {place_summary:?}");
        let used: Vec<f64> = volumes
            .iter()
            .filter_map(super::LinstorVolume::usage_kib)
            .map(|(u, s)| u as f64 / s as f64 * 100.0)
            .collect();
        println!(
            "LINSTOR usage known for {}, max {:.0}%",
            used.len(),
            used.iter().copied().fold(0.0, f64::max)
        );
        let mounted: Vec<&crate::zfs::ZfsVolume> = zfs.iter().filter(|z| z.fs.is_some()).collect();
        let ratio =
            |z: &crate::zfs::ZfsVolume| z.fs.unwrap().0 as f64 / z.capacity.unwrap_or(1) as f64;
        let (rmin, rmax) = mounted
            .iter()
            .map(|z| ratio(z))
            .fold((f64::MAX, 0.0f64), |(a, b), r| (a.min(r), b.max(r)));
        println!(
            "{} ZFS volumes, {} mounted, mount/capacity ratio {rmin:.3}..{rmax:.3}, states {:?}",
            zfs.len(),
            mounted.len(),
            zfs.iter()
                .map(|z| z.state.as_str())
                .collect::<std::collections::BTreeSet<_>>()
        );
        for z in &zfs {
            for issue in crate::zfs::zfs_volume_issues(z) {
                println!(
                    "ZFS {:?} {} {:?}: {}",
                    issue.severity, z.name, z.pvc, issue.text
                );
            }
        }
        let replicas: usize = volumes.iter().map(|v| v.replicas.len()).sum();
        let observed = volumes
            .iter()
            .flat_map(|v| &v.replicas)
            .filter(|r| r.drbd.as_ref().is_some_and(|d| d.size_kib.is_some()))
            .count();
        let peer_reads = volumes
            .iter()
            .flat_map(|v| &v.replicas)
            .filter_map(|r| r.drbd.as_ref())
            .flat_map(|d| d.peers.values())
            .filter(|p| p.cs.is_some() && p.oos_kib.is_some() && p.ap_in_flight_kib.is_some())
            .count();
        println!(
            "{} volumes, {} replicas, size read for {}, full peer state for {} peer links; LINSTOR {:?}, total {:?}",
            volumes.len(),
            replicas,
            observed,
            peer_reads,
            fetched,
            started.elapsed()
        );
        let satellites = fetch_satellites(&kube).await;
        for v in &volumes {
            for issue in volume_issues(v) {
                println!(
                    "{:?} {} {:?}: {}",
                    issue.severity, v.resource, v.pvc, issue.text
                );
            }
            for remedy in remediation(v, &satellites, "kubectl") {
                println!("  fix: {}", remedy.summary);
                for command in remedy.commands {
                    println!("       {command}");
                }
            }
        }
    }
}
