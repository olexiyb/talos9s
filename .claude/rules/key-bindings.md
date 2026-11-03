---
paths:
  - "crates/talos9s-tui/src/app.rs"
  - "crates/talos9s-tui/src/components/**"
  - "README.md"
---

# Key bindings

A letter means the same thing in every view. Before giving a key a new
meaning, grep the components for `KeyCode::Char('x')` and check the tables
below.

## Notation in hints

- Every key in a hint line or dialog is wrapped in angle brackets, as k9s
  does: `<y>`, `<Esc/q>`, `<Enter/→>`, `<[> <]>`. Square brackets are for
  status labels (`[LISTEN ONLY]`, `[AUTO:OFF]`), never keys.
- Alternatives with `/`, `Esc` before `q`: `<Esc/q>`, `<j/k>`, `<g/G>`.
- Modifiers spelled out with `+`: `<Ctrl+C>`, `<Shift+1-5>`,
  `<Alt/Shift+drag>`; no symbols (`⇧`, `⌥`, `^C`). Arrows as `←` `→` `↑/↓`.
- Named keys: `Enter`, `Esc`, `Tab`, `Space`, `Home`, `End`.
- Colour: keys use `ui_ext::KEY` (cyan), labels dim or dark gray. Only the
  answers of a confirmation dialog differ: `KEY_YES` (`<y>`, green) and
  `KEY_NO` (`<n>`, red). Status labels keep their own colours.
  `layout_tests::keys_share_one_style` checks every view's key line.

## Shared meanings (don't reuse these for something else)

| Key | Meaning |
|---|---|
| `j`/`k`, arrows | move the selection (always both) |
| `g`/`G`, `Home`/`End` | top / bottom |
| `Ctrl+U`/`Ctrl+D`, `PageUp`/`PageDown` | page up / down |
| `/`, `n`/`N` | filter or search, next / previous match |
| `Esc`, `q` | back: close a menu, filter or search first, then hand the keyboard to the tree. `q` doubles for `Esc` because some terminals (IntelliJ's) never pass `Esc` |
| `r` | refresh |
| `a` | toggle auto-refresh, only that; a view that always refreshes ignores it |
| `A` | toggle show all (or issues only, real devices only); press again to go back. Saved in `state.yaml` |
| `0` | set a filter to all (logs: all levels, or all services/nodes with that pane open). It sets, never toggles |
| `1`, `2`, ... | in tables: sort column. In logs: levels |
| `y` | copy (a line, a selection, a fix or defrag command) |
| `W` | warp into namespaces (Workloads, Volumes): the picker, shared by both views and saved per cluster |
| `w` | wrap long lines (logs); one saved setting for every logs pane |
| `V` | visual (line) selection |
| `Space` | toggle the selected entry or fold a group |

Don't use `c` for copy: it's packet capture in the network views.

## Tree

- Menu hotkeys live in `NavMenuItem::hotkey` (`components/cluster/mod.rs`).
  Uppercase means the across-nodes version of a view (`L` logs merged).
  `D`, `K` and `C` are uppercase only because the
  lowercase letter was taken; don't add more like that.
- Keys reserved in the tree, not for menu entries: `q r j k l a Space Z [ ]`
  (checked in `cluster/tests.rs`). Free lowercase letters: `f g h i u z`.
- `q` never quits at once: from a view it returns to the Dashboard, in the
  Dashboard's services list it hands the keyboard back, and on the Dashboard
  it asks "Quit talos9s?" (`App::confirm_quit`; `y`/`Enter` quits, any other
  key cancels). `Ctrl+C` quits from anywhere, except while an operation
  runs: then it says so, and a second `Ctrl+C` within 2 s quits.

## Keys a view owns

Letters outside the table above (`d`, `t`, `s`, `f`, `o`, `l`, `L` ...) may
mean different things per view, because keys go only to the focused side.
Each must appear in that view's `key_hints()`. Prefer uppercase for a second
filter of the same kind (`L` listening only next to `A` all in connections).

## Actions that change state

- Never on a lowercase key next to a harmless one; use uppercase (`R`
  restart) or the Operations view (`o`).
- Always a confirmation (`<y>` / `<n/Esc>`) and `audit_start` /
  `audit_success` / `audit_failure` (`crate::audit`), as drain, reboot and
  service restart do.

## Keys that only sometimes act

- Show a key only while it does something: `<p>` (previous run) only when a
  container shown has one, `<n/N>` instead of `</>` once a search is kept.
  A key that can't act is ignored, not an error.
- A view with modes (a focused view, a picker, a search being typed) has a
  key line per mode. `layout_tests` checks only the default one; test every
  mode's width yourself (`focus_key_line_fits`, `p_lists_pods`).
- When the line is full, leave out keys that mean the same in every view
  (`<j/k>`, `<r>`, `<PgUp/PgDn/g/G>`) before shortening the view's own.

## Pickers and text input

- While a picker or a text input is open the view `captures_text()`, so
  letters type and layout keys wait. Move with `↑`/`↓` there (`j`/`k`
  are letters), `Esc` closes, `Enter` picks.
- In the namespace picker `Space` marks several, a filter ending in `*` is
  a prefix, and `0` (all namespaces) and `1`-`4` (a recent warp) pick
  until something is typed.

## When a key changes

- Update the view's `key_hints()`, its handler, tests that press it, and the
  README Keys section.
- Moving an existing key is a behaviour change: call it out in the release's
  changelog section.
- Test: a key toggles what it should (e.g. `a_toggles_auto_refresh` in each
  view with auto-refresh); a dialog key test opens it and cancels.
- Live test the new key on a cluster (see CLAUDE.local.md); state-changing
  keys only up to the dialog, cancel with `n`. Rebuild the binary first
  (`cargo build`): clippy and test don't.
