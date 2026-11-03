---
paths:
  - "crates/talos9s-tui/src/components/workloads.rs"
  - "crates/talos9s-tui/src/components/pod_logs.rs"
  - "crates/talos9s-core/src/workloads.rs"
  - "crates/talos9s-core/src/pod_logs.rs"
  - "crates/talos9s-tui/src/components/diagnostics/k8s.rs"
  - "crates/talos9s-tui/src/kube_list.rs"
---

# Workloads: what's broken, and where

One list of unhealthy Deployments, StatefulSets, DaemonSets and pods, worst
first, with the nodes their failures are on; a detail pane below; Enter
focuses a workload and shows its pods' logs. Keys are in README.md. The
design history is in git (`plans/workloads.md`, removed once built).

- **Health from status, not events.** Workload health comes from replicas,
  a pod's issue from its container states (`PodFacts::issue`). An event is
  shown only for a pod failing now, only its latest, and never decides
  health, so an old event can't make a healthy workload look broken.
- **Finished pods are fine.** Phase Succeeded is its own state, as in
  kubectl, k9s and Lens: READY stays `0/1`, the state reads `Completed`,
  dimmed, never a problem, and the READY sort puts it after ready pods
  (`PodFacts::is_completed`). A Job's failed or finished pods aren't
  problems either: the Job retries with new pods.
- **Restarts count when recent** (`HIGH_RESTART_THRESHOLD` within
  `RECENT_RESTART_SECS`); a Pending pod within `POD_PENDING_GRACE_SECS` is
  not a problem.
- **NODES never guesses:** an unhealthy workload shows its not-ready pods'
  nodes, a healthy one where its pods run (dimmed), a pod its
  `spec.nodeName` or `-` when unscheduled, a Deployment `?` when ReplicaSets
  couldn't be read (its pods can't be tied to it).
- **Each list stands alone.** Deployments, StatefulSets, DaemonSets, Pods
  and ReplicaSets are read together in `WORKLOADS_FETCH_TIMEOUT`, each
  result kept apart: a failed list is `?` in the summary with
  `Couldn't read …`, the rest works. Only when nothing could be read is it
  an error for the whole view.
- **Events:** per failing pod by UID, at most `WORKLOAD_EVENTS_MAX`, in
  their own `WORKLOADS_EVENTS_TIMEOUT` after the lists.
- **Pod → workload** through owner references: Pod → ReplicaSet →
  Deployment, Pod → StatefulSet, Pod → DaemonSet (`pod_workload`). The
  logic is in talos9s-core and unit-tested; the component fetches and
  draws.
- **Never changes anything.** No edit, scale, restart or delete; commands
  (`kubectl describe`, `logs --previous`, `rollout status`) are copied.
- **Dropped on purpose:** a node grid (`M`), Enter jumping to the pod's
  node in the tree, `L` for its kubelet logs, metrics. The NODES column,
  the pod list sorted by NODE and the focused view answer "is it one bad
  node?"; jumps replaced the view being read.

## Kubernetes client

- **Which kubeconfig** (`create_k8s_client_with_source`): the one served by
  a pinned control plane node, or a `KUBECONFIG` context with the same CA.
  With `--kubeconfig` (`TALOS9S_KUBECONFIG`) only that file is used, its
  context picked by CA (`context_for_ca`), and nothing falls back to the
  admin kubeconfig: no matching context, an unverifiable CA or a 403 is
  shown as an error. A context for another cluster is never used.
- **Error text:** show `kube::Error` with `kube_list::error_text` (the API
  server's message); its `Display` appends the whole `Status` dump.

## Not built yet

- Jobs and CronJobs: failed last run, suspended, missed schedules.
- The same list → detail shape for the Volumes view.
- PodDisruptionBudgets in the detail pane (Lifecycle reads them already).
