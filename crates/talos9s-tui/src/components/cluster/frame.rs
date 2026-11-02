//! The workspace frame: header, nodes pane width, key line and hit testing

use super::*;
use talos9s_core::convert::to_u16;

impl ClusterComponent {
    /// What a click at (column, row) in the nodes pane lands on
    pub fn click_target(&self, column: u16, row: u16) -> ClickTarget {
        let hits = self.hits.borrow();
        if contains(hits.node_list, column, row) {
            let idx = (row - hits.node_list.y) as usize;
            if hits
                .node_rows
                .get(idx)
                .is_some_and(std::option::Option::is_some)
            {
                return ClickTarget::Row;
            }
        } else if contains(hits.menu, column, row) {
            let idx = (row - hits.menu.y) as usize;
            if (1..=NavMenuItem::ALL.len()).contains(&idx) {
                return ClickTarget::Menu;
            }
        }
        ClickTarget::None
    }

    pub fn focused_pane(&self) -> FocusedPane {
        self.focused_pane
    }

    pub fn set_focused_pane(&mut self, pane: FocusedPane) {
        self.focused_pane = pane;
    }

    /// Back from the dashboard's services list to the tree side that had
    /// focus before (Nodes or the menu)
    pub fn leave_services(&mut self) {
        self.focused_pane = self.tree_pane;
    }

    /// Mark `item` as the view shown on the right
    pub fn set_active_menu(&mut self, item: NavMenuItem) {
        if let Some(i) = NavMenuItem::ALL.iter().position(|m| *m == item) {
            self.selected_menu_item = i + 1;
        }
    }

    /// The nodes pane's mode and width for a content area `total` columns
    /// wide; `None` picks Full when the view keeps `VIEW_MIN_WIDTH`
    pub fn resolve_pane_width(&self, total: u16, choice: Option<PaneWidth>) -> (PaneWidth, u16) {
        // As wide as its content, at most 40%: the view needs the room more
        let full = self
            .nodes_pane_width()
            .min(total * 2 / 5)
            .max(NODES_PANE_MIN_WIDTH.min(total));
        let mode = choice.unwrap_or(if total.saturating_sub(full) >= VIEW_MIN_WIDTH {
            PaneWidth::Full
        } else {
            PaneWidth::Compact
        });
        let width = match mode {
            PaneWidth::Full => full,
            PaneWidth::Compact => self.compact_pane_width().min(total),
            PaneWidth::Hidden => 0,
        };
        (mode, width)
    }

    /// The longest node name (after indent, selector and health dot) or
    /// menu entry (" m Machine config"), plus borders
    pub(super) fn compact_pane_width(&self) -> u16 {
        let node = self
            .clusters
            .iter()
            .flat_map(|c| c.versions.iter().map(|v| v.node.chars().count() + 11))
            .max()
            .unwrap_or(0);
        let menu = NavMenuItem::ALL
            .iter()
            .map(|item| 1 + item.hotkey().len() + 1 + item.label().len() + 2)
            .max()
            .unwrap_or(0);
        to_u16(node.max(menu)).clamp(COMPACT_PANE_MIN_WIDTH, COMPACT_PANE_MAX_WIDTH)
    }

    /// Draw the header, the nodes pane at `width` and the bottom key line,
    /// and return the nodes pane's and the view's areas. `dashboard`: the
    /// details pane will be drawn in the view's area (mouse hits stay valid).
    pub fn draw_frame(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        width: Option<PaneWidth>,
        view_focused: bool,
        dashboard: bool,
        view_hints: Option<Line<'static>>,
    ) -> (Rect, Rect) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(2), // Header
            Constraint::Min(8),    // Nodes pane and view
            Constraint::Length(2), // Footer
        ])
        .areas(area);
        self.draw_header(frame, header);

        let (_, nodes_width) = self.resolve_pane_width(body.width, width);
        let [tree, view] =
            Layout::horizontal([Constraint::Length(nodes_width), Constraint::Min(0)]).areas(body);

        self.view_focused = view_focused;
        if self.focused_pane != FocusedPane::Services {
            self.tree_pane = self.focused_pane;
        }
        self.hits.borrow_mut().tree = tree;
        if nodes_width > 0 {
            self.draw_nodes_pane(frame, tree);
        } else {
            let mut hits = self.hits.borrow_mut();
            hits.node_list = Rect::default();
            hits.node_rows.clear();
            hits.menu = Rect::default();
        }
        if !dashboard {
            let mut hits = self.hits.borrow_mut();
            hits.details = Rect::default();
            hits.services = None;
        }
        self.draw_footer(frame, footer, view_focused, dashboard, view_hints);
        (tree, view)
    }

    /// Bottom key line: the tree's keys, or the global ones while a view has
    /// the keyboard (it lists its own keys)
    pub(super) fn draw_footer(
        &self,
        frame: &mut Frame,
        area: Rect,
        view_focused: bool,
        dashboard: bool,
        view_hints: Option<Line<'static>>,
    ) {
        let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Yellow));
        let label = |l: &'static str| Span::styled(l, Style::default().dim());
        let sep = || Span::raw("  ");
        // Only what acts on the focused window now: the keys that always work
        // there first, then the ones for the selected row and the view shown.
        // (q is listed with Esc: some terminals, IDEA's, never deliver Esc.)
        let spans = if view_focused {
            match &self.back_hint {
                Some(hint) => vec![
                    key(" [Esc/q]"),
                    Span::styled(format!(" back to {hint}"), Style::default().dim()),
                    sep(),
                    key("[←]"),
                    label(" tree"),
                ],
                None => vec![key(" [Esc/q/←]"), label(" tree")],
            }
        } else {
            let header_selected = matches!(
                self.selected_item,
                NodeListItem::ClusterHeader(_)
                    | NodeListItem::ControlPlaneHeader(_)
                    | NodeListItem::WorkersHeader(_)
            );
            let node_selected = self.current_node_name().is_some();
            let auto_refresh_status = if self.auto_refresh { "ON" } else { "OFF" };
            let auto_refresh_color = if self.auto_refresh {
                Color::Green
            } else {
                Color::DarkGray
            };
            match self.focused_pane {
                FocusedPane::Menu => vec![
                    key(" [j/k]"),
                    label(" nav"),
                    sep(),
                    key("[Enter]"),
                    label(" open view"),
                ],
                FocusedPane::Services => vec![
                    key(" [j/k]"),
                    label(" select"),
                    sep(),
                    key("[Enter]"),
                    label(" logs"),
                    sep(),
                    key("[Esc/q/←]"),
                    label(" tree"),
                ],
                FocusedPane::Nodes => {
                    let mut spans = vec![key(" [j/k]"), label(" nav"), sep()];
                    spans.extend(match (header_selected, dashboard) {
                        (true, true) => [key("[Enter/Space]"), label(" fold")],
                        (false, true) => [key("[Enter/→]"), label(" services")],
                        (_, false) => [key("[Enter/→]"), label(" into view")],
                    });
                    spans.extend([
                        sep(),
                        key("[r]"),
                        label(" refresh"),
                        sep(),
                        key("[a]"),
                        label(" auto:"),
                        Span::styled(auto_refresh_status, Style::default().fg(auto_refresh_color)),
                    ]);
                    if header_selected && !dashboard {
                        spans.extend([sep(), key("[Space]"), label(" fold")]);
                    }
                    if node_selected {
                        spans.extend([sep(), key("[o]"), label(" ops")]);
                    }
                    if !self.clusters.is_empty() {
                        spans.extend([sep(), key("[O]"), label(" rolling")]);
                    }
                    // q returns to the dashboard first, and quits from there
                    if dashboard {
                        spans.extend([sep(), key("[q]"), label(" quit")]);
                    } else {
                        spans.extend([sep(), key("[Esc/q]"), label(" dashboard")]);
                    }
                    spans
                }
            }
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let line = Line::from(spans);
        match view_hints {
            // The view's keys (or a question such as quitting) on the left,
            // the workspace keys on the right
            Some(hints) => {
                let global_width = to_u16(line.width()).min(inner.width);
                let hints_area = Rect {
                    width: inner.width.saturating_sub(global_width + 1),
                    ..inner
                };
                let global_area = Rect {
                    x: inner.x + inner.width - global_width,
                    width: global_width,
                    ..inner
                };
                frame.render_widget(Paragraph::new(hints), hints_area);
                frame.render_widget(Paragraph::new(line), global_area);
            }
            _ => frame.render_widget(Paragraph::new(line), inner),
        }
    }
    /// Draw compact header with status indicators
    pub(super) fn draw_header(&self, frame: &mut Frame, area: Rect) {
        // Only clusters that aren't connected are worth a word here: every
        // row of the tree carries its own status, so a healthy set says nothing
        let mut header_spans = vec![
            Span::styled(
                " talos9s",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", crate::update::VERSION),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        for cluster in &self.clusters {
            let (symbol, word, color) = if cluster.loading {
                ("◌", "connecting", Color::Yellow)
            } else if !cluster.connected {
                ("✗", "disconnected", Color::Red)
            } else {
                continue;
            };
            header_spans.extend([
                Span::styled(format!("  {symbol} "), Style::default().fg(color)),
                Span::styled(cluster.name.clone(), Style::default().fg(color)),
                Span::styled(format!(" {word}"), Style::default().dim()),
            ]);
        }

        // Active cluster name on the right
        let active_name = self
            .clusters
            .get(self.active_cluster)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "none".to_string());

        // A newer release: ahead of the global keys (they drop first), and
        // shorter or left out when the line is narrow (`--version` says it too)
        if let Some(newer) = &self.newer_version {
            let room = area
                .width
                .saturating_sub(to_u16(active_name.chars().count()).saturating_add(3));
            let used = to_u16(Line::from(header_spans.clone()).width());
            let long = format!("· {newer} available ");
            let short = format!("↑{newer} ");
            let style = Style::default().fg(Color::Green);
            if used.saturating_add(to_u16(long.chars().count())) <= room {
                header_spans.push(Span::styled(long, style));
            } else if used.saturating_add(to_u16(short.chars().count())) <= room {
                header_spans.push(Span::styled(short, style));
            }
        }

        let left_content = Line::from(header_spans);
        let right_content = Span::styled(
            format!(" {active_name} "),
            Style::default().fg(Color::DarkGray),
        );

        // Render header
        let header_block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = header_block.inner(area);
        frame.render_widget(header_block, area);
        let left_width = to_u16(left_content.width());
        frame.render_widget(Paragraph::new(left_content), inner);

        // Right-align active cluster name
        let name_width = to_u16(active_name.chars().count()).saturating_add(3);
        let right_area = Rect {
            x: area.x + area.width.saturating_sub(name_width),
            y: area.y,
            width: name_width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(right_content), right_area);

        // The keys that work in every window, before the name; left out when
        // the line is too narrow for them
        let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Yellow));
        let label = |l: &'static str| Span::styled(l, Style::default().dim());
        let global = Line::from(vec![
            key("[Tab]"),
            label(" next window  "),
            key("[Z]"),
            label(" zoom  "),
            key("[ ]"),
            label(" pane width  "),
            key("[Ctrl+C]"),
            label(" quit  "),
            // Mouse capture is on for click navigation, so the terminal's own
            // selection needs a modifier (Option in iTerm2, Shift elsewhere)
            key("[⌥/⇧+drag]"),
            label(" select text "),
        ]);
        let global_width = to_u16(global.width());
        if left_width + 2 + global_width + name_width <= area.width {
            let global_area = Rect {
                x: right_area.x - global_width,
                y: area.y,
                width: global_width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(global), global_area);
        }
    }
}
