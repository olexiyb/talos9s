//! Containers view: every container on a node, a group or the cluster, as
//! containerd reports it through the Talos API (no Kubernetes API needed)
//!
//! Talos services and Kubernetes pods, folded to one row per pod (Enter
//! unfolds), sorted by any column (`1`-`6`, a header click), filtered
//! (`/`), warped into namespaces (`W`), exited containers shown with `A`.
//! The logs pane below follows the selected row: a service, a container,
//! or a pod's containers merged; read through the Talos API too. `l` gives
//! it the keys (levels, search), Esc gives them back.

use crate::action::Action;
use crate::components::namespace_picker::{NamespacePicker, PickerEvent};
use crate::components::pod_logs::LogPane;
use crate::components::processes::ProcessNode;
use crate::components::workloads::{draw_warp_hint, warp_hint, write_private};
use crate::components::{BackgroundRefresh, Component};
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use talos_rs::{ContainerKind, TalosClient};
use talos9s_core::AsyncState;
use talos9s_core::constants::CONTAINERS_TIMEOUT;
use talos9s_core::containers::{
    ContainerOptions, ContainerRow, ContainerSort, CtrState, NodeContainer, container_rows,
    state_counts,
};
use talos9s_core::pod_logs::{LogSource, TalosLog};

const AUTO_REFRESH_INTERVAL: std::time::Duration =
    talos9s_core::constants::refresh_intervals::VERY_SLOW;

/// Settings that carry over to the next visit and are saved
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContainersSettings {
    pub show_exited: bool,
    pub sort: ContainerSort,
}

/// What a refresh read
#[derive(Debug, Clone, Default)]
pub struct ContainersData {
    pub containers: Vec<NodeContainer>,
    /// Per node: what couldn't be read (`bc4: timed out after 10s`)
    pub problems: Vec<String>,
}

#[derive(Clone)]
pub struct ContainersComponent {
    nodes: Vec<ProcessNode>,
    client: Option<TalosClient>,
    state: AsyncState<ContainersData>,
    options: ContainerOptions,
    /// The selected row's id (`ContainerRow::id`), kept across refreshes
    selected_id: String,
    table_state: TableState,
    filtering: bool,
    recent_namespaces: Vec<String>,
    picker: Option<NamespacePicker>,
    logs: LogPane,
    /// The logs pane has the keys (`l`)
    logs_keys: bool,
    auto_refresh: bool,
    rows: crate::ui_ext::RowHits,
    header: crate::ui_ext::ColumnHits<ContainerSort>,
}

impl ContainersComponent {
    pub fn new(nodes: Vec<ProcessNode>) -> Self {
        Self {
            nodes,
            client: None,
            state: AsyncState::new(),
            options: ContainerOptions::default(),
            selected_id: String::new(),
            table_state: TableState::default(),
            filtering: false,
            recent_namespaces: Vec::new(),
            picker: None,
            logs: LogPane::default(),
            logs_keys: false,
            auto_refresh: true,
            rows: crate::ui_ext::RowHits::default(),
            header: crate::ui_ext::ColumnHits::default(),
        }
    }

    pub fn set_client(&mut self, client: TalosClient) {
        self.logs.set_talos_client(client.clone());
        self.client = Some(client);
    }

    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    pub fn settings(&self) -> ContainersSettings {
        ContainersSettings {
            show_exited: self.options.show_exited,
            sort: self.options.sort,
        }
    }

    pub fn set_settings(&mut self, settings: ContainersSettings) {
        self.options.show_exited = settings.show_exited;
        self.options.sort = settings.sort;
    }

    pub fn log_wrap(&self) -> bool {
        self.logs.wrap()
    }

    pub fn set_log_wrap(&mut self, wrap: bool) {
        self.logs.set_wrap(wrap);
    }

    /// The namespaces warped into and the recent warps; from the app
    pub fn set_warp(&mut self, warp: Vec<String>, recent: Vec<String>) {
        self.recent_namespaces = recent;
        if self.options.warp != warp {
            self.options.warp = warp;
            self.sync_logs();
        }
    }

    /// The external viewer couldn't run: say why in the logs pane
    pub fn viewer_failed(&mut self, problem: String) {
        self.logs.set_viewer_problem(problem);
    }

    /// Several nodes: a NODE column
    fn multi_node(&self) -> bool {
        self.nodes.len() > 1
    }

    fn all(&self) -> &[NodeContainer] {
        self.state.data().map_or(&[], |d| d.containers.as_slice())
    }

    fn list_rows(&self) -> Vec<ContainerRow> {
        container_rows(self.all(), &self.options)
    }

    fn selected_index(&self, rows: &[ContainerRow]) -> usize {
        let all = self.all();
        rows.iter()
            .position(|r| r.id(all) == self.selected_id)
            .unwrap_or(0)
    }

    fn select(&mut self, rows: &[ContainerRow], index: usize) {
        if let Some(row) = rows.get(index) {
            self.selected_id = row.id(self.all());
        }
        self.sync_logs();
    }

    /// Read every node's containers, all nodes at once, each bounded
    pub async fn refresh(&mut self) -> Result<()> {
        self.state.start_loading();
        let Some(client) = self.client.clone() else {
            self.state.set_error("Not connected to the cluster");
            return Ok(());
        };
        let reads = self.nodes.iter().map(|node| {
            let client = client.with_node(&node.address);
            async move {
                let read = |kind| {
                    let client = client.clone();
                    async move {
                        match tokio::time::timeout(CONTAINERS_TIMEOUT, client.containers(kind))
                            .await
                        {
                            Ok(Ok(list)) => Ok(list),
                            Ok(Err(e)) => Err(e.to_string()),
                            Err(_) => {
                                Err(format!("timed out after {}s", CONTAINERS_TIMEOUT.as_secs()))
                            }
                        }
                    }
                };
                let (system, kubernetes) =
                    tokio::join!(read(ContainerKind::System), read(ContainerKind::Kubernetes));
                (node, system, kubernetes)
            }
        });
        let mut data = ContainersData::default();
        for (node, system, kubernetes) in futures::future::join_all(reads).await {
            let mut found = Vec::new();
            match (system, kubernetes) {
                (Ok(s), Ok(k)) => found.extend(s.into_iter().chain(k)),
                // No kubelet: the services alone are still worth showing
                (Ok(s), Err(e)) => {
                    found.extend(s);
                    data.problems
                        .push(format!("{}: Kubernetes containers: {e}", node.hostname));
                }
                (Err(e), Ok(k)) => {
                    found.extend(k);
                    data.problems
                        .push(format!("{}: system containers: {e}", node.hostname));
                }
                (Err(e), Err(_)) => data.problems.push(format!("{}: {e}", node.hostname)),
            }
            data.containers
                .extend(found.into_iter().map(|info| NodeContainer {
                    node: node.hostname.clone(),
                    address: node.address.clone(),
                    info,
                }));
        }
        self.state.set_data(data);
        self.sync_logs();
        Ok(())
    }

    /// Point the logs pane at the selected row
    fn sync_logs(&mut self) {
        let rows = self.list_rows();
        let Some(row) = rows.get(self.selected_index(&rows)) else {
            return;
        };
        let all = self.all();
        let source = |c: &NodeContainer, label: String| LogSource {
            namespace: c.namespace().to_string(),
            pod: c.pod().to_string(),
            container: c.name().to_string(),
            label,
            has_previous: false,
            talos: Some(TalosLog {
                address: c.address.clone(),
                kind: c.info.kind,
                id: c.info.id.clone(),
            }),
        };
        let (title, sources, command) = match row {
            ContainerRow::Service(i) | ContainerRow::Member(i) => {
                let Some(c) = all.get(*i) else { return };
                let title = match c.info.kind {
                    ContainerKind::System => format!("{} on {}", c.name(), c.node),
                    ContainerKind::Kubernetes => {
                        format!("{} · {} on {}", c.pod(), c.name(), c.node)
                    }
                };
                let command = match c.info.kind {
                    ContainerKind::System => format!("talosctl -n {} logs {}", c.address, c.name()),
                    ContainerKind::Kubernetes => format!(
                        "talosctl -n {} logs --namespace cri {}",
                        c.address, c.info.id
                    ),
                };
                (title, vec![source(c, String::new())], command)
            }
            ContainerRow::Pod { first, members, .. } => {
                let Some(head) = all.get(*first) else { return };
                let sources: Vec<LogSource> = members
                    .iter()
                    .filter_map(|m| all.get(*m))
                    .map(|c| {
                        let label = if members.len() > 1 {
                            c.name().to_string()
                        } else {
                            String::new()
                        };
                        source(c, label)
                    })
                    .collect();
                (
                    format!("{} on {}", head.pod(), head.node),
                    sources,
                    format!(
                        "kubectl -n {} logs {} --all-containers --prefix",
                        head.namespace(),
                        head.pod()
                    ),
                )
            }
        };
        self.logs.set_sources(title, sources, command);
    }

    fn move_selection(&mut self, step: crate::ui_ext::Move) {
        let rows = self.list_rows();
        let current = self.selected_index(&rows);
        if let Some(i) = crate::ui_ext::moved(current, rows.len(), step) {
            self.select(&rows, i);
        }
    }

    /// Fold or unfold the selected pod (or the pod of a selected container)
    fn toggle_fold(&mut self) {
        let rows = self.list_rows();
        let current = self.selected_index(&rows);
        // A container: the pod row above it
        let pod = rows
            .get(..=current)
            .unwrap_or_default()
            .iter()
            .rev()
            .find_map(|r| match r {
                ContainerRow::Pod { key, .. } => Some(key.clone()),
                ContainerRow::Service(_) => Some(String::new()),
                ContainerRow::Member(_) => None,
            })
            .filter(|k| !k.is_empty());
        if let Some(key) = pod {
            if !self.options.expanded.remove(&key) {
                self.options.expanded.insert(key.clone());
            }
            self.selected_id = key;
            self.sync_logs();
        }
    }

    fn open_picker(&mut self) {
        let counts = talos9s_core::namespaces::count_by_namespace(
            self.all()
                .iter()
                .filter(|c| !c.info.sandbox)
                .map(NodeContainer::namespace),
        );
        self.picker = Some(NamespacePicker::new(
            counts,
            self.recent_namespaces.clone(),
            self.options.warp.clone(),
            "containers",
        ));
    }

    fn open_in_viewer(&mut self) -> Option<Action> {
        let text = self.logs.export()?;
        let path = std::env::temp_dir().join(format!("talos9s-logs-{}.log", std::process::id()));
        match write_private(&path, &text) {
            Ok(()) => Some(Action::OpenInViewer(path)),
            Err(e) => {
                self.logs
                    .set_viewer_problem(format!("Couldn't write {}: {e}", path.display()));
                None
            }
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        use crate::ui_ext::{TextEdit, edit_text};
        match edit_text(&mut self.options.filter, &key) {
            TextEdit::Cancel => {
                self.filtering = false;
                self.options.filter.clear();
            }
            TextEdit::Submit => self.filtering = false,
            TextEdit::Changed => {
                self.options.filter = self.options.filter.to_lowercase();
            }
            TextEdit::Ignored => return,
        }
        self.sync_logs();
    }

    /// Keys while the logs pane has them (`l`)
    fn handle_logs_key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.logs.captures_text() || (key.code == KeyCode::Esc && self.logs.has_search()) {
            self.logs.handle_key(&key, None);
            return None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'l') => self.logs_keys = false,
            KeyCode::Char('y') => crate::clipboard::copy_to_clipboard(self.logs.command()),
            KeyCode::Char('v') => return self.open_in_viewer(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.logs.scroll_lines(-1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.logs
                    .handle_key(&KeyEvent::from(KeyCode::PageDown), None);
            }
            _ => {
                self.logs.handle_key(&key, None);
            }
        }
        None
    }

    fn summary_line(&self) -> Line<'static> {
        let dim = Style::default().dim();
        let Some(data) = self.state.data() else {
            return Line::default();
        };
        let (running, exited, other) = state_counts(&data.containers, &self.options.warp);
        let mut spans = vec![Span::styled(
            format!(
                "{} · ",
                talos9s_core::pluralize(self.nodes.len(), "node", "nodes")
            ),
            dim,
        )];
        spans.push(Span::styled(
            format!("{running} running"),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::styled(format!(" · {exited} exited"), dim));
        if other > 0 {
            spans.push(Span::styled(
                format!(" · {other} other"),
                Style::default().fg(Color::Yellow),
            ));
        }
        if !self.options.show_exited && exited > 0 {
            spans.push(Span::styled(" (A shows exited)", dim));
        }
        for problem in &data.problems {
            spans.push(Span::styled(
                format!(" · {problem}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    }

    fn table_row(&self, row: &ContainerRow) -> Row<'static> {
        let all = self.all();
        let dim = Style::default().dim();
        let state_style = |s: CtrState| match s {
            CtrState::Running => Style::default().fg(Color::Green),
            // Finished init containers and jobs exit normally
            CtrState::Exited => dim,
            CtrState::Other => Style::default().fg(Color::Yellow),
        };
        let pid = |p: u32| if p == 0 { String::new() } else { p.to_string() };
        let (head, name, image, pid_text, status) = match row {
            ContainerRow::Pod {
                first,
                members,
                running,
                expanded,
                ..
            } => {
                let Some(head) = all.get(*first) else {
                    return Row::new(Vec::<Cell>::new());
                };
                let states: Vec<CtrState> = members
                    .iter()
                    .filter_map(|m| all.get(*m))
                    .map(NodeContainer::state)
                    .collect();
                let style = if states.contains(&CtrState::Other) {
                    Style::default().fg(Color::Yellow)
                } else if *running > 0 {
                    Style::default().fg(Color::Green)
                } else {
                    dim
                };
                let images: Vec<&str> = members
                    .iter()
                    .filter_map(|m| all.get(*m))
                    .map(|c| c.info.image.as_str())
                    .collect();
                let image = match images.as_slice() {
                    [] => String::new(),
                    [one] => (*one).to_string(),
                    [one, rest @ ..] => format!("{one} +{}", rest.len()),
                };
                let marker = if *expanded { "▾" } else { "▸" };
                (
                    head,
                    Cell::from(format!("{marker} {}", head.pod())),
                    image,
                    Cell::from(pid(head.info.pid)),
                    Cell::from(format!("{running}/{}", members.len())).style(style),
                )
            }
            ContainerRow::Service(i) | ContainerRow::Member(i) => {
                let Some(c) = all.get(*i) else {
                    return Row::new(Vec::<Cell>::new());
                };
                let indent = if matches!(row, ContainerRow::Member(_)) {
                    "    "
                } else {
                    "  "
                };
                let status = c
                    .info
                    .status
                    .strip_prefix("CONTAINER_")
                    .unwrap_or(&c.info.status)
                    .to_string();
                (
                    c,
                    Cell::from(format!("{indent}{}", c.name())),
                    c.info.image.clone(),
                    Cell::from(pid(c.info.pid)),
                    Cell::from(status).style(state_style(c.state())),
                )
            }
        };
        let member = matches!(row, ContainerRow::Member(_));
        let mut cells = vec![Cell::from("")];
        if self.multi_node() {
            cells.push(Cell::from(if member {
                String::new()
            } else {
                head.node.clone()
            }));
        }
        cells.push(Cell::from(if member {
            String::new()
        } else {
            head.namespace().to_string()
        }));
        cells.extend([name, Cell::from(image).style(dim), pid_text, status]);
        Row::new(cells)
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let rows = self.list_rows();
        let all = self.all();
        let longest = |f: &dyn Fn(&NodeContainer) -> usize, min: usize, max: usize| {
            all.iter().map(f).max().unwrap_or(min).clamp(min, max)
        };
        let to_u16 = talos9s_core::convert::to_u16;
        let node_width = longest(&|c| c.node.chars().count(), 4, 16);
        let ns_width = longest(&|c| c.namespace().chars().count(), 9, 20);
        // Names as wide as the longest (marker and indent included); the
        // room left goes to IMAGE
        let name_width = rows
            .iter()
            .filter_map(|r| match r {
                ContainerRow::Pod { first, .. } => all.get(*first).map(|c| c.pod().len() + 2),
                ContainerRow::Service(i) => all.get(*i).map(|c| c.name().len() + 2),
                ContainerRow::Member(i) => all.get(*i).map(|c| c.name().len() + 4),
            })
            .max()
            .unwrap_or(16)
            .clamp(16, 60);
        let mut columns: Vec<(&str, Constraint, Option<ContainerSort>)> =
            vec![("", Constraint::Length(1), None)];
        if self.multi_node() {
            columns.push((
                "NODE",
                Constraint::Length(to_u16(node_width + 1)),
                Some(ContainerSort::Node),
            ));
        }
        columns.extend([
            (
                "NAMESPACE",
                Constraint::Length(to_u16(ns_width + 1)),
                Some(ContainerSort::Namespace),
            ),
            (
                "POD / CONTAINER",
                Constraint::Length(to_u16(name_width + 1)),
                Some(ContainerSort::Name),
            ),
            ("IMAGE", Constraint::Fill(1), Some(ContainerSort::Image)),
            ("PID", Constraint::Length(8), Some(ContainerSort::Pid)),
            (
                "STATUS",
                Constraint::Length(10),
                Some(ContainerSort::Status),
            ),
        ]);
        let header = Row::new(columns.iter().map(|(label, _, sort)| {
            if sort.is_some_and(|s| s == self.options.sort) {
                Cell::from(format!("{label}▼")).style(Style::default().fg(Color::Cyan))
            } else {
                Cell::from((*label).to_string())
            }
        }))
        .style(Style::default().add_modifier(Modifier::DIM));
        let widths: Vec<Constraint> = columns.iter().map(|(_, w, _)| *w).collect();
        let targets: Vec<Option<ContainerSort>> = columns.iter().map(|(_, _, s)| *s).collect();
        let table_rows: Vec<Row> = rows.iter().map(|r| self.table_row(r)).collect();
        let selected = self.selected_index(&rows);
        self.table_state
            .select((!rows.is_empty()).then_some(selected));
        let empty = rows.is_empty();
        let table = Table::new(table_rows, widths.clone())
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(table, area, &mut self.table_state);
        self.header = crate::ui_ext::ColumnHits::new(area, &widths, &targets);
        self.rows =
            crate::ui_ext::RowHits::new(area, false, 1, self.table_state.offset(), rows.len());
        if empty {
            let text = if self.options.filter.is_empty() && self.options.warp.is_empty() {
                "No containers"
            } else {
                "No containers match"
            };
            frame.render_widget(
                Paragraph::new(text).style(Style::default().dim()),
                Rect {
                    y: area.y.saturating_add(1),
                    height: area.height.saturating_sub(1),
                    ..area
                },
            );
        }
    }

    fn key_hints_line(&self) -> Line<'static> {
        let key = |k: &'static str| Span::styled(k, crate::ui_ext::KEY);
        if let Some(picker) = &self.picker {
            return picker.key_hints();
        }
        if self.filtering || self.logs.captures_text() {
            return Line::from(vec![
                key("<Enter>"),
                Span::raw(" keep  "),
                key("<Esc>"),
                Span::raw(" clear"),
            ]);
        }
        if self.logs_keys {
            let search = if self.logs.has_search() {
                [key("<n/N>"), Span::raw(" match  ")]
            } else {
                [key("</>"), Span::raw(" search  ")]
            };
            let mut spans = vec![
                key("<PgUp/Dn>"),
                Span::raw(" scroll  "),
                key("<1-5/0>"),
                Span::raw(" levels  "),
            ];
            spans.extend(search);
            spans.extend([
                key("<w>"),
                Span::raw(" wrap  "),
                key("<y>"),
                Span::raw(" copy  "),
                key("<v>"),
                Span::raw(" hl  "),
                key("<Esc>"),
                Span::raw(" list"),
            ]);
            return Line::from(spans);
        }
        Line::from(vec![
            key("<Enter>"),
            Span::raw(" fold  "),
            key("<l>"),
            Span::raw(" logs  "),
            key("</>"),
            Span::raw(" filter  "),
            key("<1-6>"),
            Span::raw(" sort  "),
            key("<A>"),
            Span::raw(if self.options.show_exited {
                " running  "
            } else {
                " exited  "
            }),
            key("<W>"),
            Span::raw(" ns  "),
            key("<v>"),
            Span::raw(" hl"),
        ])
    }
}

impl BackgroundRefresh for ContainersComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        self.state = refreshed.state;
        self.sync_logs();
    }
}

impl Component for ContainersComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if self.picker.is_some() || self.logs.handle_mouse(&mouse) {
            return Ok(None);
        }
        // A click in the logs gives them the keys
        if crate::ui_ext::is_left_click(&mouse) && self.logs.contains(&mouse) {
            self.logs_keys = true;
            return Ok(None);
        }
        if let Some(sort) = self.header.clicked(&mouse) {
            self.options.sort = sort;
            self.sync_logs();
            return Ok(None);
        }
        if let Some(row) = self.rows.clicked(&mouse) {
            let rows = self.list_rows();
            self.select(&rows, row);
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn captures_text(&self) -> bool {
        self.filtering || self.picker.is_some() || self.logs.captures_text()
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if self.filtering {
            self.handle_filter_key(key);
            return Ok(None);
        }
        if let Some(picker) = &mut self.picker {
            return Ok(match picker.handle_key(&key) {
                PickerEvent::Stay => None,
                PickerEvent::Close => {
                    self.picker = None;
                    None
                }
                PickerEvent::Pick(warp) => {
                    self.picker = None;
                    self.options.warp = warp.clone();
                    self.sync_logs();
                    Some(Action::WarpNamespace(warp))
                }
            });
        }
        if self.logs_keys {
            return Ok(self.handle_logs_key(key));
        }
        use crate::ui_ext::Move;
        match key.code {
            KeyCode::Esc if !self.options.filter.is_empty() => {
                self.options.filter.clear();
                self.sync_logs();
            }
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Some(Action::Back)),
            KeyCode::Enter => self.toggle_fold(),
            KeyCode::Char('l') => self.logs_keys = true,
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('A') => {
                self.options.show_exited = !self.options.show_exited;
                self.sync_logs();
            }
            KeyCode::Char('W') => self.open_picker(),
            KeyCode::Char(c @ '1'..='6') => {
                let i = usize::from(u8::try_from(c).unwrap_or(b'1') - b'1');
                // Without a NODE column the keys start at NAMESPACE
                let offset = usize::from(!self.multi_node());
                if let Some(&sort) = ContainerSort::ALL.get(i + offset) {
                    self.options.sort = sort;
                    self.sync_logs();
                }
            }
            KeyCode::Char('r') => return Ok(Some(Action::Refresh)),
            KeyCode::Char('a') => self.auto_refresh = !self.auto_refresh,
            KeyCode::Char('y') => crate::clipboard::copy_to_clipboard(self.logs.command()),
            KeyCode::Char('v') => return Ok(self.open_in_viewer()),
            KeyCode::Char('w') => self.logs.set_wrap(!self.logs.wrap()),
            _ => match Move::from_key(&key) {
                Some(step @ (Move::Prev | Move::Next)) => self.move_selection(step),
                // The page keys and g/G scroll the logs
                Some(_) => {
                    self.logs.handle_key(&key, None);
                }
                None => {}
            },
        }
        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            self.logs.tick(None);
            if self
                .state
                .should_auto_refresh(self.auto_refresh, AUTO_REFRESH_INTERVAL)
            {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        if crate::ui_ext::draw_load_gate(
            frame,
            area,
            self.state.is_loading() && !self.state.has_data(),
            self.state.error(),
            "Loading containers...",
        ) {
            return Ok(());
        }
        let summary = self.summary_line();
        let summary_width = summary.width();
        let rows = talos9s_core::convert::to_u16(self.list_rows().len());
        let show_filter = self.filtering || !self.options.filter.is_empty();
        let [summary_area, filter_area, list, logs] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(u16::from(show_filter)),
            // The list as tall as its rows, at most half; the logs the rest
            Constraint::Length(rows.saturating_add(1).clamp(4, area.height / 2)),
            Constraint::Fill(1),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(summary).block(Block::default().borders(Borders::BOTTOM)),
            summary_area,
        );
        draw_warp_hint(
            frame,
            summary_area,
            summary_width,
            warp_hint(&self.options.warp),
        );
        if show_filter {
            let mut spans = vec![
                Span::styled("Filter: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(self.options.filter.clone()),
            ];
            if self.filtering {
                spans.push(Span::styled("█", Style::default().fg(Color::Cyan)));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), filter_area);
        }
        self.draw_list(frame, list);
        self.logs.draw(frame, logs);
        if let Some(picker) = &self.picker {
            picker.draw(frame, area);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use talos_rs::ContainerInfo;

    fn ctr(node: &str, pod: &str, name: &str, sandbox: bool, status: &str) -> NodeContainer {
        NodeContainer {
            node: node.into(),
            address: format!("10.0.0.{}", node.len()),
            info: ContainerInfo {
                kind: if pod.is_empty() {
                    ContainerKind::System
                } else {
                    ContainerKind::Kubernetes
                },
                id: if pod.is_empty() {
                    name.into()
                } else {
                    format!("{pod}:{name}:abc")
                },
                name: name.into(),
                pod: pod.into(),
                sandbox,
                image: format!("img/{name}"),
                pid: 7,
                status: status.into(),
            },
        }
    }

    fn component() -> ContainersComponent {
        let nodes = ["bc1", "bc2"]
            .map(|n| ProcessNode {
                hostname: n.into(),
                address: n.into(),
            })
            .to_vec();
        let mut c = ContainersComponent::new(nodes);
        c.state.set_data(ContainersData {
            containers: vec![
                ctr("bc1", "", "apid", false, "RUNNING"),
                ctr("bc1", "web/gw-1", "gw-1", true, "SANDBOX_READY"),
                ctr("bc1", "web/gw-1", "app", false, "CONTAINER_RUNNING"),
                ctr("bc1", "web/gw-1", "proxy", false, "CONTAINER_RUNNING"),
            ],
            problems: vec!["bc2: timed out after 10s".into()],
        });
        c
    }

    fn press(c: &mut ContainersComponent, code: KeyCode) -> Option<Action> {
        c.handle_key_event(KeyEvent::from(code)).unwrap()
    }

    fn screen(c: &mut ContainersComponent) -> String {
        let backend = ratatui::backend::TestBackend::new(110, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| c.draw(f, f.area()).unwrap()).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Pods start folded; Enter unfolds; the logs follow the selection
    #[test]
    fn folds_and_logs_follow() {
        let mut c = component();
        c.sync_logs();
        let s = screen(&mut c);
        assert!(s.contains("bc2: timed out after 10s"), "{s}");
        assert!(s.contains("▸ gw-1"), "{s}");
        assert!(!s.contains("proxy"), "{s}");
        assert!(c.logs.command().contains("logs apid"));
        press(&mut c, KeyCode::Down);
        assert!(c.logs.command().starts_with("kubectl -n web logs gw-1"));
        press(&mut c, KeyCode::Enter);
        let s = screen(&mut c);
        assert!(s.contains("▾ gw-1") && s.contains("proxy"), "{s}");
        press(&mut c, KeyCode::Down);
        assert!(
            c.logs
                .command()
                .contains("--namespace cri web/gw-1:app:abc"),
            "{}",
            c.logs.command()
        );
        // Enter on a container folds its pod, selecting the pod
        press(&mut c, KeyCode::Enter);
        assert!(!screen(&mut c).contains("proxy"));
        assert_eq!(c.selected_id, "bc1/web/gw-1");
    }

    /// Sort keys follow the columns; with one node they start at NAMESPACE
    #[test]
    fn sort_keys_and_logs_keys() {
        let mut c = component();
        press(&mut c, KeyCode::Char('1'));
        assert_eq!(c.options.sort, ContainerSort::Node);
        c.nodes.truncate(1);
        press(&mut c, KeyCode::Char('1'));
        assert_eq!(c.options.sort, ContainerSort::Namespace);
        // `l` hands the digits to the logs' levels; Esc back
        press(&mut c, KeyCode::Char('l'));
        press(&mut c, KeyCode::Char('3'));
        assert_eq!(c.options.sort, ContainerSort::Namespace);
        press(&mut c, KeyCode::Esc);
        assert!(!c.logs_keys);
        assert!(matches!(press(&mut c, KeyCode::Esc), Some(Action::Back)));
    }
}
