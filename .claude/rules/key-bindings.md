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
| `A` | toggle show all (or issues only); press again to go back |
| `0` | set a filter to all (logs: all levels, or all services/nodes with that pane open). It sets, never toggles |
| `1`, `2`, ... | in tables: sort column. In logs: levels |
| `y` | copy (a line, a selection, a fix or defrag command) |
| `V` | visual (line) selection |
| `Space` | toggle the selected entry or fold a group |

Don't use `c` for copy: it's packet capture in the network views.

## Tree

- Menu hotkeys live in `NavMenuItem::hotkey` (`components/cluster/mod.rs`).
  Uppercase means the across-nodes version of a view (`L` logs merged, `O`
  rolling operations). `D`, `K` and `C` are uppercase only because the
  lowercase letter was taken; don't add more like that.
- Keys reserved in the tree, not for menu entries: `q r j k l o O a Space Z [ ]`
  (checked in `cluster/tests.rs`). Free lowercase letters: `f g h i u z`.
- `q` never quits at once: from a view it returns to the Dashboard, in the
  Dashboard's services list it hands the keyboard back, and on the Dashboard
  it asks "Quit talos9s?" (`App::confirm_quit`; `y`/`Enter` quits, any other
  key cancels). `Ctrl+C` quits from anywhere.

## Keys a view owns

Letters outside the table above (`d`, `t`, `s`, `f`, `o`, `l`, `L` ...) may
mean different things per view, because keys go only to the focused side.
Each must appear in that view's `key_hints()`. Prefer uppercase for a second
filter of the same kind (`L` listening only next to `A` all in connections).

## Actions that change state

- Never on a lowercase key next to a harmless one; use uppercase (`R`
  restart) or the operations dialog (`o`).
- Always a confirmation (`<y>` / `<n/Esc>`) and `audit_start` /
  `audit_success` / `audit_failure` (`crate::audit`), as drain, reboot and
  service restart do.

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
