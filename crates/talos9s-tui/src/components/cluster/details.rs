//! The details pane: the dashboard of the selected node

use super::*;

impl ClusterComponent {
    /// Render a compact ASCII bar for percentage values
    pub(super) fn render_compact_bar(pct: f32, width: usize) -> String {
        let filled = ((pct / 100.0) * width as f32).round() as usize;
        let empty = width.saturating_sub(filled);
        format!(
            "{}{}{:>3}%",
            "█".repeat(filled),
            "░".repeat(empty),
            pct.round() as u8
        )
    }

    pub fn draw_details_pane(&mut self, frame: &mut Frame, area: Rect) {
        {
            let mut hits = self.hits.borrow_mut();
            hits.details = area;
            hits.services = None;
        }
        // Focus indication - cyan border when focused
        let border_color = if self.focused_pane == FocusedPane::Services {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        // Get active cluster data
        let cluster_idx = self.active_cluster;
        let cluster = self.clusters.get(cluster_idx);

        // Still connecting: say so instead of an empty or error pane
        if let Some(c) = cluster
            && c.loading
        {
            let block = Block::default()
                .title(format!(" {} ", c.name))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            frame.render_widget(
                Paragraph::new(connecting_lines(c))
                    .block(block)
                    .wrap(ratatui::widgets::Wrap { trim: false }),
                area,
            );
            return;
        }

        // A disconnected cluster that failed to connect has no nodes to select,
        // so show the failure details here (endpoints tried + cause + hint).
        if let Some(c) = cluster
            && !c.connected
            && let Some(err) = &c.error
        {
            let block = Block::default()
                .title(" Connection Error ")
                .title_style(Style::default().fg(Color::Red))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));

            let msg = Paragraph::new(connection_error_lines(c, err, self.auto_refresh))
                .block(block)
                .wrap(ratatui::widgets::Wrap { trim: false });
            frame.render_widget(msg, area);
            return;
        }

        // Check if cluster is connected but has no etcd members (not bootstrapped)
        let needs_bootstrap = cluster
            .map(|c| c.connected && c.etcd_members.is_empty() && c.versions.is_empty())
            .unwrap_or(false);

        if needs_bootstrap {
            let block = Block::default()
                .title(" Bootstrap Required ")
                .title_style(Style::default().fg(Color::Yellow))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));

            // Get control plane IP from talosconfig endpoints
            let config_result = TalosConfig::load(self.config_path.as_deref());
            let cp_ip = config_result
                .ok()
                .and_then(|config| {
                    config
                        .current_context()
                        .and_then(|ctx| ctx.endpoints.first())
                        .map(|e| talos_rs::host_of(e).to_string())
                })
                .unwrap_or_else(|| "<control-plane-ip>".to_string());

            let msg = Paragraph::new(bootstrap_lines(&cp_ip)).block(block);
            frame.render_widget(msg, area);
            return;
        }

        let versions_empty = cluster.map(|c| c.versions.is_empty()).unwrap_or(true);
        if versions_empty {
            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            let msg = Paragraph::new(Line::from(Span::styled(
                "  No node selected",
                Style::default().dim(),
            )))
            .block(block);
            frame.render_widget(msg, area);
            return;
        }

        // Check if we have a node selected (vs a header)
        let Some(node_name_str) = self.current_node_name() else {
            // Header selected - show a table of every node in the group
            self.draw_group_table(frame, area, border_color);
            return;
        };

        let node_name = if node_name_str.is_empty() {
            "node-0".to_string()
        } else {
            node_name_str.clone()
        };
        let node_ip = self.node_ips().get(&node_name).cloned().unwrap_or_default();
        let role = if self.node_is_controlplane(cluster_idx, &node_name) {
            "controlplane"
        } else {
            "worker"
        };

        if let Some(down) = self.node_offline_for(cluster_idx, &node_name) {
            let block = Block::default()
                .title(format!(" {} · {} ", node_name, role))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));
            frame.render_widget(
                Paragraph::new(offline_lines(down, node_ip))
                    .block(block)
                    .wrap(ratatui::widgets::Wrap { trim: false }),
                area,
            );
            return;
        }

        let title = crate::ui_ext::title_with_refresh(
            &format!("{} · {}", node_name, role),
            self.node_stats_fetch.is_some(),
            if self.focused_pane == FocusedPane::Services {
                Color::Cyan
            } else {
                Color::White
            },
        );
        let block = Block::default()
            .title(title)
            .title_bottom(services_legend(false))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Resources, a spacer, then the services: one column that scrolls
        // (wheel, or to keep the selected service in view) when the pane is
        // shorter than it
        let mut lines = self.resource_lines(&node_name, &node_ip, inner.width as usize);
        lines.push(Line::from(""));
        let services_heading = lines.len();
        let services = self.get_node_services(&node_name).cloned();
        let service_count = services.as_ref().map_or(0, |s| s.len());
        if let Some(services) = &services {
            lines.extend(self.services_lines(services));
        }

        let height = inner.height as usize;
        let total = lines.len();
        if self.details_scroll_node != node_name {
            self.details_scroll_node = node_name.clone();
            self.details_scroll = 0;
        }
        self.details_max_scroll = total.saturating_sub(height);
        self.details_scroll = self.details_scroll.min(self.details_max_scroll);
        // Keyboard selection in the services stays in view
        if self.focused_pane == FocusedPane::Services && service_count > 0 {
            let selected_line = services_heading + 1 + self.selected_service;
            if selected_line < self.details_scroll {
                self.details_scroll = selected_line;
            } else if selected_line >= self.details_scroll + height {
                self.details_scroll = selected_line + 1 - height;
            }
        }
        frame.render_widget(
            Paragraph::new(lines).scroll((self.details_scroll as u16, 0)),
            inner,
        );
        crate::ui_ext::draw_scrollbar(
            frame,
            Rect::new(inner.x + inner.width, inner.y, 1, inner.height),
            total as u16,
            self.details_scroll as u16,
        );

        // Where the service rows are on screen (clipped to the pane), for clicks
        let rows_start = services_heading + 1;
        let first_visible = rows_start.max(self.details_scroll);
        let last_visible = (rows_start + service_count).min(self.details_scroll + height);
        self.hits.borrow_mut().services =
            (service_count > 0 && first_visible < last_visible).then(|| {
                let y = inner.y + (first_visible - self.details_scroll) as u16;
                (
                    Rect::new(
                        inner.x,
                        y,
                        inner.width,
                        (last_visible - first_visible) as u16,
                    ),
                    first_visible - rows_start,
                    service_count,
                )
            });
    }

    /// The node's resources: address, versions, memory, load, CPU, uptime,
    /// disks, ZFS pools, network, clock and the last diagnostics verdict.
    /// `width` is the pane's inner width, for wrapping address lists.
    pub(super) fn resource_lines(
        &self,
        node_name: &str,
        node_ip: &str,
        width: usize,
    ) -> Vec<Line<'static>> {
        let mut resource_lines = vec![Line::from(vec![
            Span::styled(" IP: ", Style::default().dim()),
            Span::styled(node_ip.to_string(), Style::default().fg(Color::DarkGray)),
        ])];

        // Talos and Kubernetes (kubelet) versions
        let mut version_spans = Vec::new();
        if let Some(v) = self.get_node_talos_version_for(self.active_cluster, node_name) {
            version_spans.push(Span::styled(" Talos: ", Style::default().dim()));
            version_spans.push(Span::raw(v.to_string()));
        }
        if let Some(v) = self.get_node_kubelet_version_for(self.active_cluster, node_name) {
            version_spans.push(Span::styled("  K8s: ", Style::default().dim()));
            version_spans.push(Span::raw(v.to_string()));
        }
        if !version_spans.is_empty() {
            resource_lines.push(Line::from(version_spans));
        }

        // Memory bar
        if let Some(mem) = self.get_node_memory(node_name) {
            let pct = mem.usage_percent();
            let used_gb = (mem.mem_total - mem.mem_available) as f64 / 1024.0 / 1024.0 / 1024.0;
            let total_gb = mem.mem_total as f64 / 1024.0 / 1024.0 / 1024.0;
            let bar = Self::render_compact_bar(pct, 10);
            let color = memory_color(pct);
            resource_lines.push(Line::from(vec![
                Span::styled(" Memory: ", Style::default().dim()),
                Span::styled(bar, Style::default().fg(color)),
                Span::styled(
                    format!(" {:.1}/{:.1}GB", used_gb, total_gb),
                    Style::default().dim(),
                ),
            ]));
        }

        // Load average
        if let Some(load) = self.get_node_load_avg(node_name) {
            let cores = self.get_node_cpu_info(node_name).map(|c| c.cpu_count);
            let color = load_color(load.load1, cores);
            resource_lines.push(Line::from(vec![
                Span::styled(" Load:   ", Style::default().dim()),
                Span::styled(format!("{:.2}", load.load1), Style::default().fg(color)),
                Span::styled(
                    format!(" {:.2} {:.2} (1/5/15m)", load.load5, load.load15),
                    Style::default().dim(),
                ),
            ]));
        }

        // CPU info
        if let Some(cpu) = self.get_node_cpu_info(node_name) {
            resource_lines.push(Line::from(vec![
                Span::styled(" CPU:    ", Style::default().dim()),
                Span::styled(
                    format!("{} cores", cpu.cpu_count),
                    Style::default().fg(Color::White),
                ),
                Span::styled(format!(" @ {:.0}MHz", cpu.mhz), Style::default().dim()),
            ]));
        }

        // Uptime (computed at draw time so it keeps ticking between refreshes)
        if let Some(uptime) = self
            .get_node_system_stat(node_name)
            .and_then(|s| s.uptime_secs(unix_now_secs()))
        {
            resource_lines.push(Line::from(vec![
                Span::styled(" Uptime: ", Style::default().dim()),
                Span::styled(format_uptime(uptime), Style::default().fg(Color::White)),
            ]));
        }

        // Host disks: /var (EPHEMERAL) first, then any user volumes
        if let Some(mounts) = self.get_node_mounts_for(self.active_cluster, node_name) {
            for (i, disk) in mounts.host_disks().into_iter().enumerate() {
                let Some(pct) = disk.used_pct() else {
                    continue;
                };
                let label = if i == 0 { " Disk:   " } else { "         " };
                resource_lines.push(Line::from(vec![
                    Span::styled(label, Style::default().dim()),
                    Span::styled(
                        Self::render_compact_bar(pct, 10),
                        Style::default().fg(memory_color(pct)),
                    ),
                    Span::styled(
                        format!(
                            " {:.1}/{:.1}GB, {:.1}GB free ",
                            gib(disk.used()),
                            gib(disk.size),
                            gib(disk.available)
                        ),
                        Style::default().dim(),
                    ),
                    Span::styled(
                        disk.mounted_on.clone(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }

        // ZFS pools
        for (i, pool) in self
            .get_node_zfs_pools_for(self.active_cluster, node_name)
            .iter()
            .enumerate()
        {
            let Some(pct) = pool.used_pct() else {
                continue;
            };
            let label = if i == 0 { " ZFS:    " } else { "         " };
            resource_lines.push(Line::from(vec![
                Span::styled(label, Style::default().dim()),
                Span::styled(
                    Self::render_compact_bar(pct, 10),
                    Style::default().fg(memory_color(pct)),
                ),
                Span::styled(
                    format!(
                        " {:.1}/{:.1}GB, {:.1}GB free ",
                        gib(pool.used),
                        gib(pool.size()),
                        gib(pool.free)
                    ),
                    Style::default().dim(),
                ),
                Span::styled(pool.name.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }

        if let Some(net) = self.get_node_network_info(node_name) {
            resource_lines.extend(network_lines(net, width));
        }
        if let Some(clock) = self.get_node_clock_for(self.active_cluster, node_name) {
            resource_lines.push(Line::from(vec![
                Span::styled(" Clock:  ", Style::default().dim()),
                clock_span(clock),
                Span::styled(format!(" vs {}", clock.server), Style::default().dim()),
            ]));
        }
        // From the last diagnostics run, when there was one: no extra queries
        if let Some(summary) = self
            .diagnostics_summaries
            .get(&(self.cluster_key(), node_name.to_string()))
        {
            resource_lines.push(diagnostics_line(summary));
        }
        resource_lines
    }

    /// The services heading and one line per service, the selected one
    /// marked while the services have the focus
    pub(super) fn services_lines(&self, services: &[ServiceInfo]) -> Vec<Line<'static>> {
        let running = services.iter().filter(|s| s.state == "Running").count();
        let mut lines = vec![Line::from(vec![Span::styled(
            format!(" Services ({}/{})", running, services.len()),
            Style::default().fg(Color::Gray),
        )])];

        for (i, svc) in services.iter().enumerate() {
            // The same symbols as the group table and its legend
            let (health_symbol, health_style) = service_symbol(svc);

            // Highlight selected service when services pane is focused
            let is_selected =
                i == self.selected_service && self.focused_pane == FocusedPane::Services;
            let name_style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let selector = if is_selected { "▸" } else { " " };

            lines.push(Line::from(vec![
                Span::raw(format!(" {}", selector)),
                Span::styled(health_symbol, health_style),
                Span::raw(" "),
                Span::styled(svc.id.clone(), name_style),
                Span::styled(format!(" ({})", svc.state), Style::default().dim()),
            ]));
        }
        lines
    }
}

/// A cluster that is still connecting: its endpoints, and that the others
/// stay usable meanwhile
pub(super) fn connecting_lines(c: &ClusterData) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Connecting…",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
    ];
    if !c.endpoints.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  Endpoints: ", Style::default().dim()),
            Span::raw(c.endpoints.join(", ")),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "  Other clusters stay usable while this one connects.",
        Style::default().dim(),
    )));
    lines
}

/// A cluster that failed to connect: the endpoints tried, the cause and how
/// it is retried
pub(super) fn connection_error_lines(
    c: &ClusterData,
    err: &str,
    auto_refresh: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Cluster:  "),
            Span::styled(
                c.name.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Status:   "),
            Span::styled("Disconnected", Style::default().fg(Color::Red)),
        ]),
    ];

    if !c.endpoints.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Endpoints tried:",
            Style::default().fg(Color::Yellow),
        )));
        for ep in &c.endpoints {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled("- ", Style::default().fg(Color::DarkGray)),
                Span::styled(ep.clone(), Style::default().fg(Color::White)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Error:",
        Style::default().fg(Color::Yellow),
    )));
    lines.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(err.to_string(), Style::default().fg(Color::Red)),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Verify the endpoint(s) are reachable from this",
        Style::default().dim(),
    )));
    lines.push(Line::from(Span::styled(
        "  machine and that your talosconfig is correct.",
        Style::default().dim(),
    )));
    let retry = if auto_refresh {
        format!(
            "  Retrying every {}s — press r to retry now.",
            talos9s_core::constants::refresh_intervals::SLOW
        )
    } else {
        "  Press r to retry.".to_string()
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(retry, Style::default().dim())));
    lines
}

/// A connected cluster without etcd: the `talosctl bootstrap` command to run
/// against `cp_ip` (shown, never run)
pub(super) fn bootstrap_lines(cp_ip: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Cluster not yet bootstrapped.",
            Style::default().fg(Color::Yellow),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  To bootstrap, run:",
            Style::default().dim(),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("    talosctl bootstrap -n {}", cp_ip),
            Style::default().fg(Color::Cyan),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  This initializes etcd and starts",
            Style::default().dim(),
        )]),
        Line::from(vec![Span::styled(
            "  the Kubernetes control plane.",
            Style::default().dim(),
        )]),
    ]
}

/// A known node that stopped answering, `down` ago
pub(super) fn offline_lines(down: std::time::Duration, node_ip: String) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Status: ", Style::default().dim()),
            Span::styled(
                format!("Offline for {}", format_uptime(down.as_secs())),
                Style::default().fg(Color::Red).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled("  IP:     ", Style::default().dim()),
            Span::raw(node_ip),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  The node is not answering the Talos API (rebooting, shut down or",
            Style::default().dim(),
        )),
        Line::from(Span::styled(
            "  unreachable). It is checked on every refresh and comes back by itself.",
            Style::default().dim(),
        )),
    ]
}

/// Network summary (same fields as the Talos dashboard), address lists
/// wrapped to `width`
pub(super) fn network_lines(net: &NodeNetworkInfo, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let value = Style::default().fg(Color::White);
    lines.extend(wrap_list(" Addrs:  ", &net.addresses, width, value));
    lines.extend(wrap_list(" GW:     ", &net.gateways, width, value));
    if let Some(ready) = net.connectivity_ready {
        let (text, color) = if ready {
            ("✓ OK", Color::Green)
        } else {
            ("✗ Not ready", Color::Red)
        };
        lines.push(Line::from(vec![
            Span::styled(" Conn:   ", Style::default().dim()),
            Span::styled(text, Style::default().fg(color)),
        ]));
    }
    lines.extend(wrap_list(" DNS:    ", &net.dns_servers, width, value));

    if let Some(ts) = &net.tailscale {
        let mut spans = vec![Span::styled(" TS:     ", Style::default().dim())];
        if ts.up {
            spans.push(Span::styled(
                tailnet_ip(ts).unwrap_or("no IP").to_string(),
                value,
            ));
            spans.push(Span::styled(
                format!(
                    "  {} tailnet peers{}",
                    ts.peer_routes,
                    ts.mtu.map(|m| format!("  mtu {m}")).unwrap_or_default()
                ),
                Style::default().dim(),
            ));
        } else {
            spans.push(Span::styled("down", Style::default().fg(Color::Red)));
        }
        lines.push(Line::from(spans));
        // Often dozens: a count keeps the services list in view
        if !ts.subnet_routes.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(" Routes: ", Style::default().dim()),
                Span::styled(
                    format!("{} subnet routes via the tailnet", ts.subnet_routes.len()),
                    Style::default().dim(),
                ),
            ]));
        }
    }
    // Peers up out of all, and only the ones that aren't by name:
    // readable at a hundred peers
    if let Some(peers) = net.kubespan_peers.as_ref().filter(|p| !p.is_empty()) {
        let links = kubespan_links(peers);
        let mut spans = vec![
            Span::styled(" KSpan:  ", Style::default().dim()),
            Span::styled(
                format!("{}/{} peers up", links.up, links.total),
                Style::default().fg(if links.down.is_empty() {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
        ];
        for (peer, state) in &links.down {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{peer} {state}"),
                Style::default().fg(Color::Red),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}
