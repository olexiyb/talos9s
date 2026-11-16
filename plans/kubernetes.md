# Design: Kubernetes across every cluster, read-only

## Summary

talos9s already shows every talosconfig context at once. Its Kubernetes
side grows along that axis: what's broken in every cluster, a search
across clusters, a resource browser for any kind (CRDs included),
comparison between clusters, and logs by selector across clusters. All of
it read-only. Changes go through GitOps; talos9s shows state and copies
commands.

## Status

Built (2026-11-07), all phases, as the Kubernetes (`i`) and GitOps (`g`)
views; the rules are in `.claude/rules/kubernetes.md`. Where it differs
from the design below:

- **Workloads across clusters** is the Kubernetes view's Problems list,
  not a CLUSTER column in the Workloads view. The Workloads view stays per
  cluster (focus, NODES, the pod list); Problems uses the same health
  rules on every cluster.
- **Flux became GitOps:** lab and staging run Argo CD and prod neither, so
  the list has Argo CD Applications (health and sync, out of sync as
  degraded) next to Flux's objects.
- **Search is `s`;** `/` keeps its meaning everywhere, a filter of the list.
- **ConfigMap data** is hidden in YAML and diffs (`mask_config` hides
  everything under `data:`).
- Open questions, as built: every cluster refreshes at the same rate
  (`VERY_SLOW`); the kind picker shows a curated list until you type;
  the GitOps source is shown, no local checkout is read.

## Why

- k9s is one context at a time, switched with `x`; finding "where does
  this run, and is it healthy everywhere?" means visiting each cluster.
- talos9s already holds a Kubernetes client per cluster, picked by CA, and
  the Workloads view's rules (health from status, each list stands alone).
- With lab, prod and staging, most questions are cross-cluster: which
  version of a chart runs where, why one cluster differs, what's broken
  anywhere.

## Read-only, always

No edit, apply, delete, scale, restart, exec or port-forward, not even
behind a confirmation. Cluster state comes from the GitOps repo; a change
made from a terminal is drift Flux reverts or, worse, doesn't. A
read-only tool can't change the wrong cluster, which in a multi-cluster
view is the main risk. Node operations (drain, reboot, upgrades) stay,
as they are Talos operations, not cluster state that GitOps holds.

What talos9s offers instead of a change: the `kubectl` command to copy
(`describe`, `logs --previous`, `rollout status`, `get -o yaml`), and the
path to look at in the GitOps repo where a Flux object names it
(Kustomization `path`, HelmRelease chart and values source).

## Goals

1. **Workloads across clusters.** The Workloads view with a CLUSTER
   column and every context in one list, worst first. A filter narrows it
   to one cluster.
2. **Search.** `/` across clusters for a name, label selector or image:
   pods, workloads, Services, ConfigMaps, CRD objects.
3. **Resource browser.** Pick a kind from discovery (built-ins and CRDs:
   LINSTOR, cert-manager, Flux, tuppr), list it across clusters with name,
   namespace, age and a status summary. Detail: describe-like summary,
   YAML, the object's events.
4. **Compare.** The same object in two clusters side by side, diffed:
   image tags, HelmRelease versions and values, ConfigMap data.
5. **Flux.** Kustomizations and HelmReleases with Ready, the revision and
   the last error, across clusters. Suspended objects are called out
   (suspended means GitOps isn't applying).
6. **Logs by selector** across clusters, in the existing pod-logs view.

## Non-goals

- Any change to cluster state (above).
- Secrets' values. Secrets are listed (name, type, keys, age), never their
  data, not in YAML or diffs.
- Metrics and graphs.
- Replacing `kubectl` for scripting.

## Rules carried over

From `.claude/rules/workloads.md` and `CLAUDE.md`:

- **One client per cluster,** chosen by CA (`context_for_ca`); a context
  for another cluster is never used. With `--kubeconfig` its contexts are
  matched the same way per cluster; a cluster with no match is
  "no kubeconfig" for that cluster only.
- **Each cluster stands alone.** Every cluster's lists are fetched in
  parallel, each in its own timeout; a 403, a timeout or an unreachable
  API is `?` for that cluster with the API's message (`error_text`), the
  rest of the list works.
- **Health from status, not events.** Status summaries come from
  `status.conditions` (Ready, Available) and kind-specific fields; a kind
  without known conditions shows no verdict, never a guessed one.
- **Off the event loop,** results kept per view so switching back is
  instant; refreshes on the view's interval only while it's shown.
- **Untrusted text.** Names, labels, annotations, YAML and events are
  stripped of control characters before they're drawn
  (`.claude/rules/security.md`, open check); copied commands quote
  anything that isn't a DNS name.
- **Masked:** Secret data, and in any YAML the values `node_info::mask_config`
  masks (tokens, keys, URL passwords).

## Views

**Workloads (all clusters).** The existing view; the summary gets a row per
cluster, the list a CLUSTER column, the sort keeps worst first across
clusters. The cluster tree's selection still narrows: a cluster row shows
that cluster, the root shows all.

**Search.** Results grouped by cluster, then kind. Enter opens the object's
detail. Searches use server-side selectors where they can (labels), names
match client-side over the cached lists; an image search reads pods.

**Browser.** Kinds from API discovery, merged across clusters (a CRD only
on prod shows "prod only"). Lists use `kube_list` with paging and a
per-kind row cap; a kind with thousands of objects says so instead of
fetching everything.

**Compare.** Two objects, same kind and name, chosen from the browser or
search (`c` on a row, then the other cluster). Diff of the YAML with
status, `metadata.managedFields`, `resourceVersion` and UIDs dropped.

**Flux.** A filter of the browser with Flux-aware columns; Enter shows
the source path or chart in the GitOps repo.

## Implementation notes

- Discovery is cached per cluster with a TTL; lists use `DynamicObject` for
  CRDs and typed objects where a view needs fields.
- Pure parts in talos9s-core: status summaries per kind, the YAML diff and
  its normalization, search matching, masking rules. Unit-tested.
- Bounded: per-call timeouts in `talos9s_core::constants`, a cap on
  parallel requests per cluster, list paging.

## Testing

- Unit: status summaries (healthy, progressing, failed, unknown kind),
  diff normalization, search matching, masking of Secrets and YAML.
- Live (read-only): lab and prod together; one cluster unreachable (fake
  talosconfig endpoint) and one with a 403 (`--kubeconfig` with lab-oidc),
  the other keeps working.
- No false positives: break a test Deployment in a test namespace (bad
  image), see it in the cross-cluster list, fix it, gone. Delete the
  namespace.

## Phases

1. Workloads across clusters.
2. Resource browser with discovery, describe, YAML, events.
3. Search.
4. Flux view.
5. Compare.
6. Logs by selector across clusters.

## Open questions

1. **Refresh cost:** all clusters on every refresh, or the selected one
   often and the rest slower?
2. **Kind list:** every kind from discovery, or a curated default list with
   the rest behind a filter?
3. **GitOps repo link:** only show the Flux path, or read a local checkout
   (a configured path) to open the file?
