# Changelog

## 0.4.0

Kubernetes across every cluster, an Operations view for drains and reboots that keep running in the background, and Workloads and Containers views rebuilt around what is broken. Everything Kubernetes stays read-only; operations still ask before they act.

### Behavior changes

- **Operations replace the popups.** `o` opens the Operations view for the selected node, group or cluster; the node and rolling operation popups and `O` are gone.
- **Service restart removed** (`R` in Network → Connections). It reached only kubelet, apid and trustd, and Talos restarts its services itself.
- **`Tab` always moves between windows.** Diagnostics used it to jump between its sections; they are now headings in one list that `j`/`k`, `g`/`G` and the page keys move through.
- **`--kubeconfig` is your identity.** With it, talos9s uses only the context whose CA matches the cluster. When there is none, or your user may not read something, the view shows the error instead of falling back to the admin kubeconfig from a control plane node.
- **New directory:** `~/.talos9s/operations/` holds one private file (0600) per operation run.

### Features

**Kubernetes (`i`)** lists the unhealthy Deployments, StatefulSets, DaemonSets and pods of every cluster in your talosconfig, with a CLUSTER column (`A` shows everything). Any kind from API discovery, CRDs included (`:`); search by name, label selector or image (`s`); `X` one cluster at a time. Enter shows an object's status, conditions, events and YAML with secrets masked; `d` diffs it with the same object in another cluster; `l` / `L` read its pods' logs, or those of the same selector in every cluster. A cluster that can't be reached or answers 403 shows `?` with its error; the others are listed.

**GitOps (`g`)**: Flux Kustomizations, HelmReleases and sources and Argo CD Applications across clusters, worst first.

**Operations (`o`)**: cordon, uncordon, drain, reboot, and shutdown on one node.
- Pick the operation from a drop list (`1`-`5`) and the nodes as check boxes. Each node shows Talos state and uptime, Ready, cordoned, the pods a drain evicts and its last run; a node the operation can't run on says why.
- Drain, reboot and shutdown confirm in a window with the drain options (ignore DaemonSets, delete emptyDir data, force, grace period, timeout, wait between nodes, kept per cluster; force never) and a table of every pod and what happens to it: stops the drain, waits for a PDB, lost for good, emptyDir data lost, evicted, stays. It refreshes every 10 s.
- Drains follow kubectl's rules: pods without a controller and DaemonSet pods stop the drain unless allowed, blockers are checked before cordoning (a refused drain changes nothing), never a force delete past a PDB. CloudNativePG primaries with replicas don't block (the operator switches over). "Wait to finish" lets CI and Job pods end by themselves first.
- Runs go on in the background: the header shows progress on every view, and quitting waits for a run (a second `Ctrl+C` within 2 s quits). Every run's output, with each step's shell command naming the `--context`, is kept in a history you can filter and delete; on a node it lists only the runs that included it (`A` all).

**Workloads.**
- One list of what's broken, worst first, with each workload's problem pods under it and the nodes they run on. `A` adds healthy ones, `S` groups by namespace, `/` filters.
- Enter focuses a workload: its pods, and their logs merged by time, for all pods, one pod or one container. Init containers have logs too; `p` shows the previous run where there is one.
- `P` lists every pod, sortable by any column with `1`-`8` or a header click.
- A list the API refuses (RBAC) shows its error; the rest works. When the Kubernetes API is down the view points to Containers.

**Containers (`C`)** on a node, group or cluster: every container, read through the Talos API (no Kubernetes API needed). Exited ones are hidden until `A`; sort by any column. Enter focuses one with its logs.

**Namespace warp (`W`)** in Workloads, Containers and Volumes: a picker with the last four used (`1`-`4`) and every namespace with its count. `Space` marks several and a filter ending in `*` matches a prefix (`staging*`); `0` returns to all namespaces. Kept per context in `state.yaml`.

**Logs.**
- JSON and logfmt lines read as level, time and message, with the other fields dimmed (zap, logrus, slog, Bunyan, Pino, klog), in the Logs view and in the Kubernetes and container logs.
- In Workloads and Containers logs: level filters (`1`-`5`, `0`) and search (`/`, `n`/`N`) as in the Logs view, a date when lines span days, colour codes removed, `V` to select lines and `y` to copy them.
- `v` opens them in `hl`, or the viewer named in `TALOS9S_LOG_VIEWER`.

**Help (`?`)** shows every key that works now: the view's, the view letters, the tree's and the shared ones.

**Smaller things.**
- The menu is grouped under headings (Overview, Node, Network, Storage, Cluster), and node-only entries dim on a cluster or group.
- `#` switches the machine config outline between bytes and lines.
- Views no longer repeat the window title in their own header or draw boxes inside the window: a summary line, the table, and details under a rule.

### Fixes

- **Temp files for the log viewer** get a random name and are created exclusively (0600). The fixed name in a shared `/tmp` let another user plant a symlink the write would follow.
- Kubernetes errors show the API server's message instead of kube-rs's Status dump, in every view.
- A view picked while the cluster is unreachable shows the error instead of loading for good.
- Finished pods read Completed, dimmed, and sort last by READY.
- Tables no longer leave a blank line under their header.

## 0.3.0

One workspace instead of full-screen views: the cluster tree stays on the left and the chosen view opens on the right, following the selection. talos9s also starts where you left it, works with the mouse throughout, is much faster on larger clusters, and is safer: reboots check etcd properly and diagnostics no longer change anything.

### Behavior changes

- **Diagnostics never apply fixes.** The fix dialog shows the `talosctl` command for the node and cluster and copies it; you run it yourself. Before, it could patch the machine config, apply config or restart a service.
- **Keys.** `Tab` / `Shift+Tab` move between windows (nodes, menu, view); `→`, `Enter` or a click put the keyboard in the view, `←`, `Esc` or `q` hand it back. `q` in a view goes back to the Dashboard and on the Dashboard asks "Quit talos9s?" (`y` or `Enter` quits); `Ctrl+C` quits anywhere. Views no longer use `Tab` for tabs of their own.
- **Menu.** New entries replace in-view tabs: KubeSpan (`K`), Disks (`s`) and Partitions (`t`) instead of the Storage view's tabs, and Containers (`C`), Machine config (`m`) and Extensions (`x`) without the node view's tab bar. Talos volumes (STATE, EPHEMERAL, user volumes) are now called Partitions, so they no longer clash with the LINSTOR/ZFS Volumes view.
- **Same key, same meaning in every view.** `a` only toggles auto-refresh; "show all" moved to `A` (volumes, group tables, connections), and "all services/nodes" in a logs pane to `0`. `y` copies everywhere, so `c` no longer copies in volumes, etcd and diagnostics (it stays packet capture in Network). Lifecycle moved from `y` to `b` in the tree, and "listening only" in connections from `l` to `L`. Lists in Storage, Security, Lifecycle and the install wizard stop at the ends instead of wrapping, and every list takes `g`/`G`, `Home`/`End` and the page keys.
- **Key lines.** The keys that work everywhere are on the top line; the bottom line lists only what acts in the focused window right now. Keys are written like k9s does, `<y>`, `<Esc/q>`, `<Ctrl+C>`, all in one colour (green `<y>` and red `<n>` in confirmations); square brackets are left to status labels such as `[AUTO:OFF]`.
- **Confirmations.** The wizard's disk install and bootstrap ask for `y` in a dialog (two `Enter`s used to be enough to erase a disk). Drain with emptyDir deletion off refuses and lists the pods instead of rebooting anyway.
- **New file:** `~/.talos9s/state.yaml` remembers where you were (see below). The audit log `~/.talos9s/audit.log` now actually records operations; before, it was never written.
- **One connection besides your clusters:** at most once a day talos9s asks GitHub for the latest release. Turn it off with `--no-update-check`, `TALOS9S_NO_UPDATE_CHECK=1` or `DO_NOT_TRACK=1`.
- **Building from source** needs Rust 1.95 or newer.

### Features

**Workspace.** Every view opens on the right in a titled window for the selected node, group or cluster ("Network · lab1"), and moving the selection shows the same view for the new target. The last few views come back at once and refresh. The left pane can be full, compact or hidden (`[`, `]`, `Z`), titles show a spinner while a refresh runs, and long panes and the node tree scroll, with scrollbars.

**Starts where you left it.** The selected cluster, group or node, the view, the window with the keyboard, the pane width, folded groups, log levels, sort orders and view toggles are kept in `~/.talos9s/state.yaml`. Text filters are not. A remembered cluster wins over the talosconfig's current context. Delete the file to start fresh.

**Mouse.** Click a row to select it in any view and double-click to open it; click a menu entry to switch views; the wheel scrolls the pane under the pointer. In Network, clicking a sortable column header sorts by it; in the logs menus a click toggles a service, level or node.

**Dashboard.**
- The node table fills in as nodes answer: every node is listed at once with "…" until its data arrives, and a node without data yet shows a gray dot rather than green.
- Built for large clusters: KubeSpan is a one-line summary ("56/56 links up") listing only links that aren't up, plus a KSpan column per node; the services matrix becomes one row per service with the unhealthy nodes named when the nodes don't fit side by side.
- The node dashboard shows a services legend and what the node's last diagnostics run found.
- Memory and load use one set of thresholds everywhere (memory warns above 80%, load is judged per core), so a busy 32-core node no longer looks critical.

**Machine config.** An outline on the left lists each config document, the main sections and every inline manifest and file by name with its size; the chosen part shows beside it. `/` searches the whole config and jumps to the match. A fetched config is kept for the session, so coming back is instant.

**Disks.** Loop, dm, md, DRBD, zvol and other virtual block devices are hidden until `A` ("14 virtual hidden"), as in the group view; the choice is remembered and shared by the node and group storage views. Their TYPE names the kind (loop, dm, zvol) instead of "SSD", and an RO column marks read-only disks.

**Narrow screens.** The cluster and group nodes table drops its least useful columns (GW, Cores, Tailscale, KSpan, ...) until the rest fit, instead of squeezing every value (node names were cut to 3 letters at 100 columns). Every view's key line fits 100 columns.

**Lifecycle** flags a node whose saved config differs from the one in effect ("staged, reboot"), instead of a config drift warning that compared numbers that can't be compared.

**Version.** The top line shows the version, and "0.3.0 available" when the daily check finds a newer release. `talos9s --version` prints both.

### Speed

- **One connection per node.** Requests for several nodes no longer queue on a single connection: refreshing an 8-node cluster went from 3-6 s to about half a second, and the Processes view from several seconds to under one.
- **Only the cluster you look at loads everything.** Other clusters refresh just what the tree needs, in parallel; the detailed table and the Kubernetes data (LINSTOR, ZFS, kubelet versions) load for the selected cluster, and alongside the node queries.
- **Processes** shows the first sample at once, with CPU % filled in a second later.
- **Diagnostics** runs about 10x faster (one node 21 s to 2 s); a group connects to Kubernetes once instead of once per node. **Workloads** loads in about half the time, and Kubernetes responses come gzipped.

### Fixes

**Safety**
- **Machine config masking** also hides PGP private keys, `dockerconfigjson` lines, passwords embedded in mirror and proxy URLs and secret keys inside `{…}` maps; before, only PEM blocks and plain keys were caught.
- **Reboot safety** counts etcd members that are already down (with 3 members and 1 down, rebooting another showed Safe), treats a status it couldn't read as unknown, and matches members correctly (10.0.0.1 no longer matches 10.0.0.11).
- **Rolling reboots** wait for etcd to allow each reboot, so a control plane node waits for the previous one to rejoin; a node counts as back only when Ready after its new boot.
- **Lifecycle pre-checks** that couldn't run say so instead of "All checks passed".

**No false alarms** (unknown is shown as unknown, not as a failure)
- **etcd:** a failed status call shows UNKNOWN, not NO QUORUM with every member down.
- **Diagnostics:** a node that didn't answer is unknown, not a failure with a patch to apply; certificates are expired only after they expire (23 hours left showed "Expired"); Diagnostics read the right cluster's Kubernetes state for control-plane nodes; the Cilium + KubeSpan check follows Talos' documented setups and names what is missing.
- **Workloads:** rollouts no longer show Partial, old restarts don't stay "high", finished Job pods and briefly Pending pods aren't problems.
- **Storage:** LINSTOR, ZFS and partition states that are in transition or unknown are warnings, not errors.
- **Network:** busy healthy nodes no longer warn about TIME_WAIT; workers don't show control-plane ports as down; errors and drops are flagged only while they rise, with the rate ("147301 +3.2/s"), and large counts are no longer cut off.
- **Logs:** a line is an error by the level it states, not because "stderr" or "error" appears in it.
- **Pod traffic** counts only addresses in the cluster's own pod subnets, read from the node's machine config, for cni0, flannel, veth and now Cilium's lxc and cilium_* devices. The built-in 10.244.0.0/16 missed clusters with another range, and Cilium devices listed every connection of the node.
- **Network totals** drop interfaces that were deleted (a removed pod's veth kept adding to RX/TX).

**Crashes and glitches**
- Odd log timestamps, searches in non-ASCII text, long errors, copying a process command with a non-ASCII character, and very small log panes can no longer crash the TUI.
- Offline nodes in the group table line up under their columns instead of "down …" landing under KSpan.
- Every Kubernetes request (LINSTOR, ZFS) and the `talosctl` disk and volume reads in insecure mode time out, so a dead node can't hang a view.
- IPv6 node addresses are kept whole, and endpoints written as URLs are recognized.
- The tree cursor stays on its node when a refresh adds or drops one.
- Logs no longer drop lines in a burst, and the newest lines stay visible with wrapping on.
- KubeSpan shows the last handshake instead of an RTT column that was always empty.
- Machine config gets up to two minutes to load and shows how long it has waited; picking a view while a cluster connects opens it when the nodes arrive.
- `q` does what `Esc` does in a view, for terminals that don't deliver `Esc` (IntelliJ's).

## 0.2.0

talos9s continues [talos-pilot](https://github.com/Handfish/talos-pilot) 0.1.11 under a new name: the binary is `talos9s`, the crates `talos9s-core` and `talos9s-tui`, and the audit log moves to `~/.talos9s/audit.log` (an existing `~/.talos-pilot/audit.log` is copied over on first use). Entries below are talos-pilot's.

## 0.1.11

Reliability release: node enumeration no longer depends on the Talos discovery service, and Kubernetes views always target the launched cluster.

### Bug Fixes

**Worker nodes missing when cluster discovery is disabled ([#25](https://github.com/Handfish/talos-pilot/issues/25), [#22](https://github.com/Handfish/talos-pilot/issues/22))**

0.1.10 fixed workers vanishing when `--config` broke context resolution, but they still disappeared whenever Talos cluster discovery returns no members. This is increasingly common: Sidero has disabled the Kubernetes discovery registry by default, leaving only the external `discovery.talos.dev` service registry — which hardened/airgapped clusters routinely turn off — so `talosctl get members` comes back empty as the norm, not the exception.

talos-pilot built its node roster from `talosctl get members`, and when that's empty it fell back to etcd membership — which only ever contains control-plane nodes — so workers dropped out (leaving the "cluster discovery returned no members" warning). It now falls back to the **Kubernetes API**, which knows every node (control plane and worker) independently of Talos discovery. The kubeconfig for that lookup is fetched directly from a control-plane node over the Talos API (certificate auth), so it always targets the correct cluster and never an unrelated one an ambient `KUBECONFIG` might point at. Displayed nodes still come from successful Talos queries, so unreachable/bogus IPs self-filter.

The same fallback is applied everywhere the app enumerates nodes independently of the main list:

- **Cluster node list** — workers appear again (and everything derived from it: rolling operations, per-node logs/processes/network/storage/operations).
- **Lifecycle (versions / config drift)** — the version-skew and config-drift table now covers all nodes, not just the endpoint the base client targets.

Reproduced on a discovery-disabled Docker cluster (1 control plane + 2 workers): before, both views showed the control plane only; after, all three nodes appear. Verified end-to-end + regression tests added.

**Lifecycle "Config" and "Time Sync" columns were always blank**

The Lifecycle node table showed `-` for every node's config version and time-sync status. Two causes: `talosctl get machineconfig` returns multiple YAML documents (the `persistent` and `v1alpha1` configs), but the parser tried to decode the whole blob as a single document and always errored; and per-node time-sync info was only fetched for the endpoint. Both are fixed — machineconfig parsing now splits documents and prefers the canonical `v1alpha1` config (tolerating a numeric `version`), and version + time-sync are queried per node — so the table is fully populated for control-plane and worker nodes alike. Regression tests added.

**Kubernetes views could target the wrong cluster when `KUBECONFIG` was set**

The workloads view and the drain/cordon operations build a Kubernetes client that preferred an ambient `KUBECONFIG` over the launched Talos context. If your shell's `KUBECONFIG` pointed at a *different* cluster than `--context`, read views silently showed the wrong cluster's data and — more seriously — drain/cordon could act on the wrong nodes.

These clients now pin to the launched cluster: the kubeconfig is fetched from a control-plane node over the Talos API, so it always identifies the cluster you asked for. An ambient `KUBECONFIG` is still honored when it names the **same** cluster (matched by cluster CA, so a VIP/load-balancer endpoint isn't mistaken for a different cluster) — preserving your configured, possibly more reachable endpoint — but is ignored when it names a different one. When it's ignored, a wrapped warning on the cluster view explains why, so nothing is silently overridden. Verified end-to-end against a cluster with a mismatched `KUBECONFIG`; regression tests added.

## 0.1.10

Reliability release focused on connectivity, node discovery, and node operations.

### Bug Fixes

**Multi-endpoint failover — app no longer hangs silently ([#27](https://github.com/Handfish/talos-pilot/issues/27))**

talos-pilot now fails over across endpoints like `talosctl`: it dials all of a context's endpoints concurrently and uses the first that actually connects (bounded by a 10s timeout), so a dead first endpoint is skipped instead of hanging the session. The old "Successfully connected" log was just tonic's lazy channel, not a real handshake — hence the ~21s wait before the first RPC failed. Startup no longer blocks on a blank screen: there's now a cancellable "Connecting…" screen (q/Esc/Ctrl-C), and an all-unreachable cluster shows a clear Connection Error panel. Verified on a multi-endpoint cluster with the dead endpoint first — connects in ~2ms where it used to hang, so the reorder-endpoints workaround is no longer needed. (commit 9cab34f)

**Lifecycle/Versions view now fetches data ([#26](https://github.com/Handfish/talos-pilot/issues/26))**

The Lifecycle view fetched the Talos version first and treated it as a hard gate: on failure it cleared its own error and returned early, skipping the etcd/K8s/pod checks entirely (hence zero follow-up requests and the silent "unknown" fields). Now a version-fetch failure just logs a warning and continues; the other checks fire regardless, and you only get an error banner if the whole refresh comes back empty. Regression tests added. (commit 1ce1e01)

**Worker nodes no longer missing ([#25](https://github.com/Handfish/talos-pilot/issues/25), [#22](https://github.com/Handfish/talos-pilot/issues/22))**

talos-pilot fetches the full node list (workers included) via `talosctl get members`, but it wasn't passing `--talosconfig`, so if you ran with `--config <path>` talosctl fell back to the default `~/.talos/config`, couldn't find your context, and the list silently collapsed to just the control-plane nodes. It now passes your config through correctly. Node roles are now determined from the discovery `machineType` (not inferred from the etcd service), and if worker discovery genuinely can't run (discovery service disabled, or `talosctl` not on PATH) you'll now see a visible warning instead of a silently short list. Verified end-to-end + tests added. (commit fd5080f)

**"No K8s client available" when rebooting a node ([#23](https://github.com/Handfish/talos-pilot/issues/23))**

When you open node operations, talos-pilot builds a Kubernetes client to run the pre-drain safety checks. It was only trying your environment kubeconfig and then a VIP fallback — and since we build kube-rs without the exec/OIDC auth plugins, a kubeconfig that `kubectl` accepts (e.g. EKS/OIDC) could fail to load, leaving no client, so the reboot bailed out with the generic "No K8s client available". It now fetches the kubeconfig directly from a control-plane node over the Talos API (certificate auth, always available), so it no longer depends on your local kubeconfig's auth method. And if a client genuinely can't be created, you'll now see the specific reason instead of the generic message. Verified that drain/reboot safety checks load even with no usable local kubeconfig. (commit 49814c7)

## 0.1.9

See the git history for changes prior to 0.1.10.
