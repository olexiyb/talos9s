//! Node info component - containers, machine config and system extensions
//!
//! - Containers: Talos system services (containerd `system` namespace) and
//!   Kubernetes containers (CRI), from the Containers API
//! - Config: the machine config in effect (`talosctl get machineconfig`),
//!   read-only, with secrets masked before it is kept or shown
//! - Extensions: installed system extensions (`talosctl get extensions`)

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::time::Duration;
use talos_rs::{
    ContainerInfo, ContainerKind, ExtensionInfo, TalosClient, get_extensions_for_node,
    get_machine_config_text_for_node,
};
use talos9s_core::AsyncState;

/// Auto-refresh interval in seconds (containers only; the config and
/// extensions reload on `r`)
const AUTO_REFRESH_INTERVAL_SECS: u64 = 30;
/// Upper bound for one Containers call
const CONTAINERS_TIMEOUT: Duration = Duration::from_secs(10);

/// Tabs of the node info view
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeInfoTab {
    #[default]
    Containers,
    Config,
    Extensions,
}

impl NodeInfoTab {
    const ALL: [NodeInfoTab; 3] = [
        NodeInfoTab::Containers,
        NodeInfoTab::Config,
        NodeInfoTab::Extensions,
    ];

    fn index(&self) -> usize {
        Self::ALL.iter().position(|t| t == self).unwrap_or(0)
    }
}

/// A machine config line as shown: secrets already replaced
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLine {
    pub text: String,
    /// A value on this line was hidden
    pub masked: bool,
}

/// The machine config with its secrets masked
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaskedConfig {
    pub lines: Vec<ConfigLine>,
    /// Values hidden
    pub hidden: usize,
    pub version: String,
    pub updated: Option<String>,
    pub staged_differs: bool,
}

/// Loaded node info (wrapped by AsyncState). Each source fails on its own.
#[derive(Debug, Clone, Default)]
pub struct NodeInfoData {
    pub containers: Vec<ContainerInfo>,
    pub containers_error: Option<String>,
    pub config: Option<MaskedConfig>,
    pub config_error: Option<String>,
    pub extensions: Vec<ExtensionInfo>,
    pub extensions_error: Option<String>,
    /// Per tab: a load of its source finished, with data or an error
    pub attempted: [bool; 3],
}

/// Placeholder for a hidden value
const HIDDEN: &str = "••••••";

/// Whether a key's value is a secret: keys, tokens, passwords, certificates'
/// private parts, registry auth, encryption secrets
fn secret_key(key: &str) -> bool {
    let key = key.trim_matches(|c| c == '"' || c == '\'').to_lowercase();
    key.ends_with("key")
        || key.ends_with("keys")
        || key == "auth"
        || ["token", "secret", "password", "passphrase", "credential"]
            .iter()
            .any(|s| key.contains(s))
}

/// Keys whose children are hidden whole: a Kubernetes Secret's values in an
/// inline manifest are keyed by arbitrary names
fn hide_children(key: &str) -> bool {
    matches!(key, "data" | "stringData")
}

/// Mask the secrets of a machine config, line by line, so it works the same
/// on the config's YAML and on the YAML, TOML or INI held in its block
/// scalars (inline manifests, `machine.files`):
///
/// - `key: value` and `key = value` with a secret key name: the value
/// - a secret key with a nested value or block scalar: everything under it
/// - `data:` / `stringData:` (Secret manifests): everything under them
/// - PEM private key blocks anywhere
pub fn mask_config(text: &str) -> (Vec<ConfigLine>, usize) {
    let mut lines: Vec<ConfigLine> = Vec::new();
    let mut hidden = 0;
    // Hiding lines indented deeper than this, into the placeholder line at
    // the index, counting them
    let mut hiding: Option<(usize, usize, usize)> = None;
    let mut in_pem = false;

    let finish = |lines: &mut Vec<ConfigLine>, hiding: Option<(usize, usize, usize)>| {
        if let Some((_, at, count)) = hiding
            && count > 0
        {
            lines[at]
                .text
                .push_str(&format!("  ({count} lines hidden)"));
        }
    };

    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        if let Some((depth, at, count)) = hiding {
            // A list may sit at its key's indent
            let child = indent > depth || (indent == depth && line[indent..].starts_with("- "));
            if line.trim().is_empty() || child {
                hiding = Some((depth, at, count + 1));
                continue;
            }
            finish(&mut lines, hiding);
            hiding = None;
        }
        if in_pem {
            if line.contains("-----END") {
                in_pem = false;
            }
            continue;
        }
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
            in_pem = !line.contains("-----END");
            hidden += 1;
            lines.push(ConfigLine {
                text: format!("{}{HIDDEN}  (private key hidden)", &line[..indent]),
                masked: true,
            });
            continue;
        }

        let trimmed = &line[indent..];
        let (item, body) = match trimmed.strip_prefix("- ") {
            Some(rest) => ("- ", rest),
            None => ("", trimmed),
        };
        let column = indent + item.len();
        let yaml = body
            .split_once(':')
            .filter(|(k, v)| {
                !k.is_empty()
                    && !k.starts_with('#')
                    && !k.contains(' ')
                    && (v.is_empty() || v.starts_with(' '))
            })
            .map(|(k, v)| (k, v.trim(), ": "));
        let ini = || {
            body.split_once('=')
                .map(|(k, v)| (k.trim(), v.trim(), " = "))
                .filter(|(k, v, _)| !k.is_empty() && !k.contains(' ') && !v.is_empty())
        };
        let Some((key, value, sep)) = yaml.or_else(ini) else {
            lines.push(ConfigLine {
                text: line.to_string(),
                masked: false,
            });
            continue;
        };

        let nested = value.is_empty() || value.starts_with('|') || value.starts_with('>');
        if secret_key(key) || (nested && hide_children(key)) {
            hidden += 1;
            let prefix = &line[..column];
            lines.push(ConfigLine {
                text: format!("{prefix}{key}{sep}{HIDDEN}"),
                masked: true,
            });
            if nested {
                hiding = Some((column, lines.len() - 1, 0));
            }
        } else {
            lines.push(ConfigLine {
                text: line.to_string(),
                masked: false,
            });
        }
    }
    finish(&mut lines, hiding);
    (lines, hidden)
}

/// Text being typed for the filter (lists) or search (config)
#[derive(Debug, Clone, Default)]
struct Query {
    text: String,
    editing: bool,
}

/// Node info component
#[derive(Clone)]
pub struct NodeInfoComponent {
    state: AsyncState<NodeInfoData>,
    hostname: String,
    tab: NodeInfoTab,
    client: Option<TalosClient>,
    node_address: String,
    context: Option<String>,
    config_path: Option<String>,
    /// Reload the config and extensions on the next refresh, not only the
    /// containers
    reload_all: bool,
    auto_refresh: bool,
    /// Per tab
    queries: [Query; 3],
    containers_table: TableState,
    extensions_table: TableState,
    /// First config line shown, and the first column
    config_scroll: usize,
    config_hscroll: usize,
    /// Config lines shown at once, from the last draw
    config_page: usize,
    /// Where the open list was last drawn, for mouse clicks
    content_area: Rect,
}

impl Default for NodeInfoComponent {
    fn default() -> Self {
        Self::new(
            String::new(),
            String::new(),
            None,
            None,
            NodeInfoTab::default(),
        )
    }
}

impl NodeInfoComponent {
    pub fn new(
        hostname: String,
        address: String,
        context: Option<String>,
        config_path: Option<String>,
        tab: NodeInfoTab,
    ) -> Self {
        let mut containers_table = TableState::default();
        containers_table.select(Some(0));
        let mut extensions_table = TableState::default();
        extensions_table.select(Some(0));
        Self {
            // Loading until the first (background) refresh lands
            state: {
                let mut state = AsyncState::with_data(NodeInfoData::default());
                state.start_loading();
                state
            },
            hostname,
            tab,
            client: None,
            node_address: address.split(':').next().unwrap_or(&address).to_string(),
            context,
            config_path,
            reload_all: true,
            auto_refresh: true,
            queries: Default::default(),
            containers_table,
            extensions_table,
            config_scroll: 0,
            config_hscroll: 0,
            config_page: 20,
            content_area: Rect::default(),
        }
    }

    /// Set the client (pointed at this node) for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    fn data(&self) -> Option<&NodeInfoData> {
        self.state.data()
    }

    /// Load the open tab's source first, then the others; afterwards reload
    /// the containers each time, and the config and extensions on `r` or
    /// after a failure
    pub async fn refresh(&mut self) -> Result<()> {
        self.state.start_loading();
        let Some(client) = &self.client else {
            self.state.set_error("No Talos client configured");
            return Ok(());
        };
        let mut data = self.state.take_data().unwrap_or_default();

        let first_pass = !data.attempted.iter().any(|a| *a);
        let want = |tab: NodeInfoTab| {
            if first_pass {
                return tab == self.tab;
            }
            match tab {
                NodeInfoTab::Containers => true,
                NodeInfoTab::Config => {
                    self.reload_all || data.config_error.is_some() || !data.attempted[1]
                }
                NodeInfoTab::Extensions => {
                    self.reload_all || data.extensions_error.is_some() || !data.attempted[2]
                }
            }
        };
        let (want_containers, want_config, want_extensions) = (
            want(NodeInfoTab::Containers),
            want(NodeInfoTab::Config),
            want(NodeInfoTab::Extensions),
        );

        let containers = async {
            if !want_containers {
                return None;
            }
            let (system, kubernetes) = tokio::join!(
                tokio::time::timeout(CONTAINERS_TIMEOUT, client.containers(ContainerKind::System)),
                tokio::time::timeout(
                    CONTAINERS_TIMEOUT,
                    client.containers(ContainerKind::Kubernetes)
                ),
            );
            let flatten = |r: std::result::Result<_, _>| match r {
                Ok(Ok(list)) => Ok(list),
                Ok(Err(e)) => Err(format!("{e}")),
                Err(_) => Err(format!("timed out after {}s", CONTAINERS_TIMEOUT.as_secs())),
            };
            Some((flatten(system), flatten(kubernetes)))
        };
        let context = self.context.as_deref();
        let config_path = self.config_path.as_deref();
        let node = self.node_address.as_str();
        let config = async {
            if want_config {
                Some(get_machine_config_text_for_node(context, node, config_path).await)
            } else {
                None
            }
        };
        let extensions = async {
            if want_extensions {
                Some(get_extensions_for_node(context, node, config_path).await)
            } else {
                None
            }
        };
        let (containers, config, extensions) = tokio::join!(containers, config, extensions);

        // Kubernetes containers are missing on a node without kubelet; the
        // system ones alone still make the tab useful
        if let Some((system, kubernetes)) = containers {
            match (system, kubernetes) {
                (Ok(mut system), Ok(kubernetes)) => {
                    system.extend(kubernetes);
                    data.containers = system;
                    data.containers_error = None;
                }
                (Ok(system), Err(e)) => {
                    data.containers = system;
                    data.containers_error = Some(format!("Kubernetes containers: {e}"));
                }
                (Err(e), Ok(kubernetes)) => {
                    data.containers = kubernetes;
                    data.containers_error = Some(format!("System containers: {e}"));
                }
                (Err(e), Err(_)) => data.containers_error = Some(e),
            }
            sort_containers(&mut data.containers);
            data.attempted[NodeInfoTab::Containers.index()] = true;
        }
        if let Some(config) = config {
            match config {
                Ok(config) => {
                    let (lines, hidden) = mask_config(&config.active);
                    data.config = Some(MaskedConfig {
                        lines,
                        hidden,
                        version: config.version,
                        updated: config.updated,
                        staged_differs: config.staged_differs,
                    });
                    data.config_error = None;
                }
                Err(e) => data.config_error = Some(e.to_string()),
            }
            data.attempted[NodeInfoTab::Config.index()] = true;
        }
        if let Some(extensions) = extensions {
            match extensions {
                Ok(extensions) => {
                    data.extensions = extensions;
                    data.extensions_error = None;
                }
                Err(e) => data.extensions_error = Some(e.to_string()),
            }
            data.attempted[NodeInfoTab::Extensions.index()] = true;
        }
        if want_config && want_extensions {
            self.reload_all = false;
        }

        self.state.set_data(data);
        Ok(())
    }

    /// The open tab
    pub fn tab(&self) -> NodeInfoTab {
        self.tab
    }

    fn query(&self) -> &Query {
        &self.queries[self.tab.index()]
    }

    fn query_mut(&mut self) -> &mut Query {
        &mut self.queries[self.tab.index()]
    }

    /// Containers shown: pod sandboxes left out, filtered
    fn visible_containers(&self) -> Vec<&ContainerInfo> {
        let filter = self.queries[NodeInfoTab::Containers.index()]
            .text
            .to_lowercase();
        self.data()
            .map(|d| {
                d.containers
                    .iter()
                    .filter(|c| !c.sandbox)
                    .filter(|c| {
                        filter.is_empty()
                            || [&c.id, &c.pod, &c.name, &c.image, &c.status]
                                .iter()
                                .any(|f| f.to_lowercase().contains(&filter))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visible_extensions(&self) -> Vec<&ExtensionInfo> {
        let filter = self.queries[NodeInfoTab::Extensions.index()]
            .text
            .to_lowercase();
        self.data()
            .map(|d| {
                d.extensions
                    .iter()
                    .filter(|e| {
                        filter.is_empty()
                            || [&e.name, &e.version, &e.author, &e.description]
                                .iter()
                                .any(|f| f.to_lowercase().contains(&filter))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn config_lines(&self) -> &[ConfigLine] {
        self.data()
            .and_then(|d| d.config.as_ref())
            .map_or(&[], |c| c.lines.as_slice())
    }

    /// Move the selection or scroll by `delta` rows
    fn move_by(&mut self, delta: isize) {
        let step = |table: &mut TableState, len: usize| {
            if len == 0 {
                return;
            }
            let i = table.selected().unwrap_or(0) as isize + delta;
            table.select(Some(i.clamp(0, len as isize - 1) as usize));
        };
        match self.tab {
            NodeInfoTab::Containers => {
                let len = self.visible_containers().len();
                step(&mut self.containers_table, len);
            }
            NodeInfoTab::Extensions => {
                let len = self.visible_extensions().len();
                step(&mut self.extensions_table, len);
            }
            NodeInfoTab::Config => {
                let max = self.config_lines().len().saturating_sub(1) as isize;
                self.config_scroll = (self.config_scroll as isize + delta).clamp(0, max) as usize;
            }
        }
    }

    /// Scroll the config to the next (or previous) line matching the search
    fn search_config(&mut self, forward: bool) {
        let needle = self.queries[NodeInfoTab::Config.index()]
            .text
            .to_lowercase();
        if needle.is_empty() {
            return;
        }
        let lines = self.config_lines();
        let n = lines.len();
        if n == 0 {
            return;
        }
        let found = (1..=n)
            .map(|i| {
                if forward {
                    (self.config_scroll + i) % n
                } else {
                    (self.config_scroll + n - i % n) % n
                }
            })
            .find(|&i| lines[i].text.to_lowercase().contains(&needle));
        if let Some(i) = found {
            self.config_scroll = i;
        }
    }

    fn draw_error(frame: &mut Frame, area: Rect, title: &str, error: &str) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title.to_string());
        let text = Paragraph::new(format!("Could not read: {error}"))
            .style(Style::default().fg(Color::Red))
            .block(block);
        frame.render_widget(text, area);
    }

    fn draw_containers(&mut self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };
        let (mut running, mut exited) = (0, 0);
        for c in data.containers.iter().filter(|c| !c.sandbox) {
            if c.status.ends_with("RUNNING") {
                running += 1;
            } else if c.status.ends_with("EXITED") {
                exited += 1;
            }
        }
        let error = data.containers_error.clone();
        let visible = self.visible_containers();
        let header = Row::new(
            ["NAMESPACE", "POD", "CONTAINER", "IMAGE", "PID", "STATUS"]
                .map(|h| Cell::from(h).style(Style::default().add_modifier(Modifier::BOLD))),
        );
        let rows: Vec<Row> = visible
            .iter()
            .map(|c| {
                let (namespace, pod) = match c.kind {
                    ContainerKind::System => ("system", ""),
                    ContainerKind::Kubernetes => {
                        c.pod.split_once('/').unwrap_or(("", c.pod.as_str()))
                    }
                };
                let name = if c.kind == ContainerKind::System {
                    c.id.as_str()
                } else {
                    c.name.as_str()
                };
                let status = c
                    .status
                    .strip_prefix("CONTAINER_")
                    .unwrap_or(&c.status)
                    .to_string();
                let color = if c.status.ends_with("RUNNING") {
                    Color::Green
                } else if c.status.ends_with("EXITED") {
                    // Finished jobs and init containers exit normally
                    Color::DarkGray
                } else {
                    Color::Yellow
                };
                Row::new(vec![
                    Cell::from(namespace.to_string()),
                    Cell::from(pod.to_string()),
                    Cell::from(name.to_string()),
                    Cell::from(c.image.clone()),
                    Cell::from(if c.pid == 0 {
                        String::new()
                    } else {
                        c.pid.to_string()
                    }),
                    Cell::from(status).style(Style::default().fg(color)),
                ])
            })
            .collect();
        let mut title = format!(" Containers: {running} running, {exited} exited ");
        let filter = &self.queries[NodeInfoTab::Containers.index()].text;
        if !filter.is_empty() {
            title.push_str(&format!("· {} match \"{filter}\" ", visible.len()));
        }
        if let Some(e) = &error {
            title.push_str(&format!("· {e} "));
        }
        let widths = [
            Constraint::Length(20),
            Constraint::Percentage(28),
            Constraint::Percentage(18),
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(14),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_style(Style::default().fg(if error.is_some() {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    })),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(table, area, &mut self.containers_table);
        self.content_area = area;
    }

    fn draw_config(&mut self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };
        let Some(config) = &data.config else {
            match &data.config_error {
                Some(e) => Self::draw_error(frame, area, " Machine config ", e),
                None => frame.render_widget(
                    Paragraph::new("Loading machine config...")
                        .style(Style::default().fg(Color::DarkGray)),
                    area,
                ),
            }
            return;
        };
        let mut title = vec![Span::styled(
            format!(
                " Machine config, revision {}{} · {} secrets hidden ",
                config.version,
                config
                    .updated
                    .as_deref()
                    .map(|u| format!(" (updated {u})"))
                    .unwrap_or_default(),
                config.hidden
            ),
            Style::default().fg(Color::Cyan),
        )];
        if config.staged_differs {
            title.push(Span::styled(
                "· a staged change waits for a reboot ",
                Style::default().fg(Color::Yellow),
            ));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(title));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let page = inner.height as usize;
        let total = config.lines.len();
        let needle = self.queries[NodeInfoTab::Config.index()]
            .text
            .to_lowercase();
        let number_width = total.to_string().len();
        let lines: Vec<Line> = config
            .lines
            .iter()
            .enumerate()
            .skip(self.config_scroll)
            .take(page)
            .map(|(i, line)| {
                let text: String = line.text.chars().skip(self.config_hscroll).collect();
                let mut spans = vec![Span::styled(
                    format!("{:>number_width$} ", i + 1),
                    Style::default().fg(Color::DarkGray),
                )];
                spans.extend(config_spans(&text, line.masked));
                let mut rendered = Line::from(spans);
                if !needle.is_empty() && line.text.to_lowercase().contains(&needle) {
                    rendered = rendered.style(Style::default().bg(Color::DarkGray));
                }
                rendered
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        self.config_page = page.max(1);
    }

    fn draw_extensions(&mut self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };
        if let (Some(e), true) = (&data.extensions_error, data.extensions.is_empty()) {
            Self::draw_error(frame, area, " Extensions ", e);
            return;
        }
        let total = data.extensions.len();
        let visible = self.visible_extensions();
        let selected = self
            .extensions_table
            .selected()
            .and_then(|i| visible.get(i))
            .map(|e| (*e).clone());
        let chunks = Layout::vertical([Constraint::Min(5), Constraint::Length(5)]).split(area);
        let header = Row::new(
            ["NAME", "VERSION", "AUTHOR"]
                .map(|h| Cell::from(h).style(Style::default().add_modifier(Modifier::BOLD))),
        );
        let rows: Vec<Row> = visible
            .iter()
            .map(|e| {
                Row::new(vec![
                    Cell::from(e.name.clone()).style(Style::default().fg(Color::Green)),
                    Cell::from(e.version.clone()),
                    Cell::from(e.author.clone()),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(35),
                Constraint::Min(15),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Extensions: {total} installed "))
                .title_style(Style::default().fg(Color::Cyan)),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(table, chunks[0], &mut self.extensions_table);
        self.content_area = chunks[0];

        let detail = match selected {
            Some(e) => vec![
                Line::from(e.description),
                Line::from(Span::styled(
                    format!("image {}", e.image),
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            None => vec![Line::from("No extensions installed")],
        };
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(ratatui::widgets::Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Details ")
                        .title_style(Style::default().fg(Color::Yellow)),
                ),
            chunks[1],
        );
    }

    fn key_hints_line(&self) -> Line<'static> {
        let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Cyan));
        let query = self.query();
        let spans = if query.editing {
            let what = if self.tab == NodeInfoTab::Config {
                "Search"
            } else {
                "Filter"
            };
            vec![
                Span::raw(format!(" {what}: ")),
                Span::styled(format!("{}▏", query.text), Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                key("Enter"),
                Span::raw(" done  "),
                key("Esc"),
                Span::raw(" clear"),
            ]
        } else {
            let mut spans = vec![
                key(" ↑/↓"),
                Span::raw(if self.tab == NodeInfoTab::Config {
                    " scroll  "
                } else {
                    " navigate  "
                }),
                key("/"),
            ];
            if self.tab == NodeInfoTab::Config {
                spans.extend([
                    Span::raw(" search  "),
                    key("n/N"),
                    Span::raw(" next/prev  "),
                    key("g/G"),
                    Span::raw(" top/bottom  "),
                    key("←/→"),
                    Span::raw(" pan  "),
                ]);
            } else {
                spans.push(Span::raw(" filter  "));
            }
            spans.extend([key("r"), Span::raw(" refresh")]);
            spans
        };
        Line::from(spans)
    }
}

/// Containers in a stable order: system services, then Kubernetes ones by
/// pod and name
fn sort_containers(containers: &mut [ContainerInfo]) {
    containers.sort_by(|a, b| {
        let kind = |c: &ContainerInfo| c.kind == ContainerKind::Kubernetes;
        kind(a)
            .cmp(&kind(b))
            .then_with(|| a.pod.cmp(&b.pod))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// A config line colored: keys, comments, document separators, masked values
fn config_spans(text: &str, masked: bool) -> Vec<Span<'static>> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('#') {
        return vec![Span::styled(
            text.to_string(),
            Style::default().fg(Color::DarkGray),
        )];
    }
    if trimmed == "---" {
        return vec![Span::styled(
            text.to_string(),
            Style::default().fg(Color::Magenta),
        )];
    }
    let value_style = if masked {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    match text
        .split_once(": ")
        .or_else(|| text.strip_suffix(':').map(|k| (k, "")))
    {
        Some((key, value))
            if !key.trim_start().contains(' ') || key.trim_start().starts_with("- ") =>
        {
            let mut spans = vec![Span::styled(
                format!("{key}:"),
                Style::default().fg(Color::Cyan),
            )];
            if !value.is_empty() {
                spans.push(Span::styled(format!(" {value}"), value_style));
            }
            spans
        }
        _ => vec![Span::styled(text.to_string(), value_style)],
    }
}

impl Component for NodeInfoComponent {
    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn captures_text(&self) -> bool {
        self.query().editing
    }

    fn uses_horizontal_keys(&self) -> bool {
        // Scrolled config: Left scrolls back; at column 0 it leaves the view
        self.tab == NodeInfoTab::Config && self.config_hscroll > 0
    }

    /// Click a row to select it; the wheel scrolls
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        let at = |r: &Rect| {
            mouse.column >= r.x
                && mouse.column < r.right()
                && mouse.row >= r.y
                && mouse.row < r.bottom()
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Table rows start below the border and the header
                let area = self.content_area;
                if self.tab == NodeInfoTab::Config || !at(&area) || mouse.row < area.y + 2 {
                    return Ok(None);
                }
                let (table, len) = match self.tab {
                    NodeInfoTab::Containers => {
                        let len = self.visible_containers().len();
                        (&mut self.containers_table, len)
                    }
                    _ => {
                        let len = self.visible_extensions().len();
                        (&mut self.extensions_table, len)
                    }
                };
                let row = table.offset() + (mouse.row - area.y - 2) as usize;
                if row < len {
                    table.select(Some(row));
                }
            }
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::ScrollDown => self.move_by(3),
            _ => {}
        }
        Ok(None)
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if self.query().editing {
            match key.code {
                KeyCode::Enter => {
                    self.query_mut().editing = false;
                    if self.tab == NodeInfoTab::Config {
                        self.search_config(true);
                    }
                }
                KeyCode::Esc => *self.query_mut() = Query::default(),
                KeyCode::Backspace => {
                    self.query_mut().text.pop();
                }
                KeyCode::Char(c) => self.query_mut().text.push(c),
                _ => {}
            }
            // The list shrinks as the filter narrows
            self.containers_table.select(Some(0));
            self.extensions_table.select(Some(0));
            return Ok(None);
        }
        let page = self.config_page as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.query().text.is_empty() {
                    return Ok(Some(Action::Back));
                }
                *self.query_mut() = Query::default();
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Left if self.tab == NodeInfoTab::Config => {
                self.config_hscroll = self.config_hscroll.saturating_sub(8);
            }
            KeyCode::Right if self.tab == NodeInfoTab::Config => self.config_hscroll += 8,
            KeyCode::Char('/') => {
                *self.query_mut() = Query {
                    text: String::new(),
                    editing: true,
                };
            }
            KeyCode::Char('n') if self.tab == NodeInfoTab::Config => self.search_config(true),
            KeyCode::Char('N') if self.tab == NodeInfoTab::Config => self.search_config(false),
            KeyCode::Char('r') => {
                self.reload_all = true;
                return Ok(Some(Action::Refresh));
            }
            _ => {}
        }
        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // The first refresh loaded only the open tab; load the rest now
            if self
                .data()
                .is_some_and(|d| d.attempted.iter().any(|a| *a) && !d.attempted.iter().all(|a| *a))
                && !self.state.is_loading()
            {
                return Ok(Some(Action::Refresh));
            }
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        if let Some(err) = self.state.error() {
            frame.render_widget(
                Paragraph::new(format!("Error: {err}")).style(Style::default().fg(Color::Red)),
                area,
            );
            return Ok(());
        }
        // Containers, Machine config and Extensions are separate menu
        // entries: no tab bar, and the window title names the node
        let attempted = self.data().is_some_and(|d| d.attempted[self.tab.index()]);
        if !attempted {
            let what = match self.tab {
                NodeInfoTab::Containers => "containers",
                NodeInfoTab::Config => "machine config",
                NodeInfoTab::Extensions => "extensions",
            };
            frame.render_widget(
                Paragraph::new(format!("Loading {what} of {}...", self.hostname))
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
        } else {
            match self.tab {
                NodeInfoTab::Containers => self.draw_containers(frame, area),
                NodeInfoTab::Config => self.draw_config(frame, area),
                NodeInfoTab::Extensions => self.draw_extensions(frame, area),
            }
        }
        Ok(())
    }
}

impl crate::components::BackgroundRefresh for NodeInfoComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        self.state = refreshed.state;
        // A reload asked for during the refresh still runs next time
        self.reload_all &= refreshed.reload_all;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(text: &str) -> (Vec<String>, usize) {
        let (lines, hidden) = mask_config(text);
        (lines.into_iter().map(|l| l.text).collect(), hidden)
    }

    #[test]
    fn masks_secret_values_and_keeps_the_rest() {
        let config = "\
version: v1alpha1
machine:
    type: controlplane
    token: abc.def
    ca:
        crt: LS0tPUBLIC
        key: LS0tPRIVATE
    certSANs:
        - 10.0.0.1
cluster:
    secretboxEncryptionSecret: c2VjcmV0
    aggregatorCA:
        crt: LS0tPUBLIC
    serviceAccount:
        key: |
            -----BEGIN EC PRIVATE KEY-----
            MHcCAQEEIOK
            -----END EC PRIVATE KEY-----
    clusterName: bc
";
        let (lines, hidden) = masked(config);
        let text = lines.join("\n");
        for secret in [
            "abc.def",
            "LS0tPRIVATE",
            "c2VjcmV0",
            "MHcCAQEEIOK",
            "PRIVATE KEY",
        ] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(text.contains("    token: ••••••"));
        assert!(text.contains("        key: ••••••  (3 lines hidden)"));
        assert!(text.contains("crt: LS0tPUBLIC"));
        assert!(text.contains("- 10.0.0.1"));
        assert!(text.contains("    clusterName: bc"));
        assert_eq!(hidden, 4);
    }

    #[test]
    fn masks_secrets_in_inline_manifests_and_files() {
        let config = "\
    inlineManifests:
        - name: creds
          contents: |
            apiVersion: v1
            kind: Secret
            metadata:
              name: hcloud
            stringData:
              token: hc-123
              network: net-1
            ---
            kind: ConfigMap
            data:
              ccm.yaml: plain
    files:
        - content: |
            [plugins.\"io.containerd.grpc.v1.cri\".registry.configs.\"r\".auth]
              password = \"hunter2\"
              username = \"me\"
          path: /etc/cri/conf.d/20-customization.part
---
apiVersion: v1alpha1
kind: RegistryAuthConfig
name: ghcr.io
username: me
password: hunter2
auth: bWU6aHVudGVyMg==
";
        let (lines, _) = masked(config);
        let text = lines.join("\n");
        for secret in ["hc-123", "net-1", "hunter2", "bWU6aHVudGVyMg==", "plain"] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(text.contains("            stringData: ••••••  (2 lines hidden)"));
        assert!(text.contains("              password = ••••••"));
        assert!(text.contains("              username = \"me\""));
        assert!(text.contains("username: me"));
        assert!(text.contains("path: /etc/cri/conf.d/20-customization.part"));
        assert!(text.contains("kind: RegistryAuthConfig"));
    }

    #[test]
    fn plain_text_is_unchanged() {
        let config = "# comment: with colon\nmachine:\n    install:\n        disk: /dev/sda\n";
        let (lines, hidden) = masked(config);
        assert_eq!(lines.join("\n") + "\n", config);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn orders_system_containers_first() {
        let container = |kind, pod: &str, name: &str| ContainerInfo {
            kind,
            id: name.to_string(),
            name: name.to_string(),
            pod: pod.to_string(),
            sandbox: false,
            image: String::new(),
            pid: 1,
            status: "RUNNING".to_string(),
        };
        let mut list = vec![
            container(ContainerKind::Kubernetes, "kube-system/b", "x"),
            container(ContainerKind::System, "", "trustd"),
            container(ContainerKind::Kubernetes, "kube-system/a", "y"),
            container(ContainerKind::System, "", "apid"),
        ];
        sort_containers(&mut list);
        let names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["apid", "trustd", "y", "x"]);
    }
}
