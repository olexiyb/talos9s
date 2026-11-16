//! Kubernetes view: every cluster of the talosconfig at once, read-only
//!
//! One list across clusters: the problems among workloads and pods (the
//! default), Flux objects, any kind from API discovery (`:`), or a search
//! by name, label or image (`s`). Enter shows an object (status,
//! conditions, events, masked YAML), `d` diffs it with the same object in
//! another cluster, `l`/`L` read its pods' logs in one cluster or in all.
//!
//! Nothing here changes a cluster: state comes from GitOps, the view shows
//! it and copies `kubectl` commands (`plans/kubernetes.md`). Each cluster
//! is read on its own, in its own timeout: one that can't be reached or
//! answers 403 is `?` with its error, the others are listed. Objects are
//! redacted as soon as they're read (`talos9s_core::kubernetes::redact`):
//! a Secret's values never reach memory the view keeps.

use crate::action::Action;
use crate::components::Component;
use crate::components::cluster::NavMenuItem;
use crate::components::pod_logs::LogPane;
use crate::ui_ext::{HealthIndicatorExt, KEY, Move, PAGE_ROWS, RowHits, moved};
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use k8s_openapi::api::core::v1::Event;
use kube::Client;
use kube::api::{Api, ApiResource, DynamicObject, ListParams};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use talos9s_core::kubernetes::{
    DiffLine, FluxInfo, KindRef, MergedKind, Query, Status, array_at, contains_ignore_case,
    diff_lines, flux_info, for_compare, is_default_kind, is_gitops_group, is_problem, merge_kinds,
    pod_selector, printable, redact, status,
};
use talos9s_core::pod_logs::LogSource;
use talos9s_core::{AsyncState, HasHealth};

/// Problems and Flux change slowly; every refresh reads every cluster
const AUTO_REFRESH_INTERVAL: Duration = talos9s_core::constants::refresh_intervals::VERY_SLOW;

/// Discovery is read again after this
const DISCOVERY_TTL: Duration = Duration::from_secs(600);

/// At most this many objects per kind and cluster; more is said, not fetched
pub const ROW_CAP: u32 = 2000;

/// One cluster of the talosconfig, as the tree knows it
#[derive(Clone)]
pub struct ClusterSource {
    pub name: String,
    /// `None` while it isn't connected
    pub talos: Option<talos_rs::TalosClient>,
    /// A control plane node to fetch the kubeconfig from
    pub cp_ip: Option<String>,
    /// Why it isn't connected
    pub error: Option<String>,
}

/// What the list shows
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// Unhealthy Deployments, StatefulSets, DaemonSets and pods
    Problems,
    /// Flux Kustomizations, HelmReleases and sources; Argo CD Applications
    GitOps,
    /// Every object of one kind
    Kind(KindRef),
    /// Objects of the common kinds matching a query
    Search(Query),
}

impl Mode {
    fn title(&self) -> String {
        match self {
            Mode::Problems => "Problems".to_string(),
            Mode::GitOps => "GitOps".to_string(),
            Mode::Kind(k) => k.label(),
            Mode::Search(q) => format!("Search: {}", q.describe()),
        }
    }

    /// Lists start with everything, except the problem lists
    fn shows_all_by_default(&self) -> bool {
        !matches!(self, Mode::Problems)
    }

    /// Several kinds in one list: a KIND column
    fn mixed_kinds(&self) -> bool {
        !matches!(self, Mode::Kind(_))
    }

    /// The menu entry showing this list
    pub fn menu_item(&self) -> NavMenuItem {
        match self {
            Mode::GitOps => NavMenuItem::GitOps,
            _ => NavMenuItem::Kubernetes,
        }
    }
}

/// A built-in kind, before (or without) discovery
fn builtin(group: &str, version: &str, kind: &str, plural: &str, namespaced: bool) -> KindRef {
    KindRef {
        group: group.to_string(),
        version: version.to_string(),
        kind: kind.to_string(),
        plural: plural.to_string(),
        namespaced,
    }
}

fn pod_kind() -> KindRef {
    builtin("", "v1", "Pod", "pods", true)
}

fn problem_kinds() -> Vec<KindRef> {
    vec![
        builtin("apps", "v1", "Deployment", "deployments", true),
        builtin("apps", "v1", "StatefulSet", "statefulsets", true),
        builtin("apps", "v1", "DaemonSet", "daemonsets", true),
        pod_kind(),
    ]
}

/// What a search looks through: the common kinds, and Flux's where served
fn search_kinds(query: &Query, discovered: Option<&[KindRef]>) -> Vec<KindRef> {
    let mut kinds = problem_kinds();
    if matches!(query, Query::Image(_)) {
        kinds.push(builtin("batch", "v1", "CronJob", "cronjobs", true));
        return kinds;
    }
    kinds.extend([
        builtin("", "v1", "Service", "services", true),
        builtin("", "v1", "ConfigMap", "configmaps", true),
        builtin("batch", "v1", "Job", "jobs", true),
        builtin("batch", "v1", "CronJob", "cronjobs", true),
    ]);
    kinds.extend(
        discovered
            .unwrap_or_default()
            .iter()
            .filter(|k| {
                is_gitops_group(&k.group)
                    && matches!(
                        k.kind.as_str(),
                        "Kustomization" | "HelmRelease" | "Application"
                    )
            })
            .cloned(),
    );
    kinds
}

const GITOPS_KINDS: &[&str] = &[
    "Application",
    "Kustomization",
    "HelmRelease",
    "GitRepository",
    "OCIRepository",
    "HelmRepository",
    "HelmChart",
    "Bucket",
];

/// The kinds to list in a cluster for `mode`, given what it serves
fn kinds_for(mode: &Mode, discovered: Option<&[KindRef]>) -> Result<Vec<KindRef>, String> {
    match mode {
        Mode::Problems => Ok(problem_kinds()),
        Mode::Search(q) => Ok(search_kinds(q, discovered)),
        // None served: not an error, a note
        Mode::GitOps => Ok(discovered
            .ok_or("API discovery failed")?
            .iter()
            .filter(|k| is_gitops_group(&k.group) && GITOPS_KINDS.contains(&k.kind.as_str()))
            .cloned()
            .collect()),
        Mode::Kind(k) => Ok(match discovered {
            // The cluster's own version of the kind
            Some(kinds) => kinds
                .iter()
                .find(|d| d.group == k.group && d.kind == k.kind)
                .cloned()
                .into_iter()
                .collect(),
            None => vec![k.clone()],
        }),
    }
}

fn api_resource(k: &KindRef) -> ApiResource {
    ApiResource {
        group: k.group.clone(),
        version: k.version.clone(),
        api_version: if k.group.is_empty() {
            k.version.clone()
        } else {
            format!("{}/{}", k.group, k.version)
        },
        kind: k.kind.clone(),
        plural: k.plural.clone(),
    }
}

/// One object of the list
#[derive(Clone, Debug)]
pub struct ObjectRow {
    /// Cluster, group, kind, namespace and name: the selection and the mark
    pub id: String,
    pub cluster: String,
    pub kind: KindRef,
    /// Empty for cluster-scoped kinds
    pub namespace: String,
    pub name: String,
    pub status: Status,
    pub age_secs: Option<i64>,
    pub flux: Option<FluxInfo>,
    /// Redacted
    pub obj: Arc<Value>,
}

fn row_id(cluster: &str, kind: &KindRef, namespace: &str, name: &str) -> String {
    format!("{cluster}/{}/{}/{namespace}/{name}", kind.group, kind.kind)
}

impl ObjectRow {
    /// The same object in another cluster
    fn same_object(&self, other: &ObjectRow) -> bool {
        self.kind.group == other.kind.group
            && self.kind.kind == other.kind.kind
            && self.namespace == other.namespace
            && self.name == other.name
            && self.cluster != other.cluster
    }

    fn kubectl(&self, verb: &str) -> String {
        let ns = if self.namespace.is_empty() {
            String::new()
        } else {
            format!(" -n {}", self.namespace)
        };
        format!(
            "kubectl{ns} {verb} {} {}",
            self.kind.kubectl_name(),
            self.name
        )
    }
}

/// How one cluster's read went
#[derive(Clone, Debug, Default)]
pub struct ClusterResult {
    pub name: String,
    /// Not reachable, or no list could be read
    pub error: Option<String>,
    /// Kinds that couldn't be listed, with why
    pub failed: Vec<String>,
    /// Nothing to list: the cluster doesn't serve the kind (no Flux, no
    /// such CRD); shown dimmed, not as an error
    pub absent: Option<String>,
    /// Kinds with more objects than [`ROW_CAP`]
    pub truncated: Vec<String>,
    /// Rows of this cluster in the list
    pub count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct KubeData {
    pub rows: Vec<ObjectRow>,
    pub clusters: Vec<ClusterResult>,
    /// Kinds across clusters, for the kind picker
    pub kinds: Vec<MergedKind>,
}

/// The rows to show: problems only unless `show_all`, worst first in the
/// problem and Flux lists, by cluster, namespace and name otherwise
pub fn arrange(mode: &Mode, mut rows: Vec<ObjectRow>, show_all: bool) -> Vec<ObjectRow> {
    if !show_all {
        rows.retain(|r| is_problem(&r.kind.kind, &r.obj, &r.status));
    }
    let by_health = matches!(mode, Mode::Problems | Mode::GitOps);
    rows.sort_by(|a, b| {
        // `ObjectHealth` orders worst first
        let health = if by_health {
            a.status.health.cmp(&b.status.health)
        } else {
            std::cmp::Ordering::Equal
        };
        health
            .then_with(|| a.cluster.cmp(&b.cluster))
            .then_with(|| a.namespace.cmp(&b.namespace))
            .then_with(|| a.kind.kind.cmp(&b.kind.kind))
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// A value filled in by a spawned task, taken by the view on a tick
type Slot<T> = Arc<Mutex<Option<T>>>;

fn take<T>(slot: &Slot<T>) -> Option<T> {
    slot.lock().ok().and_then(|mut s| s.take())
}

/// The kind picker (`:`), drawn over the list
#[derive(Clone, Default)]
struct KindPicker {
    input: String,
    cursor: usize,
}

/// One entry of the kind picker
#[derive(Clone, Debug, PartialEq)]
enum PickerEntry {
    Problems,
    GitOps,
    Kind(MergedKind),
}

impl PickerEntry {
    fn label(&self) -> String {
        match self {
            PickerEntry::Problems => "Problems (workloads and pods)".to_string(),
            PickerEntry::GitOps => {
                "GitOps (Flux Kustomizations, HelmReleases, sources; Argo CD Applications)"
                    .to_string()
            }
            PickerEntry::Kind(k) => k.kind.label(),
        }
    }
}

/// The picker's entries for `input`: the curated kinds when empty, every
/// kind whose name contains it otherwise
fn picker_entries(kinds: &[MergedKind], input: &str) -> Vec<PickerEntry> {
    let input = input.trim();
    let mut entries = Vec::new();
    if input.is_empty() {
        entries.push(PickerEntry::Problems);
        entries.push(PickerEntry::GitOps);
        entries.extend(
            kinds
                .iter()
                .filter(|k| is_default_kind(&k.kind))
                .cloned()
                .map(PickerEntry::Kind),
        );
        return entries;
    }
    let matches = |s: &str| contains_ignore_case(s, input);
    if matches("problems") {
        entries.push(PickerEntry::Problems);
    }
    if matches("gitops") || matches("flux") || matches("argo") {
        entries.push(PickerEntry::GitOps);
    }
    entries.extend(
        kinds
            .iter()
            .filter(|k| matches(&k.kind.label()) || matches(&k.kind.plural))
            .cloned()
            .map(PickerEntry::Kind),
    );
    entries
}

/// What Enter, `d` or `l` opened over the list
#[derive(Clone)]
enum Focus {
    Detail {
        row: Box<ObjectRow>,
        lines: Vec<Line<'static>>,
        /// The object's events, read after opening
        events: Slot<Result<Vec<String>, String>>,
        events_read: Option<Result<Vec<String>, String>>,
        scroll: usize,
    },
    Diff {
        title: String,
        lines: Vec<Line<'static>>,
        changed: usize,
        scroll: usize,
    },
    Logs {
        title: String,
        pane: Box<LogPane>,
        /// The sources, while the pods are being listed
        pending: Slot<Result<Vec<LogSource>, String>>,
        problem: Option<String>,
    },
}

#[derive(Clone)]
pub struct KubernetesComponent {
    state: AsyncState<KubeData>,
    sources: Vec<ClusterSource>,
    /// Kubernetes clients by cluster, made on the first refresh
    clients: HashMap<String, Client>,
    connect_errors: HashMap<String, String>,
    discovered: HashMap<String, (Instant, Vec<KindRef>)>,
    mode: Mode,
    show_all: bool,
    /// One cluster only (`X`), else all
    cluster_filter: Option<String>,
    filter: String,
    filter_input: Option<String>,
    search_input: Option<String>,
    picker: Option<KindPicker>,
    /// The rows as listed: `state`'s after the cluster filter, `/` and `A`,
    /// arranged; rebuilt by `rearrange` when one of those changes
    shown: Vec<ObjectRow>,
    selected: usize,
    selected_id: Option<String>,
    table_state: TableState,
    hits: RowHits,
    /// The row `d` marked, to diff with the next one
    mark: Option<String>,
    focus: Option<Focus>,
    message: Option<String>,
    auto_refresh: bool,
    log_wrap: bool,
    /// Lines a page scroll moves in the detail and diff
    page: usize,
    /// The list changed and hasn't been read for it yet: asked again on
    /// every tick, since a refresh still running for the old list makes
    /// the app drop the request
    unread: bool,
}

impl KubernetesComponent {
    pub fn new(mode: Mode, sources: Vec<ClusterSource>) -> Self {
        Self {
            state: AsyncState::new(),
            sources,
            clients: HashMap::new(),
            connect_errors: HashMap::new(),
            discovered: HashMap::new(),
            show_all: mode.shows_all_by_default(),
            mode,
            cluster_filter: None,
            filter: String::new(),
            filter_input: None,
            search_input: None,
            picker: None,
            shown: Vec::new(),
            selected: 0,
            selected_id: None,
            table_state: TableState::default(),
            hits: RowHits::default(),
            mark: None,
            focus: None,
            message: None,
            auto_refresh: true,
            log_wrap: false,
            page: 20,
            unread: false,
        }
    }

    /// The tree's clusters again: one connected since gets read too
    pub fn set_sources(&mut self, sources: Vec<ClusterSource>) {
        self.sources = sources;
    }

    pub fn start_loading(&mut self) {
        self.state.start_loading();
    }

    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    pub fn log_wrap(&self) -> bool {
        match &self.focus {
            Some(Focus::Logs { pane, .. }) => pane.wrap(),
            _ => self.log_wrap,
        }
    }

    pub fn set_log_wrap(&mut self, wrap: bool) {
        self.log_wrap = wrap;
    }

    pub fn viewer_failed(&mut self, problem: String) {
        if let Some(Focus::Logs { pane, .. }) = &mut self.focus {
            pane.set_viewer_problem(problem);
        }
    }

    /// Connect, discover and list every cluster, each on its own
    pub async fn refresh(&mut self) -> Result<()> {
        self.state.start_loading();
        self.connect_missing().await;
        self.discover_stale().await;

        let mode = self.mode.clone();
        let reads = self.sources.iter().map(|source| {
            let client = self.clients.get(&source.name).cloned();
            let error = self
                .connect_errors
                .get(&source.name)
                .or(source.error.as_ref())
                .cloned()
                .unwrap_or_else(|| "Not connected".to_string());
            let discovered = self.discovered.get(&source.name).map(|(_, k)| k.clone());
            let name = source.name.clone();
            let mode = mode.clone();
            async move {
                let mut result = ClusterResult {
                    name: name.clone(),
                    ..Default::default()
                };
                let Some(client) = client else {
                    result.error = Some(error);
                    return (result, Vec::new());
                };
                let kinds = match kinds_for(&mode, discovered.as_deref()) {
                    Ok(kinds) if kinds.is_empty() => {
                        result.absent = Some(match &mode {
                            Mode::GitOps => "no Flux or Argo CD".to_string(),
                            Mode::Kind(k) => format!("doesn't serve {}", k.label()),
                            _ => "nothing to list".to_string(),
                        });
                        return (result, Vec::new());
                    }
                    Ok(kinds) => kinds,
                    Err(e) => {
                        result.error = Some(e);
                        return (result, Vec::new());
                    }
                };
                let timeout = talos9s_core::constants::WORKLOADS_FETCH_TIMEOUT;
                match tokio::time::timeout(timeout, list_cluster(&name, &client, &kinds, &mode))
                    .await
                {
                    Ok((rows, failed, truncated)) => {
                        if rows.is_empty() && !failed.is_empty() && failed.len() == kinds.len() {
                            result.error = Some(failed.join("; "));
                        } else {
                            result.failed = failed;
                        }
                        result.truncated = truncated;
                        (result, rows)
                    }
                    Err(_) => {
                        result.error = Some(format!("Timed out after {}s", timeout.as_secs()));
                        (result, Vec::new())
                    }
                }
            }
        });
        let results = futures::future::join_all(reads).await;
        let mut rows = Vec::new();
        let mut clusters = Vec::new();
        for (result, cluster_rows) in results {
            clusters.push(result);
            rows.extend(cluster_rows);
        }
        let kinds = merge_kinds(
            &self
                .sources
                .iter()
                .filter_map(|s| {
                    self.discovered
                        .get(&s.name)
                        .map(|(_, k)| (s.name.clone(), k.clone()))
                })
                .collect::<Vec<_>>(),
        );
        tracing::info!(
            "kubernetes: {} objects from {} clusters ({})",
            rows.len(),
            clusters.len(),
            mode.title()
        );
        self.state.set_data(KubeData {
            rows,
            clusters,
            kinds,
        });
        self.rearrange();
        Ok(())
    }

    async fn connect_missing(&mut self) {
        let pending: Vec<_> = self
            .sources
            .iter()
            .filter(|s| !self.clients.contains_key(&s.name))
            .filter_map(|s| Some((s.name.clone(), s.talos.clone()?, s.cp_ip.clone())))
            .collect();
        let connects = pending.into_iter().map(|(name, talos, cp_ip)| async move {
            let result =
                crate::components::diagnostics::k8s::connect(&talos, cp_ip.as_deref()).await;
            (name, result)
        });
        for (name, result) in futures::future::join_all(connects).await {
            match result {
                Ok((client, _)) => {
                    self.connect_errors.remove(&name);
                    self.clients.insert(name, client);
                }
                Err(e) => {
                    self.connect_errors
                        .insert(name, format!("Failed to connect to Kubernetes: {e}"));
                }
            }
        }
    }

    async fn discover_stale(&mut self) {
        let stale: Vec<(String, Client)> = self
            .clients
            .iter()
            .filter(|(name, _)| {
                self.discovered
                    .get(*name)
                    .is_none_or(|(at, _)| at.elapsed() > DISCOVERY_TTL)
            })
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect();
        let runs = stale.into_iter().map(|(name, client)| async move {
            let timeout = talos9s_core::constants::WORKLOADS_FETCH_TIMEOUT;
            let found =
                tokio::time::timeout(timeout, kube::Discovery::new(client).run_aggregated()).await;
            (name, found)
        });
        for (name, found) in futures::future::join_all(runs).await {
            match found {
                Ok(Ok(discovery)) => {
                    let mut kinds = Vec::new();
                    for group in discovery.groups() {
                        for (ar, caps) in group.recommended_resources() {
                            if !caps.supports_operation(kube::discovery::verbs::LIST) {
                                continue;
                            }
                            kinds.push(KindRef {
                                group: ar.group,
                                version: ar.version,
                                kind: ar.kind,
                                plural: ar.plural,
                                namespaced: caps.scope == kube::discovery::Scope::Namespaced,
                            });
                        }
                    }
                    self.discovered.insert(name, (Instant::now(), kinds));
                }
                Ok(Err(e)) => tracing::warn!(
                    "kubernetes: discovery on {name} failed: {}",
                    crate::kube_list::error_text(&e)
                ),
                Err(_) => tracing::warn!("kubernetes: discovery on {name} timed out"),
            }
        }
    }

    // ---- the list ----

    /// Rebuild the rows shown (after the cluster filter, `/` and `A`,
    /// arranged) and keep the selection on the same object
    fn rearrange(&mut self) {
        let Some(data) = self.state.data() else {
            self.shown.clear();
            self.selected = 0;
            return;
        };
        let rows = data
            .rows
            .iter()
            .filter(|r| self.cluster_filter.as_ref().is_none_or(|c| &r.cluster == c))
            .filter(|r| {
                self.filter.is_empty()
                    || [
                        &r.name,
                        &r.namespace,
                        &r.cluster,
                        &r.kind.kind,
                        &r.status.text,
                    ]
                    .iter()
                    .any(|s| contains_ignore_case(s, &self.filter))
            })
            .cloned()
            .collect();
        self.shown = arrange(&self.mode, rows, self.show_all);
        let found = self
            .selected_id
            .as_ref()
            .and_then(|id| self.shown.iter().position(|r| &r.id == id));
        let last = self.shown.len().saturating_sub(1);
        self.selected = found.unwrap_or(self.selected.min(last));
        self.selected_id = self.shown.get(self.selected).map(|r| r.id.clone());
    }

    fn select(&mut self, i: usize) {
        if let Some(row) = self.shown.get(i) {
            self.selected = i;
            self.selected_id = Some(row.id.clone());
        }
    }

    fn selected_row(&self) -> Option<ObjectRow> {
        self.shown.get(self.selected).cloned()
    }

    /// The problem list without `A`: only problems, and a count of none is good
    fn problems_only(&self) -> bool {
        matches!(self.mode, Mode::Problems) && !self.show_all
    }

    /// A new list: read again, the selection from the top
    fn switch_mode(&mut self, mode: Mode) -> Action {
        self.show_all = mode.shows_all_by_default();
        self.mode = mode;
        self.selected = 0;
        self.selected_id = None;
        self.mark = None;
        self.unread = true;
        self.state.start_loading();
        if let Some(data) = self.state.data_mut() {
            data.rows.clear();
        }
        self.rearrange();
        Action::Refresh
    }

    fn cycle_cluster(&mut self) {
        let names: Vec<String> = self.sources.iter().map(|s| s.name.clone()).collect();
        self.cluster_filter = match &self.cluster_filter {
            None => names.first().cloned(),
            Some(current) => names
                .iter()
                .position(|n| n == current)
                .and_then(|i| names.get(i + 1))
                .cloned(),
        };
        self.rearrange();
    }

    // ---- focus ----

    fn open_detail(&mut self, row: ObjectRow) {
        let lines = detail_lines(&row);
        let events: Slot<Result<Vec<String>, String>> = Arc::default();
        if let Some(client) = self.clients.get(&row.cluster).cloned() {
            let slot = Arc::clone(&events);
            let uid = row
                .obj
                .pointer("/metadata/uid")
                .and_then(Value::as_str)
                .map(str::to_string);
            let namespace = row.namespace.clone();
            tokio::spawn(async move {
                let read = read_events(client, &namespace, uid).await;
                if let Ok(mut s) = slot.lock() {
                    *s = Some(read);
                }
            });
        }
        self.focus = Some(Focus::Detail {
            row: Box::new(row),
            lines,
            events,
            events_read: None,
            scroll: 0,
        });
    }

    /// `d`: diff with the same object in the one other cluster that has
    /// it, or mark this one for the next `d`
    fn compare(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let marked = self
            .mark
            .as_ref()
            .and_then(|id| self.shown.iter().find(|r| &r.id == id));
        let other = match marked {
            Some(m) if m.id != row.id => Some(m.clone()),
            _ => {
                let mut same = self.shown.iter().filter(|r| r.same_object(&row));
                match (same.next(), same.next()) {
                    (Some(one), None) => Some(one.clone()),
                    _ => None,
                }
            }
        };
        let Some(other) = other else {
            self.mark = Some(row.id.clone());
            self.message = Some(format!(
                "Marked {}/{} in {}: <d> on another row to compare",
                row.kind.kind, row.name, row.cluster
            ));
            return;
        };
        self.mark = None;
        self.message = None;
        let (left, right) = if other.cluster <= row.cluster {
            (other, row)
        } else {
            (row, other)
        };
        let (lines, changed) = diff_view(&left, &right);
        self.focus = Some(Focus::Diff {
            title: format!(
                "{} {}: {} ↔ {}",
                left.kind.kind,
                qualified(&left.namespace, &left.name),
                left.cluster,
                right.cluster
            ),
            lines,
            changed,
            scroll: 0,
        });
    }

    /// `l`: logs of the row's pods in its cluster; `L`: the same selector
    /// in every cluster; a label search: every pod found
    fn open_logs(&mut self, everywhere: bool) {
        let pending: Slot<Result<Vec<LogSource>, String>> = Arc::default();
        let mut pane = LogPane::default();
        pane.set_wrap(self.log_wrap);
        pane.set_clients(self.clients.clone());

        let title;
        // Known now (pods listed), else read into `pending`
        let mut sources = None;
        if let Mode::Search(Query::Label(selector)) = &self.mode
            && everywhere
        {
            let pods: Vec<&ObjectRow> =
                self.shown.iter().filter(|r| r.kind.kind == "Pod").collect();
            title = format!("pods {selector} in {} clusters", cluster_count(&pods));
            sources = Some(pods.iter().flat_map(|r| pod_sources(r, true)).collect());
        } else {
            let Some(row) = self.selected_row() else {
                return;
            };
            if row.kind.kind == "Pod" {
                title = format!(
                    "{} in {}",
                    qualified(&row.namespace, &row.name),
                    row.cluster
                );
                sources = Some(pod_sources(&row, false));
            } else {
                let Some(selector) = pod_selector(&row.obj) else {
                    self.message = Some(format!(
                        "{} {} has no pod selector",
                        row.kind.kind, row.name
                    ));
                    return;
                };
                let clients: Vec<(String, Client)> = if everywhere {
                    self.clients
                        .iter()
                        .filter(|(n, _)| self.cluster_filter.as_ref().is_none_or(|c| c == *n))
                        .map(|(n, c)| (n.clone(), c.clone()))
                        .collect()
                } else {
                    self.clients
                        .get(&row.cluster)
                        .map(|c| vec![(row.cluster.clone(), c.clone())])
                        .unwrap_or_default()
                };
                title = if everywhere {
                    format!(
                        "{} {} pods ({selector}) in every cluster",
                        row.namespace, row.name
                    )
                } else {
                    format!("{} {} pods in {}", row.namespace, row.name, row.cluster)
                };
                let slot = Arc::clone(&pending);
                let namespace = row.namespace;
                tokio::spawn(async move {
                    let found = pods_by_selector(clients, &namespace, &selector).await;
                    if let Ok(mut s) = slot.lock() {
                        *s = Some(found);
                    }
                });
            }
        }
        let mut problem = None;
        if let Some(sources) = sources {
            receive_sources(&title, &mut pane, &mut problem, Ok(sources));
        }
        self.focus = Some(Focus::Logs {
            title,
            pane: Box::new(pane),
            pending,
            problem,
        });
    }

    fn copy_command(&mut self) {
        let row = match &self.focus {
            Some(Focus::Detail { row, .. }) => Some((**row).clone()),
            None => self.selected_row(),
            _ => None,
        };
        if let Some(row) = row {
            let command = format!("{} -o yaml", row.kubectl("get"));
            crate::clipboard::copy_to_clipboard(format!("# cluster: {}\n{command}", row.cluster));
            self.message = Some(format!("Copied: {command}"));
        }
    }

    fn handle_focus_key(&mut self, key: KeyEvent) -> Option<Action> {
        let page = self.page;
        if let Some(Focus::Logs { pane, .. }) = &mut self.focus {
            if pane.handle_key(&key, None) {
                return None;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.log_wrap = pane.wrap();
                    self.focus = None;
                }
                KeyCode::Char('v') => {
                    let text = pane.export()?;
                    match crate::components::workloads::write_private_temp("talos9s-logs-", &text) {
                        Ok(path) => return Some(Action::OpenInViewer(path)),
                        Err(e) => pane.set_viewer_problem(e.to_string()),
                    }
                }
                _ => {}
            }
            return None;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.focus = None;
            return None;
        }
        if key.code == KeyCode::Char('y') {
            self.copy_command();
            return None;
        }
        if let Some(Focus::Detail { row, .. }) = &self.focus
            && matches!(key.code, KeyCode::Char('l' | 'L'))
        {
            let id = row.id.clone();
            self.focus = None;
            self.select_row_id(&id);
            self.open_logs(key.code == KeyCode::Char('L'));
            return None;
        }
        let (scroll, len) = match &mut self.focus {
            Some(Focus::Detail {
                scroll,
                lines,
                events_read,
                ..
            }) => {
                let events = events_read
                    .as_ref()
                    .map_or(1, |e| e.as_ref().map_or(1, Vec::len));
                (scroll, lines.len() + events + 3)
            }
            Some(Focus::Diff { scroll, lines, .. }) => (scroll, lines.len()),
            _ => return None,
        };
        let page = isize::try_from(page).unwrap_or(PAGE_ROWS);
        let ctrl = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        let step = match key.code {
            KeyCode::Char('u') if ctrl => Some(Move::By(-page)),
            KeyCode::Char('d') if ctrl => Some(Move::By(page)),
            KeyCode::PageUp => Some(Move::By(-page)),
            KeyCode::PageDown => Some(Move::By(page)),
            _ => Move::from_key(&key),
        };
        if let Some(step) = step
            && let Some(next) = moved(*scroll, len, step)
        {
            *scroll = next;
        }
        None
    }

    fn select_row_id(&mut self, id: &str) {
        if let Some(i) = self.shown.iter().position(|r| r.id == id) {
            self.select(i);
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> Option<Action> {
        let kinds = self.state.data().map_or(&[][..], |d| d.kinds.as_slice());
        let picker = self.picker.as_mut()?;
        let entries = picker_entries(kinds, &picker.input);
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Enter => {
                let entry = entries.get(picker.cursor).cloned();
                self.picker = None;
                return Some(match entry? {
                    PickerEntry::Problems => self.switch_mode(Mode::Problems),
                    PickerEntry::GitOps => self.switch_mode(Mode::GitOps),
                    PickerEntry::Kind(k) => self.switch_mode(Mode::Kind(k.kind)),
                });
            }
            KeyCode::Up => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::Down => {
                picker.cursor = (picker.cursor + 1).min(entries.len().saturating_sub(1));
            }
            KeyCode::Backspace => {
                picker.input.pop();
                picker.cursor = 0;
            }
            KeyCode::Char(c) => {
                picker.input.push(c);
                picker.cursor = 0;
            }
            _ => {}
        }
        None
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> Option<Action> {
        if let Some(step) = Move::from_key(&key) {
            if let Some(i) = moved(self.selected, self.shown.len(), step) {
                self.select(i);
            }
            return None;
        }
        self.message = None;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.mark.take().is_some() {
                    return None;
                }
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rearrange();
                    return None;
                }
                if matches!(self.mode, Mode::Search(_)) {
                    return Some(self.switch_mode(Mode::Problems));
                }
                return Some(Action::Back);
            }
            KeyCode::Enter => {
                if let Some(row) = self.selected_row() {
                    self.open_detail(row);
                }
            }
            KeyCode::Char(':') => self.picker = Some(KindPicker::default()),
            KeyCode::Char('s') => self.search_input = Some(String::new()),
            KeyCode::Char('/') => self.filter_input = Some(self.filter.clone()),
            KeyCode::Char('A') => {
                self.show_all = !self.show_all;
                self.rearrange();
            }
            KeyCode::Char('X') => self.cycle_cluster(),
            KeyCode::Char('d') => self.compare(),
            KeyCode::Char('l') => self.open_logs(false),
            KeyCode::Char('L') => self.open_logs(true),
            KeyCode::Char('y') => self.copy_command(),
            KeyCode::Char('r') => return Some(Action::Refresh),
            KeyCode::Char('a') => self.auto_refresh = !self.auto_refresh,
            _ => {}
        }
        None
    }

    fn handle_input_key(key: KeyEvent, input: &mut String) -> Option<bool> {
        match key.code {
            KeyCode::Enter => Some(true),
            KeyCode::Esc => Some(false),
            KeyCode::Backspace => {
                input.pop();
                None
            }
            KeyCode::Char(c) => {
                input.push(c);
                None
            }
            _ => None,
        }
    }

    // ---- drawing ----

    fn summary_line(&self) -> Line<'static> {
        // GitOps is the window's title already; the other lists name
        // what they show (Problems, a kind, a search)
        let title = if matches!(self.mode, Mode::GitOps) {
            String::new()
        } else {
            format!("{}  ", self.mode.title())
        };
        let mut spans = vec![Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        let Some(data) = self.state.data() else {
            return Line::from(spans);
        };
        for c in &data.clusters {
            let count = self.shown.iter().filter(|r| r.cluster == c.name).count();
            let filtered_out = self.cluster_filter.as_ref().is_some_and(|f| f != &c.name);
            let (text, color) = if c.absent.is_some() {
                (format!("{} -", c.name), Color::DarkGray)
            } else if c.error.is_some() {
                (format!("{} ?", c.name), Color::Red)
            } else if !c.failed.is_empty() {
                (format!("{} {count}?", c.name), Color::Yellow)
            } else if self.problems_only() {
                (
                    format!("{} {count}", c.name),
                    if count == 0 { Color::Green } else { Color::Red },
                )
            } else {
                (format!("{} {count}", c.name), Color::White)
            };
            let style = if filtered_out {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(color)
            };
            spans.push(Span::styled(text, style));
            spans.push(Span::raw("  "));
        }
        if let Some(c) = &self.cluster_filter {
            spans.push(Span::styled(
                format!("[{c} ONLY]"),
                Style::default().fg(Color::Magenta),
            ));
        }
        if !self.auto_refresh {
            spans.push(Span::styled(
                " [AUTO:OFF]",
                Style::default().fg(Color::DarkGray),
            ));
        }
        Line::from(spans)
    }

    /// Problems of the clusters (errors, failed lists, caps), one per line
    fn problem_lines(&self) -> Vec<Line<'static>> {
        let Some(data) = self.state.data() else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for c in &data.clusters {
            if let Some(a) = &c.absent {
                lines.push(Line::styled(
                    format!("{}: {a}", c.name),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some(e) = &c.error {
                lines.push(Line::styled(
                    format!("{}: {}", c.name, printable(e)),
                    Style::default().fg(Color::Red),
                ));
            }
            for f in &c.failed {
                lines.push(Line::styled(
                    format!("{}: couldn't read {}", c.name, printable(f)),
                    Style::default().fg(Color::Yellow),
                ));
            }
            for t in &c.truncated {
                lines.push(Line::styled(
                    format!(
                        "{}: more than {ROW_CAP} {t}, the first {ROW_CAP} shown",
                        c.name
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        lines
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let rows = &self.shown;
        let mut problems = self.problem_lines();
        problems.truncate(usize::from(area.height / 4).max(1));
        let mut extra = Vec::new();
        if let Some(input) = &self.filter_input {
            extra.push(input_line("/", input));
        } else if let Some(input) = &self.search_input {
            extra.push(input_line("search (name, l:label=x, i:image): ", input));
        } else if !self.filter.is_empty() {
            extra.push(Line::from(vec![
                Span::styled("filter: ", Style::default().dim()),
                Span::raw(self.filter.clone()),
            ]));
        }
        if let Some(m) = &self.message {
            extra.push(Line::styled(m.clone(), Style::default().fg(Color::Yellow)));
        }
        let header_height = 1 + problems.len() + extra.len();
        let [head, table_area] = Layout::vertical([
            Constraint::Length(u16::try_from(header_height).unwrap_or(1)),
            Constraint::Min(3),
        ])
        .areas(area);
        let mut head_lines = vec![self.summary_line()];
        head_lines.extend(problems);
        head_lines.extend(extra);
        frame.render_widget(Paragraph::new(head_lines), head);

        if rows.is_empty() {
            let text = if self.state.is_loading() {
                "Loading…".to_string()
            } else if let Some(e) = self.state.error() {
                format!("Error: {e}")
            } else if self.problems_only() {
                "No problems in the clusters read. <A> shows everything".to_string()
            } else {
                "Nothing found".to_string()
            };
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().dim())
                    .block(Block::default().borders(Borders::TOP)),
                table_area,
            );
            self.hits = RowHits::default();
            return;
        }

        let mixed = self.mode.mixed_kinds();
        let flux = matches!(self.mode, Mode::GitOps);
        let width_of = |f: &dyn Fn(&ObjectRow) -> usize, min: usize, max: usize| {
            let w = rows.iter().map(f).max().unwrap_or(0).clamp(min, max);
            Constraint::Length(u16::try_from(w).unwrap_or(10))
        };
        let mut header = vec!["CLUSTER", "NAMESPACE"];
        let mut widths = vec![
            width_of(&|r| r.cluster.chars().count(), 7, 16),
            width_of(&|r| r.namespace.chars().count(), 9, 24),
        ];
        if mixed {
            header.push("KIND");
            widths.push(width_of(&|r| r.kind.kind.chars().count(), 4, 16));
        }
        header.extend(["NAME", "STATUS", "AGE"]);
        widths.push(Constraint::Min(16));
        widths.push(width_of(&|r| r.status.text.chars().count(), 6, 24));
        widths.push(Constraint::Length(5));
        if flux {
            header.extend(["REVISION", "SOURCE"]);
            widths.push(width_of(
                &|r| r.flux.as_ref().map_or(0, |f| f.revision.chars().count()),
                8,
                24,
            ));
            widths.push(Constraint::Min(12));
        }

        let marked = self.mark.clone();
        let table_rows: Vec<Row> = rows
            .iter()
            .map(|r| {
                let style = Style::default().fg(r.status.health.health().color());
                let mut name = printable(&r.name);
                if marked.as_ref() == Some(&r.id) {
                    name = format!("* {name}");
                }
                let mut cells = vec![
                    Cell::from(r.cluster.clone()),
                    Cell::from(printable(&r.namespace)).style(Style::default().dim()),
                ];
                if mixed {
                    cells.push(Cell::from(r.kind.kind.clone()));
                }
                cells.push(Cell::from(name));
                cells.push(Cell::from(printable(&r.status.text)).style(style));
                cells.push(
                    Cell::from(
                        r.age_secs
                            .map(|a| talos9s_core::format_secs_human(a.unsigned_abs()))
                            .unwrap_or_default(),
                    )
                    .style(Style::default().dim()),
                );
                if flux {
                    let f = r.flux.clone().unwrap_or_default();
                    cells.push(Cell::from(printable(&f.revision)));
                    cells.push(Cell::from(printable(&f.source)).style(Style::default().dim()));
                }
                Row::new(cells)
            })
            .collect();
        let count = table_rows.len();
        let table = Table::new(table_rows, widths)
            .header(
                Row::new(header).style(
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::Gray),
                ),
            )
            .block(Block::default().borders(Borders::TOP))
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        self.table_state.select(Some(self.selected));
        frame.render_stateful_widget(table, table_area, &mut self.table_state);
        // The top border is one line, like a header
        self.hits = RowHits::new(
            Rect {
                y: table_area.y + 1,
                height: table_area.height.saturating_sub(1),
                ..table_area
            },
            false,
            1,
            self.table_state.offset(),
            count,
        );

        if let Some(picker) = &self.picker {
            let kinds = self.state.data().map_or(&[][..], |d| d.kinds.as_slice());
            draw_picker(frame, area, picker, kinds);
        }
    }

    fn key_line(&self) -> Line<'static> {
        let key = |k: &'static str| Span::styled(k, KEY);
        let label = |t: &'static str| Span::raw(t);
        if self.picker.is_some() {
            return Line::from(vec![
                key("<↑/↓>"),
                label(" move  "),
                key("<Enter>"),
                label(" pick  "),
                key("<Esc>"),
                label(" close  type to find a kind"),
            ]);
        }
        if self.filter_input.is_some() || self.search_input.is_some() {
            return Line::from(vec![
                key("<Enter>"),
                label(" apply  "),
                key("<Esc>"),
                label(" cancel"),
            ]);
        }
        match &self.focus {
            Some(Focus::Logs { pane, .. }) => {
                let mut spans = vec![key("<Esc/q>"), label(" back  ")];
                if pane.captures_text() {
                    return Line::from(spans);
                }
                spans.extend([
                    key("</>"),
                    label(" search  "),
                    key("<1-5/0>"),
                    label(" levels  "),
                    key("<w>"),
                    label(" wrap  "),
                    key("<V>"),
                    label(" select  "),
                    key("<v>"),
                    label(" viewer  "),
                    key("<y>"),
                    label(" copy"),
                ]);
                return Line::from(spans);
            }
            Some(Focus::Detail { .. }) => {
                return Line::from(vec![
                    key("<Esc/q>"),
                    label(" back  "),
                    key("<j/k/PgUp/PgDn>"),
                    label(" scroll  "),
                    key("<l/L>"),
                    label(" logs here/all  "),
                    key("<y>"),
                    label(" copy kubectl"),
                ]);
            }
            Some(Focus::Diff { .. }) => {
                return Line::from(vec![
                    key("<Esc/q>"),
                    label(" back  "),
                    key("<j/k/PgUp/PgDn>"),
                    label(" scroll"),
                ]);
            }
            None => {}
        }
        Line::from(vec![
            key("<:>"),
            label(" kind/gitops "),
            key("<s>"),
            label(" search "),
            key("</>"),
            label(" filter "),
            key("<A>"),
            label(" all "),
            key("<X>"),
            label(" cluster "),
            key("<d>"),
            label(" diff "),
            key("<l/L>"),
            label(" logs"),
        ])
    }

    fn draw_focus(&mut self, frame: &mut Frame, area: Rect) {
        self.page = usize::from(area.height.saturating_sub(3)).max(1);
        match &mut self.focus {
            Some(Focus::Detail {
                row,
                lines,
                events,
                events_read,
                scroll,
            }) => {
                if let Some(read) = take(events) {
                    *events_read = Some(read);
                }
                let mut all = lines.clone();
                all.push(Line::default());
                all.push(Line::styled(
                    "Events",
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                match events_read {
                    None => all.push(Line::styled("  Loading…", Style::default().dim())),
                    Some(Err(e)) => all.push(Line::styled(
                        format!("  couldn't read: {}", printable(e)),
                        Style::default().fg(Color::Yellow),
                    )),
                    Some(Ok(list)) if list.is_empty() => {
                        all.push(Line::styled("  none", Style::default().dim()));
                    }
                    Some(Ok(list)) => {
                        all.extend(list.iter().map(|e| Line::raw(format!("  {e}"))));
                    }
                }
                all.push(Line::default());
                all.extend(yaml_lines(&row.obj));
                let title = format!(
                    " {} {} · {} ",
                    row.kind.label(),
                    qualified(&row.namespace, &row.name),
                    row.cluster
                );
                *scroll = (*scroll).min(all.len().saturating_sub(1));
                frame.render_widget(
                    Paragraph::new(all.into_iter().skip(*scroll).collect::<Vec<_>>())
                        .block(Block::default().borders(Borders::ALL).title(title)),
                    area,
                );
            }
            Some(Focus::Diff {
                title,
                lines,
                changed,
                scroll,
            }) => {
                let title = format!(" {title} · {changed} lines differ ");
                frame.render_widget(
                    Paragraph::new(lines.iter().skip(*scroll).cloned().collect::<Vec<_>>())
                        .block(Block::default().borders(Borders::ALL).title(title)),
                    area,
                );
            }
            Some(Focus::Logs {
                title,
                pane,
                pending,
                problem,
            }) => {
                if let Some(found) = take(pending) {
                    receive_sources(title, pane, problem, found);
                }
                if let Some(p) = problem {
                    frame.render_widget(
                        Paragraph::new(Line::styled(
                            printable(p),
                            Style::default().fg(Color::Yellow),
                        ))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(format!(" {title} ")),
                        ),
                        area,
                    );
                } else {
                    pane.draw(frame, area);
                }
            }
            None => {}
        }
    }
}

fn qualified(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        printable(name)
    } else {
        format!("{}/{}", printable(namespace), printable(name))
    }
}

/// A text being typed: prompt, text, cursor
fn input_line(prompt: &'static str, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(prompt, KEY),
        Span::raw(text.to_string()),
        Span::styled("█", Style::default().fg(Color::Cyan)),
    ])
}

/// The log sources found for a logs focus: into the pane, or why there are none
fn receive_sources(
    title: &str,
    pane: &mut LogPane,
    problem: &mut Option<String>,
    found: Result<Vec<LogSource>, String>,
) {
    match found {
        Ok(sources) if sources.is_empty() => *problem = Some("No pods found".to_string()),
        Ok(sources) => pane.set_sources(title.to_string(), sources, String::new()),
        Err(e) => *problem = Some(e),
    }
}

fn cluster_count(rows: &[&ObjectRow]) -> usize {
    let mut names: Vec<&str> = rows.iter().map(|r| r.cluster.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.len()
}

/// Every container of a pod row, labelled for a merged pane
fn pod_sources(row: &ObjectRow, with_cluster: bool) -> Vec<LogSource> {
    let containers: Vec<String> = array_at(&row.obj, &["spec", "containers"])
        .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    let restarted = |name: &str| {
        array_at(&row.obj, &["status", "containerStatuses"]).any(|s| {
            s.get("name").and_then(Value::as_str) == Some(name)
                && s.pointer("/lastState/terminated").is_some()
        })
    };
    let several = containers.len() > 1;
    containers
        .iter()
        .map(|c| {
            let mut label = if with_cluster {
                format!("{}/{}", row.cluster, row.name)
            } else {
                row.name.clone()
            };
            if several {
                label = format!("{label}/{c}");
            }
            LogSource {
                namespace: row.namespace.clone(),
                pod: row.name.clone(),
                container: c.clone(),
                label,
                has_previous: restarted(c),
                talos: None,
                file: None,
                cluster: Some(row.cluster.clone()),
            }
        })
        .collect()
}

/// The pods of `selector` in `namespace` of each cluster, as log sources
async fn pods_by_selector(
    clients: Vec<(String, Client)>,
    namespace: &str,
    selector: &str,
) -> Result<Vec<LogSource>, String> {
    let pod = pod_kind();
    let ar = api_resource(&pod);
    let several = clients.len() > 1;
    let reads = clients.into_iter().map(|(cluster, client)| {
        let ar = ar.clone();
        async move {
            let api: Api<DynamicObject> = Api::namespaced_with(client, namespace, &ar);
            let params = ListParams::default().labels(selector);
            let list = crate::kube_list::list(&api, &params).await;
            (cluster, list)
        }
    });
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    for (cluster, list) in futures::future::join_all(reads).await {
        match list {
            Ok(list) => {
                for item in list.items {
                    let Some(row) = to_row(&cluster, &pod, &item, 0) else {
                        continue;
                    };
                    sources.extend(pod_sources(&row, several));
                }
            }
            Err(e) => errors.push(format!("{cluster}: {}", crate::kube_list::error_text(&e))),
        }
    }
    if sources.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(sources)
}

/// An object as a row: redacted, its status and Flux details read
fn to_row(cluster: &str, kind: &KindRef, obj: &DynamicObject, now: i64) -> Option<ObjectRow> {
    let mut value = serde_json::to_value(obj).ok()?;
    redact(&kind.kind, &mut value);
    let name = obj.metadata.name.clone().unwrap_or_default();
    let namespace = obj.metadata.namespace.clone().unwrap_or_default();
    let status = status(&kind.kind, &value, now);
    let flux = is_gitops_group(&kind.group).then(|| flux_info(&kind.kind, &value));
    Some(ObjectRow {
        id: row_id(cluster, kind, &namespace, &name),
        cluster: cluster.to_string(),
        kind: kind.clone(),
        namespace,
        name,
        status,
        age_secs: talos9s_core::kubernetes::age_secs(
            &value,
            &["metadata", "creationTimestamp"],
            now,
        ),
        flux,
        obj: Arc::new(value),
    })
}

/// Every kind of `kinds` in one cluster: rows, failed kinds, capped kinds
async fn list_cluster(
    cluster: &str,
    client: &Client,
    kinds: &[KindRef],
    mode: &Mode,
) -> (Vec<ObjectRow>, Vec<String>, Vec<String>) {
    let mut params = ListParams::default().limit(ROW_CAP);
    if let Mode::Search(Query::Label(selector)) = mode {
        params = params.labels(selector);
    }
    let reads = kinds.iter().map(|k| {
        let api: Api<DynamicObject> = Api::all_with(client.clone(), &api_resource(k));
        let params = params.clone();
        async move { (k, crate::kube_list::list(&api, &params).await) }
    });
    let now = chrono::Utc::now().timestamp();
    let mut rows = Vec::new();
    let mut failed = Vec::new();
    let mut truncated = Vec::new();
    for (kind, list) in futures::future::join_all(reads).await {
        match list {
            Ok(list) => {
                if list
                    .metadata
                    .continue_
                    .as_deref()
                    .is_some_and(|c| !c.is_empty())
                {
                    truncated.push(kind.plural.clone());
                }
                for item in list.items {
                    let Some(row) = to_row(cluster, kind, &item, now) else {
                        continue;
                    };
                    if let Mode::Search(q) = mode
                        && !q.matches(&row.obj)
                    {
                        continue;
                    }
                    rows.push(row);
                }
            }
            Err(e) => failed.push(format!(
                "{}: {}",
                kind.plural,
                crate::kube_list::error_text(&e)
            )),
        }
    }
    (rows, failed, truncated)
}

/// The latest events of the object with `uid`, newest first
async fn read_events(
    client: Client,
    namespace: &str,
    uid: Option<String>,
) -> Result<Vec<String>, String> {
    let Some(uid) = uid else {
        return Ok(Vec::new());
    };
    let api: Api<Event> = if namespace.is_empty() {
        Api::all(client)
    } else {
        Api::namespaced(client, namespace)
    };
    let params = ListParams::default().fields(&format!("involvedObject.uid={uid}"));
    let list = crate::kube_list::list(&api, &params)
        .await
        .map_err(|e| crate::kube_list::error_text(&e))?;
    let now = chrono::Utc::now().timestamp();
    let mut events: Vec<(i64, String)> = list
        .items
        .iter()
        .map(|e| {
            let at = e
                .last_timestamp
                .as_ref()
                .map(|t| t.0.as_second())
                .or_else(|| e.event_time.as_ref().map(|t| t.0.as_second()))
                .unwrap_or(0);
            let age = if at > 0 {
                talos9s_core::format_secs_human((now - at).unsigned_abs())
            } else {
                "?".to_string()
            };
            let count = e
                .count
                .filter(|c| *c > 1)
                .map(|c| format!(" (x{c})"))
                .unwrap_or_default();
            (
                at,
                printable(&format!(
                    "{age:>4} {} {}: {}{count}",
                    e.type_.as_deref().unwrap_or(""),
                    e.reason.as_deref().unwrap_or(""),
                    e.message.as_deref().unwrap_or("").trim()
                )),
            )
        })
        .collect();
    events.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    Ok(events.into_iter().take(30).map(|(_, e)| e).collect())
}

/// An object's summary: status, conditions, labels, images, Flux details
fn detail_lines(row: &ObjectRow) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().dim();
    let field = |name: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{name:<11}"), dim),
            Span::raw(printable(&value)),
        ])
    };
    let mut lines = vec![
        field("Cluster", row.cluster.clone()),
        field(
            "Kind",
            format!(
                "{} ({})",
                row.kind.kind,
                api_resource(&row.kind).api_version
            ),
        ),
        field("Name", qualified(&row.namespace, &row.name)),
        Line::from(vec![
            Span::styled(format!("{:<11}", "Status"), dim),
            Span::styled(
                printable(&row.status.text),
                Style::default().fg(row.status.health.health().color()),
            ),
        ]),
    ];
    if let Some(m) = &row.status.message {
        lines.push(field("", m.clone()));
    }
    if let Some(age) = row.age_secs {
        lines.push(field(
            "Age",
            talos9s_core::format_secs_human(age.unsigned_abs()),
        ));
    }
    if let Some(f) = &row.flux {
        if !f.revision.is_empty() {
            lines.push(field("Revision", f.revision.clone()));
        }
        if !f.source.is_empty() {
            lines.push(field("Source", f.source.clone()));
        }
    }
    let images = talos9s_core::kubernetes::images(&row.obj);
    if !images.is_empty() {
        lines.push(field("Images", images.join(", ")));
    }
    if let Some(sel) = pod_selector(&row.obj) {
        lines.push(field("Selector", sel));
    }
    if let Some(labels) = row
        .obj
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
    {
        let text: Vec<String> = labels
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
            .collect();
        if !text.is_empty() {
            lines.push(field("Labels", text.join(", ")));
        }
    }
    let conditions: Vec<&Value> = array_at(&row.obj, &["status", "conditions"]).collect();
    if !conditions.is_empty() {
        lines.push(Line::default());
        lines.push(Line::styled("Conditions", bold));
        for c in conditions {
            let get = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let ok = get("status") == "True";
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<22}", get("type")),
                    Style::default().fg(if ok { Color::Green } else { Color::Yellow }),
                ),
                Span::raw(printable(&format!(
                    "{} {} {}",
                    get("status"),
                    get("reason"),
                    get("message")
                ))),
            ]));
        }
    }
    lines
}

/// The object as YAML, masked like the machine config
fn yaml_text(obj: &Value) -> String {
    let yaml = serde_saphyr::to_string(obj).unwrap_or_else(|e| format!("# not shown: {e}"));
    let (lines, _) = crate::components::node_info::mask_config(&yaml);
    lines
        .into_iter()
        .map(|l| l.text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn yaml_lines(obj: &Value) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "YAML",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        yaml_text(obj)
            .lines()
            .map(|l| Line::raw(format!("  {}", printable(l)))),
    );
    lines
}

/// The diff of two objects' YAML, without status and per-object fields
fn diff_view(left: &ObjectRow, right: &ObjectRow) -> (Vec<Line<'static>>, usize) {
    let a = yaml_text(&for_compare(&left.obj));
    let b = yaml_text(&for_compare(&right.obj));
    let mut lines = vec![
        Line::styled(
            format!("- {}", left.cluster),
            Style::default().fg(Color::Red),
        ),
        Line::styled(
            format!("+ {}", right.cluster),
            Style::default().fg(Color::Green),
        ),
        Line::default(),
    ];
    let Some(diff) = diff_lines(&a, &b) else {
        lines.push(Line::styled(
            "Too large to compare here; diff the two `kubectl get -o yaml` instead",
            Style::default().fg(Color::Yellow),
        ));
        return (lines, 0);
    };
    let mut changed = 0;
    for d in diff {
        lines.push(match d {
            DiffLine::Same(l) => {
                Line::styled(format!("  {}", printable(&l)), Style::default().dim())
            }
            DiffLine::Left(l) => {
                changed += 1;
                Line::styled(
                    format!("- {}", printable(&l)),
                    Style::default().fg(Color::Red),
                )
            }
            DiffLine::Right(l) => {
                changed += 1;
                Line::styled(
                    format!("+ {}", printable(&l)),
                    Style::default().fg(Color::Green),
                )
            }
        });
    }
    if changed == 0 {
        lines.insert(
            3,
            Line::styled("Same in both clusters", Style::default().fg(Color::Green)),
        );
    }
    (lines, changed)
}

fn draw_picker(frame: &mut Frame, area: Rect, picker: &KindPicker, kinds: &[MergedKind]) {
    let entries = picker_entries(kinds, &picker.input);
    let width = area.width.min(64);
    let height = area.height.min(20);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + 1,
        width,
        height,
    };
    let visible = usize::from(height.saturating_sub(3));
    let start = picker.cursor.saturating_sub(visible.saturating_sub(1));
    let mut lines = vec![Line::from(vec![
        Span::styled(":", KEY),
        Span::raw(picker.input.clone()),
        Span::styled("█", Style::default().fg(Color::Cyan)),
    ])];
    if kinds.is_empty() {
        lines.push(Line::styled(
            "(kinds not discovered yet)",
            Style::default().dim(),
        ));
    }
    for (i, e) in entries.iter().enumerate().skip(start).take(visible) {
        let style = if i == picker.cursor {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let clusters = match e {
            PickerEntry::Kind(k) => format!("  {}", k.clusters.join(" ")),
            _ => String::new(),
        };
        lines.push(Line::from(vec![
            Span::styled(e.label(), style),
            Span::styled(clusters, Style::default().dim()),
        ]));
    }
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Kind ")),
        rect,
    );
}

impl Component for KubernetesComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if let Some(Focus::Logs { pane, .. }) = &mut self.focus {
            pane.handle_mouse(&mouse);
            return Ok(None);
        }
        if self.focus.is_none()
            && self.picker.is_none()
            && let Some(i) = self.hits.clicked(&mouse)
        {
            self.select(i);
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_line())
    }

    fn captures_text(&self) -> bool {
        self.picker.is_some()
            || self.filter_input.is_some()
            || self.search_input.is_some()
            || matches!(&self.focus, Some(Focus::Logs { pane, .. }) if pane.captures_text())
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if self.picker.is_some() {
            return Ok(self.handle_picker_key(key));
        }
        if let Some(input) = &mut self.filter_input {
            match Self::handle_input_key(key, input) {
                Some(true) => {
                    self.filter = input.trim().to_string();
                    self.filter_input = None;
                    self.rearrange();
                }
                Some(false) => self.filter_input = None,
                None => {}
            }
            return Ok(None);
        }
        if let Some(input) = &mut self.search_input {
            match Self::handle_input_key(key, input) {
                Some(true) => {
                    let query = Query::parse(input);
                    self.search_input = None;
                    if let Some(q) = query {
                        return Ok(Some(self.switch_mode(Mode::Search(q))));
                    }
                }
                Some(false) => self.search_input = None,
                None => {}
            }
            return Ok(None);
        }
        if self.focus.is_some() {
            return Ok(self.handle_focus_key(key));
        }
        Ok(self.handle_list_key(key))
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if !matches!(action, Action::Tick) {
            return Ok(None);
        }
        if let Some(Focus::Logs { pane, .. }) = &mut self.focus {
            pane.tick(None);
        }
        let auto = self.focus.is_none()
            && self
                .state
                .should_auto_refresh(self.auto_refresh, AUTO_REFRESH_INTERVAL);
        Ok((self.unread || auto).then_some(Action::Refresh))
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        if self.focus.is_some() {
            self.draw_focus(frame, area);
        } else {
            self.draw_list(frame, area);
        }
        Ok(())
    }
}

impl crate::components::BackgroundRefresh for KubernetesComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        // A list read for another mode (switched while it ran) is dropped
        if refreshed.mode != self.mode {
            return;
        }
        self.unread = false;
        self.state = refreshed.state;
        self.clients = refreshed.clients;
        self.connect_errors = refreshed.connect_errors;
        self.discovered = refreshed.discovered;
        self.rearrange();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    fn press(c: &mut KubernetesComponent, code: KeyCode) -> Option<Action> {
        c.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
            .ok()
            .flatten()
    }

    fn row(cluster: &str, kind: &str, name: &str, obj: Value) -> ObjectRow {
        let k = match kind {
            "Pod" => pod_kind(),
            other => builtin(
                "apps",
                "v1",
                other,
                &format!("{}s", other.to_lowercase()),
                true,
            ),
        };
        let mut obj = obj;
        redact(kind, &mut obj);
        let status = status(kind, &obj, chrono::Utc::now().timestamp());
        ObjectRow {
            id: row_id(cluster, &k, "apps", name),
            cluster: cluster.into(),
            kind: k,
            namespace: "apps".into(),
            name: name.into(),
            status,
            age_secs: Some(60),
            flux: None,
            obj: Arc::new(obj),
        }
    }

    fn broken() -> Value {
        json!({"metadata": {"name": "web"}, "spec": {"replicas": 2,
            "selector": {"matchLabels": {"app": "web"}},
            "template": {"spec": {"containers": [{"name": "web", "image": "nginx:1"}]}}},
            "status": {"readyReplicas": 0}})
    }

    fn healthy() -> Value {
        json!({"metadata": {"name": "web"}, "spec": {"replicas": 1,
            "template": {"spec": {"containers": [{"name": "web", "image": "nginx:2"}]}}},
            "status": {"readyReplicas": 1, "availableReplicas": 1}})
    }

    fn with_rows(rows: Vec<ObjectRow>) -> KubernetesComponent {
        let mut c = KubernetesComponent::new(Mode::Problems, Vec::new());
        let clusters = ["bc", "kprod"]
            .map(|n| ClusterResult {
                name: n.into(),
                ..Default::default()
            })
            .to_vec();
        c.state.set_data(KubeData {
            rows,
            clusters,
            kinds: Vec::new(),
        });
        c.rearrange();
        c
    }

    #[test]
    fn problems_list_only_problems_until_a() {
        let mut c = with_rows(vec![
            row("bc", "Deployment", "web", healthy()),
            row("kprod", "Deployment", "web", broken()),
        ]);
        assert_eq!(c.shown.len(), 1);
        assert_eq!(c.shown[0].cluster, "kprod");
        press(&mut c, KeyCode::Char('A'));
        assert_eq!(c.shown.len(), 2);
    }

    #[test]
    fn x_cycles_through_clusters() {
        let mut c = with_rows(vec![
            row("bc", "Deployment", "a", broken()),
            row("kprod", "Deployment", "b", broken()),
        ]);
        c.sources = ["bc", "kprod"]
            .map(|n| ClusterSource {
                name: n.into(),
                talos: None,
                cp_ip: None,
                error: None,
            })
            .to_vec();
        press(&mut c, KeyCode::Char('X'));
        assert_eq!(c.cluster_filter.as_deref(), Some("bc"));
        assert_eq!(c.shown.len(), 1);
        press(&mut c, KeyCode::Char('X'));
        assert_eq!(c.cluster_filter.as_deref(), Some("kprod"));
        press(&mut c, KeyCode::Char('X'));
        assert_eq!(c.cluster_filter, None);
        assert_eq!(c.shown.len(), 2);
    }

    #[test]
    fn d_diffs_the_same_object_in_the_other_cluster() {
        let mut c = with_rows(vec![
            row("bc", "Deployment", "web", healthy()),
            row("kprod", "Deployment", "web", broken()),
        ]);
        press(&mut c, KeyCode::Char('A'));
        press(&mut c, KeyCode::Char('d'));
        let Some(Focus::Diff { title, changed, .. }) = &c.focus else {
            panic!("no diff");
        };
        assert!(title.contains("bc ↔ kprod"), "{title}");
        assert!(*changed > 0);
        press(&mut c, KeyCode::Esc);
        assert!(c.focus.is_none());
    }

    #[test]
    fn d_marks_when_the_other_object_is_ambiguous() {
        let mut c = with_rows(vec![
            row("bc", "Deployment", "a", broken()),
            row("kprod", "Deployment", "b", broken()),
        ]);
        press(&mut c, KeyCode::Char('d'));
        assert!(c.focus.is_none());
        assert!(c.mark.is_some());
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Char('d'));
        assert!(matches!(c.focus, Some(Focus::Diff { .. })));
    }

    #[test]
    fn enter_opens_the_detail_with_masked_yaml() {
        let secret = json!({"metadata": {"name": "web"}, "spec": {"replicas": 2,
            "template": {"spec": {"containers": [{"name": "web", "image": "nginx:1",
            "env": [{"name": "PASSWORD", "value": "hunter2"}]}]}}}, "status": {}});
        let mut c = with_rows(vec![row("bc", "Deployment", "web", secret)]);
        press(&mut c, KeyCode::Enter);
        assert!(matches!(c.focus, Some(Focus::Detail { .. })));
        let text = draw(&mut c, 100, 60);
        assert!(text.contains("Deployment"), "{text}");
        assert!(text.contains("nginx:1"));
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn colon_picks_a_kind_and_s_searches() {
        let mut c = with_rows(Vec::new());
        press(&mut c, KeyCode::Char(':'));
        assert!(c.captures_text());
        for ch in "flux".chars() {
            press(&mut c, KeyCode::Char(ch));
        }
        let action = press(&mut c, KeyCode::Enter);
        assert_eq!(c.mode, Mode::GitOps);
        assert!(matches!(action, Some(Action::Refresh)));

        press(&mut c, KeyCode::Char('s'));
        for ch in "i:redis".chars() {
            press(&mut c, KeyCode::Char(ch));
        }
        press(&mut c, KeyCode::Enter);
        assert_eq!(c.mode, Mode::Search(Query::Image("redis".into())));
        // Esc leaves a search for the problems
        press(&mut c, KeyCode::Esc);
        assert_eq!(c.mode, Mode::Problems);
    }

    /// A list picked while the old one is being read is asked for until a
    /// read for it lands
    #[test]
    fn a_new_list_is_read_even_after_a_stale_refresh() {
        let mut c = with_rows(Vec::new());
        let stale = c.clone();
        press(&mut c, KeyCode::Char('s'));
        for ch in "web".chars() {
            press(&mut c, KeyCode::Char(ch));
        }
        press(&mut c, KeyCode::Enter);
        crate::components::BackgroundRefresh::adopt_refresh(&mut c, stale);
        assert!(matches!(c.update(Action::Tick), Ok(Some(Action::Refresh))));
        let fresh = c.clone();
        crate::components::BackgroundRefresh::adopt_refresh(&mut c, fresh);
        assert!(!c.unread);
    }

    #[test]
    fn a_toggles_auto_refresh() {
        let mut c = with_rows(Vec::new());
        press(&mut c, KeyCode::Char('a'));
        assert!(!c.auto_refresh);
        press(&mut c, KeyCode::Char('a'));
        assert!(c.auto_refresh);
    }

    #[test]
    fn unreachable_cluster_shows_its_error_and_the_rest() {
        let mut c = with_rows(vec![row("kprod", "Deployment", "web", broken())]);
        if let Some(d) = c.state.data_mut() {
            d.clusters[0].error = Some("Failed to connect to Kubernetes: 403\x1b[31m".into());
        }
        let text = draw(&mut c, 110, 20);
        assert!(text.contains("bc ?"), "{text}");
        assert!(text.contains("Failed to connect"));
        assert!(text.contains("web"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn kinds_depend_on_discovery() {
        let flux = builtin(
            "kustomize.toolkit.fluxcd.io",
            "v1",
            "Kustomization",
            "kustomizations",
            true,
        );
        assert!(kinds_for(&Mode::GitOps, None).is_err());
        assert_eq!(
            kinds_for(&Mode::GitOps, Some(std::slice::from_ref(&flux))),
            Ok(vec![flux.clone()])
        );
        assert_eq!(kinds_for(&Mode::GitOps, Some(&[])), Ok(Vec::new()));
        let newer = KindRef {
            version: "v2".into(),
            ..flux.clone()
        };
        assert_eq!(
            kinds_for(
                &Mode::Kind(flux.clone()),
                Some(std::slice::from_ref(&newer))
            ),
            Ok(vec![newer])
        );
        assert_eq!(kinds_for(&Mode::Kind(flux), Some(&[])), Ok(Vec::new()));
    }

    #[test]
    fn picker_shows_defaults_then_matches() {
        let merged = merge_kinds(&[(
            "bc".into(),
            vec![
                pod_kind(),
                builtin("zeta.io", "v1", "Widget", "widgets", true),
            ],
        )]);
        let empty = picker_entries(&merged, "");
        assert_eq!(empty.len(), 3); // Problems, Flux, Pod
        let found = picker_entries(&merged, "widg");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn pod_sources_name_their_cluster() {
        let pod = row(
            "kprod",
            "Pod",
            "web-1",
            json!({"spec": {"containers": [{"name": "a"}, {"name": "b"}]}, "status": {}}),
        );
        let sources = pod_sources(&pod, true);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].cluster.as_deref(), Some("kprod"));
        assert_eq!(sources[0].label, "kprod/web-1/a");
    }

    fn draw(c: &mut KubernetesComponent, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                c.draw(f, f.area()).unwrap();
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<Vec<_>>()
            .chunks(usize::from(w))
            .map(<[&str]>::concat)
            .collect::<Vec<_>>()
            .join("\n")
    }
}
