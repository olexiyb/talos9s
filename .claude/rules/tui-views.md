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

## Wiring a new view (all of these, or it silently does nothing)

1. `action.rs`: a `Show…` variant with what the view needs (hostname, address, tab).
2. `components/mod.rs`: `pub mod` and `pub use`.
3. `app.rs`:
   - import; `View::…`; `Option<…Component>` field, `None` in `new`
   - `RefreshJob::…` + its arms in `is_finished`, `abort`, `finish!`
   - draw arm, key dispatch arm, `update(action)` dispatch arm
   - `Action::Back`: drop the component
   - `Action::Tick` arm (component `update(Tick)`), `Action::Refresh` arm
     (start `spawn_refresh!` only when `refresh_job.is_none()`)
   - the `Show…` handler: build the component, set the node client
     (`client.with_node(&address)`), start the first refresh, set the view
   - `dispatch_mouse` if the view handles clicks (otherwise the wheel arrives
     as Up/Down keys)
4. `cluster.rs`: hotkey in the key handler and, for menu entries, `NavMenuItem`
   (`ALL`, `label` with the full name, `hotkey`, `navigate_to_selected_menu`).
5. README: the feature row and the key binding.

## Conventions

- Footer lists the view's keys; `Esc`/`q` goes back (clearing a filter first).
- `/` filters lists or searches text; `r` refreshes; Tab or 1-n switches tabs.
- Clickable areas: record the `Rect`s drawn (tabs, table area) and hit-test
  them in `handle_mouse_event`; table rows start 2 lines below the table's
  top (border + header) plus `TableState::offset()`.
- Widths: size to content where possible (see `nodes_pane_width`) instead of
  fixed percentages; never let one long error line set a pane's width.
- Test drawing and clicks with `ratatui::backend::TestBackend`.
