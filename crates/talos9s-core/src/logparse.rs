//! Parsing of Talos service and kernel log lines: timestamp, level and
//! message, and the text search the logs view highlights. Pure: no
//! terminal, no streams.
// Every slice here is at a position `find` returned on the same string or
// right after ASCII just checked, so it can't split a character.
#![allow(clippy::string_slice)]

/// Level of a log line
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Unknown,
}

impl LogLevel {
    /// The level a level field or word names
    pub fn named(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "error" | "err" | "fatal" | "panic" | "crit" | "critical" | "alert" | "emerg" => {
                Some(LogLevel::Error)
            }
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" | "notice" => Some(LogLevel::Info),
            "debug" | "trace" => Some(LogLevel::Debug),
            _ => None,
        }
    }

    /// Level of a log line. A level the line states (klog prefix,
    /// `level=`, JSON `"level"`, a leading level word) wins over words in
    /// the message: `level=info msg="retrying after error"` is Info. Without
    /// one, whole words count, so "stderr" or "deferred" are not errors.
    /// `rest` is the line after its timestamp.
    pub fn of_line(line: &str, rest: &str) -> Self {
        // klog: I0109 16:42:01.123456 ...
        let bytes = line.as_bytes();
        if bytes.len() > 6 && bytes[1..5].iter().all(u8::is_ascii_digit) && bytes[5] == b' ' {
            match bytes[0] {
                b'I' => return LogLevel::Info,
                b'W' => return LogLevel::Warn,
                b'E' | b'F' => return LogLevel::Error,
                _ => {}
            }
        }

        // level=info, lvl=warn, "level":"error", "severity":"INFO"
        for key in ["level", "lvl", "severity"] {
            for pattern in [format!("{key}="), format!("\"{key}\":")] {
                if let Some(pos) = line.find(&pattern) {
                    let value = line[pos + pattern.len()..]
                        .trim_start()
                        .trim_start_matches('"');
                    let end = value
                        .find(|c: char| !c.is_ascii_alphabetic())
                        .unwrap_or(value.len());
                    if let Some(level) = Self::named(&value[..end]) {
                        return level;
                    }
                }
            }
        }

        let mut words = rest
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty() && !w.bytes().all(|b| b.is_ascii_digit()));
        // A leading level word: "INFO msg", "[warn] msg", "info\tmsg"
        if let Some(level) = words.next().and_then(Self::named) {
            return level;
        }
        let mut found = LogLevel::Unknown;
        for word in line.split(|c: char| !c.is_ascii_alphanumeric()) {
            match Self::named(word) {
                Some(LogLevel::Error) => return LogLevel::Error,
                Some(LogLevel::Warn) => found = LogLevel::Warn,
                Some(level) if found == LogLevel::Unknown => found = level,
                _ => {}
            }
        }
        found
    }

    pub fn badge(&self) -> &'static str {
        match self {
            LogLevel::Error => "ERR",
            LogLevel::Warn => "WRN",
            LogLevel::Info => "INF",
            LogLevel::Debug => "DBG",
            LogLevel::Unknown => "---",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            LogLevel::Error => "Error",
            LogLevel::Warn => "Warn",
            LogLevel::Info => "Info",
            LogLevel::Debug => "Debug",
            LogLevel::Unknown => "Other",
        }
    }
}

/// First of `total` entries to draw so the last one ends at the bottom of a
/// pane `height` rows high, when entry `i` takes `rows(i)` rows
pub fn bottom_start(total: usize, height: usize, rows: impl Fn(usize) -> usize) -> usize {
    let mut used = 0;
    for i in (0..total).rev() {
        used += rows(i);
        if used >= height {
            return i;
        }
    }
    0
}

/// Byte ranges of `text` matching `query` case-insensitively, without
/// overlap. Compared char by char in `text` itself: offsets found in a
/// lowercased copy don't fit the original when lowercasing changes a
/// char's length (`İ`, the Kelvin sign).
pub fn match_ranges(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut next_start = 0;
    for (start, _) in text.char_indices() {
        if start < next_start {
            continue;
        }
        let mut wanted = query.iter();
        let mut end = start;
        let mut matched = false;
        for (i, c) in text[start..].char_indices() {
            let lower: Vec<char> = c.to_lowercase().collect();
            if !lower.iter().all(|l| wanted.next() == Some(l)) {
                break;
            }
            end = start + i + c.len_utf8();
            if wanted.as_slice().is_empty() {
                matched = true;
                break;
            }
        }
        if matched {
            ranges.push(start..end);
            next_start = end;
        }
    }
    ranges
}

/// A log line split into what the logs view shows
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedLine {
    /// `HH:MM:SS` (or empty when the line has no time)
    pub timestamp: String,
    /// Seconds since the epoch where the line has a date, for ordering
    pub timestamp_sort: i64,
    pub level: LogLevel,
    pub message: String,
}

/// Parse one line; `kernel` for the kernel log's `kern: info: [ts]: msg`
pub fn parse(line: &str, kernel: bool) -> ParsedLine {
    let line = line.trim();
    if kernel && let Some((ts, level, message)) = parse_kernel_line(line) {
        return ParsedLine {
            timestamp: extract_time_part(ts),
            timestamp_sort: time_to_sort_key(ts),
            level,
            message,
        };
    }
    let (timestamp, timestamp_sort, rest) = extract_timestamp(line);
    // The whole line: klog's level letter is part of its timestamp
    let level = LogLevel::of_line(line, rest);
    ParsedLine {
        timestamp,
        timestamp_sort,
        level,
        message: clean_message(rest),
    }
}

/// Extract timestamp from line - handles multiple formats
pub fn extract_timestamp(line: &str) -> (String, i64, &str) {
    // Try klog format first: I0109 16:42:01.123456 or W0109 16:42:01.123456
    // Format: [IWEF]MMDD HH:MM:SS.microseconds
    if line.len() > 20 {
        let first_char = line.chars().next().unwrap_or(' ');
        if matches!(first_char, 'I' | 'W' | 'E' | 'F')
            && let Some(space_pos) = line[1..].find(' ')
        {
            let date_part = &line[1..space_pos + 1]; // MMDD
            if date_part.len() == 4 && date_part.chars().all(|c| c.is_ascii_digit()) {
                // Find the time part after the space
                let after_date = &line[space_pos + 2..];
                if let Some(time_end) =
                    after_date.find(|c: char| !c.is_ascii_digit() && c != ':' && c != '.')
                {
                    let time_part = &after_date[..time_end];
                    if time_part.contains(':') {
                        let rest_start = space_pos + 2 + time_end;
                        let rest = line[rest_start..].trim();
                        let short_ts = extract_time_part(time_part);
                        let sort_key = klog_sort_key(date_part, time_part, chrono::Utc::now())
                            .unwrap_or_else(|| time_to_sort_key(time_part));
                        return (short_ts, sort_key, rest);
                    }
                }
            }
        }
    }

    // Try JSON format with numeric timestamp: {"ts":1767972610784.5803,...}
    if line.starts_with('{') {
        // Try numeric "ts" field first (Unix timestamp in ms)
        if let Some(ts_start) = line.find("\"ts\":") {
            let ts_value_start = ts_start + 5;
            let rest = &line[ts_value_start..];
            // Find end of number (comma, space, or closing brace)
            if let Some(ts_end) = rest.find([',', '}', ' ']) {
                let ts_str = rest[..ts_end].trim();
                // Try to parse as float (ms timestamp)
                if let Ok(ts_ms) = ts_str.parse::<f64>() {
                    let (short_ts, sort_key) = unix_ts_to_time(ts_ms);
                    return (short_ts, sort_key, line);
                }
                // Try as quoted string
                if let Some(stripped) = ts_str.strip_prefix('"')
                    && let Some(end_quote) = stripped.find('"')
                {
                    let ts = &stripped[..end_quote];
                    let short_ts = extract_time_part(ts);
                    // Pass the full timestamp for sorting to preserve date
                    let sort_key = time_to_sort_key(ts);
                    return (short_ts, sort_key, line);
                }
            }
        }
        // Try "time" field as string
        if let Some(ts_start) = line.find("\"time\":\"") {
            let ts_value_start = ts_start + 8;
            if let Some(ts_end) = line[ts_value_start..].find('"') {
                let ts = &line[ts_value_start..ts_value_start + ts_end];
                let short_ts = extract_time_part(ts);
                // Pass the full timestamp for sorting to preserve date
                let sort_key = time_to_sort_key(ts);
                return (short_ts, sort_key, line);
            }
        }
    }

    // Try containerd format: time="2024-01-09T16:42:01.123456789Z" level=info msg="..."
    if let Some(ts_start) = line.find("time=\"") {
        let ts_value_start = ts_start + 6;
        if let Some(ts_end) = line[ts_value_start..].find('"') {
            let ts = &line[ts_value_start..ts_value_start + ts_end];
            let short_ts = extract_time_part(ts);
            // Pass the full timestamp for sorting to preserve date
            let sort_key = time_to_sort_key(ts);
            return (short_ts, sort_key, line);
        }
    }

    // Try ISO 8601 or standard date format at start
    // 2024-01-09T16:42:01 or 2024/01/09 16:42:01
    let chars: Vec<char> = line.chars().collect();
    let mut end = 0;
    let mut has_colon = false;

    for (i, c) in chars.iter().enumerate() {
        if *c == ':' {
            has_colon = true;
        }

        if c.is_ascii_digit()
            || *c == '/'
            || *c == '-'
            || *c == ':'
            || *c == '.'
            || *c == 'T'
            || *c == 'Z'
        {
            end = i + 1;
        } else if *c == ' ' {
            // Allow space between date and time (2024-01-09 16:42:01)
            if let Some(next) = chars.get(i + 1)
                && next.is_ascii_digit()
            {
                end = i + 1;
                continue;
            }
            break;
        } else {
            break;
        }
    }

    if end >= 8 && has_colon && end < line.len() {
        let ts = line[..end].trim();
        let rest = line[end..].trim();
        let short_ts = extract_time_part(ts);
        // Reject 00:00:00 - it's likely Go's zero time from event dumps
        if short_ts == "00:00:00" {
            return (String::new(), 0, line);
        }
        // Pass the full timestamp for sorting to preserve date
        let sort_key = time_to_sort_key(ts);
        (short_ts, sort_key, rest)
    } else {
        // No timestamp found - don't search the whole line as that matches
        // embedded times in log content (Go zero times, SHA hashes, etc.)
        (String::new(), 0, line)
    }
}

/// Extract HH:MM:SS from any timestamp format
/// Only searches within the first portion of the string to avoid matching
/// time-like patterns in content (e.g., sha256:70... would match as 56:70)
pub fn extract_time_part(ts: &str) -> String {
    let bytes = ts.as_bytes();
    // Only search the first 30 chars to avoid matching patterns in log content
    let search_len = bytes.len().min(30);

    // Look for HH:MM:SS pattern (must have two colons)
    for i in 0..search_len.saturating_sub(7) {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2] == b':'
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4].is_ascii_digit()
            && bytes[i + 5] == b':'
            && bytes[i + 6].is_ascii_digit()
            && bytes[i + 7].is_ascii_digit()
        {
            // Validate it's a real time (HH < 24, MM < 60, SS < 60)
            let hh = (bytes[i] - b'0') * 10 + (bytes[i + 1] - b'0');
            let mm = (bytes[i + 3] - b'0') * 10 + (bytes[i + 4] - b'0');
            let ss = (bytes[i + 6] - b'0') * 10 + (bytes[i + 7] - b'0');
            if hh < 24 && mm < 60 && ss < 60 {
                let result = ts[i..i + 8].to_string();
                // Reject 00:00:00 - likely Go's zero time from event dumps
                if result == "00:00:00" {
                    continue;
                }
                return result;
            }
        }
    }
    // Fallback: look for HH:MM pattern at the start only (first 20 chars)
    let fallback_len = bytes.len().min(20);
    for i in 0..fallback_len.saturating_sub(4) {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2] == b':'
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4].is_ascii_digit()
        {
            // Validate it's a real time (HH < 24, MM < 60)
            let hh = (bytes[i] - b'0') * 10 + (bytes[i + 1] - b'0');
            let mm = (bytes[i + 3] - b'0') * 10 + (bytes[i + 4] - b'0');
            if hh < 24 && mm < 60 {
                let result = ts[i..i + 5].to_string();
                // Reject 00:00 - likely Go's zero time
                if result == "00:00" {
                    continue;
                }
                return result;
            }
        }
    }
    String::new()
}

/// Convert a full timestamp string to a Unix timestamp (seconds since epoch)
/// Handles RFC 3339 with an offset, ISO 8601 and slash dates without one
/// (taken as UTC), and falls back to time-only (HHMMSS) if no date is
/// found. Invalid dates (month 13) fall back too, never panic.
pub fn time_to_sort_key(ts: &str) -> i64 {
    use chrono::{DateTime, NaiveDateTime};
    let ts = ts.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(ts) {
        return t.timestamp();
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y/%m/%d %H:%M:%S%.f",
    ] {
        if let Ok((t, _)) = NaiveDateTime::parse_and_remainder(ts, format) {
            return t.and_utc().timestamp();
        }
    }

    // Fallback: extract just time digits from HH:MM:SS or HH:MM format
    // This won't sort correctly across days, but it's the best we can do
    let digits: String = ts.chars().filter(char::is_ascii_digit).take(6).collect();
    digits.parse().unwrap_or(0)
}

/// Sort key of a klog timestamp (`MMDD` and `HH:MM:SS.ffffff`). klog has
/// no year: the current one, or last year for a date that would be
/// ahead of `now` (a December line read in January). `None` for an
/// invalid date.
pub fn klog_sort_key(mmdd: &str, time: &str, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    use chrono::{Datelike, NaiveDate, NaiveTime};
    let month: u32 = mmdd.get(0..2)?.parse().ok()?;
    let day: u32 = mmdd.get(2..4)?.parse().ok()?;
    let time = NaiveTime::parse_from_str(time, "%H:%M:%S%.f").ok()?;
    let at = |year| {
        Some(
            NaiveDate::from_ymd_opt(year, month, day)?
                .and_time(time)
                .and_utc(),
        )
    };
    let this_year = at(now.year())?;
    let t = if this_year > now + chrono::Duration::days(1) {
        at(now.year() - 1)?
    } else {
        this_year
    };
    Some(t.timestamp())
}

/// Convert Unix timestamp to (HH:MM:SS, sort_key)
/// Auto-detects whether timestamp is in seconds or milliseconds
/// Sort key uses the full Unix timestamp to preserve date ordering
pub fn unix_ts_to_time(ts: f64) -> (String, i64) {
    // Auto-detect: timestamps < 10^12 are likely seconds, >= 10^12 are milliseconds
    // Current Unix time in seconds is ~1.7 billion (2024), in ms it's ~1.7 trillion
    let ts_secs = if ts >= 1_000_000_000_000.0 {
        // Milliseconds
        (ts / 1000.0) as i64
    } else {
        // Seconds (with possible fractional part)
        ts as i64
    };

    let secs_in_day = ts_secs % 86400;
    let hours = secs_in_day / 3600;
    let minutes = (secs_in_day % 3600) / 60;
    let seconds = secs_in_day % 60;
    let short_ts = format!("{hours:02}:{minutes:02}:{seconds:02}");
    // Use the full Unix timestamp as the sort key to preserve date ordering
    let sort_key = ts_secs;
    (short_ts, sort_key)
}

/// Clean message text
pub fn clean_message(text: &str) -> String {
    let text = text.trim();
    let text = if let Some(pos) = text.find(": ") {
        if pos < 20 {
            text[pos + 2..].trim()
        } else {
            text
        }
    } else {
        text
    };
    text.trim_start_matches("[INFO]")
        .trim_start_matches("[WARN]")
        .trim_start_matches("[ERROR]")
        .trim_start_matches("[DEBUG]")
        .trim_start_matches("INFO")
        .trim_start_matches("WARN")
        .trim_start_matches("ERROR")
        .trim_start_matches("DEBUG")
        .trim_start_matches("OK")
        .trim()
        .to_string()
}

/// Split a Talos kernel log line, `kern:    info: [2026-10-19T08:20:59.58Z]: text`,
/// into its timestamp, level and message (prefixed with the facility when it
/// isn't `kern`)
pub fn parse_kernel_line(line: &str) -> Option<(&str, LogLevel, String)> {
    let (facility, rest) = line.split_once(':')?;
    let (priority, rest) = rest.split_once(':')?;
    let rest = rest.trim_start().strip_prefix('[')?;
    let (ts, message) = rest.split_once("]:")?;
    let level = match priority.trim() {
        "emerg" | "alert" | "crit" | "err" => LogLevel::Error,
        "warning" => LogLevel::Warn,
        "notice" | "info" => LogLevel::Info,
        "debug" => LogLevel::Debug,
        _ => LogLevel::Unknown,
    };
    let message = message.trim();
    let message = match facility.trim() {
        "kern" => message.to_string(),
        other => format!("{other}: {message}"),
    };
    Some((ts, level, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_start_counts_wrapped_rows() {
        // One row each: the last 5 of 10 fill a 5-row pane
        assert_eq!(bottom_start(10, 5, |_| 1), 5);
        // The newest entry wraps to 3 rows: only 3 entries fit
        assert_eq!(bottom_start(10, 5, |i| if i == 9 { 3 } else { 1 }), 7);
        // One entry taller than the pane
        assert_eq!(bottom_start(10, 5, |i| if i == 9 { 8 } else { 1 }), 9);
        assert_eq!(bottom_start(2, 5, |_| 1), 0);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn search_matches_fit_the_original_text() {
        assert_eq!(match_ranges("Error and ERROR", "error"), [0..5, 10..15]);
        assert_eq!(match_ranges("aaa", "aa"), [0..2]);
        assert!(match_ranges("abc", "").is_empty());
        // Kelvin sign (3 bytes) lowercases to a 1-byte 'k'
        let text = "\u{212A}elvin kelvin";
        let ranges = match_ranges(text, "kelvin");
        assert_eq!(ranges.len(), 2);
        for r in ranges {
            let _ = &text[r];
        }
        // Lowercasing İ gives two chars
        assert_eq!(match_ranges("İstanbul", "i̇stanbul").len(), 1);
        assert_eq!(match_ranges("ünïcödé", "CÖD"), [5..9]);
    }

    #[test]
    fn timestamps_never_panic_and_respect_offsets() {
        let key = time_to_sort_key;
        assert_eq!(key("2026-11-01T10:00:00Z"), 1_793_527_200);
        assert_eq!(key("2026-11-01T12:00:00+02:00"), 1_793_527_200);
        assert_eq!(key("2026-11-01T10:00:00.123456789Z"), 1_793_527_200);
        assert_eq!(key("2026/11/01 10:00:00"), 1_793_527_200);
        // Invalid dates and non-ASCII text fall back instead of panicking
        let _ = key("2026-99-01 12:00:00");
        let _ = key("2026-ab-ü1 12:00:00");
        let _ = key("ééééééééééééééééééééé");
        for line in [
            "2026-99-01 12:00:00 x",
            "E9999 12:00:00.123456    1 x.go:1] bad month",
            "I0230 12:00:00.123456    1 x.go:1] Feb 30",
            "2026-11-01T10:00:00Zü message",
            "ü2026-11-01 10:00:00 message",
        ] {
            let _ = parse(line, false);
        }
    }

    #[test]
    fn klog_dates_take_the_year_they_were_written() {
        use chrono::TimeZone;
        let jan_2 = chrono::Utc.with_ymd_and_hms(2027, 1, 2, 0, 0, 0).unwrap();
        let dec_31 = klog_sort_key("1231", "23:59:00.5", jan_2).unwrap();
        let expected = chrono::Utc
            .with_ymd_and_hms(2026, 12, 31, 23, 59, 0)
            .unwrap();
        assert_eq!(dec_31, expected.timestamp());
        let jan_1 = klog_sort_key("0101", "10:00:00", jan_2).unwrap();
        assert!(jan_1 > dec_31);
        assert_eq!(klog_sort_key("1301", "10:00:00", jan_2), None);
    }

    #[test]
    fn log_level_prefers_the_stated_level() {
        let level = |line: &str| {
            let (_, _, rest) = extract_timestamp(line);
            LogLevel::of_line(line, rest)
        };
        // Words inside other words are not levels
        assert_eq!(level("copying stderr to file"), LogLevel::Unknown);
        assert_eq!(level("deferred cleanup done"), LogLevel::Unknown);
        // A stated level wins over words in the message
        assert_eq!(
            level(r#"time="2026-11-01T10:00:00Z" level=info msg="retry after error""#),
            LogLevel::Info
        );
        assert_eq!(
            level(r#"{"level":"info","msg":"error budget ok"}"#),
            LogLevel::Info
        );
        assert_eq!(level(r#"{"severity":"WARNING","msg":"x"}"#), LogLevel::Warn);
        assert_eq!(
            level("I1101 10:00:00.123456    1 reflector.go:1] error watching"),
            LogLevel::Info
        );
        assert_eq!(
            level("E1101 10:00:00.123456    1 server.go:1] failed"),
            LogLevel::Error
        );
        assert_eq!(
            level("2026-11-01T10:00:00.000Z\tinfo\tthe error count is 0"),
            LogLevel::Info
        );
        assert_eq!(level("[WARN] disk almost full"), LogLevel::Warn);
        // Without a stated level, whole words count
        assert_eq!(level("connection error, retrying"), LogLevel::Error);
        assert_eq!(level("warning: clock skew"), LogLevel::Warn);
    }

    #[test]
    fn parses_kernel_lines() {
        let (ts, level, message) = parse_kernel_line(
            "kern:    info: [2026-10-19T08:20:59.588932729Z]: drbd pvc-1 bc2: conn( Connecting -> Connected )",
        )
        .unwrap();
        assert_eq!(ts, "2026-10-19T08:20:59.588932729Z");
        assert_eq!(level, LogLevel::Info);
        assert_eq!(message, "drbd pvc-1 bc2: conn( Connecting -> Connected )");

        let (_, level, message) =
            parse_kernel_line("daemon: warning: [2026-10-19T08:21:00Z]: low on entropy").unwrap();
        assert_eq!(level, LogLevel::Warn);
        assert_eq!(message, "daemon: low on entropy");
        let (_, level, _) =
            parse_kernel_line("kern:     err: [2026-10-19T08:21:00Z]: I/O error").unwrap();
        assert_eq!(level, LogLevel::Error);
        assert!(parse_kernel_line("not a kernel line").is_none());

        let entry = parse(
            "kern:    info: [2026-10-19T08:20:59.588932729Z]: eth0: renamed",
            true,
        );
        assert_eq!(entry.timestamp, "08:20:59");
        assert_eq!(entry.message, "eth0: renamed");
    }
}
