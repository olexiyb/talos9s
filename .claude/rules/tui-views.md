---
paths:
  - "crates/talos9s-tui/src/app.rs"
  - "crates/talos9s-tui/src/action.rs"
  - "crates/talos9s-tui/src/components/**"
---

# TUI views

## Component pattern

- One file per view in `components/`, implementing `Component`
  (`handle_key_event`, optional `handle_mouse_event`, `update`, `draw`).
- Async data in `state: AsyncState<XData>`; UI state (selection, scroll,
  filters, hit areas) as plain fields.
- `pub async fn refresh(&mut self) -> Result<()>` loads data with the client
  the component was given (`set_client`). No client argument.
- Slow refreshes run on a clone: implement `BackgroundRefresh`
  (`adopt_refresh` takes only the loaded state, keeping UI state the user
  changed meanwhile). `app/view.rs` starts them (`ViewComponent::spawn_refresh`)
  and the app adopts the result when it finishes.
- Auto-refresh: `update(Action::Tick)` returns `Action::Refresh` when
  `state.should_auto_refresh(..)`.
- Show a loading message for each part until its data arrives (an empty table
  looks like "no data"); load what the open tab needs first.

## The workspace

`app.rs` draws one screen: `ClusterComponent::draw_frame` (header, nodes pane
at `PaneWidth` Full/Compact/Hidden, key line) and the current view in the
area it returns. `App::pane` is the menu entry shown, `App::view` the
component realizing it for the selection (Network is `View::Network` on a
node, `View::Group` on a group). Overlays (operations) draw over everything.

- **Focus:** the tree or the view (`view_has_focus`). Keys go to the focused
  side only, so views keep their letters; a component's `Action::Back` hands
  focus back to the tree (the view stays). Layout keys `Z` `[` `]` work in
  both unless the view `captures_text()` (implement it for filter/search
  inputs).
- **Following the selection:** `follow_selection` compares
  `scope_key(pane)` (cluster-wide views use `cluster_key`) with
  `applied_target`, waits `FOLLOW_DELAY` (logs longer), then `retarget`s via
  `ClusterComponent::action_for(item)`.
- **Leaving a view:** every `Show…` handler calls `leave_view()` first (stops
  log streams, keeps settings that carry over, caches the component by
  (pane, target)) and ends with `show_component(item, …)`.
- **Target:** the selected tree row: a node, the control plane or workers
  group, or a cluster. Moving the selection keeps the view and shows it for
  the new target; a menu entry keeps the target and changes the view.
- **Retargeting:** the selection must rest `FOLLOW_DELAY` (logs
  `FOLLOW_DELAY_LOGS`, streams cost more) so walking ten nodes loads one;
  the last `VIEW_CACHE_SIZE` (view, target) pairs show at once while they
  reload (constants in `talos9s_core::constants::workspace`). A retarget
  aborts the running refresh (`cancel_refresh_job`) and stops old streams.
- **Carry over** the open tab, filters, sort, log levels and toggles
  (`ViewSettings`); reset selection and scroll.
- **Pane widths:** Full (sized to content), Compact (longest node name,
  14-22 columns), Hidden. `[` narrows, `]` widens, `Z` toggles Hidden.
  Default is Full when the view keeps at least 100 columns.
- **Pop-ups** (operations `o`, rolling operations `O`, confirmations) draw
  over the whole screen. A picker that belongs to one view (the namespace
  picker, `components/namespace_picker.rs`) is the view's: it draws over
  the view, takes its keys, and tells the app the result through an action
  (`WarpNamespace`).
- **Focused view:** a view may focus one item (Workloads: Enter on a
  workload) and draw it with its own panes; Esc returns to the list with
  the selection, filter and warp where they were. Keep it in the component
  (`Focus`), not in the app: the view cache and retargeting stay simple.
- **External programs** (the log viewer, `v`): the view returns
  `Action::OpenInViewer`; the loop, which owns the terminal, suspends the
  TUI, runs it and restores. Views never touch the terminal. Files handed
  over are private (0600) temp files, removed after.
- **Persisted:** selection, view, pane width, folds and view settings in
  `~/.talos9s/state.yaml` (`state.rs`); text filters are not. Settings that
  mean the same in several views are one setting (`log_wrap`); what
  depends on the cluster is kept per talosconfig context (namespace warp).
  When a saved field changes shape, keep reading the old one (`warp` was
  one name, now a list: `one_or_many`): a field that fails to parse resets
  the whole file to defaults.
- Not doing: `Alt+<hotkey>` to switch views from inside one. `Esc`/`q`
  first is enough, and views keep all their letters.

What each view shows per target:

| View | Node | Control plane / workers | Cluster |
|---|---|---|---|
| Dashboard | node details | nodes table and problems (`group.rs`) | cluster summary |
| Logs | node services | merged across the group | merged across the cluster |
| Network, Storage, Diagnostics | the node | group view | group view (all nodes) |
| Processes | the node | the group's nodes | all nodes |
| Containers, Machine config, Extensions | the node | "Select a node" hint | same hint |
| Volumes, Workloads | on this node | on those nodes | whole cluster |
| etcd, Lifecycle, Security | the target's cluster | same | same |

## Wiring a new view (all of these, or it silently does nothing)

1. `action.rs`: a `Show…` variant with what the view needs (hostname, address, tab).
2. `components/mod.rs`: `pub mod` and `pub use`.
3. `app/view.rs`: a `View::…` and a `ViewComponent::…(Box<…>)` variant,
   with its arms in `each!`, `view()`, and `spawn_refresh` / `adopt_refresh`
   (for a background refresh) or `spawn_refresh`'s `None` arm; settings that
   carry over to the next visit go in `ViewSettings` and `carry_over`.
   Keys, mouse, key hints, drawing, ticks and actions then reach the
   component through `Component` with no more wiring.
4. `app.rs`: the `Show…` handler: `leave_view()`, build the component, set
   the node client (`client.with_node(&address)`), and end with
   `show_component(item, ViewComponent::…(Box::new(c)), load)` (`load`
   starts the first refresh). Anything the refresh needs from the cluster
   each time (a redialled client) goes in the `Action::Refresh` arm.
5. `cluster/mod.rs`: a `NavMenuItem` (`ALL`, `label` with the full name,
   `hotkey`) and its arm in `action_for` (`cluster/actions.rs`).
6. README: the feature row and the key binding.

## Conventions

- `key_hints()` returns the keys that act now (per mode: filter typing, a
  dialog, a submode); the app shows them on the bottom line while the view
  has the keyboard. Views draw no key line of their own. `Esc`/`q` go back
  (clearing a filter first).
- `/` filters lists or searches text; `r` refreshes. Views have no tab bars:
  each mode is its own menu entry, and `Tab` always moves between windows.
- Clicks: every view gets left clicks through `handle_mouse_event`; a click
  on a row selects it (a double-click arrives as `Enter`). Record rows with
  `ui_ext::RowHits::new(area, bordered, header_lines, state.offset(), rows)`
  right after rendering a table or list, and `ui_ext::LineHits` (one
  `push` per drawn line) for lists drawn as text; then
  `rows.clicked(&mouse)` gives the index. A sortable table records its
  header with `ui_ext::ColumnHits::new(area, &widths, &targets)` (the same
  widths it was drawn with) so a click on a column sorts by it, as the
  sort keys do. Other targets (tabs) record their `Rect`s.
- Widths: size to content where possible (see `nodes_pane_width`) instead of
  fixed percentages; never let one long error line set a pane's width.
- Tables whose columns come and go (the group table): build the header and
  every row kind from one column list (`GroupCol`), or offline rows shift.
- Tree and list selection by name, not index: a refresh adds and drops
  nodes.
- Visual selection and copying: `talos9s_core::VisualSelection` and
  `clipboard::copy_lines`.
- Logs from the Kubernetes API (`components/pod_logs.rs`): read with
  `timestamps=true` to merge sources by time, bounded (sources, lines,
  timeout), read again only while following the end, and after the
  selection rests (300 ms). Lines are shown through `logparse` (level,
  JSON/logfmt as message and fields), the same as the Logs view.
- Streams: never drop a backlog wholesale (status events go with it); wrap
  counts drawn rows, not entries, when following the bottom.
- Pull decisions out of `async` handlers into pure functions to test them
  (`follow_step`, `cache_insert`, `nodes_to_query`).
- Test drawing and clicks with `ratatui::backend::TestBackend`.
- Narrow screens: the right pane is 70-120 columns on a 100-120 column
  terminal. A new view goes in `components/layout_tests.rs`, which draws
  every view at those widths and checks its `key_hints()` fit beside
  `<Esc/q/←> tree` on a 100 column screen (85 columns). When a key line
  grows, group keys (`<s/o/l>`, `<f/F>`) or move a pane's keys into that
  pane's hints instead of letting it be cut.
