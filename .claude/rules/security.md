---
paths:
  - "src/main.rs"
  - "crates/talos9s-tui/src/app.rs"
  - "crates/talos9s-tui/src/audit.rs"
  - "crates/talos9s-tui/src/clipboard.rs"
  - "crates/talos9s-tui/src/operations.rs"
  - "crates/talos9s-tui/src/state.rs"
  - "crates/talos9s-tui/src/update.rs"
  - "crates/talos9s-tui/src/components/node_info.rs"
  - "crates/talos9s-tui/src/components/pod_logs.rs"
  - "crates/talos9s-tui/src/components/network/capture.rs"
  - "crates/talos9s-tui/src/components/diagnostics/k8s.rs"
  - "crates/talos9s-core/src/logparse.rs"
  - "deny.toml"
  - ".github/workflows/**"
---

# Security: trust boundaries

talos9s runs on an operator's machine with admin access to Talos nodes and
Kubernetes. What it reads from a cluster is data, never commands; what it
writes stays private. For a review (`/security-review`, a full audit), these
are the boundaries to check.

## Secrets

- **Masked before stored or drawn:** the machine config and anything that
  may hold secrets go through `node_info::mask_config` (values under secret
  keys, inline manifests and files, registry auth in JSON, URL passwords,
  PGP and PEM private key blocks). Operation run files mask each line with
  the same function (`operations::mask`).
- **Never in logs:** the tracing log, the audit log and run files never get
  a machine config, kubeconfig, token or key, not even at debug level.
  Kubeconfig contents are never logged; a path and a context name are fine.
- **Copied text** follows the same rule: a copied command or line never
  carries a secret.

## Credentials

- The talosconfig (client cert and key) is read, never written or copied.
- The admin kubeconfig fetched from a control plane node lives in memory
  only. `--kubeconfig` is used as given, with no fallback to admin.
- A kubeconfig context for another cluster is never used: the context is
  picked by CA (`context_for_ca`), so a view or a drain can't hit the wrong
  cluster. Never the cluster VIP for the Talos API (`CLAUDE.md`).

## Cluster data is untrusted

Names, labels, annotations, event messages, log lines and API errors come
from the cluster, and anyone who can create a pod controls some of them.

- **Drawn on screen:** terminal escape codes in log lines are removed
  (`logparse::strip_ansi`, in the pod and container logs). Text from other
  sources must not reach the terminal with control characters either.
- **In copied commands:** a value put into a `kubectl`/`talosctl` command
  must not add arguments or shell syntax. Kubernetes names (DNS labels)
  are safe as they are; anything freer (labels, annotations, paths, event
  text) needs quoting.
- **Never run:** talos9s shows and copies fixes, it doesn't run them
  (`CLAUDE.md`). Destructive operations need a `y`/`n` confirmation and go
  to the audit log (`.claude/rules/operations.md`).

## Files and processes

- **Private files** (0600, directory 0700): run files in
  `~/.talos9s/operations/` (`operations::create_private`), the default
  tracing log (`main.rs`), the temp file for the external log viewer, and
  packet captures. A new file that may hold cluster data is created the
  same way. The audit log and `state.yaml` hold no secrets and use the
  default mode; a `--log-file` path is created with the default mode too.
- **Audit entries** are one line each: newlines in details are replaced
  (`audit.rs`), so a value can't forge another entry.
- **External viewer** (`TALOS9S_LOG_VIEWER`, default `hl`): run without a
  shell, the command split on whitespace and the temp file passed as the
  last argument, then removed. The variable is the user's own; the file
  name is ours, never from the cluster.
- **Update check** (`update.rs`): one bounded HTTPS GET to the GitHub
  releases API, at most daily; off with `--no-update-check`,
  `TALOS9S_NO_UPDATE_CHECK` or `DO_NOT_TRACK`. It only reports a version;
  it never downloads or runs anything.

## Supply chain

- `cargo deny check` (`deny.toml`: advisories, licenses, sources) runs in
  CI; a new dependency must pass it.
- Releases come from the `v*` tag workflow (cargo-dist). The release and CI
  workflows are part of the trust boundary: a change to them gets the same
  review as code.

## Open checks

Not verified when this was written; worth a look in the next review:

- Control characters in cluster text outside the log views (event
  messages, annotations, API errors) reaching the terminal.
- Copied commands that embed a label, annotation or path without quoting.
- **Viewer temp files** (`workloads::write_private`, used by Workloads,
  Containers and Operations): a predictable name in the temp dir
  (`talos9s-logs-<pid>.log`) opened with `create`, not `create_new`. In a
  shared `/tmp` (Linux) another user can plant a symlink there, and the
  write follows it; an existing file also keeps its own mode. A random
  name with `create_new` (or a private directory) closes it.
