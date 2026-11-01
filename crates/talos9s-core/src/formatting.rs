//! Formatting utilities for consistent display across the application
//!
//! Provides functions for formatting bytes, durations, percentages,
//! and other common data types for user display.

use chrono::{DateTime, Duration, Utc};

// Byte size constants
const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const GB: u64 = MB * 1024;
const TB: u64 = GB * 1024;

/// Format bytes into a human-readable string
///
/// Uses binary units (KiB-style but labeled as KB for familiarity).
///
/// # Examples
///
/// ```
/// use talos9s_core::formatting::format_bytes;
///
/// assert_eq!(format_bytes(500), "500 B");
/// assert_eq!(format_bytes(1536), "1.5 KB");
/// assert_eq!(format_bytes(1_572_864), "1.5 MB");
/// ```
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format signed bytes (for etcd and other APIs that use i64)
///
/// Handles negative values by showing "0 B".
pub fn format_bytes_signed(bytes: i64) -> String {
    if bytes < 0 {
        "0 B".to_string()
    } else {
        format_bytes(bytes as u64)
    }
}

/// Format bytes with automatic precision
///
/// Uses 0 decimal places for values >= 10, 1 decimal for smaller.
pub fn format_bytes_compact(bytes: u64) -> String {
    if bytes >= TB {
        let val = bytes as f64 / TB as f64;
        if val >= 10.0 {
            format!("{:.0}TB", val)
        } else {
            format!("{:.1}TB", val)
        }
    } else if bytes >= GB {
        let val = bytes as f64 / GB as f64;
        if val >= 10.0 {
            format!("{:.0}GB", val)
        } else {
            format!("{:.1}GB", val)
        }
    } else if bytes >= MB {
        let val = bytes as f64 / MB as f64;
        if val >= 10.0 {
            format!("{:.0}MB", val)
        } else {
            format!("{:.1}MB", val)
        }
    } else if bytes >= KB {
        let val = bytes as f64 / KB as f64;
        if val >= 10.0 {
            format!("{:.0}KB", val)
        } else {
            format!("{:.1}KB", val)
        }
    } else {
        format!("{}B", bytes)
    }
}

/// Format a percentage value
///
/// # Examples
///
/// ```
/// use talos9s_core::formatting::format_percent;
///
/// assert_eq!(format_percent(75.5), "75.5%");
/// assert_eq!(format_percent(100.0), "100.0%");
/// ```
pub fn format_percent(value: f64) -> String {
    format!("{:.1}%", value)
}

/// Calculate and format percentage from used/total values
///
/// # Examples
///
/// ```
/// use talos9s_core::formatting::format_usage_percent;
///
/// assert_eq!(format_usage_percent(50, 100), "50.0%");
/// assert_eq!(format_usage_percent(0, 0), "0.0%");
/// ```
pub fn format_usage_percent(used: u64, total: u64) -> String {
    if total == 0 {
        "0.0%".to_string()
    } else {
        format_percent(used as f64 / total as f64 * 100.0)
    }
}

/// Format a duration in a human-readable way
///
/// Chooses the most appropriate unit for the duration.
///
/// # Examples
///
/// ```
/// use talos9s_core::formatting::format_duration_human;
/// use chrono::Duration;
///
/// assert_eq!(format_duration_human(Duration::seconds(30)), "30s");
/// assert_eq!(format_duration_human(Duration::minutes(5)), "5m");
/// assert_eq!(format_duration_human(Duration::hours(2)), "2h");
/// assert_eq!(format_duration_human(Duration::days(3)), "3d");
/// ```
pub fn format_duration_human(duration: Duration) -> String {
    let secs = duration.num_seconds().abs();

    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Format a duration with more detail
///
/// Shows multiple units for longer durations (e.g., "2d 5h").
pub fn format_duration_detailed(duration: Duration) -> String {
    let total_secs = duration.num_seconds().abs();

    if total_secs < 60 {
        format!("{}s", total_secs)
    } else if total_secs < 3600 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        if secs > 0 {
            format!("{}m {}s", mins, secs)
        } else {
            format!("{}m", mins)
        }
    } else if total_secs < 86400 {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        if mins > 0 {
            format!("{}h {}m", hours, mins)
        } else {
            format!("{}h", hours)
        }
    } else {
        let days = total_secs / 86400;
        let hours = (total_secs % 86400) / 3600;
        if hours > 0 {
            format!("{}d {}h", days, hours)
        } else {
            format!("{}d", days)
        }
    }
}

/// Format time relative to now (e.g., "5 minutes ago")
pub fn format_time_ago(timestamp: DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(timestamp);

    if duration < Duration::zero() {
        "in the future".to_string()
    } else {
        format!("{} ago", format_duration_human(duration))
    }
}

/// Format a timestamp for display
///
/// Shows time only if today, otherwise shows date and time.
pub fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    let now = Utc::now();
    let local = timestamp.with_timezone(&chrono::Local);

    if timestamp.date_naive() == now.date_naive() {
        local.format("%H:%M:%S").to_string()
    } else {
        local.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// Format a timestamp as ISO 8601
pub fn format_timestamp_iso(timestamp: DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Truncate a string to a maximum length with ellipsis
///
/// # Examples
///
/// ```
/// use talos9s_core::formatting::truncate_string;
///
/// assert_eq!(truncate_string("Hello, World!", 10), "Hello, ...");
/// assert_eq!(truncate_string("Short", 10), "Short");
/// ```
pub fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        s.chars().take(max_len).collect()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}

/// Format a count with singular/plural form
///
/// # Examples
///
/// ```
/// use talos9s_core::formatting::pluralize;
///
/// assert_eq!(pluralize(1, "node", "nodes"), "1 node");
/// assert_eq!(pluralize(5, "node", "nodes"), "5 nodes");
/// assert_eq!(pluralize(0, "item", "items"), "0 items");
/// ```
pub fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{} {}", count, singular)
    } else {
        format!("{} {}", count, plural)
    }
}

/// Format a ratio as "X/Y"
pub fn format_ratio(numerator: usize, denominator: usize) -> String {
    format!("{}/{}", numerator, denominator)
}

/// Format a version string, handling common prefixes
pub fn format_version(version: &str) -> String {
    // Remove 'v' prefix if present for consistency
    version.strip_prefix('v').unwrap_or(version).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(1_073_741_824), "1.0 GB");
        assert_eq!(format_bytes(1_099_511_627_776), "1.0 TB");
    }

    #[test]
    fn test_format_bytes_signed() {
        assert_eq!(format_bytes_signed(-100), "0 B");
        assert_eq!(format_bytes_signed(0), "0 B");
        assert_eq!(format_bytes_signed(1024), "1.0 KB");
    }

    #[test]
    fn test_format_bytes_compact() {
        assert_eq!(format_bytes_compact(500), "500B");
        assert_eq!(format_bytes_compact(1536), "1.5KB");
        assert_eq!(format_bytes_compact(15 * 1024), "15KB");
        assert_eq!(format_bytes_compact(1_073_741_824), "1.0GB");
        assert_eq!(format_bytes_compact(15 * 1024 * 1024 * 1024), "15GB");
    }

    #[test]
    fn test_format_percent() {
        assert_eq!(format_percent(0.0), "0.0%");
        assert_eq!(format_percent(50.5), "50.5%");
        assert_eq!(format_percent(100.0), "100.0%");
    }

    #[test]
    fn test_format_usage_percent() {
        assert_eq!(format_usage_percent(50, 100), "50.0%");
        assert_eq!(format_usage_percent(0, 0), "0.0%");
        assert_eq!(format_usage_percent(75, 100), "75.0%");
    }

    #[test]
    fn test_format_duration_human() {
        assert_eq!(format_duration_human(Duration::seconds(30)), "30s");
        assert_eq!(format_duration_human(Duration::seconds(90)), "1m");
        assert_eq!(format_duration_human(Duration::minutes(5)), "5m");
        assert_eq!(format_duration_human(Duration::hours(2)), "2h");
        assert_eq!(format_duration_human(Duration::days(3)), "3d");
    }

    #[test]
    fn test_format_duration_detailed() {
        assert_eq!(format_duration_detailed(Duration::seconds(30)), "30s");
        assert_eq!(format_duration_detailed(Duration::seconds(90)), "1m 30s");
        assert_eq!(format_duration_detailed(Duration::minutes(65)), "1h 5m");
        assert_eq!(format_duration_detailed(Duration::hours(25)), "1d 1h");
    }

    #[test]
    fn test_truncate_string() {
        assert_eq!(truncate_string("Hello, World!", 10), "Hello, ...");
        assert_eq!(truncate_string("Short", 10), "Short");
        assert_eq!(truncate_string("Hi", 2), "Hi");
        assert_eq!(truncate_string("Hello", 3), "Hel");
    }

    #[test]
    fn test_pluralize() {
        assert_eq!(pluralize(0, "node", "nodes"), "0 nodes");
        assert_eq!(pluralize(1, "node", "nodes"), "1 node");
        assert_eq!(pluralize(5, "node", "nodes"), "5 nodes");
    }

    #[test]
    fn test_format_version() {
        assert_eq!(format_version("v1.2.3"), "1.2.3");
        assert_eq!(format_version("1.2.3"), "1.2.3");
    }
}
