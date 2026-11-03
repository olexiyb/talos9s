# Design: Operations view and run history

Status: built · Author: Olexiy Buyanskyy · 2026-11-03

## Summary

Operations get their own menu entry. It lists what can be run on the
selected node, group or cluster, and every run so far: when, where, what,
how it ended. Starting one shows the exact command in the confirmation
dialog, then its output live in a pane. Every run is kept on disk with its
output until the user cleans the history, and the history can be filtered
by node, operation and text.

## Why

Today:

- Operations are popups: `o` on a node (drain, reboot, shutdown), `O` on a
  group or cluster (rolling drain, rolling reboot), `R` in Network →
  Connections (restart a service). The wizard (`--insecure`) installs and
  bootstraps.
- Progress lives only in the popup. Close it and the output is gone; what
  is left is one line per step in the audit log (`audit.rs`), which no
  view shows.
- The user doesn't see what was done in terms they know (`talosctl`,
  `kubectl`), so can't compare with running it by hand or repeat it.
- Operations are scattered: `o`/`O` in the tree, `R` deep in a network tab.

Prior art: k9s runs plugins and shells in the real terminal and keeps no
history; lazygit has a command log panel; Rundeck, AWX and Semaphore keep
every job run with its target, output and status, filterable. This is the
last idea, in a terminal.

## Goals

1. One place to start operations and see their history: a menu entry.
2. Each run shows the equivalent command and streams its output.
3. Runs are kept (time, context, node(s), operation, command, exit status,
   duration, output) until the user cleans them.
4. History filter by node, operation, status and text; view a past run's
   output with the log pane (search, wrap, `V`/`y`).

## Non-goals

- **No embedded interactive shell.** A terminal emulator in the TUI
  (pseudo-terminal, `tui-term`) brings resizing, key passthrough and focus
  trouble for commands that aren't interactive. If a shell is wanted
  later, suspend the TUI and run it in the real terminal, as k9s does and
  as the external log viewer (`v`) already does.
- **Fixes stay copy-only.** Diagnostic fixes and storage remedies are not
  operations (`CLAUDE.md`, "never run remediation"). The menu holds only
  deliberate operations.
- No new operations in the first version besides cordon and uncordon
  (upgrade, reset, apply config come later, each with its own safety review).
- No sharing of history between machines.

## How operations run

The safety checks live in talos9s's code, not in `talosctl` or `kubectl`:
etcd quorum before each control-plane reboot, waiting for a new boot ID,
drain rules like kubectl's with PDB reporting (`.claude/rules/operations.md`).
Shelling out to `talosctl reboot` would lose them. So:

- **Operations keep running in-process**, as today. Each step writes a
  line to the run's output (`cordon bc2`, `evicting web/gateway-7f9c-x2`,
  `etcd quorum: 4/4 healthy, rebooting is safe`, `waiting for bc2: boot
  id changed, Ready`).
- **Each run records the equivalent command(s)** a user would type
  (`kubectl drain bc2 --ignore-daemonsets …`, `talosctl -n 65.21.130.10
  reboot`), shown in the dialog and the run's header and copyable with `y`.
- Running real subprocesses is reserved for operations that are a single
  `talosctl` call with no checks of ours (service restart may qualify);
  their stdout/stderr stream into the run the same way.

## Layout

```
┌ Operations · bc ─────────────────────────────────────────────────────────┐
│ Run on bc (cluster)                                                      │
│ ▸ Cordon   Uncordon   Drain   Reboot                                     │
│ ───────────────────────────────────────────────────────────────────────── │
│ History · 14 runs · 1 failed            /bc2                      <A> all │
│   WHEN             NODE  OPERATION        STATUS   TOOK   COMMAND        │
│ ● 11-03 10:42:07   bc2   Reboot           ok       3m12s  talosctl -n …  │
│ ● 11-03 10:30:55   bc2   Drain            ok       41s    kubectl drain… │
│ ● 11-02 18:01:13   bc2   Service restart  failed   2s     talosctl -n …  │
└──────────────────────────────────────────────────────────────────────────┘
```

- **Top:** an Operation drop list and the target's nodes as check boxes
  (a group starts with none checked, "all nodes" checks every one; a
  single node is checked already). Operations, named the same on a node
  and a group: Cordon, Uncordon, Drain, Reboot, Shutdown (node only). On a group or cluster an
  operation runs node by node (see "Operations on a group"). Unavailable ones are dimmed with the
  reason (`needs the Kubernetes API`).
- **History:** newest first; NODE is a node, `3 nodes` for a rolling run.
  Status colors: running cyan, ok green, failed red, cancelled dim.
- **Starting:** Enter on a node row → the confirmation dialog naming
  the checked nodes (the group when all are) and showing the
  command(s) → `y` starts it and opens the run; `n`/`Esc` cancels.
- **A run (Enter on a history row or a started run):** header (operation,
  target, context, started, took, status, command) and the output in the
  `LogPane`: follows the end while running, search, wrap, `V`/`y` lines,
  `v` external viewer. A rolling run's output has a section per node.
  Esc returns to the list; a running operation keeps running.
- **Keys:** `/` filters (node, operation, status, text in the command),
  `A` shows only this context's runs (default: every context), `y` copies the
  command, `D` deletes the selected run or, with a filter, the filtered
  runs, after a confirmation naming how many.
- `o` in the tree opens this view (the menu entry's letter); `O` and the
  popups go away.

## Data

- `~/.talos9s/operations/<YYYYMMDD-HHMMSS>-<context>-<op>-<target>.log`,
  0600, one per run: a JSON header line (context, target, nodes,
  operation, commands, started), the output lines with a time each, and a
  JSON end line (finished, status, message). Format and parsing in
  `talos9s_core::operations`.
- Written as the run goes (appended, flushed per line), so a crash keeps
  what happened; a run with no `finished` on next start shows as
  `interrupted`.
- Output is masked like the machine config (`mask_config` rules) before it
  is written: no secrets on disk.
- The view reads headers only for the list, the output on Enter.
- Never deleted automatically. A size line in the history title (`14
  runs, 2.1 MB`) so growth is visible.
- `audit.log` stays as is (one line per step) for now; later it may be
  derived from the run files.

## Principles applied

- Operations keep their guards: `y`/`n` dialog, audit, quorum, boot ID
  wait, drain rules.
- Bounded: every call inside a run has its timeout, as today; a run that
  stops reporting shows how long since its last line.
- Degrade: without the Kubernetes API, drain and anything that drains are
  dimmed with the reason; the history still shows.
- No secrets on disk or screen.

## Implementation steps

1. Done. Core (`talos9s-core::operations`): run header type, file name, parse
   and format, filter and sort of runs, `interrupted` detection. Tests.
2. Done. A run recorder in the TUI: open the file, append lines, finish; feed it
   from the existing drain/reboot/shutdown/rolling code's progress
   messages instead of (or as well as) the popup.
3. Done. The Operations view: list of operations for the target, history,
   filter, `A`, `D` with confirmation. Menu entry and wiring
   (`.claude/rules/tui-views.md`).
4. Done. The run pane: `LogPane` with a run source (file while finished, the
   live recorder while running).
5. Done. Move `o`/`O` to open the view; remove the popups. Service restart
   (`R` in Connections) was removed instead of moved, see "Decided".
6. Done. README, key bindings, `.claude/rules/operations.md`.

## Testing

- Unit: header round trip, file names, filters, interrupted runs, masking
  of output.
- TestBackend: the view draws at 70-120 columns; key lines fit (85).
- Live (bc): open each operation's dialog, check the command shown, cancel
  with `n`. Never confirm. A real run is checked only on a test cluster
  (`test-clusters/`).

## Running in the background

- A run belongs to the app, not the view: switching views, nodes or
  clusters leaves it running. Its recorder and progress live in `App`.
- **Global progress:** while a run is going, the header shows it on every
  view: `▶ Reboot bc 2/4 · bc2: waiting for Ready 1m12s`. A
  rolling run has a progress bar (nodes done of total, and the current
  step) in its run pane, as the popup has today. `o` jumps to it.
- **One run at a time per cluster context.** Two operations on one
  cluster can defeat each other's checks (a drain while a reboot of the
  group counts etcd quorum). Starting another on the same context says which run
  is going; another context may run its own.
- **Quitting while a run is going:** `q` and `Ctrl+C` don't quit; they say
  "Reboot on bc is running: cancel it in Operations (`o`) first".
  A second `Ctrl+C` within 2 s quits anyway (the run file is marked
  `interrupted`), so a hung run can never trap the user. A killed terminal
  can't be stopped; the next start shows the run as `interrupted`.
- **Cancel** (`x` in the run, with a `y`/`n` confirmation) stops after
  the current step, never in the middle of one: a reboot already sent
  can't be taken back, so cancel waits for that node to come back and
  uncordons what this run cordoned. The run ends `cancelled` with what was
  done.

## Operations on a group

One name per operation; the target says whether it is one node or several.
On a group or cluster it runs node by node, stops at the first failure,
and has a progress bar (nodes done of total, the current step).

| Operation | One node | Each node of a group, in turn |
|---|---|---|
| Cordon | mark unschedulable (`kubectl cordon`) | all at once is fine: nothing moves |
| Uncordon | schedulable again (`kubectl uncordon`) | same |
| Drain | cordon, then evict pods (`kubectl drain`); left cordoned | drain each in turn; all left cordoned (empties the group: pods may move to a later node and be evicted again; the dialog says so) |
| Reboot | drain, reboot, wait for Ready with a new boot ID, uncordon | the same per node, etcd quorum checked before each control-plane node, a delay between nodes |
| Shutdown | drain, shut down | node only |

Drain cordons first, so Cordon is for "stop new pods here, keep the
running ones", e.g. before a planned drain, and Uncordon undoes either.
Today's "Rolling drain" and "Rolling reboot" become Drain and Reboot on a
group.

## Drain options

Status: built · 2026-11-03

Drain, Reboot and Shutdown all drain first. Today they always use
fixed options (ignore DaemonSets, delete emptyDir data, pod default grace
period, 30 s PDB retry per pod, 30 s between nodes) and the dialog can't
change them.

### How others do it

| Tool | Options offered | Defaults |
|---|---|---|
| `kubectl drain` | `--ignore-daemonsets`, `--delete-emptydir-data`, `--force`, `--grace-period`, `--timeout`, `--pod-selector`, `--disable-eviction`, `--skip-wait-for-delete-timeout` | all off, so drain refuses and lists the pods that stop it; timeout none |
| k9s (`r` on a node) | a form: grace period, timeout, check boxes Ignore DaemonSets, Delete local data, Force | grace -1 (pod default), timeout 5 s |
| Rancher | Delete emptyDir data, Force, grace period (pod default or N s), timeout (none or N s); DaemonSets always ignored | off |
| Lens / Freelens | one confirmation, no options | |

The common set is three check boxes (DaemonSets, emptyDir, force) and two
values (grace period, timeout). kubectl's model is the useful part: an
option left off doesn't do something worse, it **stops the drain and names
the pods**.

### Gaps in our engine (fixed)

The engine doesn't follow kubectl where the options matter
(`select_pods_to_evict`, `drain_node_with_progress`):

1. **Unmanaged pods are evicted without `--force`.** kubectl refuses them,
   since they won't come back; ours evicts them and they're gone. Without
   Force they must stop the drain, listed; with Force they are evicted.
2. **DaemonSets unchecked evicts DaemonSet pods**, which their controller
   recreates on the same node at once. kubectl stops and lists them. Do
   the same.
3. **Grace period only reaches force deletes.** kubectl passes it to the
   eviction too (`DeleteOptions.gracePeriodSeconds`).
4. **Timeout is per pod** (PDB retry, 30 s). kubectl's `--timeout` is for
   the whole drain. Use one timeout per node; on expiry the drain fails
   naming the pods left (and the run uncordons what it cordoned, as now).
5. Check the blocking pods **before cordoning**, so a refused drain changes
   nothing.

### The dialog

The start dialog gets an options block when the operation drains:

```
┌ Drain bc2 ─────────────────────────────────────────────────────────┐
│ Options                                                            │
│ ▶ [✓] Ignore DaemonSets      7 pods stay                           │
│   [✓] Delete emptyDir data   2 pods lose it: web/cache-0, mon/prom-0│
│   [ ] Force                  1 unmanaged pod stops the drain:      │
│                              default/debug-shell                   │
│       Grace period           pod default                           │
│       Timeout                5m                                    │
│       Between nodes          30s            (group, Reboot only)   │
│                                                                    │
│ $ kubectl --context bc drain bc2 --ignore-daemonsets               │
│     --delete-emptydir-data --timeout=5m                            │
│                                                                    │
│ <↑/↓> option  <Space> toggle  <←/→> value  <y> start  <n/Esc> cancel│
└────────────────────────────────────────────────────────────────────┘
```

- Check boxes use the shared pick style (`check_box`, `PICK_CURSOR`).
- **Each option says what it hits on the checked nodes**, from a pod list
  read when the dialog opens (bounded, like the node facts): how many
  DaemonSet pods stay, which pods lose emptyDir data, which unmanaged pods
  Force would remove or which stop the drain. Unknown (pods not read) says
  unknown, never "none".
- An option left off that will stop the drain shows its pods in red, and
  the dialog says `drain will stop: 1 unmanaged pod`. `y` still starts it
  (pods may change before the run reaches that node); the engine checks
  again.
- Values step with `←`/`→`: grace period pod default, 0, 10, 30, 60, 120 s;
  timeout 1, 5, 10, 30 min, none; between nodes 0, 30 s, 1, 2, 5 min.
- The `$` command updates as options change and goes to the run header.
- Mouse: a click on a check box toggles it.
- Cordon and Uncordon have no options; the block isn't shown.

### Defaults and memory

- First time: Ignore DaemonSets on (every Talos cluster has CNI and
  storage DaemonSets, so off always stops), Delete emptyDir **on** (today's
  behaviour), Force off, grace pod default, timeout 5 min, 30 s between
  nodes.
- The last choice is saved per talosconfig context in `state.yaml`, as the
  namespace warp is. Force is never saved: it starts off every time.

### Not offered

- `--disable-eviction`: deletes pods past their PDBs; defeats the point.
- `--pod-selector`, `--chunk-size`, `--skip-wait-for-delete-timeout`:
  rare, and easy to get wrong in a dialog. Run kubectl for those.

### Steps

1. Done. Engine: the five gaps above, with unit tests on
   `select_pods_to_evict` (DaemonSet off, unmanaged without and with Force,
   emptyDir, finished pods). The force-delete fallback for unmanaged pods
   (deleting past a PDB) was removed: kubectl has none.
2. Done in part: `DrainOptions` has `timeout_secs` (5 min) and `force`;
   `drain_command` prints `--force` and `--timeout`. `between_nodes` comes
   with the dialog.
3. Done. The dialog's options block (`components/drain_options.rs`), the
   pod preview (`SafetyInfo::drain`), keys, mouse. The summary under the
   options gives counts only (`stops at 57 emptyDir pod(s)`): listing every
   pod flooded the dialog.
   Then replaced by a confirmation over the whole view: the options, a
   table of every pod and its fate (`PodImpact`, worst first, `PgUp`/`PgDn`),
   the commands and safety checks. Rows count, the table names.
   It covers the tree too (`Component::modal`): a click on the tree or menu
   used to close it. Pods a blocking PDB covers (its label selector matched
   against the pod) show `waits: PDB … allows 0`: the drain would wait for
   them until its timeout. On bc every CloudNativePG primary is such a pod,
   so draining a node with primaries needs a switchover first.
4. Done. Saved choice per context (`state.yaml` `drain`), force never.
5. Done. README, `.claude/rules/operations.md`, this plan.
6. Done (live on bc, cancelled with `n`): open the dialog on bc, toggle, check the preview and the command,
   cancel with `n`. A real drain only on `test-clusters/`, with an
   unmanaged pod, an emptyDir pod and a PDB to see each one stop it.

## Not done yet

- **A real run** was not tested against a live cluster (only dialogs opened
  and cancelled, per the live-test rule); try it on `test-clusters/` first.

## Decided

- **No service restart.** `R` in Network → Connections (restart the Talos
  service on a connection's port) was removed: it reached only kubelet,
  apid and trustd, and Talos supervises and restarts its services itself.
- Rolling runs keep a progress bar.
- History shows every context by default; `A` narrows to the current one.
- Runs are never deleted automatically, only with `D`.
- The menu entry's letter is `o`; `O` is no longer needed.

- One name per operation (Drain, not Rolling drain); on a group it runs
  node by node.
- Cordon and Uncordon are operations, on a node and a group.
- One run at a time per cluster context.
