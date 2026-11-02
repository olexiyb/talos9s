---
paths:
  - "crates/talos9s-tui/src/linstor.rs"
  - "crates/talos9s-tui/src/linstor_volumes.rs"
  - "crates/talos9s-tui/src/zfs.rs"
  - "crates/talos9s-tui/src/orphans.rs"
  - "crates/talos9s-tui/src/components/volumes.rs"
  - "crates/talos9s-tui/src/components/storage.rs"
---

# Storage: LINSTOR/DRBD, ZFS

## Sources

- LINSTOR controller REST API (through the Kubernetes API proxy): resources,
  definitions, resource groups. Its view can lag the kernel: "UpToDate" in
  LINSTOR doesn't mean the kernel agrees.
- Kernel state over the Talos API (`read_file`): `/sys/block/drbd<minor>/size`,
  DRBD debugfs `resources/<res>/connections/<peer>/<vol>/proc_drbd`
  (`cs:`, `oos:`, suspended flag, resync progress) and `.../debug`
  (`ap_in_flight`). debugfs format may change without notice: anything
  unreadable or unparsable is unknown, never a problem.
- Kernel log (Dmesg API) only to explain a state, e.g. why a link is
  StandAlone: the reason counts only if it precedes the link's latest
  transition into that state. The ring buffer rotates, so it is often gone;
  fall back to describing the state.
- ZFS LocalPV: its CRDs plus the Mounts API for usage.

## What matters

- Out-of-sync bits toward the Primary on an Established link can make the
  Primary a SyncTarget after a short link loss and fail its I/O
  (LINBIT/drbd#153). Report as an error.
- StandAlone links never reconnect by themselves (split-brain, manual
  disconnect). One issue per pair, with each end's state.
- Resyncs in progress are warnings with progress, not errors.
- A missing `disk_state` is unknown; transitional states (Attaching,
  Negotiating, DUnknown) are at most a Warning. A ZFSVolume with no status
  yet is unknown; volume phases `located`, `provisioned`, `prepared`,
  `closed` are normal.
- A failed fetch keeps its error in `AsyncState`; clearing the lists shows
  "no disks", which is a false statement.

## Remedies

- Only show and copy commands (`kubectl -n <ns> exec <satellite pod> -c
  linstor-satellite -- drbdadm …`); never run them. Satellite pods carry
  `app.kubernetes.io/component=linstor-satellite`; use a placeholder when the
  pod is unknown.
- Suggest commands only for provably safe cases (e.g. both ends Secondary and
  in sync with an Established Primary). Never make the Primary a SyncTarget;
  never discard the Primary's data. Otherwise give a note without commands.
- Nothing while a resync runs.

Tests use recorded states from real incidents (2026-10-17 resize, 2026-10-31
split-brain); add a fixture for each new case.
