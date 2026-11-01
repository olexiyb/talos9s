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

From source (needs only a Rust toolchain):

```bash
cargo install --git https://github.com/olexiyb/talos9s --locked talos9s
```

With Nix:

```bash
nix run github:olexiyb/talos9s           # try it
nix shell github:olexiyb/talos9s         # put it on PATH
```

<details>
<summary>NixOS flake</summary>

```nix
{
  inputs.talos9s.url = "github:olexiyb/talos9s";
  outputs = { nixpkgs, talos9s, ... }: {
    nixosConfigurations.mymachine = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        { nixpkgs.overlays = [ talos9s.overlays.default ]; }
        ({ pkgs, ... }: { environment.systemPackages = [ pkgs.talos9s ]; })
      ];
    };
  };
}
```

</details>

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

### One workspace

The cluster tree and the menu stay on the left; the chosen view fills the
right side for whatever is selected. Move the selection and the view follows
(a node's network, then the next node's, or the whole group's on a group
row); pick another menu entry and the selection stays. Recently shown views
come back instantly. `[` / `]` narrow or widen the left pane, `Z` zooms the
view to the full width.

### Cluster overview (Dashboard)

- All contexts at once, each with its control plane and workers, node health
  and etcd quorum.
- Per cluster: a node table (versions, memory, disk, ZFS, load, uptime,
  services, clock offset, gateway), the KubeSpan peer matrix and LINSTOR
  nodes and pools.
- Per node: resources, addresses, routes, DNS, Tailscale, KubeSpan peers,
  clock, and every Talos service with its state.

### Logs

- Service logs, searchable and colored by level; several services interleaved
  (Stern-style).
- On a cluster, control plane or workers row: the same services from every
  node merged by time with a node column, including the kernel log.

### Per node

| View | Shows |
|---|---|
| Processes | htop-like process list and tree, sorted by CPU or memory |
| Network | Interface traffic and connections, packet capture (filtered so it doesn't capture itself) |
| KubeSpan | The node's WireGuard peers; on a cluster or group row, the peer matrix |
| Disks | The node's disks |
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
| Workloads | Deployments, StatefulSets and pod problems by namespace |
| Lifecycle | Talos and Kubernetes versions across nodes, config drift |
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

Where a fix is provably safe, it suggests the exact commands to copy (`c`).
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
| `e` `y` `c` | etcd, lifecycle, security (cluster-wide) | same |
| `O` | Rolling operations | Rolling operations |

Moving around:

- `j`/`k` or arrows move the selection; the view follows after a moment.
- `→` or `Enter` (or a click) puts the keyboard in the view; `←`, `Esc` or
  `q` gives it back to the side of the tree you came from (after closing the
  view's own menu, filter or search). `q` doubles for `Esc` because some
  terminals, IntelliJ's among them, never pass `Esc` to the app. In the tree,
  `Esc` returns to the Dashboard.
- `Tab` moves to the next window: Nodes → menu → view → Nodes (`Shift+Tab`
  the other way).
- Mouse: click a row to select it in any view, double-click to open it (a
  node from a group table, an interface's connections), click a menu entry
  to switch. In the logs panels a click toggles a service, level
  or node. In Network, clicking a sortable column header sorts by it and
  "Interfaces" in the connections breadcrumb goes back.
- `Space` folds a cluster or group, `r` refreshes, `a` toggles auto-refresh,
  `q` in the tree quits, `Ctrl+C` quits from anywhere.
- `[` / `]` narrow or widen the left pane (full, compact, hidden), `Z` zooms.
- talos9s starts where you left it: the selected cluster, group or node, the
  view, the pane width, folded groups, log levels, sort orders and view
  toggles are kept in `~/.talos9s/state.yaml`. Text filters are not. Delete
  the file to start fresh.
- The top line lists the keys that work everywhere (`Tab`, `Z`, `[` `]`,
  `Ctrl+C`); the bottom line lists only what acts in the focused window
  right now, so `Space` fold appears on an expandable row, `o` ops on a
  node, and a view's own keys (filters, search, submodes) while it has the
  keyboard.

## Safety

talos9s runs with your talosconfig, which usually means full admin on every
node, so it is careful about what it changes:

- **Reads by default.** It changes a node only for drain, reboot, shutdown,
  rolling operations and diagnostic fixes, each after a confirmation dialog,
  and logs every step to `~/.talos9s/audit.log`.
- **Never runs suggested commands.** Storage fixes (drbdadm, kubectl) are
  shown and copied for you to review and run.
- **Never shows secrets.** The machine config viewer masks keys, tokens,
  passwords, private keys and Secret data before anything is drawn.
- **Writes only:** the audit log, the remembered view state
  (`~/.talos9s/state.yaml`, no cluster data), its debug log (temp dir or
  `--log-file`), packet captures you save (`/tmp`, owner-only) and the
  wizard's output directory. It has no configuration file.
- **No in-cluster access needed.** Nothing runs in the cluster and no
  Kubernetes namespace needs Talos API rights.

## How it works

- **State over logs.** Health comes from current state (files, Talos and
  Kubernetes APIs), not from log lines, so an old error in a log doesn't
  raise an alarm. When a log is used to explain a state, it counts only if it
  matches what the API reports now.
- **Unknown over wrong.** If a source can't be read, a check says unknown
  instead of guessing; nothing crashes, the rest of the screen keeps working.
- **Nodes, not the VIP.** The Talos API is addressed at each node; the
  cluster VIP needs a healthy control plane, so it fails exactly when you
  need diagnostics.
- **talosctl for COSI.** Talos resources (COSI) aren't exposed on `:50000`;
  talos9s reads them through `talosctl get`.
- **Works without discovery.** With the Talos discovery service disabled,
  nodes are found through the Kubernetes API.
- **Bounded everything.** Every API call has a timeout and slow refreshes run
  in the background, so a dead node never freezes the UI.

## Development

```bash
cargo test --all
cargo clippy --all --all-targets -- -D warnings
cargo fmt --all -- --check
RUST_LOG=debug cargo run
TALOS9S_CONTEXT=my-cluster cargo dev   # rebuild and restart on every change (needs cargo-watch)
```

The workspace has three crates: `talos-rs` (Talos client), `talos9s-core`
(logic, unit-tested without a terminal or cluster) and `talos9s-tui` (the
ratatui app). `protoc` is needed only to regenerate the Talos API client
(`cargo build -p talos-rs --features regenerate`) after changing
`crates/talos-rs/proto/`.

- Local Talos clusters for testing: [test-clusters/](test-clusters/README.md)
- Design plans, e.g. [rolling upgrades](plans/upgrades.md): [plans/](plans/)
- Conventions for contributors and AI assistants: [CLAUDE.md](CLAUDE.md) and
  [.claude/rules/](.claude/rules/)

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
