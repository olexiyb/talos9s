# Design: Containers view rework

Status: built · Author: Olexiy Buyanskyy · 2026-11-02

## Summary

The Containers view becomes the Workloads view's counterpart one level
down: every container on a node, a group or the whole cluster, read from
containerd through the Talos API. It works when the Kubernetes API doesn't,
which is when it matters most. It gets the same shape as Workloads:
summary line, a list grouped by pod, sort by every column, namespace warp,
and Enter for the logs, read through Talos instead of Kubernetes.

## Why

Today (`components/node_info.rs`, the Containers tab):

- Node only. On a group or the cluster it says "Select a node", so "where
  does this container run" means visiting nodes one by one.
- One flat table: NAMESPACE, POD, CONTAINER, IMAGE, PID, STATUS. A pod's
  containers aren't grouped; there's no sort, no warp, no logs.
- It shares a component with Machine config and Extensions, which have
  nothing in common with it.
- Nothing says it is containerd: that it works without the Kubernetes API,
  and Workloads doesn't point to it when the API is down.

## Goals

1. Show containers on a node, a group or the cluster, with a NODE column
   off the node.
2. Same keys as Workloads where they mean the same: `/`, `A`, `W`, `1`-`N`
   sort and header clicks, Enter for logs, `v`, `w`, `y`.
3. Logs of any container, system services included, through the Talos API:
   no Kubernetes API needed.
4. Work with the Kubernetes API down, and say so; Workloads points here
   when it can't reach the API.
5. Bound and degrade per node: a dead node shows its error, the others load.

## Non-goals

- Starting, stopping or restarting containers (`talosctl restart` is shown
  and copied, never run).
- Restart counts and exit codes: the Talos Containers API doesn't return
  them (only id, name, image, pid, status, pod). Workloads has them from
  Kubernetes.
- CPU and memory per container (`talosctl stats` could add it later).

## Layout

```
┌ Containers · bc (containerd, no Kubernetes API needed) ──────────── <W> ──┐
│ 4 nodes · 212 running · 3 exited · 1 other · bc4: timed out after 10s       │
│   NODE  NAMESPACE    POD / CONTAINER                 IMAGE          STATUS  │
│ ▾ bc1   system       apid                            ghcr.io/sidero RUNNING │
│ ▾ bc1   kube-system  cilium-7xk2p                                   2/2     │
│           cilium-agent                               quay.io/cil..  RUNNING │
│           mount-cgroup                               quay.io/cil..  EXITED  │
│ ▾ bc2   web          gateway-7f9c-x2                                0/1     │
│           gateway                                    reg/gateway    EXITED  │
│ ─────────────────────────────────────────────────────────────────────────── │
│ logs: web/gateway-7f9c-x2 gateway on bc2 (talosctl -n bc2 logs -k …)        │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Summary line

Nodes read, running / exited / other counts, and each failed node's error
("bc4: timed out after 10s"). Kubernetes containers missing on a node
without kubelet only notes it, as today. The warp hint at the right end.

### The list

- Rows: pods (from the CRI sandboxes) with their containers indented, and
  system services under a `system` namespace. One row per pod keeps a
  DaemonSet on four nodes as four short entries.
- Columns: NODE (off a single node), NAMESPACE, POD / CONTAINER, IMAGE,
  PID, STATUS. A pod row's STATUS is `running/total`.
- Health color: running green; exited dimmed (finished init containers and
  jobs exit normally, "no false positives"); other states yellow. A pod
  whose containers have all exited and that isn't a job is still dimmed:
  the API doesn't tell us why it exited, Workloads does.
- `1`-`6` sort by column (NODE first) and header clicks, as in the pod list;
  default NAMESPACE, then pod. Saved.
- `A` shows exited containers too (default: hidden, an exited container
  next to its running successor is noise). Saved.
- `/` filters on node, namespace, pod, container and image.
- `W` warps by namespace with the shared picker and the same per-context
  warp as Workloads and Volumes. `system` is offered as a namespace.
- Enter on a pod folds or unfolds its containers.

### Logs pane

The lower pane shows the selected row's logs, like the focused workload:

- A container: its logs; a pod: its containers merged by time; a system
  service: its logs (as in the Logs view).
- Read with the Talos Logs API: namespace `k8s.io`, driver CRI, the
  container id for pod containers; `system`, containerd for services.
  Bounded by the same `POD_LOG_SOURCES_MAX`, `POD_LOG_TOTAL_LINES` and a
  timeout; read again while following the end.
- Previous run: CRI keeps the last exited container of a pod, listed with
  `A`; selecting it shows that run's logs. No `p` key needed.
- Level filters, `/` search with `n`/`N`, `w` wrap (the shared setting),
  `y` copy, `v` external viewer: the `LogPane` from Workloads, given a
  second kind of source.
- `y` on a row (outside the logs) copies `talosctl -n NODE logs -k ID` or
  `talosctl -n NODE logs SERVICE`.

### From Workloads

When the Kubernetes API can't be reached, the Workloads error says "the
Containers view (`C`) reads containerd directly".

## Data

| Read | When | Why |
|---|---|---|
| Containers, `system` and `k8s.io`, per node | each refresh, all target nodes concurrently | rows |
| Logs, per selected source | after the selection rests 300 ms; while following | logs pane |

- One `CONTAINERS_TIMEOUT` (moved to `talos9s_core::constants`) per node
  call; results kept per node, so one dead node is one error line.
- Nodes come from the tree like Processes (`ShowProcesses(title, nodes)`).
- Grouping, sorting, filtering and the warp in talos9s-core (pure, tested):
  `container_rows(containers, options)`, a `ContainerSort`, and splitting
  `namespace/pod`.
- To verify first: whether Talos returns CRI log lines with the CRI prefix
  (`2026-11-02T… stdout F msg`) or bare; the merge needs the timestamp.

## Principles applied

- **No false positives.** Exited is not an error: no restart counts or exit
  codes here, so nothing is called failing. Only states that are neither
  running nor exited (created, unknown) are yellow.
- **Degrade gracefully.** Per node errors; Kubernetes containers missing on
  a node only notes it; works without the Kubernetes API.
- **Bound every call.** Timeout per node call and per log read; log sources
  and lines capped.

## Implementation steps

1. Move Containers out of `node_info.rs` into `components/containers.rs`
   (node only, same table), wired as its own view. Constants moved. (Done)
2. Several nodes: the Processes pattern, NODE column, per-node errors,
   summary line. (Done; the containerd note is in the README, the title
   has no room)
3. Core `container_rows`: pods with their containers, `A`, `/`, sort keys
   and header clicks, saved settings. (Done)
4. `W` warp with the shared picker. (Done)
5. The logs pane: a Talos source for `LogPane`, merged pod logs, `v`, `y`.
   (Done: CRI lines carry their time; service lines' `2026/11/02 …` time
   is turned into RFC 3339. `l` gives the pane the keys, since the digits
   and `/` belong to the list.)
6. Workloads' hint when the Kubernetes API is down; README rows and keys;
   rules (`tui-views.md` table: Containers on group and cluster). (Done)

Each step works on its own; 1–2 already answer "where does it run".

## Testing

- Unit tests: grouping (a pod with an init and a main container, a system
  service, a sandbox without containers), sort, filter, warp.
- Live on bc (read-only): node, control plane group, cluster; compare with
  `talosctl -n bc1 containers -k`. A node made unreachable (wrong address in
  a test talosconfig) shows its error and the others load.
- Logs: a system service and a pod container, compared with `talosctl logs`.
- Layout tests at 70-120 columns.

## Decided

1. Exited containers are hidden by default; `A` shows them.
2. ~~Pods folded, one row per pod~~ and ~~the logs pane always visible~~:
   built that way first, then changed after trying it: the list is one row
   per container (POD and CONTAINER columns, `1`-`7`), and Enter focuses a
   container, its row and its logs (Esc back), as Workloads does. A pod's
   merged logs stay in Workloads.
3. Logs show the date (`MM-DD`) when the lines read span more than one day
   (both views).
