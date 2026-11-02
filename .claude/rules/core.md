---
paths:
  - "crates/talos9s-core/**"
---

# talos9s-core

Pure logic used by the TUI, with no I/O: no network, filesystem, terminal or
async runtime calls. Everything here has unit tests (doc tests for public
helpers).

| Module | Holds |
|--------|-------|
| `indicators` | `HealthIndicator`, `HasHealth`, `QuorumState`, `SafetyStatus` |
| `async_state` | `AsyncState<T>`: loading, data, error, retry, auto-refresh timing |
| `formatting` | `format_bytes`, `format_kib` (storage, KiB/GiB), `format_eta`, `format_duration_human`, `truncate_string`, `pluralize` |
| `selection` | `SelectableList<T>`, `VisualSelection` (anchor + cursor, for `V`/`y`) |
| `errors` | `format_talos_error`: user-facing error text |
| `network` | port → service names, `cidr_contains`, route table and resolv.conf formatting |
| `diagnostics` | `CheckStatus`, `CniType`, `CniInfo`, `PodHealthInfo` |
| `constants` | thresholds, CRD lists, refresh intervals |
| `convert` | saturating numeric conversions (`to_u16`, `float_to_u64`, …) instead of `as`; `parse_quantity` (Kubernetes quantities) |
| `etcd` | `EtcdQuorum`: quorum and whether a node can go down |
| `logparse` | log line timestamp, level and message; search match ranges |
| `stats` | `most_common`, `used_of_total`, `used_pct`, `gib` |
| `version` | `is_newer`, `check_due` (update check) |

If logic in the TUI can be tested without a terminal or a cluster, it belongs
here.

Bugs that showed up here before:

- Slicing a `&str` at a byte offset found in another string (a lowercased
  copy, a fixed `[..77]`) panics on non-ASCII. Cut at char boundaries or
  match char-wise; `clippy::string_slice` warns, and an `allow` needs a
  comment saying why each index is a boundary.
- Counters go backwards (reboot, PID reuse, `available > total`): use
  saturating math and clamp percentages to 0-100.
- Dates: chrono, never hand-rolled month tables; keep offsets, and give
  invalid or zero dates a key that sorts below real ones.
