//! Service actions: logs, restart with confirmation, and the output pane

use super::{CommandOutput, NetworkStatsComponent, PendingAction, port_to_service, with_timeout};
use crate::action::Action;
use crate::components::LogNode;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::time::Instant;
use talos_rs::{ConnectionState, ServiceRestartResult, TalosError};

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

    /// Handle key events when a confirmation is pending
    pub(super) fn handle_confirmation_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                // Confirmed - execute the action
                if let Some(action) = self.pending_action.take() {
                    return Some(self.execute_pending_action(action));
                }
                None
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                // Cancelled
                self.pending_action = None;
                None
            }
            _ => None,
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
            }],
            service_vec.clone(),
            service_vec, // from network view we only know about this service
        ))
    }

    /// Initiate service restart - sets pending_action for confirmation
    pub(super) fn initiate_service_restart(&mut self) {
        let conns = self.get_filtered_connections();
        let Some(conn) = conns.get(self.conn_selected) else {
            return;
        };

        // Only for known service ports
        let Some(service_name) = port_to_service(conn.local_port) else {
            return;
        };

        // Check if service exists
        let service_exists = self
            .data()
            .map(|d| d.services.contains_key(service_name))
            .unwrap_or(false);
        if !service_exists {
            self.status_message = Some((
                format!("Service '{service_name}' not found"),
                Instant::now(),
            ));
            return;
        }

        // Set pending action - will require confirmation
        self.pending_action = Some(PendingAction::RestartService(
            service_name.to_string(),
            service_name.to_string(),
        ));
    }

    /// Execute a pending action after confirmation
    fn execute_pending_action(&mut self, action: PendingAction) -> Action {
        match action {
            PendingAction::RestartService(service_id, service_name) => {
                // Show status message while waiting
                self.status_message =
                    Some((format!("Restarting {service_name}..."), Instant::now()));

                // Store that we need to restart - will be executed on next refresh
                self.pending_restart_service = Some(service_id);

                // Trigger immediate refresh to execute the restart
                Action::Refresh
            }
        }
    }

    /// Start the pending service restart in the background (from `refresh`)
    pub(super) fn perform_pending_restart(&mut self) {
        let Some(service_id) = self.pending_restart_service.take() else {
            return;
        };

        let Some(client) = self.client.clone() else {
            let output = CommandOutput {
                command: format!("Restart {service_id}"),
                lines: vec!["Error: No client configured".to_string()],
                timestamp: Instant::now(),
                success: false,
            };
            self.command_output = Some(output);
            self.show_output_pane = true;
            return;
        };

        self.status_message = Some((format!("Restarting {service_id}..."), Instant::now()));
        crate::audit::audit_start("SERVICE_RESTART", &self.hostname, &service_id);
        self.tasks.restart = Some(tokio::spawn(async move {
            let result = with_timeout(client.service_restart(&service_id)).await;
            (service_id, result)
        }));
    }

    /// Show the outcome of a finished service restart
    pub(super) fn apply_restart(
        &mut self,
        service_id: &str,
        result: std::result::Result<Vec<ServiceRestartResult>, TalosError>,
    ) {
        match result {
            Ok(results) => {
                crate::audit::audit_success("SERVICE_RESTART", &self.hostname, service_id);
                let mut lines = Vec::new();
                lines.push(format!("Restarting service: {service_id}"));
                lines.push(String::new());

                if let Some(result) = results.first() {
                    if !result.response.is_empty() {
                        lines.push(format!("Response: {}", result.response));
                    }
                    // Show metadata if available
                    if !result.node.is_empty() {
                        lines.push(format!("Node: {}", result.node));
                    }
                }

                lines.push(String::new());
                lines.push("Service restart initiated successfully.".to_string());
                lines.push("The service will restart momentarily.".to_string());

                let output = CommandOutput {
                    command: format!("Restart {service_id}"),
                    lines,
                    timestamp: Instant::now(),
                    success: true,
                };
                self.command_output = Some(output);
                self.show_output_pane = true;
                self.status_message = None; // Clear status message, using output pane instead
            }
            Err(e) => {
                crate::audit::audit_failure(
                    "SERVICE_RESTART",
                    &self.hostname,
                    &format!("{service_id}: {e}"),
                );
                let lines = vec![
                    format!("Restarting service: {}", service_id),
                    String::new(),
                    format!("Error: {}", e),
                    String::new(),
                    "The service restart failed. Check the error above.".to_string(),
                ];

                let output = CommandOutput {
                    command: format!("Restart {service_id}"),
                    lines,
                    timestamp: Instant::now(),
                    success: false,
                };
                self.command_output = Some(output);
                self.show_output_pane = true;
                self.status_message = None;
            }
        }
    }

    /// Draw the confirmation dialog
    pub(super) fn draw_confirmation_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        action: &PendingAction,
    ) {
        let (title, message) = match action {
            PendingAction::RestartService(_, name) => {
                ("Confirm Restart", format!("Restart service '{name}'?"))
            }
        };

        // Center the dialog
        let dialog_width = 40u16;
        let dialog_height = 5u16;
        let dialog_area = crate::ui_ext::centered(area, dialog_width, dialog_height);

        // Clear the area first (removes any text underneath)
        frame.render_widget(Clear, dialog_area);

        // Draw the dialog block with background
        let block = Block::default()
            .title(Span::styled(
                format!(" {title} "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .style(Style::default().bg(Color::Black));

        let inner = block.inner(dialog_area);

        let text = vec![
            Line::from(message),
            Line::from(""),
            Line::from(vec![
                Span::styled("[y]", Style::default().fg(Color::Green)),
                Span::raw(" Yes  "),
                Span::styled("[n/Esc]", Style::default().fg(Color::Red)),
                Span::raw(" No"),
            ]),
        ];

        let paragraph = Paragraph::new(text)
            .alignment(ratatui::layout::Alignment::Center)
            .style(Style::default().bg(Color::Black));

        frame.render_widget(block, dialog_area);
        frame.render_widget(paragraph, inner);
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
