//! Kubernetes objects across clusters, read-only: a status summary for any
//! kind, Flux details, search matching, the YAML diff of one object in two
//! clusters, and the redaction applied before an object is kept
//!
//! Objects are JSON values (`DynamicObject`s), so one set of functions
//! covers built-in kinds and CRDs. A kind without known status fields gets
//! no verdict ([`ObjectHealth::Unknown`]), never a guessed one.

use crate::indicators::{HasHealth, HealthIndicator};
use crate::workloads::{WorkloadHealth, classify_workload};
use serde_json::Value;

/// What an object's status says
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectHealth {
    /// Not working: Ready False, a pod crashing, nothing ready
    Failed,
    /// Partly working (some replicas ready)
    Degraded,
    /// On its way: reconciling, scheduling, rolling out
    Progressing,
    /// `spec.suspend: true`: nothing reconciles it
    Suspended,
    /// The status says it's fine
    Ready,
    /// No status this summary knows how to read
    Unknown,
}

impl HasHealth for ObjectHealth {
    fn health(&self) -> HealthIndicator {
        match self {
            ObjectHealth::Failed => HealthIndicator::Error,
            ObjectHealth::Degraded | ObjectHealth::Suspended => HealthIndicator::Warning,
            ObjectHealth::Progressing => HealthIndicator::Pending,
            ObjectHealth::Ready => HealthIndicator::Healthy,
            ObjectHealth::Unknown => HealthIndicator::Unknown,
        }
    }
}

impl ObjectHealth {
    /// A problem to list: failing, degraded, suspended or still progressing
    pub fn is_problem(self) -> bool {
        !matches!(self, ObjectHealth::Ready | ObjectHealth::Unknown)
    }
}

/// An object's status in a word or two, and why
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub health: ObjectHealth,
    /// `2/3`, `CrashLoopBackOff`, `Ready`, `ReconciliationFailed`
    pub text: String,
    /// The condition's message, when it says more
    pub message: Option<String>,
}

impl Status {
    fn new(health: ObjectHealth, text: impl Into<String>) -> Self {
        Self {
            health,
            text: text.into(),
            message: None,
        }
    }

    fn with_message(mut self, message: Option<&str>) -> Self {
        self.message = message.filter(|m| !m.is_empty()).map(str::to_string);
        self
    }
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    at(v, path)?.as_str()
}

fn int_at(v: &Value, path: &[&str]) -> Option<i64> {
    at(v, path)?.as_i64()
}

fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, key| v.get(key))
}

/// The items of the array at `path`; none when missing or not an array
pub fn array_at<'a>(v: &'a Value, path: &[&str]) -> impl Iterator<Item = &'a Value> + use<'a> {
    at(v, path).and_then(Value::as_array).into_iter().flatten()
}

fn to_i32(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX })
}

/// Seconds since the RFC 3339 time at `path`, `None` when missing or invalid
pub fn age_secs(v: &Value, path: &[&str], now: i64) -> Option<i64> {
    let t = chrono::DateTime::parse_from_rfc3339(str_at(v, path)?).ok()?;
    Some((now - t.timestamp()).max(0))
}

/// Waiting reasons that mean a container won't start by itself
const STUCK_REASONS: &[&str] = &[
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "CreateContainerConfigError",
    "CreateContainerError",
    "InvalidImageName",
    "RunContainerError",
];

/// Condition reasons that mean "still working on it", not "failed"
const PROGRESSING_REASONS: &[&str] = &[
    "Progressing",
    "Reconciling",
    "ProgressingWithRetry",
    "DependencyNotReady",
    "ArtifactOutdated",
    "Pending",
];

/// The status of `obj`, a `kind` object, at `now` (Unix seconds)
pub fn status(kind: &str, obj: &Value, now: i64) -> Status {
    if at(obj, &["spec", "suspend"]).and_then(Value::as_bool) == Some(true) {
        return Status::new(ObjectHealth::Suspended, "Suspended");
    }
    match kind {
        "Pod" => pod_status(obj, now),
        "Deployment" => replicas_status(
            int_at(obj, &["status", "readyReplicas"]).unwrap_or(0),
            int_at(obj, &["spec", "replicas"]).unwrap_or(1),
            int_at(obj, &["status", "availableReplicas"]).unwrap_or(0),
        ),
        "StatefulSet" => {
            let ready = int_at(obj, &["status", "readyReplicas"]).unwrap_or(0);
            replicas_status(
                ready,
                int_at(obj, &["spec", "replicas"]).unwrap_or(1),
                ready,
            )
        }
        "DaemonSet" => {
            let ready = int_at(obj, &["status", "numberReady"]).unwrap_or(0);
            let desired = int_at(obj, &["status", "desiredNumberScheduled"]).unwrap_or(0);
            replicas_status(ready, desired, ready)
        }
        "Job" => job_status(obj),
        "Application" if at(obj, &["status", "health"]).is_some() => argo_status(obj),
        _ => conditions_status(obj),
    }
}

fn replicas_status(ready: i64, desired: i64, available: i64) -> Status {
    let health = match classify_workload(to_i32(ready), to_i32(desired), to_i32(available)) {
        WorkloadHealth::Healthy => ObjectHealth::Ready,
        WorkloadHealth::Degraded => ObjectHealth::Degraded,
        WorkloadHealth::Pending => ObjectHealth::Progressing,
        WorkloadHealth::Failing => ObjectHealth::Failed,
    };
    Status::new(health, format!("{ready}/{desired}"))
}

fn job_status(obj: &Value) -> Status {
    for c in conditions(obj) {
        let is_true = str_at(c, &["status"]) == Some("True");
        match str_at(c, &["type"]) {
            Some("Complete") if is_true => return Status::new(ObjectHealth::Ready, "Complete"),
            Some("Failed") if is_true => {
                return Status::new(ObjectHealth::Failed, "Failed")
                    .with_message(str_at(c, &["message"]));
            }
            _ => {}
        }
    }
    Status::new(ObjectHealth::Progressing, "Running")
}

/// An Argo CD Application: its health, and out of sync as degraded (the
/// cluster isn't what git says); a failed sync operation fails it
fn argo_status(obj: &Value) -> Status {
    let health = str_at(obj, &["status", "health", "status"]).unwrap_or("Unknown");
    let sync = str_at(obj, &["status", "sync", "status"]).unwrap_or("Unknown");
    let text = format!("{health}/{sync}");
    let operation = at(obj, &["status", "operationState"]);
    let phase = operation.and_then(|o| str_at(o, &["phase"]));
    if matches!(phase, Some("Failed" | "Error")) {
        return Status::new(ObjectHealth::Failed, text)
            .with_message(operation.and_then(|o| str_at(o, &["message"])));
    }
    let message = str_at(obj, &["status", "health", "message"]);
    let state = match health {
        "Healthy" if sync == "Synced" => ObjectHealth::Ready,
        "Healthy" => ObjectHealth::Degraded,
        "Progressing" => ObjectHealth::Progressing,
        "Degraded" | "Missing" => ObjectHealth::Failed,
        "Suspended" => ObjectHealth::Suspended,
        _ => ObjectHealth::Unknown,
    };
    Status::new(state, text).with_message(message)
}

fn conditions(obj: &Value) -> impl Iterator<Item = &Value> {
    array_at(obj, &["status", "conditions"])
}

fn pod_status(obj: &Value, now: i64) -> Status {
    let phase = str_at(obj, &["status", "phase"]).unwrap_or("");
    if phase == "Succeeded" {
        return Status::new(ObjectHealth::Ready, "Completed");
    }
    let statuses = ["initContainerStatuses", "containerStatuses"]
        .iter()
        .flat_map(|k| array_at(obj, &["status", k]));
    for c in statuses {
        if let Some(reason) = str_at(c, &["state", "waiting", "reason"])
            && STUCK_REASONS.contains(&reason)
        {
            return Status::new(ObjectHealth::Failed, reason)
                .with_message(str_at(c, &["state", "waiting", "message"]));
        }
    }
    if phase == "Failed" {
        return Status::new(ObjectHealth::Failed, "Failed")
            .with_message(str_at(obj, &["status", "message"]));
    }
    let containers = at(obj, &["status", "containerStatuses"])
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let ready = containers
        .iter()
        .filter(|c| c.get("ready").and_then(Value::as_bool) == Some(true))
        .count();
    let text = format!("{ready}/{}", containers.len());
    if phase == "Running" && !containers.is_empty() && ready == containers.len() {
        return Status::new(ObjectHealth::Ready, text);
    }
    // Being scheduled or pulled: not a problem yet
    let young = age_secs(obj, &["metadata", "creationTimestamp"], now)
        .is_some_and(|a| a < crate::constants::POD_PENDING_GRACE_SECS);
    let health = if young {
        ObjectHealth::Progressing
    } else if phase == "Pending" || ready == 0 {
        ObjectHealth::Failed
    } else {
        ObjectHealth::Degraded
    };
    let text = if phase == "Pending" {
        "Pending".to_string()
    } else {
        text
    };
    Status::new(health, text)
}

/// The `Ready` condition (Flux, cert-manager, Cluster API, most operators),
/// else `Available`; none of them: no verdict
fn conditions_status(obj: &Value) -> Status {
    let find = |t: &str| conditions(obj).find(|c| str_at(c, &["type"]) == Some(t));
    let Some(c) = find("Ready").or_else(|| find("Available")) else {
        return Status::new(ObjectHealth::Unknown, "");
    };
    let message = str_at(c, &["message"]);
    let reason = str_at(c, &["reason"]).unwrap_or("");
    let reconciling = find("Reconciling").is_some_and(|r| str_at(r, &["status"]) == Some("True"));
    match str_at(c, &["status"]) {
        Some("True") => {
            // Ready for an older spec: the new one is still being applied
            let observed = int_at(c, &["observedGeneration"]);
            let generation = int_at(obj, &["metadata", "generation"]);
            if matches!((observed, generation), (Some(o), Some(g)) if o < g) {
                Status::new(ObjectHealth::Progressing, "Reconciling")
            } else {
                Status::new(ObjectHealth::Ready, str_at(c, &["type"]).unwrap_or("Ready"))
            }
        }
        Some("False") if reconciling || PROGRESSING_REASONS.contains(&reason) => {
            Status::new(ObjectHealth::Progressing, non_empty(reason, "Progressing"))
                .with_message(message)
        }
        Some("False") => {
            Status::new(ObjectHealth::Failed, non_empty(reason, "NotReady")).with_message(message)
        }
        _ => Status::new(ObjectHealth::Progressing, non_empty(reason, "Unknown"))
            .with_message(message),
    }
}

fn non_empty<'a>(s: &'a str, default: &'a str) -> &'a str {
    if s.is_empty() { default } else { s }
}

/// A problem worth listing among a cluster's workloads: the status says so,
/// and a pod isn't one a Job will replace
pub fn is_problem(kind: &str, obj: &Value, status: &Status) -> bool {
    if !status.health.is_problem() {
        return false;
    }
    if kind == "Pod" {
        // Being scheduled, or a Job's pod: the Job retries with new ones
        if status.health == ObjectHealth::Progressing {
            return false;
        }
        let owned_by_job = array_at(obj, &["metadata", "ownerReferences"])
            .any(|o| str_at(o, &["kind"]) == Some("Job"));
        if owned_by_job {
            return false;
        }
    }
    true
}

/// A Flux object's applied revision and source, for the Flux list
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FluxInfo {
    /// `main@sha1:abc123`, a chart version
    pub revision: String,
    /// `GitRepository/flux-system ./clusters/prod`, `cilium@1.18.0 from HelmRepository/cilium`
    pub source: String,
}

/// Revision and source of a Kustomization, HelmRelease, Flux source or
/// Argo CD Application
pub fn flux_info(kind: &str, obj: &Value) -> FluxInfo {
    if kind == "Application" {
        return argo_info(obj);
    }
    let revision = str_at(obj, &["status", "lastAppliedRevision"])
        .or_else(|| {
            at(obj, &["status", "history"])
                .and_then(Value::as_array)
                .and_then(|h| h.first())
                .and_then(|h| str_at(h, &["chartVersion"]))
        })
        .or_else(|| str_at(obj, &["status", "lastAttemptedRevision"]))
        .or_else(|| str_at(obj, &["status", "artifact", "revision"]))
        .unwrap_or("")
        .to_string();
    let source_ref = |path: &[&str]| {
        let r = at(obj, path)?;
        Some(format!(
            "{}/{}",
            str_at(r, &["kind"]).unwrap_or("?"),
            str_at(r, &["name"]).unwrap_or("?")
        ))
    };
    let source = match kind {
        "Kustomization" => {
            let from = source_ref(&["spec", "sourceRef"]).unwrap_or_default();
            match str_at(obj, &["spec", "path"]) {
                Some(path) => format!("{from} {path}"),
                None => from,
            }
        }
        "HelmRelease" => {
            if let Some(chart) = at(obj, &["spec", "chart", "spec"]) {
                let name = str_at(chart, &["chart"]).unwrap_or("?");
                let version =
                    str_at(chart, &["version"]).map_or(String::new(), |v| format!("@{v}"));
                let from = source_ref(&["spec", "chart", "spec", "sourceRef"]).unwrap_or_default();
                format!("{name}{version} from {from}")
            } else {
                source_ref(&["spec", "chartRef"]).unwrap_or_default()
            }
        }
        _ => str_at(obj, &["spec", "url"]).unwrap_or("").to_string(),
    };
    FluxInfo { revision, source }
}

fn argo_info(obj: &Value) -> FluxInfo {
    let revision = str_at(obj, &["status", "sync", "revision"])
        .map(str::to_string)
        .or_else(|| {
            let revs = at(obj, &["status", "sync", "revisions"])?.as_array()?;
            let revs: Vec<&str> = revs.iter().filter_map(Value::as_str).collect();
            (!revs.is_empty()).then(|| revs.join(","))
        })
        .unwrap_or_default();
    let describe = |src: &Value| {
        let repo = str_at(src, &["repoURL"]).unwrap_or("?");
        let what = str_at(src, &["chart"])
            .or_else(|| str_at(src, &["path"]))
            .unwrap_or("");
        let rev = str_at(src, &["targetRevision"]).map_or(String::new(), |r| format!("@{r}"));
        format!("{repo} {what}{rev}").trim().to_string()
    };
    let source = match at(obj, &["spec", "source"]) {
        Some(src) => describe(src),
        None => array_at(obj, &["spec", "sources"])
            .map(describe)
            .collect::<Vec<_>>()
            .join(" + "),
    };
    FluxInfo { revision, source }
}

/// Is `group` one of the GitOps API groups (Flux's `*.toolkit.fluxcd.io`,
/// Argo CD's `argoproj.io`)
pub fn is_gitops_group(group: &str) -> bool {
    is_flux_group(group) || group == "argoproj.io"
}

/// Is `group` one of Flux's API groups (`kustomize.toolkit.fluxcd.io`, ...)
pub fn is_flux_group(group: &str) -> bool {
    group.ends_with(".toolkit.fluxcd.io")
}

/// What `/`-style search looks for across clusters
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// Part of the name, any case
    Name(String),
    /// A label selector, given to the API server (`app=redis,tier!=cache`)
    Label(String),
    /// Part of a container image (pods and workloads)
    Image(String),
}

impl Query {
    /// `l:app=x` or `label:…` a selector, `i:redis` or `image:…` an image,
    /// anything else a name; `None` when empty
    ///
    /// ```
    /// use talos9s_core::kubernetes::Query;
    /// assert_eq!(Query::parse("i:redis"), Some(Query::Image("redis".into())));
    /// assert_eq!(Query::parse("  "), None);
    /// ```
    pub fn parse(text: &str) -> Option<Query> {
        let text = text.trim();
        let rest = |prefixes: &[&str]| {
            prefixes
                .iter()
                .find_map(|p| text.strip_prefix(p))
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
        };
        if let Some(sel) = rest(&["label:", "l:"]) {
            return Some(Query::Label(sel));
        }
        if let Some(image) = rest(&["image:", "i:"]) {
            return Some(Query::Image(image));
        }
        (!text.is_empty()).then(|| Query::Name(text.to_string()))
    }

    /// The query as typed back, for the title
    pub fn describe(&self) -> String {
        match self {
            Query::Name(n) => format!("name ~ {n}"),
            Query::Label(l) => format!("label {l}"),
            Query::Image(i) => format!("image ~ {i}"),
        }
    }

    /// Whether `obj` matches. Labels are matched by the API server; here
    /// they always match
    pub fn matches(&self, obj: &Value) -> bool {
        match self {
            Query::Name(n) => {
                contains_ignore_case(str_at(obj, &["metadata", "name"]).unwrap_or(""), n)
            }
            Query::Label(_) => true,
            Query::Image(i) => images(obj).iter().any(|img| contains_ignore_case(img, i)),
        }
    }
}

/// `needle` in `hay`, ignoring case, char-wise
pub fn contains_ignore_case(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// The container images of a pod, or of a workload's pod template
pub fn images(obj: &Value) -> Vec<String> {
    let spec = at(obj, &["spec", "template", "spec"]).or_else(|| obj.get("spec"));
    let Some(spec) = spec else {
        return Vec::new();
    };
    ["initContainers", "containers"]
        .iter()
        .flat_map(|k| array_at(spec, &[k]))
        .filter_map(|c| str_at(c, &["image"]).map(str::to_string))
        .collect()
}

/// A workload's pod selector as a label selector (`matchLabels` only;
/// `None` when it has `matchExpressions` or none)
pub fn pod_selector(obj: &Value) -> Option<String> {
    let selector = at(obj, &["spec", "selector"])?;
    if selector
        .get("matchExpressions")
        .is_some_and(|e| e.as_array().is_some_and(|a| !a.is_empty()))
    {
        return None;
    }
    let labels = selector.get("matchLabels")?.as_object()?;
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
        .collect();
    parts.sort();
    (!parts.is_empty()).then(|| parts.join(","))
}

/// The annotation `kubectl apply` leaves: the whole object again, a Secret's
/// data included
const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Remove what must never be kept or drawn, right after an object is read:
/// a Secret's values (keys stay, with their size) and the last-applied
/// annotation; `managedFields` too, noise in every view
pub fn redact(kind: &str, obj: &mut Value) {
    if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.remove("managedFields");
        if let Some(ann) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
            ann.remove(LAST_APPLIED);
        }
    }
    hide_secret_values(obj);
    if kind == "Secret"
        && let Some(map) = obj.as_object_mut()
    {
        for field in ["data", "stringData"] {
            if let Some(data) = map.get_mut(field).and_then(Value::as_object_mut) {
                for value in data.values_mut() {
                    let len = value.as_str().map_or(0, str::len);
                    *value = Value::String(format!("<hidden, {len} bytes>"));
                }
            }
        }
    }
}

/// A name that says its value is a secret (`DB_PASSWORD`, `apiToken`)
pub fn is_secret_name(name: &str) -> bool {
    let name = name.to_lowercase();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "apikey",
        "api_key",
        "private",
    ]
    .iter()
    .any(|s| name.contains(s))
}

/// `{name: DB_PASSWORD, value: …}` entries anywhere (container env, Helm
/// `set` lists): the value hidden. Key names alone don't show these
fn hide_secret_values(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let secret = map
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(is_secret_name);
            if secret
                && let Some(value) = map.get_mut("value")
                && value.is_string()
            {
                *value = Value::String("<hidden>".to_string());
            }
            for child in map.values_mut() {
                hide_secret_values(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(hide_secret_values),
        _ => {}
    }
}

/// The object as compared between clusters: without status and what the
/// API server sets per object (uid, resourceVersion, times, generation)
pub fn for_compare(obj: &Value) -> Value {
    let mut obj = obj.clone();
    if let Some(map) = obj.as_object_mut() {
        map.remove("status");
    }
    if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        for key in [
            "uid",
            "resourceVersion",
            "creationTimestamp",
            "generation",
            "selfLink",
            "managedFields",
        ] {
            meta.remove(key);
        }
    }
    obj
}

/// A line of a diff
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Same(String),
    /// Only in the left object
    Left(String),
    /// Only in the right object
    Right(String),
}

/// Inputs longer than this (lines, each side) aren't diffed: the table is
/// quadratic
pub const DIFF_MAX_LINES: usize = 4000;

/// Line diff of `left` and `right` (longest common subsequence); `None`
/// when either is longer than [`DIFF_MAX_LINES`]
pub fn diff_lines(left: &str, right: &str) -> Option<Vec<DiffLine>> {
    let a: Vec<&str> = left.lines().collect();
    let b: Vec<&str> = right.lines().collect();
    if a.len() > DIFF_MAX_LINES || b.len() > DIFF_MAX_LINES {
        return None;
    }
    let (n, m) = (a.len(), b.len());
    // lcs[i * (m + 1) + j]: common lines of a[i..] and b[j..]
    let width = m + 1;
    let mut lcs = vec![0u32; (n + 1) * width];
    let at = |lcs: &[u32], i: usize, j: usize| lcs.get(i * width + j).copied().unwrap_or(0);
    for (i, line_a) in a.iter().enumerate().rev() {
        for (j, line_b) in b.iter().enumerate().rev() {
            let value = if line_a == line_b {
                at(&lcs, i + 1, j + 1) + 1
            } else {
                at(&lcs, i + 1, j).max(at(&lcs, i, j + 1))
            };
            if let Some(cell) = lcs.get_mut(i * width + j) {
                *cell = value;
            }
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(n.max(m));
    while let (Some(line_a), Some(line_b)) = (a.get(i), b.get(j)) {
        if line_a == line_b {
            out.push(DiffLine::Same((*line_a).to_string()));
            i += 1;
            j += 1;
        } else if at(&lcs, i + 1, j) >= at(&lcs, i, j + 1) {
            out.push(DiffLine::Left((*line_a).to_string()));
            i += 1;
        } else {
            out.push(DiffLine::Right((*line_b).to_string()));
            j += 1;
        }
    }
    out.extend(a.iter().skip(i).map(|l| DiffLine::Left((*l).to_string())));
    out.extend(b.iter().skip(j).map(|l| DiffLine::Right((*l).to_string())));
    Some(out)
}

/// Text from a cluster made safe to draw: control characters (escape codes,
/// carriage returns) replaced, so a name or message can't move the cursor
/// or recolour the terminal
///
/// ```
/// assert_eq!(talos9s_core::kubernetes::printable("a\x1b[31mb\tc"), "a?[31mb c");
/// ```
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => ' ',
            c if c.is_control() => '?',
            c => c,
        })
        .collect()
}

/// An API kind as discovery lists it
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KindRef {
    pub group: String,
    pub version: String,
    pub kind: String,
    pub plural: String,
    pub namespaced: bool,
}

impl KindRef {
    /// `Deployment` for core and well-known groups, `Kustomization.kustomize.toolkit.fluxcd.io` else
    pub fn label(&self) -> String {
        if self.group.is_empty() || BUILTIN_GROUPS.contains(&self.group.as_str()) {
            self.kind.clone()
        } else {
            format!("{}.{}", self.kind, self.group)
        }
    }

    /// `kind.group` as `kubectl get` takes it
    pub fn kubectl_name(&self) -> String {
        if self.group.is_empty() {
            self.plural.clone()
        } else {
            format!("{}.{}", self.plural, self.group)
        }
    }
}

const BUILTIN_GROUPS: &[&str] = &[
    "apps",
    "batch",
    "networking.k8s.io",
    "policy",
    "storage.k8s.io",
    "rbac.authorization.k8s.io",
    "autoscaling",
    "coordination.k8s.io",
    "discovery.k8s.io",
    "scheduling.k8s.io",
    "apiextensions.k8s.io",
    "admissionregistration.k8s.io",
    "events.k8s.io",
    "certificates.k8s.io",
    "node.k8s.io",
];

/// The kinds offered before anything is typed in the kind picker, in this
/// order (open question 2 of `plans/kubernetes.md`: curated, the rest by
/// typing)
pub const DEFAULT_KINDS: &[&str] = &[
    "Pod",
    "Deployment",
    "StatefulSet",
    "DaemonSet",
    "Job",
    "CronJob",
    "Service",
    "Ingress",
    "ConfigMap",
    "Secret",
    "PersistentVolumeClaim",
    "PersistentVolume",
    "Node",
    "Namespace",
    "Event",
    "HelmRelease",
    "Kustomization",
    "GitRepository",
    "OCIRepository",
    "HelmRepository",
    "Application",
    "Certificate",
    "CustomResourceDefinition",
];

/// One kind offered across clusters, and which clusters serve it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedKind {
    pub kind: KindRef,
    pub clusters: Vec<String>,
}

/// The kinds of every cluster in one list (same group and kind, the
/// version of the first cluster that serves it), defaults first in their
/// order, then by label
pub fn merge_kinds(per_cluster: &[(String, Vec<KindRef>)]) -> Vec<MergedKind> {
    let mut merged: Vec<MergedKind> = Vec::new();
    for (cluster, kinds) in per_cluster {
        for k in kinds {
            match merged
                .iter_mut()
                .find(|m| m.kind.group == k.group && m.kind.kind == k.kind)
            {
                Some(m) => {
                    if !m.clusters.contains(cluster) {
                        m.clusters.push(cluster.clone());
                    }
                }
                None => merged.push(MergedKind {
                    kind: k.clone(),
                    clusters: vec![cluster.clone()],
                }),
            }
        }
    }
    let rank = |k: &KindRef| {
        DEFAULT_KINDS
            .iter()
            .position(|d| *d == k.kind)
            .unwrap_or(DEFAULT_KINDS.len())
    };
    merged.sort_by(|a, b| {
        rank(&a.kind)
            .cmp(&rank(&b.kind))
            .then_with(|| a.kind.label().cmp(&b.kind.label()))
    });
    merged
}

/// Is this one of the curated default kinds
pub fn is_default_kind(k: &KindRef) -> bool {
    DEFAULT_KINDS.contains(&k.kind.as_str())
        && (k.group.is_empty()
            || BUILTIN_GROUPS.contains(&k.group.as_str())
            || is_gitops_group(&k.group)
            || k.group == "cert-manager.io")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    fn ts(secs_ago: i64) -> String {
        chrono::DateTime::from_timestamp(NOW - secs_ago, 0)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default()
    }

    #[test]
    fn deployment_replicas() {
        let d = json!({"spec": {"replicas": 3}, "status": {"readyReplicas": 2, "availableReplicas": 2}});
        let s = status("Deployment", &d, NOW);
        assert_eq!((s.health, s.text.as_str()), (ObjectHealth::Degraded, "2/3"));
        let ok = json!({"spec": {"replicas": 1}, "status": {"readyReplicas": 1, "availableReplicas": 1}});
        assert_eq!(status("Deployment", &ok, NOW).health, ObjectHealth::Ready);
        let scaled_down = json!({"spec": {"replicas": 0}, "status": {}});
        assert_eq!(
            status("Deployment", &scaled_down, NOW).health,
            ObjectHealth::Ready
        );
    }

    #[test]
    fn pod_states() {
        let crash = json!({"metadata": {"creationTimestamp": ts(3600)}, "status": {"phase": "Running",
            "containerStatuses": [{"ready": false, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}});
        let s = status("Pod", &crash, NOW);
        assert_eq!(
            (s.health, s.text.as_str()),
            (ObjectHealth::Failed, "CrashLoopBackOff")
        );

        let done = json!({"status": {"phase": "Succeeded"}});
        assert_eq!(status("Pod", &done, NOW).text, "Completed");
        assert!(!is_problem("Pod", &done, &status("Pod", &done, NOW)));

        let young =
            json!({"metadata": {"creationTimestamp": ts(10)}, "status": {"phase": "Pending"}});
        let s = status("Pod", &young, NOW);
        assert_eq!(s.health, ObjectHealth::Progressing);
        assert!(!is_problem("Pod", &young, &s));

        let stuck =
            json!({"metadata": {"creationTimestamp": ts(3600)}, "status": {"phase": "Pending"}});
        assert!(is_problem("Pod", &stuck, &status("Pod", &stuck, NOW)));

        let job_pod = json!({"metadata": {"creationTimestamp": ts(3600),
            "ownerReferences": [{"kind": "Job", "name": "j"}]}, "status": {"phase": "Failed"}});
        assert!(!is_problem("Pod", &job_pod, &status("Pod", &job_pod, NOW)));

        let ready = json!({"status": {"phase": "Running", "containerStatuses": [{"ready": true}]}});
        assert_eq!(status("Pod", &ready, NOW).health, ObjectHealth::Ready);
    }

    #[test]
    fn conditions_drive_crds() {
        let failed = json!({"status": {"conditions": [{"type": "Ready", "status": "False",
            "reason": "InstallFailed", "message": "chart not found"}]}});
        let s = status("HelmRelease", &failed, NOW);
        assert_eq!(s.health, ObjectHealth::Failed);
        assert_eq!(s.text, "InstallFailed");
        assert_eq!(s.message.as_deref(), Some("chart not found"));

        let progressing = json!({"status": {"conditions": [{"type": "Ready", "status": "False", "reason": "Progressing"}]}});
        assert_eq!(
            status("Kustomization", &progressing, NOW).health,
            ObjectHealth::Progressing
        );

        let stale = json!({"metadata": {"generation": 3}, "status": {"conditions": [
            {"type": "Ready", "status": "True", "observedGeneration": 2}]}});
        assert_eq!(
            status("Certificate", &stale, NOW).health,
            ObjectHealth::Progressing
        );

        let suspended = json!({"spec": {"suspend": true}});
        assert_eq!(
            status("Kustomization", &suspended, NOW).health,
            ObjectHealth::Suspended
        );

        // No known status: no verdict, never a problem
        let plain = json!({"data": {"a": "b"}});
        let s = status("ConfigMap", &plain, NOW);
        assert_eq!(s.health, ObjectHealth::Unknown);
        assert!(!is_problem("ConfigMap", &plain, &s));
    }

    #[test]
    fn jobs() {
        let done = json!({"status": {"conditions": [{"type": "Complete", "status": "True"}]}});
        assert_eq!(status("Job", &done, NOW).health, ObjectHealth::Ready);
        let failed = json!({"status": {"conditions": [{"type": "Failed", "status": "True"}]}});
        assert_eq!(status("Job", &failed, NOW).health, ObjectHealth::Failed);
    }

    #[test]
    fn flux_details() {
        let ks = json!({"spec": {"path": "./clusters/prod", "sourceRef": {"kind": "GitRepository", "name": "flux-system"}},
            "status": {"lastAppliedRevision": "main@sha1:abc"}});
        let f = flux_info("Kustomization", &ks);
        assert_eq!(f.revision, "main@sha1:abc");
        assert_eq!(f.source, "GitRepository/flux-system ./clusters/prod");

        let hr = json!({"spec": {"chart": {"spec": {"chart": "cilium", "version": "1.18.0",
            "sourceRef": {"kind": "HelmRepository", "name": "cilium"}}}},
            "status": {"history": [{"chartVersion": "1.18.0"}]}});
        let f = flux_info("HelmRelease", &hr);
        assert_eq!(f.revision, "1.18.0");
        assert_eq!(f.source, "cilium@1.18.0 from HelmRepository/cilium");
        assert!(is_flux_group("helm.toolkit.fluxcd.io"));
    }

    #[test]
    fn argo_applications() {
        let app = |health: &str, sync: &str| {
            json!({"spec": {"source": {"repoURL": "https://git/x", "path": "apps/web", "targetRevision": "main"}},
                "status": {"health": {"status": health}, "sync": {"status": sync, "revision": "abc"}}})
        };
        assert_eq!(
            status("Application", &app("Healthy", "Synced"), NOW).health,
            ObjectHealth::Ready
        );
        let drift = status("Application", &app("Healthy", "OutOfSync"), NOW);
        assert_eq!(
            (drift.health, drift.text.as_str()),
            (ObjectHealth::Degraded, "Healthy/OutOfSync")
        );
        assert_eq!(
            status("Application", &app("Degraded", "Synced"), NOW).health,
            ObjectHealth::Failed
        );
        let f = flux_info("Application", &app("Healthy", "Synced"));
        assert_eq!(f.source, "https://git/x apps/web@main");
        assert_eq!(f.revision, "abc");
        let failed = json!({"status": {"health": {"status": "Healthy"}, "sync": {"status": "Synced"},
            "operationState": {"phase": "Failed", "message": "hook failed"}}});
        let s = status("Application", &failed, NOW);
        assert_eq!(
            (s.health, s.message.as_deref()),
            (ObjectHealth::Failed, Some("hook failed"))
        );
        assert!(is_gitops_group("argoproj.io"));
    }

    #[test]
    fn queries() {
        assert_eq!(Query::parse("l: app=x"), Some(Query::Label("app=x".into())));
        assert_eq!(Query::parse("Redis"), Some(Query::Name("Redis".into())));
        assert_eq!(Query::parse("i:"), Some(Query::Name("i:".into())));
        let pod = json!({"metadata": {"name": "redis-sentinel-0"},
            "spec": {"containers": [{"image": "docker.io/bitnami/redis:7.4"}]}});
        assert!(Query::Name("SENTINEL".into()).matches(&pod));
        assert!(Query::Image("bitnami/redis".into()).matches(&pod));
        assert!(!Query::Image("postgres".into()).matches(&pod));
        let deploy =
            json!({"spec": {"template": {"spec": {"containers": [{"image": "nginx:1"}]}}}});
        assert_eq!(images(&deploy), vec!["nginx:1"]);
    }

    #[test]
    fn selectors() {
        let d = json!({"spec": {"selector": {"matchLabels": {"b": "2", "a": "1"}}}});
        assert_eq!(pod_selector(&d).as_deref(), Some("a=1,b=2"));
        let expr = json!({"spec": {"selector": {"matchLabels": {"a": "1"},
            "matchExpressions": [{"key": "x", "operator": "Exists"}]}}});
        assert_eq!(pod_selector(&expr), None);
    }

    #[test]
    fn secrets_are_redacted() {
        let mut s = json!({"metadata": {"name": "s", "managedFields": [1],
            "annotations": {LAST_APPLIED: "{\"data\":{\"password\":\"aHVudGVyMg==\"}}", "keep": "x"}},
            "data": {"password": "aHVudGVyMg=="}, "stringData": {"token": "abc"}});
        redact("Secret", &mut s);
        let text = s.to_string();
        assert!(!text.contains("aHVudGVyMg"));
        assert!(!text.contains("abc\""));
        assert!(text.contains("password"));
        assert!(text.contains("<hidden, 12 bytes>"));
        assert!(text.contains("keep"));
        assert!(!text.contains("managedFields"));
    }

    #[test]
    fn secret_env_values_are_hidden() {
        let mut pod = json!({"spec": {"containers": [{"env": [
            {"name": "DB_PASSWORD", "value": "hunter2"},
            {"name": "LOG_LEVEL", "value": "debug"},
            {"name": "API_TOKEN", "valueFrom": {"secretKeyRef": {"name": "s", "key": "k"}}}]}]}});
        redact("Pod", &mut pod);
        let text = pod.to_string();
        assert!(!text.contains("hunter2"));
        assert!(text.contains("debug"));
        assert!(text.contains("secretKeyRef"));
    }

    #[test]
    fn compare_and_diff() {
        let a = json!({"metadata": {"name": "x", "uid": "1", "resourceVersion": "5"}, "status": {"a": 1}, "spec": {"r": 1}});
        let c = for_compare(&a);
        assert_eq!(c, json!({"metadata": {"name": "x"}, "spec": {"r": 1}}));

        let d = diff_lines("a\nb\nc", "a\nB\nc\nd").unwrap_or_default();
        assert_eq!(
            d,
            vec![
                DiffLine::Same("a".into()),
                DiffLine::Left("b".into()),
                DiffLine::Right("B".into()),
                DiffLine::Same("c".into()),
                DiffLine::Right("d".into()),
            ]
        );
        let big = "x\n".repeat(DIFF_MAX_LINES + 1);
        assert_eq!(diff_lines(&big, "x"), None);
    }

    #[test]
    fn printable_strips_controls() {
        assert_eq!(printable("ok\r\nnext\u{9b}"), "ok??next?");
        assert_eq!(printable("ünï"), "ünï");
    }

    #[test]
    fn kinds_merge_across_clusters() {
        let k = |group: &str, kind: &str| KindRef {
            group: group.into(),
            version: "v1".into(),
            kind: kind.into(),
            plural: format!("{}s", kind.to_lowercase()),
            namespaced: true,
        };
        let merged = merge_kinds(&[
            ("lab".into(), vec![k("", "Pod"), k("zeta.io", "Widget")]),
            (
                "prod".into(),
                vec![k("helm.toolkit.fluxcd.io", "HelmRelease"), k("", "Pod")],
            ),
        ]);
        let labels: Vec<String> = merged.iter().map(|m| m.kind.label()).collect();
        assert_eq!(
            labels,
            vec![
                "Pod",
                "HelmRelease.helm.toolkit.fluxcd.io",
                "Widget.zeta.io"
            ]
        );
        assert_eq!(merged[0].clusters, vec!["lab", "prod"]);
        assert_eq!(merged[2].clusters, vec!["lab"]);
        assert!(is_default_kind(&merged[1].kind));
        assert!(!is_default_kind(&merged[2].kind));
        assert_eq!(
            merged[1].kind.kubectl_name(),
            "helmreleases.helm.toolkit.fluxcd.io"
        );
    }
}
