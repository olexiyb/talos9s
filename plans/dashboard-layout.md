# Design: one workspace instead of full-screen views

Status: implemented (phases 0-4) · Author: Olexiy Buyanskyy · 2026-11-01

Differences from this plan, as built: the details pane stays in `cluster.rs`
(drawn into the workspace by `draw_details_pane`) instead of moving to a new
`dashboard.rs`; `Enter` on any tree row enters the view when it isn't the
Dashboard (`Space` folds); compact width is the longest node name within
14-22 columns; carried-over settings are the node view's tab, the volumes
view state and the log levels (not yet processes sorting); open questions 1
(`Alt+key`) and 4 (remembering layout) are not done.
Comes before [upgrades](upgrades.md); the upgrade view will use this layout.

## Summary

Today each view (logs, etcd, network, …) replaces the whole screen, and `Esc`
goes back to the cluster view. Instead, the cluster tree and the menu stay on
the left, and the chosen view fills the right pane for whatever is selected
in the tree. Moving the selection keeps the view and shows it for the new
node or group; picking a menu item keeps the selection and changes the view.
The current details pane becomes the first menu item, **Dashboard**.

```
┌ Nodes ───────────────┐┌ Network · bc2 ─────────────────────────────────┐
│ ▼ ● bc (4) etcd 4/4● ││ Interfaces  Connections  KubeSpan  Capture     │
│   ▼ Control Plane (4)││ eth0   ▲ 12.4 MB/s  ▼ 3.1 MB/s  ...            │
│      ● bc1           ││ ...                                            │
│    ▸ ● bc2           ││                                                │
│      ● bc3           ││                                                │
│ ▶ ● kprod (8)        ││                                                │
│──────────────────────││                                                │
│   [D] Dashboard      ││                                                │
│   [L] Logs           ││                                                │
│ ▸ [n] Network        ││                                                │
│   ...                ││ Tab views  / filter  r refresh  Esc tree       │
└──────────────────────┘└────────────────────────────────────────────────┘
 [j/k] nav  [Enter] into view  [Z] zoom  [ [ ] ] pane width  [q] quit
```

## Why

- You keep your place: which cluster, group and node you're looking at stays
  visible.
- Comparing nodes is moving the selection, not `Esc` → select → hotkey.
- Mouse use becomes natural: click a node, click a view, read.
- One pattern for every view, including the coming upgrade view.

## Layout

- **Left pane:** the cluster tree and the menu (as today), at one of three
  widths:
  - **Full:** sized to its content (today's `nodes_pane_width`).
  - **Compact:** about 18 columns: health dot and node name per row (long
    names cut with `…`), menu entries as keys and short labels.
  - **Hidden:** zoom; the view gets the whole width.
- **Default width:** Full when the right pane keeps at least 100 columns,
  otherwise Compact.
- **Keys:** `[` narrows (Full → Compact → Hidden), `]` widens, `Z` toggles
  Hidden and back to the previous width. They work in both panes (no view
  uses `Z`, `[` or `]` today).
- **Right pane:** the active view, with its own title (view · target) and its
  own key line at the bottom.
- **Bottom line of the screen:** the left pane's keys while it has focus,
  otherwise only the global ones (`Z`, `[`/`]`, `Esc` tree, `Ctrl+C` quit).

## Focus and keys

Two places have focus: the **tree** (tree rows and menu, as today's
`FocusedPane::Nodes` / `Menu`) and the **view**.

| In | Key | Does |
|---|---|---|
| tree | `j`/`k`, arrows | move the selection; the view follows (see below) |
| tree | view hotkeys (`n`, `s`, `L`, `e`, …) | switch the right pane to that view |
| tree | `Enter`, `Tab` from the menu, click in the right pane | focus the view |
| view | everything | goes to the view (its own `/`, `Tab`, `n`, `s`, `c`, …) |
| view | `Esc` | the view's own `Esc` first (close a filter, a search, a dialog); when it has nothing to close, focus goes back to the tree |
| both | `Z`, `[`, `]`, `Ctrl+C` | layout and quit |
| both | click | focus the pane clicked |

Views use many letters the tree uses as hotkeys: Network uses `n s t u`,
logs `a d f g l n s w`, node info `n N`, volumes `a c t`, diagnostics `h l`.
Routing every key to the focused pane keeps all of them working unchanged;
the price is pressing `Esc` before switching views from inside one. If that
proves annoying, `Alt+<hotkey>` can switch views from either pane (open
question 1).

Pop-ups stay pop-ups over the whole screen: operations (`o`), rolling
operations (`O`), confirmations.

## Following the selection

The **target** is the selected tree row: a node, a control-plane or workers
group, or a cluster. When it changes, the view is shown for the new target.

- **Debounce:** wait 300 ms after the last move before loading (logs 500 ms:
  restarting streams costs more), so scrolling through ten nodes loads one.
- **Cache:** keep the last loaded data for the four most recent
  (view, target) pairs; coming back shows it at once, marked as refreshing,
  while it reloads.
- **View settings carry over:** the open tab, filters, sort order, log level
  and service selection stay when the target changes. Selection and scroll
  position reset. Each component exposes them as a small settings struct,
  as `VolumesViewState` already does.
- **Cancel stale work:** a retarget aborts the running refresh job and stops
  log streams of the old target.

### What each view shows per target

| View | Node | Control plane / workers | Cluster |
|---|---|---|---|
| Dashboard | node details (today's pane) | group summary (nodes table) | cluster summary (today's cluster pane) |
| Logs | node services | merged across the group | merged across the cluster |
| Network, Storage, Diagnostics | the node | group view | group view (all nodes) |
| Processes | the node | the group's nodes | all nodes |
| Containers, Machine config, Extensions | the node | "Select a node" hint | same hint |
| Volumes, Workloads | on this node | on those nodes | whole cluster |
| etcd, Lifecycle, Security | the cluster the target belongs to | same | same |

## Architecture

Changes are mostly in `app.rs` and `cluster.rs`; the view components keep
their drawing and key handling.

- **`Target`** (new): context, kind (node with hostname, address and role;
  group or cluster with its nodes), and a stable id for caching and debounce.
- **Selection events:** the cluster component emits
  `Action::TargetChanged(Target)` on every tree move; `app.rs` debounces it
  and retargets the active view.
- **Active view:** `App` holds which view is open (today's `View` enum, minus
  `Cluster`, plus `Dashboard`) and its component. Retargeting builds the
  component for the new target with the previous settings, through the same
  constructors the `Show…` actions use today.
- **`cluster.rs` splits:** tree and menu drawing (left pane) stay; the
  details pane moves into a Dashboard component (`components/dashboard.rs`).
  The file is 4,775 lines, so this also starts breaking it up.
- **Drawing:** `App::draw` lays out header, left pane, right pane, bottom
  line, and calls the view's `draw` with the right pane's `Rect`. Components
  already draw into the area they get; each needs a check at 100–120 columns
  (tables with fixed widths, footers).
- **Mouse:** the app routes by pane; components already record the `Rect`s
  they drew (absolute coordinates), so clicks in the right pane work unchanged.
- **Refresh:** one refresh job as today, tied to the active (view, target);
  `Action::Back` and the `Show…` actions go away once every view lives in the
  pane.

## Phases

0. **Frame:** `Target`, `TargetChanged` with debounce, the three pane widths
   and zoom, focus routing, Dashboard as the only pane view. Other views still
   open full screen. Ships on its own: it's the current screen plus widths and
   zoom.
1. **Node views** in the pane: network, storage, processes, containers,
   machine config, extensions, diagnostics, with following, cache and
   settings carry-over.
2. **Cluster-wide and group views:** etcd, volumes, workloads, lifecycle,
   security, and the group network/storage/diagnostics.
3. **Logs:** single node and merged, with stream restart on retarget.
4. **Clean-up:** remove the full-screen paths (`Action::Back`, `logs_return`,
   `return_to_group`), update README keys and screenshots,
   `.claude/rules/tui-views.md` (the "wiring a new view" list gets shorter).

Each phase keeps everything working and goes through the usual install,
review and commit.

## Testing

- `TestBackend` layout tests at 100, 160 and 220 columns for each pane width:
  nothing overlaps, titles and key lines fit.
- Key routing tests: a hotkey in the tree switches views; the same key in the
  view reaches the view; `Esc` closes the view's filter first, then returns
  focus.
- Debounce and cache with paused tokio time: ten quick moves load once;
  coming back shows cached data.
- Live on bc and kprod in tmux: walk the tree with each view open, zoom,
  compact, mouse clicks.

## Open questions

1. `Alt+<hotkey>` to switch views from inside a view, or is `Esc` first fine?
2. Compact width: fixed about 18 columns, or the longest node name plus the
   dot?
3. Should the Dashboard on a group show a nodes table (as today's cluster
   pane) or a summary of problems (unhealthy services, etcd, storage issues)?
4. Should the open view and pane width be remembered between runs? That
   would be talos9s's first settings file (see [upgrades](upgrades.md),
   Configuration).
