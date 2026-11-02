//! Warping into namespaces (`W`): which namespaces a warp keeps, the warps
//! used last, and the picker's entries
//!
//! A warp is a set of namespaces and prefixes (`kolmena-staging*`); empty
//! is all. Namespace names are DNS labels, so a set is stored joined with
//! commas.

/// Warps remembered per cluster, newest first
pub const RECENT_NAMESPACES: usize = 4;

/// `namespace` is in `warp`: one of its names, or under one of its
/// prefixes (`web*`); everything is in an empty warp
pub fn matches(warp: &[String], namespace: &str) -> bool {
    warp.is_empty()
        || warp.iter().any(|w| match w.strip_suffix('*') {
            Some(prefix) => namespace.starts_with(prefix),
            None => w == namespace,
        })
}

/// A warp as stored and shown in the recent list: `web,db`
pub fn join(warp: &[String]) -> String {
    warp.join(",")
}

/// A stored warp back into its parts
pub fn split(stored: &str) -> Vec<String> {
    stored
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A warp for a title: `all`, `web`, `web, db`, or `4 namespaces` when the
/// names don't fit `width`
pub fn label(warp: &[String], width: usize) -> String {
    let joined = warp.join(", ");
    match warp.len() {
        0 => "all".to_string(),
        1 => joined,
        n if joined.chars().count() > width => format!("{n} namespaces"),
        _ => joined,
    }
}

/// `warp` to the front of `recent`, once, at most `RECENT_NAMESPACES`;
/// all namespaces (an empty warp) isn't remembered
pub fn push_recent(recent: &mut Vec<String>, warp: &[String]) {
    if warp.is_empty() {
        return;
    }
    let stored = join(warp);
    recent.retain(|r| *r != stored);
    recent.insert(0, stored);
    recent.truncate(RECENT_NAMESPACES);
}

/// An entry of the namespace picker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerEntry {
    /// What Enter warps into (empty: all namespaces)
    pub warp: Vec<String>,
    /// What the view has in it (pods, volumes); `None` when none of its
    /// namespaces has anything now
    pub count: Option<usize>,
    /// Its place among the recent warps, `1`-`4` picks it
    pub recent: Option<usize>,
}

impl PickerEntry {
    /// One namespace, the ones `Space` marks
    pub fn single(&self) -> Option<&str> {
        match self.warp.as_slice() {
            [one] if !one.ends_with('*') => Some(one),
            _ => None,
        }
    }
}

/// The picker's entries. Without a filter: all namespaces, the recent
/// warps, then every namespace of `counts` by name. A filter (any case)
/// keeps what contains it; one ending in `*` is a prefix, offered first as
/// a warp of its own.
pub fn picker_entries(
    counts: &[(String, usize)],
    recent: &[String],
    filter: &str,
) -> Vec<PickerEntry> {
    let needle = filter.to_lowercase();
    let prefix = needle.strip_suffix('*').filter(|p| !p.is_empty());
    let hit = |ns: &str| match prefix {
        Some(p) => ns.to_lowercase().starts_with(p),
        None => needle.is_empty() || ns.to_lowercase().contains(&needle),
    };
    let count = |warp: &[String]| {
        let n: usize = counts
            .iter()
            .filter(|(ns, _)| matches(warp, ns))
            .map(|(_, c)| c)
            .sum();
        (n > 0).then_some(n)
    };
    let mut entries = Vec::new();
    if needle.is_empty() {
        entries.push(PickerEntry {
            warp: Vec::new(),
            count: Some(counts.iter().map(|(_, c)| c).sum()),
            recent: None,
        });
    }
    if let Some(p) = prefix {
        let warp = vec![format!("{p}*")];
        entries.push(PickerEntry {
            count: count(&warp),
            warp,
            recent: None,
        });
    }
    for (i, stored) in recent.iter().enumerate() {
        let warp = split(stored);
        if warp.iter().any(|w| hit(w.trim_end_matches('*'))) {
            entries.push(PickerEntry {
                count: count(&warp),
                warp,
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
        warp: vec![ns.clone()],
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

    fn warp(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn matches_names_and_prefixes() {
        assert!(matches(&[], "anything"));
        let w = warp(&["db", "web*"]);
        assert!(matches(&w, "db"));
        assert!(!matches(&w, "db2"), "a name is exact");
        assert!(matches(&w, "web-staging"));
        assert!(!matches(&w, "my-web"));
        assert_eq!(split(&join(&w)), w);
        assert_eq!(label(&w, 30), "db, web*");
        assert_eq!(label(&w, 5), "2 namespaces");
        assert_eq!(label(&[], 5), "all");
    }

    #[test]
    fn recent_keeps_the_last_four_sets_once() {
        let mut recent = Vec::new();
        for w in [&["a"][..], &["b", "c"], &["a"], &["d"], &["e"], &[]] {
            push_recent(&mut recent, &warp(w));
        }
        assert_eq!(recent, ["e", "d", "a", "b,c"]);
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
        let recent = vec!["web".to_string(), "db,web".to_string(), "gone".to_string()];
        let shown: Vec<_> = picker_entries(&counts, &recent, "")
            .into_iter()
            .map(|e| (join(&e.warp), e.count, e.recent))
            .collect();
        assert_eq!(
            shown,
            [
                (String::new(), Some(4), None),
                ("web".into(), Some(2), Some(1)),
                ("db,web".into(), Some(3), Some(2)),
                ("gone".into(), None, Some(3)),
                ("db".into(), Some(1), None),
                ("kube-system".into(), Some(1), None),
            ]
        );
        let names = |filter: &str| -> Vec<String> {
            picker_entries(&counts, &[], filter)
                .into_iter()
                .map(|e| join(&e.warp))
                .collect()
        };
        assert_eq!(names("D"), ["db"]);
        // A prefix: offered as a warp first, then what it covers
        assert_eq!(names("kube*"), ["kube*", "kube-system"]);
        assert_eq!(picker_entries(&counts, &[], "w*")[0].count, Some(2));
    }
}
