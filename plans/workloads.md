# Design: Workloads view rework

Status: draft for review · Author: Olexiy Buyanskyy · 2026-11-02

## Summary

The Workloads view becomes one list of what is broken, workloads and pods
together, with a detail pane for the selected row and the nodes its failures
are on. It answers the question talos9s exists for, "is it one bad node?",
without a drill-down per namespace or per workload, and links each failure to
the node in the tree, where the Talos-side views are.

Ideas come from [Headlamp](https://headlamp.dev): a summary above the lists, a
detail page per resource showing its pods and events, related resources as
links, and its map view grouped by node. Editing resources (Headlamp scales,
edits and deletes) stays out: talos9s never changes cluster state.

## Why

Today (`crates/talos9s-tui/src/components/workloads.rs`):

- The first screen is a namespace list; Enter drills into one namespace and
  shows its Deployments, StatefulSets and DaemonSets, then its problem pods.
- A workload row says `1/3 ready` but not where. The `NODE` column is filled
  for problem pods only; a pod that runs but isn't ready, or a workload's
  pods spread over several nodes, has to be matched by eye.
- At cluster level ReplicaSets aren't fetched, so a Deployment's pods can't
  be tied back to it (`workloads.rs:378`).
- One failed list (say StatefulSets forbidden by RBAC) fails the whole view,
  against "degrade gracefully".
- The 15 s timeout is a literal in the component, not in
  `talos9s_core::constants`.

k9s gets to the node in two steps (`:deploy`, Enter, the pod list's NODE
column) and stops at Kubernetes. talos9s can go one step further: from the
pod to its node's Talos state, logs and storage.

## Goals

1. Show every unhealthy workload and pod in one list, worst first, with the
   nodes its failures are on.
2. Show why for the selected row without leaving the view: replicas,
   conditions, its pods with node and state, the latest event of each failing
   pod.
3. Make a node that many failures share obvious at a glance.
4. Jump from a failure to its node in the tree, and to that node's kubelet
   logs.
5. Keep working when part of the data can't be read: that part says unknown
   or shows its error, the rest stays.

## Non-goals

- Editing, scaling, restarting or deleting anything. Commands (`kubectl
  rollout restart`, `kubectl describe`) are shown and copied, never run.
- A browser for every resource kind. That is k9s's and Headlamp's job.
- Jobs and CronJobs in the first version (see [Later](#later)).
- Metrics (CPU and memory per pod); there's no metrics-server dependency.

## Layout

```
┌ Workloads · bc ─────────────────────────────────────────────────────────────┐
│ Deploy 41/42 · StatefulSet 12/12 · DaemonSet 9/10 · Pods 3 failing           │
│                                                                              │
│   KIND    NAMESPACE   NAME              READY   NODES        ISSUE           │
│ ● Deploy  api         gateway           1/3     bc3 +1       2 not ready     │
│ ● DS      kube-system cilium            3/4     bc3          1 not ready     │
│ ● Pod     api         gateway-7f9c-x2   0/1     bc3          CrashLoopBackOff│
│ ● Pod     media       transcoder-0      -       -            Pending 12m     │
│ ─────────────────────────────────────────────────────────────────────────── │
│ Deploy api/gateway · 1/3 ready · Progressing: ReplicaSetUpdated              │
│   POD               NODE  STATE              RESTARTS  LAST EVENT            │
│ ● gateway-7f9c-x2   bc3   CrashLoopBackOff   14        Back-off restarting   │
│ ● gateway-7f9c-k8   bc4   Running, not ready 0         Readiness probe failed│
│ ● gateway-7f9c-p1   bc1   Running            0                               │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Summary line

Ready/total per kind and the count of failing pods. Counts the view computes
today, shown first, so a healthy cluster reads as healthy in one line.

### The list

- Rows: unhealthy Deployments, StatefulSets and DaemonSets, and problem pods
  (the current `PodIssue` classification). Worst first (failing, degraded),
  then namespace, then name.
- `A` shows healthy workloads too (`a` stays auto-refresh, as in every
  view; `A` is "show all" there), saved in `state.yaml`.
- `/` filters on namespace, name and node. `S` groups the rows under
  namespace sections, the current layout for those who want it (`N` is
  the previous match after `/` in other views).
- A pod owned by a listed workload is shown under it (indented), not as a
  separate top-level row, so one broken Deployment is one entry.

### NODES column

- On an unhealthy workload: the nodes of its not-ready pods, sorted, cut to
  `bc3 +1` when narrow, in the health color.
- On a healthy workload: the nodes its pods run on, dimmed. (First drafted
  as empty; an all-healthy cluster then showed an empty column.)
- On a pod: its `spec.nodeName`.
- `-` for a pod that isn't scheduled (Pending without a node). Never a
  guess.
- Unknown (`?`) for Deployments when ReplicaSets couldn't be read, since
  their pods can't be tied to them.

### Detail pane

For the selected row, below the list, in the lower part of the view:

- Workload: kind, name, ready/desired, its conditions from status (e.g.
  `Progressing: ReplicaSetUpdated`, `Available: False`), then its pods with
  node, state, restarts and the latest event of each failing pod.
- Pod: phase, each container's state and last termination (reason, exit
  code, when), node, and its latest event.
- Copied commands (`y`, as in the etcd, Processes and Volumes views): `kubectl -n NS describe pod NAME`, `kubectl -n NS
  logs NAME --previous`, `kubectl -n NS rollout status deploy/NAME`.

### Node grid

`M` (map, Headlamp's name for it) swaps the list for a grid of the unhealthy workloads by node, built like
the services grid on the cluster dashboard
(`components/cluster/group_table.rs`): one row per workload, one column per
node, a dot per pod colored by state. Three workloads failing only in the
`bc3` column is the "one bad node" answer.

### Jumps

- `L` on a pod: open its node's kubelet and containerd logs (the Logs view,
  those services selected).
- Esc: back to the tree, as today.

## Data

All reads, each in the existing timeout (moved to
`talos9s_core::constants`):

| Read | When | Why |
|---|---|---|
| Deployments, StatefulSets, DaemonSets | always (as today) | rows and summary |
| Pods | always; per node when scoped (as today) | problem pods, workload pods |
| ReplicaSets (metadata only) | always (today: scoped only) | tie pods to Deployments |
| Events | only for failing pods, at most `WORKLOAD_EVENTS_MAX` (say 20) | the latest event per failing pod |

- Events: `fieldSelector=involvedObject.uid=<uid>` per failing pod,
  concurrent, inside the same timeout; or one namespace-scoped list filtered
  locally when many pods fail in one namespace. Pick by measuring on kprod.
- Each list's result is kept separately: a failed StatefulSets list shows
  "StatefulSets: forbidden" in the summary and the rest of the view works.
- Pod-to-workload mapping goes through owner references: Pod → ReplicaSet →
  Deployment, Pod → StatefulSet, Pod → DaemonSet.
- Mapping and classification go to talos9s-core (pure, unit-tested):
  workload health from replicas, a pod's issue from its status, the nodes of
  a workload's not-ready pods. The component keeps fetching and drawing.

## Principles applied

- **State over logs.** Health comes from the objects' status. An event is
  shown only for a pod failing now, and only the latest one, so an old event
  never makes a healthy workload look broken; events never decide health.
- **No false positives.** Restarts count only when recent (already the
  case: `HIGH_RESTART_THRESHOLD` with `RECENT_RESTART_SECS`). A pod that
  isn't scheduled shows `-`, not a node. A Pending pod within
  `POD_PENDING_GRACE_SECS` isn't a problem (as today).
- **Degrade gracefully.** Per-list errors, unknown NODES when ReplicaSets
  are missing, events optional.
- **Bound every call.** One timeout around the lists, events capped in
  count and in the same timeout.

## Scope

On a node, or the control plane or workers row, the view keeps today's
scoping: workloads with pods on those nodes, `x/y here` in READY, and only
those nodes' pods. The node grid shows only the scoped nodes.

## Implementation steps

1. talos9s-core: workload/pod classification and the pod → workload → nodes
   mapping, with tests (moved from `workloads.rs`, unchanged behavior).
2. Per-list results, ReplicaSets at cluster level, timeout in constants.
3. The flat list with the NODES column, `a`, `/`, `S`.
4. The detail pane, events for failing pods, copied commands. (Done: events per pod by UID, at most `WORKLOAD_EVENTS_MAX`, in their own `WORKLOADS_EVENTS_TIMEOUT` after the lists.)
5. Pods and their logs. (Done: Enter focuses a workload: the list shows it and its pods, the logs pane below the merged logs of the selected row: all pods, one pod's containers, or one container (Enter on a pod lists them). At most `POD_LOG_SOURCES_MAX` containers, `POD_LOG_TOTAL_LINES` shared, read again every `POD_LOG_FOLLOW_INTERVAL` while following. Selecting the node and its kubelet logs were dropped.)
6. The node grid (`M`).
7. README: the Workloads row and keys.

Each step is usable on its own; 1–3 already answer "where".

## Testing

- Unit tests in talos9s-core for classification and mapping, including a
  Deployment with pods on three nodes, one not ready; an unscheduled pod; a
  missing ReplicaSet.
- Live on bc and kprod:
  - **No false positives:** break a workload (a bad image tag on a test
    Deployment), check it and its node appear; fix it, check the row
    disappears and the summary is healthy again. Done only on a test
    namespace, with the user's go-ahead; talos9s itself never changes it.
  - **Degradation:** a kubeconfig without permission to list StatefulSets;
    an unreachable API (the view shows the error, nothing hangs).
  - **Stale events:** a pod that failed and recovered shows no event and no
    issue.

## Later

- Jobs and CronJobs (failed last run, suspended, missed schedules).
- The same list → detail shape for the Volumes view.
- PodDisruptionBudgets in the detail pane (Lifecycle reads them already).

## Open questions

1. Detail pane below the list, or to the right on wide terminals?
2. Should `a` (show healthy) be remembered in the state file, like other
   view toggles?
3. Is the grid worth it next to the NODES column, or later?
