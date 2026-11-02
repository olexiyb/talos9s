//! Warping into a namespace (`W`): the namespaces a view offers, the ones
//! used last, and the picker's entries

/// Namespaces remembered per cluster, newest first
pub const RECENT_NAMESPACES: usize = 4;

/// `namespace` to the front of `recent`, once, at most `RECENT_NAMESPACES`
pub fn push_recent(recent: &mut Vec<String>, namespace: &str) {
    recent.retain(|n| n != namespace);
    recent.insert(0, namespace.to_string());
    recent.truncate(RECENT_NAMESPACES);
}

/// An entry of the namespace picker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerEntry {
    /// `None`: all namespaces (no warp)
    pub namespace: Option<String>,
    /// What the view has in it (pods, volumes); `None` when unknown, or a
    /// recent namespace that has nothing now
    pub count: Option<usize>,
    /// Its place among the recent ones, `1`-`4` picks it
    pub recent: Option<usize>,
}

/// The picker's entries: all namespaces, the recent ones, then every other
/// namespace of `counts` by name; with a `filter` (any case), only the
/// namespaces containing it
pub fn picker_entries(
    counts: &[(String, usize)],
    recent: &[String],
    filter: &str,
) -> Vec<PickerEntry> {
    let needle = filter.to_lowercase();
    let hit = |ns: &str| needle.is_empty() || ns.to_lowercase().contains(&needle);
    let count = |ns: &str| counts.iter().find(|(n, _)| n == ns).map(|(_, c)| *c);
    let mut entries = Vec::new();
    if needle.is_empty() {
        entries.push(PickerEntry {
            namespace: None,
            count: Some(counts.iter().map(|(_, c)| c).sum()),
            recent: None,
        });
    }
    for (i, ns) in recent.iter().enumerate() {
        if hit(ns) {
            entries.push(PickerEntry {
                namespace: Some(ns.clone()),
                count: count(ns),
                recent: Some(i + 1),
            });
        }
    }
    let mut others: Vec<&(String, usize)> = counts
        .iter()
        .filter(|(ns, _)| !recent.contains(ns) && hit(ns))
        .collect();
    others.sort_by(|a, b| a.0.cmp(&b.0));
    entries.extend(others.into_iter().map(|(ns, c)| PickerEntry {
        namespace: Some(ns.clone()),
        count: Some(*c),
        recent: None,
    }));
    entries
}

/// How many of `namespaces` are in each, by name
pub fn count_by_namespace<'a>(
    namespaces: impl IntoIterator<Item = &'a str>,
) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for ns in namespaces {
        *counts.entry(ns).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(ns, c)| (ns.to_string(), c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_keeps_the_last_four_once() {
        let mut recent = Vec::new();
        for ns in ["a", "b", "c", "a", "d", "e"] {
            push_recent(&mut recent, ns);
        }
        assert_eq!(recent, ["e", "d", "a", "c"]);
    }

    #[test]
    fn entries_all_recent_then_the_rest() {
        let counts = count_by_namespace(["web", "db", "web", "kube-system"]);
        assert_eq!(
            counts,
            [
                ("db".into(), 1),
                ("kube-system".into(), 1),
                ("web".into(), 2)
            ]
        );
        let recent = vec!["web".to_string(), "gone".to_string()];
        let entries = picker_entries(&counts, &recent, "");
        let shown: Vec<_> = entries
            .iter()
            .map(|e| (e.namespace.as_deref(), e.count, e.recent))
            .collect();
        assert_eq!(
            shown,
            [
                (None, Some(4), None),
                (Some("web"), Some(2), Some(1)),
                (Some("gone"), None, Some(2)),
                (Some("db"), Some(1), None),
                (Some("kube-system"), Some(1), None),
            ]
        );
        // A filter drops "all" and keeps what contains it
        let names: Vec<_> = picker_entries(&counts, &recent, "D")
            .into_iter()
            .filter_map(|e| e.namespace)
            .collect();
        assert_eq!(names, ["db"]);
    }
}
