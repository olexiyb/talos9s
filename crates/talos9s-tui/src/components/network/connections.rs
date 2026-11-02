//! Connections drill-down: filtering, visual selection, table and detail

use super::{ConnSortBy, NetworkStatsComponent, ViewMode, port_to_service};
use crate::action::Action;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use talos_rs::{ConnectionCounts, ConnectionInfo, ConnectionState};

/// Interfaces that only carry pod traffic: flannel's bridge and overlay,
/// veth pairs, Cilium's per-pod `lxc*` and its `cilium_host`/`cilium_net`/
/// `cilium_vxlan` devices
fn is_pod_interface(name: &str) -> bool {
    name == "cni0"
        || ["flannel", "veth", "lxc", "cilium_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

impl NetworkStatsComponent {
    /// Update connections and calculate counts
    pub(super) fn update_connections(&mut self, connections: Vec<ConnectionInfo>) {
        if let Some(data) = self.data_mut() {
            data.conn_counts = ConnectionCounts::count_by_state(&connections);
            data.connections = connections;
        }
    }

    /// Get filtered and sorted connections for display
    /// Uses pre-filtered connections based on selected interface
    pub(super) fn get_filtered_connections(&self) -> Vec<ConnectionInfo> {
        let Some(data) = self.data() else {
            return Vec::new();
        };

        // Use the pre-filtered list (filtered by interface) unless showing all
        let source: Vec<_> = if self.show_all_connections || self.view_mode == ViewMode::Interfaces
        {
            // Show all connections
            data.connections.clone()
        } else {
            // In connections view, use interface-filtered list
            self.filtered_connections.clone()
        };

        let mut conns: Vec<_> = source
            .into_iter()
            .filter(|c| !self.listening_only || c.state == ConnectionState::Listen)
            .collect();

        match self.conn_sort_by {
            ConnSortBy::State => {
                // Sort by state priority: LISTEN, ESTABLISHED, TIME_WAIT, CLOSE_WAIT, others
                conns.sort_by(|a, b| {
                    let priority = |s: &ConnectionState| match s {
                        ConnectionState::Listen => 0,
                        ConnectionState::Established => 1,
                        ConnectionState::TimeWait => 2,
                        ConnectionState::CloseWait => 3,
                        ConnectionState::SynSent => 4,
                        _ => 5,
                    };
                    priority(&a.state)
                        .cmp(&priority(&b.state))
                        .then_with(|| a.local_port.cmp(&b.local_port))
                });
            }
            ConnSortBy::Port => {
                conns.sort_by_key(|a| a.local_port);
            }
        }

        conns
    }

    /// Move the connection cursor
    fn move_connection(&mut self, step: crate::ui_ext::Move) {
        let count = self.get_filtered_connections().len();
        if let Some(i) = crate::ui_ext::moved(self.conn_selected, count, step) {
            self.conn_selected = i;
            self.conn_table_state.select(Some(i));
        }
    }

    /// Enter connection drill-down view for the selected interface
    pub(super) fn enter_connections_view(&mut self) {
        // Store the selected interface name
        self.selected_interface = self.selected_device_name();

        // Filter connections for this interface
        self.filter_connections_for_interface();

        self.view_mode = ViewMode::Connections;
        self.conn_selected = 0;
        self.conn_table_state.select(Some(0));
    }

    /// Filter connections based on the selected interface
    fn filter_connections_for_interface(&mut self) {
        let Some(data) = self.data() else {
            self.filtered_connections.clear();
            return;
        };

        let Some(ref iface) = self.selected_interface else {
            // No interface selected - show all connections
            self.filtered_connections = data.connections.clone();
            return;
        };

        let pod_subnets = self.pod_subnets();
        self.filtered_connections = data
            .connections
            .iter()
            .filter(|conn| Self::connection_matches_interface(conn, iface, &pod_subnets))
            .cloned()
            .collect();
    }

    /// The cluster's pod networks: from the node's config once read, Talos'
    /// default until then (or when it can't be read)
    pub(super) fn pod_subnets(&self) -> Vec<String> {
        match &self.pod_subnets {
            Some(subnets) if !subnets.is_empty() => subnets.clone(),
            _ => vec![talos9s_core::network::DEFAULT_POD_SUBNET.to_string()],
        }
    }

    /// Read the pod networks from the node's config once, in the background
    /// (`talosctl get machineconfig`, of which only the subnets are kept)
    pub(super) fn poll_pod_subnets(&mut self) {
        if let Some(task) = self.pod_subnets_task.take_if(|t| t.is_finished()) {
            if let Some(Ok(subnets)) = crate::task::poll_finished(task) {
                self.pod_subnets = Some(subnets);
                if self.view_mode == ViewMode::Connections {
                    self.filter_connections_for_interface();
                }
            }
            return;
        }
        if self.pod_subnets.is_some() || self.pod_subnets_task.is_some() {
            return;
        }
        let node = self.address.clone();
        let context = self.talos_context.clone();
        let config_path = self.config_path.clone();
        self.pod_subnets_task = Some(tokio::spawn(async move {
            talos_rs::get_machine_config_for_node(context.as_deref(), &node, config_path.as_deref())
                .await
                .map(|config| config.pod_subnets)
                // Unreadable: the default stands, and isn't asked again
                .unwrap_or_default()
        }));
    }

    /// Check if a connection likely uses the given interface
    fn connection_matches_interface(
        conn: &ConnectionInfo,
        iface: &str,
        pod_subnets: &[String],
    ) -> bool {
        match iface {
            // Loopback - connections to/from localhost
            "lo" => {
                conn.local_ip.starts_with("127.")
                    || conn.local_ip == "::1"
                    || conn.remote_ip.starts_with("127.")
                    || conn.remote_ip == "::1"
            }
            // CNI bridge, flannel overlay, veth pairs and Cilium's lxc /
            // cilium_* devices carry pod traffic: addresses in the cluster's
            // pod networks. Nodes often sit on 10.x too, and their traffic
            // doesn't cross these interfaces.
            name if is_pod_interface(name) => {
                let is_pod = |ip: &str| {
                    pod_subnets
                        .iter()
                        .any(|net| talos9s_core::network::cidr_contains(net, ip))
                };
                is_pod(&conn.local_ip) || is_pod(&conn.remote_ip)
            }
            // Main interface (eth0, enp0s*, etc.) - non-loopback, non-pod connections
            _ => {
                // Exclude loopback
                let is_loopback = conn.local_ip.starts_with("127.")
                    || conn.local_ip == "::1"
                    || conn.remote_ip.starts_with("127.")
                    || conn.remote_ip == "::1";

                // Include all external connections and listeners on 0.0.0.0
                !is_loopback || conn.local_ip == "0.0.0.0" || conn.local_ip == "::"
            }
        }
    }

    /// Return to interfaces view
    pub(super) fn exit_connections_view(&mut self) {
        self.view_mode = ViewMode::Interfaces;
        self.selected_interface = None;
        self.filtered_connections.clear();
        self.show_all_connections = false;
        self.conn_visual.clear();
    }

    /// Format a connection as a string for copying
    fn format_connection(conn: &ConnectionInfo) -> String {
        let local = if !conn.local_ip.is_empty() {
            format!("{}:{}", conn.local_ip, conn.local_port)
        } else {
            format!(":{}", conn.local_port)
        };

        let remote = if conn.remote_port > 0 {
            format!("{}:{}", conn.remote_ip, conn.remote_port)
        } else {
            "*:*".to_string()
        };

        let state = match conn.state {
            ConnectionState::Established => "ESTABLISHED",
            ConnectionState::Listen => "LISTEN",
            ConnectionState::TimeWait => "TIME_WAIT",
            ConnectionState::CloseWait => "CLOSE_WAIT",
            ConnectionState::SynSent => "SYN_SENT",
            ConnectionState::SynRecv => "SYN_RECV",
            ConnectionState::FinWait1 => "FIN_WAIT1",
            ConnectionState::FinWait2 => "FIN_WAIT2",
            ConnectionState::Closing => "CLOSING",
            ConnectionState::LastAck => "LAST_ACK",
            ConnectionState::Close => "CLOSE",
            ConnectionState::Unknown => "UNKNOWN",
        };

        // Format process info
        let process = match (&conn.process_name, conn.process_pid) {
            (Some(name), Some(pid)) => format!("{name} ({pid})"),
            (Some(name), None) => name.clone(),
            (None, Some(pid)) => format!("pid:{pid}"),
            (None, None) => "-".to_string(),
        };

        format!(
            "{:<6} {:<22} {:<24} {:<12} {}",
            conn.protocol, local, remote, state, process
        )
    }

    /// Yank (copy) the selected connections, or the current one
    fn yank_conn_selection(&self) -> usize {
        let conns = self.get_filtered_connections();
        let lines: Vec<String> = (self.conn_visual.rows(self.conn_selected))
            .filter_map(|idx| conns.get(idx).map(Self::format_connection))
            .collect();
        crate::clipboard::copy_lines(&lines)
    }

    /// Page up in connection list
    fn conn_page_up(&mut self) {
        let page_size = self.conn_viewport_height.saturating_sub(2) as usize;
        self.conn_selected = self.conn_selected.saturating_sub(page_size);
        self.conn_table_state.select(Some(self.conn_selected));
    }

    /// Page down in connection list
    fn conn_page_down(&mut self) {
        let count = self.get_filtered_connections().len();
        let page_size = self.conn_viewport_height.saturating_sub(2) as usize;
        if count > 0 {
            self.conn_selected = (self.conn_selected + page_size).min(count - 1);
            self.conn_table_state.select(Some(self.conn_selected));
        }
    }

    /// Half page up in connection list
    fn conn_half_page_up(&mut self) {
        let half = (self.conn_viewport_height / 2).max(1) as usize;
        self.conn_selected = self.conn_selected.saturating_sub(half);
        self.conn_table_state.select(Some(self.conn_selected));
    }

    /// Half page down in connection list
    fn conn_half_page_down(&mut self) {
        let count = self.get_filtered_connections().len();
        let half = (self.conn_viewport_height / 2).max(1) as usize;
        if count > 0 {
            self.conn_selected = (self.conn_selected + half).min(count - 1);
            self.conn_table_state.select(Some(self.conn_selected));
        }
    }

    /// Draw the connection view header
    fn draw_conn_header(&self, frame: &mut Frame, area: Rect) {
        let conn_count = self.get_filtered_connections().len();
        let filter_label = if self.listening_only {
            " [LISTEN ONLY]"
        } else {
            ""
        };

        // Show interface name or "all" if showing all connections
        let iface_display = if self.show_all_connections {
            "all".to_string()
        } else {
            self.selected_interface
                .clone()
                .unwrap_or_else(|| "all".to_string())
        };

        // Connections is a subscreen of Interfaces - show breadcrumb style
        let mut spans = vec![
            Span::styled("Interfaces", Style::default().fg(Color::DarkGray)),
            Span::styled(" > ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "Connections: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &iface_display,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" @ ", Style::default().fg(Color::DarkGray)),
            Span::raw(&self.hostname),
            Span::raw("  "),
            Span::styled(
                format!("{conn_count} connections"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(filter_label, Style::default().fg(Color::Yellow)),
        ];

        // Show visual mode indicator
        if let Some(range) = self.conn_visual.range(self.conn_selected) {
            let count = range.count();
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("-- VISUAL ({count} lines) --"),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let header = Paragraph::new(Line::from(spans));
        frame.render_widget(header, area);
    }

    /// Draw the connection summary bar
    fn draw_conn_summary_bar(&self, frame: &mut Frame, area: Rect) {
        let cc = self
            .data()
            .map(|d| d.conn_counts.clone())
            .unwrap_or_default();

        let spans = vec![
            Span::styled(
                format!("{} ", cc.established),
                Style::default().fg(Color::Green),
            ),
            Span::styled("ESTABLISHED", Style::default().fg(Color::DarkGray)),
            Span::raw("   "),
            Span::styled(format!("{} ", cc.listen), Style::default().fg(Color::Cyan)),
            Span::styled("LISTEN", Style::default().fg(Color::DarkGray)),
            Span::raw("   "),
            Span::styled(
                format!("{} ", cc.time_wait),
                if cc.time_wait > 100 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                },
            ),
            Span::styled("TIME_WAIT", Style::default().fg(Color::DarkGray)),
            Span::raw("   "),
            Span::styled(
                format!("{} ", cc.close_wait),
                if cc.close_wait > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                },
            ),
            Span::styled("CLOSE_WAIT", Style::default().fg(Color::DarkGray)),
        ];

        let summary = Paragraph::new(Line::from(spans));
        frame.render_widget(summary, area);
    }

    /// Draw the connection table
    fn draw_conn_table(&mut self, frame: &mut Frame, area: Rect) {
        // Update viewport height for page navigation
        self.conn_viewport_height = area.height;
        self.hits.connections = area;

        // Build column headers with sort indicators
        let state_header = if self.conn_sort_by == ConnSortBy::State {
            "STATE▼"
        } else {
            "STATE"
        };
        let local_header = if self.conn_sort_by == ConnSortBy::Port {
            "LOCAL▼"
        } else {
            "LOCAL"
        };

        let header_cells = [
            Cell::from("PROTO"),
            Cell::from(local_header),
            Cell::from("REMOTE"),
            Cell::from(state_header),
            Cell::from("PROCESS"),
        ];
        let header = Row::new(header_cells)
            .style(Style::default().add_modifier(Modifier::DIM))
            .bottom_margin(1);

        let conns = self.get_filtered_connections();
        let in_visual = self.conn_visual.active();

        // Extract data needed for the closure
        let time_wait_count = self.data().map(|d| d.conn_counts.time_wait).unwrap_or(0);
        let services = self.data().map(|d| d.services.clone()).unwrap_or_default();

        let rows: Vec<Row> = conns
            .iter()
            .enumerate()
            .map(|(idx, conn)| {
                // Format local address with IP
                let local = if !conn.local_ip.is_empty() && conn.local_port > 0 {
                    format!("{}:{}", conn.local_ip, conn.local_port)
                } else if conn.local_port > 0 {
                    format!("*:{}", conn.local_port)
                } else {
                    "*:*".to_string()
                };

                // Format remote address
                let remote = if conn.remote_port > 0 {
                    format!("{}:{}", conn.remote_ip, conn.remote_port)
                } else {
                    "*:*".to_string()
                };

                // Format state with color
                let (state_str, state_color) = match conn.state {
                    ConnectionState::Established => ("ESTABLISHED", Color::Green),
                    ConnectionState::Listen => ("LISTEN", Color::Cyan),
                    ConnectionState::TimeWait => (
                        "TIME_WAIT",
                        if time_wait_count > 100 {
                            Color::Yellow
                        } else {
                            Color::White
                        },
                    ),
                    ConnectionState::CloseWait => ("CLOSE_WAIT", Color::Red),
                    ConnectionState::SynSent => ("SYN_SENT", Color::Yellow),
                    ConnectionState::SynRecv => ("SYN_RECV", Color::Yellow),
                    ConnectionState::FinWait1 => ("FIN_WAIT1", Color::DarkGray),
                    ConnectionState::FinWait2 => ("FIN_WAIT2", Color::DarkGray),
                    ConnectionState::Closing => ("CLOSING", Color::DarkGray),
                    ConnectionState::LastAck => ("LAST_ACK", Color::DarkGray),
                    ConnectionState::Close => ("CLOSE", Color::DarkGray),
                    ConnectionState::Unknown => ("UNKNOWN", Color::DarkGray),
                };

                // Format process/service info
                // For listening ports, show service name if known
                let (owner_text, owner_color) =
                    if let Some(service_name) = port_to_service(conn.local_port) {
                        // Check service health
                        let health_status = services
                            .get(service_name)
                            .and_then(talos_rs::ServiceInfo::is_healthy)
                            .map(|healthy| if healthy { "+" } else { "!" })
                            .unwrap_or("?");

                        let process_suffix = conn
                            .process_name
                            .as_ref()
                            .map(|p| format!(" ({p})"))
                            .unwrap_or_default();

                        (
                            format!("[{health_status}{service_name}]{process_suffix}"),
                            Color::Cyan,
                        )
                    } else {
                        // Regular process info
                        let process = conn
                            .process_name
                            .as_ref()
                            .map(|name| {
                                if let Some(pid) = conn.process_pid {
                                    format!("{name} ({pid})")
                                } else {
                                    name.clone()
                                }
                            })
                            .unwrap_or_else(|| "-".to_string());
                        (process, Color::Yellow)
                    };

                // Check if this row is selected in visual mode
                let is_selected = in_visual && self.conn_visual.contains(idx, self.conn_selected);

                // Row style - highlight selection with magenta background
                let row_style = if is_selected {
                    Style::default().bg(Color::Rgb(60, 20, 60)) // Dark magenta
                } else {
                    Style::default()
                };

                Row::new([
                    Cell::from(conn.protocol.clone()),
                    Cell::from(local),
                    Cell::from(remote),
                    Cell::from(state_str).style(Style::default().fg(state_color)),
                    Cell::from(owner_text).style(Style::default().fg(owner_color)),
                ])
                .style(row_style)
            })
            .collect();

        let widths = [
            Constraint::Length(6),  // PROTO
            Constraint::Length(22), // LOCAL (IP:port)
            Constraint::Length(24), // REMOTE
            Constraint::Length(12), // STATE
            Constraint::Min(16),    // PROCESS (takes remaining space)
        ];

        let row_count = rows.len();
        let table = Table::new(rows, widths)
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(table, area, &mut self.conn_table_state);
        self.hits.connection_rows =
            crate::ui_ext::RowHits::new(area, false, 2, self.conn_table_state.offset(), row_count);
    }

    /// Draw footer for connection view
    pub(super) fn connections_key_hints(&self) -> Line<'static> {
        let listen_label = if self.listening_only { "all" } else { "listen" };
        let all_label = if self.show_all_connections {
            "iface"
        } else {
            "all"
        };

        // BPF filter indicator with color coding
        let (bpf_label, bpf_color) = if self.capture.use_bpf_filter {
            ("BPF:ON", Color::Green)
        } else {
            ("BPF:OFF", Color::Yellow)
        };

        let spans = if self.conn_visual.active() {
            // Visual mode footer
            vec![
                Span::styled("<j/k>", crate::ui_ext::KEY),
                Span::raw(" extend  "),
                Span::styled("<y>", crate::ui_ext::KEY),
                Span::raw(" yank  "),
                Span::styled("<Esc/V>", crate::ui_ext::KEY),
                Span::raw(" cancel  "),
                Span::styled("<Esc/q>", crate::ui_ext::KEY),
                Span::raw(" interfaces"),
            ]
        } else {
            // Normal mode footer
            vec![
                Span::styled("<j/k>", crate::ui_ext::KEY),
                Span::raw(" navigate  "),
                Span::styled("<c>", crate::ui_ext::KEY),
                Span::raw(" capture  "),
                Span::styled("<f>", crate::ui_ext::KEY),
                Span::raw(" "),
                Span::styled(bpf_label, Style::default().fg(bpf_color)),
                Span::raw("  "),
                Span::styled("<L>", crate::ui_ext::KEY),
                Span::raw(format!(" {listen_label}  ")),
                Span::styled("<A>", crate::ui_ext::KEY),
                Span::raw(format!(" {all_label}  ")),
                Span::styled("<Esc/q>", crate::ui_ext::KEY),
                Span::raw(" interfaces"),
            ]
        };

        Line::from(spans)
    }

    /// Draw the selected connection detail section
    fn draw_conn_detail(&self, frame: &mut Frame, area: Rect) {
        let conns = self.get_filtered_connections();
        let Some(conn) = conns.get(self.conn_selected) else {
            return;
        };
        let services = self.data().map(|d| d.services.clone()).unwrap_or_default();

        // Format local address
        let local_addr = if !conn.local_ip.is_empty() {
            format!("{}:{}", conn.local_ip, conn.local_port)
        } else {
            format!("*:{}", conn.local_port)
        };

        // Format remote address
        let remote_addr = if conn.remote_port > 0 {
            format!("{}:{}", conn.remote_ip, conn.remote_port)
        } else {
            "*:*".to_string()
        };

        // State with color
        let (state_str, state_color) = match conn.state {
            ConnectionState::Established => ("ESTABLISHED", Color::Green),
            ConnectionState::Listen => ("LISTEN", Color::Cyan),
            ConnectionState::TimeWait => ("TIME_WAIT", Color::Yellow),
            ConnectionState::CloseWait => ("CLOSE_WAIT", Color::Red),
            ConnectionState::SynSent => ("SYN_SENT", Color::Yellow),
            ConnectionState::SynRecv => ("SYN_RECV", Color::Yellow),
            _ => ("OTHER", Color::DarkGray),
        };

        // Format process info
        let process_info = match (&conn.process_name, conn.process_pid) {
            (Some(name), Some(pid)) => format!("{name} (PID: {pid})"),
            (Some(name), None) => name.clone(),
            (None, Some(pid)) => format!("PID: {pid}"),
            (None, None) => "-".to_string(),
        };

        // Format namespace
        let netns_info = conn.netns.as_deref().unwrap_or("host");

        let mut lines = vec![
            Line::from(vec![
                Span::styled("Protocol: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&conn.protocol),
                Span::raw("   "),
                Span::styled("State: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    state_str,
                    Style::default()
                        .fg(state_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("Process: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(&process_info, Style::default().fg(Color::Yellow)),
            ]),
            Line::from(vec![
                Span::styled(
                    "Local:  ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(&local_addr),
                Span::raw("   "),
                Span::styled(
                    "Remote: ",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(&remote_addr),
                Span::raw("   "),
                Span::styled("Netns: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(netns_info, Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(vec![
                Span::styled("RX Queue: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    conn.rx_queue.to_string(),
                    if conn.rx_queue > 0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(" bytes   "),
                Span::styled("TX Queue: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    conn.tx_queue.to_string(),
                    if conn.tx_queue > 0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(" bytes"),
            ]),
        ];

        // Show service info and available actions
        if let Some(service_name) = port_to_service(conn.local_port) {
            let service_info = services.get(service_name);
            let (health_text, health_color) = service_info
                .and_then(talos_rs::ServiceInfo::is_healthy)
                .map(|healthy| {
                    if healthy {
                        ("healthy", Color::Green)
                    } else {
                        ("unhealthy", Color::Red)
                    }
                })
                .unwrap_or(("unknown", Color::DarkGray));

            let state_text = service_info.map(|s| s.state.as_str()).unwrap_or("unknown");

            lines.push(Line::from(vec![
                Span::styled("Service: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    service_name,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ("),
                Span::styled(state_text, Style::default().fg(Color::DarkGray)),
                Span::raw(", "),
                Span::styled(health_text, Style::default().fg(health_color)),
                Span::raw(")   "),
                Span::styled("<o>", crate::ui_ext::KEY),
                Span::styled(" logs  ", Style::default().fg(Color::DarkGray)),
                Span::styled("<R>", crate::ui_ext::KEY),
                Span::styled(" restart", Style::default().fg(Color::DarkGray)),
            ]));
        } else {
            // No service - show process info hint
            lines.push(Line::from(vec![
                Span::styled("Actions: ", Style::default().fg(Color::DarkGray)),
                Span::styled("<y>", crate::ui_ext::KEY),
                Span::styled(" yank  ", Style::default().fg(Color::DarkGray)),
                Span::styled("<V>", crate::ui_ext::KEY),
                Span::styled(" visual select", Style::default().fg(Color::DarkGray)),
            ]));
        }

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Selected Connection ",
                Style::default().add_modifier(Modifier::BOLD),
            ));

        let detail = Paragraph::new(lines).block(block);
        frame.render_widget(detail, area);
    }

    /// Draw the connection drill-down view
    pub(super) fn draw_connections_view(&mut self, frame: &mut Frame, area: Rect) {
        let [header_area, summary_area, table_area, detail_area] = Layout::vertical([
            Constraint::Length(1), // Header
            Constraint::Length(1), // Summary bar
            Constraint::Min(5),    // Connection table
            Constraint::Length(5), // Detail section (4 lines + border)
        ])
        .areas(area);

        self.hits.breadcrumb = header_area;
        self.draw_conn_header(frame, header_area);
        self.draw_conn_summary_bar(frame, summary_area);
        self.draw_conn_table(frame, table_area);
        self.draw_conn_detail(frame, detail_area);
    }

    /// Handle key events in Connections view
    pub(super) fn handle_connections_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            // Quit/back
            KeyCode::Char('q') => {
                self.exit_connections_view();
                None
            }
            KeyCode::Esc => {
                if self.conn_visual.active() {
                    self.conn_visual.clear();
                } else {
                    self.exit_connections_view();
                }
                None
            }

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_connection(crate::ui_ext::Move::Next);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_connection(crate::ui_ext::Move::Prev);
                None
            }
            KeyCode::Char('g') => {
                self.move_connection(crate::ui_ext::Move::First);
                None
            }
            KeyCode::Char('G') => {
                self.move_connection(crate::ui_ext::Move::Last);
                None
            }

            // Page navigation
            KeyCode::PageUp => {
                self.conn_page_up();
                None
            }
            KeyCode::PageDown => {
                self.conn_page_down();
                None
            }

            // Half-page scroll (Ctrl+U / Ctrl+D)
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.conn_half_page_up();
                None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.conn_half_page_down();
                None
            }

            // Sorting
            KeyCode::Char('1') => {
                self.conn_sort_by = ConnSortBy::State;
                self.conn_selected = 0;
                self.conn_table_state.select(Some(0));
                self.conn_visual.clear();
                None
            }
            KeyCode::Char('2') => {
                self.conn_sort_by = ConnSortBy::Port;
                self.conn_selected = 0;
                self.conn_table_state.select(Some(0));
                self.conn_visual.clear();
                None
            }

            // Filter
            KeyCode::Char('L') => {
                self.listening_only = !self.listening_only;
                self.conn_selected = 0;
                self.conn_table_state.select(Some(0));
                self.conn_visual.clear();
                None
            }

            // Toggle show all connections (bypass interface filter)
            KeyCode::Char('A') => {
                self.show_all_connections = !self.show_all_connections;
                self.conn_selected = 0;
                self.conn_table_state.select(Some(0));
                self.conn_visual.clear();
                None
            }

            // Visual line selection mode
            KeyCode::Char('V') => {
                self.conn_visual.toggle(self.conn_selected);
                None
            }

            // Yank selection or current line to clipboard
            KeyCode::Char('y') => {
                let count = self.yank_conn_selection();
                if count > 0 {
                    tracing::info!("Copied {} connection(s) to clipboard", count);
                }
                // Clear visual selection after yank
                self.conn_visual.clear();
                None
            }

            // Refresh
            KeyCode::Char('r') => Some(Action::Refresh),

            // Open service logs (for known service ports)
            KeyCode::Char('o') => self.open_service_logs(),

            // Restart service (for known service ports) - requires confirmation
            KeyCode::Char('R') => {
                self.initiate_service_restart();
                None
            }

            // Show DNS config (/etc/resolv.conf)
            KeyCode::Char('d') => {
                self.show_dns_config();
                Some(Action::Refresh) // Trigger fetch immediately
            }

            // Show routing table (/proc/net/route)
            KeyCode::Char('t') => {
                self.show_routing_table();
                Some(Action::Refresh) // Trigger fetch immediately
            }

            // Packet capture (uses BPF filter to exclude port 50000, preventing feedback loop)
            KeyCode::Char('c') => {
                self.toggle_capture();
                None
            }
            KeyCode::Char('s') => {
                self.save_capture();
                None
            }
            // Toggle BPF filter for packet capture
            KeyCode::Char('f') => {
                self.toggle_bpf_filter();
                None
            }

            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(local: &str, remote: &str) -> ConnectionInfo {
        ConnectionInfo {
            protocol: "tcp".into(),
            local_ip: local.into(),
            local_port: 1,
            remote_ip: remote.into(),
            remote_port: 2,
            state: ConnectionState::Established,
            rx_queue: 0,
            tx_queue: 0,
            process_pid: None,
            process_name: None,
            netns: None,
        }
    }

    fn matches(c: &ConnectionInfo, iface: &str) -> bool {
        let pods = ["10.244.0.0/16".to_string()];
        NetworkStatsComponent::connection_matches_interface(c, iface, &pods)
    }

    #[test]
    fn node_network_on_10_x_is_not_pod_traffic() {
        // Node-to-node etcd on a 10.x private network
        let etcd = conn("10.0.0.11", "10.0.0.12");
        assert!(!matches(&etcd, "cni0"));
        assert!(!matches(&etcd, "flannel.1"));
        assert!(!matches(&etcd, "veth1234"));
        assert!(matches(&etcd, "eth0"));

        let pod = conn("10.244.1.5", "10.0.0.11");
        assert!(matches(&pod, "cni0"));
        assert!(matches(&pod, "flannel.1"));
        assert!(matches(&pod, "veth1234"));
    }

    #[test]
    fn loopback_stays_on_lo() {
        let local = conn("127.0.0.1", "127.0.0.1");
        assert!(matches(&local, "lo"));
        assert!(!matches(&local, "eth0"));
        assert!(matches(&conn("::1", "::1"), "lo"));
        assert!(!matches(&conn("65.21.130.10", "1.1.1.1"), "lo"));
        // Listeners on every address show on the main interface
        assert!(matches(&conn("0.0.0.0", ""), "eth0"));
    }

    #[test]
    fn pod_traffic_follows_the_clusters_subnets() {
        let custom = ["10.44.0.0/16".to_string(), "fd00:44::/56".to_string()];
        let matches = |c: &ConnectionInfo| {
            NetworkStatsComponent::connection_matches_interface(c, "cni0", &custom)
        };
        assert!(matches(&conn("10.44.2.9", "10.0.0.11")));
        assert!(matches(&conn("fd00:44::9", "fd00::1")));
        // Talos' default range is not a pod network here
        assert!(!matches(&conn("10.244.1.5", "10.0.0.11")));
        // Cilium's devices count as pod interfaces; node traffic stays off them
        let node = conn("37.27.227.99", "65.21.130.10");
        for iface in ["lxc11e671568111", "cilium_host", "cilium_vxlan"] {
            assert!(NetworkStatsComponent::connection_matches_interface(
                &conn("10.44.2.9", "10.44.0.1"),
                iface,
                &custom
            ));
            assert!(!NetworkStatsComponent::connection_matches_interface(
                &node, iface, &custom
            ));
        }
        assert!(NetworkStatsComponent::connection_matches_interface(
            &node, "enp5s0", &custom
        ));
    }
}
