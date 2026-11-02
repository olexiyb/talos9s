---
paths:
  - "crates/talos9s-tui/src/audit.rs"
  - "crates/talos9s-tui/src/components/node_operations.rs"
  - "crates/talos9s-tui/src/components/rolling_operations.rs"
  - "crates/talos9s-tui/src/components/lifecycle.rs"
  - "crates/talos9s-tui/src/components/wizard.rs"
  - "crates/talos9s-tui/src/components/network/services.rs"
  - "crates/talos9s-tui/src/components/diagnostics/k8s.rs"
---

# Operations: reboot, shutdown, drain, upgrade, apply, restart

What talos9s changes on a cluster, and the guards each change needs.

- **Confirm with `y`/`n`** in a dialog naming the target, never with the
  key that opened it or a second Enter: a double press must do nothing.
- **Audit** start, success and failure (`audit::audit_start` …). Entries
  are one line, details escaped (a newline would forge an entry). Check
  the logger is initialized: it once had no caller and nothing was logged.
- **Reboot safety** from `talos9s_core::etcd::EtcdQuorum`: members already
  down count, a failed status call is unknown (never "all healthy"),
  learners don't vote. Rolling reboots check it before each control-plane
  node; decide control plane by role, not by "has an etcd service".
- **Waiting for a node:** it's back when Ready with a new boot ID (or a
  Ready transition after the reboot started), not when a stale Ready=True
  is seen before it went down.
- **Drain** like kubectl: skip mirror and DaemonSet pods, stop on
  unmanaged pods, and on emptyDir pods unless `delete_emptydir_data`; eviction errors are classified,
  PDB blocks reported.
- **Fixes are not operations:** diagnostic fixes and storage remedies are
  shown and copied, never run (`CLAUDE.md`).
- Live tests open these dialogs and cancel with `n`; never confirm one.
