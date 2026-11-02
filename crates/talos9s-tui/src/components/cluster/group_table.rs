//! The group table and the KubeSpan / services matrices of a group of nodes

use super::*;

impl ClusterComponent {
    /// The KubeSpan peer matrix of `nodes` of the active cluster, from the
    /// peer status the dashboard already keeps per node
    pub fn draw_kubespan_matrix_pane(&self, frame: &mut Frame, area: Rect, nodes: &[String]) {
        let [caption, body] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
        // The node × node matrix shows asymmetric failures but needs a column
        // per node, so past the pane's width it becomes one row per node
        let fits = kubespan_matrix_width(nodes) <= body.width && (nodes.len() as u16) < body.height;
        let text = if fits {
            " Each row: the node's WireGuard peers (● up, ✗ down, · not a peer, … not read yet)"
        } else {
            " Each row: the node's WireGuard peers that are up, and the ones that aren't"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(Color::DarkGray),
            ))),
            caption,
        );
        if fits {
            self.draw_kubespan_matrix(frame.buffer_mut(), body, self.active_cluster, nodes);
        } else {
            self.draw_kubespan_list(frame.buffer_mut(), body, self.active_cluster, nodes);
        }
    }

    /// One row per node: peers up out of all, and the peers that aren't up
    pub(super) fn draw_kubespan_list(
        &self,
        buf: &mut Buffer,
        area: Rect,
        cluster_idx: usize,
        nodes: &[String],
    ) {
        let dim = Style::default().dim();
        let rows: Vec<Row> = nodes
            .iter()
            .map(|node| {
                let peers = self
                    .get_node_network_info_for(cluster_idx, node)
                    .and_then(|net| net.kubespan_peers.as_ref());
                let (up, down) = match peers {
                    None => (Cell::from(Span::styled("…", dim)), Cell::default()),
                    Some(peers) if peers.is_empty() => (
                        Cell::from(Span::styled("no peers", Style::default().fg(Color::Yellow))),
                        Cell::default(),
                    ),
                    Some(peers) => {
                        let links = kubespan_links(peers);
                        (
                            Cell::from(Span::styled(
                                format!("{}/{}", links.up, links.total),
                                Style::default().fg(if links.down.is_empty() {
                                    Color::Green
                                } else {
                                    Color::Red
                                }),
                            )),
                            Cell::from(Line::from(
                                links
                                    .down
                                    .iter()
                                    .flat_map(|(peer, state)| {
                                        [
                                            Span::styled(
                                                format!("{peer} {state}"),
                                                Style::default().fg(Color::Red),
                                            ),
                                            Span::raw("  "),
                                        ]
                                    })
                                    .collect::<Vec<_>>(),
                            )),
                        )
                    }
                };
                Row::new(vec![Cell::from(node.clone()), up, down])
            })
            .collect();
        let name_width = nodes
            .iter()
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0)
            .max(4) as u16;
        let header = Row::new(vec!["Node", "Peers up", "Not up"])
            .style(Style::default().fg(Color::Gray).bold());
        Table::new(
            rows,
            [
                Constraint::Length(name_width),
                Constraint::Length(8),
                Constraint::Fill(1),
            ],
        )
        .header(header)
        .column_spacing(2)
        .render(area, buf);
    }

    /// Draw the details pane (right column)
    /// Table of every node under the selected cluster / control plane /
    /// workers header: memory, load, cores, uptime, gateway, connectivity
    /// and service health side by side.
    pub(super) fn draw_group_table(&mut self, frame: &mut Frame, area: Rect, border_color: Color) {
        let Some((cluster_idx, nodes)) = self.selected_nodes() else {
            return;
        };
        let group = match &self.selected_item {
            NodeListItem::ControlPlaneHeader(_) => "Control Plane".to_string(),
            NodeListItem::WorkersHeader(_) => "Workers".to_string(),
            _ => self
                .clusters
                .get(cluster_idx)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "Cluster".to_string()),
        };
        // A cluster refresh in flight rebuilds this table
        let refreshing = self
            .background_refresh
            .get(cluster_idx)
            .is_some_and(std::option::Option::is_some);
        let block = Block::default()
            .title(crate::ui_ext::title_with_refresh(
                &format!("{} · {} nodes", group, nodes.len()),
                refreshing,
                Color::White,
            ))
            .title_bottom(services_legend(true))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let dim = Style::default().dim();
        let pending = || Cell::from(Span::styled("…", dim));
        let now = unix_now_secs();
        // Drop the least important columns on narrow panes rather than truncate
        let wide = inner.width >= 90;
        let has_zfs = nodes
            .iter()
            .any(|n| !self.get_node_zfs_pools_for(cluster_idx, n).is_empty());
        let has_tailscale = nodes.iter().any(|n| {
            self.get_node_network_info_for(cluster_idx, n)
                .is_some_and(|net| net.tailscale.is_some())
        });
        let has_kubespan = nodes.iter().any(|n| {
            self.get_node_network_info_for(cluster_idx, n)
                .and_then(|net| net.kubespan_peers.as_ref())
                .is_some_and(|peers| !peers.is_empty())
        });

        // A version differing from the group's most common one is highlighted,
        // which makes a partly rolled-out upgrade stand out.
        let talos_common = most_common(
            nodes
                .iter()
                .filter_map(|n| self.get_node_talos_version_for(cluster_idx, n)),
        );
        let k8s_common = most_common(
            nodes
                .iter()
                .filter_map(|n| self.get_node_kubelet_version_for(cluster_idx, n)),
        );
        let version_cell = |version: Option<&str>, common: Option<&str>| match version {
            Some(v) if Some(v) == common => Cell::from(v.to_string()),
            Some(v) => Cell::from(Span::styled(
                v.to_string(),
                Style::default().fg(Color::Yellow),
            )),
            // Not fetched yet (an offline node has no version at all)
            None => pending(),
        };

        let rows: Vec<Row> = nodes
            .iter()
            .map(|node| {
                let role = if self.node_is_controlplane(cluster_idx, node) {
                    "cp"
                } else {
                    "worker"
                };

                // Offline: nothing current to show; say so in the Uptime column
                if let Some(down) = self.node_offline_for(cluster_idx, node) {
                    let none = || Cell::from(Span::styled("-", dim));
                    let red = Style::default().fg(Color::Red);
                    let mut cells = vec![
                        Cell::from(Span::styled(node.clone(), red)),
                        Cell::from(Span::styled(role, dim)),
                        none(),
                        none(),
                        none(),
                        none(),
                    ];
                    if has_zfs {
                        cells.push(none());
                    }
                    cells.push(none());
                    if wide {
                        cells.push(none());
                    }
                    cells.extend([
                        Cell::from(Span::styled("offline", red)),
                        Cell::from(Span::styled("✗", red)),
                        none(),
                        Cell::from(Span::styled(
                            format!("down {}", format_uptime(down.as_secs())),
                            dim,
                        )),
                    ]);
                    return Row::new(cells);
                }

                let mem = match self.get_node_memory_for(cluster_idx, node) {
                    Some(m) => {
                        let pct = m.usage_percent();
                        let total_gb = m.mem_total as f64 / 1024.0 / 1024.0 / 1024.0;
                        Cell::from(Line::from(vec![
                            Span::styled(
                                format!("{pct:>3.0}%"),
                                Style::default().fg(memory_color(pct)),
                            ),
                            Span::styled(format!(" {total_gb:.0}G"), dim),
                        ]))
                    }
                    None => pending(),
                };

                let load = match self.get_node_load_avg_for(cluster_idx, node) {
                    Some(l) => Cell::from(Span::styled(
                        format!("{:.2}", l.load1),
                        Style::default().fg(load_color(
                            l.load1,
                            self.get_node_cpu_info_for(cluster_idx, node)
                                .map(|c| c.cpu_count),
                        )),
                    )),
                    None => pending(),
                };

                let cores = match self.get_node_cpu_info_for(cluster_idx, node) {
                    Some(c) => Cell::from(c.cpu_count.to_string()),
                    None => pending(),
                };

                let uptime = match self
                    .get_node_system_stat_for(cluster_idx, node)
                    .and_then(|s| s.uptime_secs(now))
                {
                    Some(secs) => Cell::from(format_uptime(secs)),
                    None => pending(),
                };

                let disk = match self
                    .get_node_mounts_for(cluster_idx, node)
                    .map(|m| m.var().and_then(|v| v.used_pct().map(|pct| (v, pct))))
                {
                    Some(Some((var, pct))) => Cell::from(Line::from(vec![
                        Span::styled(
                            format!("{pct:>3.0}%"),
                            Style::default().fg(memory_color(pct)),
                        ),
                        Span::styled(format!(" {:.0}G free", gib(var.available)), dim),
                    ])),
                    // Node answered but has no /var mount: nothing to show
                    Some(None) => Cell::from(Span::styled("-", dim)),
                    None => pending(),
                };

                // Fullest pool, so the column flags the one that needs attention
                let zfs = self
                    .get_node_zfs_pools_for(cluster_idx, node)
                    .iter()
                    .filter_map(|p| p.used_pct().map(|pct| (p, pct)))
                    .max_by(|a, b| a.1.total_cmp(&b.1));
                let zfs = match zfs {
                    Some((pool, pct)) => Cell::from(Line::from(vec![
                        Span::styled(
                            format!("{pct:>3.0}%"),
                            Style::default().fg(memory_color(pct)),
                        ),
                        Span::styled(format!(" {:.0}G free", gib(pool.free)), dim),
                    ])),
                    None => Cell::from(Span::styled("-", dim)),
                };

                let net = self.get_node_network_info_for(cluster_idx, node);
                let gw = match net {
                    Some(n) if n.gateways.is_empty() => Cell::from(Span::styled("-", dim)),
                    Some(n) => Cell::from(n.gateways.join(", ")),
                    None => pending(),
                };
                let conn = match net.and_then(|n| n.connectivity_ready) {
                    Some(true) => Cell::from(Span::styled("✓", Style::default().fg(Color::Green))),
                    Some(false) => Cell::from(Span::styled("✗", Style::default().fg(Color::Red))),
                    None => pending(),
                };
                // Peers up out of all: a node whose links are down shows red
                let kspan = match net.map(|n| n.kubespan_peers.as_ref()) {
                    None => pending(),
                    Some(None) => Cell::from(Span::styled("…", dim)),
                    Some(Some(peers)) if peers.is_empty() => Cell::from(Span::styled("-", dim)),
                    Some(Some(peers)) => {
                        let links = kubespan_links(peers);
                        Cell::from(Span::styled(
                            format!("{}/{}", links.up, links.total),
                            Style::default().fg(if links.down.is_empty() {
                                Color::Green
                            } else {
                                Color::Red
                            }),
                        ))
                    }
                };
                let tailscale = match net.map(|n| n.tailscale.as_ref()) {
                    None => pending(),
                    Some(None) => Cell::from(Span::styled("-", dim)),
                    Some(Some(ts)) => match (ts.up, tailnet_ip(ts)) {
                        (true, Some(ip)) => Cell::from(ip.to_string()),
                        (true, None) => {
                            Cell::from(Span::styled("no IP", Style::default().fg(Color::Red)))
                        }
                        (false, _) => {
                            Cell::from(Span::styled("down", Style::default().fg(Color::Red)))
                        }
                    },
                };
                let clock = match self.get_node_clock_for(cluster_idx, node) {
                    Some(clock) => Cell::from(clock_span(clock)),
                    None if net.is_some() => Cell::from(Span::styled("?", dim)),
                    None => pending(),
                };

                let svc = match self.get_node_services_for(cluster_idx, node) {
                    Some(services) => {
                        let ok = services.iter().filter(|s| service_ok(s)).count();
                        let color = if ok == services.len() {
                            Color::Green
                        } else {
                            Color::Red
                        };
                        Cell::from(Span::styled(
                            format!("{}/{}", ok, services.len()),
                            Style::default().fg(color),
                        ))
                    }
                    None => pending(),
                };

                let mut cells = vec![
                    Cell::from(Span::styled(
                        node.clone(),
                        Style::default().fg(Color::White),
                    )),
                    Cell::from(Span::styled(role, dim)),
                    version_cell(
                        self.get_node_talos_version_for(cluster_idx, node),
                        talos_common,
                    ),
                    version_cell(
                        self.get_node_kubelet_version_for(cluster_idx, node),
                        k8s_common,
                    ),
                    mem,
                    disk,
                ];
                if has_zfs {
                    cells.push(zfs);
                }
                cells.push(load);
                if wide {
                    cells.push(cores);
                }
                cells.extend([uptime, conn, svc]);
                if has_kubespan {
                    cells.push(kspan);
                }
                if has_tailscale {
                    cells.push(tailscale);
                }
                cells.extend([clock, gw]);
                Row::new(cells)
            })
            .collect();

        let mut header = vec!["Node", "Role", "Talos", "K8s", "Mem", "Disk"];
        let name_width = nodes.iter().map(|n| n.chars().count()).max().unwrap_or(0);
        let version_width = |versions: Vec<Option<&str>>| {
            versions
                .into_iter()
                .flatten()
                .map(|v| v.chars().count())
                .max()
                .unwrap_or(0)
                .max(5) as u16
        };
        let talos_width = version_width(
            nodes
                .iter()
                .map(|n| self.get_node_talos_version_for(cluster_idx, n))
                .collect(),
        );
        let k8s_width = version_width(
            nodes
                .iter()
                .map(|n| self.get_node_kubelet_version_for(cluster_idx, n))
                .collect(),
        );
        let mut widths = vec![
            Constraint::Length(name_width.max(4) as u16),
            Constraint::Length(6),
            Constraint::Length(talos_width),
            Constraint::Length(k8s_width),
            Constraint::Length(9),
            Constraint::Length(14),
        ];
        if has_zfs {
            header.push("ZFS");
            widths.push(Constraint::Length(15));
        }
        header.push("Load");
        widths.push(Constraint::Length(5));
        if wide {
            header.push("Cores");
            widths.push(Constraint::Length(5));
        }
        header.extend(["Uptime", "Conn", "Svc"]);
        widths.extend([
            Constraint::Length(8),
            Constraint::Length(4),
            Constraint::Length(5),
        ]);
        if has_kubespan {
            header.push("KSpan");
            widths.push(Constraint::Length(5));
        }
        if has_tailscale {
            header.push("Tailscale");
            widths.push(Constraint::Length(15));
        }
        header.extend(["Clock", "GW"]);
        widths.extend([Constraint::Length(7), Constraint::Fill(1)]);

        let header = Row::new(header).style(Style::default().fg(Color::Gray).bold());
        let table = Table::new(rows, widths).header(header).column_spacing(2);

        // The sections are drawn at full height and scrolled as a whole when
        // the pane is shorter than they are (wheel)
        let key = format!("summary:{group}");
        if self.details_scroll_node != key {
            self.details_scroll_node = key;
            self.details_scroll = 0;
        }
        let linstor = self.linstor_rows_for(cluster_idx, &nodes);
        let linstor_height = match &linstor {
            Some(Ok(rows)) => rows.len() as u16 + 2, // header + rows + spacer
            Some(Err(_)) => 2,
            None => 0,
        };
        let kubespan = if has_kubespan {
            self.kubespan_summary_lines(cluster_idx, &nodes)
        } else {
            Vec::new()
        };
        let kubespan_height = kubespan.len() as u16;
        // One row per service seen on any node, plus the header
        let services_height = {
            let mut ids: Vec<&str> = nodes
                .iter()
                .filter_map(|n| self.get_node_services_for(cluster_idx, n))
                .flat_map(|services| services.iter().map(|s| s.id.as_str()))
                .collect();
            ids.sort_unstable();
            ids.dedup();
            ids.len() as u16 + 1
        };
        let heights = [
            nodes.len() as u16 + 2, // header + rows + spacer
            linstor_height,
            services_height + u16::from(kubespan_height > 0), // spacer before KubeSpan
            kubespan_height,
        ];
        let max_scroll = crate::ui_ext::render_scrolled(
            frame,
            inner,
            heights.iter().sum(),
            self.details_scroll as u16,
            |buf, full| {
                let [table_area, linstor_area, services_area, kubespan_area] =
                    Layout::vertical(heights.map(Constraint::Length)).areas(full);
                table.render(table_area, buf);
                if let Some(linstor) = linstor {
                    draw_linstor_table(buf, linstor_area, linstor);
                }
                if kubespan_height > 0 {
                    Paragraph::new(kubespan.clone()).render(kubespan_area, buf);
                }
                self.draw_services_matrix(buf, services_area, cluster_idx, &nodes);
            },
        );
        self.details_max_scroll = max_scroll as usize;
        self.details_scroll = self.details_scroll.min(self.details_max_scroll);
    }

    /// LINSTOR nodes to show for the selection: all of them for a cluster
    /// header, otherwise those of the group's nodes. `None` when the cluster
    /// doesn't run Piraeus (or the group has no LINSTOR nodes); `Err` when the
    /// controller API is unreachable.
    pub(super) fn linstor_rows_for<'a>(
        &'a self,
        cluster_idx: usize,
        nodes: &[String],
    ) -> Option<Result<Vec<&'a LinstorNode>, &'a str>> {
        let whole_cluster = matches!(self.selected_item, NodeListItem::ClusterHeader(_));
        match self.clusters.get(cluster_idx)?.linstor.as_ref()? {
            LinstorStatus::Unreachable(e) => Some(Err(e.as_str())),
            LinstorStatus::Nodes(all) => {
                let rows: Vec<&LinstorNode> = all
                    .iter()
                    .filter(|l| {
                        whole_cluster || nodes.iter().any(|n| n.eq_ignore_ascii_case(&l.name))
                    })
                    .collect();
                (whole_cluster || !rows.is_empty()).then_some(Ok(rows))
            }
        }
    }

    /// KubeSpan on the dashboard: one line when every link is up, otherwise
    /// the links that aren't, one per line. Grows with the problems, not
    /// with the nodes. Node-to-node traffic such as DRBD replication and
    /// etcd runs over these links.
    pub(super) fn kubespan_summary_lines(
        &self,
        cluster_idx: usize,
        nodes: &[String],
    ) -> Vec<Line<'static>> {
        let dim = Style::default().dim();
        let mut up = 0;
        let mut total = 0;
        let mut unread = 0;
        let mut problems: Vec<Line<'static>> = Vec::new();
        for node in nodes {
            let peers = self
                .get_node_network_info_for(cluster_idx, node)
                .and_then(|net| net.kubespan_peers.as_ref());
            match peers {
                None => unread += 1,
                Some(peers) if peers.is_empty() => problems.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{node} has no peers"),
                        Style::default().fg(Color::Yellow),
                    ),
                ])),
                Some(peers) => {
                    let links = kubespan_links(peers);
                    up += links.up;
                    total += links.total;
                    for (peer, state) in links.down {
                        problems.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(
                                format!("{node} → {peer} {state}"),
                                Style::default().fg(Color::Red),
                            ),
                        ]));
                    }
                }
            }
        }
        let title_style = if problems.is_empty() {
            Style::default().fg(Color::Gray).bold()
        } else {
            Style::default().fg(Color::Red).bold()
        };
        let mut head = vec![
            Span::styled("KubeSpan", title_style),
            Span::raw("  "),
            Span::styled(
                format!("{up}/{total} links up"),
                Style::default().fg(if up == total {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
        ];
        if unread > 0 {
            head.push(Span::styled(format!("  {unread} nodes not read yet"), dim));
        }
        let mut lines = vec![Line::from(head)];
        lines.extend(problems);
        lines
    }

    /// KubeSpan peer state between the group's nodes: one row per node (as
    /// it sees its peers), one column per peer.
    pub(super) fn draw_kubespan_matrix(
        &self,
        buf: &mut Buffer,
        area: Rect,
        cluster_idx: usize,
        nodes: &[String],
    ) {
        let dim = Style::default().dim();
        let mut down = 0;
        let rows: Vec<Row> = nodes
            .iter()
            .map(|node| {
                let peers = self
                    .get_node_network_info_for(cluster_idx, node)
                    .and_then(|net| net.kubespan_peers.as_ref());
                let mut cells = vec![Cell::from(node.clone())];
                cells.extend(nodes.iter().map(|peer| {
                    let (symbol, style) = if peer == node {
                        (" ", dim)
                    } else {
                        match peers {
                            // Not read yet, or unreadable
                            None => ("…", dim),
                            Some(peers) => {
                                match peers.iter().find(|p| p.label.eq_ignore_ascii_case(peer)) {
                                    // Not a KubeSpan peer of this node
                                    None => ("·", dim),
                                    Some(p) => match p.state.as_str() {
                                        "up" => ("●", Style::default().fg(Color::Green)),
                                        "down" => {
                                            down += 1;
                                            ("✗", Style::default().fg(Color::Red))
                                        }
                                        _ => ("?", Style::default().fg(Color::Yellow)),
                                    },
                                }
                            }
                        }
                    };
                    Cell::from(Span::styled(symbol, style))
                }));
                Row::new(cells)
            })
            .collect();

        let title = if down > 0 {
            format!("KubeSpan ({down} links down)")
        } else {
            "KubeSpan".to_string()
        };
        let name_width = nodes
            .iter()
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0)
            .max(title.chars().count());
        let mut widths = vec![Constraint::Length(name_width as u16)];
        widths.extend(
            nodes
                .iter()
                .map(|n| Constraint::Length(n.chars().count().max(1) as u16)),
        );
        let mut header = vec![Cell::from(Span::styled(
            title,
            if down > 0 {
                Style::default().fg(Color::Red).bold()
            } else {
                Style::default().fg(Color::Gray).bold()
            },
        ))];
        header.extend(nodes.iter().map(|n| Cell::from(n.clone())));
        let header = Row::new(header).style(Style::default().fg(Color::Gray).bold());
        Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .render(area, buf);
    }

    /// Service × node matrix: one row per service seen on any node in the
    /// group, one column per node. Rows with a problem on any node are listed
    /// first so they are visible even when the list is cut off.
    pub(super) fn draw_services_matrix(
        &self,
        buf: &mut Buffer,
        area: Rect,
        cluster_idx: usize,
        nodes: &[String],
    ) {
        let per_node: Vec<Option<&Vec<ServiceInfo>>> = nodes
            .iter()
            .map(|n| self.get_node_services_for(cluster_idx, n))
            .collect();
        let offline: Vec<bool> = nodes
            .iter()
            .map(|n| self.node_offline_for(cluster_idx, n).is_some())
            .collect();

        let mut ids: Vec<&str> = per_node
            .iter()
            .flatten()
            .flat_map(|services| services.iter().map(|s| s.id.as_str()))
            .collect();
        ids.sort_unstable();
        ids.dedup();

        let find = |services: Option<&Vec<ServiceInfo>>, id: &str| -> Option<ServiceInfo> {
            services?.iter().find(|s| s.id == id).cloned()
        };
        // A service is a problem if it is unhealthy anywhere it runs; being
        // absent from some nodes is normal (control-plane-only services).
        let has_problem = |id: &str| {
            per_node
                .iter()
                .any(|services| find(*services, id).is_some_and(|s| !service_ok(&s)))
        };
        let (mut problems, ok): (Vec<&str>, Vec<&str>) =
            ids.into_iter().partition(|id| has_problem(id));
        let problem_count = problems.len();
        problems.extend(ok);

        let title = if problem_count == 0 {
            "Services".to_string()
        } else {
            format!("Services ({problem_count} with issues)")
        };
        let name_width = problems
            .iter()
            .map(|id| id.len())
            .max()
            .unwrap_or(0)
            .max(title.len());
        let header_style = Style::default().fg(Color::Gray).bold();

        // One column per node doesn't fit past a dozen nodes: then each
        // service gets a row with the nodes it is healthy on out of the nodes
        // that have it, and the others by name. Grows with the problems.
        if matrix_width(name_width, nodes) > area.width {
            let rows: Vec<Row> = problems
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let name_style = if i < problem_count {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let mut total = 0;
                    let mut bad: Vec<Span> = Vec::new();
                    for (node, services) in nodes.iter().zip(&per_node) {
                        let Some(s) = find(*services, id) else {
                            continue;
                        };
                        total += 1;
                        if !service_ok(&s) {
                            let (symbol, style) = service_symbol(&s);
                            bad.push(Span::styled(format!("{symbol} {node}"), style));
                            bad.push(Span::raw("  "));
                        }
                    }
                    let healthy = total - bad.len() / 2;
                    Row::new(vec![
                        Cell::from(Span::styled(id.to_string(), name_style)),
                        Cell::from(Span::styled(
                            format!("{healthy}/{total}"),
                            Style::default().fg(if healthy == total {
                                Color::Green
                            } else {
                                Color::Red
                            }),
                        )),
                        Cell::from(Line::from(bad)),
                    ])
                })
                .collect();
            // The problem count goes with the problem column here, which
            // keeps the name column at the names' width
            let not_healthy = if problem_count == 0 {
                "Not healthy".to_string()
            } else {
                format!("Not healthy ({problem_count})")
            };
            let name_width = problems.iter().map(|id| id.len()).max().unwrap_or(0).max(8);
            let header = Row::new(vec![
                "Services".to_string(),
                "Healthy".to_string(),
                not_healthy,
            ])
            .style(header_style);
            Table::new(
                rows,
                [
                    Constraint::Length(name_width as u16),
                    Constraint::Length(7),
                    Constraint::Fill(1),
                ],
            )
            .header(header)
            .column_spacing(2)
            .render(area, buf);
            return;
        }

        let dim = Style::default().dim();
        let rows: Vec<Row> = problems
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let name_style = if i < problem_count {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::White)
                };
                let mut cells = vec![Cell::from(Span::styled(id.to_string(), name_style))];
                cells.extend(per_node.iter().zip(&offline).map(|(services, &offline)| {
                    let (symbol, style) = match services {
                        None if offline => ("-", dim),
                        None => ("…", dim),
                        Some(_) => match find(*services, id) {
                            None => ("·", dim),
                            Some(s) => service_symbol(&s),
                        },
                    };
                    Cell::from(Span::styled(symbol, style))
                }));
                Row::new(cells)
            })
            .collect();

        let mut widths = vec![Constraint::Length(name_width as u16)];
        widths.extend(
            nodes
                .iter()
                .map(|n| Constraint::Length(n.chars().count().max(1) as u16)),
        );
        let mut header = vec![Cell::from(title)];
        header.extend(nodes.iter().map(|n| Cell::from(n.clone())));
        let header = Row::new(header).style(header_style);
        Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .render(area, buf);
    }
}
