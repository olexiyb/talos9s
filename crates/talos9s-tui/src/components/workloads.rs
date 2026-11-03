//! Workloads view
//!
//! What is broken among the cluster's Deployments, StatefulSets, DaemonSets
//! and pods, in one list, worst first: each workload with its problem pods
//! under it, and the nodes its pods that aren't ready are on. `A` shows the
//! healthy ones too, `S` puts the rows in namespace sections, `/` filters.
//!
//! Health comes from the objects' status; the classification, the mapping of
//! pods to workloads and the rows are in `talos9s_core::workloads`. Each list
//! is read on its own: one that fails (RBAC, say) is reported in the summary
//! and the rest of the view works.
//!
//! The view can be scoped to a set of nodes (a node, or a cluster's control
//! plane / workers): it then lists only the workloads with pods on those nodes,
//! with how many of their pods there are ready, and only those nodes' pods.

use crate::action::Action;
use crate::components::Component;
use crate::components::pod_logs::LogPane;
use crate::ui_ext::HealthIndicatorExt;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::core::v1::{ContainerStatus, Event, Pod};
use kube::{
    Client,
    api::{Api, ListParams},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::collections::{HashMap, HashSet};
use talos9s_core::workloads::{
    ContainerFacts, EventFacts, ListOptions, ListRow, PodFacts, PodNodes, PodSort, PodWorkload,
    ProblemPod, ReplicaSetOwners, Summary, WorkloadHealth, WorkloadInfo, WorkloadKey, WorkloadKind,
    classify_workload, focus_rows, latest_event, place_pods, pod_commands, pod_workload,
    workload_command, workload_pods,
};
use talos9s_core::{AsyncState, HasHealth};

/// Auto-refresh interval
// Pod problems change slowly, and a refresh downloads every pod of the
// cluster
const AUTO_REFRESH_INTERVAL: std::time::Duration =
    talos9s_core::constants::refresh_intervals::VERY_SLOW;

/// Width of the NODES column
const NODES_WIDTH: u16 = 16;

/// Below this height the list takes the whole view, without the detail pane
const DETAIL_MIN_HEIGHT: u16 = 16;

/// The view's toggles that carry over to the next one and are saved
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkloadsSettings {
    /// Healthy workloads too (`A`)
    pub show_healthy: bool,
    /// Namespace sections (`S`)
    pub by_namespace: bool,
    /// The pod list instead of the workloads (`P`)
    pub pods: bool,
    /// The pod list's sort (`1`-`5`)
    pub pod_sort: PodSort,
}

/// Identity of a row, to keep it selected when a refresh moves it
fn row_id(row: &ListRow) -> Option<String> {
    match row {
        ListRow::Namespace(_) => None,
        ListRow::Workload(w) => Some(format!("{}/{:?}/{}", w.namespace, w.kind, w.name)),
        ListRow::Pod { pod, .. } => Some(format!("{}/pod/{}", pod.namespace, pod.name)),
        ListRow::Member(pod) => Some(format!("{}/pod/{}", pod.namespace, pod.name)),
        ListRow::Container { pod, container } => Some(format!(
            "{}/pod/{}/{}",
            pod.namespace, pod.name, container.name
        )),
    }
}

/// Write `text` to `path`, readable by the user only: logs can hold what
/// the containers printed
pub(crate) fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

/// The warp hint at the right of a summary line, only where it fits after
/// the summary's `summary_width` columns (the title names the warp anyway)
pub(crate) fn draw_warp_hint(
    frame: &mut Frame,
    area: Rect,
    summary_width: usize,
    hint: Line<'static>,
) {
    let room = usize::from(area.width);
    if summary_width + 2 + hint.width() <= room {
        frame.render_widget(
            Paragraph::new(hint).alignment(ratatui::layout::Alignment::Right),
            Rect { height: 1, ..area },
        );
    }
}

/// `<W> ns: …` for the summary line of a view that warps
pub(crate) fn warp_hint(warp: &[String]) -> Line<'static> {
    let label = talos9s_core::namespaces::label(warp, 40);
    Line::from(vec![
        Span::styled("<W>", crate::ui_ext::KEY),
        Span::styled(" ns: ", Style::default().dim()),
        if warp.is_empty() {
            Span::styled(label, Style::default().dim())
        } else {
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )
        },
    ])
}

/// Data loaded asynchronously for the workloads component
#[derive(Debug, Clone, Default)]
pub struct WorkloadsData {
    pub workloads: Vec<WorkloadInfo>,
    /// Pods with an issue
    pub pods: Vec<ProblemPod>,
    /// Lists that couldn't be read, e.g. `StatefulSets: forbidden`
    pub errors: Vec<String>,
    /// Kinds whose list couldn't be read (`?` in the summary)
    pub unavailable: Vec<WorkloadKind>,
    /// Every pod read (`None` when the pods couldn't be), for the detail pane
    pub all_pods: Option<Vec<PodFacts>>,
    pub rs_owners: Option<ReplicaSetOwners>,
    /// Each workload's conditions that aren't true: `Available: False (…)`
    pub conditions: HashMap<WorkloadKey, Vec<String>>,
    /// The latest event of each failing pod, by (namespace, pod)
    pub events: HashMap<(String, String), EventFacts>,
}

/// A Deployment's conditions that aren't true, as `Type: Reason`
fn false_conditions(d: &Deployment) -> Vec<String> {
    d.status
        .as_ref()
        .and_then(|s| s.conditions.as_deref())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let status = c.status.as_deref().filter(|s| *s != "True")?;
            let type_ = c.type_.as_deref().unwrap_or("?");
            Some(match &c.reason {
                Some(reason) => format!("{type_}: {status} ({reason})"),
                None => format!("{type_}: {status}"),
            })
        })
        .collect()
}

/// The facts of a Kubernetes event
fn event_facts(e: &Event) -> EventFacts {
    EventFacts {
        time: e
            .last_timestamp
            .as_ref()
            .map(|t| t.0.as_second())
            .or_else(|| e.event_time.as_ref().map(|t| t.0.as_second())),
        reason: e.reason.clone().unwrap_or_default(),
        message: e.message.clone().unwrap_or_default(),
    }
}

/// The latest event of each of `pods` (namespace, name, uid), at most
/// `WORKLOAD_EVENTS_MAX`, concurrently, within `WORKLOADS_EVENTS_TIMEOUT`.
/// Events only explain a failure the status already shows: any that can't
/// be read are left out.
async fn latest_events(
    client: &Client,
    pods: Vec<(String, String, String)>,
) -> HashMap<(String, String), EventFacts> {
    let requests = pods
        .into_iter()
        .take(talos9s_core::constants::WORKLOAD_EVENTS_MAX)
        .map(|(namespace, name, uid)| {
            let api: Api<Event> = Api::namespaced(client.clone(), &namespace);
            async move {
                let params = ListParams::default().fields(&format!("involvedObject.uid={uid}"));
                let list = api.list(&params).await.ok()?;
                let events: Vec<EventFacts> = list.items.iter().map(event_facts).collect();
                Some(((namespace, name), latest_event(&events)?.clone()))
            }
        });
    tokio::time::timeout(
        talos9s_core::constants::WORKLOADS_EVENTS_TIMEOUT,
        futures::future::join_all(requests),
    )
    .await
    .map(|found| found.into_iter().flatten().collect())
    .unwrap_or_default()
}

/// The facts the view needs from a Kubernetes pod
fn pod_facts(pod: &Pod) -> PodFacts {
    let container = |c: &ContainerStatus| {
        let state = c.state.as_ref();
        ContainerFacts {
            name: c.name.clone(),
            ready: c.ready,
            restarts: c.restart_count,
            waiting_reason: state
                .and_then(|s| s.waiting.as_ref())
                .and_then(|w| w.reason.clone()),
            terminated: state
                .and_then(|s| s.terminated.as_ref())
                .map(|t| (t.exit_code, t.reason.clone())),
            last_finished: c
                .last_state
                .as_ref()
                .and_then(|s| s.terminated.as_ref())
                .and_then(|t| t.finished_at.as_ref())
                .map(|t| t.0.as_second()),
            last_terminated: c
                .last_state
                .as_ref()
                .and_then(|s| s.terminated.as_ref())
                .map(|t| (t.exit_code, t.reason.clone())),
        }
    };
    let status = pod.status.as_ref();
    let owners = pod.metadata.owner_references.as_deref().unwrap_or_default();
    let owner = owners
        .iter()
        .find(|o| o.controller == Some(true))
        .or_else(|| owners.first())
        .map(|o| (o.kind.clone(), o.name.clone()));
    PodFacts {
        namespace: pod
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        name: pod
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        node: pod.spec.as_ref().and_then(|s| s.node_name.clone()),
        // No status at all: nothing to judge. A status without a phase
        // still has container states to look at.
        phase: status.map(|s| s.phase.clone().unwrap_or_else(|| "Unknown".to_string())),
        created: pod
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.as_second()),
        ready: status
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True")),
        owner,
        job_owned: owners.iter().any(|o| o.kind == "Job"),
        init_containers: status
            .and_then(|s| s.init_container_statuses.as_deref())
            .unwrap_or_default()
            .iter()
            .map(container)
            .collect(),
        containers: status
            .and_then(|s| s.container_statuses.as_deref())
            .unwrap_or_default()
            .iter()
            .map(container)
            .collect(),
    }
}

/// Deployment of each ReplicaSet, from their owner references
fn replicaset_owners(
    replicasets: impl IntoIterator<Item = kube::core::PartialObjectMeta<ReplicaSet>>,
) -> ReplicaSetOwners {
    replicasets
        .into_iter()
        .filter_map(|rs| {
            let deploy = rs
                .metadata
                .owner_references
                .as_ref()?
                .iter()
                .find(|o| o.kind == "Deployment")?
                .name
                .clone();
            Some(((rs.metadata.namespace?, rs.metadata.name?), deploy))
        })
        .collect()
}

/// A workload's namespace, name and replica counts as read from its object
struct Counted {
    namespace: String,
    name: String,
    kind: WorkloadKind,
    ready: i32,
    desired: i32,
    available: i32,
}

fn ns_name(meta: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta) -> (String, String) {
    (
        meta.namespace
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        meta.name.clone().unwrap_or_else(|| "unknown".to_string()),
    )
}

/// Workload Health Component
#[derive(Clone)]
pub struct WorkloadHealthComponent {
    /// Async state for loaded data
    state: AsyncState<WorkloadsData>,

    /// Kubernetes client
    k8s_client: Option<Client>,
    /// Where to get a Kubernetes client when there is none yet: the Talos
    /// client and a control plane node to fetch the kubeconfig from
    k8s_source: Option<(talos_rs::TalosClient, Option<String>)>,

    /// Nodes the view is limited to (`None` for the whole cluster)
    scope: Option<HashSet<String>>,

    /// Selected row among the selectable ones (headings aren't)
    selected: usize,
    /// The selected row's identity, kept across refreshes
    selected_id: Option<String>,
    table_state: TableState,
    /// Where the rows were drawn, for clicks
    rows: crate::ui_ext::RowHits,

    /// Healthy workloads too (`A`), namespace sections (`S`), filter (`/`)
    options: ListOptions,
    /// The filter being typed, while `filtering`
    filter_input: String,
    filtering: bool,

    /// Auto-refresh enabled
    auto_refresh: bool,
    /// The focused view's logs wrap (`w`), for the next one
    log_wrap: bool,
    /// Every pod in one list (`P`), sorted by `pod_sort` (`1`-`5`)
    pod_mode: bool,
    pod_sort: PodSort,
    /// The namespace picker (`W`), drawn over the view while open
    picker: Option<crate::components::namespace_picker::NamespacePicker>,
    /// This cluster's recent namespaces, for the picker
    recent_namespaces: Vec<String>,
    /// The pod list's header, to sort by clicking a column
    pod_header: crate::ui_ext::ColumnHits<PodSort>,

    /// The focused view (Enter): one workload, its pods and their logs
    focus: Option<Focus>,
}

/// One workload (or a pod of none) with its pods, the containers of the
/// expanded ones, and the logs of the selected row
#[derive(Clone)]
struct Focus {
    workload: Option<WorkloadKey>,
    /// A pod of no listed workload: only it
    pod: Option<(String, String)>,
    /// Pods whose containers are listed under them
    expanded: HashSet<String>,
    logs: LogPane,
    /// The list's selection to go back to
    back: (usize, Option<String>),
}

impl WorkloadHealthComponent {
    /// Workloads view, limited to pods on `nodes` when given
    pub fn new(nodes: Option<Vec<String>>) -> Self {
        Self {
            state: AsyncState::new(),
            k8s_client: None,
            k8s_source: None,
            scope: nodes.map(|n| n.into_iter().collect()),
            selected: 0,
            selected_id: None,
            table_state: TableState::default(),
            rows: Default::default(),
            options: ListOptions::default(),
            filter_input: String::new(),
            filtering: false,
            auto_refresh: true,
            focus: None,
            log_wrap: false,
            pod_mode: false,
            pod_sort: PodSort::default(),
            picker: None,
            recent_namespaces: Vec::new(),
            pod_header: Default::default(),
        }
    }

    pub fn settings(&self) -> WorkloadsSettings {
        WorkloadsSettings {
            show_healthy: self.options.show_healthy,
            by_namespace: self.options.by_namespace,
            pods: self.pod_mode,
            pod_sort: self.pod_sort,
        }
    }

    /// Logs wrap (`w`), shared with the Logs view: the open logs' own, else
    /// the last ones'
    pub fn log_wrap(&self) -> bool {
        self.focus.as_ref().map_or(self.log_wrap, |f| f.logs.wrap())
    }

    pub fn set_log_wrap(&mut self, wrap: bool) {
        self.log_wrap = wrap;
    }

    pub fn set_settings(&mut self, settings: WorkloadsSettings) {
        self.options.show_healthy = settings.show_healthy;
        self.options.by_namespace = settings.by_namespace;
        self.pod_mode = settings.pods;
        self.pod_sort = settings.pod_sort;
    }

    /// Access loaded data immutably
    fn data(&self) -> Option<&WorkloadsData> {
        self.state.data()
    }

    /// Set the Kubernetes client
    pub fn set_k8s_client(&mut self, client: Client) {
        self.k8s_client = Some(client);
    }

    /// Create the Kubernetes client during the (background) refresh, from
    /// the kubeconfig of `cp_ip`, so a slow fetch never blocks the UI
    pub fn set_k8s_source(&mut self, talos_client: talos_rs::TalosClient, cp_ip: Option<String>) {
        self.k8s_source = Some((talos_client, cp_ip));
    }

    /// Show the loading state until the first background refresh lands
    pub fn start_loading(&mut self) {
        self.state.start_loading();
    }

    /// Set an error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Refresh workload data from the cluster
    pub async fn refresh(&mut self) -> Result<()> {
        if self.k8s_client.is_none()
            && let Some((talos_client, cp_ip)) = &self.k8s_source
        {
            // Pin to a control plane node of the launched context so we read
            // the right cluster's workloads, not whatever an ambient
            // KUBECONFIG points at.
            match crate::components::diagnostics::k8s::connect(talos_client, cp_ip.as_deref()).await
            {
                Ok((k8s_client, _source)) => self.k8s_client = Some(k8s_client),
                Err(e) => {
                    self.set_error(format!("Failed to connect to Kubernetes: {e}"));
                    return Ok(());
                }
            }
        }
        let Some(client) = &self.k8s_client else {
            self.set_error("No Kubernetes client configured".to_string());
            return Ok(());
        };

        self.state.start_loading();

        let deployments_api: Api<Deployment> = Api::all(client.clone());
        let statefulsets_api: Api<StatefulSet> = Api::all(client.clone());
        let daemonsets_api: Api<DaemonSet> = Api::all(client.clone());
        let pods_api: Api<Pod> = Api::all(client.clone());
        let replicasets_api: Api<ReplicaSet> = Api::all(client.clone());
        let scope = self.scope.clone();

        let list_params = ListParams::default();
        let timeout = talos9s_core::constants::WORKLOADS_FETCH_TIMEOUT;
        let started = std::time::Instant::now();
        let fetched = tokio::time::timeout(timeout, async {
            tokio::join!(
                deployments_api.list(&list_params),
                statefulsets_api.list(&list_params),
                daemonsets_api.list(&list_params),
                // Scoped: the API gives each node's pods (a field selector),
                // instead of every pod of the cluster (10 MB on a busy one)
                async {
                    match &scope {
                        Some(nodes) => {
                            let per_node = nodes.iter().map(|node| {
                                let params =
                                    ListParams::default().fields(&format!("spec.nodeName={node}"));
                                let api = pods_api.clone();
                                async move { api.list(&params).await }
                            });
                            let mut pods = Vec::new();
                            for list in futures::future::join_all(per_node).await {
                                pods.extend(list?.items);
                            }
                            Ok(pods)
                        }
                        None => pods_api.list(&list_params).await.map(|l| l.items),
                    }
                },
                // Ties a Deployment's pods to it: owner references live in
                // metadata, so names only
                replicasets_api.list_metadata(&list_params),
            )
        })
        .await;
        tracing::debug!(
            "workloads: Kubernetes lists took {:?} (scope {:?})",
            started.elapsed(),
            self.scope.as_ref().map(HashSet::len)
        );
        let Ok((deployments, statefulsets, daemonsets, pods, replicasets)) = fetched else {
            self.set_error(format!("Request timed out after {}s", timeout.as_secs()));
            return Ok(());
        };

        let mut errors = Vec::new();
        let mut unavailable = Vec::new();
        let mut counted = Vec::new();
        let mut conditions = HashMap::new();
        match deployments {
            Ok(list) => {
                for d in &list.items {
                    let (namespace, name) = ns_name(&d.metadata);
                    let not_true = false_conditions(d);
                    if !not_true.is_empty() {
                        conditions.insert(
                            (namespace.clone(), WorkloadKind::Deployment, name.clone()),
                            not_true,
                        );
                    }
                    let r = crate::components::diagnostics::k8s::DeploymentReplicas::of(d);
                    counted.push(Counted {
                        namespace,
                        name,
                        kind: WorkloadKind::Deployment,
                        ready: r.ready,
                        desired: r.desired,
                        available: r.available,
                    });
                }
            }
            Err(e) => {
                errors.push(format!("Deployments: {e}"));
                unavailable.push(WorkloadKind::Deployment);
            }
        }
        match statefulsets {
            Ok(list) => {
                for sts in &list.items {
                    let (namespace, name) = ns_name(&sts.metadata);
                    let ready = sts
                        .status
                        .as_ref()
                        .and_then(|s| s.ready_replicas)
                        .unwrap_or(0);
                    // The spec's (status counts surge pods), unset meaning 1
                    let desired = sts.spec.replicas.unwrap_or(1);
                    counted.push(Counted {
                        namespace,
                        name,
                        kind: WorkloadKind::StatefulSet,
                        ready,
                        desired,
                        available: ready,
                    });
                }
            }
            Err(e) => {
                errors.push(format!("StatefulSets: {e}"));
                unavailable.push(WorkloadKind::StatefulSet);
            }
        }
        match daemonsets {
            Ok(list) => {
                for ds in &list.items {
                    let (namespace, name) = ns_name(&ds.metadata);
                    let status = ds.status.as_ref();
                    let ready = status.map_or(0, |s| s.number_ready);
                    // One per node it's scheduled on
                    let desired = status.map_or(0, |s| s.desired_number_scheduled);
                    counted.push(Counted {
                        namespace,
                        name,
                        kind: WorkloadKind::DaemonSet,
                        ready,
                        desired,
                        available: ready,
                    });
                }
            }
            Err(e) => {
                errors.push(format!("DaemonSets: {e}"));
                unavailable.push(WorkloadKind::DaemonSet);
            }
        }
        // UIDs, to read a failing pod's events
        let mut uids = HashMap::new();
        let pods: Option<Vec<PodFacts>> = match pods {
            Ok(pods) => Some(
                pods.iter()
                    .map(|p| {
                        let facts = pod_facts(p);
                        if let Some(uid) = &p.metadata.uid {
                            uids.insert((facts.namespace.clone(), facts.name.clone()), uid.clone());
                        }
                        facts
                    })
                    .collect(),
            ),
            Err(e) => {
                errors.push(format!("Pods: {e}"));
                None
            }
        };
        let rs_owners: Option<ReplicaSetOwners> = match replicasets {
            Ok(list) => Some(replicaset_owners(list.items)),
            Err(e) => {
                errors.push(format!("ReplicaSets: {e} (Deployments' pods unknown)"));
                None
            }
        };

        // Nothing readable: an error for the whole view
        if counted.is_empty() && pods.is_none() && !errors.is_empty() {
            self.set_error(errors.join("; "));
            return Ok(());
        }
        // A scoped view needs the pods to tell which workloads are there
        if scope.is_some() && pods.is_none() {
            self.set_error(errors.join("; "));
            return Ok(());
        }

        let placement = pods.as_deref().map(|p| place_pods(p, rs_owners.as_ref()));
        let mut workloads = Vec::new();
        for c in counted {
            let key = (c.namespace.clone(), c.kind, c.name.clone());
            let placed = placement.as_ref().and_then(|p| p.get(&key));
            // Scoped: only the workloads with pods on those nodes (a
            // Deployment's are unknown without ReplicaSets: left out)
            let here = match (&scope, placed) {
                (None, _) => None,
                (Some(_), Some(p)) => Some((p.ready, p.total)),
                (Some(_), None) => continue,
            };
            let pod_nodes = if placement.is_none()
                || (c.kind == WorkloadKind::Deployment && rs_owners.is_none())
            {
                PodNodes::Unknown
            } else {
                PodNodes::from_placement(placed)
            };
            workloads.push(WorkloadInfo {
                health: classify_workload(c.ready, c.desired, c.available),
                namespace: c.namespace,
                name: c.name,
                kind: c.kind,
                ready: c.ready,
                desired: c.desired,
                available: c.available,
                here,
                pod_nodes,
            });
        }

        let now = chrono::Utc::now().timestamp();
        let problem_pods: Vec<ProblemPod> = pods
            .iter()
            .flatten()
            .cloned()
            .filter_map(|pod| {
                let issue = pod.issue(now)?;
                let workload = match pod_workload(&pod, rs_owners.as_ref()) {
                    PodWorkload::Known(kind, name) => Some((pod.namespace.clone(), kind, name)),
                    PodWorkload::UnknownDeployment | PodWorkload::None => None,
                };
                Some(ProblemPod {
                    restarts: pod.restarts(),
                    containers_ready: pod.containers_ready(),
                    age_secs: pod.created.map(|c| now - c),
                    namespace: pod.namespace,
                    name: pod.name,
                    node: pod.node,
                    workload,
                    issue,
                })
            })
            .collect();

        // The worst first, so the cap drops the least interesting
        let mut failing: Vec<&ProblemPod> = problem_pods.iter().collect();
        failing.sort_by_key(|p| p.issue.severity());
        let wanted = failing
            .iter()
            .filter_map(|p| {
                let key = (p.namespace.clone(), p.name.clone());
                let uid = uids.get(&key)?.clone();
                Some((key.0, key.1, uid))
            })
            .collect();
        let events = latest_events(client, wanted).await;

        tracing::info!(
            "Loaded {} workloads ({} lists failed)",
            workloads.len(),
            errors.len()
        );
        self.state.set_data(WorkloadsData {
            workloads,
            pods: problem_pods,
            errors,
            unavailable,
            all_pods: pods,
            rs_owners,
            conditions,
            events,
        });
        self.clamp_selection();
        Ok(())
    }

    /// The list's rows for the current options
    fn list_rows(&self) -> Vec<ListRow<'_>> {
        self.data().map_or_else(Vec::new, |d| {
            let Some(focus) = &self.focus else {
                if self.pod_mode {
                    return talos9s_core::workloads::pod_rows(
                        d.all_pods.as_deref().unwrap_or_default(),
                        d.rs_owners.as_ref(),
                        &self.options,
                        self.pod_sort,
                        chrono::Utc::now().timestamp(),
                    );
                }
                return talos9s_core::workloads::list_rows(&d.workloads, &d.pods, &self.options);
            };
            let all = d.all_pods.as_deref().unwrap_or_default();
            let workload = focus
                .workload
                .as_ref()
                .and_then(|key| d.workloads.iter().find(|w| w.key() == *key));
            let pods: Vec<&PodFacts> = match (&focus.workload, &focus.pod) {
                (Some(key), _) => {
                    let known = !(key.1 == WorkloadKind::Deployment && d.rs_owners.is_none());
                    if known {
                        workload_pods(all, d.rs_owners.as_ref(), key)
                    } else {
                        Vec::new()
                    }
                }
                (None, Some((ns, name))) => all
                    .iter()
                    .filter(|p| p.namespace == *ns && p.name == *name)
                    .collect(),
                (None, None) => Vec::new(),
            };
            focus_rows(workload, &pods, &focus.expanded)
        })
    }

    /// Rows that can be selected (all but namespace headings), as indexes
    /// into the rows
    fn selectable(rows: &[ListRow]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| !matches!(r, ListRow::Namespace(_)))
            .map(|(i, _)| i)
            .collect()
    }

    /// After a refresh: the same row selected where it moved to, or the
    /// same position when it's gone
    fn clamp_selection(&mut self) {
        let rows = self.list_rows();
        let selectable = Self::selectable(&rows);
        let found = self.selected_id.as_ref().and_then(|id| {
            selectable
                .iter()
                .position(|&r| rows.get(r).and_then(row_id).as_ref() == Some(id))
        });
        let selected = found.unwrap_or(self.selected.min(selectable.len().saturating_sub(1)));
        self.select(selected);
    }

    fn select(&mut self, i: usize) {
        let rows = self.list_rows();
        let id = Self::selectable(&rows)
            .get(i)
            .and_then(|&r| rows.get(r))
            .and_then(row_id);
        self.selected = i;
        self.selected_id = id;
    }

    fn move_selection(&mut self, step: crate::ui_ext::Move) {
        let count = Self::selectable(&self.list_rows()).len();
        if let Some(i) = crate::ui_ext::moved(self.selected, count, step) {
            self.select(i);
        }
    }

    /// Options changed: the list is different, start at its top
    fn options_changed(&mut self) {
        self.select(0);
        *self.table_state.offset_mut() = 0;
    }

    /// The namespace warped into (`None`: all) and the recent ones for the
    /// picker; from the app, per cluster
    pub fn set_warp(&mut self, warp: Vec<String>, recent: Vec<String>) {
        self.recent_namespaces = recent;
        if self.options.namespaces != warp {
            self.options.namespaces = warp;
            self.options_changed();
        }
    }

    /// `W`: the picker, with how many pods (else workloads) each namespace has
    fn open_picker(&mut self) {
        let Some(data) = self.data() else { return };
        let (counts, what) = match &data.all_pods {
            Some(pods) => (
                talos9s_core::namespaces::count_by_namespace(
                    pods.iter().map(|p| p.namespace.as_str()),
                ),
                "pods",
            ),
            None => (
                talos9s_core::namespaces::count_by_namespace(
                    data.workloads.iter().map(|w| w.namespace.as_str()),
                ),
                "workloads",
            ),
        };
        self.picker = Some(crate::components::namespace_picker::NamespacePicker::new(
            counts,
            self.recent_namespaces.clone(),
            self.options.namespaces.clone(),
            what,
        ));
    }

    /// `<W> ns: …` at the right of the summary line
    fn warp_hint(&self) -> Line<'static> {
        warp_hint(&self.options.namespaces)
    }

    /// The summary line, and the lists that couldn't be read below it
    fn summary_lines(&self) -> Vec<Line<'static>> {
        let Some(data) = self.data() else {
            return Vec::new();
        };
        // Counts for the namespace warped into
        let in_warp = |ns: &str| talos9s_core::namespaces::matches(&self.options.namespaces, ns);
        let workloads: Vec<WorkloadInfo> = data
            .workloads
            .iter()
            .filter(|w| in_warp(&w.namespace))
            .cloned()
            .collect();
        let pods: Vec<ProblemPod> = data
            .pods
            .iter()
            .filter(|p| in_warp(&p.namespace))
            .cloned()
            .collect();
        let summary = Summary::of(&workloads, &pods);
        let scope = match &self.scope {
            Some(nodes) if nodes.len() == 1 => "On this node: ".to_string(),
            Some(nodes) => format!("On {} nodes: ", nodes.len()),
            None => String::new(),
        };
        let mut spans = vec![Span::styled(scope, Style::default().dim())];
        for (kind, (healthy, total)) in WorkloadKind::ALL.iter().zip(summary.kinds) {
            if !spans.is_empty() && spans.len() > 1 {
                spans.push(Span::styled(" · ", Style::default().dim()));
            }
            let (count, color) = if data.unavailable.contains(kind) {
                ("?".to_string(), Color::DarkGray)
            } else if healthy == total {
                (format!("{healthy}/{total}"), Color::Green)
            } else {
                (format!("{healthy}/{total}"), Color::Yellow)
            };
            spans.push(Span::raw(format!("{} ", kind.plural())));
            spans.push(Span::styled(
                count,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled(" · ", Style::default().dim()));
        if summary.pods_failing + summary.pods_degraded == 0 {
            spans.push(Span::styled(
                "no pod problems",
                Style::default().fg(Color::Green),
            ));
        } else {
            if summary.pods_failing > 0 {
                spans.push(Span::styled(
                    format!("{} pods failing", summary.pods_failing),
                    Style::default().fg(Color::Red),
                ));
            }
            if summary.pods_degraded > 0 {
                if summary.pods_failing > 0 {
                    spans.push(Span::raw(", "));
                }
                spans.push(Span::styled(
                    format!("{} degraded or pending", summary.pods_degraded),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
        let mut lines = vec![Line::from(spans)];
        lines.extend(data.errors.iter().map(|e| {
            Line::from(Span::styled(
                format!("Couldn't read {e}"),
                Style::default().fg(Color::Yellow),
            ))
        }));
        lines
    }

    fn draw_filter_bar(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled("Filter: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(if self.filtering {
                self.filter_input.clone()
            } else {
                self.options.filter.clone().unwrap_or_default()
            }),
        ];
        if self.filtering {
            spans.push(Span::styled("█", Style::default().fg(Color::Cyan)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// A row of the table for `row`
    fn table_row(&self, row: &ListRow) -> Row<'static> {
        let dim = Style::default().dim();
        match row {
            ListRow::Namespace(ns) => Row::new(vec![
                Cell::from(""),
                Cell::from(""),
                Cell::from(ns.to_string()).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            ListRow::Workload(w) => {
                let (symbol, color) = w.health.health().symbol_and_color();
                let ready = match w.here {
                    Some((ready, total)) => {
                        format!("{}/{} ({ready}/{total} here)", w.ready, w.desired)
                    }
                    None => format!("{}/{}", w.ready, w.desired),
                };
                // Where it fails in its color, else where it runs, dim
                let failing = w.health != WorkloadHealth::Healthy;
                let nodes = Cell::from(w.pod_nodes.label(failing, usize::from(NODES_WIDTH))).style(
                    if failing {
                        Style::default().fg(color)
                    } else {
                        dim
                    },
                );
                Row::new(vec![
                    Cell::from(symbol).style(Style::default().fg(color)),
                    Cell::from(w.kind.display()),
                    Cell::from(w.namespace.clone()).style(dim),
                    Cell::from(w.name.clone()),
                    Cell::from(ready),
                    nodes,
                    Cell::from(w.issue_text()).style(Style::default().fg(color)),
                ])
            }
            ListRow::Container { pod, container: c } => {
                let state = c
                    .waiting_reason
                    .clone()
                    .or_else(|| {
                        c.terminated
                            .as_ref()
                            .map(|(code, r)| r.clone().unwrap_or_else(|| format!("Exit {code}")))
                    })
                    .unwrap_or_else(|| {
                        if c.ready {
                            "Ready".into()
                        } else {
                            "Not ready".into()
                        }
                    });
                let init = talos9s_core::pod_logs::is_init(pod, c);
                // A finished init container is done, not "not ready"
                let done = matches!(c.terminated, Some((0, _)));
                let fine = c.ready || done;
                let color = if fine { Color::Green } else { Color::Yellow };
                Row::new(vec![
                    Cell::from("·").style(Style::default().fg(color)),
                    Cell::from(if init { "Init" } else { "Ctr" }),
                    Cell::from(pod.namespace.clone()).style(dim),
                    Cell::from(format!("    └ {}", c.name)),
                    Cell::from(match (c.ready, done) {
                        (true, _) => "1/1",
                        (false, true) => "-",
                        (false, false) => "0/1",
                    }),
                    Cell::from(""),
                    Cell::from(format!("{state} · restarts {}", c.restarts)).style(if fine {
                        dim
                    } else {
                        Style::default().fg(color)
                    }),
                ])
            }
            ListRow::Member(pod) => {
                let now = chrono::Utc::now().timestamp();
                let (status, color) = Self::pod_status(pod, now);
                let (ready, total) = pod.containers_ready();
                let issue = pod.issue(now);
                Row::new(vec![
                    Cell::from("●").style(Style::default().fg(color)),
                    Cell::from("Pod"),
                    Cell::from(pod.namespace.clone()).style(dim),
                    Cell::from(format!("└ {}", pod.name)),
                    Cell::from(format!("{ready}/{total}")),
                    Cell::from(pod.node.clone().unwrap_or_else(|| "-".to_string())),
                    Cell::from(status).style(if issue.is_some() {
                        Style::default().fg(color)
                    } else {
                        dim
                    }),
                ])
            }
            ListRow::Pod { pod, nested } => {
                let (symbol, color) = pod.issue.severity().health().symbol_and_color();
                let name = if *nested {
                    format!("└ {}", pod.name)
                } else {
                    pod.name.clone()
                };
                let (ready, total) = pod.containers_ready;
                Row::new(vec![
                    Cell::from(symbol).style(Style::default().fg(color)),
                    Cell::from("Pod"),
                    Cell::from(pod.namespace.clone()).style(dim),
                    Cell::from(name),
                    Cell::from(format!("{ready}/{total}")),
                    Cell::from(pod.node.clone().unwrap_or_else(|| "-".to_string())),
                    Cell::from(pod.issue_text()).style(Style::default().fg(color)),
                ])
            }
        }
    }

    /// A row of the pod list (`P`)
    fn pod_table_row(&self, row: &ListRow, now: i64) -> Row<'static> {
        let dim = Style::default().dim();
        match row {
            ListRow::Member(pod) => {
                let (status, color) = Self::pod_status(pod, now);
                let (ready, total) = pod.containers_ready();
                let rs = self.data().and_then(|d| d.rs_owners.as_ref());
                let age = pod.created.map_or_else(
                    || "-".to_string(),
                    |c| talos9s_core::format_secs_human((now - c).unsigned_abs()),
                );
                let restarts = pod.restarts();
                Row::new(vec![
                    Cell::from("●").style(Style::default().fg(color)),
                    Cell::from(pod.namespace.clone()).style(dim),
                    Cell::from(pod.name.clone()),
                    Cell::from(format!("{ready}/{total}")),
                    Cell::from(pod.node.clone().unwrap_or_else(|| "-".to_string())),
                    Cell::from(status).style(if pod.issue(now).is_some() {
                        Style::default().fg(color)
                    } else {
                        dim
                    }),
                    Cell::from(restarts.to_string()).style(if restarts > 0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        dim
                    }),
                    Cell::from(age).style(dim),
                    Cell::from(talos9s_core::workloads::pod_owner(pod, rs)).style(dim),
                ])
            }
            ListRow::Namespace(ns) => Row::new(vec![
                Cell::from(""),
                Cell::from(ns.to_string()).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            _ => Row::new(Vec::<Cell>::new()),
        }
    }

    /// The pod list (`P`): one row per pod, the sorted column marked
    fn draw_pod_list(&mut self, frame: &mut Frame, area: Rect) {
        let now = chrono::Utc::now().timestamp();
        let rows = self.list_rows();
        let selectable = Self::selectable(&rows);
        let table_rows: Vec<Row> = rows.iter().map(|r| self.pod_table_row(r, now)).collect();
        let namespace_width = rows
            .iter()
            .filter_map(|r| match r {
                ListRow::Member(pod) => Some(pod.namespace.chars().count()),
                _ => None,
            })
            .max()
            .unwrap_or(10)
            .clamp(10, 20);
        // STATE and NODE as wide as what they hold; the room left goes to
        // NAME and OWNER, which get cut
        let longest = |f: &dyn Fn(&PodFacts) -> usize, min: usize, max: usize| {
            rows.iter()
                .filter_map(|r| match r {
                    ListRow::Member(pod) => Some(f(pod)),
                    _ => None,
                })
                .max()
                .unwrap_or(min)
                .clamp(min, max)
        };
        let state_width = longest(&|p| Self::pod_status(p, now).0.chars().count(), 6, 22);
        let node_width = longest(
            &|p| p.node.as_deref().map_or(1, |n| n.chars().count()),
            4,
            16,
        );
        let widths = [
            Constraint::Length(1),
            Constraint::Length(talos9s_core::convert::to_u16(namespace_width)),
            Constraint::Fill(3),
            // `READY▼`
            Constraint::Length(6),
            Constraint::Length(talos9s_core::convert::to_u16(node_width)),
            Constraint::Length(talos9s_core::convert::to_u16(state_width + 1)),
            // `RESTARTS▼`
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Fill(2),
        ];
        // `1`-`5` sort by these; the sorted one is marked
        let sorted = |label: &str, by: PodSort| {
            if self.pod_sort == by {
                Cell::from(format!("{label}▼")).style(Style::default().fg(Color::Cyan))
            } else {
                Cell::from(label.to_string())
            }
        };
        let header = Row::new(vec![
            Cell::from(""),
            sorted("NAMESPACE", PodSort::Namespace),
            sorted("NAME", PodSort::Name),
            sorted("READY", PodSort::Ready),
            sorted("NODE", PodSort::Node),
            sorted("STATE", PodSort::Health),
            sorted("RESTARTS", PodSort::Restarts),
            sorted("AGE", PodSort::Age),
            sorted("OWNER", PodSort::Owner),
        ])
        .style(Style::default().add_modifier(Modifier::DIM));
        let row_count = table_rows.len();
        self.table_state
            .select(selectable.get(self.selected).copied());
        let table = Table::new(table_rows, widths)
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(table, area, &mut self.table_state);
        // Columns in `widths` order; the ones `1`-`5` sort by
        self.pod_header = crate::ui_ext::ColumnHits::new(
            area,
            &widths,
            &[
                None,
                Some(PodSort::Namespace),
                Some(PodSort::Name),
                Some(PodSort::Ready),
                Some(PodSort::Node),
                Some(PodSort::Health),
                Some(PodSort::Restarts),
                Some(PodSort::Age),
                Some(PodSort::Owner),
            ],
        );
        self.rows =
            crate::ui_ext::RowHits::new(area, false, 1, self.table_state.offset(), row_count);
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        if self.pod_mode && self.focus.is_none() {
            return self.draw_pod_list(frame, area);
        }
        let rows = self.list_rows();
        let selectable = Self::selectable(&rows);
        let table_rows: Vec<Row> = rows.iter().map(|r| self.table_row(r)).collect();
        let namespace_width = rows
            .iter()
            .filter_map(|r| match r {
                ListRow::Workload(w) => Some(w.namespace.chars().count()),
                ListRow::Pod { pod, .. } => Some(pod.namespace.chars().count()),
                ListRow::Member(pod) | ListRow::Container { pod, .. } => {
                    Some(pod.namespace.chars().count())
                }
                ListRow::Namespace(_) => None,
            })
            .max()
            .unwrap_or(9)
            .clamp(9, 20);
        let ready_width = if self.scope.is_some() { 22 } else { 7 };
        let widths = [
            Constraint::Length(1),
            Constraint::Length(6),
            Constraint::Length(talos9s_core::convert::to_u16(namespace_width)),
            Constraint::Fill(2),
            Constraint::Length(ready_width),
            Constraint::Length(NODES_WIDTH),
            Constraint::Fill(1),
        ];
        let header = Row::new(vec![
            Cell::from(""),
            Cell::from("KIND"),
            Cell::from("NAMESPACE"),
            Cell::from("NAME"),
            Cell::from("READY"),
            Cell::from("NODES"),
            Cell::from("ISSUE"),
        ])
        .style(Style::default().add_modifier(Modifier::DIM));

        let row_count = table_rows.len();
        self.table_state
            .select(selectable.get(self.selected).copied());
        let table = Table::new(table_rows, widths)
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(table, area, &mut self.table_state);
        self.rows =
            crate::ui_ext::RowHits::new(area, false, 1, self.table_state.offset(), row_count);
    }

    /// The selected row's details and the commands `y` copies
    fn detail(&self) -> Option<(Vec<Line<'static>>, Vec<String>)> {
        let data = self.data()?;
        let rows = self.list_rows();
        let row = Self::selectable(&rows)
            .get(self.selected)
            .and_then(|&r| rows.get(r))?;
        let now = chrono::Utc::now().timestamp();
        let dim = Style::default().dim();
        let mut lines = Vec::new();
        match row {
            ListRow::Namespace(_) => None,
            ListRow::Workload(w) => {
                let key = w.key();
                let (_, color) = w.health.health().symbol_and_color();
                let mut head = vec![
                    Span::styled(
                        format!("{} {}/{}", w.kind.display(), w.namespace, w.name),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" · {}/{} ready", w.ready, w.desired),
                        Style::default().fg(color),
                    ),
                ];
                for c in data.conditions.get(&key).into_iter().flatten() {
                    head.push(Span::styled(" · ", dim));
                    head.push(Span::styled(c.clone(), Style::default().fg(Color::Yellow)));
                }
                lines.push(Line::from(head));
                let known = !(w.kind == WorkloadKind::Deployment && data.rs_owners.is_none());
                let count = data
                    .all_pods
                    .as_deref()
                    .filter(|_| known)
                    .map(|all| workload_pods(all, data.rs_owners.as_ref(), &key).len());
                lines.push(match count {
                    Some(n) => Line::from(vec![
                        Span::styled(format!("  {n} pods  "), dim),
                        Span::styled("<Enter>", crate::ui_ext::KEY),
                        Span::styled(" shows them and their logs", dim),
                    ]),
                    None => Line::from(Span::styled("  Its pods are unknown", dim)),
                });
                Some((lines, vec![workload_command(w.kind, &w.namespace, &w.name)]))
            }
            ListRow::Pod { pod, .. } => {
                let (_, color) = pod.issue.severity().health().symbol_and_color();
                let facts = Self::find_pod(data, &pod.namespace, &pod.name);
                Some(self.pod_detail(
                    &pod.namespace,
                    &pod.name,
                    pod.node.as_deref(),
                    (pod.issue_text(), color),
                    facts,
                ))
            }
            ListRow::Container { .. } => None,
            ListRow::Member(pod) => Some(self.pod_detail(
                &pod.namespace,
                &pod.name,
                pod.node.as_deref(),
                Self::pod_status(pod, now),
                Some(pod),
            )),
        }
    }

    fn find_pod<'d>(data: &'d WorkloadsData, namespace: &str, name: &str) -> Option<&'d PodFacts> {
        data.all_pods
            .iter()
            .flatten()
            .find(|p| p.namespace == namespace && p.name == name)
    }

    /// A pod's issue, else what it's doing, and its color
    fn pod_status(pod: &PodFacts, now: i64) -> (String, Color) {
        match pod.issue(now) {
            Some(issue) => (
                pod.state_text(),
                issue.severity().health().symbol_and_color().1,
            ),
            // Finished is fine but not running: dimmed, as k9s and Lens
            None if pod.is_completed() => (pod.state_text(), Color::DarkGray),
            None if pod.ready => (pod.state_text(), Color::Green),
            None => (pod.state_text(), Color::Yellow),
        }
    }

    /// A pod's lines in the detail pane: containers, last terminations, its
    /// latest event; and its commands
    fn pod_detail(
        &self,
        namespace: &str,
        name: &str,
        node: Option<&str>,
        (status, color): (String, Color),
        facts: Option<&PodFacts>,
    ) -> (Vec<Line<'static>>, Vec<String>) {
        let dim = Style::default().dim();
        let now = chrono::Utc::now().timestamp();
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!("Pod {namespace}/{name}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" · {status}"), Style::default().fg(color)),
            Span::styled(
                format!(" · node {}", node.unwrap_or("- (not scheduled)")),
                dim,
            ),
        ])];
        if let Some(facts) = facts {
            for c in facts.init_containers.iter().chain(&facts.containers) {
                let state = c
                    .waiting_reason
                    .clone()
                    .or_else(|| {
                        c.terminated
                            .as_ref()
                            .map(|(code, r)| r.clone().unwrap_or_else(|| format!("Exit {code}")))
                    })
                    .unwrap_or_else(|| {
                        if c.ready {
                            "Ready".into()
                        } else {
                            "Not ready".into()
                        }
                    });
                let style = if c.ready || matches!(c.terminated, Some((0, _))) {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(color)
                };
                lines.push(Line::from(vec![
                    Span::raw(format!("  {}  ", c.name)),
                    Span::styled(state, style),
                    Span::styled(format!("  restarts {}", c.restarts), dim),
                ]));
            }
            for t in facts.last_terminations(now) {
                lines.push(Line::from(Span::styled(format!("  Last: {t}"), dim)));
            }
        }
        if let Some(event) = self
            .data()
            .and_then(|d| d.events.get(&(namespace.to_string(), name.to_string())))
        {
            lines.push(Line::from(vec![
                Span::styled("  Event: ", dim),
                Span::raw(event.text()),
            ]));
        }
        let restarted = facts.is_some_and(|f| f.restarts() > 0);
        (lines, pod_commands(namespace, name, restarted))
    }

    /// The selected row, as an index into the rows
    fn selected_row(&self) -> Option<usize> {
        Self::selectable(&self.list_rows())
            .get(self.selected)
            .copied()
    }

    /// Enter on the list: focus the selected workload (or the workload of a
    /// selected pod, or the pod alone), with that row selected
    fn open_focus(&mut self) {
        let Some(data) = self.data() else { return };
        let rows = self.list_rows();
        let Some(row) = self.selected_row().and_then(|r| rows.get(r)) else {
            return;
        };
        let listed = |key: &WorkloadKey| data.workloads.iter().any(|w| w.key() == *key);
        let (workload, pod) = match row {
            ListRow::Workload(w) => (Some(w.key()), None),
            ListRow::Pod { pod, .. } => match &pod.workload {
                Some(key) if listed(key) => (Some(key.clone()), None),
                _ => (None, Some((pod.namespace.clone(), pod.name.clone()))),
            },
            // From the pod list: the pod alone, its containers listed
            ListRow::Member(pod) => (None, Some((pod.namespace.clone(), pod.name.clone()))),
            _ => return,
        };
        let expanded: HashSet<String> = match &pod {
            Some((_, name)) if workload.is_none() => [name.clone()].into_iter().collect(),
            _ => HashSet::new(),
        };
        let id = row_id(row);
        let mut logs = LogPane::default();
        logs.set_wrap(self.log_wrap);
        self.focus = Some(Focus {
            workload,
            pod,
            expanded,
            logs,
            back: (self.selected, self.selected_id.clone()),
        });
        // The row Enter was on: the workload, or the pod
        self.selected = 0;
        self.selected_id = id;
        self.clamp_selection();
        *self.table_state.offset_mut() = 0;
        self.sync_logs();
    }

    /// Back to the list, where it was
    fn close_focus(&mut self) {
        if let Some(focus) = self.focus.take() {
            self.log_wrap = focus.logs.wrap();
            (self.selected, self.selected_id) = focus.back;
            self.clamp_selection();
        }
    }

    /// Point the logs at the selected row: all pods of the workload, all
    /// containers of a pod, or one container
    fn sync_logs(&mut self) {
        use talos9s_core::pod_logs::log_sources;
        let rows = self.list_rows();
        let Some(row) = self.selected_row().and_then(|r| rows.get(r)) else {
            return;
        };
        let (title, sources, command) = match row {
            ListRow::Workload(w) => {
                let pods: Vec<&PodFacts> = rows
                    .iter()
                    .filter_map(|r| match r {
                        ListRow::Member(p) => Some(*p),
                        _ => None,
                    })
                    .collect();
                (
                    format!("{}/{} · {} pods", w.kind.resource(), w.name, pods.len()),
                    log_sources(&pods, None),
                    format!(
                        "kubectl -n {} logs {}/{} --all-pods --all-containers --prefix",
                        w.namespace,
                        w.kind.resource(),
                        w.name
                    ),
                )
            }
            ListRow::Member(pod) => (
                pod.name.clone(),
                log_sources(&[pod], None),
                format!(
                    "kubectl -n {} logs {} --all-containers --prefix",
                    pod.namespace, pod.name
                ),
            ),
            ListRow::Container { pod, container } => (
                format!("{} · {}", pod.name, container.name),
                log_sources(&[pod], Some(&container.name)),
                format!(
                    "kubectl -n {} logs {} -c {}",
                    pod.namespace, pod.name, container.name
                ),
            ),
            _ => return,
        };
        drop(rows);
        if let Some(focus) = &mut self.focus {
            focus.logs.set_sources(title, sources, command);
        }
    }

    /// Keys of the focused view
    fn handle_focus_key(&mut self, key: KeyEvent) -> Option<Action> {
        use crate::ui_ext::Move;
        let client = self.k8s_client.clone();
        // A search being typed takes every key; Esc clears one before leaving
        if let Some(focus) = &mut self.focus
            && (focus.logs.captures_text()
                || focus.logs.selecting()
                || (key.code == KeyCode::Esc && focus.logs.has_search()))
        {
            focus.logs.handle_key(&key, client.as_ref());
            return None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.close_focus();
                return None;
            }
            // A pod's containers under it, or folded
            KeyCode::Enter => {
                let rows = self.list_rows();
                let pod = match self.selected_row().and_then(|r| rows.get(r)) {
                    Some(ListRow::Member(p) | ListRow::Container { pod: p, .. }) => {
                        Some((p.name.clone(), row_id(&ListRow::Member(p))))
                    }
                    _ => None,
                };
                drop(rows);
                if let Some((name, id)) = pod
                    && let Some(focus) = &mut self.focus
                {
                    if !focus.expanded.remove(&name) {
                        focus.expanded.insert(name);
                    }
                    self.selected_id = id;
                    self.clamp_selection();
                }
            }
            KeyCode::Char('y') => {
                if let Some(focus) = &self.focus {
                    crate::clipboard::copy_to_clipboard(focus.logs.command());
                }
            }
            // The lines read, in the external viewer (hl)
            KeyCode::Char('v') => return self.open_in_viewer(),
            KeyCode::Char('a') => self.auto_refresh = !self.auto_refresh,
            _ => match Move::from_key(&key) {
                Some(step @ (Move::Prev | Move::Next)) => self.move_selection(step),
                _ => {
                    if let Some(focus) = &mut self.focus {
                        focus.logs.handle_key(&key, client.as_ref());
                    }
                }
            },
        }
        self.sync_logs();
        None
    }

    /// Write the logs read to a private temp file for `Action::OpenInViewer`
    fn open_in_viewer(&mut self) -> Option<Action> {
        let focus = self.focus.as_mut()?;
        let text = focus.logs.export()?;
        let path = std::env::temp_dir().join(format!("talos9s-logs-{}.log", std::process::id()));
        match write_private(&path, &text) {
            Ok(()) => Some(Action::OpenInViewer(path)),
            Err(e) => {
                focus
                    .logs
                    .set_viewer_problem(format!("Couldn't write {}: {e}", path.display()));
                None
            }
        }
    }

    /// The external viewer couldn't run: say why in the logs pane
    pub fn viewer_failed(&mut self, problem: String) {
        if let Some(focus) = &mut self.focus {
            focus.logs.set_viewer_problem(problem);
        }
    }

    fn focus_key_hints(&self) -> Line<'static> {
        let key = |k: &'static str| Span::styled(k, crate::ui_ext::KEY);
        let Some(logs) = self.focus.as_ref().map(|f| &f.logs) else {
            return Line::default();
        };
        if logs.captures_text() {
            return Line::from(vec![
                key("<Enter>"),
                Span::raw(" search  "),
                key("<Esc>"),
                Span::raw(" clear"),
            ]);
        }
        if logs.selecting() {
            return Line::from(vec![
                key("<j/k>"),
                Span::raw(" extend  "),
                key("<y>"),
                Span::raw(" copy lines  "),
                key("<Esc>"),
                Span::raw(" cancel"),
            ]);
        }
        // `<PgUp/PgDn/g/G>` scroll and `<r>` reload are left out: they mean
        // that in every view, and the line must fit 85 columns
        let search = if logs.has_search() {
            [key("<n/N>"), Span::raw(" next  ")]
        } else {
            [key("</>"), Span::raw(" find  ")]
        };
        let mut spans = vec![
            key("<Enter>"),
            Span::raw(" expand  "),
            key("<PgUp/Dn>"),
            Span::raw(" page  "),
            key("<1-5/0>"),
            Span::raw(" level  "),
        ];
        spans.extend(search);
        // `<p>` only where a container shown has a previous run
        if logs.previous() || logs.has_previous() {
            spans.extend([
                key("<p>"),
                Span::raw(if logs.previous() { " now  " } else { " prev  " }),
            ]);
        }
        spans.extend([
            key("<w>"),
            Span::raw(" wrap  "),
            key("<V>"),
            Span::raw(" copy  "),
            key("<v>"),
            Span::raw(" hl"),
        ]);
        Line::from(spans)
    }

    fn draw_focus(&mut self, frame: &mut Frame, area: Rect) {
        let rows = talos9s_core::convert::to_u16(self.list_rows().len());
        // The list as tall as its rows (header included), at most a third
        let list_height = rows.saturating_add(2).min(area.height / 3).max(4);
        let [list, logs] =
            Layout::vertical([Constraint::Length(list_height), Constraint::Fill(1)]).areas(area);
        self.draw_list(frame, list);
        if let Some(focus) = &mut self.focus {
            focus.logs.draw(frame, logs);
        }
    }

    /// Copy the selected row's commands
    fn copy_commands(&self) {
        if let Some((_, commands)) = self.detail() {
            crate::clipboard::copy_lines(&commands);
        }
    }

    fn draw_detail(
        &self,
        frame: &mut Frame,
        area: Rect,
        mut lines: Vec<Line<'static>>,
        commands: &[String],
    ) {
        let dim = Style::default().dim();
        // The commands go last, so they stay visible when the pods don't fit
        let mut tail = vec![Line::from(vec![
            Span::styled("<y>", crate::ui_ext::KEY),
            Span::styled(" copies: ", dim),
            Span::styled(commands.join("  ·  "), dim),
        ])];
        let room = usize::from(area.height.saturating_sub(1));
        if lines.len() + tail.len() > room {
            lines.truncate(room.saturating_sub(tail.len()));
        }
        lines.append(&mut tail);
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::TOP)),
            area,
        );
    }

    /// Shown instead of an empty table
    fn empty_message(&self) -> Line<'static> {
        let total = self.data().map_or(0, |d| d.workloads.len());
        if let Some(filter) = &self.options.filter {
            Line::from(Span::styled(
                format!("Nothing matches \"{filter}\""),
                Style::default().fg(Color::DarkGray),
            ))
        } else if total > 0 && !self.options.show_healthy {
            Line::from(vec![
                Span::styled(
                    format!("All {total} workloads healthy  "),
                    Style::default().fg(Color::Green),
                ),
                Span::styled("<A>", crate::ui_ext::KEY),
                Span::styled(" shows them", Style::default().fg(Color::DarkGray)),
            ])
        } else if self.scope.is_some() {
            Line::from(Span::styled(
                "No workloads on these nodes",
                Style::default().fg(Color::DarkGray),
            ))
        } else {
            Line::from(Span::styled(
                "No workloads found",
                Style::default().fg(Color::DarkGray),
            ))
        }
    }

    fn key_hints_line(&self) -> Line<'static> {
        if let Some(picker) = &self.picker {
            return picker.key_hints();
        }
        if self.focus.is_some() {
            return self.focus_key_hints();
        }
        if self.filtering {
            return Line::from(vec![
                Span::styled("<Enter>", crate::ui_ext::KEY),
                Span::raw(" keep filter  "),
                Span::styled("<Esc>", crate::ui_ext::KEY),
                Span::raw(" clear"),
            ]);
        }
        let on_off = |on: bool| {
            if on {
                Span::styled("ON ", Style::default().fg(Color::Green))
            } else {
                Span::styled("OFF", Style::default().fg(Color::DarkGray))
            }
        };
        // No `<j/k>` or `<r>`: they move and refresh in every view, and the
        // line must fit 85 columns; the pod list's sort takes `<y>`'s place
        let key = |k: &'static str| Span::styled(k, crate::ui_ext::KEY);
        let mut spans = vec![
            key("</>"),
            Span::raw(" filter  "),
            key("<A>"),
            Span::raw(" all:"),
            on_off(self.options.show_healthy),
            Span::raw(" "),
            key("<S>"),
            Span::raw(" ns:"),
            on_off(self.options.by_namespace),
            Span::raw(" "),
            key("<P>"),
            Span::raw(" pods:"),
            on_off(self.pod_mode),
            Span::raw(" "),
            key("<Enter>"),
            Span::raw(" logs  "),
        ];
        if self.pod_mode {
            spans.extend([key("<1-8>"), Span::raw(" sort ")]);
        } else {
            spans.extend([key("<y>"), Span::raw(" copy  ")]);
        }
        spans.extend([key("<a>"), Span::raw(" auto:"), on_off(self.auto_refresh)]);
        Line::from(spans)
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        use crate::ui_ext::{TextEdit, edit_text};
        match edit_text(&mut self.filter_input, &key) {
            TextEdit::Cancel => {
                self.filtering = false;
                self.filter_input.clear();
                self.options.filter = None;
                self.options_changed();
            }
            // Live filter as you type; Enter keeps it and stops typing
            TextEdit::Changed | TextEdit::Submit => {
                self.options.filter =
                    (!self.filter_input.is_empty()).then(|| self.filter_input.clone());
                self.options_changed();
                if key.code == KeyCode::Enter {
                    self.filtering = false;
                }
            }
            _ => {}
        }
    }
}

impl Component for WorkloadHealthComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        // The wheel over the focused view's logs scrolls them
        if let Some(focus) = &mut self.focus
            && focus.logs.handle_mouse(&mouse)
        {
            return Ok(None);
        }
        // A column header of the pod list: sort by it, as `1`-`5`
        if self.pod_mode
            && self.focus.is_none()
            && self.picker.is_none()
            && let Some(sort) = self.pod_header.clicked(&mouse)
        {
            self.pod_sort = sort;
            self.options_changed();
            return Ok(None);
        }
        if let Some(row) = self.rows.clicked(&mouse)
            && let Some(i) = Self::selectable(&self.list_rows())
                .iter()
                .position(|&r| r == row)
        {
            self.select(i);
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn captures_text(&self) -> bool {
        self.filtering
            || self.picker.is_some()
            || self.focus.as_ref().is_some_and(|f| f.logs.captures_text())
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if self.filtering {
            self.handle_filter_key(key);
            return Ok(None);
        }
        if let Some(picker) = &mut self.picker {
            use crate::components::namespace_picker::PickerEvent;
            return Ok(match picker.handle_key(&key) {
                PickerEvent::Stay => None,
                PickerEvent::Close => {
                    self.picker = None;
                    None
                }
                PickerEvent::Pick(namespace) => {
                    self.picker = None;
                    if self.options.namespaces != namespace {
                        self.options.namespaces = namespace.clone();
                        self.options_changed();
                    }
                    Some(Action::WarpNamespace(namespace))
                }
            });
        }
        if self.focus.is_some() {
            return Ok(self.handle_focus_key(key));
        }
        match key.code {
            // A kept filter goes first, then back to the tree
            KeyCode::Esc if self.options.filter.is_some() => {
                self.options.filter = None;
                self.filter_input.clear();
                self.options_changed();
                Ok(None)
            }
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            _ if let Some(step) = crate::ui_ext::Move::from_key(&key) => {
                self.move_selection(step);
                Ok(None)
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter_input = self.options.filter.clone().unwrap_or_default();
                Ok(None)
            }
            KeyCode::Char('A') => {
                self.options.show_healthy = !self.options.show_healthy;
                self.options_changed();
                Ok(None)
            }
            KeyCode::Char('W') => {
                self.open_picker();
                Ok(None)
            }
            KeyCode::Char('P') => {
                self.pod_mode = !self.pod_mode;
                self.options_changed();
                Ok(None)
            }
            KeyCode::Char(c @ '1'..='8') if self.pod_mode => {
                let i = usize::from(u8::try_from(c).unwrap_or(b'1') - b'1');
                if let Some(&sort) = PodSort::ALL.get(i) {
                    self.pod_sort = sort;
                    self.options_changed();
                }
                Ok(None)
            }
            KeyCode::Char('S') => {
                self.options.by_namespace = !self.options.by_namespace;
                self.options_changed();
                Ok(None)
            }
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            // One workload with its pods and their logs
            KeyCode::Enter => {
                self.open_focus();
                Ok(None)
            }
            KeyCode::Char('y') => {
                self.copy_commands();
                Ok(None)
            }
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action
            && let Some(focus) = &mut self.focus
        {
            focus.logs.tick(self.k8s_client.as_ref());
        }
        if let Action::Tick = action
            && self
                .state
                .should_auto_refresh(self.auto_refresh, AUTO_REFRESH_INTERVAL)
        {
            return Ok(Some(Action::Refresh));
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Every error here is the Kubernetes API's: the Containers view
        // reads the nodes' containerd instead, so point to it
        if let Some(error) = self.state.error() {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(format!("Error: {error}"), Style::default().fg(Color::Red)),
                    Line::default(),
                    Line::from(vec![
                        Span::styled("The ", Style::default().dim()),
                        Span::styled("Containers", Style::default().fg(Color::Cyan)),
                        Span::styled(" view (", Style::default().dim()),
                        Span::styled("C", crate::ui_ext::KEY),
                        Span::styled(
                            ") reads containerd on the nodes directly, without the Kubernetes API",
                            Style::default().dim(),
                        ),
                    ]),
                ])
                .wrap(ratatui::widgets::Wrap { trim: true }),
                area,
            );
            return Ok(());
        }
        if crate::ui_ext::draw_load_gate(
            frame,
            area,
            self.state.is_loading() && !self.state.has_data(),
            None,
            "Loading workloads...",
        ) {
            return Ok(());
        }

        let summary = self.summary_lines();
        let summary_width = summary.first().map_or(0, Line::width);
        let show_filter = self.filtering || self.options.filter.is_some();
        let [summary_area, filter_area, content] = Layout::vertical([
            // The summary and a border below it
            Constraint::Length(talos9s_core::convert::to_u16(summary.len()).saturating_add(1)),
            Constraint::Length(u16::from(show_filter)),
            Constraint::Fill(1),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(summary).block(Block::default().borders(Borders::BOTTOM)),
            summary_area,
        );
        draw_warp_hint(frame, summary_area, summary_width, self.warp_hint());
        if show_filter {
            self.draw_filter_bar(frame, filter_area);
        }
        if self.picker.is_some() {
            // The view under it, then the picker over the whole view
            self.draw_content(frame, content);
            if let Some(picker) = &self.picker {
                picker.draw(frame, area);
            }
            return Ok(());
        }
        self.draw_content(frame, content);
        Ok(())
    }
}

impl WorkloadHealthComponent {
    /// The list (or the focused view) below the summary
    fn draw_content(&mut self, frame: &mut Frame, content: Rect) {
        if self.focus.is_some() {
            self.draw_focus(frame, content);
            return;
        }
        if self.list_rows().is_empty() {
            frame.render_widget(Paragraph::new(self.empty_message()), content);
            self.rows = Default::default();
            return;
        }
        // The detail pane takes up to half, and only when the list keeps
        // room for a few rows
        match self.detail() {
            Some((lines, commands)) if content.height >= DETAIL_MIN_HEIGHT => {
                let wanted = talos9s_core::convert::to_u16(lines.len() + 2);
                let [list, detail] = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(wanted.min(content.height / 2)),
                ])
                .areas(content);
                self.draw_list(frame, list);
                self.draw_detail(frame, detail, lines, &commands);
            }
            _ => self.draw_list(frame, content),
        }
    }
}

impl crate::components::BackgroundRefresh for WorkloadHealthComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        self.state = refreshed.state;
        if self.k8s_client.is_none() {
            self.k8s_client = refreshed.k8s_client;
        }
        self.clamp_selection();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use talos9s_core::workloads::PodIssue;

    fn press(c: &mut WorkloadHealthComponent, code: KeyCode) -> Option<Action> {
        c.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
            .unwrap()
    }

    /// `a` toggles auto-refresh, as in every view that has it
    #[test]
    fn a_toggles_auto_refresh() {
        let mut c = WorkloadHealthComponent::new(None);
        assert!(c.auto_refresh);
        press(&mut c, KeyCode::Char('a'));
        assert!(!c.auto_refresh);
        press(&mut c, KeyCode::Char('a'));
        assert!(c.auto_refresh);
    }

    const NOW: i64 = 1_800_000_000;

    fn ts(secs: i64) -> String {
        chrono::DateTime::from_timestamp(secs, 0)
            .unwrap()
            .to_rfc3339()
    }

    fn pod_json(v: k8s_openapi::serde_json::Value) -> Pod {
        k8s_openapi::serde_json::from_value(v).unwrap()
    }

    fn issue(pod: &Pod) -> Option<PodIssue> {
        pod_facts(pod).issue(NOW)
    }

    /// The Kubernetes objects convert to the facts the classification uses
    /// (the classification itself is tested in talos9s-core)
    #[test]
    fn pod_facts_reads_the_pod() {
        let pod = pod_json(k8s_openapi::serde_json::json!({
            "metadata": {
                "namespace": "apps", "name": "web-1",
                "creationTimestamp": ts(NOW - 600),
                "ownerReferences": [
                    {"apiVersion": "v1", "kind": "Job", "name": "j", "uid": "1"},
                    {"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "web-7d9f",
                     "uid": "2", "controller": true},
                ],
            },
            "spec": {"nodeName": "bc3", "containers": []},
            "status": {
                "phase": "Running",
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{
                    "name": "app", "image": "i", "imageID": "", "ready": false,
                    "restartCount": 4,
                    "state": {"waiting": {"reason": "CrashLoopBackOff"}},
                    "lastState": {"terminated": {"exitCode": 1, "finishedAt": ts(NOW - 60)}},
                }],
            },
        }));
        let facts = pod_facts(&pod);
        assert_eq!(facts.namespace, "apps");
        assert_eq!(facts.node.as_deref(), Some("bc3"));
        assert!(!facts.ready);
        assert!(facts.job_owned);
        // The controller, not the first owner
        assert_eq!(
            facts.owner,
            Some(("ReplicaSet".to_string(), "web-7d9f".to_string()))
        );
        assert_eq!(facts.restarts(), 4);
        assert_eq!(facts.containers[0].last_finished, Some(NOW - 60));
        assert_eq!(issue(&pod), Some(PodIssue::CrashLoopBackOff));
    }

    #[test]
    fn old_restarts_are_not_high_restarts() {
        let restarted = |finished: i64| {
            pod_json(k8s_openapi::serde_json::json!({
                "metadata": {"creationTimestamp": ts(NOW - 86_400 * 30)},
                "status": {"phase": "Running", "containerStatuses": [{
                    "name": "app", "image": "i", "imageID": "", "ready": true,
                    "restartCount": 12,
                    "state": {"running": {}},
                    "lastState": {"terminated": {"exitCode": 1, "finishedAt": ts(finished)}},
                }]},
            }))
        };
        assert_eq!(issue(&restarted(NOW - 86_400 * 7)), None);
        assert_eq!(
            issue(&restarted(NOW - 60)),
            Some(PodIssue::HighRestarts(12))
        );
    }

    #[test]
    fn stuck_init_container_is_reported() {
        let pod = pod_json(k8s_openapi::serde_json::json!({
            "status": {"phase": "Pending", "initContainerStatuses": [{
                "name": "init", "image": "i", "imageID": "", "ready": false, "restartCount": 3,
                "state": {"waiting": {"reason": "CrashLoopBackOff"}},
            }]},
        }));
        assert_eq!(issue(&pod), Some(PodIssue::CrashLoopBackOff));
    }

    /// A status without a phase still has its containers checked; no status
    /// at all is nothing to judge
    #[test]
    fn missing_phase_still_checks_containers() {
        let pod = pod_json(k8s_openapi::serde_json::json!({
            "status": {"containerStatuses": [{
                "name": "app", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
                "state": {"waiting": {"reason": "ImagePullBackOff"}},
            }]},
        }));
        assert_eq!(issue(&pod), Some(PodIssue::ImagePullBackOff));
        assert_eq!(issue(&Pod::default()), None);
    }

    fn with_data(c: &mut WorkloadHealthComponent) {
        let workload = |name: &str, ready, desired| WorkloadInfo {
            namespace: "apps".into(),
            name: name.into(),
            kind: WorkloadKind::Deployment,
            ready,
            desired,
            available: ready,
            health: classify_workload(ready, desired, ready),
            here: None,
            pod_nodes: PodNodes::Unknown,
        };
        c.state.set_data(WorkloadsData {
            workloads: vec![workload("api", 1, 3), workload("web", 2, 2)],
            ..Default::default()
        });
    }

    /// `A` shows the healthy workloads, `S` adds namespace headings that the
    /// selection skips
    #[test]
    fn show_healthy_and_sections() {
        let mut c = WorkloadHealthComponent::new(None);
        with_data(&mut c);
        assert_eq!(c.list_rows().len(), 1);
        press(&mut c, KeyCode::Char('A'));
        assert_eq!(c.list_rows().len(), 2);
        press(&mut c, KeyCode::Char('S'));
        let rows = c.list_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(WorkloadHealthComponent::selectable(&rows), [1, 2]);
        press(&mut c, KeyCode::Char('G'));
        assert_eq!(c.selected, 1);
    }

    /// A list the kubeconfig may not read (StatefulSets forbidden) is `?`
    /// with its error in the summary; the other kinds and their rows stay
    #[test]
    fn forbidden_list_degrades_to_unknown() {
        let mut c = WorkloadHealthComponent::new(None);
        with_data(&mut c);
        if let Some(data) = c.state.data_mut() {
            data.errors.push(
                "StatefulSets: statefulsets.apps is forbidden: User \"viewer\" cannot list".into(),
            );
            data.unavailable.push(WorkloadKind::StatefulSet);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 10)).unwrap();
        terminal.draw(|f| c.draw(f, f.area()).unwrap()).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("Deployments 1/2"), "{text}");
        assert!(text.contains("StatefulSets ?"), "{text}");
        assert!(
            text.contains("Couldn't read StatefulSets: statefulsets.apps is forbidden"),
            "{text}"
        );
        // The unhealthy Deployment is still listed
        assert!(text.contains("api"), "{text}");
    }

    /// Without the Kubernetes API the error points to the Containers view
    #[test]
    fn api_error_points_to_containers() {
        let mut c = WorkloadHealthComponent::new(None);
        c.set_error("Failed to connect to Kubernetes: timed out".into());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 6)).unwrap();
        terminal.draw(|f| c.draw(f, f.area()).unwrap()).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .map(|p| buffer[p].symbol().to_string())
            .collect();
        assert!(text.contains("Failed to connect to Kubernetes"), "{text}");
        assert!(
            text.contains("Containers view (C) reads containerd"),
            "{text}"
        );
    }

    /// The focused view's key line fits 85 columns in every state
    #[test]
    fn focus_key_line_fits() {
        let mut c = WorkloadHealthComponent::new(None);
        with_data(&mut c);
        press(&mut c, KeyCode::Enter);
        assert!(c.focus.is_some());
        let width = |c: &WorkloadHealthComponent| c.key_hints_line().width();
        assert!(width(&c) <= 85, "{}", width(&c));
        press(&mut c, KeyCode::Char('p'));
        press(&mut c, KeyCode::Char('/'));
        press(&mut c, KeyCode::Char('x'));
        press(&mut c, KeyCode::Enter);
        assert!(width(&c) <= 85, "{}", width(&c));
        // Nothing read yet: nothing to open
        assert!(press(&mut c, KeyCode::Char('v')).is_none());
    }

    /// `P` lists every pod, `1`-`5` sort it, Enter opens a pod with its
    /// containers; the key line fits in both modes
    #[test]
    fn p_lists_pods() {
        let mut c = WorkloadHealthComponent::new(None);
        with_data(&mut c);
        let pod = |name: &str, restarts| PodFacts {
            namespace: "apps".into(),
            name: name.into(),
            phase: Some("Running".into()),
            ready: true,
            containers: vec![ContainerFacts {
                name: "app".into(),
                ready: true,
                restarts,
                ..Default::default()
            }],
            ..Default::default()
        };
        if let Some(d) = c.state.data_mut() {
            d.all_pods = Some(vec![pod("a", 0), pod("b", 7)]);
        }
        assert!(c.key_hints_line().width() <= 85);
        press(&mut c, KeyCode::Char('P'));
        assert!(
            c.key_hints_line().width() <= 85,
            "{}",
            c.key_hints_line().width()
        );
        // Healthy pods only with `A`
        assert!(c.list_rows().is_empty());
        press(&mut c, KeyCode::Char('A'));
        press(&mut c, KeyCode::Char('6'));
        let first = |c: &WorkloadHealthComponent| match c.list_rows().first() {
            Some(ListRow::Member(p)) => p.name.clone(),
            _ => String::new(),
        };
        assert_eq!(first(&c), "b", "most restarts first");
        assert_eq!(c.settings().pod_sort, PodSort::Restarts);
        press(&mut c, KeyCode::Enter);
        assert_eq!(c.list_rows().len(), 2, "the pod and its container");
        press(&mut c, KeyCode::Esc);
        assert!(c.focus.is_none());
        assert_eq!(first(&c), "b");
    }

    /// `W` opens the picker; picking warps the list into that namespace
    /// exactly and tells the app
    #[test]
    fn w_warps_into_a_namespace() {
        let mut c = WorkloadHealthComponent::new(None);
        with_data(&mut c);
        press(&mut c, KeyCode::Char('A'));
        assert_eq!(c.list_rows().len(), 2);
        press(&mut c, KeyCode::Char('W'));
        assert!(c.captures_text());
        // Nothing matches "o": Enter just closes it
        press(&mut c, KeyCode::Char('o'));
        assert!(press(&mut c, KeyCode::Enter).is_none());
        assert!(!c.captures_text());
        press(&mut c, KeyCode::Char('W'));
        for ch in "apps".chars() {
            press(&mut c, KeyCode::Char(ch));
        }
        assert!(matches!(
            press(&mut c, KeyCode::Enter),
            Some(Action::WarpNamespace(ns)) if ns == ["apps"]
        ));
        assert_eq!(c.list_rows().len(), 2);
        c.set_warp(vec!["other".into()], Vec::new());
        assert!(c.list_rows().is_empty());
        assert!(!c.captures_text());
    }

    /// A click on a column header of the pod list sorts by it, wherever
    /// the table put that column; `1`-`8` are the columns in order
    #[test]
    fn click_a_pod_list_header_to_sort() {
        use crossterm::event::{MouseButton, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};
        let mut c = WorkloadHealthComponent::new(None);
        with_data(&mut c);
        if let Some(d) = c.state.data_mut() {
            d.all_pods = Some(vec![PodFacts {
                namespace: "apps".into(),
                name: "web-0".into(),
                node: Some("bc1".into()),
                phase: Some("Running".into()),
                ready: true,
                ..Default::default()
            }]);
        }
        press(&mut c, KeyCode::Char('P'));
        press(&mut c, KeyCode::Char('A'));
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|f| c.draw(f, f.area()).unwrap()).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let find = |label: &str| {
            (0..buffer.area.height).find_map(|y| {
                let line: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect();
                line.find(label).map(|x| (u16::try_from(x).unwrap(), y))
            })
        };
        let click = |c: &mut WorkloadHealthComponent, (x, y): (u16, u16)| {
            c.handle_mouse_event(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x + 1,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
            .unwrap();
        };
        click(&mut c, find("NODE").unwrap());
        assert_eq!(c.pod_sort, PodSort::Node);
        click(&mut c, find(" AGE").map(|(x, y)| (x + 1, y)).unwrap());
        assert_eq!(c.pod_sort, PodSort::Age);
        // Every column sorts, NAMESPACE too
        click(&mut c, find("NAMESPACE").unwrap());
        assert_eq!(c.pod_sort, PodSort::Namespace);
        // and the digits follow the columns: `8` is OWNER, the last
        press(&mut c, KeyCode::Char('8'));
        assert_eq!(c.pod_sort, PodSort::Owner);
    }

    /// Typing filters live; Esc drops a kept filter before leaving the view
    #[test]
    fn filter_then_esc() {
        let mut c = WorkloadHealthComponent::new(None);
        with_data(&mut c);
        press(&mut c, KeyCode::Char('A'));
        press(&mut c, KeyCode::Char('/'));
        assert!(c.captures_text());
        press(&mut c, KeyCode::Char('w'));
        assert_eq!(c.list_rows().len(), 1);
        press(&mut c, KeyCode::Enter);
        assert!(!c.captures_text());
        assert_eq!(c.options.filter.as_deref(), Some("w"));
        assert!(press(&mut c, KeyCode::Esc).is_none());
        assert_eq!(c.options.filter, None);
        assert!(matches!(press(&mut c, KeyCode::Esc), Some(Action::Back)));
    }

    /// The detail pane counts the selected workload's pods, Enter lists them
    /// (not ready first), and `y` would copy a rollout status command
    #[test]
    fn detail_shows_the_workloads_pods() {
        let mut c = WorkloadHealthComponent::new(None);
        let pod = |name: &str, node: &str, ready| PodFacts {
            namespace: "apps".into(),
            name: name.into(),
            node: Some(node.into()),
            phase: Some("Running".into()),
            ready,
            owner: Some(("StatefulSet".into(), "db".into())),
            ..Default::default()
        };
        c.state.set_data(WorkloadsData {
            workloads: vec![WorkloadInfo {
                namespace: "apps".into(),
                name: "db".into(),
                kind: WorkloadKind::StatefulSet,
                ready: 1,
                desired: 2,
                available: 1,
                health: classify_workload(1, 2, 1),
                here: None,
                pod_nodes: PodNodes::Unknown,
            }],
            all_pods: Some(vec![pod("db-0", "bc1", true), pod("db-1", "bc3", false)]),
            ..Default::default()
        });
        let Some((lines, commands)) = c.detail() else {
            panic!("no detail for the selected workload");
        };
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert!(text[0].starts_with("STS apps/db · 1/2 ready"), "{text:?}");
        assert!(text[1].contains("2 pods") && text[1].contains("<Enter> shows them"));
        assert_eq!(commands, ["kubectl -n apps rollout status statefulset/db"]);
        // Expanded: the pods under it, not ready first, with their node
        press(&mut c, KeyCode::Enter);
        let rows = c.list_rows();
        assert!(
            matches!(rows[1], ListRow::Member(p) if p.name == "db-1" && p.node.as_deref() == Some("bc3"))
        );
        assert!(matches!(rows[2], ListRow::Member(p) if p.name == "db-0"));
    }

    /// Enter focuses a workload: its pods, the logs of all of them; Enter
    /// on a pod lists its containers, whose logs are then one container's;
    /// Esc goes back to the list where it was
    #[test]
    fn enter_focuses_a_workload_with_its_logs() {
        let mut c = WorkloadHealthComponent::new(None);
        with_data(&mut c);
        let pod = |name: &str| PodFacts {
            namespace: "apps".into(),
            name: name.into(),
            owner: Some(("ReplicaSet".into(), "api-7f".into())),
            init_containers: vec![ContainerFacts {
                name: "migrate".into(),
                terminated: Some((0, Some("Completed".into()))),
                ..Default::default()
            }],
            containers: vec![
                // Restarted once: it has a previous run
                ContainerFacts {
                    name: "app".into(),
                    restarts: 1,
                    last_terminated: Some((1, Some("Error".into()))),
                    ..Default::default()
                },
                ContainerFacts {
                    name: "proxy".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        if let Some(d) = c.state.data_mut() {
            d.all_pods = Some(vec![pod("api-7f-a"), pod("api-7f-b")]);
            d.rs_owners = Some(
                [(("apps".into(), "api-7f".into()), "api".into())]
                    .into_iter()
                    .collect(),
            );
        }
        let command = |c: &WorkloadHealthComponent| {
            c.focus
                .as_ref()
                .map(|f| f.logs.command())
                .unwrap_or_default()
        };
        press(&mut c, KeyCode::Enter);
        assert_eq!(c.list_rows().len(), 3);
        assert!(command(&c).contains("deployment/api --all-pods"));
        press(&mut c, KeyCode::Char('j'));
        assert!(command(&c).contains("logs api-7f-a --all-containers"));
        // Expanded: the finished init container too, which the merge skips
        press(&mut c, KeyCode::Enter);
        assert_eq!(c.list_rows().len(), 6);
        press(&mut c, KeyCode::Char('j'));
        assert_eq!(command(&c), "kubectl -n apps logs api-7f-a -c migrate");
        // No previous run there: `p` does nothing and isn't offered
        press(&mut c, KeyCode::Char('p'));
        assert_eq!(command(&c), "kubectl -n apps logs api-7f-a -c migrate");
        assert!(!c.key_hints_line().to_string().contains("<p>"));
        press(&mut c, KeyCode::Char('j'));
        assert_eq!(command(&c), "kubectl -n apps logs api-7f-a -c app");
        assert!(c.key_hints_line().to_string().contains("<p>"));
        press(&mut c, KeyCode::Char('p'));
        assert_eq!(
            command(&c),
            "kubectl -n apps logs api-7f-a -c app --previous"
        );
        assert!(press(&mut c, KeyCode::Esc).is_none());
        assert!(c.focus.is_none());
        assert_eq!(c.list_rows().len(), 1);
    }
}
