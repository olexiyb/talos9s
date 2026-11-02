//! Bootstrap Wizard - State machine driven bootstrap flow
//!
//! Guides users through the complete Talos node bootstrap process:
//! 1. Connect to maintenance mode node
//! 2. Select installation disk
//! 3. Configure cluster settings
//! 4. Generate and apply config
//! 5. Wait for reboot
//! 6. Bootstrap cluster
//! 7. Transition to secure mode

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::time::Instant;
use talos_rs::{DiskInfo, GenConfigResult, VolumeStatus};

/// Wizard states
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardState {
    /// Initial connection to maintenance mode node
    Connecting,
    /// User selects which disk to install Talos onto
    SelectDisk,
    /// User configures cluster settings
    ConfigureCluster,
    /// Config has been generated, ready to apply
    ConfigReady,
    /// Applying configuration to node
    Applying,
    /// Waiting for node to reboot after install
    WaitingReboot,
    /// Node is up, ready for bootstrap
    ReadyToBootstrap,
    /// Running bootstrap command
    Bootstrapping,
    /// Waiting for cluster to become healthy
    WaitingHealthy,
    /// Bootstrap complete, ready to transition
    Complete,
    /// Error state with message
    Error(String),
}

impl WizardState {
    /// Get the step number (1-based) for display
    pub fn step_number(&self) -> usize {
        match self {
            WizardState::Connecting => 1,
            WizardState::SelectDisk => 2,
            WizardState::ConfigureCluster => 3,
            WizardState::ConfigReady => 4,
            WizardState::Applying => 5,
            WizardState::WaitingReboot => 6,
            WizardState::ReadyToBootstrap => 7,
            WizardState::Bootstrapping => 8,
            WizardState::WaitingHealthy => 9,
            WizardState::Complete => 10,
            WizardState::Error(_) => 0,
        }
    }

    /// Get the total number of steps
    pub fn total_steps() -> usize {
        10
    }

    /// Get the step title for display
    pub fn title(&self) -> &'static str {
        match self {
            WizardState::Connecting => "Connecting",
            WizardState::SelectDisk => "Select Installation Disk",
            WizardState::ConfigureCluster => "Configure Cluster",
            WizardState::ConfigReady => "Review Configuration",
            WizardState::Applying => "Applying Configuration",
            WizardState::WaitingReboot => "Waiting for Reboot",
            WizardState::ReadyToBootstrap => "Ready to Bootstrap",
            WizardState::Bootstrapping => "Bootstrapping",
            WizardState::WaitingHealthy => "Waiting for Cluster",
            WizardState::Complete => "Complete",
            WizardState::Error(_) => "Error",
        }
    }
}

/// Node type for configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeType {
    #[default]
    Controlplane,
    Worker,
}

impl NodeType {
    pub fn toggle(&self) -> Self {
        match self {
            NodeType::Controlplane => NodeType::Worker,
            NodeType::Worker => NodeType::Controlplane,
        }
    }
}

/// Data accumulated through the wizard flow
#[derive(Debug, Clone, Default)]
pub struct WizardData {
    // Connection info
    pub endpoint: String,

    // From Connecting state
    pub disks: Vec<DiskInfo>,
    pub volumes: Vec<VolumeStatus>,
    pub connected: bool,

    // From SelectDisk state
    pub selected_disk: Option<DiskInfo>,

    // From ConfigureCluster state
    pub cluster_name: String,
    pub k8s_endpoint: String,
    pub node_type: NodeType,
    pub output_dir: String,

    // From ConfigReady state (after generation)
    pub config_result: Option<GenConfigResult>,
    pub context_name: Option<String>,

    // Timing for wait states
    pub wait_started: Option<Instant>,

    // Polling tracking
    pub poll_attempts: u32,
    pub last_poll_error: Option<String>,

    // Spinner for animations
    pub spinner_frame: usize,

    // Error tracking
    pub last_error: Option<String>,
}

/// Spinner frames for wait states
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Format error messages with better descriptions for common issues
fn format_poll_error(error: &str) -> String {
    if error.contains("certificate signed by unknown authority")
        || error.contains("x509:")
        || error.contains("tls:")
    {
        "Certificate mismatch - remove old config and regenerate".to_string()
    } else if error.contains("connection refused") {
        "Connection refused - node may not be running".to_string()
    } else if error.contains("connection error") || error.contains("Unavailable") {
        "Node not reachable - waiting for boot...".to_string()
    } else if error.contains("deadline exceeded") || error.contains("timeout") {
        "Connection timeout - node may still be booting".to_string()
    } else {
        // Truncate long error messages
        if error.chars().count() > 80 {
            talos9s_core::truncate_string(error, 80)
        } else {
            error.to_string()
        }
    }
}

impl WizardData {
    pub fn new(endpoint: String) -> Self {
        let k8s_endpoint = format!("https://{endpoint}:6443");
        Self {
            endpoint,
            cluster_name: "talos-cluster".to_string(),
            k8s_endpoint,
            node_type: NodeType::Controlplane,
            output_dir: ".".to_string(),
            ..Default::default()
        }
    }

    /// Get installable disks (filter out read-only, CD-ROM)
    pub fn installable_disks(&self) -> Vec<&DiskInfo> {
        self.disks
            .iter()
            .filter(|d| !d.readonly && !d.cdrom)
            .collect()
    }

    /// Get current spinner character
    pub fn spinner(&self) -> &'static str {
        SPINNER_FRAMES
            .get(self.spinner_frame % SPINNER_FRAMES.len())
            .copied()
            .unwrap_or_default()
    }

    /// Advance spinner to next frame
    pub fn advance_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
    }
}

/// Active field in ConfigureCluster dialog
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigField {
    #[default]
    ClusterName,
    K8sEndpoint,
    NodeType,
    OutputDir,
}

impl ConfigField {
    pub fn next(&self) -> Self {
        match self {
            ConfigField::ClusterName => ConfigField::K8sEndpoint,
            ConfigField::K8sEndpoint => ConfigField::NodeType,
            ConfigField::NodeType => ConfigField::OutputDir,
            ConfigField::OutputDir => ConfigField::ClusterName,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            ConfigField::ClusterName => ConfigField::OutputDir,
            ConfigField::K8sEndpoint => ConfigField::ClusterName,
            ConfigField::NodeType => ConfigField::K8sEndpoint,
            ConfigField::OutputDir => ConfigField::NodeType,
        }
    }
}

/// Bootstrap wizard component
pub struct WizardComponent {
    /// Current state
    state: WizardState,

    /// Accumulated data
    data: WizardData,

    /// Table state for disk selection
    disk_table_state: TableState,

    /// Active field in config dialog
    active_field: ConfigField,

    /// Whether viewing volumes instead of disks
    viewing_volumes: bool,

    /// A step that changes the node, waiting for `y`
    confirming: Option<WizardConfirm>,
}

/// Steps that change the node and need an explicit `y`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardConfirm {
    /// Install Talos to the selected disk (erases it)
    Apply,
    /// Bootstrap etcd (once per cluster)
    Bootstrap,
}

impl WizardComponent {
    pub fn new(endpoint: String) -> Self {
        let mut disk_table_state = TableState::default();
        disk_table_state.select(Some(0));

        Self {
            state: WizardState::Connecting,
            data: WizardData::new(endpoint),
            disk_table_state,
            active_field: ConfigField::default(),
            viewing_volumes: false,
            confirming: None,
        }
    }

    /// Get current state
    pub fn state(&self) -> &WizardState {
        &self.state
    }

    /// Get wizard data
    pub fn data(&self) -> &WizardData {
        &self.data
    }

    /// Get mutable wizard data
    pub fn data_mut(&mut self) -> &mut WizardData {
        &mut self.data
    }

    /// Transition to a new state
    pub fn transition(&mut self, new_state: WizardState) {
        tracing::info!("Wizard: {:?} -> {:?}", self.state, new_state);
        self.state = new_state;
    }

    /// Set error state
    pub fn set_error(&mut self, message: String) {
        self.data.last_error = Some(message.clone());
        self.state = WizardState::Error(message);
    }

    /// Connect to the maintenance mode node and fetch disk info
    pub async fn connect(&mut self) -> Result<()> {
        use talos_rs::{get_disks_insecure, get_volume_status_insecure};

        let endpoint = &self.data.endpoint;

        // Fetch disks
        match get_disks_insecure(endpoint).await {
            Ok(disks) => {
                self.data.disks = disks;
                self.data.connected = true;
            }
            Err(e) => {
                self.set_error(format!("Failed to connect: {e}"));
                return Ok(());
            }
        }

        // Fetch volumes (optional, don't fail if unavailable)
        if let Ok(volumes) = get_volume_status_insecure(endpoint).await {
            self.data.volumes = volumes;
        }

        // Transition to disk selection
        self.transition(WizardState::SelectDisk);
        Ok(())
    }

    /// Get selected disk index
    fn selected_disk_index(&self) -> usize {
        self.disk_table_state.selected().unwrap_or(0)
    }

    /// Move disk selection up
    fn select_prev_disk(&mut self) {
        let disks = self.data.installable_disks();
        if !disks.is_empty() {
            let i = self.selected_disk_index();
            let new_i = if i == 0 { disks.len() - 1 } else { i - 1 };
            self.disk_table_state.select(Some(new_i));
        }
    }

    /// Move disk selection down
    fn select_next_disk(&mut self) {
        let disks = self.data.installable_disks();
        if !disks.is_empty() {
            let i = self.selected_disk_index();
            let new_i = (i + 1) % disks.len();
            self.disk_table_state.select(Some(new_i));
        }
    }

    /// Confirm disk selection and move to configure
    fn confirm_disk_selection(&mut self) {
        let disks = self.data.installable_disks();
        let idx = self.selected_disk_index();
        if let Some(&disk) = disks.get(idx) {
            self.data.selected_disk = Some(disk.clone());
            self.transition(WizardState::ConfigureCluster);
        }
    }

    /// Handle key events for SelectDisk state
    fn handle_select_disk_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_prev_disk();
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next_disk();
                None
            }
            KeyCode::Enter => {
                self.confirm_disk_selection();
                None
            }
            KeyCode::Tab => {
                self.viewing_volumes = !self.viewing_volumes;
                None
            }
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            _ => None,
        }
    }

    /// Handle key events for ConfigureCluster state
    fn handle_configure_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.active_field = self.active_field.next();
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.active_field = self.active_field.prev();
                None
            }
            KeyCode::Enter => {
                // Generate config
                Some(Action::WizardGenConfig)
            }
            KeyCode::Esc => {
                // Go back to disk selection
                self.transition(WizardState::SelectDisk);
                None
            }
            KeyCode::Char(c) => {
                match self.active_field {
                    ConfigField::ClusterName => self.data.cluster_name.push(c),
                    ConfigField::K8sEndpoint => self.data.k8s_endpoint.push(c),
                    ConfigField::NodeType => {
                        // Space or any char toggles
                        self.data.node_type = self.data.node_type.toggle();
                    }
                    ConfigField::OutputDir => self.data.output_dir.push(c),
                }
                None
            }
            KeyCode::Backspace => {
                match self.active_field {
                    ConfigField::ClusterName => {
                        self.data.cluster_name.pop();
                    }
                    ConfigField::K8sEndpoint => {
                        self.data.k8s_endpoint.pop();
                    }
                    ConfigField::NodeType => {}
                    ConfigField::OutputDir => {
                        self.data.output_dir.pop();
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Handle key events for ConfigReady state
    fn handle_config_ready_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('a') | KeyCode::Enter => {
                self.confirming = Some(WizardConfirm::Apply);
                None
            }
            KeyCode::Esc => {
                // Go back to configure
                self.transition(WizardState::ConfigureCluster);
                None
            }
            KeyCode::Char('q') => Some(Action::Quit),
            _ => None,
        }
    }

    /// Handle key events for ReadyToBootstrap state
    fn handle_ready_bootstrap_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('b') | KeyCode::Enter => {
                self.confirming = Some(WizardConfirm::Bootstrap);
                None
            }
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            _ => None,
        }
    }

    /// Handle key events for Complete state
    fn handle_complete_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Enter => {
                // Exit to secure mode
                Some(Action::WizardComplete(self.data.context_name.clone()))
            }
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            _ => None,
        }
    }

    /// Handle key events for Error state
    fn handle_error_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('r') => {
                // Retry - go back to connecting
                self.transition(WizardState::Connecting);
                Some(Action::WizardRetry)
            }
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            _ => None,
        }
    }

    /// Handle key events for waiting states
    fn handle_waiting_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            _ => None,
        }
    }

    // ============ DRAWING ============

    /// Draw the wizard header with step indicator
    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let step = self.state.step_number();
        let total = WizardState::total_steps();
        let title = self.state.title();

        let header = if step > 0 {
            Line::from(vec![
                Span::styled(
                    " Bootstrap Wizard ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("Step {step} of {total}: "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(title, Style::default().fg(Color::White)),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " Bootstrap Wizard ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(title, Style::default().fg(Color::Red)),
            ])
        };

        frame.render_widget(Paragraph::new(header), area);
    }

    /// Draw connecting state
    fn draw_connecting(&self, frame: &mut Frame, area: Rect) {
        let spinner = self.data.spinner();
        let content = Paragraph::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(format!("  {spinner} "), Style::default().fg(Color::Cyan)),
                Span::raw("Connecting to "),
                Span::styled(&self.data.endpoint, Style::default().fg(Color::Cyan)),
                Span::raw("..."),
            ]),
            Line::raw(""),
            Line::styled(
                "  Please wait while we detect available disks.",
                Style::default().fg(Color::DarkGray),
            ),
        ])
        .block(Block::default().borders(Borders::ALL));

        frame.render_widget(content, area);
    }

    /// Draw disk selection state
    fn draw_select_disk(&mut self, frame: &mut Frame, area: Rect) {
        let [
            instructions_area,
            table_area,
            details_area,
            warning_area,
            help_area,
        ] = Layout::vertical([
            Constraint::Length(2), // Instructions
            Constraint::Min(8),    // Disk table
            Constraint::Length(8), // Disk details
            Constraint::Length(2), // Warning
            Constraint::Length(1), // Help
        ])
        .areas(area);

        // Instructions
        let instructions = Paragraph::new(Line::from(vec![Span::raw(
            "  Select the disk where Talos will be installed:",
        )]))
        .style(Style::default().fg(Color::White));
        frame.render_widget(instructions, instructions_area);

        // Disk table or volume table
        if self.viewing_volumes {
            self.draw_volume_table(frame, table_area);
        } else {
            self.draw_disk_table(frame, table_area);
        }

        // Disk details
        self.draw_disk_details(frame, details_area);

        // Warning
        let warning = Paragraph::new(Line::from(vec![
            Span::styled(
                "  ⚠ WARNING: ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Selected disk will be COMPLETELY ERASED",
                Style::default().fg(Color::Yellow),
            ),
        ]));
        frame.render_widget(warning, warning_area);

        // Help
        let help = Line::from(vec![
            Span::styled(" [↑↓] ", Style::default().fg(Color::Cyan)),
            Span::raw("Navigate"),
            Span::raw("  "),
            Span::styled(" [Enter] ", Style::default().fg(Color::Green)),
            Span::raw("Select"),
            Span::raw("  "),
            Span::styled(" [Tab] ", Style::default().fg(Color::Cyan)),
            Span::raw(if self.viewing_volumes {
                "View Disks"
            } else {
                "View Volumes"
            }),
            Span::raw("  "),
            Span::styled(" [q] ", Style::default().fg(Color::Cyan)),
            Span::raw("Quit"),
        ]);
        frame.render_widget(Paragraph::new(help), help_area);
    }

    /// Draw disk table
    fn draw_disk_table(&mut self, frame: &mut Frame, area: Rect) {
        let disks = self.data.installable_disks();

        let header = Row::new(vec![
            Cell::from("Device").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Size").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Type").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Transport").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Model").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .height(1)
        .style(Style::default().fg(Color::Cyan));

        let rows: Vec<Row> = disks
            .iter()
            .map(|disk| {
                let disk_type = if disk.rotational {
                    ("HDD", Color::Yellow)
                } else {
                    ("SSD", Color::Green)
                };

                Row::new(vec![
                    Cell::from(disk.dev_path.clone()),
                    Cell::from(disk.size_pretty.clone()),
                    Cell::from(disk_type.0).style(Style::default().fg(disk_type.1)),
                    Cell::from(disk.transport.clone().unwrap_or_default()),
                    Cell::from(disk.model.clone().unwrap_or_else(|| "-".to_string())),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(15),
                Constraint::Length(12),
                Constraint::Length(6),
                Constraint::Length(10),
                Constraint::Fill(1),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Disks ({}) ", disks.len())),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

        frame.render_stateful_widget(table, area, &mut self.disk_table_state);
    }

    /// Draw volume table
    fn draw_volume_table(&self, frame: &mut Frame, area: Rect) {
        let volumes = &self.data.volumes;

        let header = Row::new(vec![
            Cell::from("Volume").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Size").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Phase").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("Location").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .height(1)
        .style(Style::default().fg(Color::Cyan));

        let rows: Vec<Row> = volumes
            .iter()
            .map(|vol| {
                let phase_color = match vol.phase.as_str() {
                    "ready" => Color::Green,
                    "waiting" => Color::Yellow,
                    _ => Color::Red,
                };

                Row::new(vec![
                    Cell::from(vol.id.clone()),
                    Cell::from(vol.size.clone()),
                    Cell::from(vol.phase.clone()).style(Style::default().fg(phase_color)),
                    Cell::from(vol.mount_location.clone().unwrap_or_default()),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(20),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Fill(1),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Volumes ({}) - Reference Only ", volumes.len())),
        );

        frame.render_widget(table, area);
    }

    /// Draw disk details panel
    fn draw_disk_details(&self, frame: &mut Frame, area: Rect) {
        let disks = self.data.installable_disks();
        let idx = self.selected_disk_index();

        let content = if let Some(&disk) = disks.get(idx) {
            vec![
                Line::from(vec![
                    Span::styled("  Device:    ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&disk.dev_path, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Size:      ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&disk.size_pretty, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Type:      ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        if disk.rotational {
                            "HDD (rotational)"
                        } else {
                            "SSD (non-rotational)"
                        },
                        Style::default().fg(if disk.rotational {
                            Color::Yellow
                        } else {
                            Color::Green
                        }),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Transport: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        disk.transport.as_deref().unwrap_or("-"),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Model:     ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        disk.model.as_deref().unwrap_or("-"),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Serial:    ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        disk.serial.as_deref().unwrap_or("-"),
                        Style::default().fg(Color::White),
                    ),
                ]),
            ]
        } else {
            vec![Line::styled(
                "  No disk selected",
                Style::default().fg(Color::DarkGray),
            )]
        };

        let details = Paragraph::new(content).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Disk Details ")
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        frame.render_widget(details, area);
    }

    /// Draw configure cluster state
    fn draw_configure_cluster(&self, frame: &mut Frame, area: Rect) {
        let [
            disk_area,
            cluster_area,
            endpoint_area,
            type_area,
            dir_area,
            _spacer,
            help_area,
        ] = Layout::vertical([
            Constraint::Length(3), // Selected disk
            Constraint::Length(2), // Cluster name
            Constraint::Length(2), // K8s endpoint
            Constraint::Length(2), // Node type
            Constraint::Length(2), // Output dir
            Constraint::Fill(1),   // Spacer
            Constraint::Length(1), // Help
        ])
        .areas(area);

        // Selected disk (read-only)
        let disk_info = self
            .data
            .selected_disk
            .as_ref()
            .map(|d| {
                format!(
                    "{} ({} {})",
                    d.dev_path,
                    d.size_pretty,
                    if d.rotational { "HDD" } else { "SSD" }
                )
            })
            .unwrap_or_else(|| "None".to_string());

        let disk_line = Paragraph::new(Line::from(vec![
            Span::styled("  Install Disk:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(&disk_info, Style::default().fg(Color::Green)),
            Span::styled(" ✓", Style::default().fg(Color::Green)),
        ]));
        frame.render_widget(disk_line, disk_area);

        // Cluster name field
        let cluster_style = if self.active_field == ConfigField::ClusterName {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        let cluster_field = Paragraph::new(Line::from(vec![
            Span::styled("  Cluster Name:  ", Style::default().fg(Color::Cyan)),
            Span::styled(&self.data.cluster_name, cluster_style),
            if self.active_field == ConfigField::ClusterName {
                Span::styled("_", Style::default().fg(Color::Yellow))
            } else {
                Span::raw("")
            },
        ]));
        frame.render_widget(cluster_field, cluster_area);

        // K8s endpoint field
        let endpoint_style = if self.active_field == ConfigField::K8sEndpoint {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        let endpoint_field = Paragraph::new(Line::from(vec![
            Span::styled("  K8s Endpoint:  ", Style::default().fg(Color::Cyan)),
            Span::styled(&self.data.k8s_endpoint, endpoint_style),
            if self.active_field == ConfigField::K8sEndpoint {
                Span::styled("_", Style::default().fg(Color::Yellow))
            } else {
                Span::raw("")
            },
        ]));
        frame.render_widget(endpoint_field, endpoint_area);

        // Node type selector
        let type_line = Paragraph::new(Line::from(vec![
            Span::styled("  Node Type:     ", Style::default().fg(Color::Cyan)),
            if self.data.node_type == NodeType::Controlplane {
                Span::styled(
                    " Control Plane ",
                    Style::default().fg(Color::Black).bg(Color::Green),
                )
            } else {
                Span::styled(" Control Plane ", Style::default().fg(Color::DarkGray))
            },
            Span::raw(" "),
            if self.data.node_type == NodeType::Worker {
                Span::styled(
                    " Worker ",
                    Style::default().fg(Color::Black).bg(Color::Blue),
                )
            } else {
                Span::styled(" Worker ", Style::default().fg(Color::DarkGray))
            },
            if self.active_field == ConfigField::NodeType {
                Span::styled("  ← Space to toggle", Style::default().fg(Color::Yellow))
            } else {
                Span::raw("")
            },
        ]));
        frame.render_widget(type_line, type_area);

        // Output dir field
        let dir_style = if self.active_field == ConfigField::OutputDir {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        let dir_field = Paragraph::new(Line::from(vec![
            Span::styled("  Output Dir:    ", Style::default().fg(Color::Cyan)),
            Span::styled(&self.data.output_dir, dir_style),
            if self.active_field == ConfigField::OutputDir {
                Span::styled("_", Style::default().fg(Color::Yellow))
            } else {
                Span::raw("")
            },
        ]));
        frame.render_widget(dir_field, dir_area);

        // Help
        let help = Line::from(vec![
            Span::styled(" [Tab] ", Style::default().fg(Color::Cyan)),
            Span::raw("Next field"),
            Span::raw("  "),
            Span::styled(" [Enter] ", Style::default().fg(Color::Green)),
            Span::raw("Generate config"),
            Span::raw("  "),
            Span::styled(" [Esc] ", Style::default().fg(Color::Cyan)),
            Span::raw("Back"),
        ]);
        frame.render_widget(Paragraph::new(help), help_area);
    }

    /// Draw config ready state
    fn draw_config_ready(&self, frame: &mut Frame, area: Rect) {
        let config = self.data.config_result.as_ref();

        let content = if let Some(cfg) = config {
            vec![
                Line::raw(""),
                Line::styled(
                    "  Configuration generated successfully!",
                    Style::default().fg(Color::Green),
                ),
                Line::raw(""),
                Line::from(vec![Span::styled(
                    "  Files created:",
                    Style::default().fg(Color::DarkGray),
                )]),
                Line::from(vec![
                    Span::raw("    • "),
                    Span::styled(&cfg.controlplane_path, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::raw("    • "),
                    Span::styled(&cfg.worker_path, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::raw("    • "),
                    Span::styled(&cfg.talosconfig_path, Style::default().fg(Color::White)),
                ]),
                Line::raw(""),
                if let Some(ctx) = &self.data.context_name {
                    Line::from(vec![
                        Span::styled("  Context merged: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(ctx, Style::default().fg(Color::Cyan)),
                    ])
                } else {
                    Line::raw("")
                },
                Line::raw(""),
                Line::styled(
                    "  Press [a] or [Enter] to apply configuration to the node.",
                    Style::default().fg(Color::Yellow),
                ),
                Line::raw(""),
                Line::styled(
                    "  ⚠ This will install Talos to the selected disk and reboot.",
                    Style::default().fg(Color::Yellow),
                ),
            ]
        } else {
            vec![Line::styled(
                "  No configuration generated",
                Style::default().fg(Color::Red),
            )]
        };

        let para = Paragraph::new(content).block(Block::default().borders(Borders::ALL));
        frame.render_widget(para, area);
    }

    /// Draw applying state
    fn draw_applying(&self, frame: &mut Frame, area: Rect) {
        let spinner = self.data.spinner();
        let content = Paragraph::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(format!("  {spinner} "), Style::default().fg(Color::Cyan)),
                Span::styled(
                    "Applying configuration...",
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::raw(""),
            Line::styled(
                "  The node will install Talos to disk and reboot.",
                Style::default().fg(Color::DarkGray),
            ),
        ])
        .block(Block::default().borders(Borders::ALL));

        frame.render_widget(content, area);
    }

    /// Draw waiting for reboot state
    fn draw_waiting_reboot(&self, frame: &mut Frame, area: Rect) {
        let elapsed = self
            .data
            .wait_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);

        let spinner = self.data.spinner();
        let mut lines = vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(format!("  {spinner} "), Style::default().fg(Color::Cyan)),
                Span::styled(
                    "Waiting for node to come back online...",
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("     Endpoint: ", Style::default().fg(Color::DarkGray)),
                Span::styled(&self.data.endpoint, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("     Context:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.data.context_name.as_deref().unwrap_or("-"),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
            Line::from(vec![
                Span::styled("     Elapsed:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{elapsed} seconds"),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("     Attempts: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}", self.data.poll_attempts),
                    Style::default().fg(Color::White),
                ),
            ]),
        ];

        // Show last poll error if any
        if let Some(err) = &self.data.last_poll_error {
            let formatted = format_poll_error(err);
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("     Status:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(formatted, Style::default().fg(Color::Red)),
            ]));
        }

        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  The node will install Talos to disk and shut down.",
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::styled(
            "  Power on the node to boot from the installed disk.",
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::styled(
            "  This screen will automatically advance when the node responds.",
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(" [q] ", Style::default().fg(Color::Cyan)),
            Span::raw("Quit"),
        ]));

        let content = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .wrap(ratatui::widgets::Wrap { trim: false });

        frame.render_widget(content, area);
    }

    /// Draw ready to bootstrap state
    fn draw_confirm(&self, frame: &mut Frame, area: Rect, confirm: WizardConfirm) {
        let endpoint = self.data.endpoint.as_str();
        let (title, lines) = match confirm {
            WizardConfirm::Apply => {
                let disk = self
                    .data
                    .selected_disk
                    .as_ref()
                    .map_or("the selected disk".to_string(), |d| {
                        format!("{} ({})", d.dev_path, d.size_pretty)
                    });
                (
                    " Install Talos ",
                    vec![
                        Line::raw(format!("Install Talos on {endpoint} to {disk}?")),
                        Line::styled(
                            "Everything on that disk will be erased.",
                            Style::default().fg(Color::Red),
                        ),
                    ],
                )
            }
            WizardConfirm::Bootstrap => (
                " Bootstrap ",
                vec![
                    Line::raw(format!("Bootstrap etcd on {endpoint}?")),
                    Line::styled(
                        "Run this once per cluster, on one control plane node.",
                        Style::default().fg(Color::Yellow),
                    ),
                ],
            ),
        };
        let mut lines = lines;
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(" [y] ", Style::default().fg(Color::Green)),
            Span::raw("Yes  "),
            Span::styled(" [n/Esc] ", Style::default().fg(Color::Cyan)),
            Span::raw("Cancel"),
        ]));
        let width = lines
            .iter()
            .map(|l| talos9s_core::convert::to_u16(l.width()).saturating_add(4))
            .max()
            .unwrap_or(40)
            .min(area.width);
        let height = talos9s_core::convert::to_u16(lines.len())
            .saturating_add(2)
            .min(area.height);
        let dialog = crate::ui_ext::centered(area, width, height);
        frame.render_widget(ratatui::widgets::Clear, dialog);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            dialog,
        );
    }

    fn draw_ready_bootstrap(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::raw(""),
            Line::styled(
                "  ✓ Node is up and configured!",
                Style::default().fg(Color::Green),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled("     Context: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.data.context_name.as_deref().unwrap_or("-"),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
            Line::raw(""),
        ];

        if self.data.node_type == NodeType::Controlplane {
            lines.push(Line::styled(
                "  Ready to bootstrap the cluster.",
                Style::default().fg(Color::DarkGray),
            ));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(" [b/Enter] ", Style::default().fg(Color::Green)),
                Span::raw("Bootstrap"),
                Span::raw("  "),
                Span::styled(" [q] ", Style::default().fg(Color::Cyan)),
                Span::raw("Quit"),
            ]));
        } else {
            lines.push(Line::styled(
                "  Worker node ready. Join to existing cluster.",
                Style::default().fg(Color::Yellow),
            ));
        }

        let content = Paragraph::new(lines).block(Block::default().borders(Borders::ALL));

        frame.render_widget(content, area);
    }

    /// Draw bootstrapping state
    fn draw_bootstrapping(&self, frame: &mut Frame, area: Rect) {
        let spinner = self.data.spinner();
        let content = Paragraph::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(format!("  {spinner} "), Style::default().fg(Color::Cyan)),
                Span::styled(
                    "Bootstrapping cluster...",
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::raw(""),
            Line::styled(
                "  Initializing etcd and starting Kubernetes control plane.",
                Style::default().fg(Color::DarkGray),
            ),
        ])
        .block(Block::default().borders(Borders::ALL));

        frame.render_widget(content, area);
    }

    /// Draw waiting for healthy state
    fn draw_waiting_healthy(&self, frame: &mut Frame, area: Rect) {
        let elapsed = self
            .data
            .wait_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);

        let spinner = self.data.spinner();
        let content = Paragraph::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(format!("  {spinner} "), Style::default().fg(Color::Cyan)),
                Span::styled(
                    "Waiting for cluster to become healthy...",
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("     Elapsed: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{elapsed} seconds"),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::raw(""),
            Line::styled("  Checklist:", Style::default().fg(Color::DarkGray)),
            Line::raw("    [ ] etcd running"),
            Line::raw("    [ ] API server running"),
            Line::raw("    [ ] Node ready"),
            Line::raw(""),
            Line::from(vec![
                Span::styled(" [q] ", Style::default().fg(Color::Cyan)),
                Span::raw("Quit"),
            ]),
        ])
        .block(Block::default().borders(Borders::ALL));

        frame.render_widget(content, area);
    }

    /// Draw complete state
    fn draw_complete(&self, frame: &mut Frame, area: Rect) {
        let content = Paragraph::new(vec![
            Line::raw(""),
            Line::styled(
                "  🎉 Cluster bootstrap complete!",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled("     Cluster: ", Style::default().fg(Color::DarkGray)),
                Span::styled(&self.data.cluster_name, Style::default().fg(Color::Cyan)),
            ]),
            Line::from(vec![
                Span::styled("     Context: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.data.context_name.as_deref().unwrap_or("-"),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
            Line::raw(""),
            Line::styled("  Next steps:", Style::default().fg(Color::White)),
            Line::styled(
                "    • Get kubeconfig: talosctl kubeconfig",
                Style::default().fg(Color::DarkGray),
            ),
            Line::styled(
                "    • View cluster: kubectl get nodes",
                Style::default().fg(Color::DarkGray),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled(" [Enter] ", Style::default().fg(Color::Green)),
                Span::raw("Exit"),
                Span::raw("  "),
                Span::styled(" [q] ", Style::default().fg(Color::Cyan)),
                Span::raw("Quit"),
            ]),
        ])
        .block(Block::default().borders(Borders::ALL));

        frame.render_widget(content, area);
    }

    /// Draw error state
    fn draw_error(&self, frame: &mut Frame, area: Rect, message: &str) {
        let content = Paragraph::new(vec![
            Line::raw(""),
            Line::styled("  Error occurred:", Style::default().fg(Color::Red)),
            Line::raw(""),
            Line::styled(format!("  {message}"), Style::default().fg(Color::White)),
            Line::raw(""),
            Line::styled(
                "  Press [r] to retry, [q] to quit.",
                Style::default().fg(Color::Yellow),
            ),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        );

        frame.render_widget(content, area);
    }
}

impl Component for WizardComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        // Only `y` goes ahead, so the Enter that opened the dialog can't
        // confirm it
        if let Some(confirm) = self.confirming {
            return Ok(match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.confirming = None;
                    Some(match confirm {
                        WizardConfirm::Apply => Action::WizardApplyConfig,
                        WizardConfirm::Bootstrap => Action::WizardBootstrap,
                    })
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.confirming = None;
                    None
                }
                _ => None,
            });
        }
        let action = match &self.state {
            WizardState::Connecting => self.handle_waiting_key(key),
            WizardState::SelectDisk => self.handle_select_disk_key(key),
            WizardState::ConfigureCluster => self.handle_configure_key(key),
            WizardState::ConfigReady => self.handle_config_ready_key(key),
            WizardState::Applying => self.handle_waiting_key(key),
            WizardState::WaitingReboot => self.handle_waiting_key(key),
            WizardState::ReadyToBootstrap => self.handle_ready_bootstrap_key(key),
            WizardState::Bootstrapping => self.handle_waiting_key(key),
            WizardState::WaitingHealthy => self.handle_waiting_key(key),
            WizardState::Complete => self.handle_complete_key(key),
            WizardState::Error(_) => self.handle_error_key(key),
        };

        Ok(action)
    }

    fn update(&mut self, _action: Action) -> Result<Option<Action>> {
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let [header, content] = Layout::vertical([
            Constraint::Length(1), // Header
            Constraint::Fill(1),   // Content
        ])
        .areas(area);

        // Draw header
        self.draw_header(frame, header);

        // Draw content based on state
        match &self.state.clone() {
            WizardState::Connecting => self.draw_connecting(frame, content),
            WizardState::SelectDisk => self.draw_select_disk(frame, content),
            WizardState::ConfigureCluster => self.draw_configure_cluster(frame, content),
            WizardState::ConfigReady => self.draw_config_ready(frame, content),
            WizardState::Applying => self.draw_applying(frame, content),
            WizardState::WaitingReboot => self.draw_waiting_reboot(frame, content),
            WizardState::ReadyToBootstrap => self.draw_ready_bootstrap(frame, content),
            WizardState::Bootstrapping => self.draw_bootstrapping(frame, content),
            WizardState::WaitingHealthy => self.draw_waiting_healthy(frame, content),
            WizardState::Complete => self.draw_complete(frame, content),
            WizardState::Error(msg) => self.draw_error(frame, content, msg),
        }

        if let Some(confirm) = self.confirming {
            self.draw_confirm(frame, area, confirm);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn press(wizard: &mut WizardComponent, code: KeyCode) -> Option<Action> {
        wizard
            .handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
            .unwrap()
    }

    #[test]
    fn long_poll_errors_are_cut_on_char_boundaries() {
        let error = "é".repeat(100);
        let short = format_poll_error(&error);
        assert_eq!(short.chars().count(), 80);
    }

    #[test]
    fn apply_needs_y_not_a_second_enter() {
        let mut wizard = WizardComponent::new("10.0.0.5".into());
        wizard.transition(WizardState::ConfigReady);

        assert!(press(&mut wizard, KeyCode::Enter).is_none());
        // The second Enter of a double press doesn't install
        assert!(press(&mut wizard, KeyCode::Enter).is_none());
        assert!(matches!(
            press(&mut wizard, KeyCode::Char('y')),
            Some(Action::WizardApplyConfig)
        ));
    }

    #[test]
    fn cancel_closes_the_dialog() {
        let mut wizard = WizardComponent::new("10.0.0.5".into());
        wizard.transition(WizardState::ReadyToBootstrap);

        assert!(press(&mut wizard, KeyCode::Char('b')).is_none());
        assert!(press(&mut wizard, KeyCode::Esc).is_none());
        // The dialog is gone: `y` alone does nothing now
        assert!(press(&mut wizard, KeyCode::Char('y')).is_none());

        press(&mut wizard, KeyCode::Enter);
        assert!(matches!(
            press(&mut wizard, KeyCode::Char('y')),
            Some(Action::WizardBootstrap)
        ));
    }

    #[test]
    fn confirm_dialog_names_the_disk() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut wizard = WizardComponent::new("10.0.0.5".into());
        wizard.transition(WizardState::ConfigReady);
        press(&mut wizard, KeyCode::Enter);

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| wizard.draw(f, f.area()).unwrap())
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(screen.contains("Install Talos on 10.0.0.5"));
        assert!(screen.contains("will be erased"));
    }
}
