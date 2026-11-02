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
5. `cluster.rs`: a `NavMenuItem` (`ALL`, `label` with the full name,
   `hotkey`) and its arm in `action_for` (hotkeys come from `hotkey`).
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
  `rows.clicked(&mouse)` gives the index. Other targets (tabs, column
  headers) record their `Rect`s.
- Widths: size to content where possible (see `nodes_pane_width`) instead of
  fixed percentages; never let one long error line set a pane's width.
- Test drawing and clicks with `ratatui::backend::TestBackend`.
