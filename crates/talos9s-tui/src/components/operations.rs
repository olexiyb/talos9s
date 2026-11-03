//! Operations view (`o`): the operations for the selected node, group or
//! cluster, and every run so far (`plans/operations.md`).
//!
//! The top lists what can run on the target; Enter opens its dialog (the
//! nodes, the commands, the safety checks), `y` starts it. Runs go on in the
//! background (`crate::operations`), one per context; the history below
//! lists the run files, newest first, and Enter shows one's output in the
//! log pane, following it while it runs.

use crate::action::Action;
use crate::components::Component;
use crate::components::diagnostics::k8s::DrainOptions;
use crate::components::pod_logs::LogPane;
use crate::operations::{
    ActiveRun, DELAY_BETWEEN_NODES, OpKind, OpNode, OpRequest, Runs, SafetyInfo, fetch_safety,
};
use crate::ui_ext::{KEY, KEY_NO, KEY_YES, Move, RowHits};
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap},
};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use talos_rs::TalosClient;
use talos9s_core::SafetyStatus;
use talos9s_core::operations::{Run, RunStatus, run_rows};
use talos9s_core::pod_logs::LogSource;
use tokio::task::JoinHandle;

/// Read the history again this often while a run goes, else this often
const RELOAD_RUNNING: Duration = Duration::from_secs(2);
const RELOAD_IDLE: Duration = Duration::from_secs(15);

/// A dialog over the view
enum Dialog {
    /// Start `kind`: pick the nodes (a group), read the checks, `y` starts
    Start {
        kind: OpKind,
        picked: Vec<bool>,
        cursor: usize,
        safety: Option<SafetyInfo>,
        task: Option<JoinHandle<SafetyInfo>>,
    },
    /// Delete these run files
    Delete(Vec<PathBuf>),
    /// Cancel the run going in this context
    Cancel(String),
}

pub struct OperationsComponent {
    context: String,
    /// What the target is called: a node, `bc control plane`, `bc`
    target: String,
    nodes: Vec<OpNode>,
    group: bool,
    runs: Runs,
    dir: Option<PathBuf>,
    talos: Option<TalosClient>,
    cp_ip: Option<String>,
    history: Vec<(PathBuf, Run)>,
    loaded: Option<Instant>,
    /// Over the operations, then the history rows
    selected: usize,
    table: TableState,
    filter: String,
    filtering: bool,
    /// `A`: only this context's runs (default: every context)
    this_context: bool,
    dialog: Option<Dialog>,
    /// The run shown (Enter), and its output
    focus: Option<PathBuf>,
    logs: LogPane,
    /// Why the last action didn't happen
    message: Option<String>,
    ops_area: Rect,
    rows: RowHits,
}

impl OperationsComponent {
    pub fn new(
        context: String,
        target: String,
        nodes: Vec<OpNode>,
        group: bool,
        runs: Runs,
    ) -> Self {
        Self {
            context,
            target,
            nodes,
            group,
            runs,
            dir: crate::operations::runs_dir(),
            talos: None,
            cp_ip: None,
            history: Vec::new(),
            loaded: None,
            selected: 0,
            table: TableState::default(),
            filter: String::new(),
            filtering: false,
            this_context: false,
            dialog: None,
            focus: None,
            logs: LogPane::default(),
            message: None,
            ops_area: Rect::default(),
            rows: RowHits::default(),
        }
    }

    pub fn set_client(&mut self, client: TalosClient, cp_ip: Option<String>) {
        self.talos = Some(client);
        self.cp_ip = cp_ip;
    }

    pub fn set_error(&mut self, error: String) {
        self.message = Some(error);
    }

    pub fn set_log_wrap(&mut self, wrap: bool) {
        self.logs.set_wrap(wrap);
    }

    pub fn log_wrap(&self) -> bool {
        self.logs.wrap()
    }

    /// For tests: the run files from another directory
    #[cfg(test)]
    fn set_dir(&mut self, dir: PathBuf) {
        self.dir = Some(dir);
    }

    fn ops(&self) -> Vec<OpKind> {
        OpKind::offered(self.group)
    }

    fn reload(&mut self) {
        crate::operations::prune(&self.runs);
        self.history = self
            .dir
            .as_deref()
            .map(crate::operations::load_runs)
            .unwrap_or_default();
        self.loaded = Some(Instant::now());
    }

    fn running(&self) -> Vec<PathBuf> {
        crate::operations::running_paths(&self.runs)
    }

    /// History rows shown, as indexes into `history`
    fn history_rows(&self) -> Vec<usize> {
        let runs: Vec<Run> = self.history.iter().map(|(_, r)| r.clone()).collect();
        let running = self.running();
        let paths: Vec<&PathBuf> = self.history.iter().map(|(p, _)| p).collect();
        let filter = self.filter.to_lowercase();
        let context = self.this_context.then_some(self.context.as_str());
        run_rows(&runs, context, &filter, |run| {
            runs.iter()
                .position(|r| std::ptr::eq(r, run))
                .and_then(|i| paths.get(i))
                .is_some_and(|p| running.contains(p))
        })
    }

    fn row_count(&self) -> usize {
        self.ops().len() + self.history_rows().len()
    }

    fn selected_run(&self) -> Option<&(PathBuf, Run)> {
        let i = self.selected.checked_sub(self.ops().len())?;
        let rows = self.history_rows();
        self.history.get(*rows.get(i)?)
    }

    fn status_of(&self, path: &PathBuf, run: &Run) -> RunStatus {
        run.status(self.running().contains(path))
    }

    fn move_selection(&mut self, step: Move) {
        let last = self.row_count().saturating_sub(1);
        self.selected = match step {
            Move::Prev => self.selected.saturating_sub(1),
            Move::Next => (self.selected + 1).min(last),
            Move::First => 0,
            Move::Last => last,
            Move::By(n) => self.selected.saturating_add_signed(n).min(last),
        };
    }

    /// Enter: an operation's dialog, or a run's output
    fn enter(&mut self) {
        self.message = None;
        let ops = self.ops();
        if let Some(&kind) = ops.get(self.selected) {
            self.open_start(kind);
        } else if let Some((path, _)) = self.selected_run() {
            let path = path.clone();
            self.open_run(path);
        }
    }

    fn open_start(&mut self, kind: OpKind) {
        if let Some(why) = crate::operations::busy(&self.runs, &self.context) {
            self.message = Some(why);
            return;
        }
        if self.nodes.is_empty() {
            self.message = Some("No nodes to run on".to_string());
            return;
        }
        let task = self.talos.clone().map(|talos| {
            let nodes = self.nodes.clone();
            tokio::spawn(fetch_safety(talos, self.cp_ip.clone(), kind, nodes))
        });
        self.dialog = Some(Dialog::Start {
            kind,
            // A group starts with nothing picked: the user chooses the
            // nodes. A single node is the target itself
            picked: vec![!self.group; self.nodes.len()],
            cursor: 0,
            safety: None,
            task,
        });
    }

    fn open_run(&mut self, path: PathBuf) {
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let source = LogSource {
            namespace: String::new(),
            pod: String::new(),
            container: String::new(),
            label: String::new(),
            has_previous: false,
            talos: None,
            file: Some(path.clone()),
        };
        let command = self
            .history
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, r)| r.header.commands.join("\n"))
            .unwrap_or_default();
        self.logs.set_sources(title, vec![source], command);
        self.logs.fetch(None);
        self.focus = Some(path);
    }

    /// `y` in the dialog: start the run and show it
    fn start(&mut self, kind: OpKind, picked: &[bool]) {
        let Some(talos) = self.talos.clone() else {
            self.message = Some("Not connected to the cluster".to_string());
            return;
        };
        let Some(dir) = self.dir.clone() else {
            self.message = Some("No home directory for the run files".to_string());
            return;
        };
        let nodes: Vec<OpNode> = self
            .nodes
            .iter()
            .zip(picked)
            .filter(|(_, p)| **p)
            .map(|(n, _)| n.clone())
            .collect();
        if nodes.is_empty() {
            self.message = Some("No node picked".to_string());
            return;
        }
        let request = OpRequest {
            context: self.context.clone(),
            target: self.target.clone(),
            kind,
            nodes,
            options: DrainOptions::default(),
            delay: DELAY_BETWEEN_NODES,
        };
        match crate::operations::start(&self.runs, &dir, request, talos, self.cp_ip.clone()) {
            Ok(path) => {
                self.reload();
                self.open_run(path);
            }
            Err(e) => self.message = Some(e),
        }
    }

    /// `D`: the filtered runs when filtering, else the selected one; never
    /// one that runs
    fn ask_delete(&mut self) {
        let running = self.running();
        let paths: Vec<PathBuf> = if self.filter.is_empty() {
            self.selected_run()
                .map(|(p, _)| p.clone())
                .into_iter()
                .collect()
        } else {
            self.history_rows()
                .iter()
                .filter_map(|&i| self.history.get(i))
                .map(|(p, _)| p.clone())
                .collect()
        };
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| !running.contains(p)).collect();
        if paths.is_empty() {
            self.message = Some("Nothing to delete (a running run is kept)".to_string());
        } else {
            self.dialog = Some(Dialog::Delete(paths));
        }
    }

    /// `x`: cancel the run going in this context
    fn ask_cancel(&mut self) {
        if crate::operations::busy(&self.runs, &self.context).is_some() {
            self.dialog = Some(Dialog::Cancel(self.context.clone()));
        } else {
            self.message = Some("Nothing runs in this cluster".to_string());
        }
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let yes = matches!(key.code, KeyCode::Char('y' | 'Y'));
        let no = matches!(key.code, KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc);
        match dialog {
            Dialog::Start {
                kind,
                mut picked,
                mut cursor,
                safety,
                task,
            } => {
                if yes && picked.contains(&true) {
                    self.start(kind, &picked);
                    return;
                }
                if no {
                    return;
                }
                match key.code {
                    KeyCode::Char(' ') if self.group => {
                        if let Some(p) = picked.get_mut(cursor) {
                            *p = !*p;
                        }
                    }
                    // All, or none once all are picked
                    KeyCode::Char('a') if self.group => {
                        let all = !picked.iter().all(|p| *p);
                        picked.fill(all);
                    }
                    _ => match Move::from_key(&key) {
                        Some(Move::Prev) => cursor = cursor.saturating_sub(1),
                        Some(Move::Next) => {
                            cursor = (cursor + 1).min(picked.len().saturating_sub(1));
                        }
                        _ => {}
                    },
                }
                self.dialog = Some(Dialog::Start {
                    kind,
                    picked,
                    cursor,
                    safety,
                    task,
                });
            }
            Dialog::Delete(paths) => {
                if yes {
                    let n = crate::operations::delete_runs(&paths);
                    self.message = Some(format!(
                        "Deleted {}",
                        talos9s_core::pluralize(n, "run", "runs")
                    ));
                    self.reload();
                    self.selected = self.selected.min(self.row_count().saturating_sub(1));
                } else if !no {
                    self.dialog = Some(Dialog::Delete(paths));
                }
            }
            Dialog::Cancel(context) => {
                if yes {
                    if let Ok(runs) = self.runs.lock()
                        && let Some(run) = runs.get(&context)
                    {
                        run.cancel();
                    }
                    self.message = Some("Cancelling after the current step".to_string());
                } else if !no {
                    self.dialog = Some(Dialog::Cancel(context));
                }
            }
        }
    }

    fn poll_safety(&mut self) {
        if let Some(Dialog::Start { safety, task, .. }) = &mut self.dialog
            && task.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(Some(Ok(info))) = task.take().map(crate::task::poll_finished)
        {
            *safety = Some(info);
        }
    }

    fn handle_focus_key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.logs.captures_text()
            || self.logs.selecting()
            || (key.code == KeyCode::Esc && self.logs.has_search())
        {
            self.logs.handle_key(&key, None);
            return None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.focus = None;
                self.reload();
            }
            KeyCode::Char('y') => crate::clipboard::copy_to_clipboard(self.logs.command()),
            KeyCode::Char('x') if self.focus_running() => self.ask_cancel(),
            KeyCode::Char('v') => return self.open_in_viewer(),
            KeyCode::Up | KeyCode::Char('k') => self.logs.scroll_lines(-1),
            KeyCode::Down | KeyCode::Char('j') => self.logs.scroll_lines(1),
            _ => {
                self.logs.handle_key(&key, None);
            }
        }
        None
    }

    fn open_in_viewer(&mut self) -> Option<Action> {
        let text = self.logs.export()?;
        let path = std::env::temp_dir().join(format!("talos9s-run-{}.log", std::process::id()));
        match crate::components::workloads::write_private(&path, &text) {
            Ok(()) => Some(Action::OpenInViewer(path)),
            Err(e) => {
                self.logs
                    .set_viewer_problem(format!("Couldn't write {}: {e}", path.display()));
                None
            }
        }
    }

    pub fn viewer_failed(&mut self, problem: String) {
        self.logs.set_viewer_problem(problem);
    }

    fn focus_running(&self) -> bool {
        self.focus
            .as_ref()
            .is_some_and(|p| self.running().contains(p))
    }

    /// The run going in this context, with its progress
    fn with_active<T>(&self, path: &PathBuf, f: impl FnOnce(&ActiveRun) -> T) -> Option<T> {
        let runs = self.runs.lock().ok()?;
        runs.values().find(|r| &r.path == path).map(f)
    }

    fn key_hints_line(&self) -> Line<'static> {
        let key = |k: &'static str| Span::styled(k, KEY);
        let label = |l: &'static str| Span::raw(l);
        if let Some(dialog) = &self.dialog {
            let mut spans = vec![
                Span::styled("<y>", KEY_YES),
                label(" yes  "),
                Span::styled("<n/Esc>", KEY_NO),
                label(" no"),
            ];
            if matches!(dialog, Dialog::Start { .. }) && self.group {
                spans.extend([
                    label("  "),
                    key("<j/k>"),
                    label(" node  "),
                    key("<Space>"),
                    label(" pick"),
                ]);
            }
            return Line::from(spans);
        }
        if self.filtering {
            return Line::from(vec![
                Span::raw(format!(" Filter: {}▏  ", self.filter)),
                key("<Enter>"),
                label(" done  "),
                key("<Esc>"),
                label(" clear"),
            ]);
        }
        if self.focus.is_some() {
            if self.logs.selecting() {
                return Line::from(vec![
                    key("<j/k>"),
                    label(" extend  "),
                    key("<y>"),
                    label(" copy lines  "),
                    key("<Esc>"),
                    label(" cancel"),
                ]);
            }
            let mut spans = vec![
                key("<PgUp/Dn>"),
                label(" scroll  "),
                key("</>"),
                label(" find  "),
                key("<w>"),
                label(" wrap  "),
                key("<V>"),
                label(" copy  "),
                key("<y>"),
                label(" commands  "),
                key("<v>"),
                label(" hl"),
            ];
            if self.focus_running() {
                spans.extend([label("  "), key("<x>"), label(" cancel")]);
            }
            return Line::from(spans);
        }
        let mut spans = vec![
            key("<Enter>"),
            label(" run/open  "),
            key("</>"),
            label(" filter  "),
            key("<A>"),
            label(if self.this_context {
                " all clusters  "
            } else {
                " this cluster  "
            }),
            key("<y>"),
            label(" copy  "),
            key("<D>"),
            label(" delete"),
        ];
        if crate::operations::busy(&self.runs, &self.context).is_some() {
            spans.extend([label("  "), key("<x>"), label(" cancel")]);
        }
        Line::from(spans)
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let ops = self.ops();
        let busy = crate::operations::busy(&self.runs, &self.context);
        let [ops_area, msg_area, history_area] = Layout::vertical([
            Constraint::Length(talos9s_core::convert::to_u16(ops.len() + 1)),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .areas(area);

        let mut lines = vec![Line::from(vec![
            Span::styled(format!(" Run on {}", self.target), Style::default().bold()),
            Span::styled(
                format!(
                    " · {} · context {}",
                    talos9s_core::pluralize(self.nodes.len(), "node", "nodes"),
                    self.context
                ),
                Style::default().dim(),
            ),
        ])];
        for (i, kind) in ops.iter().enumerate() {
            let selected = i == self.selected;
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if busy.is_some() {
                Style::default().dim()
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::raw(if selected { " ▸ " } else { "   " }),
                Span::styled(format!("{:<10}", kind.name()), style),
                Span::styled(
                    format!(" {}", kind.about(self.group)),
                    Style::default().dim(),
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), ops_area);
        self.ops_area = ops_area;

        let note = match (&self.message, &busy) {
            (Some(m), _) => Span::styled(format!(" {m}"), Style::default().fg(Color::Yellow)),
            (None, Some(b)) => Span::styled(format!(" {b}"), Style::default().fg(Color::Cyan)),
            _ => Span::raw(""),
        };
        frame.render_widget(Paragraph::new(Line::from(note)), msg_area);

        let rows_idx = self.history_rows();
        let failed = rows_idx
            .iter()
            .filter_map(|&i| self.history.get(i))
            .filter(|(p, r)| self.status_of(p, r) == RunStatus::Failed)
            .count();
        let mut title = format!(
            " History · {}",
            talos9s_core::pluralize(rows_idx.len(), "run", "runs")
        );
        if failed > 0 {
            let _ = write!(title, " · {failed} failed");
        }
        title.push_str(if self.this_context {
            " · this cluster "
        } else {
            " · all clusters "
        });
        if !self.filter.is_empty() || self.filtering {
            let _ = write!(title, "· /{} ", self.filter);
        }
        let header = Row::new(
            [
                "",
                "WHEN",
                "CONTEXT",
                "TARGET",
                "OPERATION",
                "STATUS",
                "TOOK",
                "COMMAND",
            ]
            .map(|h| Cell::from(h).style(Style::default().bold())),
        );
        let rows: Vec<Row> = rows_idx
            .iter()
            .filter_map(|&i| self.history.get(i))
            .map(|(path, run)| {
                let status = self.status_of(path, run);
                let color = status_color(status);
                let when = run
                    .header
                    .started
                    .with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M:%S")
                    .to_string();
                Row::new(vec![
                    Cell::from("●").style(Style::default().fg(color)),
                    Cell::from(when),
                    Cell::from(run.header.context.clone()),
                    Cell::from(run.header.target.clone()),
                    Cell::from(run.header.operation.clone()),
                    Cell::from(status.name()).style(Style::default().fg(color)),
                    Cell::from(talos9s_core::format_duration_human(run.took())),
                    Cell::from(run.header.commands.first().cloned().unwrap_or_default())
                        .style(Style::default().dim()),
                ])
            })
            .collect();
        let empty = rows.is_empty();
        let widths = [
            Constraint::Length(1),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(18),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Length(5),
            Constraint::Fill(1),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(title),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let in_history = self.selected.checked_sub(ops.len());
        self.table.select(in_history);
        frame.render_stateful_widget(table, history_area, &mut self.table);
        self.rows = RowHits::new(
            Rect {
                y: history_area.y + 1,
                height: history_area.height.saturating_sub(1),
                ..history_area
            },
            false,
            1,
            self.table.offset(),
            rows_idx.len(),
        );
        if empty {
            let inner = Rect {
                y: history_area.y + 2,
                height: 1,
                ..history_area
            };
            frame.render_widget(
                Paragraph::new(" No runs yet").style(Style::default().dim()),
                inner,
            );
        }
    }

    fn draw_run(&mut self, frame: &mut Frame, area: Rect, path: &PathBuf) {
        let run = self
            .history
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, r)| r.clone());
        let progress = self.with_active(path, |r| (r.progress(), r.cancelling()));
        let running = progress.as_ref().is_some_and(|(p, _)| p.end.is_none());
        let mut lines = Vec::new();
        if let Some(run) = &run {
            let h = &run.header;
            let status = if running {
                RunStatus::Running
            } else {
                progress
                    .as_ref()
                    .and_then(|(p, _)| p.end.as_ref().map(|e| e.status))
                    .unwrap_or_else(|| self.status_of(path, run))
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", h.operation), Style::default().bold()),
                Span::raw(format!("on {} · context {} · ", h.target, h.context)),
                Span::styled(status.name(), Style::default().fg(status_color(status))),
                Span::styled(
                    format!(
                        " · started {}",
                        h.started
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S")
                    ),
                    Style::default().dim(),
                ),
            ]));
            if let Some(end) = &run.end
                && let Some(m) = &end.message
            {
                lines.push(Line::from(Span::styled(
                    format!(" {m}"),
                    Style::default().fg(status_color(end.status)),
                )));
            }
            for c in h.commands.iter().take(3) {
                lines.push(Line::from(Span::styled(
                    format!(" $ {c}"),
                    Style::default().dim(),
                )));
            }
            if h.commands.len() > 3 {
                lines.push(Line::from(Span::styled(
                    format!(" … {} more (y copies all)", h.commands.len() - 3),
                    Style::default().dim(),
                )));
            }
        }
        let gauge_height = u16::from(running);
        let [head, gauge_area, body] = Layout::vertical([
            Constraint::Length(talos9s_core::convert::to_u16(lines.len())),
            Constraint::Length(gauge_height),
            Constraint::Min(3),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), head);
        if let Some((p, cancelling)) = progress.filter(|_| running) {
            let total = p.total.max(1);
            let ratio = (p.done as f64 / total as f64).clamp(0.0, 1.0);
            let mut label = format!("{}/{} nodes", p.done, p.total);
            if let Some(node) = &p.node {
                let _ = write!(label, " · {node}");
            }
            if cancelling {
                label.push_str(" · cancelling after this step");
            }
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().fg(Color::Cyan))
                    .ratio(ratio)
                    .label(label),
                gauge_area,
            );
        }
        self.logs.draw(frame, body);
    }

    fn draw_dialog(&self, frame: &mut Frame, area: Rect) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let mut lines: Vec<Line> = Vec::new();
        let title;
        match dialog {
            Dialog::Start {
                kind,
                picked,
                cursor,
                safety,
                ..
            } => {
                title = format!(" {} on {}? ", kind.name(), self.target);
                lines.push(Line::from(Span::styled(
                    format!(" {}", kind.about(self.group)),
                    Style::default().dim(),
                )));
                lines.push(Line::default());
                for (i, (node, p)) in self.nodes.iter().zip(picked).enumerate() {
                    // Picked: a green tick and the name in bold; left out:
                    // dimmed. The cursor is a cyan arrow and bar, readable
                    // in any theme (reverse video on a dim row barely shows)
                    let here = self.group && i == *cursor;
                    let (mark, mark_style, name_style) = if *p {
                        (
                            "[✓]",
                            Style::default().fg(Color::Green),
                            Style::default().add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ("[ ]", Style::default().dim(), Style::default().dim())
                    };
                    let name_style = if here {
                        name_style.fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        name_style
                    };
                    let pointer = if here { "▶" } else { " " };
                    let role = if node.is_controlplane {
                        "control plane"
                    } else {
                        "worker"
                    };
                    let commands =
                        crate::operations::commands_for(*kind, node, &DrainOptions::default());
                    lines.push(Line::from(vec![
                        Span::styled(pointer, Style::default().fg(Color::Cyan)),
                        Span::styled(format!("{mark} "), mark_style),
                        Span::styled(node.hostname.clone(), name_style),
                        Span::styled(format!("  {role}"), Style::default().dim()),
                    ]));
                    for c in commands {
                        lines.push(Line::from(Span::styled(
                            format!("     $ {c}"),
                            Style::default().dim(),
                        )));
                    }
                }
                lines.push(Line::default());
                match safety {
                    None if kind.drains() || kind.takes_down() => {
                        lines.push(Line::from(Span::styled(
                            " Checking etcd and PDBs…",
                            Style::default().dim(),
                        )));
                    }
                    None => {}
                    Some(info) => lines.extend(safety_lines(info)),
                }
            }
            Dialog::Delete(paths) => {
                title = " Delete runs? ".to_string();
                lines.push(Line::from(format!(
                    " Delete {} from the history? The files are removed.",
                    talos9s_core::pluralize(paths.len(), "run", "runs")
                )));
            }
            Dialog::Cancel(context) => {
                title = " Cancel the run? ".to_string();
                lines.push(Line::from(format!(
                    " Stop the run in {context} after its current step?"
                )));
                lines.push(Line::from(Span::styled(
                    " A reboot already sent can't be taken back: it waits for the node.",
                    Style::default().dim(),
                )));
            }
        }
        lines.push(Line::default());
        // `y` acts only once a node is picked; until then say how
        let none_picked = matches!(dialog, Dialog::Start { picked, .. } if !picked.contains(&true));
        if none_picked {
            lines.push(Line::from(Span::styled(
                " Pick the nodes to run on first.",
                Style::default().fg(Color::Yellow),
            )));
        }
        let mut answer = vec![
            Span::raw(" "),
            if none_picked {
                Span::styled("<y>", Style::default().dim())
            } else {
                Span::styled("<y>", KEY_YES)
            },
            Span::raw(" yes   "),
            Span::styled("<n/Esc>", KEY_NO),
            Span::raw(" no"),
        ];
        if self.group && matches!(dialog, Dialog::Start { .. }) {
            answer.extend([
                Span::raw("   "),
                Span::styled("<Space>", KEY),
                Span::styled(" pick  ", Style::default().dim()),
                Span::styled("<a>", KEY),
                Span::styled(" all  ", Style::default().dim()),
                Span::styled("<j/k>", KEY),
                Span::styled(" move", Style::default().dim()),
            ]);
        }
        lines.push(Line::from(answer));
        let width = 76.min(area.width);
        // Rows the lines take once wrapped, so the answer line stays inside
        let inner = usize::from(width.saturating_sub(2)).max(1);
        let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner)).sum();
        let height = talos9s_core::convert::to_u16(rows + 2).min(area.height);
        let popup = crate::ui_ext::centered(area, width, height);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(Span::styled(title, Style::default().bold())),
            ),
            popup,
        );
    }
}

fn status_color(status: RunStatus) -> Color {
    match status {
        RunStatus::Running => Color::Cyan,
        RunStatus::Ok => Color::Green,
        RunStatus::Failed => Color::Red,
        RunStatus::Cancelled | RunStatus::Interrupted => Color::DarkGray,
    }
}

/// The checks as dialog lines: etcd without each control plane node, PDBs
fn safety_lines(info: &SafetyInfo) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let colored = |text: String, s: &SafetyStatus| {
        let color = match s {
            SafetyStatus::Safe => Color::Green,
            SafetyStatus::Warning(_) => Color::Yellow,
            SafetyStatus::Unsafe(_) => Color::Red,
            SafetyStatus::Unknown => Color::DarkGray,
        };
        Line::from(Span::styled(text, Style::default().fg(color)))
    };
    for (node, etcd) in &info.etcd {
        match etcd {
            Some(e) => {
                let healthy = e
                    .healthy_members
                    .map_or_else(|| "?".to_string(), |h| h.to_string());
                let verdict = match &e.safety {
                    SafetyStatus::Safe => "keeps quorum without it".to_string(),
                    SafetyStatus::Warning(m) | SafetyStatus::Unsafe(m) => m.clone(),
                    SafetyStatus::Unknown => "quorum unknown".to_string(),
                };
                lines.push(colored(
                    format!(
                        " etcd {healthy}/{} healthy · {node}: {verdict}",
                        e.total_members
                    ),
                    &e.safety,
                ));
            }
            None => lines.push(colored(
                format!(" etcd unreadable · {node}: the run waits until it is safe"),
                &SafetyStatus::Unknown,
            )),
        }
    }
    if let Some(e) = &info.k8s_error {
        lines.push(colored(
            format!(" No Kubernetes client: {e}"),
            &SafetyStatus::Unsafe(String::new()),
        ));
    }
    match &info.pdb {
        Some(p) if p.has_blocking_pdbs() => {
            let names: Vec<String> = p
                .blocking_pdbs
                .iter()
                .take(3)
                .map(|b| format!("{}/{}", b.namespace, b.name))
                .collect();
            lines.push(colored(
                format!(
                    " {} PDB(s) allow no disruption now: {}",
                    p.blocking_pdbs.len(),
                    names.join(", ")
                ),
                &SafetyStatus::Warning(String::new()),
            ));
        }
        Some(_) => lines.push(colored(
            " No PDB blocks a drain".to_string(),
            &SafetyStatus::Safe,
        )),
        None if info.k8s_error.is_none() && !info.etcd.is_empty() => {}
        None => {}
    }
    lines
}

impl Component for OperationsComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if self.dialog.is_some() {
            return Ok(None);
        }
        if self.focus.is_some() {
            self.logs.handle_mouse(&mouse);
            return Ok(None);
        }
        if let Some(row) = self.rows.clicked(&mouse) {
            self.selected = self.ops().len() + row;
        } else if mouse.kind
            == crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
            && mouse.row > self.ops_area.y
            && mouse.row < self.ops_area.bottom()
            && mouse.column >= self.ops_area.x
            && mouse.column < self.ops_area.right()
        {
            self.selected = usize::from(mouse.row - self.ops_area.y - 1);
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn captures_text(&self) -> bool {
        self.filtering || self.dialog.is_some() || self.logs.captures_text()
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if self.dialog.is_some() {
            self.handle_dialog_key(key);
            return Ok(None);
        }
        if self.filtering {
            use crate::ui_ext::{TextEdit, edit_text};
            match edit_text(&mut self.filter, &key) {
                TextEdit::Submit => self.filtering = false,
                TextEdit::Cancel => {
                    self.filter.clear();
                    self.filtering = false;
                }
                TextEdit::Changed => self.selected = self.selected.min(self.ops().len()),
                TextEdit::Ignored => {}
            }
            return Ok(None);
        }
        if self.focus.is_some() {
            return Ok(self.handle_focus_key(key));
        }
        self.message = None;
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => self.filter.clear(),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Some(Action::Back)),
            KeyCode::Enter => self.enter(),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('A') => {
                self.this_context = !self.this_context;
                self.selected = self.selected.min(self.row_count().saturating_sub(1));
            }
            KeyCode::Char('y') => {
                if let Some((_, run)) = self.selected_run() {
                    crate::clipboard::copy_to_clipboard(run.header.commands.join("\n"));
                }
            }
            KeyCode::Char('D') => self.ask_delete(),
            KeyCode::Char('x') => self.ask_cancel(),
            KeyCode::Char('r') => self.reload(),
            _ => {
                if let Some(step) = Move::from_key(&key) {
                    self.move_selection(step);
                }
            }
        }
        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        match action {
            Action::Tick => {
                self.poll_safety();
                let every = if self.running().is_empty() {
                    RELOAD_IDLE
                } else {
                    RELOAD_RUNNING
                };
                if self.loaded.is_none_or(|t| t.elapsed() >= every) {
                    self.reload();
                }
                if self.focus.is_some() {
                    self.logs.tick(None);
                }
            }
            Action::Refresh => self.reload(),
            _ => {}
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        if self.loaded.is_none() {
            self.reload();
        }
        match self.focus.clone() {
            Some(path) => self.draw_run(frame, area, &path),
            None => self.draw_list(frame, area),
        }
        self.draw_dialog(frame, area);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::Arc;
    use talos9s_core::operations::{RunEnd, RunHeader, format_end, format_header};

    fn node(h: &str) -> OpNode {
        OpNode {
            hostname: h.into(),
            address: format!("10.0.0.{}", h.len()),
            is_controlplane: true,
        }
    }

    fn with_history(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        for (i, (ctx, op, status)) in [
            ("bc", "Drain", RunStatus::Ok),
            ("kprod", "Reboot", RunStatus::Failed),
        ]
        .into_iter()
        .enumerate()
        {
            let started =
                chrono::Utc::now() - chrono::Duration::hours(i64::try_from(i).unwrap() + 1);
            let h = RunHeader {
                context: ctx.into(),
                target: "n1".into(),
                nodes: vec!["n1".into()],
                operation: op.into(),
                commands: vec![format!("kubectl {op} n1")],
                started,
            };
            let text = format_header(&h)
                + &format_end(&RunEnd {
                    finished: started,
                    status,
                    message: None,
                });
            std::fs::write(dir.join(talos9s_core::operations::file_name(&h)), text).unwrap();
        }
    }

    fn component(dir: &std::path::Path, group: bool) -> OperationsComponent {
        let mut c = OperationsComponent::new(
            "bc".into(),
            if group { "bc".into() } else { "bc1".into() },
            if group {
                vec![node("bc1"), node("bc2")]
            } else {
                vec![node("bc1")]
            },
            group,
            Arc::default(),
        );
        c.set_dir(dir.to_path_buf());
        c
    }

    fn press(c: &mut OperationsComponent, code: KeyCode) {
        c.handle_key_event(KeyEvent::from(code)).unwrap();
    }

    /// Operations, then the history of every context; `A` narrows it, `/`
    /// filters
    #[test]
    fn lists_operations_and_history() {
        let dir = std::env::temp_dir().join(format!("talos9s-opsview-{}", std::process::id()));
        with_history(&dir);
        let mut c = component(&dir, false);
        c.reload();
        assert_eq!(c.ops().len(), 5);
        assert_eq!(c.history_rows().len(), 2);
        press(&mut c, KeyCode::Char('A'));
        assert_eq!(c.history_rows().len(), 1, "this cluster only");
        press(&mut c, KeyCode::Char('A'));
        press(&mut c, KeyCode::Char('/'));
        for ch in "failed".chars() {
            press(&mut c, KeyCode::Char(ch));
        }
        press(&mut c, KeyCode::Enter);
        assert_eq!(c.history_rows().len(), 1);

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| c.draw(f, f.area()).unwrap()).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(screen.contains("Run on bc1"));
        assert!(screen.contains("Shutdown"));
        assert!(screen.contains("Reboot"));
        assert!(screen.contains("failed"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A group offers no shutdown; Enter on an operation opens its dialog,
    /// `n` closes it without starting anything; Space unpicks a node
    #[test]
    fn dialog_opens_and_cancels() {
        let dir = std::env::temp_dir().join(format!("talos9s-opsdlg-{}", std::process::id()));
        let mut c = component(&dir, true);
        assert!(!c.ops().contains(&OpKind::Shutdown));
        press(&mut c, KeyCode::Enter);
        let Some(Dialog::Start { kind, .. }) = &c.dialog else {
            panic!("no dialog");
        };
        assert_eq!(*kind, OpKind::Cordon);
        assert!(c.captures_text());
        press(&mut c, KeyCode::Char(' '));
        let Some(Dialog::Start { picked, .. }) = &c.dialog else {
            panic!("no dialog");
        };
        assert_eq!(picked, &[true, false]);
        // `y` with nothing picked keeps the dialog open
        press(&mut c, KeyCode::Char(' '));
        press(&mut c, KeyCode::Char('y'));
        assert!(c.dialog.is_some());
        press(&mut c, KeyCode::Char('a'));
        let Some(Dialog::Start { picked, .. }) = &c.dialog else {
            panic!("no dialog");
        };
        assert_eq!(picked, &[true, true]);
        press(&mut c, KeyCode::Char('n'));
        assert!(c.dialog.is_none());
        assert!(c.history_rows().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Every mode's key line fits beside the tree hint
    #[test]
    fn key_lines_fit() {
        let dir = std::env::temp_dir().join(format!("talos9s-opskeys-{}", std::process::id()));
        let mut c = component(&dir, true);
        assert!(c.key_hints_line().width() <= 85, "{}", c.key_hints_line());
        c.focus = Some(dir.join("x.log"));
        assert!(c.key_hints_line().width() <= 85, "{}", c.key_hints_line());
        c.focus = None;
        press(&mut c, KeyCode::Enter);
        assert!(c.key_hints_line().width() <= 85, "{}", c.key_hints_line());
    }
}
