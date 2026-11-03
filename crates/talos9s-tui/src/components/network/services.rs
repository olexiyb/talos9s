//! Service actions: health of the key ports, logs, and the output pane

use super::{NetworkStatsComponent, port_to_service};
use crate::action::Action;
use crate::components::LogNode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use talos_rs::ConnectionState;

impl NetworkStatsComponent {
    /// Update service health indicators based on connection data
    pub(super) fn update_service_health(&mut self) {
        let Some(data) = self.data_mut() else { return };
        data.service_health.clear();

        // Key ports to monitor for Kubernetes
        let key_ports: &[u16] = &[6443, 2379, 10250, 10259, 10257];

        for port in key_ports {
            // Check if port is listening
            let is_listening = data
                .connections
                .iter()
                .any(|c| c.local_port == *port as u32 && c.state == ConnectionState::Listen);
            data.service_health.insert(*port, is_listening);
        }
    }

    /// Open logs for the currently selected connection's service
    pub(super) fn open_service_logs(&self) -> Option<Action> {
        let conns = self.get_filtered_connections();
        let conn = conns.get(self.conn_selected)?;

        // Only for known service ports
        let service_name = port_to_service(conn.local_port)?;

        // Return action to show logs for this service
        let service_vec = vec![service_name.to_string()];
        Some(Action::ShowMultiLogs(
            self.address.clone(),
            String::new(), // role not needed for single service
            vec![LogNode {
                name: self.hostname.clone(),
                ip: self.address.clone(),
                services: Some(service_vec.clone()),
                color: None,
            }],
            service_vec.clone(),
            service_vec, // from network view we only know about this service
        ))
    }

    /// Draw the status message as a floating notification
    pub(super) fn draw_status_message(&self, frame: &mut Frame, area: Rect, message: &str) {
        // Determine the color based on message content
        let (fg_color, border_color) = if message.contains("Failed") || message.contains("Error") {
            (Color::White, Color::Red)
        } else if message.contains("successfully") {
            (Color::White, Color::Green)
        } else {
            (Color::Black, Color::Yellow)
        };

        // Create a floating notification centered near the top
        let msg_width = talos9s_core::convert::to_u16(message.len())
            .saturating_add(4)
            .min(area.width.saturating_sub(4));
        let msg_height = 3u16;
        let x = area.x + (area.width.saturating_sub(msg_width)) / 2;
        let y = area.y + 2; // Near the top, below header
        let msg_area = Rect::new(x, y, msg_width, msg_height);

        // Clear the area first
        frame.render_widget(Clear, msg_area);

        // Draw the notification box
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(border_color));

        let inner = block.inner(msg_area);

        let status = Paragraph::new(message)
            .style(Style::default().fg(fg_color).add_modifier(Modifier::BOLD))
            .alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(block, msg_area);
        frame.render_widget(status, inner);
    }

    /// Draw the command output pane at the bottom
    pub(super) fn draw_output_pane(&self, frame: &mut Frame, area: Rect) {
        let Some(ref output) = self.command_output else {
            return;
        };

        // Determine border color based on success/failure
        let border_color = if output.success {
            Color::Green
        } else {
            Color::Red
        };

        let title = format!(
            " {} {} ",
            if output.success { "✓" } else { "✗" },
            output.command
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title)
            .title_style(
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Build output lines
        let mut lines: Vec<Line> = output
            .lines
            .iter()
            .map(|l| {
                let style = if l.starts_with("Error:") {
                    Style::default().fg(Color::Red)
                } else if l.contains("successfully") || l.contains("initiated") {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::White)
                };
                Line::from(Span::styled(l.as_str(), style))
            })
            .collect();

        // Add footer hint
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press Esc to close",
            Style::default().fg(Color::DarkGray),
        )));

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
}
