//! Interfaces view: device table, rates, totals, service ports and warnings

use super::{NetworkData, NetworkStatsComponent, SortBy, ViewMode, conn_warnings, service_ports};
use crate::action::Action;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};
use std::time::Instant;
use talos_rs::{NetDevRate, NetDevStats};
use talos9s_core::constants::{CLOSE_WAIT_WARN, TIME_WAIT_WARN};

/// A count that fits an 8-column cell: exact below a million, then
/// `201.5M` / `3.2G` (cutting digits off would show a wrong number)
fn short_count(n: u64) -> String {
    match n {
        0..=999_999 => n.to_string(),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}G", n as f64 / 1e9),
    }
}

/// A per-second count, e.g. `0.4/s` or `120/s`
fn per_second(n: f64) -> String {
    if n >= 10.0 {
        format!("{n:.0}/s")
    } else {
        format!("{n:.1}/s")
    }
}

impl NetworkStatsComponent {
    /// Update devices and calculate rates
    pub(super) fn update_devices(&mut self, new_devices: Vec<NetDevStats>) {
        let sort_by = self.sort_by;
        let Some(data) = self.data_mut() else { return };

        let now = Instant::now();
        let elapsed_secs = data
            .last_sample
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);

        // Calculate rates if we have previous data
        if elapsed_secs > 0.1 {
            for dev in &new_devices {
                if let Some(prev) = data.prev_devices.get(&dev.name) {
                    let rate = NetDevRate::from_delta(prev, dev, elapsed_secs);
                    data.rates.insert(dev.name.clone(), rate);
                }
            }
        }

        // A deleted interface (a pod's veth) keeps no rate in the totals
        data.rates
            .retain(|name, _| new_devices.iter().any(|d| &d.name == name));

        // Store current as previous for next calculation
        data.prev_devices.clear();
        for dev in &new_devices {
            data.prev_devices.insert(dev.name.clone(), dev.clone());
        }
        data.last_sample = Some(now);

        // Calculate totals
        data.total_rx_rate = data.rates.values().map(|r| r.rx_bytes_per_sec).sum();
        data.total_tx_rate = data.rates.values().map(|r| r.tx_bytes_per_sec).sum();
        data.total_errors = new_devices
            .iter()
            .map(talos_rs::NetDevStats::total_errors)
            .sum();
        data.total_dropped = new_devices
            .iter()
            .map(talos_rs::NetDevStats::total_dropped)
            .sum();
        let current = |f: fn(&NetDevRate) -> f64| -> f64 {
            new_devices
                .iter()
                .filter_map(|d| data.rates.get(&d.name))
                .map(f)
                .sum()
        };
        // Only rates from this sample: a device gone since keeps its old entry
        data.errors_per_sec = if elapsed_secs > 0.1 {
            current(|r| r.errors_per_sec)
        } else {
            0.0
        };
        data.dropped_per_sec = if elapsed_secs > 0.1 {
            current(|r| r.dropped_per_sec)
        } else {
            0.0
        };

        // Sort and store devices
        data.devices = new_devices;
        Self::sort_devices_by(data, sort_by);
    }

    /// Sort devices based on sort order (static helper to avoid borrow issues)
    fn sort_devices_by(data: &mut NetworkData, sort_by: SortBy) {
        match sort_by {
            SortBy::Traffic => {
                // Sort by rate if available, otherwise by cumulative traffic
                data.devices.sort_by(|a, b| {
                    let rate_a = data
                        .rates
                        .get(&a.name)
                        .map(talos_rs::NetDevRate::total_rate)
                        .unwrap_or(0);
                    let rate_b = data
                        .rates
                        .get(&b.name)
                        .map(talos_rs::NetDevRate::total_rate)
                        .unwrap_or(0);
                    if rate_a != rate_b {
                        rate_b.cmp(&rate_a)
                    } else {
                        b.total_traffic().cmp(&a.total_traffic())
                    }
                });
            }
            SortBy::Errors => {
                data.devices.sort_by(|a, b| {
                    let err_a = a.total_errors() + a.total_dropped();
                    let err_b = b.total_errors() + b.total_dropped();
                    err_b.cmp(&err_a)
                });
            }
        }
    }

    /// Sort devices based on current sort order
    pub(super) fn sort_devices(&mut self) {
        let sort_by = self.sort_by;
        let Some(data) = self.data_mut() else { return };
        Self::sort_devices_by(data, sort_by);
    }

    /// Move the interface cursor
    fn move_device(&mut self, step: crate::ui_ext::Move) {
        let count = self.data().map(|d| d.devices.len()).unwrap_or(0);
        if let Some(i) = crate::ui_ext::moved(self.selected, count, step) {
            self.selected = i;
            self.table_state.select(Some(i));
        }
    }

    /// Get selected device name
    pub(super) fn selected_device_name(&self) -> Option<String> {
        self.data()
            .and_then(|d| d.devices.get(self.selected).map(|dev| dev.name.clone()))
    }

    /// Draw the header
    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let device_count = format!(
            "{} ifaces",
            self.data().map(|d| d.devices.len()).unwrap_or(0)
        );

        let auto_indicator = if self.auto_refresh { "" } else { " [AUTO:OFF]" };

        // Build tab bar for right side (Connections is a subscreen, not a tab)
        let kubespan = self.view_mode == ViewMode::KubeSpan;
        let mut spans = vec![
            Span::styled(
                if kubespan { "KubeSpan: " } else { "Network: " },
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(&self.hostname),
            Span::styled(" (", Style::default().fg(Color::DarkGray)),
            Span::raw(&self.address),
            Span::styled(")", Style::default().fg(Color::DarkGray)),
        ];
        if !kubespan {
            spans.extend([
                Span::raw("  "),
                Span::styled(&device_count, Style::default().fg(Color::DarkGray)),
                Span::styled(auto_indicator, Style::default().fg(Color::Yellow)),
            ]);
        }

        let header = Paragraph::new(Line::from(spans));
        frame.render_widget(header, area);
    }

    /// Draw the summary bar
    fn draw_summary_bar(&self, frame: &mut Frame, area: Rect) {
        let (total_errors, total_dropped, total_rx_rate, total_tx_rate, errors_ps, dropped_ps) =
            self.data()
                .map(|d| {
                    (
                        d.total_errors,
                        d.total_dropped,
                        d.total_rx_rate,
                        d.total_tx_rate,
                        d.errors_per_sec,
                        d.dropped_per_sec,
                    )
                })
                .unwrap_or_default();

        // Totals since the interfaces came up are history; flag only a rise
        let warning = if errors_ps > 0.0 || dropped_ps > 0.0 {
            "! "
        } else {
            ""
        };

        let rx_rate = NetDevStats::format_rate(total_rx_rate);
        let tx_rate = NetDevStats::format_rate(total_tx_rate);

        let mut spans = vec![
            Span::styled(
                warning,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Total:  ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("RX ", Style::default().fg(Color::Green)),
            Span::raw(&rx_rate),
            Span::raw("  "),
            Span::styled("TX ", Style::default().fg(Color::Blue)),
            Span::raw(&tx_rate),
        ];

        // Add errors/dropped if any
        spans.push(Span::raw("   "));
        let counter = |label: &str, total: u64, per_sec: f64, color: Color| {
            if per_sec > 0.0 {
                Span::styled(
                    format!("{label}: {total} (+{})", per_second(per_sec)),
                    Style::default().fg(color),
                )
            } else {
                Span::styled(
                    format!("{label}: {total}"),
                    Style::default().fg(Color::DarkGray),
                )
            }
        };
        spans.push(counter("Errors", total_errors, errors_ps, Color::Red));
        spans.push(Span::raw("   "));
        spans.push(counter("Dropped", total_dropped, dropped_ps, Color::Yellow));

        let summary = Paragraph::new(Line::from(spans));
        frame.render_widget(summary, area);
    }

    /// Draw the connection summary bar
    fn draw_connection_summary(&self, frame: &mut Frame, area: Rect) {
        let cc = self
            .data()
            .map(|d| d.conn_counts.clone())
            .unwrap_or_default();
        let (tw_high, cw_high) = (
            cc.time_wait >= TIME_WAIT_WARN,
            cc.close_wait >= CLOSE_WAIT_WARN,
        );
        let warning = if conn_warnings(&cc) { "! " } else { "" };

        let mut spans = vec![
            Span::styled(
                warning,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("Conns: ", Style::default().add_modifier(Modifier::BOLD)),
        ];

        // ESTABLISHED count
        spans.push(Span::styled(
            format!("{} ", cc.established),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::styled("EST", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw("  "));

        // LISTEN count
        spans.push(Span::styled(
            format!("{} ", cc.listen),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled("LISTEN", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw("  "));

        let tw_style = if tw_high {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        spans.push(Span::styled(format!("{} ", cc.time_wait), tw_style));
        spans.push(Span::styled(
            "TIME_WAIT",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw("  "));

        let cw_style = if cw_high {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        spans.push(Span::styled(format!("{} ", cc.close_wait), cw_style));
        spans.push(Span::styled(
            "CLOSE_WAIT",
            Style::default().fg(Color::DarkGray),
        ));

        let summary = Paragraph::new(Line::from(spans));
        frame.render_widget(summary, area);
    }

    /// Draw service health indicators
    fn draw_service_health(&self, frame: &mut Frame, area: Rect) {
        let (service_health, known) = self
            .data()
            .map(|d| (d.service_health.clone(), !d.connections.is_empty()))
            .unwrap_or_default();

        let mut spans = Vec::new();

        for &(name, port) in service_ports(self.controlplane) {
            // No connections read yet: unknown, not down
            let (indicator, color) = match (known, service_health.get(&port).copied()) {
                (false, _) => ("?", Color::DarkGray),
                (true, Some(true)) => ("●", Color::Green),
                (true, _) => ("○", Color::Red),
            };

            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }

            spans.push(Span::styled(
                format!("{name}:{port} "),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(indicator, Style::default().fg(color)));
        }

        let health = Paragraph::new(Line::from(spans));
        frame.render_widget(health, area);
    }

    /// Draw warning banner if there are errors
    fn draw_warning(&self, frame: &mut Frame, area: Rect) {
        let mut messages = Vec::new();

        let (errors_ps, dropped_ps, conn_counts) = self
            .data()
            .map(|d| (d.errors_per_sec, d.dropped_per_sec, d.conn_counts.clone()))
            .unwrap_or_default();

        // Interface warnings: only while the counters rise
        if errors_ps > 0.0 {
            messages.push(format!(
                "interface errors rising ({})",
                per_second(errors_ps)
            ));
        }
        if dropped_ps > 0.0 {
            messages.push(format!("dropping packets ({})", per_second(dropped_ps)));
        }

        // Connection warnings
        if conn_counts.time_wait > 100 {
            messages.push(format!("High TIME_WAIT ({})", conn_counts.time_wait));
        }
        if conn_counts.close_wait > 0 {
            messages.push(format!("CLOSE_WAIT ({})", conn_counts.close_wait));
        }
        if conn_counts.syn_sent > 0 {
            messages.push(format!("SYN_SENT stuck ({})", conn_counts.syn_sent));
        }

        if !messages.is_empty() {
            let warning = Paragraph::new(Line::from(vec![
                Span::styled(
                    "! ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(messages.join(" | "), Style::default().fg(Color::Yellow)),
            ]));
            frame.render_widget(warning, area);
        }
    }

    /// Draw the device table
    fn draw_device_table(&mut self, frame: &mut Frame, area: Rect) {
        // Build column headers with sort indicators
        let rx_rate_header = if self.sort_by == SortBy::Traffic {
            "RX RATE▼"
        } else {
            "RX RATE"
        };
        let rx_err_header = if self.sort_by == SortBy::Errors {
            "RX ERR▼"
        } else {
            "RX ERR"
        };

        let header_cells = [
            Cell::from("INTERFACE"),
            Cell::from(rx_rate_header),
            Cell::from("TX RATE"),
            Cell::from(rx_err_header),
            Cell::from("TX ERR"),
            Cell::from("RX DROP"),
            Cell::from("TX DROP"),
        ];
        let header = Row::new(header_cells)
            .style(Style::default().add_modifier(Modifier::DIM))
            .bottom_margin(1);

        // Get data for building rows
        self.hits.devices = area;
        let Some(data) = self.data() else {
            let table = Table::new(Vec::<Row>::new(), [Constraint::Fill(1)]).header(header);
            frame.render_stateful_widget(table, area, &mut self.table_state);
            self.hits.device_rows = Default::default();
            return;
        };

        let rows: Vec<Row> = data
            .devices
            .iter()
            .enumerate()
            .map(|(idx, dev)| {
                let rate = data.rates.get(&dev.name);
                let rx_rate = rate
                    .map(|r| NetDevStats::format_rate(r.rx_bytes_per_sec))
                    .unwrap_or_else(|| "0 B/s".to_string());
                let tx_rate = rate
                    .map(|r| NetDevStats::format_rate(r.tx_bytes_per_sec))
                    .unwrap_or_else(|| "0 B/s".to_string());

                // Colored only while rising: the counts are since the
                // interface came up and keep incidents long over
                let errors_rising = rate.is_some_and(|r| r.errors_per_sec > 0.0);
                let drops_rising = rate.is_some_and(|r| r.dropped_per_sec > 0.0);
                let is_selected = idx == self.selected;

                // Row style based on errors and selection
                let row_style = if errors_rising || drops_rising {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                // Error column styles
                let rx_err_style = if errors_rising && dev.rx_errors > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let tx_err_style = if errors_rising && dev.tx_errors > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let rx_drop_style = if drops_rising && dev.rx_dropped > 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let tx_drop_style = if drops_rising && dev.tx_dropped > 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                Row::new([
                    Cell::from(dev.name.clone()).style(row_style),
                    Cell::from(rx_rate).style(Style::default().fg(Color::Green)),
                    Cell::from(tx_rate).style(Style::default().fg(Color::Blue)),
                    Cell::from(short_count(dev.rx_errors)).style(rx_err_style),
                    Cell::from(short_count(dev.tx_errors)).style(tx_err_style),
                    Cell::from(short_count(dev.rx_dropped)).style(rx_drop_style),
                    Cell::from(short_count(dev.tx_dropped)).style(tx_drop_style),
                ])
            })
            .collect();

        let widths = [
            Constraint::Length(14), // INTERFACE
            Constraint::Length(12), // RX RATE
            Constraint::Length(12), // TX RATE
            Constraint::Length(8),  // RX ERR
            Constraint::Length(8),  // TX ERR
            Constraint::Length(8),  // RX DROP
            Constraint::Length(8),  // TX DROP
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

        frame.render_stateful_widget(table, area, &mut self.table_state);
        self.hits.device_rows =
            crate::ui_ext::RowHits::new(area, false, 2, self.table_state.offset(), row_count);
    }

    /// Draw the detail section for selected device
    fn draw_detail_section(&self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };

        let Some(dev) = data.devices.get(self.selected) else {
            return;
        };

        let rate = data.rates.get(&dev.name);
        let rx_rate = rate
            .map(|r| NetDevStats::format_rate(r.rx_bytes_per_sec))
            .unwrap_or_else(|| "0 B/s".to_string());
        let tx_rate = rate
            .map(|r| NetDevStats::format_rate(r.tx_bytes_per_sec))
            .unwrap_or_else(|| "0 B/s".to_string());

        let rx_total = NetDevStats::format_bytes(dev.rx_bytes);
        let tx_total = NetDevStats::format_bytes(dev.tx_bytes);

        let errors_rising = rate.is_some_and(|r| r.errors_per_sec > 0.0);
        let drops_rising = rate.is_some_and(|r| r.dropped_per_sec > 0.0);
        let has_errors = errors_rising || drops_rising;
        let border_style = if has_errors {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "RX: ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{rx_total} total")),
                Span::styled(
                    format!(" ({rx_rate})"),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("    Packets: {}M", dev.rx_packets / 1_000_000)),
                Span::raw("    "),
                Span::styled(
                    format!("Errors: {}", dev.rx_errors),
                    if errors_rising && dev.rx_errors > 0 {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw("    "),
                Span::styled(
                    format!("Dropped: {}", dev.rx_dropped),
                    if drops_rising && dev.rx_dropped > 0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "TX: ",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{tx_total} total")),
                Span::styled(
                    format!(" ({tx_rate})"),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("    Packets: {}M", dev.tx_packets / 1_000_000)),
                Span::raw("    "),
                Span::styled(
                    format!("Errors: {}", dev.tx_errors),
                    if errors_rising && dev.tx_errors > 0 {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw("    "),
                Span::styled(
                    format!("Dropped: {}", dev.tx_dropped),
                    if drops_rising && dev.tx_dropped > 0 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ]),
        ];

        // Add connection summary line if we have connection data
        if !data.connections.is_empty() {
            let cc = &data.conn_counts;
            lines.push(Line::from(vec![
                Span::styled(
                    "Connections: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{} ", cc.established),
                    Style::default().fg(Color::Green),
                ),
                Span::styled("EST  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} ", cc.listen), Style::default().fg(Color::Cyan)),
                Span::styled("LISTEN  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} ", cc.time_wait),
                    if cc.time_wait > 100 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled("TIME_WAIT  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} ", cc.close_wait),
                    if cc.close_wait > 0 {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled("CLOSE_WAIT", Style::default().fg(Color::DarkGray)),
            ]));
        }

        // A rise now is worth a warning; old totals only a note
        if has_errors {
            let per_sec = rate.map_or(0.0, |r| r.errors_per_sec + r.dropped_per_sec);
            lines.push(Line::from(vec![
                Span::styled(
                    "! ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "Errors or drops rising now ({}): check load, MTU, cable/driver",
                        per_second(per_sec)
                    ),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        } else if dev.has_errors() && rate.is_some() {
            lines.push(Line::from(Span::styled(
                "Errors and drops are totals since the interface came up; none since the last refresh",
                Style::default().fg(Color::DarkGray),
            )));
        }

        // Clone name before moving data
        let dev_name = dev.name.clone();

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border_style)
            .title(Span::styled(
                format!(" {dev_name} "),
                Style::default().add_modifier(Modifier::BOLD),
            ));

        let detail = Paragraph::new(lines).block(block);
        frame.render_widget(detail, area);
    }

    /// Draw the footer with keybindings (interfaces view)
    pub(super) fn interfaces_key_hints(&self) -> Line<'static> {
        let auto_label = if self.auto_refresh {
            "auto:ON"
        } else {
            "auto:OFF"
        };

        // BPF filter indicator with color coding
        let (bpf_label, bpf_color) = if self.capture.use_bpf_filter {
            ("BPF:ON", Color::Green)
        } else {
            ("BPF:OFF", Color::Yellow)
        };

        let spans = vec![
            Span::styled("[Enter]", Style::default().fg(Color::Cyan)),
            Span::raw(" conns  "),
            Span::styled("[c]", Style::default().fg(Color::Cyan)),
            Span::raw(" capture  "),
            Span::styled("[f]", Style::default().fg(Color::Cyan)),
            Span::raw(" "),
            Span::styled(bpf_label, Style::default().fg(bpf_color)),
            Span::raw("  "),
            Span::styled("[a]", Style::default().fg(Color::Cyan)),
            Span::raw(format!(" {auto_label}")),
        ];

        Line::from(spans)
    }

    /// Handle key events in Interfaces view
    pub(super) fn handle_interfaces_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            KeyCode::Enter => {
                let has_connections = self
                    .data()
                    .map(|d| !d.connections.is_empty())
                    .unwrap_or(false);
                if has_connections {
                    self.enter_connections_view();
                }
                Ok(None)
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_device(crate::ui_ext::Move::Next);
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_device(crate::ui_ext::Move::Prev);
                Ok(None)
            }
            KeyCode::Char('g') => {
                self.move_device(crate::ui_ext::Move::First);
                Ok(None)
            }
            KeyCode::Char('G') => {
                self.move_device(crate::ui_ext::Move::Last);
                Ok(None)
            }
            KeyCode::Char('1') => {
                self.sort_by = SortBy::Traffic;
                self.sort_devices();
                Ok(None)
            }
            KeyCode::Char('2') => {
                self.sort_by = SortBy::Errors;
                self.sort_devices();
                Ok(None)
            }
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
                Ok(None)
            }
            // Packet capture (uses BPF filter to exclude port 50000, preventing feedback loop)
            KeyCode::Char('c') => {
                self.toggle_capture();
                Ok(None)
            }
            KeyCode::Char('s') => {
                self.save_capture();
                Ok(None)
            }
            // Toggle BPF filter for packet capture
            KeyCode::Char('f') => {
                self.toggle_bpf_filter();
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Draw the interfaces view (main view)
    pub(super) fn draw_interfaces_view(&mut self, frame: &mut Frame, area: Rect) {
        // Build constraints dynamically based on what we need to show
        let (counters_rising, conn_counts_has_warnings, connections_empty) = self
            .data()
            .map(|d| {
                (
                    d.errors_per_sec > 0.0 || d.dropped_per_sec > 0.0,
                    conn_warnings(&d.conn_counts),
                    d.connections.is_empty(),
                )
            })
            .unwrap_or((false, false, true));
        let has_warning = counters_rising || conn_counts_has_warnings;
        let has_connections = !connections_empty;
        let is_capturing = self.is_capturing();

        let mut constraints = vec![
            Constraint::Length(1), // Header
            Constraint::Length(1), // Traffic summary bar
        ];

        if is_capturing {
            constraints.push(Constraint::Length(1)); // Capture status bar
        }

        if has_connections {
            constraints.push(Constraint::Length(1)); // Connection summary bar
            constraints.push(Constraint::Length(1)); // Service health indicators
        }

        if has_warning {
            constraints.push(Constraint::Length(1)); // Warning
        }

        constraints.push(Constraint::Min(5)); // Device table (takes remaining space)
        constraints.push(Constraint::Length(4)); // Detail section

        // One area per constraint pushed above, taken in the same order
        let chunks = Layout::vertical(constraints).split(area);
        let mut areas = chunks.iter().copied();
        let mut next_area = || areas.next().unwrap_or_default();

        // Header
        self.draw_header(frame, next_area());

        // Traffic summary
        self.draw_summary_bar(frame, next_area());

        // Capture status bar (when capturing)
        if is_capturing {
            self.draw_capture_status(frame, next_area());
        }

        // Connection data (if available)
        if has_connections {
            self.draw_connection_summary(frame, next_area());
            self.draw_service_health(frame, next_area());
        }

        // Warning (if any)
        if has_warning {
            self.draw_warning(frame, next_area());
        }

        // Device table
        self.draw_device_table(frame, next_area());

        // Detail section
        self.draw_detail_section(frame, next_area());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_counts_fit_their_cells() {
        assert_eq!(short_count(147_301), "147301");
        assert_eq!(short_count(201_500_207), "201.5M");
        assert_eq!(short_count(4_200_000_000), "4.2G");
    }

    #[test]
    fn removed_interfaces_leave_the_totals() {
        let dev = |name: &str, rx: u64| NetDevStats {
            name: name.to_string(),
            rx_bytes: rx,
            rx_packets: 0,
            rx_errors: 0,
            rx_dropped: 0,
            tx_bytes: 0,
            tx_packets: 0,
            tx_errors: 0,
            tx_dropped: 0,
        };
        let mut net = NetworkStatsComponent::new("n".into(), "10.0.0.1".into());
        net.state.set_data(NetworkData::default());
        let sample = |net: &mut NetworkStatsComponent, devices| {
            net.data_mut().unwrap().last_sample =
                Some(Instant::now() - std::time::Duration::from_secs(1));
            net.update_devices(devices);
        };
        sample(&mut net, vec![dev("eth0", 0), dev("veth1", 0)]);
        sample(&mut net, vec![dev("eth0", 1000), dev("veth1", 1000)]);
        assert!(net.data().unwrap().total_rx_rate > 1500);
        // veth1 is gone: only eth0 counts
        sample(&mut net, vec![dev("eth0", 2000)]);
        let data = net.data().unwrap();
        assert_eq!(data.rates.len(), 1);
        assert!(data.total_rx_rate < 1100, "{}", data.total_rx_rate);
    }
}
