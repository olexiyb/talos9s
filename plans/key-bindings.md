# Design: consistent key bindings

Status: implemented · Author: Olexiy Buyanskyy · 2026-11-02

## Summary

The navigation keys are consistent and stay as they are: arrows and `j`/`k`,
`g`/`G`, `Ctrl+U`/`Ctrl+D`, `/` with `n`/`N`, `Tab`/`Shift+Tab`, `Esc`/`q`
for back, `[` `]`, `Z`, and the log level keys `0`-`5`. The problems are view
letters that mean different things in different views. This plan gives `a`
and `y` one meaning each, stops `q` from quitting by accident, and moves
lifecycle off `y`.

## Problems

| Key | Meanings today | Where |
|---|---|---|
| `a` | auto-refresh | tree, processes, etcd, workloads, interfaces |
| | show all | volumes, group tables, connections |
| | activate all (services pane) | logs |
| `c` | copy fix / defrag command | volumes, diagnostics, etcd |
| | start/stop packet capture | interfaces, connections |
| `y` | copy | logs, processes, connections, diagnostics |
| | Lifecycle view | tree |
| `q` | back | every view |
| | quit | tree: pressing `q` twice to back out leaves the app |
| `l` | Logs | tree |
| | levels pane | logs |
| | listening only | connections |
| | right | diagnostics |
| `R` | restart service, next to `r` refresh | connections |

Letters that each view owns (`d`, `t`, `s`, `f`, `o`) are left alone: the
bottom hint line shows them, and every free letter would be just as arbitrary.

## Changes

1. **`a` is always auto-refresh.** The other meanings of `a` move off it:
   - **`0` sets a filter to all.** In logs, `0` already selects all levels;
     with the services pane open, `0` activates all services. It sets the
     filter rather than toggling it, so pressing it again changes nothing.
   - **`A` toggles show all** in volumes, group tables and connections
     (problems only or everything; press again to go back). `0` isn't used
     for this: a toggle isn't "all", and in connections, processes and
     interfaces the digits `1`/`2` pick the sort column, so `0` would read
     as a sort key.
2. **`y` is always copy.** Copying a fix (volumes, diagnostics) or the defrag
   command (etcd) moves from `c` to `y`. Diagnostics already accepts both, so
   only `y` stays. `c` keeps packet capture in the network views.
3. **Lifecycle moves from `y` to `b`** (boot and upgrade history). `b` is free
   in the tree. Free lowercase letters are `b f g h i u z`.
4. **`q` in the tree doesn't quit at once.** If the view isn't the Dashboard,
   `q` goes to the Dashboard (like `Esc` does today). On the Dashboard, `q`
   asks "Quit talos9s? y/n". `Ctrl+C` still quits at once from anywhere.
5. **Listening only in connections moves from `l` to `L`.** In views, `l` then
   means only the logs levels pane (logs) or right (diagnostics).
6. **`R` restart service:** check that it shows a confirmation dialog and
   writes to the audit log, as drain and reboot do (hard rule in CLAUDE.md).
   Fix it if not; the key stays.
7. **Shift in the tree:** document the rule that holds: `L` and `O` are the
   across-nodes versions of `l` and `o`. `D`, `K` and `C` are uppercase only
   because the lowercase letter is taken; they stay so muscle memory isn't
   broken.

## Code

- Tree hotkeys: `NavMenuItem::hotkey` in `components/cluster/mod.rs`,
  quit in `components/cluster/keys.rs`, a quit confirmation in `app.rs`.
- View keys: `handle_key_event` in `volumes.rs`, `group.rs`, `etcd.rs`,
  `diagnostics/mod.rs`, `multi_logs.rs`, `network/connections.rs`.
- Each view's hint line (bottom bar) and help text change with its keys.
- Tests that press these keys (`volumes.rs`, `multi_logs.rs`,
  `cluster/tests.rs`) are updated, and a test checks that `a` toggles
  auto-refresh in every view that refreshes.

## Compatibility

These are behaviour changes for current users. The changelog for the release
calls out each moved key (`a`/`A`/`0`, `c`/`y`, `y`/`b`, `l`/`L`, `q`), and the
README key table changes with it. No state file migration: `state.yaml`
holds toggles, not key bindings.

## Not doing

- Configurable key bindings: more code than the problem needs right now.
- A `:` command line like k9s: `q` with a confirmation covers the quit case.
- Renaming `D`/`K`/`C` in the tree.

## Done when

- Each key in the problems table has one meaning, or the view owns it and
  the hint line says so.
- clippy, fmt and tests clean; README and the hint lines match the code.
- A live test on `bc`: `q` from a view goes back, `q` in the tree goes to the
  Dashboard and then asks before quitting; `R` asks before restarting
  (cancel with `n`).
