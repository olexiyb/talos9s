//! Containers view: every container on a node, a group or the cluster, as
//! containerd reports it through the Talos API (no Kubernetes API needed)
//!
//! One row per container, Talos services included; sorted by any column
//! (`1`-`7`, a header click), filtered (`/`), warped into namespaces (`W`),
//! exited containers shown with `A`. Enter focuses one container: its row
//! and its logs, read through the Talos API too; Esc goes back.

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
    ContainerOptions, ContainerSort, CtrState, NodeContainer, container_rows, state_counts,
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
    /// The selected container (`NodeContainer::key`), kept across refreshes
    selected_id: String,
    table_state: TableState,
    filtering: bool,
    recent_namespaces: Vec<String>,
    picker: Option<NamespacePicker>,
    /// The focused container (Enter): its key; the list shows only it and
    /// the logs pane below
    focus: Option<String>,
    logs: LogPane,
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
            focus: None,
            logs: LogPane::default(),
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
        self.options.warp = warp;
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

    /// The rows shown: the focused container only, else the list
    fn list_rows(&self) -> Vec<usize> {
        let all = self.all();
        match &self.focus {
            Some(key) => all
                .iter()
                .position(|c| &c.key() == key)
                .into_iter()
                .collect(),
            None => container_rows(all, &self.options),
        }
    }

    fn selected_index(&self, rows: &[usize]) -> usize {
        let all = self.all();
        rows.iter()
            .position(|&i| all.get(i).is_some_and(|c| c.key() == self.selected_id))
            .unwrap_or(0)
    }

    fn select(&mut self, rows: &[usize], index: usize) {
        if let Some(c) = rows.get(index).and_then(|&i| self.all().get(i)) {
            self.selected_id = c.key();
        }
    }

    fn selected(&self) -> Option<&NodeContainer> {
        let rows = self.list_rows();
        rows.get(self.selected_index(&rows))
            .and_then(|&i| self.all().get(i))
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
        Ok(())
    }

    /// Enter: the selected container and its logs
    fn open_focus(&mut self) {
        let Some(c) = self.selected().cloned() else {
            return;
        };
        let title = match c.info.kind {
            ContainerKind::System => format!("{} on {}", c.name(), c.node),
            ContainerKind::Kubernetes => format!("{} · {} on {}", c.pod(), c.name(), c.node),
        };
        let source = LogSource {
            namespace: c.namespace().to_string(),
            pod: c.pod().to_string(),
            container: c.name().to_string(),
            label: String::new(),
            has_previous: false,
            talos: Some(TalosLog {
                address: c.address.clone(),
                kind: c.info.kind,
                id: c.info.id.clone(),
            }),
            file: None,
        };
        self.logs.set_sources(title, vec![source], logs_command(&c));
        self.selected_id = c.key();
        self.focus = Some(c.key());
    }

    fn move_selection(&mut self, step: crate::ui_ext::Move) {
        let rows = self.list_rows();
        let current = self.selected_index(&rows);
        if let Some(i) = crate::ui_ext::moved(current, rows.len(), step) {
            self.select(&rows, i);
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
            TextEdit::Changed => self.options.filter = self.options.filter.to_lowercase(),
            TextEdit::Ignored => {}
        }
    }

    /// Keys of the focused container: the logs take most
    fn handle_focus_key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.logs.captures_text()
            || self.logs.selecting()
            || (key.code == KeyCode::Esc && self.logs.has_search())
        {
            self.logs.handle_key(&key, None);
            return None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.focus = None,
            KeyCode::Char('y') => crate::clipboard::copy_to_clipboard(self.logs.command()),
            KeyCode::Char('v') => return self.open_in_viewer(),
            KeyCode::Char('a') => self.auto_refresh = !self.auto_refresh,
            KeyCode::Up | KeyCode::Char('k') => self.logs.scroll_lines(-1),
            KeyCode::Down | KeyCode::Char('j') => self.logs.scroll_lines(1),
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

    fn table_row(&self, c: &NodeContainer) -> Row<'static> {
        let dim = Style::default().dim();
        let style = match c.state() {
            CtrState::Running => Style::default().fg(Color::Green),
            // Finished init containers and jobs exit normally
            CtrState::Exited => dim,
            CtrState::Other => Style::default().fg(Color::Yellow),
        };
        let status = c
            .info
            .status
            .strip_prefix("CONTAINER_")
            .unwrap_or(&c.info.status)
            .to_string();
        let mut cells = vec![Cell::from("")];
        if self.multi_node() {
            cells.push(Cell::from(c.node.clone()));
        }
        cells.extend([
            Cell::from(c.namespace().to_string()),
            Cell::from(c.pod().to_string()),
            Cell::from(c.name().to_string()),
            Cell::from(c.info.image.clone()).style(dim),
            Cell::from(if c.info.pid == 0 {
                String::new()
            } else {
                c.info.pid.to_string()
            }),
            Cell::from(status).style(style),
        ]);
        Row::new(cells)
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let rows = self.list_rows();
        let all = self.all();
        let to_u16 = talos9s_core::convert::to_u16;
        // Columns as wide as what they hold; the room left goes to IMAGE
        let longest = |f: &dyn Fn(&NodeContainer) -> usize, min: usize, max: usize| {
            rows.iter()
                .filter_map(|&i| all.get(i))
                .map(f)
                .max()
                .unwrap_or(min)
                .clamp(min, max)
        };
        let node = longest(&|c| c.node.chars().count(), 4, 16);
        let namespace = longest(&|c| c.namespace().chars().count(), 9, 24);
        let pod = longest(&|c| c.pod().chars().count(), 3, 48);
        let name = longest(&|c| c.name().chars().count(), 9, 32);
        let mut columns: Vec<(&str, Constraint, Option<ContainerSort>)> =
            vec![("", Constraint::Length(1), None)];
        if self.multi_node() {
            columns.push((
                "NODE",
                Constraint::Length(to_u16(node + 1)),
                Some(ContainerSort::Node),
            ));
        }
        columns.extend([
            (
                "NAMESPACE",
                Constraint::Length(to_u16(namespace + 1)),
                Some(ContainerSort::Namespace),
            ),
            (
                "POD",
                Constraint::Length(to_u16(pod + 1)),
                Some(ContainerSort::Pod),
            ),
            (
                "CONTAINER",
                Constraint::Length(to_u16(name + 1)),
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
        let table_rows: Vec<Row> = rows
            .iter()
            .filter_map(|&i| all.get(i))
            .map(|c| self.table_row(c))
            .collect();
        let selected = self.selected_index(&rows);
        self.table_state
            .select((!rows.is_empty()).then_some(selected));
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
        if rows.is_empty() {
            let text = if self.focus.is_some() {
                "The container is gone"
            } else if self.options.filter.is_empty() && self.options.warp.is_empty() {
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
        let logs = &self.logs;
        if logs.selecting() {
            return Line::from(vec![
                key("<j/k>"),
                Span::raw(" extend  "),
                key("<y>"),
                Span::raw(" copy lines  "),
                key("<Esc>"),
                Span::raw(" cancel"),
            ]);
        }
        if self.focus.is_some() {
            let search = if self.logs.has_search() {
                [key("<n/N>"), Span::raw(" match  ")]
            } else {
                [key("</>"), Span::raw(" find  ")]
            };
            let mut spans = vec![
                key("<PgUp/Dn>"),
                Span::raw(" scroll  "),
                key("<1-5/0>"),
                Span::raw(" level  "),
            ];
            spans.extend(search);
            spans.extend([
                key("<w>"),
                Span::raw(" wrap  "),
                key("<V>"),
                Span::raw(" copy  "),
                key("<v>"),
                Span::raw(" hl"),
            ]);
            return Line::from(spans);
        }
        Line::from(vec![
            key("<Enter>"),
            Span::raw(" logs  "),
            key("</>"),
            Span::raw(" filter  "),
            key(if self.multi_node() { "<1-7>" } else { "<1-6>" }),
            Span::raw(" sort  "),
            key("<A>"),
            Span::raw(if self.options.show_exited {
                " running  "
            } else {
                " exited  "
            }),
            key("<W>"),
            Span::raw(" ns  "),
            key("<y>"),
            Span::raw(" copy"),
        ])
    }
}

/// The talosctl command that reads `c`'s logs
fn logs_command(c: &NodeContainer) -> String {
    match c.info.kind {
        ContainerKind::System => format!("talosctl -n {} logs {}", c.address, c.name()),
        ContainerKind::Kubernetes => {
            format!(
                "talosctl -n {} logs --namespace cri {}",
                c.address, c.info.id
            )
        }
    }
}

impl BackgroundRefresh for ContainersComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        self.state = refreshed.state;
    }
}

impl Component for ContainersComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if self.picker.is_some() || self.logs.handle_mouse(&mouse) {
            return Ok(None);
        }
        if self.focus.is_none() {
            if let Some(sort) = self.header.clicked(&mouse) {
                self.options.sort = sort;
                return Ok(None);
            }
            if let Some(row) = self.rows.clicked(&mouse) {
                let rows = self.list_rows();
                self.select(&rows, row);
            }
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
                    Some(Action::WarpNamespace(warp))
                }
            });
        }
        if self.focus.is_some() {
            return Ok(self.handle_focus_key(key));
        }
        match key.code {
            KeyCode::Esc if !self.options.filter.is_empty() => self.options.filter.clear(),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Some(Action::Back)),
            KeyCode::Enter => self.open_focus(),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('A') => self.options.show_exited = !self.options.show_exited,
            KeyCode::Char('W') => self.open_picker(),
            KeyCode::Char(c @ '1'..='7') => {
                let i = usize::from(u8::try_from(c).unwrap_or(b'1') - b'1');
                // Without a NODE column the keys start at NAMESPACE
                let offset = usize::from(!self.multi_node());
                if let Some(&sort) = ContainerSort::ALL.get(i + offset) {
                    self.options.sort = sort;
                }
            }
            KeyCode::Char('r') => return Ok(Some(Action::Refresh)),
            KeyCode::Char('a') => self.auto_refresh = !self.auto_refresh,
            KeyCode::Char('y') => {
                if let Some(c) = self.selected() {
                    crate::clipboard::copy_to_clipboard(logs_command(c));
                }
            }
            _ => {
                if let Some(step) = crate::ui_ext::Move::from_key(&key) {
                    self.move_selection(step);
                }
            }
        }
        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            if self.focus.is_some() {
                self.logs.tick(None);
            }
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
        let show_filter =
            self.focus.is_none() && (self.filtering || !self.options.filter.is_empty());
        let list_height = if self.focus.is_some() { 2 } else { area.height };
        let [summary_area, filter_area, list, logs] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(u16::from(show_filter)),
            Constraint::Length(list_height),
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
        if self.focus.is_some() {
            self.logs.draw(frame, logs);
        }
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

    /// One row per container, no logs; Enter focuses one with its logs,
    /// Esc goes back to the list where it was
    #[test]
    fn enter_focuses_a_container() {
        let mut c = component();
        let s = screen(&mut c);
        assert!(s.contains("bc2: timed out after 10s"), "{s}");
        assert!(s.contains("apid") && s.contains("proxy"), "{s}");
        assert!(!s.contains("Logs ·"), "{s}");
        press(&mut c, KeyCode::Down);
        press(&mut c, KeyCode::Enter);
        let s = screen(&mut c);
        assert!(s.contains("Logs · gw-1 · app on bc1"), "{s}");
        assert!(!s.contains("proxy") && !s.contains("apid"), "{s}");
        assert_eq!(
            c.logs.command(),
            "talosctl -n 10.0.0.3 logs --namespace cri web/gw-1:app:abc"
        );
        // The digits are the logs' levels here, not the sort
        press(&mut c, KeyCode::Char('3'));
        assert_eq!(c.options.sort, ContainerSort::Namespace);
        press(&mut c, KeyCode::Esc);
        assert!(c.focus.is_none());
        assert_eq!(c.selected().map(NodeContainer::name), Some("app"));
        assert!(matches!(press(&mut c, KeyCode::Esc), Some(Action::Back)));
    }

    /// Sort keys follow the columns; with one node they start at NAMESPACE
    #[test]
    fn sort_keys_follow_columns() {
        let mut c = component();
        press(&mut c, KeyCode::Char('1'));
        assert_eq!(c.options.sort, ContainerSort::Node);
        press(&mut c, KeyCode::Char('3'));
        assert_eq!(c.options.sort, ContainerSort::Pod);
        c.nodes.truncate(1);
        press(&mut c, KeyCode::Char('1'));
        assert_eq!(c.options.sort, ContainerSort::Namespace);
    }
}
