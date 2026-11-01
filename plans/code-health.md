# Plan: code health — correctness fixes, dead code, simplification, tests

Status: phases 1-2 done · Author: Olexiy Buyanskyy · 2026-11-01

## Summary

A read-only review of the whole workspace (~54k lines, excluding
`talos-rs/src/generated`) for simplification, library replacements and
test gaps. It also found bugs, several of which break the principles in
`CLAUDE.md` (no false positives, never run remediation, bound every network
call). This plan orders the work: correctness first, each fix with a
regression test, then dead code, then structural refactors.

Line numbers are from the review on 2026-11-01 and will drift; findings
marked ✔ were re-checked by hand, the rest come from the review and must be
confirmed when the item is picked up.

## Phase 1 — safety and project-rule violations

Each item gets a unit test of the extracted pure logic before the fix.

Done (2026-11-01). Item 1: show and copy only; fixes are `talosctl` commands to copy. Found on the way and fixed: the audit log was
never initialized (`init_audit_logger` had no caller since it was added), so
no operation was ever logged; entries are now kept to one line. The insecure
view (`insecure.rs`) is not reachable (`Action::InsecureApplyConfig` is never
handled; the wizard is the maintenance-mode UI), so it moves to phase 4.

1. **Diagnostics runs remediation** ✔ — `diagnostics/mod.rs:381-437`
   (`apply_pending_fix`) runs `talosctl patch` (`AddKernelModule`),
   `client.apply_configuration` (may reboot) and `service_restart` after a
   confirmation dialog. This contradicts "never run remediation", and none of
   it is audited. Decide: either show and copy the command only, or make this
   an explicit, audited operation and update the hard rule. Default: show and
   copy only.
2. **Wizard and insecure view apply without a dialog** ✔ — `wizard.rs:442`
   (`ConfigReady`: `a`/Enter applies, two Enters from Configure install to
   disk), bootstrap `:459`; `insecure.rs:474-509` confirms with the same key
   that opened the dialog. Add a confirmation dialog with `y`/`n` and write
   the audit log.
3. **Reboot safety says Safe when quorum would be lost** ✔ —
   `node_operations.rs:522-590`: `members_after = total - 1` ignores members
   already down; missing statuses mean "all healthy" (`:569`);
   `url.contains(node_addr)` matches `10.0.0.1` in `10.0.0.11`;
   `address.split(':')` breaks IPv6. `lifecycle.rs:534-560` has the same
   "assume healthy" fallback (`:550`) and re-implements quorum
   (`EtcdQuorumInfo`).
   Fix: one pure function in talos9s-core, e.g.
   `reboot_safety(members, statuses, node) -> Safe | Unsafe | Unknown`, built
   on `QuorumState`, matching peer URLs by parsed host. Use it in
   node_operations, lifecycle, rolling operations and the etcd view.
   Tests: 3/3 healthy → Safe; 3 with 1 down, reboot another → Unsafe;
   no statuses → Unknown; learner excluded; `.1` vs `.11`; IPv6 peer URLs.
4. **Rolling reboot can continue while the node is down** —
   `diagnostics/k8s.rs:1335-1371`: if NotReady isn't seen within 60 s, the
   stale Ready=True is trusted. Require Ready with `lastTransitionTime` after
   the reboot started (or a changed boot ID). `rolling_operations.rs` never
   checks etcd quorum before rebooting a control-plane node: use item 3.
   Extract a pure step planner and readiness predicate; test "never goes
   NotReady", API errors, last node, CP without quorum.
5. **Lifecycle pre-checks pass on unknown** — `all_passed`
   (`lifecycle.rs:574-583`) uses `unwrap_or(true)`; no timeouts on its calls.
   Unknown must not count as passed; wrap calls in timeouts.
6. **Drain skips emptyDir pods silently** — `k8s.rs:984-1030`; kubectl
   aborts instead. Extract `select_pods_to_evict` and test mirror, DaemonSet,
   unmanaged and emptyDir pods.

## Phase 2 — false positives and missing timeouts

Principle 2: showing failure for a healthy system is worse than unknown.

Done (2026-11-01), every row of the table. Also: the tree's etcd summary
counted an empty status answer as 0 healthy; pods Pending under 2 minutes
are not stuck; the Lifecycle/node-ops pod count checks init containers.

| Where | Problem | Fix |
|---|---|---|
| `etcd.rs:701-769` ✔ | status RPC fails → empty statuses → NO QUORUM, all members DOWN | Unknown when the status call fails (via phase 1 item 3) |
| `workloads.rs:477,527` | desired = `status.replicas` (includes surge) → rollouts show Degraded | use `spec.replicas` |
| `workloads.rs` ~850 | `HighRestarts` on lifetime `restart_count` | only when the last termination is recent |
| `workloads.rs` | Failed pods of finished Jobs count as Failing; init containers unchecked | skip Job-owned completed pods; check init container states |
| `linstor_volumes.rs:463,971-983` | missing `disk_state` → "Unknown" → Error; Attaching/Negotiating/DUnknown are Errors | missing = unknown; transitional = at most Warning |
| `zfs.rs:121,163` | ZFSVolume with no status yet → Error | unknown; Pending colored red in `volumes.rs:807` while its issue is a Warning |
| `storage.rs:435-440` | volume phases `located`, `provisioned`, `prepared`, `closed` drawn red | pure phase → `HealthIndicator` mapping |
| `storage.rs:186,197` | failed fetch clears lists → "no disks" | keep the error in `AsyncState` |
| `talosctl.rs:200,232` | `get_volume_status_for_node`, `get_disks_for_node` have no timeout (storage, security call them unwrapped) | route through `get_resource_yaml` |
| `multi_logs.rs` `LogLevel::from_str` | `contains("err")` matches stderr, deferred, `level=info msg="…error…"` | read structured level (`level=`, JSON `level`, klog I/W/E/F) first |
| `flannel.rs:81,148`, `cni/mod.rs:120` | API error → `unwrap_or_default()` → Fail with a patch suggestion | unknown on error |
| `pki.rs:156` | `num_days()` truncation: 23 h left → Expired | compare durations; pass `now` in |
| `core.rs:442` ✔ | "EXPIRED 5 days ago ago" | fix wording |
| `core.rs:331` | talosconfig cert check uses `current_context()` | use `ctx.talos_context` |
| `security.rs:349-355` | every `luks2` volume (TPM/KMS sealed too) → "static/weak" Warning | map by key slots, unknown otherwise |
| `cert_manager.rs:30` | hard-coded `cert-manager` namespace; completed Job pods unhealthy | find by label across namespaces |
| `cilium.rs:194-260` | Hubble Relay / Operator scaled 0/0 → Fail | 0/0 is not a failure |
| `client.rs:2629` | `ConnectionCounts::has_warnings` warns on one CLOSE_WAIT/SYN_SENT | thresholds in `constants`, tested |
| `cluster.rs:2059` | `node_row_status` counts a non-Running service without health check as healthy; `service_ok` (1297) says failed; hard-coded `mem_pct < 90` | one core `node_health() -> HealthIndicator` |
| `diagnostics/mod.rs:700` | empty category shows "Loading..." forever | show unknown / none |
| `k8s.rs:631` | `check_pod_health` double-counts multi-container pods, counts any Pending pod | per pod, Pending past a threshold |

## Phase 3 — crashes and logic bugs

1. **Log timestamp panic** ✔ — `multi_logs.rs:1102` indexes
   `days_in_months[month]` without a range check; `ts[5..7]` byte slicing
   panics on non-ASCII. Replace `datetime_to_unix`, `is_leap_year`,
   `current_year` (~90 lines) with chrono; handle offsets and the klog
   New-Year rollover.
2. **Search highlight panic** — `multi_logs.rs:1561`: offsets found in the
   lowercased copy slice the original. Match char-wise.
3. **Wizard** `format_poll_error` (`wizard.rs:172`) slices `&error[..77]`
   by bytes. **multi_logs.rs:1621** `area.width as usize - 4` underflows.
4. **`MemInfo::usage_percent`** (`client.rs:1959`) underflows when
   available > total → `saturating_sub`. `CpuStat::usage_percent_from`
   on counter reset.
5. **IPv6 hosts** — `split(':').next()` in `with_nodes`,
   `filtered_target_nodes`, `node_from_metadata` (`client.rs:155`),
   `EtcdMemberInfo::ip_address` (`client.rs:2074`), `get_disks_for_context`.
   One host:port helper (keep `config.rs::normalize_endpoint`, drop
   `strip_port` and `endpoint_host`) on `SocketAddr`/`http::Uri`.
6. **Cluster tree selection by index** — `NodeListItem::ControlPlaneNode(cluster, i)`
   survives `adopt_cluster` (`cluster.rs:2440`); when a node expires or
   appears, the cursor moves to a different node. Select by name, re-resolve
   after adopt.
7. **Stream backlog dropped** — `multi_logs.rs:596-630`: more than 500
   events in a tick discards the whole backlog, including
   `StreamEvent::Status`.
8. **Follow + wrap** — scroll counts entries while wrapping draws several
   rows per entry; the newest lines fall below the viewport. Extract
   `wrap_chunks` and the visible-window computation.
9. **Stale interface rates** — `network.rs:694` `update_devices` never drops
   removed interfaces from `data.rates`; totals keep growing.
10. **Audit log injection** — `details` with a newline forges a log line;
    escape it. Add `AuditLogger::with_path` for tests.
11. **Rolling operations `O`** (`cluster.rs:3740`) decides control plane by
    "has an etcd service"; use `node_is_controlplane`.
12. **KubeSpan RTT** — `parse_kubespan_peers_yaml` reads `rtt` from
    `lastUsedEndpoint`, which is a string in real output; check against a
    fixture.
13. **`check_cni_health`** (`core.rs:271-306`) reads `/etc/cni/net.d` as a
    file, never succeeds; drop it for `cni/mod.rs`'s probe.

## Phase 4 — dead code (~1,800 lines)

Confirm with a grep and `cargo build` after removing the `pub use`.

- TUI: `components/logs.rs` ✔ (762 lines, older copy of multi_logs),
  `components/home.rs` ✔, the `AsyncResult` channel (`app.rs:240-262,
  954-970`), `AuditLogger::{read_recent, is_enabled, set_cluster,
  log_progress, log_cancelled}`, `has_pending_restart` (network),
  `create_k8s_client`, `drain_node`, `node_exists`, `are_cni_pods_healthy`,
  `cni_pod_health_summary` (k8s.rs), `detect_ebpf_mode`, `check_hubble_ui`
  (cilium), unused `PkiStatus`/`CertStatus`/`EncryptionStatus` methods,
  `FixAction::{ApplyConfigPatch, ShowDetails, InstallCilium}`.
- talos-rs: `get_volume_status`, `get_disks`, `get_disks_for_context`,
  `get_machine_config`, `get_kubespan_peers`, `get_discovery_members`,
  `get_address_status` + `parse_address_status_yaml` + `AddressStatus`,
  `is_kubespan_enabled`, `check_insecure_connection`,
  `reboot_insecure`/`shutdown_insecure`, blocking `exec_talosctl`,
  `db_size_human`, `db_size_in_use_human`, `offset_human`, `sync_status`.
- talos9s-core: `types.rs`, `MultiSelectList`, `AsyncStateExt`,
  `ErrorCategory`/`categorize_error`/`format_timeout_error`,
  `classify_connection` and friends in `network.rs`, unused `formatting.rs`
  helpers.
- `rolling_operations.rs`: `stop_on_failure` is always true, so the
  `continue` paths are unreachable; collapse the 7 failure blocks.
- Test `test_shuffle_fallback_ips_does_not_panic` only tests fastrand.

## Phase 5 — simplification and library use

### Move to talos9s-core (pure, testable)
- Log parsing from `multi_logs.rs:817-1190`: `parse_line`,
  `extract_timestamp`, `time_to_sort_key`, `clean_message`,
  `parse_kernel_line`, `LogLevel` (~400 lines) → `talos9s_core::logparse`.
- Quorum / reboot safety (phase 1), `node_health`, workload classification,
  storage phase mapping, `most_common`, `used_of_total`, `update::is_newer`,
  `UpdateCheck::due`, DNS and route parsers from network.rs.
- Thresholds into `constants`: memory (processes 85/60, cluster 90 vs core
  80/90), fill 95/85 (`volumes.rs:992`, `linstor_volumes.rs:850`), ZFS quota,
  etcd, cert 30/7 (`pki.rs:69`), clock skew (`cluster.rs:1233`), connection
  warnings; `AUTO_REFRESH_INTERVAL_SECS` and the hard-coded `5`
  (`cluster.rs:2707`) → `refresh_intervals`.

### One implementation instead of several
- Health: `linstor_volumes::Severity`, `EtcdCheck { error }`,
  `CertStatus`/`EncryptionProvider`/`ItemStatus` colors → `HasHealth` +
  `ui_ext`.
- Formatters: `format_kib`, client.rs byte/rate formatters,
  `WorkloadHealthComponent::format_age`, `short_age` → core.
- Kube client bootstrap (13 call sites), talosconfig loading
  (`core.rs:322`, `security.rs:198,312`), the Ok/Err/timeout match.
- Selection: network's 8 and processes' 4 `select_*` methods, storage's
  wrap-around → ratatui `TableState::select_next/previous/first/last` or
  core `SelectableList`.
- Text input: one `QueryInput` for logs, processes, multi_logs, node_info.
- Visual selection + yank: network copies multi_logs.
- Popup centering (~15 places, some ignore `area.x/y`) → `Rect::centered`.
- Text wrapping: `warning_lines` + `wrap_list` → one helper measuring with
  `Line::width()`.
- wizard / insecure: `NodeType`, config form, disk table (~150 lines).
- CNI: `flannel.rs:44-74` vs `check_cni_pods`; `k8s.rs` `CniInfo`/`CniPodInfo`
  → return core types and drop the conversions; deployment readiness helper
  (cilium, cert-manager, k8s); addons table-driven (`addons/mod.rs`, also
  avoids 5 API calls per refresh).
- workloads: one `push_workload` for Deployment/StatefulSet/DaemonSet
  (~90 lines).
- cluster.rs: 14 `get_node_*_for` getters → one generic lookup; the
  `node_ips` rebuild (3×); inline `cp_ip` → `control_plane_ip()`; group table
  column spec shared by online and offline rows.
- talos-rs: `with_nodes` → `filtered_target_nodes()`; `version()` field
  mapping; BPF programs as a compact table (~280 lines);
  `kubespan_enabled_in` → `parse_kubespan_settings`; one document splitter
  (`resource_documents`).
- LINSTOR "established" predicate (stale-bit issues vs remedies disagree,
  `linstor_volumes.rs:1031` vs `:1287`).

### Library replacements (no new top-level crates beyond those noted)

| Hand-written | Replace with |
|---|---|
| `talosctl.rs` YAML parsers (~300 lines) | serde derive + serde-saphyr, per document so one bad document is skipped |
| LINSTOR REST and ZFS CRD JSON walking (~120 lines) | serde derive; `Vec<Value>` then `from_value` per item |
| kubeconfig walking (`core.rs:392`, `security.rs:248`) | `kube::config::Kubeconfig` |
| date math in multi_logs | chrono |
| enum names via `format!("{:?}")` in `state.rs`, `NavMenuItem::ALL` | strum (direct dep, already in tree via ratatui) |
| `hex_to_ip`, loopback checks, host:port | `std::net`, `http::Uri` |
| `format_grpc_error` on message text | `tonic::Status::code()` |
| RBAC role from subject string (`security.rs:219`) | x509-parser `iter_organization()` |
| `--insecure` requires `--endpoint` check (`main.rs:106`) | clap `requires` |

Kept hand-rolled on purpose: debugfs DRBD parsing, `parse_quantity`,
`parse_duration_to_ms`, update check on hyper, version parsing without
semver, `mask_config`, byte formatting (once deduplicated).

## Phase 6 — structure

1. **app.rs view dispatch.** 13 `Option<XComponent>` fields are matched by
   hand in ~10 places (`RefreshJob`, `finish!`, `dispatch_view_key`,
   `view_mouse`, hints, `draw!`, Tick, Refresh, `leave_view`, `restore_view`).
   Hold one `Box<dyn ViewComponent>`; refresh returns a boxed result adopted
   by downcast; carried-over settings via `carry_over(&self, &mut Settings)`.
   ~450 lines; update `.claude/rules/tui-views.md`.
2. **Split cluster.rs** into `components/cluster/{data, helpers, tree,
   stats, keys, frame, group_table, details}`; `draw_details_pane` into
   per-section `…_lines()` builders that can be tested like
   `diagnostics_line`.
3. **Split network.rs** into `network/{mod, interfaces, connections,
   kubespan, file_viewer, capture}`.
4. **Group diagnostics** (`group.rs:357`) runs cluster-wide checks once per
   node; run them once per cluster.

## Tests to add (beyond the regression tests above)

- `mask_config` leaks: single-line JSON `auths`, PGP private key blocks,
  `https://user:pass@host`, flow maps.
- Cert expiry at -5, 0 (23 h left), 7, 30, 31 days.
- `extract_timestamp` / `time_to_sort_key`: JSON `ts` seconds vs ms, Go zero
  time, `+02:00`, invalid months.
- Processes: per-process CPU % (PID reuse, elapsed < 0.1 s),
  `rebuild_display_list_into` with orphans and ppid cycles.
- Network: `parse_routing_table_static`, `decode_route_flags`,
  `parse_dns_config_static`, `connection_matches_interface` (10.x node
  networks are not pod networks).
- talos-rs: `parse_duration_to_ms`, `format_grpc_error` branches,
  `parse_volume_status_yaml` fallbacks, BPF jump targets in range,
  `ProcessState::parse`, `ConnectionState::from_proto`.
- Cluster: hotkeys unique and not colliding with q/r/j/k/l/o/O/a/space;
  menu bounds; Tab cycles; `navigate_up/down` over folded groups.
- App: view cache eviction and replacement; `follow_selection` debounce as a
  pure `due()`.
- `DiagnosticsSummary::of`, `CniInfo` detection precedence,
  `toggle_current` renumbering, wizard key transitions.
- Extract for testability: `ClusterData::refresh` fallback chain →
  `nodes_to_query(...)`, `node_ips_from(...)`.

## Order and verification

Phases 1-3 as small commits, one finding (or one shared root cause) each,
with its test. Each fix follows "Before merging" in `CLAUDE.md`: create the
error condition (e.g. an etcd member down on a test cluster, a node
unreachable), check the verdict, fix it, check that it shows healthy again.
Phase 4 in one commit. Phases 5-6 one area per commit, no behavior change,
tests green before and after.

## Open questions

1. Phase 6 item 1: is a trait object for views acceptable, or keep the
   enum and generate the matches with a macro?
