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
  client certs (PEM, handed to tonic's TLS config).

## API notes

- `MachineService.Upgrade` is deprecated since Talos 1.13 (removal planned in
  1.18). Upgrades use `ImageService.Pull` then `LifecycleService.Upgrade`
  (streams progress and an exit code), then a separate `Reboot`
  (see `plans/upgrades.md`).
- Machine config output holds secrets: return it raw only to callers that
  mask it before keeping or showing it.
- Hosts: `host_of` for `host:port` and URLs (`[fd00::2]:2380`); never
  `split(':')`, which breaks IPv6. Match etcd peers by parsed host
  (`has_peer_host`): `contains` takes `10.0.0.1` for `10.0.0.11`.
- Machine config: `spec` is a string holding many YAML documents. Read the
  fields needed and drop the rest (`MachineConfigInfo`). Pod subnets: a
  `kind: KubeNetworkConfig` document (Talos 1.14), else
  `cluster.network.podSubnets`.
- `KubeSpanPeerStatus` has no RTT; show the last handshake.
- gRPC errors by `tonic::Status::code()`, not message text.
- Containers: `system` namespace with the containerd driver for Talos
  services, `k8s.io` with CRI for pods; a CRI sandbox has `id == pod_id`.

## Generated code

`src/generated/` is committed. `build.rs` regenerates it only with
`--features regenerate` (needs protoc). After updating `proto/` from the
Talos repo, regenerate and commit; CI fails if they differ.

## Tests

Parsers get unit tests with real `talosctl` output shapes (no secrets in
fixtures); parse per document so one bad document is skipped. Check a
parser against a live node with an `#[ignore]`d test that prints only the
fields read (e.g. `live_pod_subnets`). gRPC metadata format tests live in
`client.rs`.
