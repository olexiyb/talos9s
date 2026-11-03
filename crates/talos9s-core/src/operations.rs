//! Operation runs: what was run, where, when and how it ended, and its output
//!
//! A run is one file under `~/.talos9s/operations/`, written as it goes so a
//! crash keeps what happened:
//!
//! ```text
//! #talos9s-run {"context":"bc","target":"bc2","nodes":["bc2"],...}
//! 2026-11-03T10:42:07.120Z cordon bc2
//! 2026-11-03T10:42:09.004Z evicting web/gateway-7f9c-x2
//! #end {"finished":"2026-11-03T10:45:19Z","status":"Ok","message":null}
//! ```
//!
//! The header and the end line are JSON, so any text escapes; an output line
//! is a time and one line of text. A file without an end line is a run still
//! going in this process, or one that was interrupted. Reading and writing the
//! files is the TUI's; this module formats, parses, filters and sorts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const HEADER: &str = "#talos9s-run ";
const END: &str = "#end ";

/// How a run ended, or that it hasn't
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    Ok,
    Failed,
    Cancelled,
    /// No end line and not running here: the app quit or was killed
    Interrupted,
}

impl RunStatus {
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

/// What the first line of a run file holds
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunHeader {
    /// talosconfig context
    pub context: String,
    /// The tree row it ran on: a node, `control-plane`, `workers` or the
    /// cluster
    pub target: String,
    /// The nodes it touches, in order
    pub nodes: Vec<String>,
    /// `Drain`, `Reboot`, ...
    pub operation: String,
    /// The equivalent commands a user would type
    pub commands: Vec<String>,
    pub started: DateTime<Utc>,
}

/// The last line of a finished run
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnd {
    pub finished: DateTime<Utc>,
    pub status: RunStatus,
    /// Why it failed or was cancelled, or a summary
    pub message: Option<String>,
}

/// A run as listed: its header, end and output
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub header: RunHeader,
    pub end: Option<RunEnd>,
    pub lines: Vec<(DateTime<Utc>, String)>,
}

impl Run {
    /// Its status: an unfinished run is running only if this process runs it
    pub fn status(&self, running_here: bool) -> RunStatus {
        match &self.end {
            Some(end) => end.status,
            None if running_here => RunStatus::Running,
            None => RunStatus::Interrupted,
        }
    }

    /// From start to end, or to the last output line when unfinished
    pub fn took(&self) -> chrono::Duration {
        let last = self
            .end
            .as_ref()
            .map(|e| e.finished)
            .or_else(|| self.lines.last().map(|(t, _)| *t))
            .unwrap_or(self.header.started);
        last - self.header.started
    }
}

/// The header line, with its newline
pub fn format_header(header: &RunHeader) -> String {
    format!("{HEADER}{}\n", json(header))
}

/// An output line, with its newline; line breaks in the text become spaces
/// so one line stays one line
pub fn format_line(time: DateTime<Utc>, text: &str) -> String {
    let text: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!(
        "{} {text}\n",
        time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    )
}

/// The end line, with its newline
pub fn format_end(end: &RunEnd) -> String {
    format!("{END}{}\n", json(end))
}

fn json<T: Serialize>(value: &T) -> String {
    // Serializing plain structs of strings and times can't fail
    serde_json::to_string(value).unwrap_or_default()
}

/// A run file's text; none when the first line isn't a run header. Output
/// lines that don't parse are kept with the start time, so nothing is lost.
pub fn parse_run(text: &str) -> Option<Run> {
    let mut lines = text.lines();
    let header: RunHeader = serde_json::from_str(lines.next()?.strip_prefix(HEADER)?).ok()?;
    let mut run = Run {
        header,
        end: None,
        lines: Vec::new(),
    };
    for line in lines {
        if let Some(end) = line.strip_prefix(END) {
            run.end = serde_json::from_str(end).ok();
            continue;
        }
        let parsed = line
            .split_once(' ')
            .and_then(|(t, rest)| Some((DateTime::parse_from_rfc3339(t).ok()?, rest)));
        run.lines.push(match parsed {
            Some((t, rest)) => (t.with_timezone(&Utc), rest.to_string()),
            None => (run.header.started, line.to_string()),
        });
    }
    Some(run)
}

/// `20261103-104207-bc-reboot-bc2.log`: sorts by time, safe on any file
/// system
pub fn file_name(header: &RunHeader) -> String {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!(
        "{}-{}-{}-{}.log",
        header.started.format("%Y%m%d-%H%M%S"),
        safe(&header.context),
        safe(&header.operation),
        safe(&header.target),
    )
}

/// The runs to list, newest first: in `context` (none: every context) and
/// matching `filter` (lower case) on node, target, operation, status or a
/// command. `running` says which unfinished runs are running here.
pub fn run_rows(
    runs: &[Run],
    context: Option<&str>,
    filter: &str,
    running: impl Fn(&Run) -> bool,
) -> Vec<usize> {
    let mut rows: Vec<(usize, DateTime<Utc>)> = runs
        .iter()
        .enumerate()
        .filter(|(_, run)| {
            let h = &run.header;
            if context.is_some_and(|c| c != h.context) {
                return false;
            }
            if filter.is_empty() {
                return true;
            }
            let has = |s: &str| s.to_lowercase().contains(filter);
            has(&h.context)
                || has(&h.target)
                || has(&h.operation)
                || has(run.status(running(run)).name())
                || h.nodes.iter().any(|n| has(n))
                || h.commands.iter().any(|c| has(c))
        })
        .map(|(i, run)| (i, run.header.started))
        .collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.1));
    rows.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn header(context: &str, op: &str, target: &str, started: &str) -> RunHeader {
        RunHeader {
            context: context.into(),
            target: target.into(),
            nodes: vec![target.into()],
            operation: op.into(),
            commands: vec![format!("talosctl -n {target} {}", op.to_lowercase())],
            started: time(started),
        }
    }

    /// Header, lines and end round trip; a line break can't forge a line
    #[test]
    fn round_trips_a_run() {
        let h = header("bc", "Reboot", "bc2", "2026-11-03T10:42:07Z");
        let mut text = format_header(&h);
        text += &format_line(time("2026-11-03T10:42:08Z"), "cordon bc2");
        text += &format_line(time("2026-11-03T10:42:09Z"), "two\n#end {\"x\":1}");
        let end = RunEnd {
            finished: time("2026-11-03T10:45:19Z"),
            status: RunStatus::Ok,
            message: Some("done \"quoted\"".into()),
        };
        text += &format_end(&end);
        let run = parse_run(&text).unwrap();
        assert_eq!(run.header, h);
        assert_eq!(run.end, Some(end));
        assert_eq!(run.lines.len(), 2);
        assert_eq!(run.lines[1].1, "two #end {\"x\":1}");
        assert_eq!(run.took().num_seconds(), 192);
        assert_eq!(run.status(false), RunStatus::Ok);
        assert!(parse_run("not a run\n").is_none());
    }

    /// Without an end line: running if this process runs it, else
    /// interrupted; took counts to the last line
    #[test]
    fn unfinished_runs() {
        let h = header("bc", "Drain", "bc1", "2026-11-03T10:00:00Z");
        let text = format_header(&h) + &format_line(time("2026-11-03T10:00:30Z"), "evicting");
        let run = parse_run(&text).unwrap();
        assert_eq!(run.status(true), RunStatus::Running);
        assert_eq!(run.status(false), RunStatus::Interrupted);
        assert_eq!(run.took().num_seconds(), 30);
    }

    #[test]
    fn file_names_sort_and_are_safe() {
        let h = header(
            "admin@bc",
            "Reboot",
            "control-plane",
            "2026-11-03T10:42:07Z",
        );
        assert_eq!(
            file_name(&h),
            "20261103-104207-admin_bc-reboot-control-plane.log"
        );
    }

    /// Newest first; context and filter narrow, the filter matching node,
    /// operation, status and command
    #[test]
    fn filters_and_sorts_runs() {
        let run = |h: RunHeader, status: Option<RunStatus>| Run {
            end: status.map(|status| RunEnd {
                finished: h.started,
                status,
                message: None,
            }),
            header: h,
            lines: Vec::new(),
        };
        let runs = vec![
            run(
                header("bc", "Drain", "bc1", "2026-11-01T10:00:00Z"),
                Some(RunStatus::Ok),
            ),
            run(
                header("bc", "Reboot", "bc2", "2026-11-03T10:00:00Z"),
                Some(RunStatus::Failed),
            ),
            run(
                header("kprod", "Cordon", "kpw1", "2026-11-02T10:00:00Z"),
                None,
            ),
        ];
        let none = |_: &Run| false;
        assert_eq!(run_rows(&runs, None, "", none), [1, 2, 0]);
        assert_eq!(run_rows(&runs, Some("bc"), "", none), [1, 0]);
        assert_eq!(run_rows(&runs, None, "failed", none), [1]);
        assert_eq!(run_rows(&runs, None, "interrupted", none), [2]);
        assert_eq!(run_rows(&runs, None, "running", |_| true), [2]);
        assert_eq!(run_rows(&runs, None, "kpw1", none), [2]);
        assert_eq!(run_rows(&runs, None, "talosctl -n bc1", none), [0]);
    }
}
