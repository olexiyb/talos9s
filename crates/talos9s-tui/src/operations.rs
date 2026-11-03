//! Operations: cordon, uncordon, drain, reboot and shutdown, on a node or
//! node by node over a group, run in the background and recorded
//! (`plans/operations.md`).
//!
//! The app holds the runs going now in a [`Runs`] registry, one per
//! talosconfig context; the Operations view starts and cancels them there
//! and the header shows their progress. Each run writes a file under
//! `~/.talos9s/operations/` as it goes (format in
//! `talos9s_core::operations`), which the view lists as the history.

use crate::audit::{audit_failure, audit_start, audit_success};
use crate::components::diagnostics::k8s::{
    DrainOptions, PdbHealthInfo, PodImpact, check_pdb_health, cordon_node, drain_blockers,
    drain_node_with_progress, drain_preview, get_node_boot_id, pods_to_wait_for, uncordon_node,
    wait_for_node_ready,
};
use chrono::Utc;
use kube::Client;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use talos_rs::TalosClient;
use talos9s_core::operations::{
    Run, RunEnd, RunHeader, RunStatus, file_name, format_end, format_header, format_line, parse_run,
};
use talos9s_core::{EtcdQuorum, SafetyStatus};
use tokio::task::JoinHandle;

/// Bound for each etcd and PDB read of the safety checks
const SAFETY_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for etcd to allow taking a control plane node down
const ETCD_SAFE_WAIT: Duration = Duration::from_secs(180);
/// Bound for creating the Kubernetes client at the start of a run
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for one cordon, uncordon, reboot or shutdown request
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Between nodes of a reboot over a group
pub const DELAY_BETWEEN_NODES: Duration = Duration::from_secs(30);

/// What an operation does
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Cordon,
    Uncordon,
    Drain,
    Reboot,
    Shutdown,
}

impl OpKind {
    pub const ALL: [OpKind; 5] = [
        OpKind::Cordon,
        OpKind::Uncordon,
        OpKind::Drain,
        OpKind::Reboot,
        OpKind::Shutdown,
    ];

    pub fn name(self) -> &'static str {
        match self {
            OpKind::Cordon => "Cordon",
            OpKind::Uncordon => "Uncordon",
            OpKind::Drain => "Drain",
            OpKind::Reboot => "Reboot",
            OpKind::Shutdown => "Shutdown",
        }
    }

    /// What it does, for the operations list and the dialog
    pub fn about(self, group: bool) -> &'static str {
        match (self, group) {
            (OpKind::Cordon, _) => "stop new pods, keep the running ones",
            (OpKind::Uncordon, _) => "allow new pods again",
            (OpKind::Drain, false) => "cordon, evict pods; stays cordoned",
            (OpKind::Drain, true) => "each node in turn; all stay cordoned",
            (OpKind::Reboot, false) => "drain, reboot, wait for Ready, uncordon",
            (OpKind::Reboot, true) => "node by node, etcd quorum checked",
            (OpKind::Shutdown, _) => "drain, power off; stays cordoned",
        }
    }

    /// Offered on a group or cluster: shutting a whole group down at once
    /// is not something to start from a list
    pub fn on_group(self) -> bool {
        self != OpKind::Shutdown
    }

    /// Moves pods: worth the PDB check
    pub fn drains(self) -> bool {
        matches!(self, OpKind::Drain | OpKind::Reboot | OpKind::Shutdown)
    }

    /// Takes a node down: worth the etcd quorum check
    pub fn takes_down(self) -> bool {
        matches!(self, OpKind::Reboot | OpKind::Shutdown)
    }

    /// The operations offered for one node or a group
    pub fn offered(group: bool) -> Vec<OpKind> {
        Self::ALL
            .into_iter()
            .filter(|k| !group || k.on_group())
            .collect()
    }
}

/// A node an operation runs on
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpNode {
    pub hostname: String,
    /// Talos API address
    pub address: String,
    pub is_controlplane: bool,
}

/// Whether a node answers the Talos API
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TalosState {
    /// Seconds since it booted
    Up(u64),
    Offline,
}

/// The node as Kubernetes has it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum K8sNode {
    /// Kubernetes answered, without this node
    Missing,
    Present {
        /// The Ready condition; `None` when not reported
        ready: Option<bool>,
        cordoned: bool,
    },
}

/// What the Operations view knows about a node now; `None` is unknown (not
/// read yet, or the source failed), which never blocks an operation
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeFacts {
    pub talos: Option<TalosState>,
    pub k8s: Option<K8sNode>,
    /// Pods a drain would evict (not DaemonSet or mirror pods, not finished)
    pub pods: Option<usize>,
}

/// Why `kind` can't run on a node with `facts`, or `None` when it can.
/// Cordon and drain go through the Kubernetes API, so they work on a node
/// that is down (the usual fix for a dead node); reboot and shutdown need
/// the node's Talos API.
pub fn blocked(kind: OpKind, facts: &NodeFacts) -> Option<&'static str> {
    let missing = matches!(facts.k8s, Some(K8sNode::Missing));
    let cordoned = match facts.k8s {
        Some(K8sNode::Present { cordoned, .. }) => Some(cordoned),
        _ => None,
    };
    match kind {
        OpKind::Cordon | OpKind::Uncordon | OpKind::Drain if missing => {
            Some("not a Kubernetes node")
        }
        OpKind::Cordon if cordoned == Some(true) => Some("already cordoned"),
        OpKind::Uncordon if cordoned == Some(false) => Some("not cordoned"),
        OpKind::Reboot | OpKind::Shutdown if facts.talos == Some(TalosState::Offline) => {
            Some("offline: needs the Talos API")
        }
        _ => None,
    }
}

/// The facts of `nodes`, and why Kubernetes couldn't be read
#[derive(Debug, Default)]
pub struct NodeFactsResult {
    pub facts: HashMap<String, NodeFacts>,
    pub k8s_error: Option<String>,
}

/// How long a node may take to answer the Talos probe
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Read every node's facts: a Talos probe per node (in parallel, bounded),
/// one node list and one pod list from Kubernetes
pub async fn fetch_node_facts(
    talos: TalosClient,
    cp_ip: Option<String>,
    nodes: Vec<OpNode>,
) -> NodeFactsResult {
    let probes = nodes.iter().map(|n| {
        let client = talos.with_node(&n.address);
        async move {
            match tokio::time::timeout(PROBE_TIMEOUT, client.system_stat()).await {
                Ok(Ok(stats)) => {
                    let now = Utc::now().timestamp().max(0).unsigned_abs();
                    let boot = stats.first().map_or(now, |s| s.boot_time);
                    TalosState::Up(now.saturating_sub(boot))
                }
                _ => TalosState::Offline,
            }
        }
    });
    let talos_states = futures::future::join_all(probes).await;
    let mut result = NodeFactsResult::default();
    for (n, state) in nodes.iter().zip(talos_states) {
        result.facts.insert(
            n.hostname.clone(),
            NodeFacts {
                talos: Some(state),
                ..NodeFacts::default()
            },
        );
    }
    let k8s = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        crate::components::diagnostics::k8s::connect(&talos, cp_ip.as_deref()),
    )
    .await
    {
        Ok(Ok((k8s, _))) => k8s,
        Ok(Err(e)) => {
            result.k8s_error = Some(e);
            return result;
        }
        Err(_) => {
            result.k8s_error = Some("timed out connecting to Kubernetes".to_string());
            return result;
        }
    };
    use k8s_openapi::api::core::v1::{Node, Pod};
    use kube::api::{Api, ListParams};
    let listed = tokio::time::timeout(
        REQUEST_TIMEOUT,
        Api::<Node>::all(k8s.clone()).list(&ListParams::default()),
    )
    .await;
    match listed {
        Ok(Ok(list)) => {
            for n in &nodes {
                let node = list
                    .items
                    .iter()
                    .find(|k| k.metadata.name.as_deref() == Some(n.hostname.as_str()));
                if let Some(f) = result.facts.get_mut(&n.hostname) {
                    f.k8s = Some(node.map_or(K8sNode::Missing, k8s_node));
                }
            }
        }
        Ok(Err(e)) => result.k8s_error = Some(format!("listing nodes: {e}")),
        Err(_) => result.k8s_error = Some("listing nodes timed out".to_string()),
    }
    if let Ok(Ok(pods)) = tokio::time::timeout(
        REQUEST_TIMEOUT,
        Api::<Pod>::all(k8s).list(&ListParams::default()),
    )
    .await
    {
        for f in result.facts.values_mut() {
            f.pods = Some(0);
        }
        for pod in pods.items.iter().filter(|p| counts_for_drain(p)) {
            let node = pod.spec.as_ref().and_then(|s| s.node_name.as_deref());
            if let Some(f) = node.and_then(|n| result.facts.get_mut(n))
                && let Some(count) = &mut f.pods
            {
                *count += 1;
            }
        }
    }
    result
}

fn k8s_node(node: &k8s_openapi::api::core::v1::Node) -> K8sNode {
    let ready = node
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .and_then(|c| c.iter().find(|c| c.type_ == "Ready"))
        .and_then(|c| match c.status.as_str() {
            "True" => Some(true),
            "False" => Some(false),
            _ => None,
        });
    let cordoned = node
        .spec
        .as_ref()
        .and_then(|s| s.unschedulable)
        .unwrap_or(false);
    K8sNode::Present { ready, cordoned }
}

/// A pod a drain would evict: not a mirror or DaemonSet pod, not finished
fn counts_for_drain(pod: &k8s_openapi::api::core::v1::Pod) -> bool {
    let mirror = pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"));
    let daemonset = pod
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|refs| refs.iter().any(|r| r.kind == "DaemonSet"));
    let finished = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .is_some_and(|p| p == "Succeeded" || p == "Failed");
    !mirror && !daemonset && !finished
}

/// Whether the node is cordoned now; `None` when it can't be read
async fn is_cordoned(k8s: &Client, hostname: &str) -> Option<bool> {
    use k8s_openapi::api::core::v1::Node;
    let api: kube::api::Api<Node> = kube::api::Api::all(k8s.clone());
    let node = tokio::time::timeout(REQUEST_TIMEOUT, api.get(hostname))
        .await
        .ok()?
        .ok()?;
    Some(node.spec.and_then(|s| s.unschedulable).unwrap_or(false))
}

/// An operation to start
#[derive(Debug, Clone)]
pub struct OpRequest {
    pub context: String,
    /// The tree row: a node, `<cluster> control plane`, ...
    pub target: String,
    pub kind: OpKind,
    /// In the order they run
    pub nodes: Vec<OpNode>,
    pub options: DrainOptions,
    /// Between nodes of a reboot over several nodes
    pub delay: Duration,
}

/// The commands a user would type for `kind` on `node`, as recorded and
/// shown in the dialog
pub fn commands_for(
    context: &str,
    kind: OpKind,
    node: &OpNode,
    options: &DrainOptions,
) -> Vec<String> {
    let h = &node.hostname;
    let mut drain = drain_command(h, options);
    if let Some(secs) = options.wait_to_finish_secs {
        // No kubectl flag waits for pods to end: say what talos9s does
        drain = format!(
            "{drain}  # after waiting up to {} for pods without a controller{} to finish",
            crate::components::drain_options::secs(secs),
            if options.wait_for_jobs {
                " and Job pods"
            } else {
                ""
            }
        );
    }
    let commands = match kind {
        OpKind::Cordon => vec![format!("kubectl cordon {h}")],
        OpKind::Uncordon => vec![format!("kubectl uncordon {h}")],
        OpKind::Drain => vec![drain],
        OpKind::Reboot => {
            let mut c = vec![drain, format!("talosctl -n {} reboot", node.address)];
            if options.uncordon_after_reboot {
                c.push(format!("kubectl uncordon {h}"));
            }
            c
        }
        OpKind::Shutdown => vec![drain, format!("talosctl -n {} shutdown", node.address)],
    };
    commands.iter().map(|c| with_context(context, c)).collect()
}

/// `kubectl cordon bc1` as `kubectl --context bc cordon bc1`: the same
/// command typed in another cluster's context would change the wrong node
fn with_context(context: &str, command: &str) -> String {
    match command.split_once(' ') {
        Some((tool, rest)) if !context.is_empty() => format!("{tool} --context {context} {rest}"),
        _ => command.to_string(),
    }
}

/// `kubectl drain` with the flags `options` stand for
fn drain_command(h: &str, options: &DrainOptions) -> String {
    let mut drain = format!("kubectl drain {h}");
    if options.ignore_daemonsets {
        drain.push_str(" --ignore-daemonsets");
    }
    if options.delete_emptydir_data {
        drain.push_str(" --delete-emptydir-data");
    }
    if options.force {
        drain.push_str(" --force");
    }
    if let Some(grace) = options.grace_period_secs {
        let _ = write!(drain, " --grace-period={grace}");
    }
    // kubectl waits for ever without one; ours always has a limit unless None
    if let Some(secs) = options.timeout_secs {
        let _ = write!(
            drain,
            " --timeout={}",
            crate::components::drain_options::secs(secs)
        );
    }
    drain
}

/// A step's shell equivalent in the run's output (`$ kubectl cordon bc1`):
/// talos9s calls the APIs itself, the line says what that amounts to
fn shell(rec: &Recorder, command: &str) {
    rec.line(&format!("$ {}", with_context(&rec.context, command)));
}

/// How a run is going, shared between its task and the app
#[derive(Debug, Clone, Default)]
pub struct Progress {
    /// Nodes finished
    pub done: usize,
    pub total: usize,
    /// The node being worked on
    pub node: Option<String>,
    /// The last line written
    pub step: String,
    pub end: Option<RunEnd>,
}

/// A run going now (or just finished, until the app drops it)
pub struct ActiveRun {
    pub header: RunHeader,
    pub path: PathBuf,
    pub progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl ActiveRun {
    pub fn progress(&self) -> Progress {
        self.progress.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Stop after the current step
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelling(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// The runs going now, by context: at most one each
pub type Runs = Arc<Mutex<HashMap<String, ActiveRun>>>;

/// Drop finished runs; the files keep them
pub fn prune(runs: &Runs) {
    if let Ok(mut runs) = runs.lock() {
        runs.retain(|_, r| !r.finished());
    }
}

/// The paths of runs going now
pub fn running_paths(runs: &Runs) -> Vec<PathBuf> {
    runs.lock()
        .map(|r| {
            r.values()
                .filter(|r| !r.finished())
                .map(|r| r.path.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// One line for the header about the runs going now: `▶ Reboot bc 2/4 ·
/// bc2: waiting for Ready`; none when nothing runs
pub fn header_status(runs: &Runs) -> Option<String> {
    let runs = runs.lock().ok()?;
    let mut parts: Vec<String> = runs
        .values()
        .filter(|r| !r.finished())
        .map(|r| {
            let p = r.progress();
            let mut s = format!("▶ {} {}", r.header.operation, r.header.context);
            if p.total > 1 {
                let _ = write!(s, " {}/{}", (p.done + 1).min(p.total), p.total);
            }
            if r.cancelling() {
                s.push_str(" · cancelling");
            } else if !p.step.is_empty() {
                // The header is one line: a long step (a pod name) is cut
                let step: String = p.step.chars().take(60).collect();
                s.push_str(" · ");
                s.push_str(&step);
            }
            s
        })
        .collect();
    parts.sort();
    (!parts.is_empty()).then(|| parts.join("  "))
}

/// The contexts with a run going
pub fn running_contexts(runs: &Runs) -> Vec<String> {
    let Ok(runs) = runs.lock() else {
        return Vec::new();
    };
    let mut c: Vec<String> = runs
        .iter()
        .filter(|(_, r)| !r.finished())
        .map(|(c, _)| c.clone())
        .collect();
    c.sort();
    c
}

/// `~/.talos9s/operations`
pub fn runs_dir() -> Option<PathBuf> {
    Some(dirs_next::home_dir()?.join(".talos9s").join("operations"))
}

/// Every run file in `dir`, unordered; unreadable files are skipped
pub fn load_runs(dir: &Path) -> Vec<(PathBuf, Run)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some((p, parse_run(&text)?))
        })
        .collect()
}

/// Delete run files; how many went
pub fn delete_runs(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

/// The output of a run file as `TIME text` lines, with its end as the last
/// line, for the log pane
pub fn run_output(text: &str) -> String {
    let Some(run) = parse_run(text) else {
        return String::new();
    };
    let mut out: Vec<String> = run
        .lines
        .iter()
        .map(|(t, l)| format_line(*t, l).trim_end().to_string())
        .collect();
    if let Some(end) = &run.end {
        let message = end.message.as_deref().unwrap_or("");
        out.push(
            format_line(end.finished, &format!("{}: {message}", end.status.name()))
                .trim_end()
                .to_string(),
        );
    }
    out.join("\n")
}

/// Writes a run's file and keeps its progress. Lines are masked like the
/// machine config before they are written, and a line repeating the last
/// one is skipped (wait loops report the same thing every few seconds).
#[derive(Clone)]
struct Recorder {
    /// The cluster's talosconfig context, named in the commands written
    context: String,
    file: Arc<Mutex<Option<std::fs::File>>>,
    progress: Arc<Mutex<Progress>>,
}

impl Recorder {
    fn line(&self, text: &str) {
        let text = mask(text);
        if let Ok(mut p) = self.progress.lock() {
            if p.step == text {
                return;
            }
            p.step = text.clone();
        }
        if let Ok(mut f) = self.file.lock()
            && let Some(f) = f.as_mut()
        {
            let _ = f.write_all(format_line(Utc::now(), &text).as_bytes());
            let _ = f.flush();
        }
    }

    fn set(&self, update: impl FnOnce(&mut Progress)) {
        if let Ok(mut p) = self.progress.lock() {
            update(&mut p);
        }
    }

    fn finish(&self, status: RunStatus, message: &str) {
        let end = RunEnd {
            finished: Utc::now(),
            status,
            message: Some(mask(message)),
        };
        if let Ok(mut f) = self.file.lock()
            && let Some(f) = f.as_mut()
        {
            let _ = f.write_all(format_end(&end).as_bytes());
            let _ = f.flush();
        }
        self.set(|p| p.end = Some(end));
    }
}

/// A line as the machine config viewer would show it: secret values hidden
fn mask(text: &str) -> String {
    crate::components::node_info::mask_config(text)
        .0
        .into_iter()
        .map(|l| l.text)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Why a run can't start in `context` now, if it can't
pub fn busy(runs: &Runs, context: &str) -> Option<String> {
    let runs = runs.lock().ok()?;
    let run = runs.get(context).filter(|r| !r.finished())?;
    Some(format!(
        "{} on {} is running; one operation at a time per cluster",
        run.header.operation, run.header.target
    ))
}

/// Start `request` in the background: write the header, run, write the
/// end. Fails when a run is going in the same context or the file can't
/// be written.
pub fn start(
    runs: &Runs,
    dir: &Path,
    request: OpRequest,
    talos: TalosClient,
    cp_ip: Option<String>,
) -> Result<PathBuf, String> {
    if let Some(why) = busy(runs, &request.context) {
        return Err(why);
    }
    let header = RunHeader {
        context: request.context.clone(),
        target: request.target.clone(),
        nodes: request.nodes.iter().map(|n| n.hostname.clone()).collect(),
        operation: request.kind.name().to_string(),
        commands: request
            .nodes
            .iter()
            .flat_map(|n| commands_for(&request.context, request.kind, n, &request.options))
            .collect(),
        started: Utc::now(),
    };
    let path = dir.join(file_name(&header));
    let file = create_private(dir, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut file = file;
    file.write_all(format_header(&header).as_bytes())
        .map_err(|e| e.to_string())?;
    let progress = Arc::new(Mutex::new(Progress {
        total: request.nodes.len(),
        ..Progress::default()
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    let recorder = Recorder {
        context: request.context.clone(),
        file: Arc::new(Mutex::new(Some(file))),
        progress: Arc::clone(&progress),
    };
    let task = tokio::spawn(run(
        request.clone(),
        talos,
        cp_ip,
        recorder,
        Arc::clone(&cancel),
    ));
    let active = ActiveRun {
        header,
        path: path.clone(),
        progress,
        cancel,
        task,
    };
    if let Ok(mut runs) = runs.lock() {
        runs.insert(request.context, active);
    }
    Ok(path)
}

/// A new file only the user can read, in a directory only the user can open
fn create_private(dir: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        options.mode(0o600);
    }
    options.open(path)
}

/// The run: connect, then each node in turn; the first failure stops it
async fn run(
    request: OpRequest,
    talos: TalosClient,
    cp_ip: Option<String>,
    rec: Recorder,
    cancel: Arc<AtomicBool>,
) {
    let tag = request.kind.name().to_uppercase();
    let total = request.nodes.len();
    audit_start(
        &tag,
        &request.target,
        &format!("{} node(s): {}", total, nodes_list(&request.nodes)),
    );
    rec.line(&format!(
        "{} on {} ({} node(s)), context {}",
        request.kind.name(),
        request.target,
        total,
        request.context
    ));
    rec.line("connecting to Kubernetes");
    let k8s = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        crate::components::diagnostics::k8s::connect(&talos, cp_ip.as_deref()),
    )
    .await
    {
        Ok(Ok((k8s, _))) => k8s,
        Ok(Err(e)) => {
            return fail(
                &rec,
                &tag,
                &request.target,
                &format!("no Kubernetes client: {e}"),
            );
        }
        Err(_) => {
            return fail(
                &rec,
                &tag,
                &request.target,
                &format!(
                    "no Kubernetes client: timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                ),
            );
        }
    };
    for (i, node) in request.nodes.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return cancelled(&rec, &tag, &request.target, i, total);
        }
        rec.set(|p| p.node = Some(node.hostname.clone()));
        if total > 1 {
            rec.line(&format!("── {} ({}/{})", node.hostname, i + 1, total));
        }
        match run_node(&request, node, &k8s, &talos, &rec, &cancel).await {
            Ok(Step::Done(msg)) => {
                rec.line(&format!("{}: {msg}", node.hostname));
                audit_success(&tag, &node.hostname, &msg);
                rec.set(|p| p.done = i + 1);
            }
            Ok(Step::Cancelled(msg)) => {
                rec.line(&format!("{}: {msg}", node.hostname));
                return cancelled(&rec, &tag, &request.target, i, total);
            }
            Err(e) => {
                let msg = format!("{}: {e}", node.hostname);
                audit_failure(&tag, &node.hostname, &e);
                let msg = if total > 1 {
                    format!("{msg} (stopped; {i} of {total} done)")
                } else {
                    msg
                };
                rec.finish(RunStatus::Failed, &msg);
                return;
            }
        }
        let more = i + 1 < total;
        if more && request.kind == OpKind::Reboot && !request.delay.is_zero() {
            rec.line(&format!(
                "waiting {}s before the next node",
                request.delay.as_secs()
            ));
            if sleep_or_cancel(request.delay, &cancel).await {
                return cancelled(&rec, &tag, &request.target, i + 1, total);
            }
        }
    }
    let msg = if total > 1 {
        format!("{total} nodes done")
    } else {
        "done".to_string()
    };
    audit_success(&tag, &request.target, &msg);
    rec.finish(RunStatus::Ok, &msg);
}

fn nodes_list(nodes: &[OpNode]) -> String {
    nodes
        .iter()
        .map(|n| n.hostname.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn fail(rec: &Recorder, tag: &str, target: &str, message: &str) {
    audit_failure(tag, target, message);
    rec.finish(RunStatus::Failed, message);
}

fn cancelled(rec: &Recorder, tag: &str, target: &str, done: usize, total: usize) {
    let msg = format!("cancelled; {done} of {total} done");
    audit_failure(tag, target, &msg);
    rec.finish(RunStatus::Cancelled, &msg);
}

/// Sleep, waking early on cancel; true if cancelled
/// How often the wait before draining looks at the node's pods again
const WAIT_POLL: Duration = Duration::from_secs(10);

/// Wait up to `secs` for the pods with no controller (and Job pods with
/// `jobs`) on the cordoned node to finish by themselves; true when
/// cancelled. Whatever is left is the drain's to decide
async fn wait_to_finish(
    k8s: &Client,
    h: &str,
    jobs: bool,
    secs: u64,
    rec: &Recorder,
    cancel: &AtomicBool,
) -> bool {
    use crate::components::drain_options::secs as fmt;
    let limit = Duration::from_secs(secs);
    let start = std::time::Instant::now();
    rec.line(&format!(
        "# wait up to {} for pods without a controller{} to finish",
        fmt(secs),
        if jobs { " and Job pods" } else { "" }
    ));
    let mut last = Vec::new();
    loop {
        match pods_to_wait_for(k8s, h, jobs).await {
            Ok(pods) if pods.is_empty() => {
                rec.line("nothing left to wait for");
                return false;
            }
            Ok(pods) => {
                if pods != last {
                    rec.line(&format!(
                        "waiting for {} ({} of {})",
                        pods.join(", "),
                        fmt(start.elapsed().as_secs()),
                        fmt(secs)
                    ));
                    last = pods;
                }
            }
            // Unknown isn't "done": keep waiting, the drain checks again
            Err(e) => rec.line(&format!("could not list pods: {e}")),
        }
        let left = limit.saturating_sub(start.elapsed());
        if left.is_zero() {
            rec.line(&format!("waited {}; draining", fmt(secs)));
            return false;
        }
        if sleep_or_cancel(WAIT_POLL.min(left), cancel).await {
            return true;
        }
    }
}

async fn sleep_or_cancel(duration: Duration, cancel: &AtomicBool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < duration {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500).min(duration)).await;
    }
    cancel.load(Ordering::Relaxed)
}

enum Step {
    Done(String),
    Cancelled(String),
}

/// One node: the steps of `request.kind`. A failure after cordoning
/// uncordons what this run cordoned, so a stopped run leaves the node as it
/// found it.
async fn run_node(
    request: &OpRequest,
    node: &OpNode,
    k8s: &Client,
    talos: &TalosClient,
    rec: &Recorder,
    cancel: &AtomicBool,
) -> Result<Step, String> {
    let h = &node.hostname;
    let options = &request.options;
    match request.kind {
        OpKind::Cordon => {
            shell(rec, &format!("kubectl cordon {h}"));
            return set_schedulable(k8s, h, false)
                .await
                .map(|()| Step::Done("cordoned".into()));
        }
        OpKind::Uncordon => {
            shell(rec, &format!("kubectl uncordon {h}"));
            return set_schedulable(k8s, h, true)
                .await
                .map(|()| Step::Done("uncordoned".into()));
        }
        OpKind::Drain | OpKind::Reboot | OpKind::Shutdown => {}
    }

    // Taking a node down must leave etcd its quorum; before each node, so a
    // group waits for the previous one to rejoin
    if request.kind.takes_down() {
        wait_for_etcd_safe(talos, node, rec).await?;
    }

    // A node cordoned before the run stays cordoned after it: a failure or
    // the reboot's end uncordons only what this run cordoned
    // Pods the options don't let go stop the drain before anything changes;
    // with a wait, unmanaged pods may still end by themselves first
    let check = DrainOptions {
        force: options.force || options.wait_to_finish_secs.is_some(),
        ..options.clone()
    };
    match drain_blockers(k8s, h, &check).await {
        Ok(b) if !b.is_empty() => return Err(b.message()),
        Ok(_) => {}
        Err(e) => return Err(format!("drain failed: {e}")),
    }
    let was_cordoned = is_cordoned(k8s, h).await == Some(true);
    if was_cordoned {
        rec.line(&format!("{h} is cordoned already; it stays cordoned"));
    } else {
        shell(rec, &format!("kubectl cordon {h}"));
        set_schedulable(k8s, h, false).await?;
    }
    if let Some(secs) = options.wait_to_finish_secs
        && wait_to_finish(k8s, h, options.wait_for_jobs, secs, rec, cancel).await
    {
        let msg = undo_cordon(
            k8s,
            h,
            rec,
            was_cordoned,
            "cancelled while waiting".to_string(),
        )
        .await;
        return Ok(Step::Cancelled(msg));
    }
    shell(rec, &drain_command(h, options));
    let r = rec.clone();
    let callback: crate::components::diagnostics::k8s::DrainProgressCallback =
        Box::new(move |msg: &str| r.line(msg));
    let drained = drain_node_with_progress(k8s, h, options, Some(callback)).await;
    let drained = match drained {
        Ok(d) if d.success => d,
        Ok(d) => {
            let why = d
                .error
                .clone()
                .unwrap_or_else(|| format!("could not evict {}", d.failed_pods.join(", ")));
            return Err(
                undo_cordon(k8s, h, rec, was_cordoned, format!("drain failed: {why}")).await,
            );
        }
        Err(e) => {
            return Err(undo_cordon(k8s, h, rec, was_cordoned, format!("drain failed: {e}")).await);
        }
    };
    let summary = format!("{} pod(s) evicted", drained.pods_evicted);
    rec.line(&summary);
    if request.kind == OpKind::Drain {
        return Ok(Step::Done(format!("drained, {summary}; stays cordoned")));
    }

    // The last moment to stop before the node goes down
    if cancel.load(Ordering::Relaxed) {
        let msg = undo_cordon(
            k8s,
            h,
            rec,
            was_cordoned,
            "cancelled before going down".to_string(),
        )
        .await;
        return Ok(Step::Cancelled(msg));
    }

    let shutdown = request.kind == OpKind::Shutdown;
    // Read before the request, so the wait can tell the new boot from the old
    let boot_before = if shutdown || !options.wait_for_node_ready {
        None
    } else {
        get_node_boot_id(k8s, h).await
    };
    let client = talos.with_node(&node.address);
    shell(
        rec,
        &format!(
            "talosctl -n {} {}",
            node.address,
            if shutdown { "shutdown" } else { "reboot" }
        ),
    );
    let sent = if shutdown {
        tokio::time::timeout(REQUEST_TIMEOUT, client.shutdown(false)).await
    } else {
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.reboot(talos_rs::RebootMode::Default),
        )
        .await
    };
    let failed = match sent {
        Ok(Ok(r)) if r.success => None,
        Ok(Ok(_)) => Some("the node refused the request".to_string()),
        Ok(Err(e)) => Some(e.to_string()),
        Err(_) => Some(format!("no answer in {}s", REQUEST_TIMEOUT.as_secs())),
    };
    if let Some(why) = failed {
        let what = if shutdown { "shutdown" } else { "reboot" };
        return Err(undo_cordon(k8s, h, rec, was_cordoned, format!("{what} failed: {why}")).await);
    }
    // Talos uncordons on boot only a node it cordoned itself
    if shutdown {
        return Ok(Step::Done(format!(
            "shutdown sent; stays cordoned: after powering it on, kubectl uncordon {h}"
        )));
    }
    if !options.wait_for_node_ready {
        return Ok(Step::Done("reboot sent (not waiting for Ready)".into()));
    }
    shell(
        rec,
        &format!(
            "kubectl wait --for=condition=Ready node/{h} --timeout={}s",
            options.post_reboot_timeout_secs
        ),
    );
    let r = rec.clone();
    let callback: crate::components::diagnostics::k8s::NodeReadyProgressCallback =
        Box::new(move |msg: &str| r.line(msg));
    let ready = wait_for_node_ready(
        k8s,
        h,
        options.post_reboot_timeout_secs,
        boot_before,
        Some(callback),
    )
    .await;
    let took = match ready {
        Ok(r) if r.success => r.time_taken_secs,
        Ok(r) => {
            return Err(format!(
                "didn't come back Ready: {}",
                r.error.unwrap_or_default()
            ));
        }
        Err(e) => return Err(format!("checking whether it is Ready: {e}")),
    };
    if !options.uncordon_after_reboot {
        return Ok(Step::Done(format!("rebooted in {took}s; stays cordoned")));
    }
    if was_cordoned {
        return Ok(Step::Done(format!(
            "rebooted in {took}s; stays cordoned, as before"
        )));
    }
    shell(rec, &format!("kubectl uncordon {h}"));
    match set_schedulable(k8s, h, true).await {
        Ok(()) => Ok(Step::Done(format!("rebooted in {took}s, uncordoned"))),
        Err(e) => Err(format!("rebooted in {took}s, but uncordon failed: {e}")),
    }
}

/// Cordon (`schedulable` false) or uncordon a node
async fn set_schedulable(k8s: &Client, hostname: &str, schedulable: bool) -> Result<(), String> {
    let call = async {
        if schedulable {
            uncordon_node(k8s, hostname).await
        } else {
            cordon_node(k8s, hostname).await
        }
    };
    match tokio::time::timeout(REQUEST_TIMEOUT, call).await {
        Ok(Ok(r)) if r.success => Ok(()),
        Ok(Ok(r)) => Err(r.error.unwrap_or_else(|| "refused".to_string())),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("no answer in {}s", REQUEST_TIMEOUT.as_secs())),
    }
}

/// Uncordon after a failure; the failure's message, saying whether the
/// node was uncordoned
async fn undo_cordon(
    k8s: &Client,
    hostname: &str,
    rec: &Recorder,
    was_cordoned: bool,
    why: String,
) -> String {
    if was_cordoned {
        rec.line(&format!("{why}; {hostname} stays cordoned, as before"));
        return format!("{why} (stays cordoned, as before)");
    }
    rec.line(&why);
    shell(rec, &format!("kubectl uncordon {hostname}"));
    match set_schedulable(k8s, hostname, true).await {
        Ok(()) => format!("{why} (uncordoned again)"),
        Err(e) => format!("{why}; uncordon failed too: {e}"),
    }
}

/// Wait until taking `node` down keeps etcd quorum. A control plane node
/// needs a known Safe verdict; a worker only stops on a known Unsafe one (it
/// is checked at all in case its role was mislabeled).
async fn wait_for_etcd_safe(
    talos: &TalosClient,
    node: &OpNode,
    rec: &Recorder,
) -> Result<(), String> {
    shell(rec, &format!("talosctl -n {} etcd status", node.address));
    let start = std::time::Instant::now();
    loop {
        let safety = fetch_etcd_info(talos, &node.address)
            .await
            .map(|i| i.safety);
        let reason = match safety {
            Some(SafetyStatus::Safe) => {
                if node.is_controlplane {
                    rec.line("etcd keeps its quorum without this node");
                }
                return Ok(());
            }
            None | Some(_) if !node.is_controlplane => return Ok(()),
            Some(SafetyStatus::Unsafe(msg)) => msg,
            _ => "etcd member health unknown".to_string(),
        };
        if start.elapsed() >= ETCD_SAFE_WAIT {
            return Err(format!("not taken down: {reason}"));
        }
        rec.line(&format!("waiting for etcd: {reason}"));
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// etcd quorum as it would be without one node
#[derive(Debug, Clone, Default)]
pub struct NodeEtcdInfo {
    /// Whether this node is a voting etcd member
    pub is_member: bool,
    /// Voting etcd members
    pub total_members: usize,
    /// Voters that answered the status call; `None` when unknown
    pub healthy_members: Option<usize>,
    /// Whether taking this node down keeps quorum
    pub safety: SafetyStatus,
}

/// etcd quorum without the node at `address`; none when etcd can't be read
pub async fn fetch_etcd_info(client: &TalosClient, address: &str) -> Option<NodeEtcdInfo> {
    let members = match tokio::time::timeout(SAFETY_CHECK_TIMEOUT, client.etcd_members()).await {
        Ok(Ok(members)) => members,
        Ok(Err(e)) => {
            tracing::warn!("Failed to fetch etcd members: {}", e);
            return None;
        }
        Err(_) => {
            tracing::warn!("Timed out fetching etcd members");
            return None;
        }
    };
    // Status calls by IP: hostnames may not resolve
    let cp_ips: Vec<String> = members
        .iter()
        .filter_map(talos_rs::EtcdMemberInfo::ip_address)
        .collect();
    // Per-node failures are left out of the reply; a failed call means unknown
    let statuses =
        match tokio::time::timeout(SAFETY_CHECK_TIMEOUT, client.etcd_status_for_nodes(&cp_ips))
            .await
        {
            Ok(Ok(statuses)) => Some(statuses),
            _ => None,
        };
    let quorum = EtcdQuorum::assess(&members, statuses.as_deref());
    Some(NodeEtcdInfo {
        is_member: EtcdQuorum::is_voter(&members, address),
        total_members: quorum.voters,
        healthy_members: quorum.healthy,
        safety: quorum.removal_safety(&members, address),
    })
}

/// What the confirmation dialog shows before a drain, reboot or shutdown
#[derive(Debug, Clone, Default)]
pub struct SafetyInfo {
    /// Per control plane node about to go down: etcd without it
    pub etcd: Vec<(String, Option<NodeEtcdInfo>)>,
    /// PDBs that would block a drain; `None` when unknown
    pub pdb: Option<PdbHealthInfo>,
    /// The pods the drain options decide about, on all the nodes; `None`
    /// when they couldn't be read
    pub drain: Option<Vec<PodImpact>>,
    /// Why there is no Kubernetes client, if there isn't
    pub k8s_error: Option<String>,
}

/// `drain_preview` over every node; one node unread makes it unknown
async fn drain_preview_all(k8s: &kube::Client, nodes: &[OpNode]) -> Option<Vec<PodImpact>> {
    let mut all = Vec::new();
    for node in nodes {
        let one = tokio::time::timeout(SAFETY_CHECK_TIMEOUT, drain_preview(k8s, &node.hostname))
            .await
            .ok()?
            .ok()?;
        all.extend(one);
    }
    Some(all)
}

/// Read the safety checks for `kind` on `nodes`, bounded
pub async fn fetch_safety(
    talos: TalosClient,
    cp_ip: Option<String>,
    kind: OpKind,
    nodes: Vec<OpNode>,
) -> SafetyInfo {
    let mut info = SafetyInfo::default();
    if kind.takes_down() {
        for node in nodes.iter().filter(|n| n.is_controlplane) {
            let etcd = fetch_etcd_info(&talos, &node.address).await;
            info.etcd.push((node.hostname.clone(), etcd));
        }
    }
    if kind.drains() {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            crate::components::diagnostics::k8s::connect(&talos, cp_ip.as_deref()),
        )
        .await
        {
            Ok(Ok((k8s, _))) => {
                info.pdb = tokio::time::timeout(SAFETY_CHECK_TIMEOUT, check_pdb_health(&k8s))
                    .await
                    .ok()
                    .and_then(Result::ok);
                info.drain = drain_preview_all(&k8s, &nodes).await;
                if let (Some(pods), Some(pdbs)) = (&mut info.drain, &info.pdb) {
                    crate::components::diagnostics::k8s::with_pdbs(pods, pdbs);
                }
            }
            Ok(Err(e)) => info.k8s_error = Some(e),
            Err(_) => info.k8s_error = Some("timed out connecting".to_string()),
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cordon only an uncordoned node, uncordon only a cordoned one;
    /// reboot needs Talos, cordon and drain don't; unknown blocks nothing
    #[test]
    fn what_a_node_allows() {
        let facts = |talos, k8s| NodeFacts {
            talos,
            k8s,
            pods: None,
        };
        let cordoned = Some(K8sNode::Present {
            ready: Some(true),
            cordoned: true,
        });
        let open = Some(K8sNode::Present {
            ready: Some(true),
            cordoned: false,
        });
        let up = Some(TalosState::Up(60));
        let down = Some(TalosState::Offline);
        assert_eq!(
            blocked(OpKind::Cordon, &facts(up, cordoned)),
            Some("already cordoned")
        );
        assert_eq!(blocked(OpKind::Uncordon, &facts(up, cordoned)), None);
        assert_eq!(
            blocked(OpKind::Uncordon, &facts(up, open)),
            Some("not cordoned")
        );
        assert_eq!(blocked(OpKind::Cordon, &facts(down, open)), None);
        assert_eq!(blocked(OpKind::Drain, &facts(down, cordoned)), None);
        assert!(blocked(OpKind::Reboot, &facts(down, open)).is_some());
        assert!(blocked(OpKind::Shutdown, &facts(down, open)).is_some());
        assert!(blocked(OpKind::Drain, &facts(up, Some(K8sNode::Missing))).is_some());
        for kind in OpKind::ALL {
            assert_eq!(blocked(kind, &NodeFacts::default()), None, "{kind:?}");
        }
    }

    fn node(h: &str, cp: bool) -> OpNode {
        OpNode {
            hostname: h.into(),
            address: format!("10.0.0.{}", h.len()),
            is_controlplane: cp,
        }
    }

    /// The recorded commands follow the drain options
    #[test]
    fn commands_follow_options() {
        let n = node("bc2", true);
        let options = DrainOptions::default();
        assert_eq!(
            commands_for("bc", OpKind::Reboot, &n, &options),
            [
                "kubectl --context bc drain bc2 --ignore-daemonsets --delete-emptydir-data --timeout=5m",
                "talosctl --context bc -n 10.0.0.3 reboot",
                "kubectl --context bc uncordon bc2",
            ]
        );
        assert_eq!(
            commands_for("bc", OpKind::Cordon, &n, &options),
            ["kubectl --context bc cordon bc2"]
        );
        let options = DrainOptions {
            ignore_daemonsets: false,
            delete_emptydir_data: false,
            force: true,
            grace_period_secs: Some(5),
            timeout_secs: Some(90),
            ..DrainOptions::default()
        };
        assert_eq!(
            commands_for("bc", OpKind::Shutdown, &n, &options),
            [
                "kubectl --context bc drain bc2 --force --grace-period=5 --timeout=90s",
                "talosctl --context bc -n 10.0.0.3 shutdown"
            ]
        );
    }

    /// Shutdown is offered for one node only
    #[test]
    fn group_operations() {
        assert!(OpKind::offered(false).contains(&OpKind::Shutdown));
        assert!(!OpKind::offered(true).contains(&OpKind::Shutdown));
        assert_eq!(OpKind::offered(true).len(), 4);
    }

    /// One run per context; the header says what runs; a file is private
    /// and holds the header
    #[tokio::test]
    async fn one_run_per_context() {
        let dir = std::env::temp_dir().join(format!("talos9s-ops-{}", std::process::id()));
        let runs: Runs = Arc::default();
        let header = RunHeader {
            context: "bc".into(),
            target: "bc2".into(),
            nodes: vec!["bc2".into()],
            operation: "Reboot".into(),
            commands: Vec::new(),
            started: Utc::now(),
        };
        let progress = Arc::new(Mutex::new(Progress {
            total: 4,
            done: 1,
            step: "waiting for Ready".into(),
            ..Progress::default()
        }));
        let task = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
        runs.lock().unwrap().insert(
            "bc".into(),
            ActiveRun {
                header,
                path: dir.join("x.log"),
                progress,
                cancel: Arc::default(),
                task,
            },
        );
        assert!(busy(&runs, "bc").unwrap().contains("Reboot on bc2"));
        assert!(busy(&runs, "kprod").is_none());
        assert_eq!(
            header_status(&runs).unwrap(),
            "▶ Reboot bc 2/4 · waiting for Ready"
        );
        assert_eq!(running_contexts(&runs), ["bc"]);
        runs.lock().unwrap().get("bc").unwrap().cancel();
        assert!(header_status(&runs).unwrap().ends_with("cancelling"));

        let path = dir.join("run.log");
        let mut f = create_private(&dir, &path).unwrap();
        f.write_all(b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(create_private(&dir, &path).is_err(), "never overwrites");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The pane shows the output and the end as lines with times
    #[test]
    fn output_for_the_pane() {
        let t = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&Utc)
        };
        let header = RunHeader {
            context: "bc".into(),
            target: "bc2".into(),
            nodes: vec!["bc2".into()],
            operation: "Drain".into(),
            commands: Vec::new(),
            started: t("2026-11-03T10:00:00Z"),
        };
        let text = format_header(&header)
            + &format_line(t("2026-11-03T10:00:01Z"), "cordon bc2")
            + &format_end(&RunEnd {
                finished: t("2026-11-03T10:00:09Z"),
                status: RunStatus::Ok,
                message: Some("done".into()),
            });
        assert_eq!(
            run_output(&text),
            "2026-11-03T10:00:01.000Z cordon bc2\n2026-11-03T10:00:09.000Z ok: done"
        );
    }
}
