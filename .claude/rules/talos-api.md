---
paths:
  - "crates/talos-rs/**"
---

# talos-rs: the Talos client

- `client.rs`: gRPC over tonic (`MachineService` etc.). `with_node(ip)`
  targets one node; responses carry per-node metadata.
- `talosctl.rs`: COSI resources through `talosctl get … -o yaml` (the COSI
  API isn't exposed on `:50000`). Every call goes through
  `get_resource_yaml` / `get_resource_yaml_within` with a timeout (10s, longer
  for large resources such as the machine config); parse with serde-saphyr
  into `serde_json::Value`.
- `config.rs` / `auth.rs`: talosconfig (`$TALOSCONFIG`, `~/.talos/config`),
  client certs (PEM via rustls-pki-types).

## API notes

- `MachineService.Upgrade` is deprecated since Talos 1.13 (removal planned in
  1.18). Upgrades use `ImageService.Pull` then `LifecycleService.Upgrade`
  (streams progress and an exit code), then a separate `Reboot`
  (see `plans/upgrades.md`).
- Machine config output holds secrets: return it raw only to callers that
  mask it before keeping or showing it.
- Containers: `system` namespace with the containerd driver for Talos
  services, `k8s.io` with CRI for pods; a CRI sandbox has `id == pod_id`.

## Generated code

`src/generated/` is committed. `build.rs` regenerates it only with
`--features regenerate` (needs protoc). After updating `proto/` from the
Talos repo, regenerate and commit; CI fails if they differ.

## Tests

Parsers get unit tests with real `talosctl` output shapes (no secrets in
fixtures). gRPC metadata format tests live in `client.rs`.
