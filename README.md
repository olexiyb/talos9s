# talos9s

A terminal UI for [Talos Linux](https://www.talos.dev/) Kubernetes clusters:
nodes, services, logs, etcd, network, storage and diagnostics in one place,
with safe node operations.

talos9s is a fork of [talos-pilot](https://github.com/Handfish/talos-pilot) by
Ken Udovic, continued with storage views (LINSTOR/DRBD, ZFS), views across a
cluster's nodes and more. It is a community project, not affiliated with
Sidero Labs.

[![CI](https://github.com/olexiyb/talos9s/actions/workflows/ci.yml/badge.svg)](https://github.com/olexiyb/talos9s/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/olexiyb/talos9s)](https://github.com/olexiyb/talos9s/releases)
![License](https://img.shields.io/badge/license-MIT-blue)

Talos has no SSH; everything goes through its API, and `talosctl` is a long
list of subcommands. talos9s puts that API, and the Kubernetes state that
depends on it, on one screen. It complements [k9s](https://k9scli.io/) rather
than replacing it:

| | Layer | Answers |
|---|---|---|
| **k9s** | Kubernetes (`:6443`) | "Why won't my pod start?" |
| **talos9s** | Talos OS (`:50000`) and the cluster around it | "Why won't my node join?", "Is it safe to reboot this node?" |

## Install

Prebuilt binaries (macOS, Linux):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/olexiyb/talos9s/releases/latest/download/talos9s-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/olexiyb/talos9s/releases/latest/download/talos9s-installer.ps1 | iex"
```

Or download an archive from [Releases](https://github.com/olexiyb/talos9s/releases).

From source (needs Rust 1.95 or newer; `rustup update` gets it):

```bash
cargo install --git https://github.com/olexiyb/talos9s --locked talos9s
```

With Nix:

```bash
nix run github:olexiyb/talos9s           # try it
nix shell github:olexiyb/talos9s         # put it on PATH
```

## Quick start

You need a talosconfig (`~/.talos/config` or `$TALOSCONFIG`) and network
access to your nodes on port 50000. The Kubernetes views fetch a kubeconfig
through the Talos API, so no separate kubeconfig is needed.

```bash
talos9s                         # every context in the talosconfig
talos9s --context homelab       # one context
talos9s --tail 1000             # lines of log history to load
talos9s --debug --log-file ~/talos9s.log
```

Without `--log-file`, each run logs to `<temp dir>/talos9s-<pid>.log`; logs
older than a week are removed at startup.

New cluster on machines in maintenance mode? The bootstrap wizard generates
the configs, applies them and bootstraps etcd:

```bash
talos9s --insecure --endpoint <node-ip>
```

## Features

### Workspace and Dashboard

The cluster tree and the menu stay on the left; the chosen view fills the
right side for whatever is selected. Move the selection and the view follows
(a node's network, then the next node's, or the whole group's on a group
row). Recently shown views come back instantly.

The menu groups the views: Overview (dashboard, diagnostics, logs,
processes), Node, Network, Storage and Cluster. The Node entries (containers,
machine config, extensions) need a node and are dimmed on a cluster or group
row.

The Dashboard shows:

- All contexts at once, each with its control plane and workers, node health
  and etcd quorum.
- Per cluster: a node table (versions, memory, disk, ZFS, load, uptime,
  services, clock offset, gateway), the KubeSpan peer matrix and LINSTOR
  nodes and pools.
- Per node: resources, addresses, routes, DNS, Tailscale, KubeSpan peers,
  clock, and every Talos service with its state.

### Logs

- Service logs, searchable and colored by level; several services interleaved
  (Stern-style). JSON and logfmt lines (kubelet, zap, slog, Pino ...) read
  as their message, then the other fields.
- On a cluster, control plane or workers row: the same services from every
  node merged by time with a node column, including the kernel log.

### Per node

| View | Shows |
|---|---|
| Processes | htop-like process list and tree, sorted by CPU or memory |
| Network | Interface traffic and connections, packet capture (filtered so it doesn't capture itself) |
| KubeSpan | The node's WireGuard peers; on a cluster or group row, the peer matrix |
| Disks | The node's disks, read-only ones marked `RO`; virtual devices (loop, dm, md, DRBD, zvol) hidden until `A`, which is remembered |
| Partitions | Talos volumes: the partitions with their filesystem, encryption and mount point |
| Containers | Talos system services and Kubernetes containers |
| Machine config | The config in effect, read-only, searchable, secrets masked; flags a staged change waiting for a reboot |
| Extensions | Installed system extensions |
| Diagnostics | Health checks with suggested fixes, CNI checks (Flannel, Cilium, Calico) and addon checks (cert-manager and more) |

On a cluster, control plane or workers row, network, storage and diagnostics
show every node at once; diagnostics as a node × category grid with each
warning listed once with its nodes.

### Cluster-wide

| View | Shows |
|---|---|
| etcd | Quorum, members, leader, alarms, advertised and listen addresses (running vs config), live member log |
| Volumes | LINSTOR/DRBD and ZFS LocalPV volumes, with problems first (see below) |
| Workloads | Unhealthy Deployments, StatefulSets, DaemonSets and pods in one list, worst first; NODES shows where each runs, or for a failing one, where its unready pods are; `A` shows healthy ones too, `S` groups by namespace, `/` filters; below the list, the selected pod's containers, last termination and latest event, and `y` copies the kubectl command. Enter focuses a workload: its pods, and the logs of all of them merged by time; select a pod for its containers' logs, Enter on it lists its containers, select one for only its logs (`p` previous run, `w` wrap, `1`-`5`/`0` levels as in Logs, `/` search with `n`/`N`, `PgUp/PgDn/g/G` scroll, follows the end), levels coloured, JSON and logfmt lines shown as message and fields |
| Lifecycle | Talos and Kubernetes versions across nodes; a saved config that differs from the one in effect is flagged "staged, reboot" |
| Security | Certificate expiry, encryption status |

**Volumes** compares LINSTOR's view with each node's kernel and flags what
LINSTOR alone doesn't show:

- DRBD devices or filesystems not grown after a resize;
- out-of-sync blocks on connected links, including those that can make a
  Primary fail its I/O after a short link loss
  ([LINBIT/drbd#153](https://github.com/LINBIT/drbd/issues/153));
- disconnected and StandAlone links (split-brain), suspended I/O, missing
  replicas, resync progress with an ETA, filesystems filling up;
- orphaned volumes: no PersistentVolume uses them, or their claim is gone.

Where a fix is provably safe, it suggests the exact commands to copy (`y`).
talos9s never runs them.

### Operations

| Operation | What it does |
|---|---|
| Drain | Cordon and evict pods, respecting PodDisruptionBudgets |
| Reboot | Cordon, drain, reboot, wait for the node to be Ready, uncordon |
| Shutdown | Cordon, drain, power off; the node stays cordoned until you uncordon it |
| Rolling | Drain or reboot nodes one after another, with progress |

Each operation checks etcd quorum and PDBs first, asks for confirmation and is
written to the audit log.

## Keys

In the tree (each key switches the view on the right; the left menu and the
mouse do the same):

| Key | On a node | On a cluster, control plane or workers row |
|---|---|---|
| `D` | Dashboard: node details | Dashboard: cluster or group summary |
| `l` / `L` | Logs | Logs merged across the nodes |
| `n` | Network | Network of every node |
| `K` | KubeSpan peers | KubeSpan peer matrix |
| `s` / `t` | Disks / partitions | Disks / partitions of every node |
| `d` | Diagnostics | Diagnostics grid |
| `p` | Processes | Processes |
| `C` / `m` / `x` | Containers / machine config / extensions | |
| `o` | Operations (drain, reboot, shutdown) | |
| `v` / `w` | Volumes / workloads on this node | Volumes / workloads on those nodes |
| `e` `b` `c` | etcd, lifecycle (boot and upgrades), security (cluster-wide) | same |
| `O` | Rolling operations | Rolling operations |

Everywhere:

- `j`/`k` or arrows move the selection and the view follows; `g`/`G` or
  `Home`/`End` jump to top or bottom, `PageUp`/`PageDown` move a page.
- `→`, `Enter` or a click puts the keyboard in the view; `←`, `Esc` or `q`
  gives it back (`q` doubles for `Esc` because some terminals, IntelliJ's
  among them, never pass `Esc`). `Tab` / `Shift+Tab` cycle Nodes → menu → view.
- `q` in a view returns to the Dashboard; there it asks before quitting
  (`y`). `Ctrl+C` quits at once.
- `/` filter or search, `r` refresh, `a` auto-refresh, `A` show all (or issues
  only), `0` all levels, services or nodes in logs, `y` copy, `Space` fold.
  The same letter means the same thing in every view; uppercase in the tree
  (`L`, `O`) is the across-nodes version of a view.
- `[` / `]` narrow or widen the left pane, `Z` zooms the view to full width.
- Mouse: click a row to select it, double-click to open it, click a menu
  entry or a sortable column header (Network) to switch or sort; the wheel
  scrolls the pane under the pointer.
- The top line lists the keys that work everywhere; the bottom line only
  what acts in the focused window right now.

talos9s starts where you left it (see [Safety](#safety) for the state file).

## Safety

talos9s runs with your talosconfig, which usually means full admin on every
node, so it is careful about what it changes:

- **Reads by default.** It changes a node only for drain, reboot, shutdown,
  rolling operations, cluster install and bootstrap, each after a
  confirmation dialog, and logs every step to `~/.talos9s/audit.log`.
- **Never runs suggested commands.** Diagnostic fixes (talosctl) and storage
  fixes (drbdadm, kubectl) are shown and copied for you to review and run.
- **Never shows secrets.** The machine config viewer masks keys, tokens,
  passwords, private keys and Secret data before anything is drawn.
- **Writes only:** the audit log, the remembered view state
  (`~/.talos9s/state.yaml`: selection, view, pane width, folded groups, sort
  orders and toggles, no cluster data, no text filters; delete it to start
  fresh), its debug log (temp dir or
  `--log-file`), packet captures you save (`/tmp`, owner-only) and the
  wizard's output directory. It has no configuration file.
- **No in-cluster access needed.** Nothing runs in the cluster and no
  Kubernetes namespace needs Talos API rights.
- **One connection besides your clusters:** at most once a day it asks
  GitHub for the latest talos9s release, to show "0.3.0 available" on the
  top line. `--no-update-check`, `TALOS9S_NO_UPDATE_CHECK=1` or
  `DO_NOT_TRACK=1` turns it off.

## How it works

- **State over logs.** Health comes from current state (files, Talos and
  Kubernetes APIs), not from log lines, so an old error in a log doesn't
  raise an alarm.
- **Unknown over wrong.** If a source can't be read, a check says unknown
  instead of guessing; nothing crashes, the rest of the screen keeps working.
- **Nodes, not the VIP.** The Talos API is addressed at each node; the
  cluster VIP needs a healthy control plane, so it fails exactly when you
  need diagnostics. Talos resources (COSI) aren't exposed on `:50000`, so
  they are read through `talosctl get`. Every call has a timeout, so a dead
  node never freezes the UI.

## Development

```bash
cargo test --all
cargo clippy --all --all-targets -- -D warnings
TALOS9S_CONTEXT=my-cluster cargo dev   # rebuild and restart on every change (needs cargo-watch)
```

The workspace has three crates: `talos-rs` (Talos client), `talos9s-core`
(logic, unit-tested without a terminal or cluster) and `talos9s-tui` (the
ratatui app). Git hooks, `cargo deny`, regenerating the Talos API client and
the other conventions are in [CLAUDE.md](CLAUDE.md).

- Local Talos clusters for testing: [test-clusters/](test-clusters/README.md)
- Design plans, e.g. [rolling upgrades](plans/upgrades.md): [plans/](plans/)
- Rules per area: [.claude/rules/](.claude/rules/)

Contributions are welcome. Keep changes in line with
[How it works](#how-it-works): check real state, degrade gracefully, and
never report failure when "unknown" is the honest answer.

## License

MIT, see [LICENSE](LICENSE).

## Acknowledgments

- [talos-pilot](https://github.com/Handfish/talos-pilot) by Ken Udovic, which
  talos9s is forked from
- [Talos Linux](https://www.talos.dev/) by Sidero Labs
- [k9s](https://k9scli.io/) for the inspiration
- [ratatui](https://ratatui.rs/)
