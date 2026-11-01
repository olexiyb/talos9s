---
paths:
  - "crates/talos9s-tui/src/components/diagnostics/**"
  - "crates/talos9s-tui/src/components/group.rs"
---

# Diagnostics checks

```
components/diagnostics/
├── mod.rs        # DiagnosticsComponent (UI)
├── types.rs      # DiagnosticCheck, DiagnosticFix, DiagnosticContext
├── core.rs       # memory, CPU, services, etcd, ...
├── pki.rs        # certificate checks
├── k8s.rs        # Kubernetes client, drain/cordon, CNI detection via the API
├── cni/          # mod.rs (detection + dispatch), flannel.rs, cilium.rs
└── addons/       # mod.rs (CRD + pod detection), cert_manager.rs
```

- Build results with `DiagnosticCheck::pass(id, name, msg)`,
  `fail(id, name, msg, Some(fix))`, `warn(id, name, msg)`, `unknown(id, name)`.
  Return `unknown` when the source can't be read; never `fail` on missing data.
- Give a `DiagnosticFix` when there is a concrete action; it is shown and
  confirmed, never applied silently.
- New check: find the source of truth first (file > API > log), add it to
  `core.rs` or the CNI/addon module.
- New CNI: `cni/<name>.rs`, detection in `cni/mod.rs` (Kubernetes API first,
  CNI config file fallback), its kernel requirements, its pods' health.
- New addon: CRD names in `talos9s_core::constants`, detection in
  `addons/mod.rs`, checks in `addons/<name>.rs` if needed.
- Group views (`group.rs`) list each warning once with the nodes it affects;
  keep check ids and messages stable across nodes so they group.

Checklist for every check:

- What is the source of truth, and can we read it directly?
- If a log is used: is the line validated against current state or time?
- What does it show when the source is unavailable?
- Can it report failure on a healthy system?
