//! Storage component - displays disk and volume information
//!
//! Shows physical disks and Talos volume status for a node.

use crate::action::Action;
use crate::components::Component;
use crate::ui_ext::HealthIndicatorExt;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use talos_rs::{DiskInfo, VolumeStatus, get_disks_for_node, get_volume_status_for_node};
use talos9s_core::{AsyncState, HealthIndicator, format_bytes};

const AUTO_REFRESH_INTERVAL: std::time::Duration =
    talos9s_core::constants::refresh_intervals::VERY_SLOW;

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
    /// Why the last disk / volume read failed; the lists keep what was last
    /// read, so a failure doesn't look like "no disks"
    pub disks_error: Option<String>,
    pub volumes_error: Option<String>,
}

/// Health of a Talos volume phase. Phases a volume passes through on the
/// way to ready are not failures; one this code doesn't know is unknown.
fn phase_health(phase: &str) -> HealthIndicator {
    match phase {
        "ready" => HealthIndicator::Healthy,
        "waiting" | "located" | "provisioned" | "prepared" => HealthIndicator::Pending,
        "missing" => HealthIndicator::Warning,
        "failed" => HealthIndicator::Error,
        // Closed on purpose (e.g. unmounted)
        "closed" => HealthIndicator::Info,
        _ => HealthIndicator::Unknown,
    }
}

/// What a virtual block device is (loop image, device mapper, software
/// RAID, DRBD replica, zvol, ...), or `None` for a real disk. These are
/// layered on the disks: hidden by default, and never called SSD or HDD
pub(crate) fn virtual_kind(dev_path: &str) -> Option<&'static str> {
    let name = dev_path.trim_start_matches("/dev/");
    [
        ("loop", "loop"),
        ("dm-", "dm"),
        ("md", "md"),
        ("drbd", "drbd"),
        ("zd", "zvol"),
        ("nbd", "nbd"),
        ("zram", "zram"),
        ("ram", "ram"),
    ]
    .into_iter()
    .find(|(prefix, _)| name.starts_with(prefix))
    .map(|(_, kind)| kind)
}

/// The selected row: a background, so cell colours (TYPE, HEALTH) stay
/// readable instead of turning into blocks under reverse video
fn selected_row() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

/// Table title, naming a failed read
fn table_title(name: &str, error: Option<&str>) -> Line<'static> {
    match error {
        None => Line::styled(format!(" {name} "), Style::default().fg(Color::Cyan)),
        Some(e) => Line::from(vec![
            Span::styled(format!(" {name} "), Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("· read failed: {e} "),
                Style::default().fg(Color::Red),
            ),
        ]),
    }
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

    /// Node address for talosctl commands
    node_address: Option<String>,

    /// Context name for authentication
    context: Option<String>,

    /// Config path for authentication
    config_path: Option<String>,

    /// Show virtual block devices too (`A`); saved in the state file
    show_all: bool,
}

impl Default for StorageComponent {
    fn default() -> Self {
        Self::new(String::new(), String::new(), None, None)
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

        let node_address = if address.is_empty() {
            None
        } else {
            // Extract IP from address (remove port if present)
            Some(talos_rs::host_of(&address).to_string())
        };

        let initial_data = StorageData {
            hostname,
            address,
            ..Default::default()
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
            node_address,
            context,
            config_path,
            show_all: false,
        }
    }

    /// The tab shown: disks or partitions
    pub fn view_mode(&self) -> StorageViewMode {
        self.view_mode
    }

    pub fn set_view_mode(&mut self, mode: StorageViewMode) {
        self.view_mode = mode;
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

        let config_path = self.config_path.as_deref();
        let (disks, volumes) = tokio::join!(
            get_disks_for_node(context, node, config_path),
            get_volume_status_for_node(context, node, config_path),
        );
        match disks {
            Ok(disks) => {
                data.disks = disks;
                data.disks_error = None;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch disks: {}", e);
                data.disks_error = Some(e.to_string());
            }
        }
        match volumes {
            Ok(volumes) => {
                data.volumes = volumes;
                data.volumes_error = None;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch volumes: {}", e);
                data.volumes_error = Some(e.to_string());
            }
        }

        // Store the data
        self.state.set_data(data);
        Ok(())
    }

    /// Show virtual block devices too
    pub fn show_all(&self) -> bool {
        self.show_all
    }

    pub fn set_show_all(&mut self, show_all: bool) {
        self.show_all = show_all;
    }

    /// The disks in the table: real ones first, virtual ones only with `A`
    fn visible_disks(&self) -> Vec<&DiskInfo> {
        self.data().map_or_else(Vec::new, |d| {
            d.disks
                .iter()
                .filter(|disk| self.show_all || virtual_kind(&disk.dev_path).is_none())
                .collect()
        })
    }

    /// Get selected disk index
    fn selected_disk_index(&self) -> usize {
        self.disk_table_state.selected().unwrap_or(0)
    }

    /// Get selected volume index
    fn selected_volume_index(&self) -> usize {
        self.volume_table_state.selected().unwrap_or(0)
    }

    /// Move the selection in the open table
    fn move_selection(&mut self, step: crate::ui_ext::Move) {
        // Disks counts the rows shown: virtual ones are hidden until `A`
        let len = match self.view_mode {
            StorageViewMode::Disks => self.visible_disks().len(),
            StorageViewMode::Partitions => self.data().map_or(0, |d| d.volumes.len()),
        };
        let table = match self.view_mode {
            StorageViewMode::Disks => &mut self.disk_table_state,
            StorageViewMode::Partitions => &mut self.volume_table_state,
        };
        if let Some(i) = crate::ui_ext::moved(table.selected().unwrap_or(0), len, step) {
            table.select(Some(i));
        }
    }

    /// Draw the disks view
    fn draw_disks_view(&mut self, frame: &mut Frame, area: Rect) {
        let [table_area, detail_area] = Layout::vertical([
            Constraint::Min(5),    // Table
            Constraint::Length(5), // Detail section
        ])
        .areas(area);

        let disks = self.visible_disks();
        let hidden = self
            .data()
            .map_or(0, |d| d.disks.len())
            .saturating_sub(disks.len());
        // RO only when a disk is read-only: on most nodes none is
        let show_ro = disks.iter().any(|d| d.readonly);
        let bold =
            |t: &'static str| Cell::from(t).style(Style::default().add_modifier(Modifier::BOLD));
        let mut header = vec![bold("DEVICE"), bold("SIZE"), bold("TYPE")];
        if show_ro {
            header.push(bold("RO"));
        }
        header.extend([bold("TRANSPORT"), bold("MODEL")]);
        let header = Row::new(header).height(1);

        let rows: Vec<Row> = disks
            .iter()
            .map(|disk| {
                let (disk_type, type_color) = if disk.cdrom {
                    ("CD-ROM", Color::Magenta)
                } else if let Some(kind) = virtual_kind(&disk.dev_path) {
                    (kind, Color::DarkGray)
                } else if disk.rotational {
                    ("HDD", Color::Yellow)
                } else {
                    ("SSD", Color::Green)
                };
                let mut cells = vec![
                    Cell::from(disk.dev_path.clone()),
                    Cell::from(disk.size_pretty.clone()),
                    Cell::from(disk_type).style(Style::default().fg(type_color)),
                ];
                if show_ro {
                    cells.push(if disk.readonly {
                        Cell::from("RO").style(Style::default().fg(Color::Red))
                    } else {
                        Cell::from("")
                    });
                }
                cells.extend([
                    Cell::from(disk.transport.clone().unwrap_or_default()),
                    Cell::from(disk.model.clone().unwrap_or_default()),
                ]);
                Row::new(cells)
            })
            .collect();

        let mut widths = vec![
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(8),
        ];
        if show_ro {
            widths.push(Constraint::Length(4));
        }
        widths.extend([Constraint::Length(12), Constraint::Min(20)]);
        let title = if hidden > 0 {
            format!("Disks · {hidden} virtual hidden")
        } else {
            "Disks".to_string()
        };

        let row_count = rows.len();
        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(table_title(
                &title,
                self.data().and_then(|d| d.disks_error.as_deref()),
            )))
            .row_highlight_style(selected_row());

        frame.render_stateful_widget(table, table_area, &mut self.disk_table_state);
        self.rows = crate::ui_ext::RowHits::new(
            table_area,
            true,
            1,
            self.disk_table_state.offset(),
            row_count,
        );

        // Draw detail section
        self.draw_disk_detail(frame, detail_area);
    }

    /// Draw disk detail section
    fn draw_disk_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Details ")
            .title_style(Style::default().fg(Color::Yellow));

        let content = if self.data().is_some() {
            if let Some(disk) = self
                .visible_disks()
                .get(self.selected_disk_index())
                .copied()
            {
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
                                .map(|w| talos9s_core::truncate_string(&w, 33))
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
        let [table_area, detail_area] = Layout::vertical([
            Constraint::Min(5),    // Table
            Constraint::Length(5), // Detail section
        ])
        .areas(area);

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
                    let phase_color = phase_health(&vol.phase).color();

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
            .block(Block::default().borders(Borders::ALL).title(table_title(
                "Partitions",
                self.data().and_then(|d| d.volumes_error.as_deref()),
            )))
            .row_highlight_style(selected_row());

        frame.render_stateful_widget(table, table_area, &mut self.volume_table_state);
        self.rows = crate::ui_ext::RowHits::new(
            table_area,
            true,
            1,
            self.volume_table_state.offset(),
            row_count,
        );

        // Draw detail section
        self.draw_volume_detail(frame, detail_area);
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
        let key = |k: &'static str| Span::styled(k, crate::ui_ext::KEY);
        let mut spans = vec![
            key("<↑/↓>"),
            Span::raw(" navigate  "),
            key("<r>"),
            Span::raw(" refresh"),
        ];
        if self.view_mode == StorageViewMode::Disks {
            spans.extend([
                Span::raw("  "),
                key("<A>"),
                Span::raw(if self.show_all {
                    " hide virtual"
                } else {
                    " show all"
                }),
            ]);
        }
        Some(Line::from(spans))
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(Some(Action::Back));
            }
            _ if let Some(step) = crate::ui_ext::Move::from_key(&key) => {
                self.move_selection(step);
            }
            KeyCode::Char('r') => {
                return Ok(Some(Action::Refresh));
            }
            KeyCode::Char('A') if self.view_mode == StorageViewMode::Disks => {
                self.show_all = !self.show_all;
                self.disk_table_state.select(Some(0));
            }
            _ => {}
        }
        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Check for auto-refresh using AsyncState
            if self.state.should_auto_refresh(true, AUTO_REFRESH_INTERVAL) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Check loading state
        if crate::ui_ext::draw_load_gate(
            frame,
            area,
            self.state.is_loading()
                && self
                    .state
                    .data()
                    .is_none_or(|d| d.disks.is_empty() && d.volumes.is_empty()),
            self.state.error(),
            "Loading storage info...",
        ) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn disk(path: &str, readonly: bool) -> DiskInfo {
        DiskInfo {
            id: path.trim_start_matches("/dev/").to_string(),
            dev_path: path.to_string(),
            size: 0,
            size_pretty: String::new(),
            model: None,
            serial: None,
            transport: None,
            rotational: false,
            readonly,
            cdrom: false,
            wwid: None,
            bus_path: None,
        }
    }

    fn with_disks(disks: Vec<DiskInfo>) -> StorageComponent {
        let mut c = StorageComponent::default();
        c.state.set_data(StorageData {
            hostname: "khn1".into(),
            address: "10.0.0.1".into(),
            disks,
            volumes: Vec::new(),
            disks_error: None,
            volumes_error: None,
        });
        c
    }

    fn screen(c: &mut StorageComponent) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| c.draw(f, f.area()).unwrap()).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn virtual_devices_are_named_not_called_ssd() {
        assert_eq!(virtual_kind("/dev/loop0"), Some("loop"));
        assert_eq!(virtual_kind("/dev/dm-0"), Some("dm"));
        assert_eq!(virtual_kind("/dev/zd16"), Some("zvol"));
        assert_eq!(virtual_kind("/dev/zram0"), Some("zram"));
        assert_eq!(virtual_kind("/dev/drbd1002"), Some("drbd"));
        for real in [
            "/dev/sda",
            "/dev/nvme0n1",
            "/dev/vda",
            "/dev/sr0",
            "/dev/mmcblk0",
        ] {
            assert_eq!(virtual_kind(real), None, "{real}");
        }
    }

    /// Virtual devices hide until `A`; RO shows as a column only when a
    /// shown disk is read-only
    #[test]
    fn virtual_disks_hide_until_a_and_read_only_gets_a_column() {
        let mut c = with_disks(vec![
            disk("/dev/loop0", true),
            disk("/dev/sda", false),
            disk("/dev/dm-0", false),
        ]);
        let text = screen(&mut c);
        assert!(text.contains("/dev/sda"), "{text}");
        assert!(!text.contains("/dev/loop0"), "{text}");
        assert!(text.contains("2 virtual hidden"), "{text}");
        assert!(!text.contains(" RO "), "no read-only disk shown: {text}");

        c.handle_key_event(KeyEvent::new(
            KeyCode::Char('A'),
            crossterm::event::KeyModifiers::NONE,
        ))
        .unwrap();
        assert!(c.show_all());
        let text = screen(&mut c);
        assert!(
            text.contains("/dev/loop0") && text.contains("loop"),
            "{text}"
        );
        assert!(text.contains("RO"), "{text}");
        assert!(!text.contains("virtual hidden"), "{text}");
    }

    #[test]
    fn volume_phases_on_the_way_to_ready_are_not_failures() {
        assert_eq!(phase_health("ready"), HealthIndicator::Healthy);
        for phase in ["waiting", "located", "provisioned", "prepared"] {
            assert_eq!(phase_health(phase), HealthIndicator::Pending, "{phase}");
        }
        assert_eq!(phase_health("closed"), HealthIndicator::Info);
        assert_eq!(phase_health("failed"), HealthIndicator::Error);
        assert_eq!(phase_health("something-new"), HealthIndicator::Unknown);
    }

    #[test]
    fn failed_read_is_named_in_the_title() {
        let title = table_title("Disks", Some("timed out after 10s"));
        let text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " Disks · read failed: timed out after 10s ");
    }
}
