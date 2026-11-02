//! Key and mouse handling of the cluster tree

use super::*;

impl Component for ClusterComponent {
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        let (column, row) = (mouse.column, mouse.row);
        let (in_nodes, in_menu, in_details) = {
            let hits = self.hits.borrow();
            (
                contains(hits.node_list, column, row),
                contains(hits.menu, column, row),
                contains(hits.details, column, row),
            )
        };

        match mouse.kind {
            // Wheel scrolls the pane under the pointer
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // A node pane taller than the screen scrolls as a whole
                if in_details && self.details_max_scroll > 0 {
                    self.details_scroll = if mouse.kind == MouseEventKind::ScrollUp {
                        self.details_scroll.saturating_sub(3)
                    } else {
                        (self.details_scroll + 3).min(self.details_max_scroll)
                    };
                    return Ok(None);
                }
                let pane = if in_details {
                    FocusedPane::Services
                } else if in_menu {
                    FocusedPane::Menu
                } else if in_nodes {
                    FocusedPane::Nodes
                } else {
                    return Ok(None);
                };
                self.focused_pane = pane;
                if pane == FocusedPane::Menu && self.selected_menu_item == 0 {
                    self.selected_menu_item = 1;
                }
                let code = if mouse.kind == MouseEventKind::ScrollUp {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                };
                self.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if in_nodes {
                    let item = {
                        let hits = self.hits.borrow();
                        let idx = (row - hits.node_list.y) as usize;
                        hits.node_rows.get(idx).cloned().flatten()
                    };
                    // A row click keeps the services list focused (showing the
                    // new node's services), as other views keep the keyboard;
                    // empty space under the tree focuses the tree
                    if item.is_none() || self.focused_pane != FocusedPane::Services {
                        self.focused_pane = FocusedPane::Nodes;
                    }
                    if let Some(item) = item {
                        let is_header = matches!(
                            item,
                            NodeListItem::ClusterHeader(_)
                                | NodeListItem::ControlPlaneHeader(_)
                                | NodeListItem::WorkersHeader(_)
                        );
                        // Clicking the already-selected header folds/unfolds it
                        if is_header && item == self.selected_item {
                            self.toggle_expand();
                        } else {
                            self.selected_item = item;
                            self.active_cluster = self.selected_cluster_index();
                        }
                    }
                    Ok(None)
                } else if in_menu {
                    let row = (row - self.hits.borrow().menu.y) as usize;
                    if let Some(idx) = NavMenuItem::entry_at_row(row) {
                        self.focused_pane = FocusedPane::Menu;
                        self.selected_menu_item = idx;
                        return Ok(self.navigate_to_selected_menu());
                    }
                    // The separator line or a heading
                    self.focused_pane = FocusedPane::Nodes;
                    Ok(None)
                } else if in_details {
                    self.focused_pane = FocusedPane::Services;
                    let service = self
                        .hits
                        .borrow()
                        .services
                        .and_then(|(area, first, count)| {
                            let idx = row.checked_sub(area.y)? as usize + first;
                            (contains(area, column, row) && idx < count).then_some(idx)
                        });
                    if let Some(idx) = service {
                        self.selected_service = idx;
                    }
                    Ok(None)
                } else {
                    // The pane's border, title or separator: the tree
                    if contains(self.hits.borrow().tree, column, row) {
                        self.focused_pane = FocusedPane::Nodes;
                    }
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        match key.code {
            KeyCode::Char('q') => Ok(Some(Action::Quit)),
            // Back to the dashboard from another view
            KeyCode::Esc => Ok(Some(Action::ShowDashboard)),
            KeyCode::Char('r') => Ok(Some(Action::Refresh)),

            // Vertical navigation within focused pane
            KeyCode::Up | KeyCode::Char('k') => {
                match self.focused_pane {
                    FocusedPane::Nodes => {
                        self.navigate_up();
                    }
                    FocusedPane::Menu => {
                        if self.selected_menu_item > 1 {
                            self.selected_menu_item -= 1;
                        }
                    }
                    FocusedPane::Services => {
                        let count = self.current_service_count();
                        if count > 0 && self.selected_service > 0 {
                            self.selected_service -= 1;
                        }
                    }
                }
                Ok(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.focused_pane {
                    FocusedPane::Nodes => {
                        self.navigate_down();
                    }
                    FocusedPane::Menu => {
                        let menu_count = NavMenuItem::ALL.len();
                        if self.selected_menu_item < menu_count {
                            self.selected_menu_item += 1;
                        }
                    }
                    FocusedPane::Services => {
                        let count = self.current_service_count();
                        if count > 0 && self.selected_service < count - 1 {
                            self.selected_service += 1;
                        }
                    }
                }
                Ok(None)
            }

            // Space: toggle expand/collapse on group headers
            KeyCode::Char(' ') => {
                if self.focused_pane == FocusedPane::Nodes {
                    self.toggle_expand();
                }
                Ok(None)
            }

            // Switch focus between panes: Nodes → Menu → Services → Nodes
            KeyCode::Tab => {
                self.focused_pane = match self.focused_pane {
                    FocusedPane::Nodes => FocusedPane::Menu,
                    FocusedPane::Menu => FocusedPane::Services,
                    FocusedPane::Services => FocusedPane::Nodes,
                };
                // Reset menu selection when entering menu
                if self.focused_pane == FocusedPane::Menu && self.selected_menu_item == 0 {
                    self.selected_menu_item = 1;
                }
                Ok(None)
            }
            KeyCode::BackTab => {
                self.focused_pane = match self.focused_pane {
                    FocusedPane::Nodes => FocusedPane::Services,
                    FocusedPane::Menu => FocusedPane::Nodes,
                    FocusedPane::Services => FocusedPane::Menu,
                };
                if self.focused_pane == FocusedPane::Menu && self.selected_menu_item == 0 {
                    self.selected_menu_item = 1;
                }
                Ok(None)
            }

            // Enter: action depends on focused pane
            KeyCode::Enter => {
                match self.focused_pane {
                    FocusedPane::Nodes => {
                        // On a header - toggle expand/collapse
                        // On a node - show all logs for that node
                        match &self.selected_item {
                            NodeListItem::ClusterHeader(_)
                            | NodeListItem::ControlPlaneHeader(_)
                            | NodeListItem::WorkersHeader(_) => {
                                self.toggle_expand();
                                Ok(None)
                            }
                            // On a node: into the view on the right
                            _ => Ok(Some(Action::FocusView)),
                        }
                    }
                    FocusedPane::Menu => {
                        // Navigate to selected screen
                        Ok(self.navigate_to_selected_menu())
                    }
                    FocusedPane::Services => {
                        // Logs of the selected service, the node's others available
                        Ok(self
                            .selected_service_id()
                            .and_then(|id| self.logs_action(Some(id))))
                    }
                }
            }

            // Views for the selection: 'l' / 'L' logs of the selected node,
            // or of the selected cluster / control plane / workers
            KeyCode::Char('l') => Ok(self.action_for(NavMenuItem::Logs)),
            KeyCode::Char(c)
                if let Some(item) = NavMenuItem::ALL
                    .iter()
                    .find(|m| m.hotkey().starts_with(c) && m.hotkey().len() == c.len_utf8()) =>
            {
                Ok(self.action_for(*item))
            }
            KeyCode::Char('o') => {
                // Show node operations overlay for selected node
                if let Some(node_name) = self.current_node_name() {
                    let node_ip = self
                        .node_ips()
                        .get(&node_name)
                        .cloned()
                        .unwrap_or_else(|| node_name.clone());
                    let is_controlplane = self.current_node_role() == "controlplane";
                    Ok(Some(Action::ShowNodeOperations(
                        node_name,
                        node_ip,
                        is_controlplane,
                    )))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('O') => {
                // Show rolling operations overlay with all nodes from active cluster
                let cluster_idx = self.active_cluster;
                let nodes: Vec<(String, String, bool)> =
                    if let Some(cluster) = self.clusters.get(cluster_idx) {
                        cluster
                            .versions
                            .iter()
                            .map(|v| {
                                let hostname = v.node.clone();
                                let ip = cluster
                                    .node_ips
                                    .get(&hostname)
                                    .cloned()
                                    .unwrap_or_else(|| hostname.clone());
                                // The tree's role (discovery, then etcd members),
                                // not "runs etcd now": an offline control plane
                                // node runs nothing
                                let is_controlplane = cluster.is_controlplane(&hostname);
                                (hostname, ip, is_controlplane)
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                if !nodes.is_empty() {
                    Ok(Some(Action::ShowRollingOperations(nodes)))
                } else {
                    Ok(None)
                }
            }

            // Toggle auto-refresh
            KeyCode::Char('a') => {
                self.auto_refresh = !self.auto_refresh;
                Ok(None)
            }

            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // Could trigger auto-refresh here
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        let (_, details) = self.draw_frame(frame, area, None, false, true, None);
        self.draw_details_pane(frame, details);
        Ok(())
    }
}
