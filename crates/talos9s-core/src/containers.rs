//! Containers on nodes, as containerd reports them through the Talos API
//! (no Kubernetes API needed): Talos services, and Kubernetes pods with
//! their containers, grouped, filtered and sorted for the Containers view
//!
//! The API gives no restart counts or exit codes, so nothing here calls a
//! container failing: exited is normal for init containers and jobs.

use std::collections::BTreeSet;
use talos_rs::{ContainerInfo, ContainerKind};

/// A container and the node it runs on
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeContainer {
    /// Hostname
    pub node: String,
    /// The node's Talos API address
    pub address: String,
    pub info: ContainerInfo,
}

/// The namespace system services are listed under
pub const SYSTEM_NAMESPACE: &str = "system";

impl NodeContainer {
    pub fn namespace(&self) -> &str {
        match self.info.kind {
            ContainerKind::System => SYSTEM_NAMESPACE,
            ContainerKind::Kubernetes => self.info.pod.split_once('/').map_or("", |(ns, _)| ns),
        }
    }

    /// The pod's name; empty for a system service
    pub fn pod(&self) -> &str {
        match self.info.kind {
            ContainerKind::System => "",
            ContainerKind::Kubernetes => self
                .info
                .pod
                .split_once('/')
                .map_or(self.info.pod.as_str(), |(_, p)| p),
        }
    }

    /// The container's name: a service's id, a pod container's name
    pub fn name(&self) -> &str {
        match self.info.kind {
            ContainerKind::System => &self.info.id,
            ContainerKind::Kubernetes => &self.info.name,
        }
    }

    pub fn state(&self) -> CtrState {
        CtrState::of(&self.info.status)
    }

    /// Rows of one pod share it
    fn pod_key(&self) -> String {
        format!("{}/{}", self.node, self.info.pod)
    }
}

/// A container's state, from containerd's status text
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CtrState {
    /// Created, unknown, paused: neither running nor done
    Other,
    Exited,
    Running,
}

impl CtrState {
    /// `RUNNING`, `CONTAINER_RUNNING`, `SANDBOX_READY`, `CONTAINER_EXITED`, …
    pub fn of(status: &str) -> Self {
        if status.ends_with("RUNNING") || status == "SANDBOX_READY" {
            CtrState::Running
        } else if status.ends_with("EXITED")
            || status.ends_with("STOPPED")
            || status == "SANDBOX_NOTREADY"
        {
            CtrState::Exited
        } else {
            CtrState::Other
        }
    }
}

/// How the list is sorted (`1`-`6`, in column order)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContainerSort {
    Node,
    #[default]
    Namespace,
    /// The pod or service name
    Name,
    Image,
    Pid,
    /// Not running first
    Status,
}

impl ContainerSort {
    pub const ALL: [ContainerSort; 6] = [
        ContainerSort::Node,
        ContainerSort::Namespace,
        ContainerSort::Name,
        ContainerSort::Image,
        ContainerSort::Pid,
        ContainerSort::Status,
    ];

    /// Its name in the state file
    pub fn name(&self) -> &'static str {
        match self {
            ContainerSort::Node => "Node",
            ContainerSort::Namespace => "Namespace",
            ContainerSort::Name => "Name",
            ContainerSort::Image => "Image",
            ContainerSort::Pid => "Pid",
            ContainerSort::Status => "Status",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// What the list shows
#[derive(Debug, Clone, Default)]
pub struct ContainerOptions {
    /// Exited containers too (`A`)
    pub show_exited: bool,
    /// Lower case; matched against node, namespace, pod, container, image
    pub filter: String,
    /// Namespaces warped into (`namespaces::matches`); empty for all
    pub warp: Vec<String>,
    pub sort: ContainerSort,
    /// Unfolded pods (`pod_key`: `node/namespace/pod`)
    pub expanded: BTreeSet<String>,
}

/// A row of the list; indices into the containers
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerRow {
    /// A Talos service
    Service(usize),
    Pod {
        /// `node/namespace/pod`, for folding and selection
        key: String,
        /// Its sandbox, or its first container
        first: usize,
        /// Its containers shown, by name
        members: Vec<usize>,
        running: usize,
        expanded: bool,
    },
    /// A container of the pod above
    Member(usize),
}

impl ContainerRow {
    /// Stable across refreshes, for keeping the selection
    pub fn id(&self, all: &[NodeContainer]) -> String {
        match self {
            ContainerRow::Pod { key, .. } => key.clone(),
            ContainerRow::Service(i) | ContainerRow::Member(i) => all
                .get(*i)
                .map(|c| format!("{}/{}:{}", c.node, c.info.pod, c.name()))
                .unwrap_or_default(),
        }
    }
}

fn hit(c: &NodeContainer, filter: &str) -> bool {
    filter.is_empty()
        || [
            c.node.as_str(),
            c.namespace(),
            c.pod(),
            c.name(),
            c.info.image.as_str(),
        ]
        .iter()
        .any(|f| f.to_lowercase().contains(filter))
}

/// The rows: services and pods sorted by `options.sort`, each unfolded
/// pod followed by its containers. Exited containers are left out unless
/// `show_exited`, and so is a pod with nothing else left. While filtering,
/// a pod's matching containers are shown under it.
pub fn container_rows(all: &[NodeContainer], options: &ContainerOptions) -> Vec<ContainerRow> {
    let shown = |c: &NodeContainer| options.show_exited || c.state() != CtrState::Exited;
    let filter = options.filter.as_str();
    // Per top-level entry: the container whose image sorts it, and the row
    let mut entries: Vec<(usize, ContainerRow)> = Vec::new();
    let mut pods: std::collections::BTreeMap<String, (Option<usize>, Vec<usize>)> =
        std::collections::BTreeMap::new();
    for (i, c) in all.iter().enumerate() {
        if !crate::namespaces::matches(&options.warp, c.namespace()) {
            continue;
        }
        match c.info.kind {
            ContainerKind::System => {
                if shown(c) && hit(c, filter) {
                    entries.push((i, ContainerRow::Service(i)));
                }
            }
            ContainerKind::Kubernetes => {
                let pod = pods.entry(c.pod_key()).or_default();
                if c.info.sandbox {
                    pod.0 = Some(i);
                } else {
                    pod.1.push(i);
                }
            }
        }
    }
    for (key, (sandbox, members)) in pods {
        let Some(first) = sandbox.or_else(|| members.first().copied()) else {
            continue;
        };
        let Some(head) = all.get(first) else { continue };
        let mut visible: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&m| all.get(m).is_some_and(shown))
            .collect();
        // Nothing running and nothing left to show: a finished pod
        let sandbox_up = sandbox
            .and_then(|s| all.get(s))
            .is_some_and(|s| s.state() == CtrState::Running);
        if visible.is_empty() && !options.show_exited && !(sandbox_up && members.is_empty()) {
            continue;
        }
        // The pod itself matches (node, namespace, name): all its containers
        let pod_hit = [head.node.as_str(), head.namespace(), head.pod()]
            .iter()
            .any(|f| f.to_lowercase().contains(filter));
        let filtering = !filter.is_empty() && !pod_hit;
        if filtering {
            visible.retain(|&m| all.get(m).is_some_and(|c| hit(c, filter)));
            if visible.is_empty() {
                continue;
            }
        }
        visible.sort_by(|&a, &b| {
            let name = |i: usize| all.get(i).map(NodeContainer::name);
            name(a).cmp(&name(b))
        });
        let running = visible
            .iter()
            .filter(|&&m| all.get(m).is_some_and(|c| c.state() == CtrState::Running))
            .count();
        let expanded = filtering || options.expanded.contains(&key);
        entries.push((
            visible.first().copied().unwrap_or(first),
            ContainerRow::Pod {
                key,
                first,
                members: visible,
                running,
                expanded,
            },
        ));
    }

    // Sort keys: node, namespace, name, image, pid, status rank
    let sort_key = |(member, row): &(usize, ContainerRow)| {
        let (head, rank, pid) = match row {
            ContainerRow::Pod {
                first,
                members,
                running,
                ..
            } => {
                let head = all.get(*first);
                let rank = u8::from(*running == members.len() && !members.is_empty());
                (head, rank, head.map_or(0, |h| h.info.pid))
            }
            ContainerRow::Service(i) | ContainerRow::Member(i) => {
                let c = all.get(*i);
                let rank = u8::from(c.is_some_and(|c| c.state() == CtrState::Running));
                (c, rank, c.map_or(0, |c| c.info.pid))
            }
        };
        let image = all.get(*member).map_or("", |c| c.info.image.as_str());
        let name = head.map_or("", |h| match row {
            ContainerRow::Pod { .. } => h.pod(),
            _ => h.name(),
        });
        let node = head.map_or("", |h| h.node.as_str());
        let namespace = head.map_or("", NodeContainer::namespace);
        (node, namespace, name, image, pid, rank)
    };
    entries.sort_by(|a, b| {
        let (ka, kb) = (sort_key(a), sort_key(b));
        let first = match options.sort {
            ContainerSort::Node => ka.0.cmp(kb.0),
            ContainerSort::Namespace => ka.1.cmp(kb.1),
            ContainerSort::Name => ka.2.cmp(kb.2),
            ContainerSort::Image => ka.3.cmp(kb.3),
            ContainerSort::Pid => ka.4.cmp(&kb.4),
            ContainerSort::Status => ka.5.cmp(&kb.5),
        };
        first
            .then_with(|| ka.1.cmp(kb.1))
            .then_with(|| ka.2.cmp(kb.2))
            .then_with(|| ka.0.cmp(kb.0))
    });

    let mut rows = Vec::new();
    for (_, row) in entries {
        let members = match &row {
            ContainerRow::Pod {
                members,
                expanded: true,
                ..
            } => members.clone(),
            _ => Vec::new(),
        };
        rows.push(row);
        rows.extend(members.into_iter().map(ContainerRow::Member));
    }
    rows
}

/// Running, exited and other containers (sandboxes left out) in the warp
pub fn state_counts(all: &[NodeContainer], warp: &[String]) -> (usize, usize, usize) {
    let mut counts = (0, 0, 0);
    for c in all
        .iter()
        .filter(|c| !c.info.sandbox && crate::namespaces::matches(warp, c.namespace()))
    {
        match c.state() {
            CtrState::Running => counts.0 += 1,
            CtrState::Exited => counts.1 += 1,
            CtrState::Other => counts.2 += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctr(node: &str, pod: &str, name: &str, status: &str) -> NodeContainer {
        let kind = if pod.is_empty() {
            ContainerKind::System
        } else {
            ContainerKind::Kubernetes
        };
        NodeContainer {
            node: node.into(),
            address: format!("10.0.0.{}", node.len()),
            info: ContainerInfo {
                kind,
                id: if pod.is_empty() {
                    name.into()
                } else {
                    format!("{pod}:{name}:abc")
                },
                name: name.into(),
                pod: pod.into(),
                sandbox: false,
                image: format!("img/{name}"),
                pid: 1,
                status: status.into(),
            },
        }
    }

    fn sandbox(node: &str, pod: &str, status: &str) -> NodeContainer {
        let mut c = ctr(node, pod, pod, status);
        c.info.sandbox = true;
        c
    }

    fn all() -> Vec<NodeContainer> {
        vec![
            ctr("bc1", "", "apid", "RUNNING"),
            sandbox("bc1", "web/gw-1", "SANDBOX_READY"),
            ctr("bc1", "web/gw-1", "init", "CONTAINER_EXITED"),
            ctr("bc1", "web/gw-1", "app", "CONTAINER_RUNNING"),
            sandbox("bc2", "web/gw-2", "SANDBOX_READY"),
            ctr("bc2", "web/gw-2", "app", "CONTAINER_CREATED"),
            // A finished job: hidden without `A`
            sandbox("bc2", "jobs/done", "SANDBOX_NOTREADY"),
            ctr("bc2", "jobs/done", "run", "CONTAINER_EXITED"),
        ]
    }

    fn labels(all: &[NodeContainer], rows: &[ContainerRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                ContainerRow::Service(i) => all[*i].name().to_string(),
                ContainerRow::Pod {
                    first,
                    members,
                    running,
                    ..
                } => format!("{} {running}/{}", all[*first].pod(), members.len()),
                ContainerRow::Member(i) => format!("  {}", all[*i].name()),
            })
            .collect()
    }

    #[test]
    fn groups_folds_and_hides_exited() {
        let all = all();
        let mut o = ContainerOptions::default();
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            ["apid", "gw-1 1/1", "gw-2 0/1"]
        );
        o.show_exited = true;
        o.expanded.insert("bc1/web/gw-1".into());
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            [
                "done 0/1", "apid", "gw-1 1/2", "  app", "  init", "gw-2 0/1"
            ]
        );
        // A filter unfolds the pod with the matching containers only
        let o = ContainerOptions {
            filter: "init".into(),
            show_exited: true,
            ..Default::default()
        };
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            ["gw-1 0/1", "  init"]
        );
        // The pod's own name matches: all its containers, folded
        let o = ContainerOptions {
            filter: "gw-2".into(),
            ..Default::default()
        };
        assert_eq!(labels(&all, &container_rows(&all, &o)), ["gw-2 0/1"]);
    }

    #[test]
    fn sorts_and_warps() {
        let all = all();
        let o = ContainerOptions {
            sort: ContainerSort::Status,
            ..Default::default()
        };
        // Not all running first
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            ["gw-2 0/1", "apid", "gw-1 1/1"]
        );
        let o = ContainerOptions {
            sort: ContainerSort::Node,
            warp: vec!["web".into()],
            ..Default::default()
        };
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            ["gw-1 1/1", "gw-2 0/1"]
        );
        let o = ContainerOptions {
            warp: vec![SYSTEM_NAMESPACE.into()],
            ..Default::default()
        };
        assert_eq!(labels(&all, &container_rows(&all, &o)), ["apid"]);
        assert_eq!(state_counts(&all, &[]), (2, 2, 1));
        for s in ContainerSort::ALL {
            assert_eq!(ContainerSort::from_name(s.name()), Some(s));
        }
        assert_eq!(CtrState::of("SANDBOX_NOTREADY"), CtrState::Exited);
        assert_eq!(CtrState::of("CONTAINER_UNKNOWN"), CtrState::Other);
    }
}
