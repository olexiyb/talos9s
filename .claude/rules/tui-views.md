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
  changed meanwhile). `app.rs` starts them with `spawn_refresh!` into a
  `RefreshJob` and adopts the result when it finishes.
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
  (pane, target)) and ends with `show_in_pane(item, View::…)`.

## Wiring a new view (all of these, or it silently does nothing)

1. `action.rs`: a `Show…` variant with what the view needs (hostname, address, tab).
2. `components/mod.rs`: `pub mod` and `pub use`.
3. `app.rs`:
   - import; `View::…`; `Option<…Component>` field, `None` in `new`
   - `RefreshJob::…` + its arms in `is_finished`, `abort`, `finish!`
   - `draw_workspace` arm, `dispatch_view_key` arm, `update(action)` arm
   - `Action::Tick` arm (component `update(Tick)`), `Action::Refresh` arm
     (start `spawn_refresh!` only when `refresh_job.is_none()`)
   - the `Show…` handler: `leave_view()`, build the component, set the node
     client (`client.with_node(&address)`), start the first refresh,
     `show_in_pane(item, View::…)`
   - `leave_view` / `CachedView` / `restore_view` arms to cache it
   - `dispatch_mouse` if the view handles clicks (otherwise the wheel arrives
     as Up/Down keys)
4. `cluster.rs`: a `NavMenuItem` (`ALL`, `label` with the full name,
   `hotkey`) and its arm in `action_for` (hotkeys come from `hotkey`).
5. README: the feature row and the key binding.

## Conventions

- `key_hints()` returns the keys that act now (per mode: filter typing, a
  dialog, a submode); the app shows them on the bottom line while the view
  has the keyboard. Views draw no key line of their own. `Esc`/`q` go back
  (clearing a filter first).
- `/` filters lists or searches text; `r` refreshes; Tab or 1-n switches tabs.
- Clickable areas: record the `Rect`s drawn (tabs, table area) and hit-test
  them in `handle_mouse_event`; table rows start 2 lines below the table's
  top (border + header) plus `TableState::offset()`.
- Widths: size to content where possible (see `nodes_pane_width`) instead of
  fixed percentages; never let one long error line set a pane's width.
- Test drawing and clicks with `ratatui::backend::TestBackend`.
