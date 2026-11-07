# Design: rolling Talos and Kubernetes upgrades

Status: draft v2 for review · Author: Olexiy Buyanskyy · 2026-11-07
(v1 2026-11-01; v2 adds what tuppr's code taught us and the custom logic)

## Summary

talos9s upgrades Talos on a cluster's nodes one at a time, then Kubernetes,
from the operator's machine with the operator's talosconfig. Every node is
gated on the cluster's current state: etcd, PDBs, and the storage talos9s
already reads (LINSTOR/DRBD, ZFS). Site logic, such as "no maya session on
the node" or "clear ClickHouse's DNS cache after a peer moved", comes from
the user as declarative checks or as hook programs at fixed points of the
rollout. Every step is on screen, in a run file and in the audit log; a
failure stops the rollout where it is, says why, and leaves the node as it
was or cordoned with the rollback command shown.

## Why

Upgrades run today through [tuppr](https://github.com/home-operations/tuppr),
an in-cluster controller (on kprod: `kolmena-infra
clusters/kprod/platform/32-tuppr`, the olexiyb/tuppr `integration` build with
seven of our PRs). It works, and the 2026-10-31 kprod roll went without a
dropped session, but:

- **It needs `os:admin` from inside the cluster.** `kubernetesTalosAPIAccess`
  writes an os:admin talosconfig into a Secret in `system-upgrade`: whoever
  can read Secrets or run a pod there is Talos admin of every node. The
  hooks' ServiceAccount (exec into ClickHouse and the Sentinels, whose env
  holds Redis passwords) lives in the same namespace.
- **Its logic keeps needing patches,** each a Go PR upstream or a fork:
  health before each batch (#668), one control-plane node at a time (#669),
  a rebooting node's verify errors (#671), upgrade path validation (#672),
  pre-batch hooks (#681), post hooks after a failure (#682), non-blocking
  health checks (#683).
- **It can't see storage.** Its health checks are CEL on Kubernetes objects,
  with no `now()`. kprod works around it: a CronJob (`27-drbd-health`)
  writes DRBD state into a ConfigMap, and the `piraeus` hook checks that the
  ConfigMap is fresh and newer than the last node Ready. talos9s reads DRBD
  and LINSTOR directly.
- **It's hard to watch.** Progress is a `status.phase` and Job logs that are
  deleted after 300 s (kept only in ClickStack). Reading why a run waits
  means `kubectl get talosupgrade -o yaml` and knowing tuppr's internals.

An upgrade run from outside the cluster, by a person at a terminal, is
visible while it happens, uses no in-cluster admin rights, and keeps the
site logic in files the operator owns.

## Goals

1. Upgrade Talos across a cluster without losing etcd quorum, violating a
   PDB, or rebooting a node while its storage is unsafe to lose.
2. Stop before touching a node when anything is wrong, with the reason; a
   failure after the node was touched leaves it cordoned with the next step
   shown, never half-done and silent.
3. Let the user add their own logic, gates and actions, without changing
   talos9s, and port kprod's four hooks and two health checks.
4. Resume an interrupted rollout from where it stopped.
5. Keep each node's extensions (same schematic, new version).
6. Show what happens: per-node step, the node's own progress messages,
   gate results with their reason, hook output, an audit trail.
7. Upgrade Kubernetes as a separate, explicit step.

## Non-goals (first version)

- Choosing versions for the user. The target is given; talos9s validates it.
- Running inside the cluster or as a daemon (see
  [Unattended runs](#unattended-runs)).
- Rolling back automatically. A failed node is reported with the rollback
  command; the operator decides.
- Writing machine configs. The config is the source of truth for the
  target image; whatever writes it (tofu, talhelper, Omni, `talosctl
  apply`) stays its owner (Decided #2).
- Running inside `tofu apply` or another config tool (Decided #3).

## What tuppr does, and what we keep

From tuppr's source (`integration` branch, read 2026-11-07). The parts that
carry over, with the bug each one fixed:

| tuppr behavior | Keep? | Why |
|---|---|---|
| A node is done when its Talos `Version` reports the target; the upgrade Job's exit code is ignored | yes | The reboot kills the process that started it; only the node's own answer counts |
| Verify errors while rebooting mean "still rebooting" until a durable deadline (#671; 1.14 answers `PermissionDenied` for a while) | yes | Otherwise a normal reboot reads as a failure |
| Control-plane nodes one at a time, and never while another control-plane node is upgrading (#669) | yes | `LifecycleService.Upgrade` checks neither etcd nor parallel upgrades |
| Cluster health before every node and after the last (#668): nodes Ready, `MachineStatus` running+ready, etcd members = control-plane nodes, no learners, a leader, IDs match | yes, extended | Finishing while the last node still rejoins reported success too early |
| Upgrade path and Kubernetes support checked per node (#672, Talos machinery's `UpgradeableFrom` / `SupportedWith`) | yes | Built-in table, same rules |
| Image: the node's `machine.install.image` with the tag replaced; refuse when it doesn't carry the running schematic; factory URL for a generic installer with extensions | yes | Reinstalling the generic installer wipes extensions; `ghcr.io/siderolabs/installer` stopped at 1.14 |
| After success, write the new `install.image` into the machine config (apply, no reboot) | no | The config owner writes the intent first (kprod: `talos_version` in tofu, applied with no reboot); talos9s writing it would make tofu's state drift (Decided #2) |
| Pre-pull the installer on every node before the first cordon | yes (stage) | Finds a missing image on all nodes at once, shortens each node's downtime. Warm the Image Factory first (90-155 s for a new build) |
| Drain: skip DaemonSet, mirror and finished pods; PDB-aware; wait for VolumeAttachments to detach | yes | talos9s's drain already follows kubectl's rules |
| A failed drain uncordons and retries later | stop instead | A person is watching: say why and stop |
| Pre-batch hooks: retried every interval until a timeout; a pass goes stale after 5 min or when the batch changes | yes | The sessions gate needs hours; a stale pass must not let a node through |
| Post hooks run after a failure too (#682) and never change the outcome | yes | Clean-up steps must run when things went wrong |
| Health checks wait, never fail the run (#683) | yes, with a timeout | Waiting forever unattended is fine; at a terminal, a timeout that stops and explains is better |
| Maintenance windows, silences, notifications, metrics | silences only, opt-in | A person starts the run, so no window. Silences keep #prod-alerts quiet for planned reboots |
| `talosctl` subprocess in a Job, matched to the node's version | no | Call the Talos API directly (below); no version-matched binary |

Still open in tuppr, and closed here: the control-plane race across plans
(talos9s holds a Lease), the path checked only at admission (talos9s checks
it in the plan and again before each node), a Talos rollback not detected
(talos9s sees the old version after the reboot and says "rolled back").

## How a Talos upgrade works (1.14)

`talosctl upgrade` (1.14) is:

1. `ImageService.Pull`: pull the installer into the node's system containerd.
2. `LifecycleService.Upgrade`: write the new version to the inactive boot
   partition, streaming progress and finally an exit code (non-zero means
   failed; the RPC itself doesn't error).
3. Drain if asked, then `MachineService.Reboot`.
4. Wait for the node to come back, uncordon.

Steps 1 and 2 don't disrupt the node: a missing image or a full disk fails
before any drain. The old version stays on the other partition;
`MachineService.Rollback` boots it again.

talos9s calls these APIs over gRPC. `LifecycleService` and `ImageService`
aren't in `crates/talos-rs/proto` yet: vendor them from Talos 1.14 and
regenerate (`--features regenerate`). Nodes below 1.13 have only the
deprecated `MachineService.Upgrade`; the first version refuses them.

## The rollout

### 1. Plan

Input: context, optionally a node selection and an order. The target is
each node's `machine.install.image` from its machine config: the config
owner has already written it. `--talos vX.Y.Z` names the expected version:
nodes whose config already says so roll; a node whose config says
otherwise is a plan error unless `--allow-drift`, which swaps the tag for
that run and reports the drift (the next config apply would write the old
image back). Output: a table of nodes (current version, target image, order) and
the result of every plan-time check. Any failure stops before anything
changes.

| Check | Source |
|---|---|
| The target is a valid upgrade from each node's version (no skipped minor, no downgrade) | Version API, built-in table |
| The cluster's Kubernetes version is supported by the target | Built-in table |
| Each node's install image carries the schematic it runs | Machine config, ExtensionStatus |
| Each node's configured image matches `--talos`, or the drift is allowed and shown | Machine config |
| The target image exists for every schematic | Registry HEAD |
| No other upgrade runs: the talos9s Lease is free, no tuppr `TalosUpgrade`/`KubernetesUpgrade` active or unsuspended | Kubernetes API |
| Every node reachable and on 1.13+ | Version API |
| The upgrade config parses; every hook exists and is executable | Local files |
| Every check the config defines passes once, in dry-run (gates only) | Config |

Order: workers first by default (a canary that hurts least), then control
plane one at a time with the etcd leader last. The plan can be reordered;
kprod's last run went kpw1, then kpd1-3, then the rest.

The plan is shown in full: nodes, order, every gate and hook that will run
and when, the actions hooks will take. Starting needs a `y`/`n`
confirmation.

### 2. Stage (default on)

Pull the target image on every node before cordoning any. Pulling doesn't
change the node.

### 3. Per node

```
gate ─► write ─► cordon ─► after-cordon ─► drain ─► reboot ─► wait ─► verify ─► uncordon ─► post-node ─► settle
  │       │        │            │            │                  │        │
  └───────┴────────┴────────────┴────────────┴── stop ──────────┴────────┘
```

**Gate.** On fresh state, right before the node. Waits while a check fails,
until its timeout; then the rollout stops. The node is untouched.

| Built-in check | Why |
|---|---|
| Cluster health as in tuppr #668 (nodes Ready, MachineStatus, etcd members/leader/learners) | A second sick node, or a broken etcd, before we take one away |
| etcd quorum holds with this node gone (control plane) | `talos9s_core::etcd::EtcdQuorum`, already used by reboots |
| PDBs allow evicting this node's pods | The drain would stop |
| DRBD: no resync running, no out-of-sync bits toward a Primary (LINBIT/drbd#153), no StandAlone link, every replica UpToDate | The failure modes we've hit (2026-10-17, 2026-10-31) |
| LINSTOR: every satellite online | A replica that can't reconnect after the reboot |
| ZFS: pools healthy on this node | Don't reboot a degraded pool |
| No node cordoned or NotReady except by this rollout | Something else is wrong |
| The config's checks and `pre-node` hooks | Site logic |

The storage checks read current state (tier 1/2), so the drbd-health
ConfigMap and its freshness workaround aren't needed.

**Write.** `LifecycleService.Upgrade` with the node's target image, its
messages streamed into the progress view; non-zero exit code stops. The
node keeps running its old version.

**Cordon, after-cordon, drain.** Cordon, then the `after-cordon` checks and
hooks (kprod's sessions: wait until no session pod is left; new ones
already start elsewhere), then the existing PDB-aware drain and the
VolumeAttachment wait. A failed drain uncordons and stops.

**Reboot.** An etcd leader forfeits leadership first
(`EtcdForfeitLeadership`), then `MachineService.Reboot`. The reboot
deadline goes into the run file before the call.

**Wait and verify.** Errors while the node is down count as "rebooting" up
to the deadline (default 15 min). Verified when all hold: Talos reports the
target version (the old version means it rolled back: stop and say so);
Talos services healthy; the Kubernetes node Ready with the expected
kubelet; control plane: its etcd member healthy and caught up; LINSTOR
satellite online and its DRBD replicas connected (resyncs may run).
On timeout: stop, leave the node cordoned, show what didn't come up and the
rollback command (`talosctl -n <node> rollback`), never run it.

**Uncordon, post-node, settle.** Uncordon, run `post-node` hooks (kprod's
ClickHouse cache drop and Sentinel restart), then wait until the gate's
cluster-wide checks pass again: DRBD resyncs started by this reboot finish
here. On storage nodes this is where a rollout spends its time.

After the last node: cluster health once more, `post-run` hooks, done.

### 4. Kubernetes

A separate action after Talos, never automatic: `talosctl upgrade-k8s --to
<version>`, first `--dry-run` shown for review, its output streamed. Done
when every kubelet and the API server report the target (tuppr's rule).
Re-implementing upgrade-k8s isn't worth it; talos9s adds the gate before
and the verify after.

## Your own logic

Three layers, from least to most power. A site needs code only for what
the first two can't express.

### Built-in checks

What talos9s knows: etcd, PDBs, DRBD/LINSTOR, ZFS, CNPG (every Cluster has
`readyInstances == instances`, hibernated ones exempt). Each can be turned
off per cluster, never silently: the plan lists what's off.

### Declarative checks

Checks on Kubernetes objects, in the upgrade config, no code. The syntax is
tuppr's, so existing checks copy over:

```yaml
checks:
  - name: cnpg-ready
    when: [pre-node, settle]
    apiVersion: postgresql.cnpg.io/v1
    kind: Cluster
    expr: >-
      object.metadata.annotations["cnpg.io/hibernation"] == "on" ||
      object.status.readyInstances == object.spec.instances
    timeout: 30m
```

- `apiVersion`, `kind`, optional `namespace`, `name`, `labelSelector`;
  every matching object must pass; a selector matching nothing fails (as
  in tuppr).
- `expr` is CEL with `object`, plus what tuppr lacks: `now`, `node` (the
  node being upgraded), `lastNodeReady` (the newest Ready transition of a
  node in the rollout), so "fresh since the reboot" is expressible.
- Two shorthands for the common cases: `podsReady: {selector,
  namespaces}` and `noPodsOnNode: {selector}` (kprod's sessions gate).
- `when`: the points it runs at; `timeout` (wait until, then stop) and
  `interval`.
- A failing check shows its `description` and which objects failed.

CEL in Rust: the `cel` crate (cel-rust) or similar; checked against
`cargo deny` before choosing.

### Hooks

Programs on the operator's machine, for logic a check can't express:
anything that acts (exec into a pod, restart something, cordon), or reads
something other than Kubernetes.

```yaml
hooks:
  - name: sessions
    when: after-cordon
    kind: check
    needs: [kubernetes]
    run: [hooks/with-api.sh, python3, hooks/hooks.py, sessions]
    interval: 1m
    timeout: 3h
  - name: clickhouse-dns
    when: post-node
    kind: action
    needs: [kubernetes]
    run: [hooks/with-api.sh, python3, hooks/hooks.py, clickhouse]
    env: { WAIT_S: "1800" }
    timeout: 32m
```

- **Points:** `pre-run`, `pre-node`, `after-cordon`, `post-node`,
  `post-run`, `on-failure`.
- **Kinds.** A `check` only reads: it may run any time, also in the plan's
  dry-run, and is retried every `interval` until it passes or `timeout`
  stops the rollout. An `action` changes something: it runs only at its
  point in a confirmed rollout, the plan lists it, a failure stops the
  rollout (`post-run` and `on-failure` failures are shown and don't change
  the outcome, as tuppr #682).
- **Contract.** Exit 0 passes, non-zero fails; stdout and stderr are
  streamed into the progress view and the run file (masked); the last line
  is the reason shown. Environment: `TALOS9S_CONTEXT`, `TALOS9S_STEP`,
  `TALOS9S_NODE`, `TALOS9S_NODE_IP`, `TALOS9S_NODES` (all nodes of the
  rollout), `TALOS9S_TARGET`, `TALOS9S_DRY_RUN=1` in the plan, and
  `TUPPR_NODES` (= the node) so kprod's script runs unchanged. Run without a
  shell, from the config file's directory, with a timeout always.
- **Kubernetes access for hooks: the operator's own kubeconfig, never
  admin.** A hook that declares `needs: [kubernetes]` gets `KUBECONFIG` and
  `KUBECONFIG_CONTEXT` pointing at the operator's kubeconfig (`--kubeconfig`,
  or the `KUBECONFIG` context matched by CA). It acts as the operator: their
  RBAC, their name in the Kubernetes audit log. Without such a kubeconfig the
  plan refuses those hooks before anything runs ("hook clickhouse-dns needs
  your own kubeconfig: pass `--kubeconfig` or add a context for this
  cluster"); built-in and declarative checks still work, they run inside
  talos9s. The admin kubeconfig fetched from Talos never reaches a hook:
  not as a file, not through a proxy. Decided 2026-11-07, see
  [Decided](#decided).
- **`API_URL` hooks** (kprod's `hooks.py` reads `API_URL`, not a
  kubeconfig) run through a wrapper that starts `kubectl proxy` on the
  operator's kubeconfig for the hook's lifetime, as its laptop dry run does
  today:

  ```sh
  #!/bin/sh
  # hooks/with-api.sh: API_URL for a hook, as the operator, for its lifetime
  out=$(mktemp) || exit 1   # not a fixed name: /tmp may be shared
  kubectl proxy --context="$KUBECONFIG_CONTEXT" --port=0 --accept-hosts='^127\.0\.0\.1$' \
    > "$out" 2>&1 &
  proxy=$!; trap 'kill $proxy 2>/dev/null; wait $proxy 2>/dev/null; rm -f "$out"' EXIT
  until port=$(sed -n 's/.*127.0.0.1:\([0-9]*\).*/\1/p' "$out"); [ -n "$port" ]; do
    kill -0 $proxy 2>/dev/null || { cat "$out"; exit 1; }   # kubectl proxy failed
    sleep 0.2
  done
  API_URL="http://127.0.0.1:$port" "$@"
  ```

  The proxy is an unauthenticated local port, but only while the hook runs
  and only with the operator's rights. If many hooks need it, talos9s can
  start it for them later (`api: proxy` on a hook); not in the first
  version.
- **Never remediation by talos9s.** CLAUDE.md's rule stays: talos9s's own
  fix suggestions are shown, never run. A hook is the operator's program,
  configured by them and confirmed in the plan; CLAUDE.md's rule gets a
  sentence saying so.

### Where the config lives

`--upgrade-config <file>` (default `~/.talos9s/upgrades/<context>.yaml`).
The file and its hooks belong in the GitOps repo next to the cluster
(`kolmena-infra/clusters/kprod/upgrade/`), reviewed like the tuppr
manifests are today. The plan and the run file record the file's path and
SHA-256, so a run shows which logic it ran with.

```yaml
# kolmena-infra/clusters/kprod/upgrade/talos9s.yaml
nodes:
  selector: kolmena.ai/provider=hetzner-robot
  order: [kpw1, kpd1, kpd2, kpd3, kpc1, kpc3, kpc2, kpg1]
timeouts: { reboot: 15m, drain: 10m, settle: 1h }
silences:
  alertmanager: http://vmalertmanager.monitoring.svc:9093  # through the API proxy
  matchers: ['alertname=~"KubeNodeNotReady|PiraeusNodeOffline|..."']
  maxDuration: 3h
checks: [...]
hooks: [...]
```

### kprod, ported

| tuppr today | talos9s |
|---|---|
| healthCheck `drbd-health` ConfigMap (CEL) | built-in DRBD/LINSTOR checks, read from the nodes |
| healthCheck CNPG Clusters | built-in CNPG check (or the declarative one above) |
| hook `piraeus` (freshness after the last Ready) | not needed: built-in checks read current state |
| hook `sessions` (cordon, wait for sessions) | declarative `noPodsOnNode` at `after-cordon`, timeout 3h; talos9s cordons |
| hook `clickhouse` (drop DNS/connection caches) | `post-node` action, same script |
| hook `sentinels` (restart a spinning Sentinel) | `post-node` action, same script, until redis#15917 ships |
| `silences` | `silences`, created while the run lasts, expired at the end |
| `maintenance` window | none: a person starts the run |
| `nodeSelector` hetzner-robot | `nodes.selector` |

The `system-upgrade` namespace keeps only what's still needed there:
nothing, once tuppr goes. The hooks' exec rights move to the operator's own
RBAC.

## Visibility

- **Upgrade view** (TUI): one row per node with step, elapsed time and last
  message; below, the selected node's stream (Talos upgrade messages, gate
  results with reasons, hook output). A waiting gate says what it waits
  for and until when ("sessions: 2 on kpw1, timeout in 2h41m").
- **Run file** (`~/.talos9s/operations/`, as for other operations): every
  step, check result and hook line, masked, flushed per line; it's what a
  resume reads.
- **Audit log:** start, each node's write/reboot/verify, stop, end.
- **Kubernetes:** the Lease names the holder (user, host) and the step, so
  a colleague's `kubectl get lease -n kube-system talos9s-upgrade` shows an
  upgrade is running and where. An event on each node (`Upgrading`,
  `Upgraded`, `UpgradeFailed`) shows in `kubectl describe node`.

## Stopping and resuming

- **Stop:** a failed check or step stops the rollout; `s` stops after the
  current step. Between cordon and uncordon, stopping says so and leaves
  the node cordoned.
- **Resume:** re-reads the cluster, skips nodes on the target and verified,
  and re-runs the current node from its gate. Every step is safe to repeat:
  a node already on the target skips write and reboot; a reboot deadline
  survives in the run file.
- **Lock:** a `Lease` (`kube-system/talos9s-upgrade`) held while running,
  renewed every 30 s. A second talos9s refuses to start; a stale lease
  (holder gone > 2 min) can be taken after confirmation.
- **Quitting** waits for the current step, as for other operations.

## Integration with config tools

talos9s never runs inside `tofu apply` (Decided #3). The two meet through
the cluster:

1. tofu (or any config tool) bumps the version and applies the machine
   configs with no reboot. Quick, fits tofu's model, as kprod does today.
2. talos9s rolls each node to the image its config names, with gates,
   hooks and the live view. It doesn't need to know who wrote the config.

Without config management, `--talos` with `--allow-drift` swaps the tag
and reports the drift. A one-command flow for tofu users is a recipe, not
code: a Makefile target in the GitOps repo running `tofu apply`, then
`talos9s upgrade run`.

## Interfaces

**TUI:** `U` on a cluster opens the upgrade view: target version (prefilled
with the newest patch, and the next minor), config file, the plan with
check results, then progress. Keys: start (confirm), stop after step,
resume, reorder in the plan, open a node's logs.

**Headless**, for a jump host or CI, same engine:

```
talos9s upgrade plan   --context kprod --talos v1.14.6 [--upgrade-config F]
talos9s upgrade run    --context kprod --talos v1.14.6 [--yes]
talos9s upgrade resume --context kprod
talos9s upgrade status --context kprod
talos9s upgrade attach --context kprod
talos9s upgrade k8s    --context kprod --to v1.37.2 [--yes]
```

Human-readable by default, `--output json` for one JSON event per line.
Exit 0 only when every node verified.

**Attach:** watch a rollout something else started (CI, a jump host, a
colleague). `upgrade attach`, or the TUI's upgrade view opening on its own
when the Lease shows a running rollout, follows its progress from the
Lease (holder, node, step) and, on the same machine, the run file's
events. Read-only from another machine; stop and resume only where the run
file is.

## Unattended runs

tuppr starts a rollout when Renovate merges a version bump. Options:

1. Keep tuppr installed but suspended, for clusters without storage, until
   2 or 3 exist.
2. A CI job on the GitOps repo runs `talos9s upgrade run --yes` when the
   version changes; the talosconfig lives in CI secrets, not the cluster.
3. A talos9s controller mode: out of scope, it brings back in-cluster admin
   rights.

Recommended: 2 later; for now a person runs it. Either way, anyone can
`attach` to watch.

## Security

- The operator's talosconfig and kubeconfig; no in-cluster Talos access.
  Once tuppr is gone on a cluster, `kubernetesTalosAPIAccess` and its
  os:admin Secret go too.
- Hooks run as the operator, from config the operator owns and reviews in
  git; the plan shows each one and its config hash before anything runs.
  The admin kubeconfig fetched from Talos never reaches a hook.
- Run files, logs and hook output are masked; no secrets in the config
  (hooks read their own, e.g. the Sentinel password from the pod env as
  today).
- `.claude/rules/security.md` gets the hook boundary: hooks are run without
  a shell, the admin kubeconfig never reaches them.

## Testing

- Unit (talos9s-core, pure): upgrade path and support rules; image tag
  replacement and schematic checks; gate decisions from recorded state
  (the 2026-10-17 and 2026-10-31 DRBD cases); "rebooting until deadline"
  and "rolled back" verdicts; resume from saved run files; CEL checks
  including `now`/`lastNodeReady`; hook pass/fail/timeout/staleness.
- Integration: QEMU clusters in `test-clusters/` (3 control plane + worker),
  between two patch releases; kill talos9s mid-reboot and resume; a hook
  that fails, one that times out, one that hangs.
- Live: kprod's kpw1 alone first (tuppr suspended), with the ported hooks in
  dry run, then for real; then the rest of kprod; then bc.
- No false positives: a gate that fails when made to fail (start a DRBD
  resync, run a session pod) and passes again after.

## Phases

1. **Plan and checks only** (`upgrade plan`, TUI plan view): reads
   everything, changes nothing. Useful on its own: "is it safe to upgrade
   now?" Includes the built-in gates and declarative checks.
2. **Single-node upgrade** from Operations (stage, write, drain, reboot,
   verify), protos vendored.
3. **Rolling run:** run file, resume, Lease, settle, headless `run`.
4. **Hooks and the config file;** port kprod's hooks; silences.
5. **Kubernetes** via upgrade-k8s.
6. Retire tuppr on kprod and bc; remove `kubernetesTalosAPIAccess`.

## Decided

1. **Hooks' Kubernetes access** (2026-11-07): the operator's own
   kubeconfig, required by hooks that declare `needs: [kubernetes]`; no proxy
   on the admin credentials. A proxy on admin would give every hook admin
   rights silently, log their changes under the admin certificate instead
   of the operator, open admin to any local process for the run, and need a
   reverse proxy for exec (WebSocket upgrades) inside talos9s. The operator
   already has a matching context on bc and kprod (client certificates).
   `API_URL` hooks use a `kubectl proxy` wrapper on the operator's
   kubeconfig.

2. **Install image** (2026-11-07): the machine config is the source of
   truth. Each node rolls to the image its config names; talos9s never
   writes the config in v1. `--talos` beyond the config is a plan error,
   or with `--allow-drift` a one-run tag swap with the drift reported.
   Writing it would make tofu's state drift and give two owners for one
   field.
3. **No tofu integration** (2026-11-07): talos9s doesn't run inside `tofu
   apply` (provider or `local-exec`). The apply would hold the state lock
   for the whole rollout (hours per node on kprod), die with its CI runner
   or ssh session, and hide the plan behind `--yes`; a failure halfway
   doesn't fit "converges or errors". tofu writes the intent, talos9s
   carries it out ([Integration](#integration-with-config-tools)); `attach`
   covers watching a run started elsewhere.

## Open questions

1. **Settle scope:** all DRBD resyncs cluster-wide, or only those touching
   the node just upgraded?
2. **Support table:** built in and updated with releases, or Talos
   machinery's data via a small build step?
3. **Worker batches** (tuppr's `parallelism`): worth it for our sizes, or
   always one at a time?
4. **Coexistence with tuppr:** refuse while any `TalosUpgrade` isn't
   suspended, or only while one is active?
