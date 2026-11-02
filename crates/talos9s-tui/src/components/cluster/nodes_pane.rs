//! The nodes pane: the node list and the navigation menu

use super::*;

impl ClusterComponent {
    /// Draw the nodes pane (left column) with navigation menu below
    /// Lines of the node list and the item each line selects. Discovery
    /// warnings wrap to `warning_width`, and are left out without one (when
    /// measuring how wide the list needs to be).
    pub(super) fn node_list_lines(
        &self,
        warning_width: Option<u16>,
    ) -> (Vec<Line<'_>>, Vec<Option<NodeListItem>>) {
        // Build multi-cluster accordion-style node list
        let mut lines = Vec::new();
        let mut row_items: Vec<Option<NodeListItem>> = Vec::new();
        // Record which item the line just pushed belongs to
        let mut mark = |lines: &Vec<Line>, item: NodeListItem| {
            row_items.resize(lines.len() - 1, None);
            row_items.push(Some(item));
        };
        let nodes_focused = self.focused_pane == FocusedPane::Nodes;

        for (cluster_idx, cluster) in self.clusters.iter().enumerate() {
            // Cluster header
            let is_cluster_selected =
                self.selected_item == NodeListItem::ClusterHeader(cluster_idx);
            let expand_icon = if cluster.expanded { "▼" } else { "▶" };
            let selector = if is_cluster_selected && nodes_focused {
                "▸"
            } else {
                " "
            };

            // Status indicator
            let (status_symbol, status_color) = if cluster.loading {
                ("◌", Color::Yellow)
            } else if cluster.connected {
                ("●", Color::Green)
            } else {
                ("○", Color::Red)
            };

            let header_style = if is_cluster_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Magenta)
            };

            let node_count = cluster.versions.len();

            // Build etcd status for this cluster
            let etcd_spans: Vec<Span> = if let Some(etcd) = &cluster.etcd_summary {
                let (indicator, color) = match (etcd.has_quorum, etcd.healthy) {
                    (Some(true), Some(h)) if h == etcd.total => ("●", Color::Green),
                    (Some(true), _) => ("◐", Color::Yellow),
                    (Some(false), _) => ("✗", Color::Red),
                    (None, _) => ("?", Color::DarkGray),
                };
                let healthy = etcd
                    .healthy
                    .map_or_else(|| "?".to_string(), |h| h.to_string());
                vec![
                    Span::styled("  etcd ", Style::default().dim()),
                    Span::styled(
                        format!("{}/{}", healthy, etcd.total),
                        Style::default().fg(color),
                    ),
                    Span::styled(indicator, Style::default().fg(color)),
                ]
            } else {
                vec![]
            };

            let mut cluster_line = vec![
                Span::styled(format!("{} {} ", selector, expand_icon), header_style),
                Span::styled(status_symbol, Style::default().fg(status_color)),
                Span::raw(" "),
                Span::styled(&cluster.name, header_style),
                Span::styled(
                    if cluster.loading {
                        " (…)".to_string()
                    } else {
                        format!(" ({})", node_count)
                    },
                    Style::default().dim(),
                ),
            ];
            if !cluster.offline.is_empty() {
                cluster_line.push(Span::styled(
                    format!(" {} offline", cluster.offline.len()),
                    Style::default().fg(Color::Red),
                ));
            }
            cluster_line.extend(etcd_spans);
            lines.push(Line::from(cluster_line));
            mark(&lines, NodeListItem::ClusterHeader(cluster_idx));

            if cluster.loading {
                lines.push(Line::from(Span::styled(
                    "    connecting…",
                    Style::default().fg(Color::Yellow).dim(),
                )));
            }

            // Show a connection error under the cluster header (always, even
            // when collapsed) so failures are visible instead of a blank list.
            if let Some(err) = &cluster.error {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled("⚠ ", Style::default().fg(Color::Red)),
                    Span::styled(err.as_str(), Style::default().fg(Color::Red)),
                ]));
            }

            // Skip if cluster is collapsed
            if !cluster.expanded {
                continue;
            }

            let cp_nodes = self.controlplane_nodes_for(cluster_idx);
            let worker_nodes = self.worker_nodes_for(cluster_idx);

            // Control Plane section
            if !cp_nodes.is_empty() {
                let is_selected =
                    self.selected_item == NodeListItem::ControlPlaneHeader(cluster_idx);
                let expand_icon = if cluster.controlplane_expanded {
                    "▼"
                } else {
                    "▶"
                };
                let selector = if is_selected && nodes_focused {
                    "▸"
                } else {
                    " "
                };
                let header_style = if is_selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Blue)
                };

                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{} {} ", selector, expand_icon), header_style),
                    Span::styled(format!("Control Plane ({})", cp_nodes.len()), header_style),
                ]));
                mark(&lines, NodeListItem::ControlPlaneHeader(cluster_idx));

                // Show control plane nodes if expanded
                if cluster.controlplane_expanded {
                    for (idx, (_, v)) in cp_nodes.iter().enumerate() {
                        let node_name = if v.node.is_empty() {
                            "node".to_string()
                        } else {
                            v.node.clone()
                        };
                        let is_node_selected =
                            self.selected_item == NodeListItem::ControlPlaneNode(cluster_idx, idx);

                        let (health_symbol, health_color, offline_note) =
                            self.node_row_status(cluster_idx, &v.node);

                        let selector = if is_node_selected && nodes_focused {
                            "▸"
                        } else {
                            " "
                        };
                        let name_style = if is_node_selected {
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };

                        lines.push(Line::from(vec![
                            Span::raw("     "),
                            Span::styled(
                                format!("{} {} ", selector, health_symbol),
                                Style::default().fg(health_color),
                            ),
                            Span::styled(node_name, name_style),
                            Span::styled(offline_note, Style::default().fg(Color::Red).dim()),
                        ]));
                        mark(&lines, NodeListItem::ControlPlaneNode(cluster_idx, idx));
                    }
                }
            }

            // Workers section
            if !worker_nodes.is_empty() {
                let is_selected = self.selected_item == NodeListItem::WorkersHeader(cluster_idx);
                let expand_icon = if cluster.workers_expanded {
                    "▼"
                } else {
                    "▶"
                };
                let selector = if is_selected && nodes_focused {
                    "▸"
                } else {
                    " "
                };
                let header_style = if is_selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Blue)
                };

                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{} {} ", selector, expand_icon), header_style),
                    Span::styled(format!("Workers ({})", worker_nodes.len()), header_style),
                ]));
                mark(&lines, NodeListItem::WorkersHeader(cluster_idx));

                // Show worker nodes if expanded
                if cluster.workers_expanded {
                    for (idx, (_, v)) in worker_nodes.iter().enumerate() {
                        let node_name = if v.node.is_empty() {
                            "node".to_string()
                        } else {
                            v.node.clone()
                        };
                        let is_node_selected =
                            self.selected_item == NodeListItem::WorkerNode(cluster_idx, idx);

                        let (health_symbol, health_color, offline_note) =
                            self.node_row_status(cluster_idx, &v.node);

                        let selector = if is_node_selected && nodes_focused {
                            "▸"
                        } else {
                            " "
                        };
                        let name_style = if is_node_selected {
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };

                        lines.push(Line::from(vec![
                            Span::raw("     "),
                            Span::styled(
                                format!("{} {} ", selector, health_symbol),
                                Style::default().fg(health_color),
                            ),
                            Span::styled(node_name, name_style),
                            Span::styled(offline_note, Style::default().fg(Color::Red).dim()),
                        ]));
                        mark(&lines, NodeListItem::WorkerNode(cluster_idx, idx));
                    }
                }
            }

            // Discovery warning: worker enumeration failed, so the list
            // above may be control-plane-only. Surface it instead of
            // silently hiding workers. Indented + wrapped so it reads as
            // part of this cluster's tray rather than a clipped stray line.
            if let Some(warning) = &cluster.discovery_warning
                && let Some(width) = warning_width
            {
                lines.extend(warning_lines(warning, width));
            }
        }
        (lines, row_items)
    }

    /// Columns the nodes pane needs to show its node list and the menu
    /// without cutting them: the widest line, borders and a space
    pub(super) fn nodes_pane_width(&self) -> u16 {
        let (lines, _) = self.node_list_lines(None);
        let list = lines.iter().map(Line::width).max().unwrap_or(0);
        // " m Machine config"
        let menu = NavMenuItem::ALL
            .iter()
            .map(|item| 1 + item.hotkey().len() + 1 + item.label().len())
            .max()
            .unwrap_or(0);
        (list.max(menu) + 3) as u16
    }

    pub(super) fn draw_nodes_pane(&mut self, frame: &mut Frame, area: Rect) {
        // Focus indication - cyan border when focused
        let focused = self.focused_pane == FocusedPane::Nodes && !self.view_focused;
        let border_color = if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let block = Block::default()
            .title(" Nodes ")
            .title_style(Style::default().fg(if focused { Color::Cyan } else { Color::White }))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Split inner area: nodes list at top, nav menu at bottom
        let menu_height = NavMenuItem::ALL.len() as u16 + 1; // +1 for separator
        let pane_layout = Layout::vertical([
            Constraint::Min(3),              // Nodes list
            Constraint::Length(menu_height), // Navigation menu (vertical)
        ])
        .split(inner);

        if self.clusters.is_empty() {
            let msg = Paragraph::new(Line::from(Span::styled(
                "  No clusters found",
                Style::default().dim(),
            )));
            frame.render_widget(msg, pane_layout[0]);
        } else {
            let (lines, row_items) = self.node_list_lines(Some(pane_layout[0].width));
            // A tree taller than the pane scrolls, keeping the selected row
            // in view; the rows the mouse can hit are the visible ones
            let list = pane_layout[0];
            let height = list.height as usize;
            let total = lines.len();
            // (`lines` borrow the tree, so the scroll is stored after drawing)
            let mut scroll = self.tree_scroll;
            if let Some(selected) = row_items
                .iter()
                .position(|item| item.as_ref() == Some(&self.selected_item))
            {
                if selected < scroll {
                    scroll = selected;
                } else if selected >= scroll + height {
                    scroll = selected + 1 - height;
                }
            }
            scroll = scroll.min(total.saturating_sub(height));
            frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), list);
            crate::ui_ext::draw_scrollbar(
                frame,
                Rect::new(area.x + area.width - 1, list.y, 1, list.height),
                total as u16,
                scroll as u16,
            );
            self.tree_scroll = scroll;
            let mut hits = self.hits.borrow_mut();
            hits.node_list = list;
            hits.node_rows = row_items.into_iter().skip(scroll).take(height).collect();
        }
        self.hits.borrow_mut().menu = pane_layout[1];

        // Draw navigation menu
        self.draw_nav_menu(frame, pane_layout[1]);
    }

    /// Draw the navigation menu (vertical list)
    pub(super) fn draw_nav_menu(&self, frame: &mut Frame, area: Rect) {
        // Dim while a view has the keyboard, even though this side is the one
        // Left returns to
        let menu_focused = self.focused_pane == FocusedPane::Menu && !self.view_focused;
        let mut lines = Vec::new();

        // Separator line with focus color
        let sep_color = if menu_focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let sep_text = if menu_focused {
            " Navigate ".to_string()
        } else {
            "─".repeat(area.width as usize)
        };
        lines.push(Line::from(Span::styled(
            sep_text,
            Style::default().fg(sep_color),
        )));

        // Menu items (1-indexed, 0 means not in menu)
        for (i, item) in NavMenuItem::ALL.iter().enumerate() {
            let menu_index = i + 1; // 1-based for selection
            let is_selected = menu_index == self.selected_menu_item;
            // The highlighted row is the selection: no marker column, so
            // the compact pane keeps room for the names
            let highlighted = is_selected && menu_focused;
            let bar = Style::default().fg(Color::Black).bg(Color::Cyan);
            let hotkey_style = if highlighted {
                bar.add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            };
            let label_style = if highlighted {
                bar.add_modifier(Modifier::BOLD)
            } else if is_selected {
                // The view shown on the right
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let text = format!(" {} {}", item.hotkey(), item.label());
            // A highlight spans the row
            let pad = (area.width as usize).saturating_sub(text.chars().count());
            lines.push(Line::from(vec![
                Span::styled(" ", if highlighted { bar } else { Style::default() }),
                Span::styled(item.hotkey(), hotkey_style),
                Span::styled(format!(" {}", item.label()), label_style),
                Span::styled(
                    " ".repeat(if highlighted { pad } else { 0 }),
                    if highlighted { bar } else { Style::default() },
                ),
            ]));
        }

        frame.render_widget(Paragraph::new(lines), area);
    }
}
