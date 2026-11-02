//! KubeSpan view: peer status fetched via talosctl

use super::NetworkStatsComponent;
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
use talos_rs::{get_kubespan_peers_for_node, is_kubespan_enabled_for_node};
use talos9s_core::format_bytes;

/// WireGuard renews a session's handshake every 2 minutes while traffic
/// flows; well past that the peer has gone quiet
const HANDSHAKE_STALE_SECS: i64 = 180;

/// Seconds since a KubeSpan peer's `lastHandshakeTime`; `None` when there
/// was none (Go's zero time) or it can't be read
fn handshake_age(time: &str, now: i64) -> Option<i64> {
    let t = chrono::DateTime::parse_from_rfc3339(time).ok()?.timestamp();
    (t > 0).then(|| (now - t).max(0))
}

/// KubeSpan peers are fetched via talosctl, so refresh them less often
const KUBESPAN_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

impl NetworkStatsComponent {
    /// Apply a finished KubeSpan fetch and start the next one when due.
    ///
    /// KubeSpan data comes from `talosctl get` (two subprocesses), far slower
    /// than the gRPC stats, so it runs in a spawned task on its own cadence
    /// instead of holding up every 2s refresh (which blocks the event loop).
    pub(super) fn poll_kubespan_data(&mut self) {
        if self
            .kubespan_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(task) = self.kubespan_task.take()
            && let Some(Ok((enabled, peers))) = crate::task::poll_finished(task)
        {
            if let Some(data) = self.data_mut() {
                data.kubespan_enabled = enabled;
                match peers {
                    Ok(peers) => {
                        data.kubespan_peers = peers;
                        data.kubespan_error = None;
                    }
                    // A failed read (e.g. talosctl timing out) says nothing
                    // about the peers, so keep the last ones we saw
                    Err(e) => data.kubespan_error = Some(e),
                }
            }

            // Reset selection if needed
            let peer_count = self.data().map(|d| d.kubespan_peers.len()).unwrap_or(0);
            if peer_count > 0 && self.kubespan_selected >= peer_count {
                self.kubespan_selected = 0;
            }
            self.kubespan_table_state
                .select(Some(self.kubespan_selected));
        }

        let due = self
            .kubespan_started
            .is_none_or(|at| at.elapsed() >= KUBESPAN_REFRESH_INTERVAL);
        if self.kubespan_task.is_some() || !due {
            return;
        }
        self.kubespan_started = Some(std::time::Instant::now());

        let node = self.address.clone();
        let context = self.talos_context.clone();
        let config_path = self.config_path.clone();
        self.kubespan_task = Some(tokio::spawn(async move {
            let context = context.as_deref();
            let config_path = config_path.as_deref();
            if !is_kubespan_enabled_for_node(context, &node, config_path).await {
                return (Some(false), Ok(Vec::new()));
            }
            let peers = get_kubespan_peers_for_node(context, &node, config_path)
                .await
                .map_err(|e| e.to_string());
            (Some(true), peers)
        }));
    }

    /// Draw the KubeSpan peer status view
    pub(super) fn draw_kubespan_view(&mut self, frame: &mut Frame, area: Rect) {
        self.hits.peer_rows = Default::default();
        let chunks = Layout::vertical([
            Constraint::Length(1), // Caption
            Constraint::Min(5),    // Content
        ])
        .split(area);

        frame.render_widget(
            Paragraph::new(" WireGuard peers of this node, from its KubeSpanPeerStatus resources")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[0],
        );

        // Check if KubeSpan is enabled
        let kubespan_enabled = self.data().and_then(|d| d.kubespan_enabled);
        let kubespan_peers_empty = self
            .data()
            .map(|d| d.kubespan_peers.is_empty())
            .unwrap_or(true);
        match kubespan_enabled {
            Some(false) => {
                // KubeSpan not enabled
                self.draw_kubespan_disabled(frame, chunks[1]);
            }
            Some(true) if kubespan_peers_empty => {
                // Enabled but no peers
                self.draw_kubespan_no_peers(frame, chunks[1]);
            }
            Some(true) => {
                // Draw peer table and detail
                self.draw_kubespan_peers(frame, chunks[1]);
            }
            None => {
                // Loading/checking status
                self.draw_kubespan_loading(frame, chunks[1]);
            }
        }
    }

    /// Draw KubeSpan disabled message
    fn draw_kubespan_disabled(&self, frame: &mut Frame, area: Rect) {
        let content_chunks = Layout::vertical([
            Constraint::Length(2), // Header
            Constraint::Min(5),    // Info section
        ])
        .split(area);

        let header = Paragraph::new(format!(" KubeSpan │ {} ({})", self.hostname, self.address))
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(header, content_chunks[0]);

        let info_lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  KubeSpan Status: ", Style::default().fg(Color::Yellow)),
                Span::styled("Not Enabled", Style::default().fg(Color::Gray)),
            ]),
            Line::from(""),
            Line::from(vec![Span::raw(
                "  KubeSpan provides encrypted WireGuard tunnels between cluster nodes.",
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  To enable KubeSpan:",
                Style::default().fg(Color::Cyan),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "    machine:",
                Style::default().fg(Color::White),
            )]),
            Line::from(vec![Span::styled(
                "      network:",
                Style::default().fg(Color::White),
            )]),
            Line::from(vec![Span::styled(
                "        kubespan:",
                Style::default().fg(Color::White),
            )]),
            Line::from(vec![Span::styled(
                "          enabled: true",
                Style::default().fg(Color::Green),
            )]),
        ];

        let info = Paragraph::new(info_lines).block(Block::default().borders(Borders::NONE));
        frame.render_widget(info, content_chunks[1]);
    }

    /// Draw KubeSpan no peers message
    fn draw_kubespan_no_peers(&self, frame: &mut Frame, area: Rect) {
        let content_chunks = Layout::vertical([
            Constraint::Length(2), // Header
            Constraint::Min(5),    // Info section
        ])
        .split(area);

        let header = Paragraph::new(format!(" KubeSpan │ {} ({})", self.hostname, self.address))
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(header, content_chunks[0]);

        let error = self.data().and_then(|d| d.kubespan_error.clone());
        let info_lines = match error {
            Some(error) => vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  KubeSpan Status: ", Style::default().fg(Color::Yellow)),
                    Span::styled("Enabled", Style::default().fg(Color::Green)),
                    Span::raw(" │ "),
                    Span::styled("Could not read peers", Style::default().fg(Color::Yellow)),
                ]),
                Line::from(""),
                Line::from(vec![Span::raw(format!("  {error}"))]),
                Line::from(vec![Span::styled(
                    format!("  Retrying every {}s", KUBESPAN_REFRESH_INTERVAL.as_secs()),
                    Style::default().fg(Color::DarkGray),
                )]),
            ],
            None => vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  KubeSpan Status: ", Style::default().fg(Color::Yellow)),
                    Span::styled("Enabled", Style::default().fg(Color::Green)),
                    Span::raw(" │ "),
                    Span::styled("No peers connected", Style::default().fg(Color::Gray)),
                ]),
                Line::from(""),
                Line::from(vec![Span::raw(
                    "  Waiting for other nodes to establish KubeSpan connections.",
                )]),
            ],
        };

        let info = Paragraph::new(info_lines).block(Block::default().borders(Borders::NONE));
        frame.render_widget(info, content_chunks[1]);
    }

    /// Draw KubeSpan loading message
    fn draw_kubespan_loading(&self, frame: &mut Frame, area: Rect) {
        let content_chunks = Layout::vertical([
            Constraint::Length(2), // Header
            Constraint::Min(5),    // Info section
        ])
        .split(area);

        let header = Paragraph::new(format!(" KubeSpan │ {} ({})", self.hostname, self.address))
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(header, content_chunks[0]);

        let info_lines = vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Checking KubeSpan status...",
                Style::default().fg(Color::Yellow),
            )]),
        ];

        let info = Paragraph::new(info_lines).block(Block::default().borders(Borders::NONE));
        frame.render_widget(info, content_chunks[1]);
    }

    /// Draw KubeSpan peers table and detail
    fn draw_kubespan_peers(&mut self, frame: &mut Frame, area: Rect) {
        let content_chunks = Layout::vertical([
            Constraint::Length(2), // Header with summary
            Constraint::Min(8),    // Peer table
            Constraint::Length(7), // Detail section
        ])
        .split(area);

        // Get peer data
        let kubespan_peers = self
            .data()
            .map(|d| d.kubespan_peers.clone())
            .unwrap_or_default();

        // Count connected peers
        let connected = kubespan_peers.iter().filter(|p| p.state == "up").count();
        let total = kubespan_peers.len();

        // Header with summary
        let header = Line::from(vec![
            Span::styled(
                format!(" KubeSpan │ {} ({}) │ ", self.hostname, self.address),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{connected}/{total} peers connected"),
                if connected == total {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Yellow)
                },
            ),
        ]);
        frame.render_widget(Paragraph::new(header), content_chunks[0]);

        // Peer table
        let header_row = Row::new(vec![
            Cell::from("HOSTNAME").style(Style::default().fg(Color::DarkGray)),
            Cell::from("ENDPOINT").style(Style::default().fg(Color::DarkGray)),
            Cell::from("HANDSHAKE").style(Style::default().fg(Color::DarkGray)),
            Cell::from("STATE").style(Style::default().fg(Color::DarkGray)),
            Cell::from("RX").style(Style::default().fg(Color::DarkGray)),
            Cell::from("TX").style(Style::default().fg(Color::DarkGray)),
        ]);

        let rows: Vec<Row> = kubespan_peers
            .iter()
            .enumerate()
            .map(|(i, peer)| {
                let is_selected = i == self.kubespan_selected;
                let style = if is_selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                let state_style = match peer.state.as_str() {
                    "up" => Style::default().fg(Color::Green),
                    "down" => Style::default().fg(Color::Red),
                    _ => Style::default().fg(Color::Yellow),
                };

                let now = chrono::Utc::now().timestamp();
                let (handshake, handshake_style) = match peer
                    .last_handshake
                    .as_deref()
                    .and_then(|t| handshake_age(t, now))
                {
                    Some(age) => (
                        format!(
                            "{} ago",
                            talos9s_core::format_secs_human(age.unsigned_abs())
                        ),
                        if age > HANDSHAKE_STALE_SECS {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default()
                        },
                    ),
                    None => ("--".to_string(), Style::default()),
                };
                let endpoint_str = peer.endpoint.clone().unwrap_or_else(|| "--".to_string());

                Row::new(vec![
                    Cell::from(peer.label.clone()).style(style),
                    Cell::from(endpoint_str).style(style),
                    Cell::from(handshake).style(handshake_style.patch(style)),
                    Cell::from(peer.state.clone()).style(state_style.patch(style)),
                    Cell::from(format_bytes(peer.rx_bytes)).style(style),
                    Cell::from(format_bytes(peer.tx_bytes)).style(style),
                ])
            })
            .collect();

        let row_count = rows.len();
        let table = Table::new(
            rows,
            [
                Constraint::Min(20),    // Hostname
                Constraint::Length(22), // Endpoint
                Constraint::Length(10), // Handshake
                Constraint::Length(10), // State
                Constraint::Length(10), // RX
                Constraint::Length(10), // TX
            ],
        )
        .header(header_row)
        .block(Block::default().borders(Borders::TOP));

        frame.render_stateful_widget(table, content_chunks[1], &mut self.kubespan_table_state);
        // Top border and the header come before the first row
        self.hits.peer_rows = crate::ui_ext::RowHits::new(
            content_chunks[1],
            false,
            2,
            self.kubespan_table_state.offset(),
            row_count,
        );

        // Detail section for selected peer
        if let Some(peer) = kubespan_peers.get(self.kubespan_selected) {
            let detail_lines = vec![
                Line::from(vec![
                    Span::styled(" Selected: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&peer.label, Style::default().fg(Color::Cyan)),
                ]),
                Line::from(vec![
                    Span::styled("   ID: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&peer.id),
                ]),
                Line::from(vec![
                    Span::styled("   Endpoint: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(peer.endpoint.as_deref().unwrap_or("--")),
                ]),
                Line::from(vec![
                    Span::styled("   Last Handshake: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(peer.last_handshake.as_deref().unwrap_or("--")),
                ]),
                Line::from(vec![
                    Span::styled("   Transfer: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(format!(
                        "RX {} / TX {}",
                        format_bytes(peer.rx_bytes),
                        format_bytes(peer.tx_bytes)
                    )),
                ]),
            ];

            let detail = Paragraph::new(detail_lines).block(Block::default().borders(Borders::TOP));
            frame.render_widget(detail, content_chunks[2]);
        }
    }

    /// Handle key events in KubeSpan view
    pub(super) fn handle_kubespan_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        let peer_count = self.data().map(|d| d.kubespan_peers.len()).unwrap_or(0);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(Some(Action::Back)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),
            KeyCode::Char('j') | KeyCode::Down => {
                if peer_count > 0 {
                    self.kubespan_selected = (self.kubespan_selected + 1) % peer_count;
                    self.kubespan_table_state
                        .select(Some(self.kubespan_selected));
                }
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if peer_count > 0 {
                    self.kubespan_selected = if self.kubespan_selected == 0 {
                        peer_count - 1
                    } else {
                        self.kubespan_selected - 1
                    };
                    self.kubespan_table_state
                        .select(Some(self.kubespan_selected));
                }
                Ok(None)
            }
            KeyCode::Char('g') => {
                if peer_count > 0 {
                    self.kubespan_selected = 0;
                    self.kubespan_table_state.select(Some(0));
                }
                Ok(None)
            }
            KeyCode::Char('G') => {
                if peer_count > 0 {
                    self.kubespan_selected = peer_count - 1;
                    self.kubespan_table_state
                        .select(Some(self.kubespan_selected));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubespan_handshake_age() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-01T20:35:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(
            handshake_age("2026-11-01T20:33:22.402498694Z", now),
            Some(98)
        );
        // Never: Go's zero time
        assert_eq!(handshake_age("0001-01-01T00:00:00Z", now), None);
        assert_eq!(handshake_age("garbage", now), None);
    }
}
