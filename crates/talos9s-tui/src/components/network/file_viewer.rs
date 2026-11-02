//! File viewer overlay: DNS config and routing table read from the node

use super::{FileViewerOverlay, FileViewerType, NetworkStatsComponent, with_timeout};
use crate::action::Action;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use talos_rs::TalosError;

impl NetworkStatsComponent {
    /// Check if file viewer needs content fetched
    pub fn file_viewer_needs_fetch(&self) -> bool {
        self.file_viewer
            .as_ref()
            .map(|v| v.lines.len() == 1 && v.lines[0] == "Loading...")
            .unwrap_or(false)
    }

    /// Handle key events when file viewer is open
    pub(super) fn handle_file_viewer_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            // Close the viewer
            KeyCode::Esc | KeyCode::Char('q') => {
                self.file_viewer = None;
                Ok(None)
            }
            // Scroll up
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = viewer.scroll.saturating_sub(1);
                }
                Ok(None)
            }
            // Scroll down
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(ref mut viewer) = self.file_viewer {
                    let max_scroll = viewer.lines.len().saturating_sub(10);
                    viewer.scroll = (viewer.scroll + 1).min(max_scroll);
                }
                Ok(None)
            }
            // Page up / Ctrl+U (half page)
            KeyCode::PageUp => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = viewer.scroll.saturating_sub(10);
                }
                Ok(None)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = viewer.scroll.saturating_sub(10);
                }
                Ok(None)
            }
            // Page down / Ctrl+D (half page)
            KeyCode::PageDown => {
                if let Some(ref mut viewer) = self.file_viewer {
                    let max_scroll = viewer.lines.len().saturating_sub(10);
                    viewer.scroll = (viewer.scroll + 10).min(max_scroll);
                }
                Ok(None)
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ref mut viewer) = self.file_viewer {
                    let max_scroll = viewer.lines.len().saturating_sub(10);
                    viewer.scroll = (viewer.scroll + 10).min(max_scroll);
                }
                Ok(None)
            }
            // Go to top
            KeyCode::Char('g') => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = 0;
                }
                Ok(None)
            }
            // Go to bottom
            KeyCode::Char('G') => {
                if let Some(ref mut viewer) = self.file_viewer {
                    viewer.scroll = viewer.lines.len().saturating_sub(10);
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Show DNS configuration overlay
    pub(super) fn show_dns_config(&mut self) {
        // Request file fetch via action - actual fetch happens in refresh
        self.file_viewer = Some(FileViewerOverlay {
            viewer_type: FileViewerType::DnsConfig,
            title: "DNS Configuration (/etc/resolv.conf)".to_string(),
            lines: vec!["Loading...".to_string()],
            scroll: 0,
        });
    }

    /// Show routing table overlay
    pub(super) fn show_routing_table(&mut self) {
        self.file_viewer = Some(FileViewerOverlay {
            viewer_type: FileViewerType::RoutingTable,
            title: "Routing Table (/proc/net/route)".to_string(),
            lines: vec!["Loading...".to_string()],
            scroll: 0,
        });
    }

    /// Fetch file content for the file viewer in the background
    pub(super) fn fetch_file_content(&mut self) {
        let Some(viewer_type) = self.file_viewer.as_ref().map(|v| v.viewer_type.clone()) else {
            return;
        };
        let path = match viewer_type {
            FileViewerType::DnsConfig => "/etc/resolv.conf",
            FileViewerType::RoutingTable => "/proc/net/route",
        };

        let Some(client) = self.client.clone() else {
            if let Some(ref mut viewer) = self.file_viewer {
                viewer.lines = vec!["Error: No client configured".to_string()];
            }
            return;
        };
        if self.tasks.file.is_some() {
            return;
        }

        self.tasks.file = Some(tokio::spawn(async move {
            let result = with_timeout(client.read_file(path)).await;
            (viewer_type, path, result)
        }));
    }

    /// Show fetched file content in the file viewer
    pub(super) fn apply_file_content(
        &mut self,
        viewer_type: FileViewerType,
        path: &str,
        result: std::result::Result<String, TalosError>,
    ) {
        let lines = match result {
            Ok(content) => match viewer_type {
                FileViewerType::DnsConfig => talos9s_core::network::resolv_conf_lines(&content),
                FileViewerType::RoutingTable => talos9s_core::network::route_table_lines(&content),
            },
            Err(e) => vec![format!("Error reading {}: {}", path, e)],
        };

        if let Some(ref mut viewer) = self.file_viewer
            && viewer.viewer_type == viewer_type
        {
            viewer.lines = lines;
        }
    }

    /// Draw the file viewer overlay
    pub(super) fn draw_file_viewer(&self, frame: &mut Frame, area: Rect) {
        let Some(ref viewer) = self.file_viewer else {
            return;
        };

        // Create a centered overlay
        let overlay_width = (area.width * 4 / 5).min(100);
        let overlay_height = (area.height * 3 / 4).min(30);
        let overlay_area = crate::ui_ext::centered(area, overlay_width, overlay_height);

        // Clear the area
        frame.render_widget(Clear, overlay_area);

        // Draw the block
        let block = Block::default()
            .title(Span::styled(
                format!(" {} ", viewer.title),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black));

        let inner = block.inner(overlay_area);
        frame.render_widget(block, overlay_area);

        // Build visible lines
        let visible_height = inner.height.saturating_sub(2) as usize; // Leave room for footer
        let visible_lines: Vec<Line> = viewer
            .lines
            .iter()
            .skip(viewer.scroll)
            .take(visible_height)
            .map(|l| {
                let style = if l.starts_with("─") {
                    Style::default().fg(Color::DarkGray)
                } else if l.starts_with("Error") {
                    Style::default().fg(Color::Red)
                } else if l.starts_with("  •") {
                    Style::default().fg(Color::Green)
                } else if l.contains(':') && !l.contains('.') {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(l.as_str(), style))
            })
            .collect();

        let content = Paragraph::new(visible_lines);
        frame.render_widget(
            content,
            Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(2),
            ),
        );

        // Draw footer
        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        let scroll_info = if viewer.lines.len() > visible_height {
            format!(
                "Lines {}-{} of {} | ",
                viewer.scroll + 1,
                (viewer.scroll + visible_height).min(viewer.lines.len()),
                viewer.lines.len()
            )
        } else {
            String::new()
        };

        let footer = Paragraph::new(Line::from(vec![
            Span::styled(&scroll_info, Style::default().fg(Color::DarkGray)),
            Span::styled("[j/k]", Style::default().fg(Color::Cyan)),
            Span::raw(" scroll "),
            Span::styled("[g/G]", Style::default().fg(Color::Cyan)),
            Span::raw(" top/bottom "),
            Span::styled("[q/Esc]", Style::default().fg(Color::Cyan)),
            Span::raw(" close"),
        ]));
        frame.render_widget(footer, footer_area);
    }
}
