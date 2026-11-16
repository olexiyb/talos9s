---
paths:
  - "crates/talos9s-tui/src/components/kubernetes.rs"
  - "crates/talos9s-core/src/kubernetes.rs"
---

# Kubernetes and GitOps views: every cluster, read-only

The design is `plans/kubernetes.md`. Keys are in README.md.

- **Read-only, always.** No edit, apply, delete, scale, restart, exec or
  port-forward, not behind a confirmation either: cluster state comes from
  GitOps. The view shows and copies `kubectl` commands.
- **Every cluster, whatever is selected.** The component gets every
  cluster of the tree (`ClusterComponent::cluster_sources`) and its own
  Kubernetes client per cluster (`k8s::connect`, by CA). `scope_key` is
  constant, so moving in the tree doesn't reload it; `X` narrows to one.
- **Each cluster stands alone:** connected, discovered and listed in
  parallel, each in `WORKLOADS_FETCH_TIMEOUT`; a failure is `?` with the
  API's message for that cluster only. A kind a cluster doesn't serve
  (no Flux or Argo CD, no such CRD) is a dim note, not an error.
- **Health from status** (`talos9s_core::kubernetes::status`): replicas
  for workloads, container states for pods, Argo CD's health and sync,
  else the `Ready` (or `Available`) condition. No known status means no
  verdict (`Unknown`), never a problem. The Problems list follows the
  Workloads rules: finished pods, pods being scheduled and Job pods aren't
  problems.
- **Redacted when read** (`redact`): a Secret's values, `{name: *PASSWORD*,
  value: …}` entries (container env), the last-applied annotation and
  `managedFields` are gone before a row is kept. YAML is drawn through
  `node_info::mask_config` too, and every string from a cluster through
  `printable` (no control characters reach the terminal).
- **Bounded:** at most `ROW_CAP` objects per kind and cluster (more is
  said); discovery is cached `DISCOVERY_TTL`; diffs of more than
  `DIFF_MAX_LINES` lines aren't computed.
- **A list picked while another is read** (`unread`): the app drops a
  refresh request while one runs, so the view asks again on every tick
  until a read for its list lands; a read for another list is dropped.
- **Logs across clusters:** `LogSource::cluster` names the cluster, and the
  pane reads it with that cluster's client (`LogPane::set_clients`).

Known limits:

- `mask_config` hides everything under `data:`, so a ConfigMap's data is
  hidden in the YAML and the diff too.
- Search looks through the common kinds (workloads, pods, Services,
  ConfigMaps, Jobs, CronJobs, Kustomizations, HelmReleases, Applications),
  not every CRD.
