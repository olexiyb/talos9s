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
use talos9s_core::pod_logs::{LogLine, LogSource, merge_logs, tail_per_source};

/// The selection must rest this long before its logs are read, so walking
/// the list doesn't read every pod on the way
const SETTLE: Duration = Duration::from_millis(300);

/// What a read gave: the lines and the sources that couldn't be read
#[derive(Debug, Clone)]
struct Read {
    lines: Vec<LogLine>,
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
                lines: merge_logs(&texts),
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

    /// Handle a pane key (`p`, `w`, `r`, scrolling); `false` when it isn't one
    pub fn handle_key(&mut self, key: &KeyEvent, client: Option<&Client>) -> bool {
        use crate::ui_ext::Move;
        use crossterm::event::KeyCode;
        match key.code {
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
                    let count = self.read.as_ref().map_or(0, |r| r.lines.len());
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
        let width = read
            .lines
            .iter()
            .map(|l| l.source.chars().count())
            .max()
            .unwrap_or(0);
        let last = read.lines.len().saturating_sub(self.height.max(1));
        let top = self.scroll.unwrap_or(last).min(last);
        let shown: Vec<Line> = read
            .lines
            .iter()
            .skip(top)
            .take(self.height)
            .map(|l| {
                let mut spans = Vec::new();
                if width > 0 {
                    spans.push(Span::styled(
                        format!("{:width$} ", l.source),
                        Style::default().fg(source_color(&l.source)),
                    ));
                }
                spans.push(Span::raw(l.text.clone()));
                Line::from(spans)
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
            talos9s_core::convert::to_u16(read.lines.len()),
            talos9s_core::convert::to_u16(top),
        );
    }
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

    #[test]
    fn missing_previous_run_is_short() {
        let e = "previous terminated container \"app\" in pod \"web-a\" not found";
        assert_eq!(failure(&source("web-a"), e), "web-a (no previous run)");
    }
}
