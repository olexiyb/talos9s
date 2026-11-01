# CLAUDE.md - talos9s

talos9s is a terminal UI for Talos Linux Kubernetes clusters: nodes, services,
logs, etcd, network, storage (LINSTOR/DRBD, ZFS), diagnostics and node
operations. It is a fork of [talos-pilot](https://github.com/Handfish/talos-pilot)
(MIT). It talks to nodes over the Talos API (`:50000`) and to Kubernetes
through a kubeconfig fetched from a control-plane node.

<!-- Topic guidance lives in .claude/rules/*.md and loads only when matching
files are opened. Personal cluster details go in CLAUDE.local.md (gitignored). -->

## Layout

```
src/main.rs                 # CLI (clap), logging, starts the TUI
crates/talos-rs/            # Talos client: gRPC (tonic) + `talosctl` subprocess for COSI resources
crates/talos9s-core/        # Pure logic shared by the TUI: health indicators, formatting, AsyncState
crates/talos9s-tui/         # ratatui app: app.rs (views, refresh jobs), components/ (one per view)
plans/                      # Design plans, reviewed before implementation
test-clusters/              # QEMU/Docker Talos clusters for integration testing
```

Stack: Rust 2024, tokio, ratatui 0.30 + crossterm, tonic/prost 0.14, kube 4,
color-eyre + thiserror.

## Commands

```bash
cargo build                                          # no protoc needed
cargo test --all
cargo clippy --all --all-targets -- -D warnings      # must be clean
cargo fmt --all -- --check                           # must be clean
cargo build -p talos-rs --features regenerate        # only after changing crates/talos-rs/proto/ (needs protoc); commit src/generated
cargo install --path . --locked                      # installs the `talos9s` binary
```

Live tests against real clusters are `#[ignore]`d and read
`TALOS9S_LIVE_CONTEXT` (e.g. `TALOS9S_LIVE_CONTEXT=bc cargo test live_scan -- --ignored --nocapture`).

## Principles

1. **State over logs.** Health comes from current state: files and procfs
   (tier 1), Talos/Kubernetes APIs (tier 2). Logs are tier 3, a last resort,
   and a log line counts only when tied to the current state (e.g. the latest
   transition matches what the API reports now), never because it exists.
2. **No false positives.** Showing failure for a healthy system is worse than
   showing unknown. When a source can't be read or parsed, say unknown.
3. **Degrade gracefully.** A missing source shows unknown or an error for that
   part only; nothing crashes, other parts keep working.
4. **Bound every network call.** Wrap Talos, Kubernetes and `talosctl` calls
   in a timeout: kube-rs waits minutes by default, a dead node must not hang
   a view. Slow refreshes run off the event loop (see `.claude/rules/tui-views.md`).

## Hard rules

- **Never run remediation.** Fix suggestions (drbdadm, kubectl, talosctl) are
  shown and copied, never executed by talos9s. Destructive operations (drain,
  reboot, shutdown) need an explicit confirmation dialog and go to the audit log.
- **Never show secrets.** Anything that may hold them (machine config, Secret
  manifests) is masked before it is stored or drawn. Never print secrets in
  logs or tool output.
- **COSI is not reachable on `:50000`.** A gRPC COSI client gets
  `PermissionDenied`; read resources with `talosctl get` (talos-rs `talosctl.rs`).
- **Never use the cluster VIP for the Talos API.** Address nodes directly; the
  VIP needs a healthy control plane, so it fails exactly when diagnostics matter.
- **talos9s-core stays pure:** no network, no filesystem, unit-tested.

## Conventions

- Components hold async data in `AsyncState<T>` (talos9s-core) and UI state
  (selection, scroll, filters) outside it.
- Health enums implement `HasHealth`; colors come from the extension traits in
  `talos9s-tui/src/ui_ext.rs`. Don't duplicate indicator logic.
- Thresholds and refresh intervals live in `talos9s_core::constants`.
- Comments explain why, not what; match the surrounding code's density.
- New user-facing features get a row in README.md (features, key bindings).

## Before merging

1. clippy and fmt clean, all tests pass.
2. No false positives: create the error condition, fix it, the check shows healthy again.
3. Graceful degradation: with the API or node unreachable, nothing crashes or hangs.
4. Stale log immunity: old log lines don't change the current verdict.

## Documentation

| Doc | Purpose |
|-----|---------|
| `README.md` | User-facing features, installation, key bindings |
| `CHANGELOG.md` | Release notes |
| `plans/*.md` | Design plans (e.g. `plans/upgrades.md`) |
| `.claude/rules/*.md` | Guidance per area, loaded with the matching files |
