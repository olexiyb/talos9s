//! Kubernetes workload health: classification of workloads and pods, which
//! workload a pod belongs to, the nodes a workload's failures are on, and the
//! rows of the Workloads view.
//!
//! The inputs are plain facts (`PodFacts`, `ContainerFacts`) the TUI reads
//! from the Kubernetes objects, so this module needs no Kubernetes types.

use crate::constants::{HIGH_RESTART_THRESHOLD, POD_PENDING_GRACE_SECS, RECENT_RESTART_SECS};
use crate::{HasHealth, HealthIndicator};
use std::collections::{BTreeSet, HashMap};

/// Health of a workload or pod, worst first (so `min` is the worst)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkloadHealth {
    /// Not working: nothing ready, or a pod crashing
    Failing,
    /// Partly working
    Degraded,
    /// Waiting: a pod not scheduled or started yet
    Pending,
    Healthy,
}

impl HasHealth for WorkloadHealth {
    fn health(&self) -> HealthIndicator {
        match self {
            WorkloadHealth::Failing => HealthIndicator::Error,
            WorkloadHealth::Degraded => HealthIndicator::Warning,
            WorkloadHealth::Pending => HealthIndicator::Pending,
            WorkloadHealth::Healthy => HealthIndicator::Healthy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkloadKind {
    Deployment,
    StatefulSet,
    DaemonSet,
}

impl WorkloadKind {
    pub const ALL: [WorkloadKind; 3] = [
        WorkloadKind::Deployment,
        WorkloadKind::StatefulSet,
        WorkloadKind::DaemonSet,
    ];

    /// Short name for the KIND column
    pub fn display(&self) -> &'static str {
        match self {
            WorkloadKind::Deployment => "Deploy",
            WorkloadKind::StatefulSet => "STS",
            WorkloadKind::DaemonSet => "DS",
        }
    }

    /// Name for the summary line
    /// The kubectl resource name
    pub fn resource(&self) -> &'static str {
        match self {
            Self::Deployment => "deployment",
            Self::StatefulSet => "statefulset",
            Self::DaemonSet => "daemonset",
        }
    }

    pub fn plural(&self) -> &'static str {
        match self {
            WorkloadKind::Deployment => "Deployments",
            WorkloadKind::StatefulSet => "StatefulSets",
            WorkloadKind::DaemonSet => "DaemonSets",
        }
    }
}

/// A workload's namespace, kind and name
pub type WorkloadKey = (String, WorkloadKind, String);

/// Health of a workload from its replica counts: `desired` from the spec,
/// `ready` and `available` from the status
pub fn classify_workload(ready: i32, desired: i32, available: i32) -> WorkloadHealth {
    if desired == 0 {
        return WorkloadHealth::Healthy; // Scaled to 0 is intentional
    }
    // More than desired happens mid-rollout (surge pods)
    if ready >= desired && available >= desired {
        return WorkloadHealth::Healthy;
    }
    if ready == 0 {
        return WorkloadHealth::Failing;
    }
    if ready < desired {
        return WorkloadHealth::Degraded;
    }
    // All ready, not all available yet (minReadySeconds)
    WorkloadHealth::Healthy
}

/// Why a pod is a problem now
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodIssue {
    CrashLoopBackOff,
    ImagePullBackOff,
    ErrImagePull,
    /// Pending past `POD_PENDING_GRACE_SECS`
    Pending,
    OOMKilled,
    Error,
    /// Restarted at least `HIGH_RESTART_THRESHOLD` times, the last one recent
    HighRestarts(i32),
}

impl PodIssue {
    pub fn display(&self) -> &'static str {
        match self {
            PodIssue::CrashLoopBackOff => "CrashLoopBackOff",
            PodIssue::ImagePullBackOff => "ImagePullBackOff",
            PodIssue::ErrImagePull => "ErrImagePull",
            PodIssue::Pending => "Pending",
            PodIssue::OOMKilled => "OOMKilled",
            PodIssue::Error => "Error",
            PodIssue::HighRestarts(_) => "Restarting",
        }
    }

    pub fn severity(&self) -> WorkloadHealth {
        match self {
            PodIssue::CrashLoopBackOff
            | PodIssue::ImagePullBackOff
            | PodIssue::ErrImagePull
            | PodIssue::OOMKilled
            | PodIssue::Error => WorkloadHealth::Failing,
            PodIssue::Pending => WorkloadHealth::Pending,
            PodIssue::HighRestarts(_) => WorkloadHealth::Degraded,
        }
    }
}

/// A container's status, as the pod reports it
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerFacts {
    pub name: String,
    pub ready: bool,
    pub restarts: i32,
    /// `state.waiting.reason`
    pub waiting_reason: Option<String>,
    /// `state.terminated`: exit code and reason
    pub terminated: Option<(i32, Option<String>)>,
    /// `lastState.terminated.finishedAt`, seconds since the epoch
    pub last_finished: Option<i64>,
    /// `lastState.terminated`: exit code and reason
    pub last_terminated: Option<(i32, Option<String>)>,
}

/// What the view needs from a pod
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodFacts {
    pub namespace: String,
    pub name: String,
    /// `spec.nodeName`; `None` while not scheduled
    pub node: Option<String>,
    /// `status.phase`; `None` when the pod has no status yet
    pub phase: Option<String>,
    /// Creation time, seconds since the epoch
    pub created: Option<i64>,
    /// The `Ready` condition is true
    pub ready: bool,
    /// The controller owner reference: kind and name
    pub owner: Option<(String, String)>,
    /// Any owner is a Job
    pub job_owned: bool,
    pub init_containers: Vec<ContainerFacts>,
    pub containers: Vec<ContainerFacts>,
}

impl PodFacts {
    /// Restarts of the app containers
    pub fn restarts(&self) -> i32 {
        self.containers.iter().map(|c| c.restarts).sum()
    }

    /// Why the pod is a problem now (`now`: seconds since the epoch), or
    /// `None`. Only current state counts: old restarts and the pods a Job
    /// leaves behind are not problems.
    pub fn issue(&self, now: i64) -> Option<PodIssue> {
        let phase = self.phase.as_deref()?;

        // A Job retries with new pods; its failed or finished pods stay
        // behind and say nothing about the Job now
        if self.job_owned && matches!(phase, "Failed" | "Succeeded") {
            return None;
        }

        // Init containers first: while one is stuck the app containers
        // haven't started
        for c in self.init_containers.iter().chain(&self.containers) {
            match c.waiting_reason.as_deref() {
                Some("CrashLoopBackOff") => return Some(PodIssue::CrashLoopBackOff),
                Some("ImagePullBackOff") => return Some(PodIssue::ImagePullBackOff),
                Some("ErrImagePull") => return Some(PodIssue::ErrImagePull),
                _ => {}
            }
            // A finished init container is terminated by design
            if let Some((exit_code, reason)) = &c.terminated
                && *exit_code != 0
            {
                match reason.as_deref() {
                    Some("OOMKilled") => return Some(PodIssue::OOMKilled),
                    Some("Error") => return Some(PodIssue::Error),
                    _ => {}
                }
            }
        }

        if phase == "Pending"
            && self
                .created
                .is_none_or(|c| now - c >= POD_PENDING_GRACE_SECS)
        {
            return Some(PodIssue::Pending);
        }
        if phase == "Failed" {
            return Some(PodIssue::Error);
        }

        let restarts = self.restarts();
        let last_restart = self.containers.iter().filter_map(|c| c.last_finished).max();
        if restarts >= HIGH_RESTART_THRESHOLD
            && last_restart.is_some_and(|t| now - t < RECENT_RESTART_SECS)
        {
            return Some(PodIssue::HighRestarts(restarts));
        }
        None
    }

    /// Ready / total app containers
    pub fn containers_ready(&self) -> (usize, usize) {
        (
            self.containers.iter().filter(|c| c.ready).count(),
            self.containers.len(),
        )
    }

    /// What the pod is doing, for the detail pane: a container's waiting or
    /// termination reason first, then the phase (`Running, not ready`)
    pub fn state_text(&self) -> String {
        let Some(phase) = self.phase.as_deref() else {
            return "Unknown".to_string();
        };
        for c in self.init_containers.iter().chain(&self.containers) {
            if let Some(reason) = &c.waiting_reason
                && reason != "PodInitializing"
            {
                return reason.clone();
            }
            if let Some((code, reason)) = &c.terminated
                && *code != 0
            {
                return reason.clone().unwrap_or_else(|| format!("Exit {code}"));
            }
        }
        if phase == "Running" && !self.ready {
            "Running, not ready".to_string()
        } else {
            phase.to_string()
        }
    }

    /// Each app container's last termination, for a pod that restarted:
    /// `app: OOMKilled (exit 137) 5m ago`
    pub fn last_terminations(&self, now: i64) -> Vec<String> {
        self.containers
            .iter()
            .filter_map(|c| {
                let (code, reason) = c.last_terminated.as_ref()?;
                let reason = reason.as_deref().unwrap_or("Terminated");
                let when = c.last_finished.map_or_else(String::new, |t| {
                    format!(
                        " {} ago",
                        crate::format_secs_human((now - t).unsigned_abs())
                    )
                });
                Some(format!("{}: {reason} (exit {code}){when}", c.name))
            })
            .collect()
    }

    /// Not ready and not done: what makes a workload short of ready pods
    fn counts_as_not_ready(&self) -> bool {
        !self.ready && self.phase.as_deref() != Some("Succeeded")
    }
}

/// Deployment of each ReplicaSet: (namespace, ReplicaSet) → Deployment
pub type ReplicaSetOwners = HashMap<(String, String), String>;

/// The workload a pod belongs to
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodWorkload {
    Known(WorkloadKind, String),
    /// Owned by a ReplicaSet while the ReplicaSets couldn't be read: some
    /// Deployment's, which one is unknown
    UnknownDeployment,
    /// Not owned by a workload (a Job's, a bare pod, a bare ReplicaSet)
    None,
}

/// The workload `pod` belongs to, through its controller owner (a
/// ReplicaSet resolves to its Deployment with `rs_owners`; `None` when the
/// ReplicaSets couldn't be read)
pub fn pod_workload(pod: &PodFacts, rs_owners: Option<&ReplicaSetOwners>) -> PodWorkload {
    let Some((kind, name)) = &pod.owner else {
        return PodWorkload::None;
    };
    match kind.as_str() {
        "StatefulSet" => PodWorkload::Known(WorkloadKind::StatefulSet, name.clone()),
        "DaemonSet" => PodWorkload::Known(WorkloadKind::DaemonSet, name.clone()),
        "ReplicaSet" => match rs_owners {
            None => PodWorkload::UnknownDeployment,
            Some(owners) => owners
                .get(&(pod.namespace.clone(), name.clone()))
                .map_or(PodWorkload::None, |deploy| {
                    PodWorkload::Known(WorkloadKind::Deployment, deploy.clone())
                }),
        },
        _ => PodWorkload::None,
    }
}

/// The pods of the workload `key` among `pods`: not ready first, then by
/// name. Empty for a Deployment when `rs_owners` is unknown.
pub fn workload_pods<'a>(
    pods: &'a [PodFacts],
    rs_owners: Option<&ReplicaSetOwners>,
    key: &WorkloadKey,
) -> Vec<&'a PodFacts> {
    let mut found: Vec<&PodFacts> = pods
        .iter()
        .filter(|p| {
            p.namespace == key.0
                && matches!(pod_workload(p, rs_owners),
                    PodWorkload::Known(kind, name) if kind == key.1 && name == key.2)
        })
        .collect();
    found.sort_by(|a, b| a.ready.cmp(&b.ready).then_with(|| a.name.cmp(&b.name)));
    found
}

/// A Kubernetes event about a pod
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFacts {
    /// Last seen (`lastTimestamp`, else `eventTime`), seconds since the epoch
    pub time: Option<i64>,
    pub reason: String,
    pub message: String,
}

impl EventFacts {
    /// `Reason: message`, on one line
    pub fn text(&self) -> String {
        let message = self
            .message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if message.is_empty() {
            self.reason.clone()
        } else {
            format!("{}: {message}", self.reason)
        }
    }
}

/// The most recent of `events` (undated ones last)
pub fn latest_event(events: &[EventFacts]) -> Option<&EventFacts> {
    events.iter().max_by_key(|e| e.time.unwrap_or(i64::MIN))
}

/// Commands copied with `y`; shown, never run
pub fn workload_command(kind: WorkloadKind, namespace: &str, name: &str) -> String {
    format!(
        "kubectl -n {namespace} rollout status {}/{name}",
        kind.resource()
    )
}

/// Commands copied with `y` for a pod: describe, and the previous logs when
/// a container restarted
pub fn pod_commands(namespace: &str, name: &str, restarted: bool) -> Vec<String> {
    let mut commands = vec![format!("kubectl -n {namespace} describe pod {name}")];
    if restarted {
        commands.push(format!("kubectl -n {namespace} logs {name} --previous"));
    }
    commands
}

/// Where a workload's pods are
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    pub ready: i32,
    pub total: i32,
    /// Nodes of all its scheduled pods
    pub nodes: BTreeSet<String>,
    /// Nodes of the pods that aren't ready
    pub not_ready_nodes: BTreeSet<String>,
    /// Pods that aren't ready and aren't scheduled
    pub unscheduled: usize,
}

/// Each workload's pods among `pods`, by workload
pub fn place_pods(
    pods: &[PodFacts],
    rs_owners: Option<&ReplicaSetOwners>,
) -> HashMap<WorkloadKey, Placement> {
    let mut placement: HashMap<WorkloadKey, Placement> = HashMap::new();
    for pod in pods {
        let PodWorkload::Known(kind, name) = pod_workload(pod, rs_owners) else {
            continue;
        };
        let entry = placement
            .entry((pod.namespace.clone(), kind, name))
            .or_default();
        entry.total += 1;
        if let Some(node) = &pod.node {
            entry.nodes.insert(node.clone());
        }
        if pod.ready {
            entry.ready += 1;
        } else if pod.counts_as_not_ready() {
            match &pod.node {
                Some(node) => {
                    entry.not_ready_nodes.insert(node.clone());
                }
                None => entry.unscheduled += 1,
            }
        }
    }
    placement
}

/// The nodes a workload's pods are on
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodNodes {
    Known {
        /// All its scheduled pods
        all: BTreeSet<String>,
        /// Its pods that aren't ready
        not_ready: BTreeSet<String>,
        /// Not ready and not scheduled
        unscheduled: usize,
    },
    /// Its pods couldn't be tied to it (pods or ReplicaSets unreadable)
    Unknown,
}

impl PodNodes {
    pub fn from_placement(placement: Option<&Placement>) -> Self {
        let p = placement.cloned().unwrap_or_default();
        PodNodes::Known {
            all: p.nodes,
            not_ready: p.not_ready_nodes,
            unscheduled: p.unscheduled,
        }
    }

    /// For the NODES column, at most `width` characters: where it fails
    /// (the nodes of its pods that aren't ready, `-` for unscheduled ones)
    /// when `failing`, else where it runs; `bc2, bc3`, or `bc2 +2` when they
    /// don't fit; `?` unknown
    pub fn label(&self, failing: bool, width: usize) -> String {
        let PodNodes::Known {
            all,
            not_ready,
            unscheduled,
        } = self
        else {
            return "?".to_string();
        };
        let mut items: Vec<&str> = if failing { not_ready } else { all }
            .iter()
            .map(String::as_str)
            .collect();
        if failing && *unscheduled > 0 {
            items.push("-");
        }
        nodes_label(&items, width)
    }

    /// Any of its pods' nodes contains `needle` (lowercase)
    pub fn contains(&self, needle: &str) -> bool {
        match self {
            PodNodes::Known { all, .. } => all.iter().any(|n| n.to_lowercase().contains(needle)),
            PodNodes::Unknown => false,
        }
    }
}

/// `items` joined with ", " when that fits `width` characters, else the
/// first and how many more (`bc2 +2`)
///
/// ```
/// use talos9s_core::workloads::nodes_label;
/// assert_eq!(nodes_label(&["bc2", "bc3"], 10), "bc2, bc3");
/// assert_eq!(nodes_label(&["bc2", "bc3", "bc4"], 10), "bc2 +2");
/// ```
pub fn nodes_label(items: &[&str], width: usize) -> String {
    let full = items.join(", ");
    if full.chars().count() <= width || items.len() <= 1 {
        return full;
    }
    let first = items.first().copied().unwrap_or_default();
    format!("{first} +{}", items.len() - 1)
}

/// A Deployment, StatefulSet or DaemonSet
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadInfo {
    pub namespace: String,
    pub name: String,
    pub kind: WorkloadKind,
    pub ready: i32,
    pub desired: i32,
    pub available: i32,
    pub health: WorkloadHealth,
    /// Ready / total pods on the scoped nodes (`None` for the whole cluster)
    pub here: Option<(i32, i32)>,
    pub pod_nodes: PodNodes,
}

impl WorkloadInfo {
    pub fn key(&self) -> WorkloadKey {
        (self.namespace.clone(), self.kind, self.name.clone())
    }

    /// For the ISSUE column; empty when healthy
    pub fn issue_text(&self) -> String {
        if self.health == WorkloadHealth::Healthy {
            String::new()
        } else if self.ready == 0 {
            "none ready".to_string()
        } else {
            format!("{} not ready", (self.desired - self.ready).max(0))
        }
    }
}

/// A pod with an issue
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemPod {
    pub namespace: String,
    pub name: String,
    pub node: Option<String>,
    /// The workload it belongs to, when known
    pub workload: Option<WorkloadKey>,
    pub issue: PodIssue,
    pub restarts: i32,
    /// Ready / total app containers
    pub containers_ready: (usize, usize),
    /// Seconds since it was created
    pub age_secs: Option<i64>,
}

impl ProblemPod {
    /// For the ISSUE column: `Pending 12m`, `Restarting (14)`, the reason
    pub fn issue_text(&self) -> String {
        match &self.issue {
            PodIssue::Pending => match self.age_secs {
                Some(age) => format!("Pending {}", crate::format_secs_human(age.unsigned_abs())),
                None => "Pending".to_string(),
            },
            PodIssue::HighRestarts(n) => format!("Restarting ({n})"),
            other => other.display().to_string(),
        }
    }
}

/// How the list is shown
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOptions {
    /// Healthy workloads too, not only problems
    pub show_healthy: bool,
    /// Rows in namespace sections instead of worst first across namespaces
    pub by_namespace: bool,
    /// Keep rows whose namespace, name or node contains this (any case)
    pub filter: Option<String>,
}

/// A row of the Workloads list
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRow<'a> {
    /// A namespace section's heading (`by_namespace`)
    Namespace(&'a str),
    Workload(&'a WorkloadInfo),
    /// A problem pod; `nested` when shown under its workload
    Pod {
        pod: &'a ProblemPod,
        nested: bool,
    },
    /// A pod of the focused workload, problem or not
    Member(&'a PodFacts),
    /// A container of an expanded pod in the focused view
    Container {
        pod: &'a PodFacts,
        container: &'a ContainerFacts,
    },
}

/// The rows of the focused view (Enter on a workload): the workload, each
/// of its pods, and every container of the `expanded` pods under them.
/// Without a workload (a pod of none), only `pods`.
pub fn focus_rows<'a>(
    workload: Option<&'a WorkloadInfo>,
    pods: &[&'a PodFacts],
    expanded: &std::collections::HashSet<String>,
) -> Vec<ListRow<'a>> {
    let mut rows: Vec<ListRow<'a>> = workload.into_iter().map(ListRow::Workload).collect();
    for &pod in pods {
        rows.push(ListRow::Member(pod));
        if expanded.contains(&pod.name) {
            rows.extend(
                crate::pod_logs::all_containers(pod)
                    .into_iter()
                    .map(|container| ListRow::Container { pod, container }),
            );
        }
    }
    rows
}

/// A workload and its problem pods, or a problem pod of no listed workload
struct Group<'a> {
    workload: Option<&'a WorkloadInfo>,
    pods: Vec<&'a ProblemPod>,
}

impl<'a> Group<'a> {
    fn health(&self) -> WorkloadHealth {
        let pods = self.pods.iter().map(|p| p.issue.severity());
        self.workload
            .map(|w| w.health)
            .into_iter()
            .chain(pods)
            .min()
            .unwrap_or(WorkloadHealth::Healthy)
    }

    fn namespace(&self) -> &'a str {
        self.workload
            .map(|w| w.namespace.as_str())
            .or_else(|| self.pods.first().map(|p| p.namespace.as_str()))
            .unwrap_or_default()
    }

    fn name(&self) -> &'a str {
        self.workload
            .map(|w| w.name.as_str())
            .or_else(|| self.pods.first().map(|p| p.name.as_str()))
            .unwrap_or_default()
    }

    fn matches(&self, needle: &str) -> bool {
        let hit = |s: &str| s.to_lowercase().contains(needle);
        self.workload
            .is_some_and(|w| hit(&w.namespace) || hit(&w.name) || w.pod_nodes.contains(needle))
            || self
                .pods
                .iter()
                .any(|p| hit(&p.namespace) || hit(&p.name) || p.node.as_deref().is_some_and(hit))
    }
}

/// The list's rows: each workload with its problem pods under it, then the
/// problem pods of no listed workload; worst first (or in namespace
/// sections), healthy workloads only with `show_healthy`
pub fn list_rows<'a>(
    workloads: &'a [WorkloadInfo],
    pods: &'a [ProblemPod],
    options: &ListOptions,
) -> Vec<ListRow<'a>> {
    let index: HashMap<WorkloadKey, usize> = workloads
        .iter()
        .enumerate()
        .map(|(i, w)| (w.key(), i))
        .collect();
    let mut groups: Vec<Group> = workloads
        .iter()
        .map(|w| Group {
            workload: Some(w),
            pods: Vec::new(),
        })
        .collect();
    for pod in pods {
        match pod.workload.as_ref().and_then(|k| index.get(k)) {
            Some(&i) => {
                if let Some(group) = groups.get_mut(i) {
                    group.pods.push(pod);
                }
            }
            None => groups.push(Group {
                workload: None,
                pods: vec![pod],
            }),
        }
    }

    let needle = options
        .filter
        .as_deref()
        .filter(|f| !f.is_empty())
        .map(str::to_lowercase);
    groups.retain(|g| {
        (options.show_healthy || g.health() != WorkloadHealth::Healthy)
            && needle.as_deref().is_none_or(|n| g.matches(n))
    });
    groups.sort_by(|a, b| {
        let by_health = a.health().cmp(&b.health());
        let by_namespace = a.namespace().cmp(b.namespace());
        let first = if options.by_namespace {
            by_namespace.then(by_health)
        } else {
            by_health.then(by_namespace)
        };
        first.then_with(|| a.name().cmp(b.name()))
    });

    let mut rows = Vec::new();
    let mut section: Option<&'a str> = None;
    for group in groups {
        if options.by_namespace && section != Some(group.namespace()) {
            section = Some(group.namespace());
            rows.push(ListRow::Namespace(group.namespace()));
        }
        let nested = group.workload.is_some();
        if let Some(w) = group.workload {
            rows.push(ListRow::Workload(w));
        }
        for pod in group.pods {
            rows.push(ListRow::Pod { pod, nested });
        }
    }
    rows
}

/// Counts for the summary line
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Healthy / total workloads per kind, in `WorkloadKind::ALL` order
    pub kinds: [(usize, usize); 3],
    pub pods_failing: usize,
    pub pods_degraded: usize,
}

impl Summary {
    pub fn of(workloads: &[WorkloadInfo], pods: &[ProblemPod]) -> Self {
        let mut summary = Summary::default();
        for w in workloads {
            let i = WorkloadKind::ALL
                .iter()
                .position(|k| *k == w.kind)
                .unwrap_or_default();
            if let Some((healthy, total)) = summary.kinds.get_mut(i) {
                *total += 1;
                *healthy += usize::from(w.health == WorkloadHealth::Healthy);
            }
        }
        for pod in pods {
            match pod.issue.severity() {
                WorkloadHealth::Failing => summary.pods_failing += 1,
                WorkloadHealth::Degraded | WorkloadHealth::Pending => summary.pods_degraded += 1,
                WorkloadHealth::Healthy => {}
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn running() -> PodFacts {
        PodFacts {
            namespace: "apps".into(),
            name: "web-1".into(),
            node: Some("bc1".into()),
            phase: Some("Running".into()),
            created: Some(NOW - 86_400),
            ready: true,
            containers: vec![ContainerFacts {
                ready: true,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn owned(kind: &str, name: &str, node: Option<&str>, ready: bool) -> PodFacts {
        PodFacts {
            node: node.map(Into::into),
            ready,
            owner: Some((kind.into(), name.into())),
            ..running()
        }
    }

    #[test]
    fn workload_health_tolerates_rollouts_and_scale_to_zero() {
        assert_eq!(classify_workload(3, 3, 3), WorkloadHealth::Healthy);
        // Surge pod during a rollout: more ready than desired
        assert_eq!(classify_workload(4, 3, 4), WorkloadHealth::Healthy);
        assert_eq!(classify_workload(0, 0, 0), WorkloadHealth::Healthy);
        assert_eq!(classify_workload(2, 3, 2), WorkloadHealth::Degraded);
        assert_eq!(classify_workload(0, 3, 0), WorkloadHealth::Failing);
    }

    #[test]
    fn old_restarts_are_not_an_issue() {
        let restarted = |finished: i64| PodFacts {
            containers: vec![ContainerFacts {
                ready: true,
                restarts: 12,
                last_finished: Some(finished),
                ..Default::default()
            }],
            ..running()
        };
        assert_eq!(restarted(NOW - 86_400 * 7).issue(NOW), None);
        assert_eq!(
            restarted(NOW - 60).issue(NOW),
            Some(PodIssue::HighRestarts(12))
        );
    }

    #[test]
    fn finished_job_pods_are_not_failing() {
        let failed = |job_owned| PodFacts {
            phase: Some("Failed".into()),
            job_owned,
            ..running()
        };
        assert_eq!(failed(true).issue(NOW), None);
        assert_eq!(failed(false).issue(NOW), Some(PodIssue::Error));
    }

    #[test]
    fn stuck_init_container_comes_first() {
        let pod = PodFacts {
            phase: Some("Pending".into()),
            init_containers: vec![ContainerFacts {
                waiting_reason: Some("CrashLoopBackOff".into()),
                ..Default::default()
            }],
            ..running()
        };
        assert_eq!(pod.issue(NOW), Some(PodIssue::CrashLoopBackOff));
        // A completed init container exits 0
        let done = PodFacts {
            init_containers: vec![ContainerFacts {
                terminated: Some((0, Some("Completed".into()))),
                ..Default::default()
            }],
            ..running()
        };
        assert_eq!(done.issue(NOW), None);
    }

    #[test]
    fn new_pending_pod_is_not_stuck() {
        let pending = |created| PodFacts {
            phase: Some("Pending".into()),
            created: Some(created),
            ..running()
        };
        assert_eq!(pending(NOW - 10).issue(NOW), None);
        assert_eq!(pending(NOW - 600).issue(NOW), Some(PodIssue::Pending));
    }

    #[test]
    fn a_pod_without_status_has_no_issue() {
        let pod = PodFacts {
            phase: None,
            ..running()
        };
        assert_eq!(pod.issue(NOW), None);
    }

    #[test]
    fn pods_resolve_to_their_workload() {
        let owners = ReplicaSetOwners::from([(("apps".into(), "web-7d9f".into()), "web".into())]);
        let pod = |kind, name| owned(kind, name, Some("bc1"), true);
        assert_eq!(
            pod_workload(&pod("ReplicaSet", "web-7d9f"), Some(&owners)),
            PodWorkload::Known(WorkloadKind::Deployment, "web".into())
        );
        assert_eq!(
            pod_workload(&pod("DaemonSet", "cilium"), Some(&owners)),
            PodWorkload::Known(WorkloadKind::DaemonSet, "cilium".into())
        );
        assert_eq!(
            pod_workload(&pod("StatefulSet", "db"), None),
            PodWorkload::Known(WorkloadKind::StatefulSet, "db".into())
        );
        // A ReplicaSet without a Deployment, a Job, no owner: no workload
        assert_eq!(
            pod_workload(&pod("ReplicaSet", "bare"), Some(&owners)),
            PodWorkload::None
        );
        assert_eq!(
            pod_workload(&pod("Job", "backup"), Some(&owners)),
            PodWorkload::None
        );
        assert_eq!(pod_workload(&running(), Some(&owners)), PodWorkload::None);
        // ReplicaSets unreadable: some Deployment's, unknown which
        assert_eq!(
            pod_workload(&pod("ReplicaSet", "web-7d9f"), None),
            PodWorkload::UnknownDeployment
        );
    }

    /// A Deployment with pods on three nodes, one not ready, and one pod
    /// not scheduled
    #[test]
    fn placement_finds_the_nodes_of_not_ready_pods() {
        let owners = ReplicaSetOwners::from([(("apps".into(), "web-7d9f".into()), "web".into())]);
        let pods = [
            owned("ReplicaSet", "web-7d9f", Some("bc1"), true),
            owned("ReplicaSet", "web-7d9f", Some("bc2"), true),
            owned("ReplicaSet", "web-7d9f", Some("bc3"), false),
            owned("ReplicaSet", "web-7d9f", None, false),
            // Done: not short of a ready pod
            PodFacts {
                phase: Some("Succeeded".into()),
                ..owned("ReplicaSet", "web-7d9f", Some("bc4"), false)
            },
        ];
        let placement = place_pods(&pods, Some(&owners));
        let web = placement
            .get(&("apps".into(), WorkloadKind::Deployment, "web".into()))
            .cloned()
            .unwrap_or_default();
        assert_eq!(web.ready, 2);
        assert_eq!(web.total, 5);
        assert_eq!(web.not_ready_nodes, BTreeSet::from(["bc3".to_string()]));
        assert_eq!(web.unscheduled, 1);
        assert_eq!(
            web.nodes,
            BTreeSet::from(["bc1", "bc2", "bc3", "bc4"].map(String::from))
        );
        let nodes = PodNodes::from_placement(Some(&web));
        assert_eq!(nodes.label(true, 20), "bc3, -");
        assert_eq!(nodes.label(false, 20), "bc1, bc2, bc3, bc4");
        assert_eq!(nodes.label(false, 10), "bc1 +3");

        // ReplicaSets unreadable: the Deployment's pods aren't placed
        assert!(place_pods(&pods, None).is_empty());
    }

    #[test]
    fn nodes_label_shortens_to_fit() {
        assert_eq!(nodes_label(&[], 10), "");
        assert_eq!(nodes_label(&["bc1"], 2), "bc1");
        assert_eq!(nodes_label(&["bc1", "bc2"], 8), "bc1, bc2");
        assert_eq!(nodes_label(&["bc1", "bc2", "bc3"], 8), "bc1 +2");
        assert_eq!(PodNodes::Unknown.label(true, 10), "?");
    }

    fn workload(name: &str, kind: WorkloadKind, ready: i32, desired: i32) -> WorkloadInfo {
        WorkloadInfo {
            namespace: "apps".into(),
            name: name.into(),
            kind,
            ready,
            desired,
            available: ready,
            health: classify_workload(ready, desired, ready),
            here: None,
            pod_nodes: PodNodes::from_placement(None),
        }
    }

    fn problem(name: &str, ns: &str, workload: Option<WorkloadKey>, issue: PodIssue) -> ProblemPod {
        ProblemPod {
            namespace: ns.into(),
            name: name.into(),
            node: Some("bc3".into()),
            workload,
            issue,
            restarts: 0,
            containers_ready: (0, 1),
            age_secs: Some(600),
        }
    }

    fn names<'a>(rows: &[ListRow<'a>]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                ListRow::Namespace(ns) => format!("# {ns}"),
                ListRow::Workload(w) => w.name.clone(),
                ListRow::Pod { pod, nested: true } => format!("  {}", pod.name),
                ListRow::Pod { pod, nested: false } => pod.name.clone(),
                ListRow::Member(pod) => format!("  {}", pod.name),
                ListRow::Container { container, .. } => format!("    {}", container.name),
            })
            .collect()
    }

    #[test]
    fn list_puts_problems_first_with_pods_under_their_workload() {
        let workloads = [
            workload("api", WorkloadKind::Deployment, 1, 3),
            workload("db", WorkloadKind::StatefulSet, 0, 1),
            workload("web", WorkloadKind::Deployment, 2, 2),
        ];
        let pods = [
            problem(
                "api-x2",
                "apps",
                Some(workloads[0].key()),
                PodIssue::CrashLoopBackOff,
            ),
            problem("backup-1", "jobs", None, PodIssue::Pending),
        ];
        let rows = list_rows(&workloads, &pods, &ListOptions::default());
        // The failing ones first (api has a crashing pod, db none ready),
        // then the pending pod; healthy web hidden
        assert_eq!(names(&rows), ["api", "  api-x2", "db", "backup-1"]);

        let all = ListOptions {
            show_healthy: true,
            ..Default::default()
        };
        assert_eq!(list_rows(&workloads, &pods, &all).len(), 5);
    }

    #[test]
    fn a_crashing_pod_lists_its_healthy_workload() {
        let workloads = [workload("web", WorkloadKind::Deployment, 2, 2)];
        let pods = [problem(
            "web-x",
            "apps",
            Some(workloads[0].key()),
            PodIssue::HighRestarts(9),
        )];
        let rows = list_rows(&workloads, &pods, &ListOptions::default());
        assert_eq!(names(&rows), ["web", "  web-x"]);
    }

    #[test]
    fn list_filters_on_namespace_name_and_node() {
        let mut api = workload("api", WorkloadKind::Deployment, 1, 3);
        api.pod_nodes = PodNodes::Known {
            all: BTreeSet::from(["bc2".to_string()]),
            not_ready: BTreeSet::new(),
            unscheduled: 0,
        };
        let workloads = [api, workload("db", WorkloadKind::StatefulSet, 0, 1)];
        let filter = |f: &str| ListOptions {
            filter: Some(f.into()),
            ..Default::default()
        };
        assert_eq!(names(&list_rows(&workloads, &[], &filter("DB"))), ["db"]);
        assert_eq!(names(&list_rows(&workloads, &[], &filter("bc2"))), ["api"]);
        assert_eq!(list_rows(&workloads, &[], &filter("apps")).len(), 2);
        assert!(list_rows(&workloads, &[], &filter("nothing")).is_empty());
    }

    #[test]
    fn list_in_namespace_sections() {
        let workloads = [workload("api", WorkloadKind::Deployment, 1, 3)];
        let pods = [
            problem("backup-1", "jobs", None, PodIssue::Error),
            problem("cron-2", "jobs", None, PodIssue::Pending),
        ];
        let options = ListOptions {
            by_namespace: true,
            ..Default::default()
        };
        assert_eq!(
            names(&list_rows(&workloads, &pods, &options)),
            ["# apps", "api", "# jobs", "backup-1", "cron-2"]
        );
    }

    #[test]
    fn summary_counts_healthy_per_kind_and_problem_pods() {
        let workloads = [
            workload("api", WorkloadKind::Deployment, 1, 3),
            workload("web", WorkloadKind::Deployment, 2, 2),
            workload("cilium", WorkloadKind::DaemonSet, 4, 4),
        ];
        let pods = [
            problem("a", "apps", None, PodIssue::Error),
            problem("b", "apps", None, PodIssue::Pending),
        ];
        let summary = Summary::of(&workloads, &pods);
        assert_eq!(summary.kinds, [(1, 2), (0, 0), (1, 1)]);
        assert_eq!(summary.pods_failing, 1);
        assert_eq!(summary.pods_degraded, 1);
    }

    #[test]
    fn pod_state_and_last_termination() {
        let mut pod = running();
        assert_eq!(pod.state_text(), "Running");
        pod.ready = false;
        assert_eq!(pod.state_text(), "Running, not ready");
        pod.containers[0].name = "app".into();
        pod.containers[0].waiting_reason = Some("CrashLoopBackOff".into());
        pod.containers[0].last_terminated = Some((137, Some("OOMKilled".into())));
        pod.containers[0].last_finished = Some(NOW - 300);
        assert_eq!(pod.state_text(), "CrashLoopBackOff");
        assert_eq!(
            pod.last_terminations(NOW),
            ["app: OOMKilled (exit 137) 5m ago"]
        );
        pod.phase = None;
        assert_eq!(pod.state_text(), "Unknown");
    }

    #[test]
    fn workload_pods_not_ready_first() {
        let pods = vec![
            PodFacts {
                name: "web-a".into(),
                ..owned("StatefulSet", "web", Some("bc1"), true)
            },
            PodFacts {
                name: "web-b".into(),
                ..owned("StatefulSet", "web", Some("bc2"), false)
            },
            owned("StatefulSet", "db", Some("bc1"), true),
        ];
        let key = (
            "apps".to_string(),
            WorkloadKind::StatefulSet,
            "web".to_string(),
        );
        let names: Vec<_> = workload_pods(&pods, None, &key)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["web-b", "web-a"]);
    }

    #[test]
    fn latest_event_and_commands() {
        let ev = |time, reason: &str| EventFacts {
            time,
            reason: reason.into(),
            message: "line\n two".into(),
        };
        let events = [
            ev(Some(10), "Pulled"),
            ev(None, "Old"),
            ev(Some(20), "BackOff"),
        ];
        assert_eq!(
            latest_event(&events).map(EventFacts::text).as_deref(),
            Some("BackOff: line two")
        );
        assert_eq!(latest_event(&[]), None);
        assert_eq!(
            workload_command(WorkloadKind::DaemonSet, "kube-system", "cilium"),
            "kubectl -n kube-system rollout status daemonset/cilium"
        );
        assert_eq!(pod_commands("a", "p", true).len(), 2);
    }

    #[test]
    fn focus_lists_pods_and_expanded_containers() {
        let w = workload("db", WorkloadKind::StatefulSet, 1, 2);
        let a = PodFacts {
            name: "db-0".into(),
            containers: vec![ContainerFacts {
                name: "pg".into(),
                ..Default::default()
            }],
            ..owned("StatefulSet", "db", Some("bc1"), true)
        };
        let b = PodFacts {
            name: "db-1".into(),
            ..a.clone()
        };
        let expanded = ["db-1".to_string()].into_iter().collect();
        let rows = focus_rows(Some(&w), &[&a, &b], &expanded);
        assert_eq!(names(&rows), ["db", "  db-0", "  db-1", "    pg"]);
    }

    #[test]
    fn issue_texts() {
        let api = workload("api", WorkloadKind::Deployment, 1, 3);
        assert_eq!(api.issue_text(), "2 not ready");
        assert_eq!(
            workload("db", WorkloadKind::StatefulSet, 0, 1).issue_text(),
            "none ready"
        );
        assert_eq!(
            workload("ok", WorkloadKind::Deployment, 2, 2).issue_text(),
            ""
        );
        assert_eq!(
            problem("p", "apps", None, PodIssue::Pending).issue_text(),
            "Pending 10m"
        );
        assert_eq!(
            problem("p", "apps", None, PodIssue::HighRestarts(14)).issue_text(),
            "Restarting (14)"
        );
    }
}
