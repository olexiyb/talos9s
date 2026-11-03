//! Containers on nodes, as containerd reports them through the Talos API
//! (no Kubernetes API needed): Talos services and Kubernetes containers,
//! filtered and sorted for the Containers view
//!
//! The API gives no restart counts or exit codes, so nothing here calls a
//! container failing: exited is normal for init containers and jobs.

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

    /// Stable across refreshes, for keeping the selection
    pub fn key(&self) -> String {
        format!("{}/{}:{}", self.node, self.info.pod, self.name())
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

/// How the list is sorted (`1`-`7`, in column order)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContainerSort {
    Node,
    #[default]
    Namespace,
    Pod,
    /// The container (or service) name
    Name,
    Image,
    Pid,
    /// Not running first
    Status,
}

impl ContainerSort {
    pub const ALL: [ContainerSort; 7] = [
        ContainerSort::Node,
        ContainerSort::Namespace,
        ContainerSort::Pod,
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
            ContainerSort::Pod => "Pod",
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

/// The containers shown, as indices, sorted by `options.sort`: pod
/// sandboxes left out, exited ones too unless `show_exited`
pub fn container_rows(all: &[NodeContainer], options: &ContainerOptions) -> Vec<usize> {
    let mut rows: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            !c.info.sandbox
                && (options.show_exited || c.state() != CtrState::Exited)
                && crate::namespaces::matches(&options.warp, c.namespace())
                && hit(c, &options.filter)
        })
        .map(|(i, _)| i)
        .collect();
    rows.sort_by(|&a, &b| {
        let (Some(a), Some(b)) = (all.get(a), all.get(b)) else {
            return std::cmp::Ordering::Equal;
        };
        let first = match options.sort {
            ContainerSort::Node => a.node.cmp(&b.node),
            ContainerSort::Namespace => a.namespace().cmp(b.namespace()),
            ContainerSort::Pod => a.pod().cmp(b.pod()),
            ContainerSort::Name => a.name().cmp(b.name()),
            ContainerSort::Image => a.info.image.cmp(&b.info.image),
            ContainerSort::Pid => a.info.pid.cmp(&b.info.pid),
            // Not running first
            ContainerSort::Status => a.state().cmp(&b.state()),
        };
        first
            .then_with(|| a.namespace().cmp(b.namespace()))
            .then_with(|| a.pod().cmp(b.pod()))
            .then_with(|| a.name().cmp(b.name()))
            .then_with(|| a.node.cmp(&b.node))
    });
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

    fn labels(all: &[NodeContainer], rows: &[usize]) -> Vec<String> {
        rows.iter()
            .map(|&i| format!("{}:{}", all[i].pod(), all[i].name()))
            .collect()
    }

    #[test]
    fn lists_containers_and_hides_exited() {
        let all = all();
        let mut o = ContainerOptions::default();
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            [":apid", "gw-1:app", "gw-2:app"]
        );
        o.show_exited = true;
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            ["done:run", ":apid", "gw-1:app", "gw-1:init", "gw-2:app"]
        );
        o.filter = "init".into();
        assert_eq!(labels(&all, &container_rows(&all, &o)), ["gw-1:init"]);
    }

    #[test]
    fn sorts_and_warps() {
        let all = all();
        let o = ContainerOptions {
            sort: ContainerSort::Status,
            ..Default::default()
        };
        // Not running first
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            ["gw-2:app", ":apid", "gw-1:app"]
        );
        let o = ContainerOptions {
            sort: ContainerSort::Node,
            warp: vec!["web".into()],
            ..Default::default()
        };
        assert_eq!(
            labels(&all, &container_rows(&all, &o)),
            ["gw-1:app", "gw-2:app"]
        );
        let o = ContainerOptions {
            warp: vec![SYSTEM_NAMESPACE.into()],
            ..Default::default()
        };
        assert_eq!(labels(&all, &container_rows(&all, &o)), [":apid"]);
        assert_eq!(state_counts(&all, &[]), (2, 2, 1));
        for s in ContainerSort::ALL {
            assert_eq!(ContainerSort::from_name(s.name()), Some(s));
        }
        assert_eq!(CtrState::of("SANDBOX_NOTREADY"), CtrState::Exited);
        assert_eq!(CtrState::of("CONTAINER_UNKNOWN"), CtrState::Other);
    }
}
