//! Container logs read through the Kubernetes API: which containers to read
//! for a workload, a pod or one container, and merging their lines by time
//!
//! Lines are read with `timestamps=true`: the API puts an RFC 3339 time
//! before each line, so lines of several containers can be put in order.

use crate::workloads::{ContainerFacts, PodFacts};

/// One container to read the logs of
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSource {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    /// Shown before each line when several sources are merged
    pub label: String,
    /// The container has a previous run to read (`--previous`): the kubelet
    /// keeps the last terminated one only
    pub has_previous: bool,
}

/// A merged line
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Nanoseconds since the epoch, from the API's timestamp
    pub time: Option<i64>,
    /// The source's label (empty for a single source)
    pub source: String,
    pub text: String,
}

/// The containers of `pod` whose logs say something: the app containers and
/// the init containers that haven't finished
pub fn log_containers(pod: &PodFacts) -> Vec<&ContainerFacts> {
    pod.init_containers
        .iter()
        .filter(|c| !matches!(c.terminated, Some((0, _))))
        .chain(&pod.containers)
        .collect()
}

/// Every container of `pod`, init containers first: the rows under an
/// expanded pod
pub fn all_containers(pod: &PodFacts) -> Vec<&ContainerFacts> {
    pod.init_containers.iter().chain(&pod.containers).collect()
}

/// Whether `container` is one of `pod`'s init containers
pub fn is_init(pod: &PodFacts, container: &ContainerFacts) -> bool {
    pod.init_containers
        .iter()
        .any(|c| std::ptr::eq(c, container))
}

/// The sources for `pods` (all their log containers), or for one container
/// of a single pod, a finished init container too. Labels name what tells
/// the sources apart: the pod, the container, or both.
pub fn log_sources(pods: &[&PodFacts], container: Option<&str>) -> Vec<LogSource> {
    let several_pods = pods.len() > 1;
    let mut sources = Vec::new();
    for pod in pods {
        let containers: Vec<&ContainerFacts> = match container {
            Some(name) => all_containers(pod)
                .into_iter()
                .filter(|c| c.name == name)
                .collect(),
            None => log_containers(pod),
        };
        let several_containers = containers.len() > 1;
        for c in containers {
            let label = match (several_pods, several_containers) {
                (true, true) => format!("{}/{}", pod.name, c.name),
                (true, false) => pod.name.clone(),
                (false, true) => c.name.clone(),
                (false, false) => String::new(),
            };
            sources.push(LogSource {
                namespace: pod.namespace.clone(),
                pod: pod.name.clone(),
                container: c.name.clone(),
                label,
                has_previous: c.last_terminated.is_some(),
            });
        }
    }
    sources
}

/// Lines to read per source so that `total` lines are read in all, at
/// least 50 each
pub fn tail_per_source(sources: usize, total: i64) -> i64 {
    let n = i64::try_from(sources.max(1)).unwrap_or(i64::MAX);
    (total / n).max(50)
}

/// The API's timestamp in front of `line`, and the rest
pub fn split_timestamp(line: &str) -> (Option<i64>, &str) {
    match line.split_once(' ') {
        Some((first, rest)) => match chrono::DateTime::parse_from_rfc3339(first) {
            Ok(t) => (t.timestamp_nanos_opt(), rest),
            Err(_) => (None, line),
        },
        None => match chrono::DateTime::parse_from_rfc3339(line) {
            Ok(t) => (t.timestamp_nanos_opt(), ""),
            Err(_) => (None, line),
        },
    }
}

/// The lines of each source's text, oldest first; a line without a
/// timestamp keeps its place after the line before it
pub fn merge_logs(texts: &[(String, String)]) -> Vec<LogLine> {
    let mut lines = Vec::new();
    for (source, text) in texts {
        let mut last = None;
        for raw in text.lines() {
            let (time, rest) = split_timestamp(raw);
            last = time.or(last);
            lines.push(LogLine {
                time: last,
                source: source.clone(),
                text: rest.to_string(),
            });
        }
    }
    // Stable: lines of one source with the same time keep their order
    lines.sort_by_key(|l| l.time.unwrap_or(i64::MIN));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str) -> ContainerFacts {
        ContainerFacts {
            name: name.into(),
            ..Default::default()
        }
    }

    fn pod(name: &str, containers: &[&str]) -> PodFacts {
        PodFacts {
            namespace: "apps".into(),
            name: name.into(),
            init_containers: vec![ContainerFacts {
                terminated: Some((0, Some("Completed".into()))),
                ..container("init")
            }],
            containers: containers.iter().map(|c| container(c)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn sources_and_labels() {
        let a = pod("web-a", &["app", "proxy"]);
        let b = pod("web-b", &["app", "proxy"]);
        let labels = |s: Vec<LogSource>| s.into_iter().map(|s| s.label).collect::<Vec<_>>();
        assert_eq!(
            labels(log_sources(&[&a, &b], None)),
            ["web-a/app", "web-a/proxy", "web-b/app", "web-b/proxy"]
        );
        // One pod: its containers; one container: no label
        assert_eq!(labels(log_sources(&[&a], None)), ["app", "proxy"]);
        assert_eq!(labels(log_sources(&[&a], Some("proxy"))), [""]);
        // A failed init container is read, a finished one isn't
        let mut stuck = pod("web-c", &["app"]);
        stuck.init_containers[0].terminated = Some((1, Some("Error".into())));
        assert_eq!(labels(log_sources(&[&stuck], None)), ["init", "app"]);
        // A finished init container: left out of the merge, read on its own
        assert_eq!(labels(log_sources(&[&a], Some("init"))), [""]);
        assert!(is_init(&a, &a.init_containers[0]));
        assert!(!is_init(&a, &a.containers[0]));
        // A previous run only where a container terminated before
        assert!(!log_sources(&[&a], None).iter().any(|s| s.has_previous));
        let mut restarted = pod("web-d", &["app"]);
        restarted.containers[0].last_terminated = Some((1, Some("Error".into())));
        assert!(log_sources(&[&restarted], None)[0].has_previous);
        assert_eq!(tail_per_source(4, 1000), 250);
        assert_eq!(tail_per_source(40, 1000), 50);
    }

    #[test]
    fn merges_by_time() {
        let texts = vec![
            (
                "a".to_string(),
                "2026-11-02T15:00:00.5Z first a\n2026-11-02T15:00:02Z second a\ncontinued"
                    .to_string(),
            ),
            (
                "b".to_string(),
                "2026-11-02T15:00:01.000000001Z b\nno time".to_string(),
            ),
        ];
        let merged: Vec<String> = merge_logs(&texts)
            .into_iter()
            .map(|l| format!("{} {}", l.source, l.text))
            .collect();
        assert_eq!(
            merged,
            ["a first a", "b b", "b no time", "a second a", "a continued"]
        );
        assert_eq!(split_timestamp("plain line"), (None, "plain line"));
    }
}
