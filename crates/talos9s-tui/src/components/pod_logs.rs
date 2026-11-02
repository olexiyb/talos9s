//! The logs pane of the Workloads view's focused view: the merged logs of a
//! workload's pods, a pod's containers, or one container
//!
//! Each source is read once through the Kubernetes API (`kubectl logs
//! --timestamps`), at most `POD_LOG_SOURCES_MAX` at once, together within
//! `WORKLOADS_FETCH_TIMEOUT`; while the pane follows the end it reads again
//! every `POD_LOG_FOLLOW_INTERVAL`. A source that can't be read is counted,
//! the others are shown.

use crossterm::event::KeyEvent;
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client, api::LogParams};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use talos9s_core::constants::{
    POD_LOG_FOLLOW_INTERVAL, POD_LOG_SOURCES_MAX, POD_LOG_TOTAL_LINES, WORKLOADS_FETCH_TIMEOUT,
};
use talos9s_core::logparse::{JsonLine, LogLevel, match_ranges};
use talos9s_core::pod_logs::{LogLine, LogSource, merge_logs, tail_per_source};

/// The selection must rest this long before its logs are read, so walking
/// the list doesn't read every pod on the way
const SETTLE: Duration = Duration::from_millis(300);

/// A line as the pane shows it: its level, and a JSON or logfmt line as
/// its message and fields
#[derive(Debug, Clone)]
struct Shown {
    line: LogLine,
    level: LogLevel,
    message: String,
    /// `key=value ...`, dimmed after the message
    fields: String,
}

impl Shown {
    fn of(line: LogLine) -> Self {
        let json = JsonLine::parse(&line.text);
        let level = json
            .as_ref()
            .and_then(|j| j.level)
            .unwrap_or_else(|| LogLevel::of_line(&line.text, &line.text));
        let (message, fields) = match json {
            Some(j) => (
                j.message,
                j.fields
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            None => (line.text.clone(), String::new()),
        };
        Self {
            line,
            level,
            message,
            fields,
        }
    }

    /// What the search looks in: the source, the message and the fields
    fn searchable(&self) -> String {
        format!("{} {} {}", self.line.source, self.message, self.fields)
    }
}

/// Position of a level in the filter, in the Logs view's order
fn level_index(level: LogLevel) -> usize {
    match level {
        LogLevel::Error => 0,
        LogLevel::Warn => 1,
        LogLevel::Info => 2,
        LogLevel::Debug => 3,
        LogLevel::Unknown => 4,
    }
}

/// What a read gave: the lines and the sources that couldn't be read
#[derive(Debug, Clone)]
struct Read {
    lines: Vec<Shown>,
    failed: Vec<String>,
    /// More sources than `POD_LOG_SOURCES_MAX`: these weren't read
    skipped: usize,
}

type Slot = Arc<Mutex<Option<(u64, Read)>>>;

#[derive(Clone, Default)]
pub struct LogPane {
    /// What the logs are of, for the title
    title: String,
    sources: Vec<LogSource>,
    /// The kubectl command for what is shown, for `y`
    command: String,
    /// The previous run's logs (`--previous`)
    previous: bool,
    read: Option<Read>,
    slot: Slot,
    /// Bumped per read, so a late answer for an old selection is dropped
    generation: u64,
    /// When the sources changed and haven't been read yet
    pending: Option<Instant>,
    last_read: Option<Instant>,
    /// First line shown; `None` follows the end
    scroll: Option<usize>,
    wrap: bool,
    height: usize,
    /// Levels hidden (`1`-`5`, `0`), in `level_index` order
    hidden: [bool; 5],
    /// Typing a search (`/`)
    searching: bool,
    query: String,
    /// The match `n`/`N` went to, a position among the visible lines
    current_match: Option<usize>,
}

impl LogPane {
    /// Show the logs of `sources`; read once the selection rests. The same
    /// sources again keep what is shown.
    pub fn set_sources(&mut self, title: String, sources: Vec<LogSource>, command: String) {
        // The same containers (a pod of one): new title and command only
        self.title = title;
        self.command = command;
        if sources == self.sources {
            return;
        }
        self.sources = sources;
        self.read = None;
        self.scroll = None;
        self.pending = Some(Instant::now());
    }

    pub fn previous(&self) -> bool {
        self.previous
    }

    pub fn set_wrap(&mut self, wrap: bool) {
        self.wrap = wrap;
    }

    pub fn wrap(&self) -> bool {
        self.wrap
    }

    /// The command for what is shown, `--previous` included
    pub fn command(&self) -> String {
        if self.previous {
            format!("{} --previous", self.command)
        } else {
            self.command.clone()
        }
    }

    /// On each tick: read when the selection has rested, or again while
    /// following the end
    pub fn tick(&mut self, client: Option<&Client>) {
        self.poll();
        let due = match (self.pending, self.last_read) {
            (Some(since), _) => since.elapsed() >= SETTLE,
            (None, Some(last)) => {
                self.scroll.is_none() && last.elapsed() >= POD_LOG_FOLLOW_INTERVAL
            }
            (None, None) => false,
        };
        if due {
            self.fetch(client);
        }
    }

    /// Read the logs again now (`r`, `p`)
    pub fn fetch(&mut self, client: Option<&Client>) {
        self.pending = None;
        self.last_read = Some(Instant::now());
        self.generation += 1;
        let Some(client) = client.cloned() else {
            self.read = Some(Read {
                lines: Vec::new(),
                failed: vec!["Not connected to Kubernetes".to_string()],
                skipped: 0,
            });
            return;
        };
        let sources: Vec<LogSource> = self
            .sources
            .iter()
            .take(POD_LOG_SOURCES_MAX)
            .cloned()
            .collect();
        let skipped = self.sources.len().saturating_sub(sources.len());
        let tail = tail_per_source(sources.len(), POD_LOG_TOTAL_LINES);
        let previous = self.previous;
        let slot = Arc::clone(&self.slot);
        let generation = self.generation;
        tokio::spawn(async move {
            let reads = sources.iter().map(|s| {
                let api: Api<Pod> = Api::namespaced(client.clone(), &s.namespace);
                let params = LogParams {
                    container: Some(s.container.clone()),
                    previous,
                    tail_lines: Some(tail),
                    timestamps: true,
                    ..Default::default()
                };
                async move { api.logs(&s.pod, &params).await }
            });
            let results =
                tokio::time::timeout(WORKLOADS_FETCH_TIMEOUT, futures::future::join_all(reads))
                    .await;
            let mut texts = Vec::new();
            let mut failed = Vec::new();
            match results {
                Ok(results) => {
                    for (source, result) in sources.iter().zip(results) {
                        match result {
                            Ok(text) => texts.push((source.label.clone(), text)),
                            Err(e) => failed.push(failure(source, &e.to_string())),
                        }
                    }
                }
                Err(_) => failed.push(format!(
                    "Timed out after {}s",
                    WORKLOADS_FETCH_TIMEOUT.as_secs()
                )),
            }
            let read = Read {
                lines: merge_logs(&texts).into_iter().map(Shown::of).collect(),
                failed,
                skipped,
            };
            if let Ok(mut slot) = slot.lock() {
                *slot = Some((generation, read));
            }
        });
    }

    /// Take a finished read of the current sources
    fn poll(&mut self) {
        let done = self.slot.lock().ok().and_then(|mut s| s.take());
        if let Some((generation, read)) = done
            && generation == self.generation
        {
            self.read = Some(read);
        }
    }

    /// Indexes of the lines whose level is shown
    fn visible(&self) -> Vec<usize> {
        self.read.as_ref().map_or_else(Vec::new, |r| {
            r.lines
                .iter()
                .enumerate()
                .filter(|(_, l)| {
                    !self
                        .hidden
                        .get(level_index(l.level))
                        .copied()
                        .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect()
        })
    }

    /// Positions among the visible lines that match the search
    fn matches(&self) -> Vec<usize> {
        let Some(read) = &self.read else {
            return Vec::new();
        };
        if self.query.is_empty() {
            return Vec::new();
        }
        self.visible()
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                read.lines
                    .get(**i)
                    .is_some_and(|l| !match_ranges(&l.searchable(), &self.query).is_empty())
            })
            .map(|(pos, _)| pos)
            .collect()
    }

    /// Show the next (or previous) match, centered
    fn jump_to_match(&mut self, forward: bool) {
        let matches = self.matches();
        let count = self.visible().len();
        let last = count.saturating_sub(self.height.max(1));
        let from = self
            .current_match
            .unwrap_or_else(|| self.scroll.unwrap_or(last) + if forward { 0 } else { self.height });
        let next = if forward {
            matches
                .iter()
                .find(|&&m| self.current_match.map_or(m >= from, |c| m > c))
                .or(matches.first())
        } else {
            matches
                .iter()
                .rev()
                .find(|&&m| self.current_match.map_or(m < from, |c| m < c))
                .or(matches.last())
        };
        if let Some(&pos) = next {
            self.current_match = Some(pos);
            let top = pos.saturating_sub(self.height / 2).min(last);
            self.scroll = (top < last).then_some(top);
        }
    }

    /// Typing a search (`/`): the view's other keys wait
    pub fn captures_text(&self) -> bool {
        self.searching
    }

    /// A search to clear before Esc leaves
    pub fn has_search(&self) -> bool {
        self.searching || !self.query.is_empty()
    }

    fn clear_search(&mut self) {
        self.searching = false;
        self.query.clear();
        self.current_match = None;
    }

    fn handle_search_key(&mut self, key: &KeyEvent) {
        use crate::ui_ext::{TextEdit, edit_text};
        match edit_text(&mut self.query, key) {
            TextEdit::Cancel => self.clear_search(),
            TextEdit::Submit => {
                self.searching = false;
                self.current_match = None;
                if self.query.is_empty() {
                    self.clear_search();
                } else {
                    self.jump_to_match(true);
                }
            }
            _ => {}
        }
    }

    /// Handle a pane key (levels, search, `p`, `w`, `r`, scrolling); `false`
    /// when it isn't one
    pub fn handle_key(&mut self, key: &KeyEvent, client: Option<&Client>) -> bool {
        use crate::components::multi_logs::LevelHotkey;
        use crate::ui_ext::Move;
        use crossterm::event::KeyCode;
        if self.searching {
            self.handle_search_key(key);
            return true;
        }
        if let Some(hotkey) = LevelHotkey::from_key(key) {
            match hotkey {
                LevelHotkey::All => self.hidden = [false; 5],
                LevelHotkey::Only(i) => {
                    self.hidden = [true; 5];
                    if let Some(h) = self.hidden.get_mut(i) {
                        *h = false;
                    }
                }
                LevelHotkey::Toggle(i) => {
                    if let Some(h) = self.hidden.get_mut(i) {
                        *h = !*h;
                    }
                }
            }
            self.current_match = None;
            return true;
        }
        match key.code {
            KeyCode::Esc if self.has_search() => self.clear_search(),
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
                self.current_match = None;
            }
            KeyCode::Char('n') if !self.query.is_empty() => self.jump_to_match(true),
            KeyCode::Char('N') if !self.query.is_empty() => self.jump_to_match(false),
            KeyCode::Char('p') => {
                self.previous = !self.previous;
                self.read = None;
                self.scroll = None;
                self.fetch(client);
            }
            KeyCode::Char('r') => self.fetch(client),
            KeyCode::Char('w') => self.wrap = !self.wrap,
            _ => match Move::from_key(key) {
                // j/k move the list; the page keys and g/G scroll the logs
                Some(step @ (Move::First | Move::Last | Move::By(_))) => {
                    let count = self.visible().len();
                    let last = count.saturating_sub(self.height.max(1));
                    let step = match step {
                        Move::By(n) => Move::By(
                            n.signum() * isize::try_from(self.height.max(2) - 1).unwrap_or(10),
                        ),
                        other => other,
                    };
                    let top = self.scroll.unwrap_or(last);
                    let moved = crate::ui_ext::moved(top, last + 1, step).unwrap_or(0);
                    self.scroll = (moved < last).then_some(moved);
                }
                _ => return false,
            },
        }
        true
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.poll();
        let dim = Style::default().dim();
        let block = Block::default().borders(Borders::TOP);
        let inner = block.inner(area);
        let mut title = vec![
            Span::styled(" Logs ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("· {} ", self.title),
                Style::default().fg(Color::Cyan),
            ),
        ];
        if self.previous {
            title.push(Span::styled(
                "· previous run ",
                Style::default().fg(Color::Yellow),
            ));
        }
        if self.hidden.iter().any(|h| *h) {
            let shown: Vec<&str> = [
                LogLevel::Error,
                LogLevel::Warn,
                LogLevel::Info,
                LogLevel::Debug,
                LogLevel::Unknown,
            ]
            .iter()
            .zip(self.hidden)
            .filter(|(_, hidden)| !hidden)
            .map(|(l, _)| l.badge())
            .collect();
            title.push(Span::styled(
                format!(
                    "· levels {} ",
                    if shown.is_empty() {
                        "none".to_string()
                    } else {
                        shown.join(" ")
                    }
                ),
                Style::default().fg(Color::Magenta),
            ));
        }
        if self.searching || !self.query.is_empty() {
            let cursor = if self.searching { "█" } else { "" };
            let count = if self.searching {
                String::new()
            } else {
                let matches = self.matches();
                let at = self
                    .current_match
                    .and_then(|c| matches.iter().position(|&m| m == c))
                    .map_or_else(String::new, |p| format!("{}/", p + 1));
                format!(" {at}{}", matches.len())
            };
            title.push(Span::styled(
                format!("· /{}{cursor}{count} ", self.query),
                Style::default().fg(Color::Cyan),
            ));
        }
        if self.scroll.is_none() && self.read.is_some() {
            title.push(Span::styled("· following ", dim));
        }
        frame.render_widget(block.title(Line::from(title)), area);

        let Some(read) = &self.read else {
            frame.render_widget(Paragraph::new("Loading logs...").style(dim), inner);
            return;
        };
        let mut notes = Vec::new();
        if !read.failed.is_empty() {
            notes.push(Line::from(Span::styled(
                format!("Couldn't read: {}", read.failed.join("; ")),
                Style::default().fg(Color::Yellow),
            )));
        }
        if read.skipped > 0 {
            notes.push(Line::from(Span::styled(
                format!("{} more containers not read (select a pod)", read.skipped),
                dim,
            )));
        }
        let [notes_area, body] = Layout::vertical([
            Constraint::Length(talos9s_core::convert::to_u16(notes.len())),
            Constraint::Fill(1),
        ])
        .areas(inner);
        frame.render_widget(Paragraph::new(notes).wrap(Wrap { trim: true }), notes_area);
        self.height = usize::from(body.height);
        if read.lines.is_empty() {
            let text = if self.previous {
                "No previous run: no container here has restarted"
            } else {
                "No log lines"
            };
            frame.render_widget(Paragraph::new(text).style(dim), body);
            return;
        }
        let visible = self.visible();
        if visible.is_empty() {
            frame.render_widget(
                Paragraph::new("No lines at the levels shown  <0> all levels").style(dim),
                body,
            );
            return;
        }
        let width = read
            .lines
            .iter()
            .map(|l| l.line.source.chars().count())
            .max()
            .unwrap_or(0);
        let last = visible.len().saturating_sub(self.height.max(1));
        let top = self.scroll.unwrap_or(last).min(last);
        let shown: Vec<Line> = visible
            .iter()
            .enumerate()
            .skip(top)
            .take(self.height)
            .filter_map(|(pos, &i)| {
                let l = read.lines.get(i)?;
                let mut spans = Vec::new();
                if width > 0 {
                    spans.push(Span::styled(
                        format!("{:width$} ", l.line.source),
                        Style::default().fg(source_color(&l.line.source)),
                    ));
                }
                spans.extend(line_spans(l, &self.query, self.current_match == Some(pos)));
                Some(Line::from(spans))
            })
            .collect();
        let mut paragraph = Paragraph::new(shown);
        if self.wrap {
            paragraph = paragraph.wrap(Wrap { trim: false });
        }
        let [text, bar] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(1)]).areas(body);
        frame.render_widget(paragraph, text);
        crate::ui_ext::draw_scrollbar(
            frame,
            bar,
            talos9s_core::convert::to_u16(visible.len()),
            talos9s_core::convert::to_u16(top),
        );
    }
}

/// A line's time (`HH:MM:SS`, UTC like the Logs view), level badge,
/// message and its fields dimmed, with the matches of `query` marked (the
/// current one brighter)
fn line_spans(line: &Shown, query: &str, current: bool) -> Vec<Span<'static>> {
    let time = line
        .line
        .time
        .and_then(|nanos| chrono::DateTime::from_timestamp(nanos.div_euclid(1_000_000_000), 0))
        .map_or_else(|| " ".repeat(8), |t| t.format("%H:%M:%S").to_string());
    let color = crate::components::multi_logs::level_color(line.level);
    let text_style = match line.level {
        LogLevel::Error => Style::default().fg(Color::Red),
        LogLevel::Warn => Style::default().fg(Color::Yellow),
        LogLevel::Debug => Style::default().dim(),
        _ => Style::default(),
    };
    let mark = if current {
        Style::default().fg(Color::Black).bg(Color::Yellow)
    } else {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    };
    let mut spans = vec![
        Span::styled(format!("{time} "), Style::default().dim()),
        Span::styled(
            format!("{} ", line.level.badge()),
            Style::default().fg(color),
        ),
    ];
    spans.extend(highlighted(&line.message, query, text_style, mark));
    if !line.fields.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(highlighted(
            &line.fields,
            query,
            Style::default().dim(),
            mark,
        ));
    }
    spans
}

/// `text` in `style`, the matches of `query` in `mark`
fn highlighted(text: &str, query: &str, style: Style, mark: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut at = 0;
    for range in match_ranges(text, query) {
        if let (Some(before), Some(found)) = (text.get(at..range.start), text.get(range.clone())) {
            if !before.is_empty() {
                spans.push(Span::styled(before.to_string(), style));
            }
            spans.push(Span::styled(found.to_string(), mark));
        }
        at = range.end;
    }
    if let Some(rest) = text.get(at..)
        && !rest.is_empty()
    {
        spans.push(Span::styled(rest.to_string(), style));
    }
    spans
}

/// Why a source couldn't be read, short: a missing previous run is expected
fn failure(source: &LogSource, error: &str) -> String {
    let name = if source.label.is_empty() {
        source.container.as_str()
    } else {
        source.label.as_str()
    };
    if error.contains("previous terminated container") {
        format!("{name} (no previous run)")
    } else {
        format!("{name}: {error}")
    }
}

/// A steady color per source, so merged lines can be told apart
fn source_color(source: &str) -> Color {
    const COLORS: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Blue,
        Color::Green,
        Color::Yellow,
        Color::LightRed,
    ];
    let hash = source.bytes().fold(0usize, |h, b| {
        h.wrapping_mul(31).wrapping_add(usize::from(b))
    });
    COLORS
        .get(hash % COLORS.len())
        .copied()
        .unwrap_or(Color::Cyan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(pod: &str) -> LogSource {
        LogSource {
            namespace: "apps".into(),
            pod: pod.into(),
            container: "app".into(),
            label: pod.into(),
        }
    }

    /// New sources wait for the selection to rest; the same ones keep what
    /// was read
    #[test]
    fn sources_settle_before_reading() {
        let mut pane = LogPane::default();
        pane.set_sources("web".into(), vec![source("web-a")], "kubectl".into());
        assert!(pane.pending.is_some());
        pane.tick(None);
        assert!(pane.read.is_none(), "read before the selection rested");
        pane.fetch(None);
        assert!(pane.read.is_some());
        pane.set_sources("web".into(), vec![source("web-a")], "kubectl".into());
        assert!(pane.read.is_some());
        pane.handle_key(&KeyEvent::from(crossterm::event::KeyCode::Char('p')), None);
        assert_eq!(pane.command(), "kubectl --previous");
    }

    /// A JSON or logfmt line shows its message, then its fields dimmed, the
    /// level from its field; other lines as they are
    #[test]
    fn json_lines_read_as_text() {
        let line = LogLine {
            time: Some(1_793_630_002_000_000_000),
            source: String::new(),
            text: r#"{"level":"error","msg":"db down","retry":3}"#.into(),
        };
        let text: String = line_spans(&Shown::of(line), "", false)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.ends_with("ERR db down retry=3"), "{text}");
        let plain = LogLine {
            time: None,
            source: String::new(),
            text: "level=warn msg=slow took=3s".into(),
        };
        let text: String = line_spans(&Shown::of(plain), "", false)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.ends_with("WRN slow took=3s"), "{text}");
        let raw = LogLine {
            time: None,
            source: String::new(),
            text: "Error: connection refused".into(),
        };
        let text: String = line_spans(&Shown::of(raw), "", false)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.ends_with("ERR Error: connection refused"), "{text}");
    }

    /// `2` shows warnings only, `0` all again; a search marks its matches
    /// and Esc clears it
    #[test]
    fn levels_and_search() {
        use crossterm::event::KeyCode;
        let mut pane = LogPane::default();
        let text = |t: &str| {
            Shown::of(LogLine {
                time: None,
                source: String::new(),
                text: t.into(),
            })
        };
        pane.read = Some(Read {
            lines: vec![
                text("level=error msg=boom"),
                text("level=warn msg=\"slow db\""),
                text("level=info msg=\"ok db\""),
            ],
            failed: Vec::new(),
            skipped: 0,
        });
        // Drawn: all three lines fit
        pane.height = 10;
        let key = |pane: &mut LogPane, code| pane.handle_key(&KeyEvent::from(code), None);
        key(&mut pane, KeyCode::Char('2'));
        assert_eq!(pane.visible(), [1]);
        key(&mut pane, KeyCode::Char('0'));
        assert_eq!(pane.visible(), [0, 1, 2]);
        key(&mut pane, KeyCode::Char('/'));
        assert!(pane.captures_text());
        for c in "db".chars() {
            key(&mut pane, KeyCode::Char(c));
        }
        key(&mut pane, KeyCode::Enter);
        assert!(!pane.captures_text());
        assert_eq!(pane.matches(), [1, 2]);
        assert_eq!(pane.current_match, Some(1));
        key(&mut pane, KeyCode::Char('n'));
        assert_eq!(pane.current_match, Some(2));
        key(&mut pane, KeyCode::Char('n'));
        assert_eq!(pane.current_match, Some(1), "wraps around");
        let marked = line_spans(&text("level=warn msg=\"slow db\""), "db", true);
        assert!(
            marked
                .iter()
                .any(|s| s.content == "db" && s.style.bg == Some(Color::Yellow))
        );
        key(&mut pane, KeyCode::Esc);
        assert!(!pane.has_search());
    }

    #[test]
    fn missing_previous_run_is_short() {
        let e = "previous terminated container \"app\" in pod \"web-a\" not found";
        assert_eq!(failure(&source("web-a"), e), "web-a (no previous run)");
    }
}
