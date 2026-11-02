//! Summaries over node values: the common version, capacity in use

/// The value occurring most often (ties go to the first seen), or `None` if empty
pub fn most_common<'a>(values: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for v in values {
        match counts.iter_mut().find(|(c, _)| *c == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    counts
        .into_iter()
        .rev()
        .max_by_key(|(_, n)| *n)
        .map(|(v, _)| v)
}

/// `used/total` in the total's unit, e.g. `287/840G` or `0.4/6.8T`
pub fn used_of_total(used: u64, total: u64) -> String {
    const TIB: f64 = (1u64 << 40) as f64;
    if total as f64 >= TIB {
        let precision = |v: f64| if v < 10.0 { 1 } else { 0 };
        let (u, t) = (used as f64 / TIB, total as f64 / TIB);
        format!("{u:.p$}/{t:.q$}T", p = precision(u), q = precision(t))
    } else {
        format!("{:.0}/{:.0}G", gib(used), gib(total))
    }
}

/// Percent of `total` in use, `None` when `total` is 0
///
/// ```
/// use talos9s_core::stats::used_pct;
///
/// assert_eq!(used_pct(1, 4), Some(25.0));
/// assert_eq!(used_pct(5, 0), None);
/// ```
pub fn used_pct(used: u64, total: u64) -> Option<f32> {
    (total > 0).then(|| used as f32 / total as f32 * 100.0)
}

/// Bytes to GiB, for display
pub fn gib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / 1024.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_common_prefers_majority_then_first_seen() {
        let v = ["v1.11.2", "v1.11.3", "v1.11.2"];
        assert_eq!(most_common(v.into_iter()), Some("v1.11.2"));
        assert_eq!(most_common(["a", "b"].into_iter()), Some("a"));
        assert_eq!(most_common(std::iter::empty()), None);
    }

    #[test]
    fn used_of_total_takes_the_total_unit() {
        const GIB: u64 = 1 << 30;
        assert_eq!(used_of_total(287 * GIB, 840 * GIB), "287/840G");
        assert_eq!(used_of_total(400 * GIB, 6963 * GIB), "0.4/6.8T");
        assert_eq!(used_of_total(12 * 1024 * GIB, 20 * 1024 * GIB), "12/20T");
        assert_eq!(used_of_total(0, 0), "0/0G");
    }
}
