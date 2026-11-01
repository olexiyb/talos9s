# Design: rolling Talos and Kubernetes upgrades

Status: draft for review · Author: Olexiy Buyanskyy · 2026-11-01

## Summary

talos9s will upgrade Talos on a cluster's nodes one at a time (later in
batches for workers), and Kubernetes after it, from the operator's machine
with the operator's talosconfig. Every node is gated on the cluster's actual
state: etcd, PodDisruptionBudgets and the storage talos9s already reads
(LINSTOR/DRBD, ZFS). The same engine runs in the TUI and as a headless
command, records its progress so an interrupted rollout resumes, and never
needs Talos admin rights granted to anything inside the cluster.

## Why

Today upgrades run through [tuppr](https://github.com/home-operations/tuppr),
an in-cluster controller. It works, but:

- **It needs `os:admin` from inside the cluster.**
  `kubernetesTalosAPIAccess` gives a Kubernetes namespace full admin over
  every node; compromising that namespace's service account compromises the
  nodes.
- **Its safety logic is generic and keeps needing fixes.** Our own open PRs
  add waiting for cluster health before each batch (#668), one control-plane
  node at a time (#669), waiting out a rebooting node (#671), validating the
  upgrade path (#672), hooks before each batch (#681), post-hooks on failure
  (#682) and non-blocking health checks (#683).
- **It can't see storage.** Its health checks are CEL expressions on
  Kubernetes objects; a DRBD resync, out-of-sync bits toward a Primary
  (LINBIT/drbd#153) or a StandAlone link are invisible to it, and those are
  exactly what made past node reboots dangerous here.
- **Custom logic means a Go PR upstream** or carrying a fork.

talos9s already reads all of that state and is ours to change.

What tuppr does that this design does not replace on day one: starting a
rollout unattended when Renovate merges a version bump (see
[Unattended runs](#unattended-runs)).

## Goals

1. Upgrade Talos across a cluster without losing etcd quorum, violating a
   PDB, or rebooting a node while its storage is unsafe to lose.
2. Stop before touching a node when anything is wrong, with the reason, and
   leave the cluster as it was (node uncordoned, nothing half-done).
3. Resume an interrupted rollout from where it stopped.
4. Keep each node's extensions: upgrade to the same installer image (same
   schematic) at the new version.
5. Show what is happening: per-node step, the node's own progress messages,
   and an audit trail.
6. Upgrade Kubernetes as a separate, explicit step.

## Non-goals (first version)

- Choosing versions for the user. The target is given; talos9s validates it.
- Running inside the cluster or as a daemon.
- Rolling back automatically. A failed node is reported with the rollback
  command; the operator decides.
- Changing machine configs (beyond what `upgrade-k8s` does) or schematics.

## How Talos upgrades work (1.14)

Talos 1.13 added `LifecycleService` and deprecated
`MachineService.Upgrade` (removal planned for 1.18). `talosctl upgrade`
(v1.14.5) does:

1. `ImageService.Pull`: pull the installer image into the node's system
   containerd.
2. `LifecycleService.Upgrade`: write the new version to the inactive boot
   partition, streaming progress messages and finally an exit code
   (non-zero = failed; the RPC itself doesn't error).
3. Drain the node (if asked), then `MachineService.Reboot`.
4. Wait for the node to come back, uncordon.

Two properties matter for us:

- **Steps 1 and 2 don't disrupt the node.** The image is pulled and written
  while workloads keep running; a missing image, a schematic the factory
  can't build or a full disk fails here, before any drain.
- **The node keeps the old version on its other partition.**
  `MachineService.Rollback` boots it again.

For nodes on Talos < 1.13 the legacy `MachineService.Upgrade` is the only
path; the first version supports 1.13+ only (both our clusters run 1.14.5).

### Which image

Each node's machine config names its installer in `machine.install.image`,
e.g. on bc `factory.talos.dev/metal-installer/<schematic>:v1.14.5` and on
kprod `factory.talos.dev/installer/<schematic>:v1.14.5`. The target image is
that reference with only the tag replaced by the target version, which keeps
the registry path and the schematic, and with it the node's extensions.

Before planning, talos9s compares that schematic with the one the node runs
(the `schematic` extension in ExtensionStatus). If they differ, someone
changed the config without upgrading; talos9s stops and says so instead of
guessing which one is wanted.

## The rollout

### 1. Plan

Input: cluster (context), target Talos version, and optionally a node
selection. The plan lists each node, its current version, its target image,
and the order:

- control-plane nodes first, one at a time, the etcd leader last;
- then workers, one at a time (batches later, see open questions).

Plan-time checks, any failure stops before anything changes:

| Check | Source |
|---|---|
| Target is a valid upgrade from every node's version: no skipped minor (1.12 → 1.14 must go through 1.13), not a downgrade | Version API |
| The cluster's Kubernetes version is supported by the target Talos | Built-in support table |
| Every node's installer schematic matches what it runs | Machine config, ExtensionStatus |
| No other upgrade is running: no talos9s lock held, no tuppr `TalosUpgrade`/`KubernetesUpgrade` in progress | Kubernetes API |
| All nodes reachable and on 1.13+ | Version API |

The plan is shown in full and needs an explicit confirmation (TUI dialog, or
`--yes` headless).

### 2. Stage (optional, recommended)

Pull the target image on every node before rebooting any
(`ImageService.Pull` only). This finds a missing image or an extension that
doesn't exist for the target version on all nodes at once, and makes each
node's later downtime shorter. Pulling doesn't change the node.

### 3. Per node

```
gate ─► write ─► drain ─► reboot ─► wait ─► verify ─► uncordon ─► settle ─► next
  │       │        │                  │        │
  └───────┴────────┴──── stop, uncordon if cordoned, report ───────────────┘
```

**Gate** (checked right before the node, on fresh state):

| Check | Why |
|---|---|
| etcd: every member healthy, and quorum holds with this node gone (control plane) | A second down member loses quorum |
| PDBs allow evicting this node's pods | The drain would hang or break an app |
| DRBD: no resync running anywhere in the volumes this node holds | A reboot mid-resync leaves fewer good copies |
| DRBD: no out-of-sync bits toward any Primary (LINBIT/drbd#153), no StandAlone link, every replica UpToDate | The failure modes we've hit |
| LINSTOR: every satellite online | A replica that can't reconnect after the reboot |
| ZFS: pools healthy on this node | A degraded pool shouldn't be rebooted |
| No node in the cluster cordoned or NotReady, except by this rollout | Something else is already wrong |
| Custom pre-node hooks pass (see [Hooks](#hooks)) | Site-specific rules |

**Write:** `LifecycleService.Upgrade` with the node's target image; stream
its messages into the progress view; non-zero exit code = stop. The node is
untouched if this fails.

**Drain:** cordon and drain with the existing PDB-aware drain (timeouts,
DaemonSets ignored). A failed drain uncordons and stops.

**Reboot:** on a control-plane node that leads etcd, forfeit leadership
first (`EtcdForfeitLeadership`), then `MachineService.Reboot`.

**Wait:** for the node to go away and come back on the API, with a timeout
(default 15 min). The node's kernel log and service states are shown live.

**Verify** (all must hold, polled until a timeout):

- Talos reports the target version;
- all Talos services healthy;
- the Kubernetes node Ready, kubelet on the expected version;
- control plane: its etcd member healthy and caught up;
- LINSTOR satellite online, its DRBD replicas connected; resyncs it starts
  are allowed to run (they're expected after a reboot).

If verify times out: stop, leave the node cordoned, report what didn't come
up and the rollback command (`talosctl -n <node> rollback`), never run it.

**Uncordon**, then **settle:** wait until DRBD resyncs touching this node
finish and the gate's cluster-wide checks pass again before the next node.
This is where a rollout spends most of its time on storage nodes, and why
it's the step tuppr lacks.

### 4. Kubernetes

A separate command/action after Talos, never automatic: run
`talosctl upgrade-k8s --to <version>` (first with `--dry-run`, shown for
review), streaming its output. Re-implementing upgrade-k8s (static pods,
manifests, kubelet rollout) isn't worth it; talos9s adds its gate before and
the verify after (all nodes Ready on the new kubelet, control-plane pods
healthy).

## Stopping and resuming

State is a file per rollout: `~/.talos9s/upgrades/<cluster>/<id>.json`,
written after every step (the plan, each node's status and timestamps, the
last error). It holds no secrets: versions, image references, node names.

- **Stop:** any failed check or step stops the rollout; `Ctrl+C` / `s`
  stops after the current step (never between drain and uncordon without
  saying so).
- **Resume:** reopening the rollout re-reads the cluster, skips nodes
  already on the target version and verified, and re-runs the current node
  from its gate (every step is safe to repeat: write is idempotent, a node
  already on the target skips write and reboot).
- **Lock:** a Kubernetes `Lease` (`kube-system/talos9s-upgrade`) held while
  running, renewed every 30 s, so a second talos9s, on this machine or
  another, refuses to start. A stale lease (holder gone > 2 min) can be taken
  over after confirmation.

Every step also goes to the audit log (`~/.talos9s/audit.log`).

## Interfaces

### TUI

From the cluster view: `U` on a cluster header opens the upgrade view:
target version input (prefilled with the newest patch of the current minor,
and the next minor), the plan with check results, then live progress: a row
per node (step, elapsed, last message), the selected node's streamed output
below. Keys: start/confirm, stop after step, resume, open node logs.

### Headless

```
talos9s upgrade plan   --context bc --talos v1.14.6
talos9s upgrade run    --context bc --talos v1.14.6 [--stage] [--yes]
talos9s upgrade resume --context bc
talos9s upgrade status --context bc
talos9s upgrade k8s    --context bc --to v1.37.2 [--yes]
```

Human-readable progress by default, `--output json` for one JSON object per
event (for CI and notifications). Exit code 0 only when every node verified.

## Hooks

Executables run on the operator's machine with the context in environment
variables (`TALOS9S_CLUSTER`, `TALOS9S_NODE`, `TALOS9S_NODE_IP`,
`TALOS9S_STEP`, `TALOS9S_TARGET`), configured per cluster:

- `pre-node`: non-zero exit stops before the node (like tuppr #681);
- `post-node`: after verify;
- `on-failure`: always runs when a rollout stops on an error (like #682).

Built-in checks cover what talos9s can read; hooks cover the rest (an app's
own readiness, a maintenance flag elsewhere).

## Configuration

Upgrades are the first feature needing settings that persist:
`~/.talos9s/config.yaml`, per cluster: timeouts, hooks, whether to stage,
the support table override. Optional: everything has a default, flags
override it.

## Unattended runs

Headless mode covers a person or a scheduled job starting a rollout (tmux on
a jump host, a CI job with a talosconfig secret). What it doesn't do is
react to a version bump merged in git the way tuppr does. Options, decided
later:

1. Keep tuppr for starting rollouts; talos9s is used for manual and
   storage-heavy clusters.
2. A CI job on the GitOps repo runs `talos9s upgrade run --yes` when the
   version file changes (the talosconfig lives in CI secrets, not the
   cluster).
3. A talos9s controller mode: out of scope; it would bring back the
   in-cluster admin rights this design avoids.

## Security

- Uses the operator's talosconfig and the kubeconfig fetched from it; no
  in-cluster Talos API access, so `kubernetesTalosAPIAccess` with `os:admin`
  can be removed once tuppr is retired.
- Never prints or stores secrets: state and logs hold versions, images and
  node names only.
- Every disruptive step is behind the plan confirmation; hooks run as the
  operator, from config the operator owns.

## Testing

- Unit: upgrade-path and support-table rules, image tag replacement and
  schematic checks, gate decisions from recorded cluster state (incl. the
  DRBD cases from the 2026-10-17 and 2026-10-31 incidents), resume logic on
  saved state files.
- Integration: the QEMU test clusters in `test-clusters/` (multi control
  plane), upgrading between two patch releases, killing talos9s mid-rollout
  and resuming.
- Live: kprod workers first, then bc, with `--stage` and one node at a time.

## Phases

1. **Plan and checks only** (`upgrade plan`, TUI plan view): no changes to
   nodes. Useful on its own as "is it safe to upgrade now".
2. **Single-node upgrade** from Operations (`o` → upgrade), gate + write +
   drain + reboot + verify.
3. **Rolling run** with state, resume, lock, staging, headless `run`.
4. **Kubernetes** via upgrade-k8s.
5. Hooks and config file; worker batches.

## Open questions

1. Worker batches: worth it for our cluster sizes, or always one at a time?
2. Settle step: wait for all DRBD resyncs cluster-wide, or only those
   involving the node just upgraded?
3. Where the Talos/Kubernetes support table comes from: built in and
   updated with releases, or fetched (Talos docs / Image Factory)?
4. Should the plan also offer the newest patch of the current minor when
   the target skips it (Talos recommends upgrading to the latest patch
   first)?
5. Coexistence with tuppr while both are installed: refuse when a tuppr
   upgrade exists at all, or only while one is in progress?
