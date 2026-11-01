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
| `formatting` | `format_bytes`, `format_duration`, `format_percent`, `pluralize` |
| `selection` | `SelectableList<T>`, `MultiSelectList<T>` |
| `errors` | `format_talos_error`, `ErrorCategory`: user-facing error text |
| `network` | port → service names, connection classification |
| `diagnostics` | `CheckStatus`, `CniType`, `CniInfo`, `PodHealthInfo` |
| `constants` | thresholds, CRD lists, refresh intervals |
| `types` | shared plain types |

If logic in the TUI can be tested without a terminal or a cluster, it belongs
here.
