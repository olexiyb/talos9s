//! Release versions and the new-release check schedule

/// `latest` is a newer release than `current` (versions as `X.Y.Z`; anything
/// else counts as not newer, so an odd tag never nags)
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse(current), parse(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim_start_matches('v').split('.');
    let mut next = || parts.next()?.parse().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// No answer yet (`checked_at` is `None`), or the last one is at least
/// `interval` old; `now` and `checked_at` are seconds since the Unix epoch
pub fn check_due(checked_at: Option<u64>, now: u64, interval: std::time::Duration) -> bool {
    checked_at.is_none_or(|at| now.saturating_sub(at) >= interval.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_release_versions() {
        assert!(is_newer("0.2.0", "0.3.0"));
        assert!(is_newer("0.2.0", "v0.2.1"));
        assert!(is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.2.9"));
        // Pre-releases and odd tags never count
        assert!(!is_newer("0.2.0", "0.3.0-rc1"));
        assert!(!is_newer("0.2.0", "nightly"));
    }

    #[test]
    fn check_is_due_after_the_interval() {
        let day = std::time::Duration::from_secs(86_400);
        assert!(check_due(None, 1_000, day));
        assert!(!check_due(Some(1_000), 1_000 + 86_399, day));
        assert!(check_due(Some(1_000), 1_000 + 86_400, day));
        // A clock that went back is not due
        assert!(!check_due(Some(2_000), 1_000, day));
    }
}
