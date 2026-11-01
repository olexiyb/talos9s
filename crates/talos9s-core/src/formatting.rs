//! Formatting utilities for consistent display across the application
//!
//! Provides functions for formatting bytes, durations, percentages,
//! and other common data types for user display.

use chrono::Duration;

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
    format_secs_human(duration.num_seconds().unsigned_abs())
}

/// [`format_duration_human`] for a number of seconds: `45s`, `3m`, `2h`, `5d`
///
/// ```
/// assert_eq!(talos9s_core::format_secs_human(98), "1m");
/// ```
pub fn format_secs_human(secs: u64) -> String {
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

/// Truncate a string to a maximum length with ellipsis
///
/// # Examples
///
/// ```
/// use talos9s_core::formatting::truncate_string;
///
/// assert_eq!(truncate_string("Hello, World!", 10), "Hello, ...");
/// assert_eq!(truncate_string("Short", 10), "Short");
/// // Counts chars, never cuts one in half
/// assert_eq!(truncate_string("ééééé", 4), "é...");
/// ```
pub fn truncate_string(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        s.chars().take(max_len).collect()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}

/// Greedy word wrap: `words` joined by single spaces into rows of at most
/// `width` chars. A word longer than `width` gets a row of its own.
///
/// ```
/// use talos9s_core::wrap_words;
/// assert_eq!(wrap_words("a bb ccc".split(' '), 4), ["a bb", "ccc"]);
/// assert_eq!(wrap_words(["toolongword"], 4), ["toolongword"]);
/// ```
pub fn wrap_words<'a>(words: impl IntoIterator<Item = &'a str>, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in words {
        if !row.is_empty() && row.chars().count() + 1 + word.chars().count() > width {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
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
}
