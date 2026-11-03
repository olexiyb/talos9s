//! Operations view (`o`): the operations for the selected node, group or
//! cluster, and every run so far (`plans/operations.md`).
//!
//! The top picks what to run: the operation (a drop list) and the nodes
//! (check boxes; a single node is checked already). Enter opens the
//! confirmation (the commands, the safety checks), `y` starts it. Runs go on in the
//! background (`crate::operations`), one per context; the history below
//! lists the run files, newest first, and Enter shows one's output in the
//! log pane, following it while it runs.

use crate::action::Action;
use crate::components::Component;
use crate::components::diagnostics::k8s::{Fate, PodImpact};
use crate::components::drain_options::{DrainChoice, Opt, Preview};
use crate::components::pod_logs::LogPane;
use crate::operations::{
    ActiveRun, K8sNode, NodeFacts, NodeFactsResult, OpKind, OpNode, OpRequest, Runs, SafetyInfo,
    TalosState, fetch_node_facts, fetch_safety,
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
use std::collections::HashMap;
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
/// How often the shown run's file is read while it runs
const RUN_FOLLOW_INTERVAL: Duration = Duration::from_millis(500);
/// Read the nodes' state (Talos, Kubernetes, pods) again this often, and
/// whenever a run in this cluster ends
const FACTS_EVERY: Duration = Duration::from_secs(15);
/// Pod rows `PgUp`/`PgDn` move in the drain confirmation
const IMPACT_PAGE: usize = 10;
/// How often the open drain confirmation reads its checks again
const SAFETY_EVERY: Duration = Duration::from_secs(10);
/// Pod rows one wheel step moves
const IMPACT_WHEEL: usize = 3;

/// A dialog over the view
enum Dialog {
    /// Start `kind` on the checked nodes: read the checks, `y` starts
    Start {
        kind: OpKind,
        nodes: Vec<OpNode>,
        safety: Option<SafetyInfo>,
        task: Option<JoinHandle<SafetyInfo>>,
        /// The drain option row the cursor is on (`Opt::shown`)
        cursor: usize,
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
    /// Over the top rows (`TopRow`), then the history rows
    selected: usize,
    /// The operation picked, an index into `ops()`
    kind: usize,
    /// The nodes checked; a single node is checked from the start
    picked: Vec<bool>,
    /// The operation drop list, open, with its cursor
    dropdown: Option<usize>,
    /// Where the drop list was drawn, for clicks
    dropdown_area: Rect,
    /// Each node's state now, by hostname
    facts: HashMap<String, NodeFacts>,
    facts_error: Option<String>,
    facts_task: Option<JoinHandle<NodeFactsResult>>,
    facts_at: Option<Instant>,
    /// A run went in this cluster at the last tick: its end reloads facts
    was_busy: bool,
    table: TableState,
    filter: String,
    filtering: bool,
    /// `A`: narrow the history to this node's runs (a node, the default
    /// there) or this cluster's (a group); wide is every run
    this_context: bool,
    dialog: Option<Dialog>,
    /// The drain options the start dialog shows and the run gets
    drain: DrainChoice,
    /// Where the dialog's option rows were drawn: the first row's area, for
    /// clicks
    option_rows: Rect,
    /// The first pod row shown in the drain confirmation (`PgUp`/`PgDn`)
    impact_scroll: usize,
    /// The drain confirmation reads its checks again every `SAFETY_EVERY`
    /// (`a` toggles), so pods that end show as gone
    safety_auto: bool,
    /// When the confirmation's checks were last read
    safety_at: Option<Instant>,
    /// The run shown (Enter), and its output
    focus: Option<PathBuf>,
    /// Whether the run shown was running at the last tick, for the read
    /// after it ends
    focus_was_running: bool,
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
        let picked = vec![!group; nodes.len()];
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
            kind: 0,
            picked,
            dropdown: None,
            dropdown_area: Rect::default(),
            facts: HashMap::new(),
            facts_error: None,
            facts_task: None,
            facts_at: None,
            was_busy: false,
            table: TableState::default(),
            filter: String::new(),
            filtering: false,
            // A node shows its own runs first; a group every run
            this_context: !group,
            dialog: None,
            drain: DrainChoice::default(),
            option_rows: Rect::default(),
            impact_scroll: 0,
            safety_auto: true,
            safety_at: None,
            focus: None,
            focus_was_running: false,
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

    /// The drain options last started with in this cluster
    pub fn set_drain(&mut self, choice: &DrainChoice) {
        self.drain = choice.clone();
    }

    /// The operation picked, to carry to the next target
    pub fn operation(&self) -> OpKind {
        self.kind()
    }

    /// Pick `kind` as the last target had it; one not offered here (Shutdown
    /// on a group) leaves the first
    pub fn set_operation(&mut self, kind: OpKind) {
        if let Some(i) = self.ops().iter().position(|k| *k == kind) {
            self.kind = i;
        }
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
        // On a node, narrowed: only the runs that included it
        let node = (self.this_context && !self.group)
            .then(|| self.nodes.first().map(|n| n.hostname.as_str()))
            .flatten();
        run_rows(&runs, context, &filter, |run| {
            runs.iter()
                .position(|r| std::ptr::eq(r, run))
                .and_then(|i| paths.get(i))
                .is_some_and(|p| running.contains(p))
        })
        .into_iter()
        .filter(|&i| {
            node.is_none_or(|n| {
                runs.get(i)
                    .is_some_and(|r| r.header.nodes.iter().any(|x| x == n))
            })
        })
        .collect()
    }

    /// The operation picked
    fn kind(&self) -> OpKind {
        self.ops().get(self.kind).copied().unwrap_or(OpKind::Cordon)
    }

    /// Rows above the history: the operation, "all nodes" on a group, the
    /// nodes
    fn top_rows(&self) -> usize {
        1 + usize::from(self.group) + self.nodes.len()
    }

    fn top_row(&self, row: usize) -> Option<TopRow> {
        match (row, self.group) {
            (0, _) => Some(TopRow::Operation),
            (1, true) => Some(TopRow::All),
            _ => {
                let i = row - 1 - usize::from(self.group);
                (i < self.nodes.len()).then_some(TopRow::Node(i))
            }
        }
    }

    fn row_count(&self) -> usize {
        self.top_rows() + self.history_rows().len()
    }

    fn selected_run(&self) -> Option<&(PathBuf, Run)> {
        let i = self.selected.checked_sub(self.top_rows())?;
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

    /// Enter: the drop list on the operation, the confirmation on a node,
    /// a run's output in the history
    fn enter(&mut self) {
        self.message = None;
        match self.top_row(self.selected) {
            Some(TopRow::Operation) => self.dropdown = Some(self.kind),
            Some(_) => self.open_start(),
            None => {
                if let Some((path, _)) = self.selected_run() {
                    let path = path.clone();
                    self.open_run(path);
                }
            }
        }
    }

    /// Space: open the drop list, or check a node (all of them on "all")
    fn toggle(&mut self) {
        match self.top_row(self.selected) {
            Some(TopRow::Operation) => self.dropdown = Some(self.kind),
            // Every node the operation can run on, or none once they are
            Some(TopRow::All) => {
                let open: Vec<usize> = (0..self.nodes.len())
                    .filter(|&i| self.blocked(i).is_none())
                    .collect();
                let all = !open
                    .iter()
                    .all(|&i| self.picked.get(i).copied().unwrap_or(false));
                self.picked.fill(false);
                for i in open {
                    if let Some(p) = self.picked.get_mut(i) {
                        *p = all;
                    }
                }
            }
            Some(TopRow::Node(i)) if self.blocked(i).is_some() => {
                self.message = self.blocked(i).map(|why| {
                    format!(
                        "{} can't run on {}: {why}",
                        self.kind().name(),
                        self.nodes.get(i).map_or("", |n| n.hostname.as_str())
                    )
                });
            }
            // A single node is the target itself: always checked
            Some(TopRow::Node(i)) if self.group => {
                if let Some(p) = self.picked.get_mut(i) {
                    *p = !*p;
                }
            }
            _ => {}
        }
    }

    /// Pick the `i`th operation and move to the nodes: checking them is
    /// the next step
    fn pick_operation(&mut self, i: usize) {
        if i >= self.ops().len() {
            return;
        }
        self.kind = i;
        self.dropdown = None;
        self.drop_blocked();
        // The first row under the operation: "all nodes", or the node
        self.selected = 1;
    }

    fn handle_dropdown_key(&mut self, key: KeyEvent, cursor: usize) {
        let last = self.ops().len().saturating_sub(1);
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            self.pick_operation(usize::from(c as u8 - b'1'));
            return;
        }
        self.dropdown = match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.pick_operation(cursor);
                None
            }
            KeyCode::Esc | KeyCode::Char('q') => None,
            _ => match Move::from_key(&key) {
                Some(Move::Prev) => Some(cursor.saturating_sub(1)),
                Some(Move::Next) => Some((cursor + 1).min(last)),
                Some(Move::First) => Some(0),
                Some(Move::Last) => Some(last),
                _ => Some(cursor),
            },
        };
    }

    /// Why the picked operation can't run on the `i`th node
    fn blocked(&self, i: usize) -> Option<&'static str> {
        let node = self.nodes.get(i)?;
        let facts = self.facts.get(&node.hostname)?;
        crate::operations::blocked(self.kind(), facts)
    }

    /// Uncheck nodes the operation can't run on (after the operation or the
    /// facts change); a single node keeps its check, it is the target
    fn drop_blocked(&mut self) {
        if !self.group {
            return;
        }
        for i in 0..self.picked.len() {
            if self.blocked(i).is_some()
                && let Some(p) = self.picked.get_mut(i)
            {
                *p = false;
            }
        }
    }

    fn checked_nodes(&self) -> Vec<OpNode> {
        self.nodes
            .iter()
            .zip(&self.picked)
            .enumerate()
            .filter(|(i, (_, p))| **p && self.blocked(*i).is_none())
            .map(|(_, (n, _))| n.clone())
            .collect()
    }

    /// Start reading the nodes' facts when due
    fn load_facts(&mut self) {
        let busy = crate::operations::busy(&self.runs, &self.context).is_some();
        let run_ended = self.was_busy && !busy;
        self.was_busy = busy;
        if self.facts_task.is_some() {
            return;
        }
        let due = run_ended || self.facts_at.is_none_or(|t| t.elapsed() >= FACTS_EVERY);
        if let (true, Some(talos)) = (due, self.talos.clone()) {
            self.facts_at = Some(Instant::now());
            self.facts_task = Some(tokio::spawn(fetch_node_facts(
                talos,
                self.cp_ip.clone(),
                self.nodes.clone(),
            )));
        }
    }

    fn poll_facts(&mut self) {
        if self
            .facts_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
            && let Some(Some(Ok(result))) = self.facts_task.take().map(crate::task::poll_finished)
        {
            self.facts = result.facts;
            self.facts_error = result.k8s_error;
            self.drop_blocked();
        }
    }

    /// The newest run that included `hostname`: operation, status, when
    fn last_run(&self, hostname: &str) -> Option<(String, RunStatus, String)> {
        let (path, run) = self
            .history
            .iter()
            .filter(|(_, r)| r.header.context == self.context)
            .filter(|(_, r)| r.header.nodes.iter().any(|n| n == hostname))
            .max_by_key(|(_, r)| r.header.started)?;
        let when = run
            .header
            .started
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        Some((
            run.header.operation.clone(),
            self.status_of(path, run),
            when,
        ))
    }

    fn open_start(&mut self) {
        if let Some(why) = crate::operations::busy(&self.runs, &self.context) {
            self.message = Some(why);
            return;
        }
        let nodes = self.checked_nodes();
        if nodes.is_empty() {
            self.message = Some(match (self.group, self.blocked(0)) {
                (false, Some(why)) => format!("{} can't run here: {why}", self.kind().name()),
                _ => "Check the nodes to run on first (Space)".to_string(),
            });
            return;
        }
        let kind = self.kind();
        let task = self.spawn_safety(kind, &nodes);
        self.dialog = Some(Dialog::Start {
            kind,
            nodes,
            safety: None,
            task,
            cursor: 0,
        });
        self.impact_scroll = 0;
        self.safety_at = None;
    }

    /// Read the checks the confirmation shows (etcd, PDBs, the pods)
    fn spawn_safety(&self, kind: OpKind, nodes: &[OpNode]) -> Option<JoinHandle<SafetyInfo>> {
        self.talos.clone().map(|talos| {
            tokio::spawn(fetch_safety(
                talos,
                self.cp_ip.clone(),
                kind,
                nodes.to_vec(),
            ))
        })
    }

    /// `r` in the drain confirmation, or its auto-refresh: read the checks
    /// again, the old ones staying on screen until the new arrive
    fn refresh_safety(&mut self) {
        let Some(Dialog::Start {
            kind, nodes, task, ..
        }) = &self.dialog
        else {
            return;
        };
        if task.is_some() {
            return;
        }
        let new = self.spawn_safety(*kind, nodes);
        if let Some(Dialog::Start { task, .. }) = &mut self.dialog {
            *task = new;
        }
    }

    /// What the run is on: the group when all its nodes are checked, else
    /// the nodes (a run on bc1 from the cluster is "bc1", not "bc")
    fn run_target(&self, nodes: &[OpNode]) -> String {
        if nodes.len() == self.nodes.len() {
            self.target.clone()
        } else {
            nodes
                .iter()
                .map(|n| n.hostname.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
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
    fn start(&mut self, kind: OpKind, nodes: Vec<OpNode>) {
        let Some(talos) = self.talos.clone() else {
            self.message = Some("Not connected to the cluster".to_string());
            return;
        };
        let Some(dir) = self.dir.clone() else {
            self.message = Some("No home directory for the run files".to_string());
            return;
        };
        let request = OpRequest {
            context: self.context.clone(),
            target: self.run_target(&nodes),
            kind,
            nodes,
            options: self.drain.options.clone(),
            delay: self.drain.between,
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

    fn handle_dialog_key(&mut self, key: KeyEvent) -> Option<Action> {
        let dialog = self.dialog.take()?;
        let yes = matches!(key.code, KeyCode::Char('y' | 'Y'));
        let no = matches!(key.code, KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc);
        match dialog {
            Dialog::Start {
                kind,
                nodes,
                safety,
                task,
                mut cursor,
            } => {
                if yes {
                    let drains = kind.drains();
                    self.start(kind, nodes);
                    return drains.then(|| Action::SaveDrain(self.drain.to_state(&self.context)));
                }
                if no {
                    return None;
                }
                let opts = Opt::shown(kind, nodes.len());
                let opt = opts.get(cursor).copied();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => cursor = cursor.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        cursor = (cursor + 1).min(opts.len().saturating_sub(1));
                    }
                    KeyCode::Char('r') => {
                        self.dialog = Some(Dialog::Start {
                            kind,
                            nodes,
                            safety,
                            task,
                            cursor,
                        });
                        self.refresh_safety();
                        return None;
                    }
                    KeyCode::Char('a') => self.safety_auto = !self.safety_auto,
                    KeyCode::PageDown => self.impact_scroll += IMPACT_PAGE,
                    KeyCode::PageUp => {
                        self.impact_scroll = self.impact_scroll.saturating_sub(IMPACT_PAGE);
                    }
                    KeyCode::Char(' ') => {
                        if let Some(opt) = opt {
                            self.drain.toggle(opt);
                        }
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') => {
                        let forward = matches!(key.code, KeyCode::Right | KeyCode::Char('l'));
                        if let Some(opt) = opt {
                            self.drain.step(opt, forward);
                        }
                    }
                    _ => {}
                }
                self.dialog = Some(Dialog::Start {
                    kind,
                    nodes,
                    safety,
                    task,
                    cursor,
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
        None
    }

    fn poll_safety(&mut self) {
        if let Some(Dialog::Start { safety, task, .. }) = &mut self.dialog
            && task.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(Some(Ok(info))) = task.take().map(crate::task::poll_finished)
        {
            *safety = Some(info);
            self.safety_at = Some(Instant::now());
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

    /// "running Reboot" while a run goes in this view's cluster, for the
    /// window title
    pub fn running_here(&self) -> Option<String> {
        let runs = self.runs.lock().ok()?;
        let run = runs.get(&self.context).filter(|r| !r.finished())?;
        Some(format!("running {}", run.header.operation))
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
        if let Some(Dialog::Start { kind, nodes, .. }) = &self.dialog
            && !Opt::shown(*kind, nodes.len()).is_empty()
        {
            return Line::from(vec![
                key("<j/k/Space/←/→>"),
                label(" edit  "),
                key("<PgUp/PgDn>"),
                label(" scroll  "),
                key("<r/a>"),
                label(" refresh  "),
                Span::styled("<y>", KEY_YES),
                label(" start  "),
                Span::styled("<n/Esc>", KEY_NO),
                label(" cancel"),
            ]);
        }
        if self.dialog.is_some() {
            return Line::from(vec![
                Span::styled("<y>", KEY_YES),
                label(" yes  "),
                Span::styled("<n/Esc>", KEY_NO),
                label(" no"),
            ]);
        }
        if self.dropdown.is_some() {
            return Line::from(vec![
                key("<j/k>"),
                label(" move  "),
                key("<Enter/1-5>"),
                label(" pick  "),
                key("<Esc>"),
                label(" close"),
            ]);
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
        let busy = crate::operations::busy(&self.runs, &self.context).is_some();
        let cancel = [label("  "), key("<x>"), label(" cancel")];
        match self.top_row(self.selected) {
            Some(TopRow::Operation) => {
                let mut spans = vec![
                    key("<Enter/o>"),
                    label(" choose the operation  "),
                    key("<1-5>"),
                    label(" pick one"),
                ];
                if busy {
                    spans.extend(cancel);
                }
                return Line::from(spans);
            }
            Some(_) => {
                let mut spans = vec![
                    key("<Space>"),
                    label(" check  "),
                    key("<Enter>"),
                    label(" start (asks first)  "),
                    key("<o/1-5>"),
                    label(" operation"),
                ];
                if busy {
                    spans.extend(cancel);
                }
                return Line::from(spans);
            }
            None => {}
        }
        let mut spans = vec![
            key("<Enter>"),
            label(" open  "),
            key("</>"),
            label(" filter  "),
            key("<A>"),
            label(match (self.this_context, self.group) {
                (true, _) => " all runs  ",
                (false, true) => " this cluster  ",
                (false, false) => " this node  ",
            }),
            key("<y>"),
            label(" copy  "),
            key("<D>"),
            label(" delete"),
        ];
        if busy {
            spans.extend(cancel);
        }
        Line::from(spans)
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let busy = crate::operations::busy(&self.runs, &self.context);
        let [ops_area, msg_area, history_area] = Layout::vertical([
            Constraint::Length(talos9s_core::convert::to_u16(self.top_rows() + 2)),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .areas(area);

        let checked = self.picked.iter().filter(|p| **p).count();
        let mut lines = vec![Line::from(vec![
            Span::styled(format!(" Run on {}", self.target), Style::default().bold()),
            Span::styled(
                format!(
                    " · {} of {} checked · context {}",
                    checked,
                    talos9s_core::pluralize(self.nodes.len(), "node", "nodes"),
                    self.context
                ),
                Style::default().dim(),
            ),
        ])];
        let pointer = |row: usize| {
            if row == self.selected {
                Span::styled(crate::ui_ext::PICK_POINTER, crate::ui_ext::PICK_CURSOR)
            } else {
                Span::raw(" ")
            }
        };
        let here = |row: usize, style: Style| {
            if row == self.selected {
                crate::ui_ext::PICK_CURSOR
            } else {
                style
            }
        };
        let kind = self.kind();
        lines.push(Line::from(vec![
            pointer(0),
            Span::styled("Operation  ", Style::default().dim()),
            Span::styled(
                format!("[ {:<9}▾ ]", kind.name()),
                here(0, Style::default().bold()),
            ),
            Span::styled(
                format!("  {}", kind.about(self.group)),
                Style::default().dim(),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "     {:<12} {:<13} {:<10} {:<9} {:<10} {:>4}  LAST RUN",
                "NODE", "ROLE", "TALOS", "K8S", "SCHEDULE", "PODS"
            ),
            Style::default().bold(),
        )));
        if self.group {
            let open: Vec<usize> = (0..self.nodes.len())
                .filter(|&i| self.blocked(i).is_none())
                .collect();
            let all = !open.is_empty()
                && open
                    .iter()
                    .all(|&i| self.picked.get(i).copied().unwrap_or(false));
            lines.push(Line::from(vec![
                pointer(1),
                crate::ui_ext::check_box(all),
                Span::styled("all nodes", here(1, Style::default().dim())),
            ]));
        }
        let first = 1 + usize::from(self.group);
        for (i, (node, on)) in self.nodes.iter().zip(&self.picked).enumerate() {
            let row = first + i;
            let style = if *on {
                Style::default().bold()
            } else {
                crate::ui_ext::PICK_DIM
            };
            let role = if node.is_controlplane {
                "control plane"
            } else {
                "worker"
            };
            let blocked = self.blocked(i);
            let style = if blocked.is_some() {
                crate::ui_ext::PICK_DIM
            } else {
                style
            };
            let check = if blocked.is_some() {
                Span::styled("[-] ", crate::ui_ext::PICK_DIM)
            } else {
                crate::ui_ext::check_box(*on)
            };
            let facts = self.facts.get(&node.hostname);
            let mut spans = vec![
                pointer(row),
                check,
                Span::styled(format!("{:<12}", node.hostname), here(row, style)),
                Span::styled(format!(" {role:<13}"), Style::default().dim()),
            ];
            spans.extend(fact_cells(facts, self.facts_task.is_some()));
            match (blocked, self.last_run(&node.hostname)) {
                (Some(why), _) => {
                    spans.push(Span::styled(
                        format!("  {why}"),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                (None, Some((op, status, when))) => {
                    spans.push(Span::styled(format!("  {op} "), Style::default().dim()));
                    spans.push(Span::styled(
                        status.name(),
                        Style::default().fg(status_color(status)),
                    ));
                    spans.push(Span::styled(format!(" {when}"), Style::default().dim()));
                }
                (None, None) => {}
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), ops_area);
        self.ops_area = ops_area;

        let note = match (&self.message, &busy, &self.facts_error) {
            (Some(m), _, _) => Span::styled(format!(" {m}"), Style::default().fg(Color::Yellow)),
            (None, Some(b), _) => Span::styled(format!(" {b}"), Style::default().fg(Color::Cyan)),
            (None, None, Some(e)) => Span::styled(
                format!(" Kubernetes unreadable ({e}): K8S, SCHEDULE and PODS unknown"),
                Style::default().dim(),
            ),
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
        let scope = match (self.this_context, self.group) {
            (true, true) => " · this cluster ".to_string(),
            (true, false) => format!(" · {} only ", self.target),
            (false, _) => " · all clusters ".to_string(),
        };
        title.push_str(&scope);
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
                    Cell::from(status_label(status)).style(Style::default().fg(color)),
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
        let in_history = self.selected.checked_sub(self.top_rows());
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
                Span::styled(
                    status_label(status),
                    Style::default().fg(status_color(status)),
                ),
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

    fn draw_dialog(&mut self, frame: &mut Frame, area: Rect) {
        self.option_rows = Rect::default();
        let Some(dialog) = &self.dialog else {
            return;
        };
        // A drain gets the whole view: its options and what happens to
        // every pod don't fit a popup
        if let Dialog::Start { kind, nodes, .. } = dialog
            && !Opt::shown(*kind, nodes.len()).is_empty()
        {
            self.draw_drain_start(frame, area);
            return;
        }
        let width = 76.min(area.width);
        // Rows lines take once wrapped, so the answer line stays inside
        let inner = usize::from(width.saturating_sub(2)).max(1);
        let rows_of = |lines: &[Line]| -> usize {
            lines.iter().map(|l| l.width().max(1).div_ceil(inner)).sum()
        };
        let mut lines: Vec<Line> = Vec::new();
        let title;
        match dialog {
            Dialog::Start {
                kind,
                nodes,
                safety,
                ..
            } => {
                title = format!(" {} on {}? ", kind.name(), self.run_target(nodes));
                lines.push(Line::from(Span::styled(
                    format!(" {}", kind.about(nodes.len() > 1)),
                    Style::default().dim(),
                )));
                lines.push(Line::default());
                for node in nodes {
                    let role = if node.is_controlplane {
                        "control plane"
                    } else {
                        "worker"
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {}", node.hostname), Style::default().bold()),
                        Span::styled(format!("  {role}"), Style::default().dim()),
                    ]));
                    for c in crate::operations::commands_for(
                        &self.context,
                        *kind,
                        node,
                        &self.drain.options,
                    ) {
                        lines.push(Line::from(Span::styled(
                            format!("   $ {c}"),
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
        let answer = vec![
            Span::raw(" "),
            Span::styled("<y>", KEY_YES),
            Span::raw(" yes   "),
            Span::styled("<n/Esc>", KEY_NO),
            Span::raw(" no"),
        ];
        lines.push(Line::from(answer));
        let height = talos9s_core::convert::to_u16(rows_of(&lines) + 2).min(area.height);
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

impl OperationsComponent {
    /// Whether the drain confirmation is open (it takes the whole screen)
    fn drain_start_open(&self) -> bool {
        matches!(&self.dialog, Some(Dialog::Start { kind, nodes, .. })
            if !Opt::shown(*kind, nodes.len()).is_empty())
    }

    /// The confirmation of a drain, reboot or shutdown, over the whole
    /// view: options, every pod on the checked nodes and what happens to
    /// it, the commands and the safety checks. `y` starts, `n` cancels
    fn draw_drain_start(&mut self, frame: &mut Frame, area: Rect) {
        let Some(Dialog::Start {
            kind,
            nodes,
            safety,
            cursor,
            task,
        }) = &self.dialog
        else {
            return;
        };
        let refreshing = task.is_some() && safety.is_some();
        let (kind, cursor) = (*kind, *cursor);
        let opts = Opt::shown(kind, nodes.len());
        let preview = match safety {
            None => Preview::Reading,
            Some(SafetyInfo {
                drain: Some(pods), ..
            }) => Preview::Pods(pods),
            Some(_) => Preview::Unknown,
        };
        let options = &self.drain.options;
        let dim = Style::default().dim();

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                format!(" {} on {}? ", kind.name(), self.run_target(nodes)),
                Style::default().bold(),
            ))
            .title(Span::styled(
                format!(
                    " {}{} ",
                    if refreshing { "refreshing · " } else { "" },
                    if self.safety_auto {
                        "[AUTO:ON 10s]"
                    } else {
                        "[AUTO:OFF]"
                    }
                ),
                Style::default().fg(if self.safety_auto {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = usize::from(inner.width).max(1);
        let wrapped = |lines: &[Line]| -> u16 {
            talos9s_core::convert::to_u16(
                lines
                    .iter()
                    .map(|l| l.width().max(1).div_ceil(width))
                    .sum::<usize>(),
            )
        };

        // Commands: the node name once, every command under it
        let name_w = nodes.iter().map(|n| n.hostname.len()).max().unwrap_or(0);
        let mut commands = Vec::new();
        for node in nodes {
            let cmds = crate::operations::commands_for(&self.context, kind, node, options);
            for (i, c) in cmds.into_iter().enumerate() {
                let head = if i == 0 { node.hostname.as_str() } else { "" };
                commands.push(Line::from(vec![
                    Span::styled(format!(" {head:<name_w$} "), Style::default().bold()),
                    Span::styled(format!("$ {c}"), dim),
                ]));
            }
        }
        let safety_text = match safety {
            None => vec![Line::from(Span::styled(" Checking etcd and PDBs…", dim))],
            Some(info) => safety_lines(info),
        };

        let [
            about,
            opts_area,
            summary,
            _,
            table_area,
            _,
            cmd_area,
            safety_area,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(talos9s_core::convert::to_u16(opts.len())),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(wrapped(&commands).min(8)),
            Constraint::Length(wrapped(&safety_text).min(6)),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {}", kind.about(nodes.len() > 1)),
                dim,
            )),
            about,
        );
        let option_lines: Vec<Line> = self
            .drain
            .lines(&opts, cursor, preview)
            .into_iter()
            .map(|l| crate::ui_ext::ellipsize(l, width))
            .collect();
        frame.render_widget(Paragraph::new(option_lines), opts_area);

        // Every pod and its fate, worst first
        let mut pods: Vec<(&PodImpact, Fate)> = match preview {
            Preview::Pods(pods) => pods.iter().map(|p| (p, p.fate(options))).collect(),
            _ => Vec::new(),
        };
        pods.sort_by(|a, b| (&a.1, &a.0.node, &a.0.pod).cmp(&(&b.1, &b.0.node, &b.0.pod)));
        frame.render_widget(Paragraph::new(impact_summary(preview, &pods)), summary);

        let visible = usize::from(table_area.height.saturating_sub(1));
        self.impact_scroll = self.impact_scroll.min(pods.len().saturating_sub(visible));
        let focused = opts.get(cursor).copied();
        let rows: Vec<Row> = pods
            .iter()
            .skip(self.impact_scroll)
            .take(visible)
            .map(|(p, fate)| {
                let (what, style) = fate_cell(fate, p.owner.as_deref());
                // The pods the option under the cursor decides about stand out
                let row_style = match focused {
                    Some(o) if o.hits(p) => Style::default().bold(),
                    _ => Style::default(),
                };
                Row::new(vec![
                    Cell::from(p.node.clone()),
                    Cell::from(p.pod.clone()),
                    Cell::from(Span::styled(
                        p.owner.clone().unwrap_or_else(|| "none".to_string()),
                        dim,
                    )),
                    Cell::from(Span::styled(what, style)),
                ])
                .style(row_style)
            })
            .collect();
        let more = pods.len().saturating_sub(self.impact_scroll + visible);
        let header = Row::new(vec![
            "NODE".to_string(),
            "POD".to_string(),
            "OWNER".to_string(),
            if more > 0 {
                format!("WHAT HAPPENS  (+{more} below, PgDn)")
            } else {
                "WHAT HAPPENS".to_string()
            },
        ])
        .style(Style::default().bold());
        frame.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(talos9s_core::convert::to_u16(name_w.max(4)) + 1),
                    Constraint::Fill(1),
                    Constraint::Length(12),
                    Constraint::Fill(1),
                ],
            )
            .header(header),
            table_area.inner(ratatui::layout::Margin::new(1, 0)),
        );

        // A scroll bar when the pods don't fit, beside the rows
        if pods.len() > visible {
            let mut bar = ratatui::widgets::ScrollbarState::new(pods.len().saturating_sub(visible))
                .position(self.impact_scroll)
                .viewport_content_length(visible);
            frame.render_stateful_widget(
                ratatui::widgets::Scrollbar::new(
                    ratatui::widgets::ScrollbarOrientation::VerticalRight,
                ),
                Rect {
                    y: table_area.y + 1,
                    height: table_area.height.saturating_sub(1),
                    ..table_area
                },
                &mut bar,
            );
        }
        frame.render_widget(
            Paragraph::new(commands).wrap(Wrap { trim: false }),
            cmd_area,
        );
        frame.render_widget(
            Paragraph::new(safety_text).wrap(Wrap { trim: false }),
            safety_area,
        );
        self.option_rows = opts_area;
    }

    /// The operation drop list, under the operation row
    fn draw_dropdown(&mut self, frame: &mut Frame, cursor: usize) {
        let ops = self.ops();
        let width = ops
            .iter()
            .map(|k| k.name().len().max(9) + k.about(self.group).len() + 7)
            .max()
            .unwrap_or(20);
        let area = frame.area();
        let x = self.ops_area.x + 12;
        let y = self.ops_area.y + 2;
        let rect = Rect {
            x,
            y,
            width: talos9s_core::convert::to_u16(width).min(area.right().saturating_sub(x)),
            height: talos9s_core::convert::to_u16(ops.len() + 2)
                .min(area.bottom().saturating_sub(y)),
        };
        let lines: Vec<Line> = ops
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let (pointer, style) = if i == cursor {
                    (crate::ui_ext::PICK_POINTER, crate::ui_ext::PICK_CURSOR)
                } else {
                    (" ", Style::default())
                };
                Line::from(vec![
                    Span::styled(pointer, crate::ui_ext::PICK_CURSOR),
                    Span::styled(format!("{} ", i + 1), KEY),
                    Span::styled(format!("{:<9}", k.name()), style),
                    Span::styled(format!(" {}", k.about(self.group)), Style::default().dim()),
                ])
            })
            .collect();
        self.dropdown_area = rect;
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
            rect,
        );
    }
}

/// A row above the history
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopRow {
    Operation,
    /// "all nodes", on a group
    All,
    Node(usize),
}

/// TALOS, K8S, SCHEDULE and PODS for a node; `…` while the first read
/// goes, `?` when a source couldn't be read
/// One line over the pod table: how many pods meet which fate
fn impact_summary(preview: Preview, pods: &[(&PodImpact, Fate)]) -> Line<'static> {
    match preview {
        Preview::Reading => {
            return Line::from(Span::styled(" Reading the pods…", Style::default().dim()));
        }
        Preview::Unknown => {
            return Line::from(Span::styled(
                " Pods unknown: the drain checks them itself",
                Style::default().dim(),
            ));
        }
        Preview::Pods(_) => {}
    }
    let count = |f: fn(&Fate) -> bool| pods.iter().filter(|(_, fate)| f(fate)).count();
    let stops = count(|f| matches!(f, Fate::Stops(_)));
    let waits = count(|f| matches!(f, Fate::WaitsForPdb(_)));
    let single = count(|f| matches!(f, Fate::SingleInstance(_)));
    let switchover = count(|f| matches!(f, Fate::Switchover(_)));
    let parts: Vec<String> = [
        (count(|f| *f == Fate::Evicted), "evicted"),
        (count(|f| *f == Fate::DataLost), "lose emptyDir data"),
        (count(|f| *f == Fate::Gone), "gone for good"),
        (switchover, "switch over first (CloudNativePG)"),
        (
            count(|f| matches!(f, Fate::WaitsToFinish { .. })),
            "waited for to finish",
        ),
        (count(|f| *f == Fate::Stays), "stay"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    let mut spans = vec![Span::raw(format!(
        " {}: {}",
        talos9s_core::pluralize(pods.len(), "pod", "pods"),
        parts.join(", ")
    ))];
    if waits > 0 {
        spans.push(Span::styled(
            format!(
                " · {} wait for a PDB, up to the timeout",
                talos9s_core::pluralize(waits, "pod", "pods")
            ),
            Style::default().fg(Color::Yellow),
        ));
    }
    if single > 0 {
        spans.push(Span::styled(
            format!(
                " · {} single-instance CloudNativePG {} block it",
                single,
                if single == 1 { "cluster" } else { "clusters" }
            ),
            Style::default().fg(Color::Red),
        ));
    }
    if stops > 0 {
        spans.push(Span::styled(
            format!(
                " · the drain stops at {} (<y> still starts it)",
                talos9s_core::pluralize(stops, "pod", "pods")
            ),
            Style::default().fg(Color::Red),
        ));
    }
    Line::from(spans)
}

/// A pod's WHAT HAPPENS cell
fn fate_cell(fate: &Fate, owner: Option<&str>) -> (String, Style) {
    match fate {
        Fate::Stops(needs) => (
            format!("stops the drain (needs {})", needs.join(", ")),
            Style::default().fg(Color::Red),
        ),
        Fate::WaitsToFinish { then_stops } => (
            if *then_stops {
                "waits to finish; still there, stops the drain".to_string()
            } else {
                "waits to finish, then evicted".to_string()
            },
            Style::default().fg(Color::Yellow),
        ),
        Fate::SingleInstance(cluster) => (
            format!("blocks: CNPG {cluster} has one instance, no switchover"),
            Style::default().fg(Color::Red),
        ),
        Fate::WaitsForPdb(pdb) => (
            format!("waits: PDB {pdb} allows 0, up to the timeout"),
            Style::default().fg(Color::Yellow),
        ),
        Fate::Switchover(cluster) => (
            format!("CNPG {cluster} switches over, then evicted"),
            Style::default().fg(Color::Cyan),
        ),
        Fate::Gone => (
            "evicted, nothing recreates it".to_string(),
            Style::default().fg(Color::Yellow),
        ),
        Fate::DataLost => (
            "evicted, emptyDir data lost".to_string(),
            Style::default().fg(Color::Yellow),
        ),
        Fate::Evicted => (
            format!("evicted, {} restarts it", owner.unwrap_or("its controller")),
            Style::default(),
        ),
        Fate::Stays => ("stays (DaemonSet)".to_string(), Style::default().dim()),
    }
}

fn fact_cells(facts: Option<&NodeFacts>, loading: bool) -> Vec<Span<'static>> {
    let unknown = if loading && facts.is_none() {
        "…"
    } else {
        "?"
    };
    let dim = Style::default().dim();
    let (talos, talos_style) = match facts.and_then(|f| f.talos) {
        Some(TalosState::Up(secs)) => (
            format!(
                "up {}",
                talos9s_core::format_duration_human(chrono::Duration::seconds(
                    i64::try_from(secs).unwrap_or(i64::MAX)
                ))
            ),
            Style::default().fg(Color::Green),
        ),
        Some(TalosState::Offline) => ("offline".to_string(), Style::default().fg(Color::Red)),
        None => (unknown.to_string(), dim),
    };
    let (k8s, k8s_style, sched, sched_style) = match facts.and_then(|f| f.k8s) {
        Some(K8sNode::Present { ready, cordoned }) => {
            let (r, rs) = match ready {
                Some(true) => ("Ready", Style::default().fg(Color::Green)),
                Some(false) => ("NotReady", Style::default().fg(Color::Red)),
                None => ("?", dim),
            };
            let (c, cs) = if cordoned {
                ("cordoned", Style::default().fg(Color::Yellow))
            } else {
                ("yes", Style::default())
            };
            (r, rs, c, cs)
        }
        Some(K8sNode::Missing) => ("missing", Style::default().fg(Color::Red), "-", dim),
        None => (unknown, dim, unknown, dim),
    };
    let pods = facts
        .and_then(|f| f.pods)
        .map_or_else(|| unknown.to_string(), |p| p.to_string());
    vec![
        Span::styled(format!(" {talos:<10}"), talos_style),
        Span::styled(format!(" {k8s:<9}"), k8s_style),
        Span::styled(format!(" {sched:<10}"), sched_style),
        Span::raw(format!(" {pods:>4}")),
    ]
}

/// The status, with a spinner while it runs (as a refresh shows one)
fn status_label(status: RunStatus) -> String {
    if status == RunStatus::Running {
        format!("{} {}", crate::ui_ext::spinner(), status.name())
    } else {
        status.name().to_string()
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
        let click = mouse.kind
            == crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left);
        if let Some(Dialog::Start {
            kind,
            nodes,
            cursor,
            ..
        }) = &mut self.dialog
        {
            // The wheel scrolls the pod table, wherever the pointer is
            match mouse.kind {
                crossterm::event::MouseEventKind::ScrollDown => {
                    self.impact_scroll += IMPACT_WHEEL;
                    return Ok(None);
                }
                crossterm::event::MouseEventKind::ScrollUp => {
                    self.impact_scroll = self.impact_scroll.saturating_sub(IMPACT_WHEEL);
                    return Ok(None);
                }
                _ => {}
            }
            let a = self.option_rows;
            if click && a.contains(ratatui::layout::Position::new(mouse.column, mouse.row)) {
                let row = usize::from(mouse.row - a.y);
                if let Some(&opt) = Opt::shown(*kind, nodes.len()).get(row) {
                    *cursor = row;
                    self.drain.toggle(opt);
                }
            }
            return Ok(None);
        }
        if self.dialog.is_some() {
            return Ok(None);
        }
        // The drop list: a click on an item picks it, elsewhere closes it
        if self.dropdown.is_some() {
            if click {
                let a = self.dropdown_area;
                let inside = mouse.column > a.x
                    && mouse.column + 1 < a.right()
                    && mouse.row > a.y
                    && mouse.row + 1 < a.bottom();
                if inside {
                    self.pick_operation(usize::from(mouse.row - a.y - 1));
                } else {
                    self.dropdown = None;
                }
            }
            return Ok(None);
        }
        if self.focus.is_some() {
            self.logs.handle_mouse(&mouse);
            return Ok(None);
        }
        if let Some(row) = self.rows.clicked(&mouse) {
            self.selected = self.top_rows() + row;
        } else if mouse.kind
            == crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
            && mouse.row > self.ops_area.y
            && mouse.row < self.ops_area.bottom()
            && mouse.column >= self.ops_area.x
            && mouse.column < self.ops_area.right()
        {
            // Line 1 is the operation (a click opens the drop list), line 2
            // the column header; a click on a node checks it
            match mouse.row - self.ops_area.y {
                1 => {
                    self.selected = 0;
                    self.dropdown = Some(self.kind);
                }
                2 => {}
                n => {
                    self.selected = usize::from(n - 2);
                    self.message = None;
                    self.toggle();
                }
            }
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn modal(&self) -> bool {
        self.drain_start_open()
    }

    fn captures_text(&self) -> bool {
        self.filtering
            || self.dialog.is_some()
            || self.dropdown.is_some()
            || self.logs.captures_text()
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if self.dialog.is_some() {
            return Ok(self.handle_dialog_key(key));
        }
        if let Some(cursor) = self.dropdown {
            self.handle_dropdown_key(key, cursor);
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
                TextEdit::Changed => self.selected = self.selected.min(self.top_rows()),
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
            KeyCode::Char(' ') => self.toggle(),
            // The operation: `o` opens the drop list, a digit picks one
            KeyCode::Char('o') => self.dropdown = Some(self.kind),
            KeyCode::Char(c @ '1'..='9') => self.pick_operation(usize::from(c as u8 - b'1')),
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
                if self.safety_auto
                    && self.drain_start_open()
                    && self.safety_at.is_some_and(|t| t.elapsed() >= SAFETY_EVERY)
                {
                    self.refresh_safety();
                }
                self.poll_facts();
                self.load_facts();
                let every = if self.running().is_empty() {
                    RELOAD_IDLE
                } else {
                    RELOAD_RUNNING
                };
                if self.loaded.is_none_or(|t| t.elapsed() >= every) {
                    self.reload();
                }
                // The pane reads the run's file; the default pace for logs
                // (seconds) would leave a short run looking empty
                let running = self.focus_running();
                if self.focus.is_some() {
                    if running {
                        self.logs.tick_every(None, RUN_FOLLOW_INTERVAL);
                    } else if self.focus_was_running {
                        self.logs.fetch(None);
                    } else {
                        self.logs.tick(None);
                    }
                }
                self.focus_was_running = running;
            }
            Action::Refresh => {
                self.reload();
                self.facts_at = None;
                self.load_facts();
            }
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
        if let Some(cursor) = self.dropdown {
            self.draw_dropdown(frame, cursor);
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
            // The bc run was on bc1, the kprod one on another node
            let n = if ctx == "bc" { "bc1" } else { "n1" };
            let h = RunHeader {
                context: ctx.into(),
                target: n.into(),
                nodes: vec![n.into()],
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
        // On a node: its own runs; `A` shows every run
        assert_eq!(c.history_rows().len(), 1, "bc1's runs only");
        press(&mut c, KeyCode::Char('A'));
        assert_eq!(c.history_rows().len(), 2);
        // On a group: every run; `A` this cluster's
        let mut g = component(&dir, true);
        g.reload();
        assert_eq!(g.history_rows().len(), 2);
        press(&mut g, KeyCode::Char('A'));
        assert_eq!(g.history_rows().len(), 1, "this cluster only");
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
        assert!(screen.contains("[ Cordon"));
        assert!(screen.contains("[✓]bc1") || screen.contains("[✓] bc1"));
        assert!(screen.contains("failed"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A group offers no shutdown and starts with nothing checked; the
    /// drop list picks the operation, Space checks nodes, Enter asks first,
    /// `n` closes without starting anything
    #[test]
    fn pick_operation_and_nodes() {
        let dir = std::env::temp_dir().join(format!("talos9s-opsdlg-{}", std::process::id()));
        let mut c = component(&dir, true);
        assert!(!c.ops().contains(&OpKind::Shutdown));
        assert_eq!(c.picked, [false, false]);
        // The drop list: Enter opens, j moves, Enter picks
        press(&mut c, KeyCode::Enter);
        assert!(c.captures_text());
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Enter);
        assert_eq!(c.kind(), OpKind::Uncordon);
        assert!(c.dropdown.is_none());
        // Picking moves to the nodes ("all nodes")
        assert_eq!(c.top_row(c.selected), Some(TopRow::All));
        // A digit picks an operation without the list; `o` opens it
        press(&mut c, KeyCode::Char('3'));
        assert_eq!(c.kind(), OpKind::Drain);
        press(&mut c, KeyCode::Char('o'));
        assert!(c.dropdown.is_some());
        press(&mut c, KeyCode::Char('2'));
        assert_eq!(c.kind(), OpKind::Uncordon);
        // Nothing checked: Enter on a node says so
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Enter);
        assert!(c.dialog.is_none());
        assert!(c.message.is_some());
        // Space on "all nodes" checks both, again unchecks
        press(&mut c, KeyCode::Char('k'));
        press(&mut c, KeyCode::Char(' '));
        assert_eq!(c.picked, [true, true]);
        press(&mut c, KeyCode::Char(' '));
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Char(' '));
        assert_eq!(c.picked, [true, false]);
        assert_eq!(c.run_target(&c.checked_nodes()), "bc1");
        press(&mut c, KeyCode::Enter);
        let Some(Dialog::Start { kind, nodes, .. }) = &c.dialog else {
            panic!("no dialog");
        };
        assert_eq!(*kind, OpKind::Uncordon);
        assert_eq!(nodes.len(), 1);
        press(&mut c, KeyCode::Char('n'));
        assert!(c.dialog.is_none());
        assert!(c.history_rows().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A single node is checked from the start and stays checked
    /// The operation carries to the next target when it is offered there
    #[test]
    fn operation_carries_over() {
        let dir = std::env::temp_dir().join(format!("talos9s-opscarry-{}", std::process::id()));
        let mut c = component(&dir, false);
        c.set_operation(OpKind::Drain);
        assert_eq!(c.operation(), OpKind::Drain);
        let mut group = component(&dir, true);
        group.set_operation(OpKind::Shutdown);
        assert_eq!(group.operation(), OpKind::Cordon);
    }

    #[test]
    fn single_node_is_checked() {
        let dir = std::env::temp_dir().join(format!("talos9s-opsone-{}", std::process::id()));
        let mut c = component(&dir, false);
        assert_eq!(c.picked, [true]);
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Char(' '));
        assert_eq!(c.picked, [true]);
        assert_eq!(c.run_target(&c.checked_nodes()), "bc1");
        press(&mut c, KeyCode::Enter);
        assert!(matches!(c.dialog, Some(Dialog::Start { .. })));
        press(&mut c, KeyCode::Esc);
        assert!(c.dialog.is_none());
    }

    /// A cordoned node can't be cordoned again, only uncordoned; "all
    /// nodes" skips what the operation can't run on, and changing the
    /// operation unchecks it
    #[test]
    fn node_state_limits_the_checks() {
        let dir = std::env::temp_dir().join(format!("talos9s-opsfacts-{}", std::process::id()));
        let mut c = component(&dir, true);
        let state = |cordoned| NodeFacts {
            talos: Some(TalosState::Up(3600)),
            k8s: Some(K8sNode::Present {
                ready: Some(true),
                cordoned,
            }),
            pods: Some(7),
        };
        c.facts.insert("bc1".into(), state(true));
        c.facts.insert("bc2".into(), state(false));
        // Cordon: "all nodes" checks bc2 only, Space on bc1 says why
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Char(' '));
        assert_eq!(c.picked, [false, true]);
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Char(' '));
        assert_eq!(c.picked, [false, true]);
        assert!(
            c.message
                .as_deref()
                .is_some_and(|m| m.contains("already cordoned"))
        );
        // Uncordon: bc2 no longer fits and is unchecked
        c.kind = 1;
        c.drop_blocked();
        assert_eq!(c.picked, [false, false]);
        press(&mut c, KeyCode::Char(' '));
        assert_eq!(c.picked, [true, false]);
        assert_eq!(c.checked_nodes().len(), 1);

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|f| c.draw(f, f.area()).unwrap()).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(screen.contains("cordoned"), "{screen}");
        assert!(screen.contains("not cordoned"), "{screen}");
        assert!(screen.contains("[-]"), "{screen}");
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
        for _ in 0..4 {
            assert!(c.key_hints_line().width() <= 85, "{}", c.key_hints_line());
            press(&mut c, KeyCode::Char('j'));
        }
        press(&mut c, KeyCode::Char('g'));
        press(&mut c, KeyCode::Enter);
        assert!(c.key_hints_line().width() <= 85, "{}", c.key_hints_line());
    }

    /// Drain's dialog has the options: keys move, toggle and step them,
    /// the command follows, and the choice outlives the dialog
    #[test]
    fn drain_dialog_options() {
        let dir = std::env::temp_dir().join(format!("talos9s-opsdrain-{}", std::process::id()));
        let mut c = component(&dir, false);
        press(&mut c, KeyCode::Char('3'));
        assert_eq!(c.kind(), OpKind::Drain);
        press(&mut c, KeyCode::Enter);
        assert!(matches!(c.dialog, Some(Dialog::Start { .. })));
        assert!(c.key_hints_line().width() <= 85, "{}", c.key_hints_line());
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Char(' '));
        assert!(!c.drain.options.delete_emptydir_data);
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Right);
        assert_eq!(c.drain.options.wait_to_finish_secs, Some(300));
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Char('j'));
        press(&mut c, KeyCode::Right);
        assert_eq!(c.drain.options.grace_period_secs, Some(0));
        // `Enter` never confirms
        press(&mut c, KeyCode::Enter);
        assert!(c.dialog.is_some());

        let backend = ratatui::backend::TestBackend::new(80, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                c.draw(f, f.area()).unwrap();
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("WHAT HAPPENS"));
        assert!(text.contains("[ ] Delete emptyDir data"));
        assert!(text.contains("--grace-period=0"));

        // `a` toggles auto-refresh; `r` with no client reads nothing
        assert!(c.safety_auto);
        press(&mut c, KeyCode::Char('a'));
        assert!(!c.safety_auto);
        press(&mut c, KeyCode::Char('r'));
        assert!(c.dialog.is_some());
        // It covers the screen and takes the wheel
        assert!(c.modal());
        let wheel = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        c.handle_mouse_event(wheel(crossterm::event::MouseEventKind::ScrollDown))
            .unwrap();
        assert_eq!(c.impact_scroll, IMPACT_WHEEL);
        c.handle_mouse_event(wheel(crossterm::event::MouseEventKind::ScrollUp))
            .unwrap();
        assert_eq!(c.impact_scroll, 0);

        press(&mut c, KeyCode::Char('n'));
        assert!(c.dialog.is_none());
        assert!(!c.modal());
        assert_eq!(c.drain.options.grace_period_secs, Some(0));
        // Cordon has no options
        press(&mut c, KeyCode::Char('1'));
        press(&mut c, KeyCode::Enter);
        if let Some(Dialog::Start { kind, nodes, .. }) = &c.dialog {
            assert!(Opt::shown(*kind, nodes.len()).is_empty());
        }
    }
}
