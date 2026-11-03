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
    fn of(mut line: LogLine) -> Self {
        if let std::borrow::Cow::Owned(clean) = talos9s_core::logparse::strip_ansi(&line.text) {
            line.text = clean;
        }
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
    /// Why the external viewer (`v`) failed
    viewer_problem: Option<String>,
    /// Selecting lines to copy (`V`): the cursor, a position among the
    /// visible lines, and where the selection started
    cursor: Option<usize>,
    visual: talos9s_core::VisualSelection,
    /// For sources read through the Talos API (`LogSource::talos`)
    talos: Option<talos_rs::TalosClient>,
    /// Where the pane was last drawn, for the mouse wheel
    area: Rect,
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
        self.viewer_problem = None;
        self.stop_selecting();
        self.sources = sources;
        // No earlier run among these: back to the current one
        if !self.has_previous() {
            self.previous = false;
        }
        self.read = None;
        self.scroll = None;
        self.pending = Some(Instant::now());
    }

    /// Selecting lines (`V`): `j`/`k` extend, `y` copies, Esc stops
    pub fn selecting(&self) -> bool {
        self.cursor.is_some()
    }

    /// `V`: select from the newest line on screen
    fn start_selecting(&mut self) {
        let visible = self.visible().len();
        if visible == 0 {
            return;
        }
        let bottom = match self.scroll {
            Some(top) => (top + self.height.max(1)).min(visible) - 1,
            None => visible - 1,
        };
        self.cursor = Some(bottom);
        self.visual.clear();
        self.visual.toggle(bottom);
    }

    fn stop_selecting(&mut self) {
        self.cursor = None;
        self.visual.clear();
    }

    /// The selected lines as shown (time, level, message, fields), for `y`
    fn selected_text(&self) -> Vec<String> {
        let (Some(cursor), Some(read)) = (self.cursor, self.read.as_ref()) else {
            return Vec::new();
        };
        let visible = self.visible();
        let with_date = spans_days(read.lines.iter().filter_map(|l| l.line.time));
        self.visual
            .rows(cursor)
            .filter_map(|pos| visible.get(pos).and_then(|&i| read.lines.get(i)))
            .map(|l| {
                let (prefix, body) = line_spans(l, "", false, with_date);
                let mut text = String::new();
                if !l.line.source.is_empty() {
                    text.push_str(&l.line.source);
                    text.push(' ');
                }
                for span in prefix.iter().chain(&body) {
                    text.push_str(&span.content);
                }
                text.trim_end().to_string()
            })
            .collect()
    }

    /// Keys while selecting; whether the key was the selection's
    fn handle_selecting_key(&mut self, key: &KeyEvent) -> bool {
        use crate::ui_ext::Move;
        use crossterm::event::KeyCode;
        let Some(cursor) = self.cursor else {
            return false;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('V' | 'q') => self.stop_selecting(),
            KeyCode::Char('y') => {
                crate::clipboard::copy_lines(&self.selected_text());
                self.stop_selecting();
            }
            _ => {
                let len = self.visible().len();
                if let Some(step) = Move::from_key(key)
                    && let Some(moved) = crate::ui_ext::moved(cursor, len, step)
                {
                    self.cursor = Some(moved);
                    // Keep it on screen: the draw scrolls to it
                    if self.scroll.is_none() && moved + 1 < len {
                        self.scroll = Some(moved.min(len.saturating_sub(self.height.max(1))));
                    }
                }
            }
        }
        true
    }

    /// The wheel over the pane scrolls it (3 lines); scrolled back to the
    /// end it follows again. Whether the event was the pane's.
    pub fn handle_mouse(&mut self, mouse: &crossterm::event::MouseEvent) -> bool {
        use crossterm::event::MouseEventKind;
        let delta: isize = match mouse.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => return false,
        };
        let a = self.area;
        if mouse.column < a.x
            || mouse.column >= a.right()
            || mouse.row < a.y
            || mouse.row >= a.bottom()
        {
            return false;
        }
        self.scroll_lines(delta);
        true
    }

    /// Scroll by `delta` lines; back at the end it follows again
    pub fn scroll_lines(&mut self, delta: isize) {
        let count = self.visible().len();
        let last = count.saturating_sub(self.height.max(1));
        let top = self.scroll.unwrap_or(last);
        let moved = top.saturating_add_signed(delta).min(last);
        self.scroll = (moved < last).then_some(moved);
    }

    /// Whether the mouse event is over the pane
    pub fn contains(&self, mouse: &crossterm::event::MouseEvent) -> bool {
        let a = self.area;
        mouse.column >= a.x
            && mouse.column < a.right()
            && mouse.row >= a.y
            && mouse.row < a.bottom()
    }

    /// The Talos client for sources read through the Talos API
    pub fn set_talos_client(&mut self, client: talos_rs::TalosClient) {
        self.talos = Some(client);
    }

    pub fn previous(&self) -> bool {
        self.previous
    }

    /// One of the containers shown has a previous run (`p` works)
    pub fn has_previous(&self) -> bool {
        self.sources.iter().any(|s| s.has_previous)
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
        let kube = client.cloned();
        let talos = self.talos.clone();
        let needs_kube = self
            .sources
            .iter()
            .any(|s| s.talos.is_none() && s.file.is_none());
        if needs_kube && kube.is_none() {
            self.read = Some(Read {
                lines: Vec::new(),
                failed: vec!["Not connected to Kubernetes".to_string()],
                skipped: 0,
            });
            return;
        }
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
                let kube = kube.clone();
                let talos = talos.clone();
                async move {
                    if let Some(path) = &s.file {
                        return tokio::fs::read_to_string(path)
                            .await
                            .map(|text| crate::operations::run_output(&text))
                            .map_err(|e| e.to_string());
                    }
                    if let Some(target) = &s.talos {
                        // CRI lines carry their time; services' lines don't
                        let Some(talos) = talos else {
                            return Err("Not connected to the cluster".to_string());
                        };
                        let tail = i32::try_from(tail).unwrap_or(i32::MAX);
                        return talos
                            .with_node(&target.address)
                            .container_logs(target.kind, &target.id, tail)
                            .await
                            .map(|text| {
                                use talos9s_core::pod_logs::{service_time, strip_cri};
                                let system = target.kind == talos_rs::ContainerKind::System;
                                text.lines()
                                    .map(|l| {
                                        if system {
                                            service_time(l)
                                        } else {
                                            strip_cri(l)
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .map_err(|e| e.to_string());
                    }
                    let Some(kube) = kube else {
                        return Err("Not connected to Kubernetes".to_string());
                    };
                    let api: Api<Pod> = Api::namespaced(kube, &s.namespace);
                    let params = LogParams {
                        container: Some(s.container.clone()),
                        previous,
                        tail_lines: Some(tail),
                        timestamps: true,
                        ..Default::default()
                    };
                    api.logs(&s.pod, &params).await.map_err(|e| e.to_string())
                }
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
                            Err(e) => failed.push(failure(source, &e)),
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

    /// The lines read, as the containers wrote them (each after its label
    /// when several are merged), for the external viewer; `None` before a read
    pub fn export(&self) -> Option<String> {
        let read = self.read.as_ref().filter(|r| !r.lines.is_empty())?;
        let mut text = String::new();
        for shown in &read.lines {
            if !shown.line.source.is_empty() {
                text.push_str(&shown.line.source);
                text.push_str(" | ");
            }
            text.push_str(&shown.line.text);
            text.push('\n');
        }
        Some(text)
    }

    /// Why the external viewer couldn't show the logs, until other logs are
    /// shown or `r`
    pub fn set_viewer_problem(&mut self, problem: String) {
        self.viewer_problem = Some(problem);
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
        if self.selecting() {
            return self.handle_selecting_key(key);
        }
        if key.code == crossterm::event::KeyCode::Char('V') {
            self.start_selecting();
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
            // Nothing to switch to: no container shown has restarted
            KeyCode::Char('p') if !self.previous && !self.has_previous() => return false,
            KeyCode::Char('p') => {
                self.previous = !self.previous;
                self.read = None;
                self.scroll = None;
                self.fetch(client);
            }
            KeyCode::Char('r') => {
                self.viewer_problem = None;
                self.fetch(client);
            }
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
        self.area = area;
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
        if let Some(problem) = &self.viewer_problem {
            notes.push(Line::from(Span::styled(
                problem.clone(),
                Style::default().fg(Color::Yellow),
            )));
        }
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
        let with_date = spans_days(read.lines.iter().filter_map(|l| l.line.time));
        let [text, bar] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(1)]).areas(body);
        let text_width = usize::from(text.width);
        let height = self.height.max(1);
        let selected = |pos: usize| {
            self.cursor
                .is_some_and(|cursor| self.visual.contains(pos, cursor))
        };
        // The screen rows of the line at `pos` among the visible ones
        let rows_of = |pos: usize| -> Vec<Line<'static>> {
            let Some(l) = visible.get(pos).and_then(|&i| read.lines.get(i)) else {
                return Vec::new();
            };
            let mut prefix = Vec::new();
            if width > 0 {
                prefix.push(Span::styled(
                    format!("{:width$} ", l.line.source),
                    Style::default().fg(source_color(&l.line.source)),
                ));
            }
            let (head, rest) =
                line_spans(l, &self.query, self.current_match == Some(pos), with_date);
            prefix.extend(head);
            let mut rows = layout_line(prefix, rest, text_width, self.wrap);
            if selected(pos) {
                let style = if self.cursor == Some(pos) {
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().bg(Color::DarkGray)
                };
                for row in &mut rows {
                    row.style = style;
                }
            }
            rows
        };
        // Lines from `top` down until the pane is full
        let forward = |top: usize| {
            let mut rows = Vec::new();
            let mut pos = top;
            while rows.len() < height && pos < visible.len() {
                rows.extend(rows_of(pos));
                pos += 1;
            }
            rows.truncate(height);
            (rows, pos)
        };
        // Lines up to `end` (inclusive) from the bottom, the newest last
        let backward = |end: usize| {
            let mut rows: Vec<Line> = Vec::new();
            let mut top = end + 1;
            while rows.len() < height && top > 0 {
                top -= 1;
                let mut above = rows_of(top);
                above.append(&mut rows);
                rows = above;
            }
            let cut = rows.len().saturating_sub(height);
            (rows.split_off(cut), top)
        };
        let end = visible.len() - 1;
        // From `top`; near the end, where that leaves rows empty, the last
        // lines fill the pane instead (unless the cursor is above them)
        let from = |top: usize, cursor: Option<usize>| {
            let (rows, next) = forward(top);
            if cursor.is_some_and(|c| c >= next) {
                return backward(cursor.unwrap_or(end));
            }
            if rows.len() < height && top > 0 {
                let (last, last_top) = backward(end);
                if cursor.is_none_or(|c| c >= last_top) {
                    return (last, last_top);
                }
            }
            (rows, top)
        };
        let (shown, top) = match (self.cursor, self.scroll) {
            (Some(cursor), scroll) => from(scroll.unwrap_or(cursor).min(cursor), Some(cursor)),
            (None, Some(top)) => from(top.min(end), None),
            (None, None) => backward(end),
        };
        if self.cursor.is_some() {
            self.scroll = Some(top);
        }
        let paragraph = Paragraph::new(shown);
        frame.render_widget(paragraph, text);
        crate::ui_ext::draw_scrollbar(
            frame,
            bar,
            talos9s_core::convert::to_u16(visible.len()),
            talos9s_core::convert::to_u16(top),
        );
    }
}

/// Times (nanoseconds) on more than one day (UTC): then lines show the date
fn spans_days(times: impl Iterator<Item = i64>) -> bool {
    let day = |nanos: i64| nanos.div_euclid(86_400 * 1_000_000_000);
    let mut days = times.map(day);
    let Some(first) = days.next() else {
        return false;
    };
    days.any(|d| d != first)
}

/// A line's time (`HH:MM:SS`, UTC like the Logs view; `MM-DD HH:MM:SS`
/// when `with_date`), level badge, message and its fields dimmed, with the
/// matches of `query` marked (the current one brighter)
fn line_spans(
    line: &Shown,
    query: &str,
    current: bool,
    with_date: bool,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let (format, blank) = if with_date {
        ("%m-%d %H:%M:%S", 14)
    } else {
        ("%H:%M:%S", 8)
    };
    let time = line
        .line
        .time
        .and_then(|nanos| chrono::DateTime::from_timestamp(nanos.div_euclid(1_000_000_000), 0))
        .map_or_else(|| " ".repeat(blank), |t| t.format(format).to_string());
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
    let prefix = vec![
        Span::styled(format!("{time} "), Style::default().dim()),
        Span::styled(
            format!("{} ", line.level.badge()),
            Style::default().fg(color),
        ),
    ];
    let mut spans = highlighted(&line.message, query, text_style, mark);
    if !line.fields.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(highlighted(
            &line.fields,
            query,
            Style::default().dim(),
            mark,
        ));
    }
    (prefix, spans)
}

/// A log line as screen rows of `width`: `prefix` (source, time, level),
/// then `body`; wrapped, the rest goes on rows indented by the prefix, so
/// the time and level column stays clear. Breaks after a space when one is
/// in the second half of the row.
fn layout_line(
    prefix: Vec<Span<'static>>,
    body: Vec<Span<'static>>,
    width: usize,
    wrap: bool,
) -> Vec<Line<'static>> {
    let indent: usize = prefix.iter().map(|s| s.content.chars().count()).sum();
    if !wrap || width < indent + 20 {
        let mut spans = prefix;
        spans.extend(body);
        return vec![Line::from(spans)];
    }
    let room = width - indent;
    let mut rows = Vec::new();
    let mut row = prefix;
    let mut used = 0;
    for span in body {
        let mut text: &str = &span.content;
        while !text.is_empty() {
            let free = room - used;
            let count = text.chars().count();
            if count <= free {
                row.push(Span::styled(text.to_string(), span.style));
                used += count;
                break;
            }
            let split = text.char_indices().nth(free).map_or(text.len(), |(i, _)| i);
            // At a space in the second half of the row, when there is one
            let split = text
                .get(..split)
                .and_then(|piece| piece.rfind(' '))
                .filter(|&at| used + text.get(..at).map_or(0, |p| p.chars().count()) > room / 2)
                .map_or(split, |at| at + 1);
            let (piece, rest) = text.split_at(split);
            row.push(Span::styled(piece.to_string(), span.style));
            rows.push(Line::from(std::mem::take(&mut row)));
            row.push(Span::raw(" ".repeat(indent)));
            used = 0;
            text = rest;
        }
    }
    rows.push(Line::from(row));
    rows
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

    /// The text of a line as drawn on one row
    fn spans_text((prefix, body): (Vec<Span<'static>>, Vec<Span<'static>>)) -> String {
        prefix
            .into_iter()
            .chain(body)
            .map(|s| s.content.into_owned())
            .collect()
    }

    fn source(pod: &str) -> LogSource {
        LogSource {
            namespace: "apps".into(),
            pod: pod.into(),
            container: "app".into(),
            label: pod.into(),
            has_previous: true,
            talos: None,
            file: None,
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
        let text = spans_text(line_spans(&Shown::of(line), "", false, false));
        assert!(text.ends_with("ERR db down retry=3"), "{text}");
        let plain = LogLine {
            time: None,
            source: String::new(),
            text: "level=warn msg=slow took=3s".into(),
        };
        let text = spans_text(line_spans(&Shown::of(plain), "", false, false));
        assert!(text.ends_with("WRN slow took=3s"), "{text}");
        let raw = LogLine {
            time: None,
            source: String::new(),
            text: "Error: connection refused".into(),
        };
        let text = spans_text(line_spans(&Shown::of(raw), "", false, false));
        assert!(text.ends_with("ERR Error: connection refused"), "{text}");
    }

    /// The date shows only when the lines span more than one day
    #[test]
    fn date_when_lines_span_days() {
        let day = 86_400 * 1_000_000_000_i64;
        assert!(!spans_days([5, day - 1].into_iter()));
        assert!(spans_days([5, day + 1].into_iter()));
        assert!(!spans_days(std::iter::empty()));
        let line = LogLine {
            time: Some(1_793_630_002_000_000_000),
            source: String::new(),
            text: "x".into(),
        };
        let text = spans_text(line_spans(&Shown::of(line), "", false, true));
        assert!(text.starts_with("11-02 "), "{text}");
    }

    /// Wrapped rows go on under the message, the time and level column
    /// clear; a break falls after a space when there is one
    #[test]
    fn wraps_under_the_message() {
        let prefix = vec![Span::raw("19:41:48 "), Span::raw("INF ")];
        let body = vec![Span::raw(
            "GET /harbor/docker/registry/v2/blobs/sha256/e9 done",
        )];
        // A space in the second half: the break goes after it
        let words = vec![Span::raw("one two three four five six seven eight")];
        let rows: Vec<String> = layout_line(prefix.clone(), words, 40, true)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(
            rows,
            [
                "19:41:48 INF one two three four five ",
                "             six seven eight",
            ]
        );
        let rows: Vec<String> = layout_line(prefix.clone(), body.clone(), 40, true)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(
            rows,
            [
                "19:41:48 INF GET /harbor/docker/registry",
                "             /v2/blobs/sha256/e9 done",
            ]
        );
        assert!(rows.iter().all(|r| r.chars().count() <= 40));
        // Unwrapped: one row
        assert_eq!(layout_line(prefix, body, 40, false).len(), 1);
    }

    /// `V` selects from the newest line, `k` extends upwards; the lines
    /// copied read as shown; Esc stops
    #[test]
    fn selects_lines_to_copy() {
        use crossterm::event::KeyCode;
        let mut pane = LogPane {
            height: 10,
            ..Default::default()
        };
        pane.read = Some(Read {
            lines: [
                "level=info msg=one",
                "level=warn msg=two",
                "level=error msg=three",
            ]
            .map(|t| {
                Shown::of(LogLine {
                    time: None,
                    source: String::new(),
                    text: format!("\u{1b}[2m{t}\u{1b}[0m"),
                })
            })
            .to_vec(),
            failed: Vec::new(),
            skipped: 0,
        });
        let key = |pane: &mut LogPane, code| pane.handle_key(&KeyEvent::from(code), None);
        key(&mut pane, KeyCode::Char('V'));
        assert!(pane.selecting());
        key(&mut pane, KeyCode::Char('k'));
        let copied = pane.selected_text();
        assert_eq!(copied.len(), 2);
        assert!(copied[0].ends_with("WRN two"), "{copied:?}");
        assert!(copied[1].ends_with("ERR three"), "{copied:?}");
        key(&mut pane, KeyCode::Esc);
        assert!(!pane.selecting());
    }

    /// The wheel over the pane scrolls it, elsewhere it doesn't; back at
    /// the end it follows again
    #[test]
    fn wheel_scrolls_over_the_pane() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let mut pane = LogPane {
            height: 10,
            area: Rect::new(0, 20, 80, 11),
            ..Default::default()
        };
        pane.read = Some(Read {
            lines: (0..50)
                .map(|i| {
                    Shown::of(LogLine {
                        time: None,
                        source: String::new(),
                        text: format!("line {i}"),
                    })
                })
                .collect(),
            failed: Vec::new(),
            skipped: 0,
        });
        let wheel = |kind, row| MouseEvent {
            kind,
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(!pane.handle_mouse(&wheel(MouseEventKind::ScrollUp, 5)));
        assert_eq!(pane.scroll, None);
        assert!(pane.handle_mouse(&wheel(MouseEventKind::ScrollUp, 25)));
        assert_eq!(pane.scroll, Some(37));
        assert!(pane.handle_mouse(&wheel(MouseEventKind::ScrollDown, 25)));
        assert_eq!(pane.scroll, None, "following again");
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
        let marked = line_spans(&text("level=warn msg=\"slow db\""), "db", true, false);
        assert!(
            marked
                .1
                .iter()
                .any(|s| s.content == "db" && s.style.bg == Some(Color::Yellow))
        );
        key(&mut pane, KeyCode::Esc);
        assert!(!pane.has_search());
    }

    /// The viewer gets the lines as written, each after its label when
    /// merged; nothing before a read
    #[test]
    fn export_for_the_viewer() {
        let mut pane = LogPane::default();
        assert_eq!(pane.export(), None);
        let line = |source: &str, text: &str| {
            Shown::of(LogLine {
                time: None,
                source: source.into(),
                text: text.into(),
            })
        };
        pane.read = Some(Read {
            lines: vec![line("web-a", r#"{"msg":"up"}"#), line("web-b", "plain")],
            failed: Vec::new(),
            skipped: 0,
        });
        assert_eq!(
            pane.export().as_deref(),
            Some("web-a | {\"msg\":\"up\"}\nweb-b | plain\n")
        );
    }

    #[test]
    fn missing_previous_run_is_short() {
        let e = "previous terminated container \"app\" in pod \"web-a\" not found";
        assert_eq!(failure(&source("web-a"), e), "web-a (no previous run)");
    }
}
