//! LINSTOR (Piraeus) node list
//!
//! Piraeus runs a LINSTOR controller behind a `linstor-controller` Service.
//! Its presence is how we detect that a cluster uses Piraeus; the node list
//! (`linstor node list`) comes from the controller's REST API, reached through
//! the kube-apiserver's service proxy so no port-forward is needed, along
//! with each node's storage pools (`linstor storage-pool list`). Clusters
//! without the Service simply report nothing.

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::serde_json::{self, Value};
use kube::Client;
use kube::api::{Api, ListParams};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;

/// Name of the LINSTOR controller Service created by the Piraeus operator
const CONTROLLER_SERVICE: &str = "linstor-controller";

/// One LINSTOR node, as `linstor node list` shows it
#[derive(Debug, Clone, PartialEq)]
pub struct LinstorNode {
    pub name: String,
    /// e.g. `SATELLITE`, `COMBINED`
    pub node_type: String,
    /// `address:port (encryption)` per network interface
    pub addresses: Vec<String>,
    /// Controller's connection status, e.g. `ONLINE`, `OFFLINE`
    pub state: String,
    /// Node flags such as `EVICTED`
    pub flags: Vec<String>,
    /// Storage pools with capacity (diskless pools left out), `None` when
    /// the pool list could not be fetched
    pub pools: Option<Vec<StoragePool>>,
}

/// One LINSTOR storage pool of a node
#[derive(Debug, Clone, PartialEq)]
pub struct StoragePool {
    pub name: String,
    /// e.g. `ZFS_THIN`, `LVM_THIN`
    pub provider: String,
    /// Free and total capacity in bytes, when the satellite reported them
    pub free: Option<u64>,
    pub total: Option<u64>,
    /// Whether LINSTOR attached error or warning reports to the pool
    pub has_problems: bool,
}

impl StoragePool {
    /// Percentage of the pool in use, `None` without capacity data
    pub fn used_pct(&self) -> Option<f32> {
        let (free, total) = (self.free?, self.total?);
        (total > 0).then(|| total.saturating_sub(free) as f32 / total as f32 * 100.0)
    }
}

impl LinstorNode {
    /// Whether the controller reports the node connected and unflagged
    pub fn is_online(&self) -> bool {
        self.state.eq_ignore_ascii_case("ONLINE") && self.flags.is_empty()
    }
}

/// LINSTOR state of a cluster that runs Piraeus
#[derive(Debug, Clone, PartialEq)]
pub enum LinstorStatus {
    /// Node list from the controller, sorted by name
    Nodes(Vec<LinstorNode>),
    /// The controller Service exists but its API could not be queried
    Unreachable(String),
}

/// LINSTOR status of the cluster, or `None` when it doesn't run Piraeus
/// (no `linstor-controller` Service).
///
/// An error means the Kubernetes API itself failed, so the caller can keep
/// what it had; a failing LINSTOR API is reported as `Unreachable`.
pub async fn fetch_linstor(client: &Client) -> Result<Option<LinstorStatus>, kube::Error> {
    let base = match controller_base(client).await? {
        None => return Ok(None),
        Some(Err(e)) => return Ok(Some(LinstorStatus::Unreachable(e))),
        Some(Ok(base)) => base,
    };
    let path = format!("{base}/v1/nodes");

    let pools_path = format!("{base}/v1/view/storage-pools");
    let (nodes, pools) = tokio::join!(get_json(client, &path), get_json(client, &pools_path));
    let status = match nodes {
        Ok(json) => {
            let pools = match pools {
                Ok(json) => Some(json),
                Err(e) => {
                    tracing::debug!("LINSTOR storage pools unavailable: {}", e);
                    None
                }
            };
            LinstorStatus::Nodes(nodes_from(&json, pools.as_ref()))
        }
        Err(e) => LinstorStatus::Unreachable(e),
    };
    Ok(Some(status))
}

/// Service proxy path to the LINSTOR controller API (append e.g.
/// `/v1/nodes`). `None` when the cluster has no `linstor-controller` Service;
/// `Err` when the Service can't be proxied to.
pub(crate) async fn controller_base(
    client: &Client,
) -> Result<Option<Result<String, String>>, kube::Error> {
    let services: Api<Service> = Api::all(client.clone());
    let params = ListParams::default().fields(&format!("metadata.name={CONTROLLER_SERVICE}"));
    let Some(service) = services.list(&params).await?.items.into_iter().next() else {
        return Ok(None);
    };
    Ok(Some(proxy_path(&service, "").ok_or_else(|| {
        "linstor-controller Service has no ports".to_string()
    })))
}

/// GET a LINSTOR API path through the service proxy and parse it as JSON
pub(crate) async fn get_json(client: &Client, path: &str) -> Result<Value, String> {
    let request = http::Request::get(path)
        .body(Vec::new())
        .map_err(|e| e.to_string())?;
    let body = client
        .request_text(request)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&body).map_err(|e| format!("unexpected LINSTOR response: {e}"))
}

// Lenient JSON parsing. LINSTOR's REST API and the OpenEBS CRDs aren't
// versioned with us: a field that changes type or shape must degrade to
// "unknown" for that field (or that entry), never fail a whole list.

/// The items of a JSON array, each parsed on its own so an entry that
/// doesn't match `T` is skipped, not the whole response
pub(crate) fn items<T: DeserializeOwned>(json: &Value) -> impl Iterator<Item = T> + '_ {
    json.as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match T::deserialize(item) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                tracing::debug!("skipping unexpected entry: {}", e);
                None
            }
        })
}

/// A field of the wrong type reads as missing (`#[serde(default)]` covers a
/// missing one)
pub(crate) fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Ok(T::deserialize(Value::deserialize(d)?).ok())
}

/// A field that counts by being present, even `null` or of the wrong
/// shape (then with `T`'s defaults)
pub(crate) fn present<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Ok(Some(
        T::deserialize(Value::deserialize(d)?).unwrap_or_default(),
    ))
}

/// An array field keeping only the elements that parse; anything else is
/// an empty list
pub(crate) fn lenient_items<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    let value = Value::deserialize(d)?;
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| T::deserialize(item).ok())
        .collect())
}

/// An object field keyed by name, values that don't parse with `T`'s
/// defaults; anything else is an empty map
pub(crate) fn lenient_map<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    d: D,
) -> Result<HashMap<String, T>, D::Error> {
    let value = Value::deserialize(d)?;
    Ok(value
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, v)| (key.clone(), T::deserialize(v).unwrap_or_default()))
        .collect())
}

/// One entry of `/v1/view/storage-pools`
#[derive(Deserialize)]
struct PoolJson {
    node_name: String,
    storage_pool_name: String,
    #[serde(default, deserialize_with = "lenient")]
    provider_kind: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    free_capacity: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    total_capacity: Option<u64>,
    #[serde(default, deserialize_with = "lenient_items")]
    reports: Vec<ReportJson>,
}

#[derive(Deserialize)]
struct ReportJson {
    #[serde(default, deserialize_with = "lenient")]
    ret_code: Option<i64>,
}

/// Storage pools with capacity per node name, from `/v1/view/storage-pools`.
/// LINSTOR reports capacity in KiB.
fn pools_from(json: &Value) -> HashMap<String, Vec<StoragePool>> {
    let mut by_node: HashMap<String, Vec<StoragePool>> = Default::default();
    for pool in items::<PoolJson>(json) {
        let provider = pool.provider_kind.unwrap_or_else(|| "?".to_string());
        // Every node has a diskless pool; it holds no data
        if provider == "DISKLESS" {
            continue;
        }
        // A report whose return code has the top bit set is an error or warning
        let has_problems = pool
            .reports
            .iter()
            .any(|r| r.ret_code.is_some_and(|c| c < 0));
        by_node
            .entry(pool.node_name)
            .or_default()
            .push(StoragePool {
                name: pool.storage_pool_name,
                provider,
                free: pool.free_capacity.map(|k| k * 1024),
                total: pool.total_capacity.map(|k| k * 1024),
                has_problems,
            });
    }
    for pools in by_node.values_mut() {
        pools.sort_by(|a, b| a.name.cmp(&b.name));
    }
    by_node
}

/// kube-apiserver service proxy path to `api_path` on the controller.
///
/// Prefers the plain HTTP API port (`api`, 3370) and falls back to the TLS
/// one (`secure-api`, 3371), then to the first port.
fn proxy_path(service: &Service, api_path: &str) -> Option<String> {
    let namespace = service.metadata.namespace.as_deref().unwrap_or("default");
    let name = service.metadata.name.as_deref()?;
    let ports = service.spec.as_ref()?.ports.as_ref()?;
    let is = |p: &&k8s_openapi::api::core::v1::ServicePort, port_name: &str, number: i32| {
        p.name.as_deref() == Some(port_name) || p.port == number
    };
    let (scheme, port) = if let Some(p) = ports.iter().find(|p| is(p, "api", 3370)) {
        ("", p)
    } else if let Some(p) = ports.iter().find(|p| is(p, "secure-api", 3371)) {
        ("https:", p)
    } else {
        ("", ports.first()?)
    };
    let port = port.name.clone().unwrap_or_else(|| port.port.to_string());
    Some(format!(
        "/api/v1/namespaces/{namespace}/services/{scheme}{name}:{port}/proxy{api_path}"
    ))
}

/// One entry of `/v1/nodes`
#[derive(Deserialize)]
struct NodeJson {
    name: String,
    #[serde(rename = "type", default, deserialize_with = "lenient")]
    node_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    connection_status: Option<String>,
    #[serde(default, deserialize_with = "lenient_items")]
    flags: Vec<String>,
    /// Interfaces without an address are left out
    #[serde(default, deserialize_with = "lenient_items")]
    net_interfaces: Vec<NicJson>,
}

#[derive(Deserialize)]
struct NicJson {
    address: String,
    #[serde(default, deserialize_with = "lenient")]
    satellite_port: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    satellite_encryption_type: Option<String>,
}

/// Parse the controller's `/v1/nodes` response, skipping unnamed entries,
/// and attach each node's storage pools from `pools`
fn nodes_from(json: &Value, pools: Option<&Value>) -> Vec<LinstorNode> {
    let mut pools = pools.map(pools_from);
    let mut nodes: Vec<LinstorNode> = items::<NodeJson>(json)
        .map(|node| {
            let addresses = node
                .net_interfaces
                .into_iter()
                .map(|nic| {
                    let mut text = match nic.satellite_port {
                        Some(port) => format!("{}:{port}", nic.address),
                        None => nic.address,
                    };
                    if let Some(enc) = nic.satellite_encryption_type {
                        text.push_str(&format!(" ({enc})"));
                    }
                    text
                })
                .collect();
            LinstorNode {
                pools: pools
                    .as_mut()
                    .map(|p| p.remove(&node.name).unwrap_or_default()),
                name: node.name,
                node_type: node.node_type.unwrap_or_else(|| "?".to_string()),
                addresses,
                state: node
                    .connection_status
                    .unwrap_or_else(|| "UNKNOWN".to_string()),
                flags: node.flags,
            }
        })
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::serde_json::json;

    #[test]
    fn parses_node_list() {
        let json = json!([
            {
                "name": "worker-2",
                "type": "SATELLITE",
                "connection_status": "OFFLINE",
                "flags": ["EVICTED"],
                "net_interfaces": [{
                    "name": "default-ipv4",
                    "address": "10.0.0.12",
                    "satellite_port": 3366,
                    "satellite_encryption_type": "PLAIN"
                }]
            },
            {
                "name": "worker-1",
                "type": "SATELLITE",
                "connection_status": "ONLINE",
                "net_interfaces": [{"address": "10.0.0.11"}]
            },
            {"type": "SATELLITE"}
        ]);
        let nodes = nodes_from(&json, None);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].pools, None);
        assert_eq!(nodes[0].name, "worker-1");
        assert_eq!(nodes[0].addresses, ["10.0.0.11"]);
        assert!(nodes[0].is_online());
        assert_eq!(nodes[1].addresses, ["10.0.0.12:3366 (PLAIN)"]);
        assert_eq!(nodes[1].flags, ["EVICTED"]);
        assert!(!nodes[1].is_online());
    }

    #[test]
    fn odd_entries_and_fields_degrade_per_item() {
        let json = json!([
            "not a node",
            {"name": 7, "type": "SATELLITE"},
            {
                "name": "bc1",
                "type": 5,
                "connection_status": null,
                "flags": ["EVICTED", 3],
                "net_interfaces": [
                    {"name": "no-address"},
                    {"address": "10.0.0.1", "satellite_port": "3366",
                     "satellite_encryption_type": false}
                ]
            }
        ]);
        let pools = json!([
            {"node_name": "bc1", "storage_pool_name": "datapool",
             "free_capacity": "300", "total_capacity": 400, "reports": "none"},
            {"node_name": "bc1"},
            42
        ]);
        let nodes = nodes_from(&json, Some(&pools));
        assert_eq!(nodes.len(), 1);
        let bc1 = &nodes[0];
        assert_eq!(bc1.node_type, "?");
        assert_eq!(bc1.state, "UNKNOWN");
        assert_eq!(bc1.flags, ["EVICTED"]);
        assert_eq!(bc1.addresses, ["10.0.0.1"]);
        let pools = bc1.pools.as_ref().unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].provider, "?");
        assert_eq!((pools[0].free, pools[0].total), (None, Some(400 * 1024)));
        assert!(!pools[0].has_problems);

        // Not a list at all: nothing, not an error
        assert!(nodes_from(&json!({"error": "x"}), None).is_empty());
    }

    #[test]
    fn attaches_storage_pools_without_diskless() {
        let nodes = json!([{"name": "bc1", "connection_status": "ONLINE"}, {"name": "bc2"}]);
        let pools = json!([
            {"node_name": "bc1", "storage_pool_name": "datapool", "provider_kind": "ZFS_THIN",
             "free_capacity": 300, "total_capacity": 400},
            {"node_name": "bc1", "storage_pool_name": "DfltDisklessStorPool",
             "provider_kind": "DISKLESS", "free_capacity": 9223372036854775807u64},
            {"node_name": "bc2", "storage_pool_name": "datapool", "provider_kind": "ZFS_THIN",
             "reports": [{"ret_code": -4611686018427387904i64, "message": "no space"}]}
        ]);
        let nodes = nodes_from(&nodes, Some(&pools));
        let bc1 = nodes[0].pools.as_ref().unwrap();
        assert_eq!(bc1.len(), 1);
        assert_eq!(bc1[0].free, Some(300 * 1024));
        assert_eq!(bc1[0].used_pct(), Some(25.0));
        assert!(!bc1[0].has_problems);
        let bc2 = nodes[1].pools.as_ref().unwrap();
        assert!(bc2[0].has_problems);
        assert_eq!(bc2[0].used_pct(), None);
    }

    #[test]
    fn proxy_path_prefers_plain_api_port() {
        let service: Service = serde_json::from_value(json!({
            "metadata": {"name": "linstor-controller", "namespace": "piraeus-datastore"},
            "spec": {"ports": [
                {"name": "secure-api", "port": 3371},
                {"name": "api", "port": 3370}
            ]}
        }))
        .unwrap();
        assert_eq!(
            proxy_path(&service, "/v1/nodes").as_deref(),
            Some(
                "/api/v1/namespaces/piraeus-datastore/services/linstor-controller:api/proxy/v1/nodes"
            )
        );

        let tls_only: Service = serde_json::from_value(json!({
            "metadata": {"name": "linstor-controller", "namespace": "storage"},
            "spec": {"ports": [{"name": "secure-api", "port": 3371}]}
        }))
        .unwrap();
        assert_eq!(
            proxy_path(&tls_only, "/v1/nodes").as_deref(),
            Some(
                "/api/v1/namespaces/storage/services/https:linstor-controller:secure-api/proxy/v1/nodes"
            )
        );
    }
}
