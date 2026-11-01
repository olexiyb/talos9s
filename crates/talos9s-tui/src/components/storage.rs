//! Storage component - displays disk and volume information
//!
//! Shows physical disks and Talos volume status for a node.

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::time::Duration;
use talos_rs::{
    DiskInfo, TalosClient, VolumeStatus, get_disks_for_node, get_volume_status_for_node,
};
use talos9s_core::{AsyncState, format_bytes};

/// Auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 30;

/// View mode for the storage component
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageViewMode {
    #[default]
    Disks,
    /// Talos volumes: partitions with a filesystem and a mount point
    Partitions,
}

impl StorageViewMode {
    pub fn label(&self) -> &'static str {
        match self {
            StorageViewMode::Disks => "Disks",
            StorageViewMode::Partitions => "Partitions",
        }
    }
}

/// Loaded storage data (wrapped by AsyncState)
#[derive(Debug, Clone, Default)]
pub struct StorageData {
    /// Node hostname
    pub hostname: String,
    /// Node address
    pub address: String,
    /// Physical disks
    pub disks: Vec<DiskInfo>,
    /// Volume status
    pub volumes: Vec<VolumeStatus>,
}

/// Storage component for viewing disk and volume information
#[derive(Clone)]
pub struct StorageComponent {
    /// Async state wrapping all storage data
    state: AsyncState<StorageData>,

    /// Current view mode (Disks or Volumes)
    view_mode: StorageViewMode,

    /// Table state for disk list
    disk_table_state: TableState,

    /// Table state for volume list
    volume_table_state: TableState,
    /// Where the table's rows were drawn last, for clicks
    rows: crate::ui_ext::RowHits,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls (unused but kept for consistency)
    #[allow(dead_code)]
    client: Option<TalosClient>,

    /// Node address for talosctl commands
    node_address: Option<String>,

    /// Context name for authentication
    context: Option<String>,

    /// Config path for authentication
    config_path: Option<String>,
}

impl Default for StorageComponent {
    fn default() -> Self {
        Self::new("".to_string(), "".to_string(), None, None)
    }
}

impl StorageComponent {
    pub fn new(
        hostname: String,
        address: String,
        context: Option<String>,
        config_path: Option<String>,
    ) -> Self {
        let mut disk_table_state = TableState::default();
        disk_table_state.select(Some(0));
        let mut volume_table_state = TableState::default();
        volume_table_state.select(Some(0));

        let initial_data = StorageData {
            hostname,
            address: address.clone(),
            ..Default::default()
        };

        let node_address = if address.is_empty() {
            None
        } else {
            // Extract IP from address (remove port if present)
            Some(address.split(':').next().unwrap_or(&address).to_string())
        };

        Self {
            // Loading until the first (background) refresh lands
            state: {
                let mut state = AsyncState::with_data(initial_data);
                state.start_loading();
                state
            },
            view_mode: StorageViewMode::Disks,
            disk_table_state,
            volume_table_state,
            rows: Default::default(),
            auto_refresh: true,
            client: None,
            node_address,
            context,
            config_path,
        }
    }

    /// The tab shown: disks or partitions
    pub fn view_mode(&self) -> StorageViewMode {
        self.view_mode
    }

    pub fn set_view_mode(&mut self, mode: StorageViewMode) {
        self.view_mode = mode;
    }

    /// Set the client for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Helper to get data reference
    fn data(&self) -> Option<&StorageData> {
        self.state.data()
    }

    /// Refresh storage data
    pub async fn refresh(&mut self) -> Result<()> {
        self.state.start_loading();

        let Some(node) = &self.node_address else {
            self.state.set_error("No node address configured");
            return Ok(());
        };

        let Some(context) = &self.context else {
            self.state.set_error("No context configured");
            return Ok(());
        };

        // Get or create data
        let mut data = self.state.take_data().unwrap_or_default();

        // Fetch disk information using context-aware async function
        match get_disks_for_node(context, node, self.config_path.as_deref()).await {
            Ok(disks) => {
                data.disks = disks;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch disks: {}", e);
                data.disks.clear();
            }
        }

        // Fetch volume status using context-aware async function
        match get_volume_status_for_node(context, node, self.config_path.as_deref()).await {
            Ok(volumes) => {
                data.volumes = volumes;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch volumes: {}", e);
                data.volumes.clear();
            }
        }

        // Store the data
        self.state.set_data(data);
        Ok(())
    }

    /// Get selected disk index
    fn selected_disk_index(&self) -> usize {
        self.disk_table_state.selected().unwrap_or(0)
    }

    /// Get selected volume index
    fn selected_volume_index(&self) -> usize {
        self.volume_table_state.selected().unwrap_or(0)
    }

    /// Move selection up
    fn select_prev(&mut self) {
        match self.view_mode {
            StorageViewMode::Disks => {
                if let Some(data) = self.data()
                    && !data.disks.is_empty()
                {
                    let i = self.selected_disk_index();
                    let new_i = if i == 0 { data.disks.len() - 1 } else { i - 1 };
                    self.disk_table_state.select(Some(new_i));
                }
            }
            StorageViewMode::Partitions => {
                if let Some(data) = self.data()
                    && !data.volumes.is_empty()
                {
                    let i = self.selected_volume_index();
                    let new_i = if i == 0 {
                        data.volumes.len() - 1
                    } else {
                        i - 1
                    };
                    self.volume_table_state.select(Some(new_i));
                }
            }
        }
    }

    /// Move selection down
    fn select_next(&mut self) {
        match self.view_mode {
            StorageViewMode::Disks => {
                if let Some(data) = self.data()
                    && !data.disks.is_empty()
                {
                    let i = self.selected_disk_index();
                    let new_i = (i + 1) % data.disks.len();
                    self.disk_table_state.select(Some(new_i));
                }
            }
            StorageViewMode::Partitions => {
                if let Some(data) = self.data()
                    && !data.volumes.is_empty()
                {
                    let i = self.selected_volume_index();
                    let new_i = (i + 1) % data.volumes.len();
                    self.volume_table_state.select(Some(new_i));
                }
            }
        }
    }

    /// Draw the disks view
    fn draw_disks_view(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Min(5),    // Table
            Constraint::Length(5), // Detail section
        ])
        .split(area);

        // Draw disk table
        let header = Row::new(vec![
            Cell::from("DEVICE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("SIZE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("TYPE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("TRANSPORT").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("MODEL").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .height(1);

        let rows: Vec<Row> = if let Some(data) = self.data() {
            data.disks
                .iter()
                .map(|disk| {
                    let disk_type = if disk.cdrom {
                        "CD-ROM"
                    } else if disk.rotational {
                        "HDD"
                    } else {
                        "SSD"
                    };

                    let type_color = if disk.cdrom {
                        Color::Magenta
                    } else if disk.rotational {
                        Color::Yellow
                    } else {
                        Color::Green
                    };

                    Row::new(vec![
                        Cell::from(disk.dev_path.clone()),
                        Cell::from(disk.size_pretty.clone()),
                        Cell::from(disk_type).style(Style::default().fg(type_color)),
                        Cell::from(disk.transport.clone().unwrap_or_default()),
                        Cell::from(disk.model.clone().unwrap_or_default()),
                    ])
                })
                .collect()
        } else {
            vec![]
        };

        let widths = [
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Min(20),
        ];

        let row_count = rows.len();
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Disks ")
                    .title_style(Style::default().fg(Color::Cyan)),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(table, chunks[0], &mut self.disk_table_state);
        self.rows = crate::ui_ext::RowHits::new(
            chunks[0],
            true,
            1,
            self.disk_table_state.offset(),
            row_count,
        );

        // Draw detail section
        self.draw_disk_detail(frame, chunks[1]);
    }

    /// Draw disk detail section
    fn draw_disk_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Details ")
            .title_style(Style::default().fg(Color::Yellow));

        let content = if let Some(data) = self.data() {
            if let Some(disk) = data.disks.get(self.selected_disk_index()) {
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled("Device: ", Style::default().fg(Color::Gray)),
                        Span::raw(&disk.dev_path),
                        Span::raw("  "),
                        Span::styled("Size: ", Style::default().fg(Color::Gray)),
                        Span::raw(format!(
                            "{} ({})",
                            disk.size_pretty,
                            format_bytes(disk.size)
                        )),
                    ]),
                    Line::from(vec![
                        Span::styled("Serial: ", Style::default().fg(Color::Gray)),
                        Span::raw(disk.serial.clone().unwrap_or_else(|| "N/A".to_string())),
                        Span::raw("  "),
                        Span::styled("WWID: ", Style::default().fg(Color::Gray)),
                        Span::raw(
                            disk.wwid
                                .clone()
                                .map(|w| {
                                    if w.len() > 30 {
                                        format!("{}...", &w[..30])
                                    } else {
                                        w
                                    }
                                })
                                .unwrap_or_else(|| "N/A".to_string()),
                        ),
                    ]),
                ];

                if disk.readonly {
                    lines.push(Line::from(vec![Span::styled(
                        "  [READ-ONLY]",
                        Style::default().fg(Color::Red),
                    )]));
                }

                lines
            } else {
                vec![Line::from("No disk selected")]
            }
        } else {
            vec![Line::from("Loading...")]
        };

        let paragraph = Paragraph::new(content).block(block);
        frame.render_widget(paragraph, area);
    }

    /// Draw the volumes view
    fn draw_volumes_view(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Min(5),    // Table
            Constraint::Length(5), // Detail section
        ])
        .split(area);

        // Draw volume table
        let header = Row::new(vec![
            Cell::from("VOLUME").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("SIZE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("PHASE").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("FS").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("ENCRYPTION").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("MOUNT").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .height(1);

        let rows: Vec<Row> = if let Some(data) = self.data() {
            data.volumes
                .iter()
                .map(|vol| {
                    let phase_color = match vol.phase.as_str() {
                        "ready" => Color::Green,
                        "waiting" => Color::Yellow,
                        _ => Color::Red,
                    };

                    let encryption = vol
                        .encryption_provider
                        .clone()
                        .unwrap_or_else(|| "none".to_string());
                    let encryption_color = if encryption == "none" {
                        Color::Yellow
                    } else {
                        Color::Green
                    };

                    Row::new(vec![
                        Cell::from(vol.id.clone()),
                        Cell::from(vol.size.clone()),
                        Cell::from(vol.phase.clone()).style(Style::default().fg(phase_color)),
                        Cell::from(vol.filesystem.clone().unwrap_or_default()),
                        Cell::from(encryption).style(Style::default().fg(encryption_color)),
                        Cell::from(vol.mount_location.clone().unwrap_or_default()),
                    ])
                })
                .collect()
        } else {
            vec![]
        };

        let widths = [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Min(15),
        ];

        let row_count = rows.len();
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Partitions ")
                    .title_style(Style::default().fg(Color::Cyan)),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(table, chunks[0], &mut self.volume_table_state);
        self.rows = crate::ui_ext::RowHits::new(
            chunks[0],
            true,
            1,
            self.volume_table_state.offset(),
            row_count,
        );

        // Draw detail section
        self.draw_volume_detail(frame, chunks[1]);
    }

    /// Draw volume detail section
    fn draw_volume_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Details ")
            .title_style(Style::default().fg(Color::Yellow));

        let content = if let Some(data) = self.data() {
            if let Some(vol) = data.volumes.get(self.selected_volume_index()) {
                vec![
                    Line::from(vec![
                        Span::styled("Volume: ", Style::default().fg(Color::Gray)),
                        Span::raw(&vol.id),
                        Span::raw("  "),
                        Span::styled("Size: ", Style::default().fg(Color::Gray)),
                        Span::raw(&vol.size),
                    ]),
                    Line::from(vec![
                        Span::styled("Mount: ", Style::default().fg(Color::Gray)),
                        Span::raw(
                            vol.mount_location
                                .clone()
                                .unwrap_or_else(|| "N/A".to_string()),
                        ),
                        Span::raw("  "),
                        Span::styled("Filesystem: ", Style::default().fg(Color::Gray)),
                        Span::raw(vol.filesystem.clone().unwrap_or_else(|| "N/A".to_string())),
                    ]),
                    Line::from(vec![
                        Span::styled("Encryption: ", Style::default().fg(Color::Gray)),
                        Span::raw(
                            vol.encryption_provider
                                .clone()
                                .unwrap_or_else(|| "none".to_string()),
                        ),
                    ]),
                ]
            } else {
                vec![Line::from("No volume selected")]
            }
        } else {
            vec![Line::from("Loading...")]
        };

        let paragraph = Paragraph::new(content).block(block);
        frame.render_widget(paragraph, area);
    }
}

impl Component for StorageComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if let Some(i) = self.rows.clicked(&mouse) {
            match self.view_mode {
                StorageViewMode::Disks => self.disk_table_state.select(Some(i)),
                StorageViewMode::Partitions => self.volume_table_state.select(Some(i)),
            }
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Cyan));
        Some(Line::from(vec![
            key("[↑/↓]"),
            Span::raw(" navigate  "),
            key("[r]"),
            Span::raw(" refresh"),
        ]))
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(Some(Action::Back));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_prev();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
            }
            KeyCode::Char('r') => {
                return Ok(Some(Action::Refresh));
            }
            _ => {}
        }
        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Check for auto-refresh using AsyncState
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Check loading state
        if self.state.is_loading()
            && self
                .state
                .data()
                .is_none_or(|d| d.disks.is_empty() && d.volumes.is_empty())
        {
            let loading = Paragraph::new("Loading storage info...")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, area);
            return Ok(());
        }

        if let Some(err) = self.state.error() {
            let error =
                Paragraph::new(format!("Error: {}", err)).style(Style::default().fg(Color::Red));
            frame.render_widget(error, area);
            return Ok(());
        }

        // Disks and Partitions are separate menu entries: no tab bar, and
        // the window title names the node

        // Draw content based on view mode
        match self.view_mode {
            StorageViewMode::Disks => self.draw_disks_view(frame, area),
            StorageViewMode::Partitions => self.draw_volumes_view(frame, area),
        }

        Ok(())
    }
}

impl crate::components::BackgroundRefresh for StorageComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        self.state = refreshed.state;
    }
}
