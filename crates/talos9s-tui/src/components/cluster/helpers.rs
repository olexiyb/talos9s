//! Free functions shared by the cluster tree, group table and dashboard

use super::*;

/// Wrap a cluster warning into indented lines so it reads as part of the
/// cluster's expanded tray rather than a detached block clipped at the pane
/// edge. The ⚠ sits on the first line, aligned under the group rows (Control
/// Plane / Workers); continuations hang-indent under the text.
pub(super) fn warning_lines(text: &str, pane_width: u16) -> Vec<Line<'static>> {
    const INDENT: &str = "    "; // aligns ⚠ under the "▼ Control Plane" group rows
    const HANG: &str = "      "; // continuations align under the wrapped text
    let avail = (pane_width as usize).saturating_sub(HANG.len()).max(12);

    let wrapped = talos9s_core::wrap_words(text.split_whitespace(), avail);
    let style = Style::default().fg(Color::Yellow);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let rendered = if i == 0 {
                format!("{INDENT}⚠ {line}")
            } else {
                format!("{HANG}{line}")
            };
            Line::from(Span::styled(rendered, style))
        })
        .collect()
}

/// A clock offset as `+12ms` / `-1.5s`, colored by size
pub(super) fn clock_span(clock: &NodeClock) -> Span<'static> {
    let o = clock.offset_secs;
    let text = if o.abs() < 1.0 {
        format!("{:+.0}ms", o * 1000.0)
    } else {
        format!("{o:+.1}s")
    };
    let color = if o.abs() >= CLOCK_ERROR_SECS {
        Color::Red
    } else if o.abs() >= CLOCK_WARN_SECS {
        Color::Yellow
    } else {
        Color::Green
    };
    Span::styled(text, Style::default().fg(color))
}

/// A node's tailnet IPv4 (or first) address without its prefix
pub(super) fn tailnet_ip(ts: &talos_rs::TailscaleLink) -> Option<&str> {
    let addr = ts
        .addresses
        .iter()
        .find(|a| !a.contains(':'))
        .or_else(|| ts.addresses.first())?;
    Some(addr.split('/').next().unwrap_or(addr))
}

/// Whether a service is in a good state: running and not failing its health
/// check, a one-shot service that finished (e.g. `ext-nvidia-cdi-gen`), or
/// one the config leaves out.
pub(super) fn service_ok(s: &ServiceInfo) -> bool {
    match s.state.as_str() {
        "Running" => s.is_healthy() != Some(false),
        "Finished" | "Skipped" => true,
        _ => false,
    }
}

/// The tree's dot for a node: every service as the services matrix judges
/// it, and memory below critical. What isn't known yet doesn't count.
pub(super) fn node_row_ok(memory_pct: Option<f64>, services: Option<&[ServiceInfo]>) -> bool {
    let services_ok = services.is_none_or(|services| services.iter().all(service_ok));
    let memory_ok = memory_pct
        .is_none_or(|pct| talos9s_core::memory_health(pct) != talos9s_core::HealthIndicator::Error);
    services_ok && memory_ok
}

/// A node's KubeSpan links as it sees them: how many peers are up, and the
/// ones that aren't
pub(super) struct KubeSpanLinks {
    pub(super) up: usize,
    pub(super) total: usize,
    /// Peers not up, with their state (`down`, `unknown`, ...)
    pub(super) down: Vec<(String, String)>,
}

pub(super) fn kubespan_links(peers: &[talos_rs::KubeSpanPeerStatus]) -> KubeSpanLinks {
    let mut down: Vec<(String, String)> = peers
        .iter()
        .filter(|p| p.state != "up")
        .map(|p| (p.label.clone(), p.state.clone()))
        .collect();
    down.sort();
    KubeSpanLinks {
        up: peers.len() - down.len(),
        total: peers.len(),
        down,
    }
}

/// Columns the KubeSpan matrix needs
pub(super) fn kubespan_matrix_width(nodes: &[String]) -> u16 {
    let name_width = nodes.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    matrix_width(name_width.max("KubeSpan".len()), nodes)
}

/// Columns a node × node or service × node matrix needs: the name column,
/// then one per node (two spaces apart)
pub(super) fn matrix_width(name_width: usize, nodes: &[String]) -> u16 {
    let columns: usize = nodes.iter().map(|n| n.chars().count().max(1) + 2).sum();
    (name_width + columns) as u16
}

/// What the service symbols mean, for a window's bottom border: it stays while
/// the content scrolls. `absent` adds the matrix-only entry for a service a
/// node doesn't have.
pub(super) fn services_legend(absent: bool) -> Line<'static> {
    let dim = Style::default().dim();
    let mut spans = vec![
        Span::styled(" ●", Style::default().fg(Color::Green)),
        Span::styled(" healthy  ", dim),
        Span::styled("●", Style::default().fg(Color::DarkGray)),
        Span::styled(" no check  ", dim),
        Span::styled("✓", Style::default().fg(Color::Green)),
        Span::styled(" finished  ", dim),
        Span::styled("○", Style::default().fg(Color::Red)),
        Span::styled(" unhealthy  ", dim),
        Span::styled("✗", Style::default().fg(Color::Red)),
        Span::styled(" not running ", dim),
    ];
    if absent {
        spans.push(Span::styled(" ·", dim));
        spans.push(Span::styled(" absent ", dim));
    }
    Line::from(spans)
}

/// Matrix symbol and style for one service on one node
pub(super) fn service_symbol(s: &ServiceInfo) -> (&'static str, Style) {
    match (s.state.as_str(), s.is_healthy()) {
        ("Running", Some(true)) => ("●", Style::default().fg(Color::Green)),
        ("Running", None) => ("●", Style::default().fg(Color::DarkGray)),
        ("Running", Some(false)) => ("○", Style::default().fg(Color::Red)),
        ("Finished", _) => ("✓", Style::default().fg(Color::Green)),
        _ => ("✗", Style::default().fg(Color::Red)),
    }
}

/// Current time in seconds since the Unix epoch
pub(super) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format an uptime in seconds, e.g. "5d 17h"
pub(super) fn format_uptime(secs: u64) -> String {
    talos9s_core::format_duration_detailed(chrono::Duration::seconds(secs as i64))
}

/// Color for a memory usage percentage
/// The dashboard's "Diag:" line: what the last diagnostics run found
pub(super) fn diagnostics_line(summary: &DiagnosticsSummary) -> Line<'static> {
    let age = talos9s_core::format_duration_human(
        chrono::Duration::from_std(summary.checked_at.elapsed()).unwrap_or_default(),
    );
    let mut spans = vec![Span::styled(" Diag:   ", Style::default().dim())];
    if summary.failures.is_empty() && summary.warnings.is_empty() {
        spans.push(Span::styled(
            "● all checks pass",
            Style::default().fg(Color::Green),
        ));
    } else {
        let (symbol, color) = if summary.failures.is_empty() {
            ("◐", Color::Yellow)
        } else {
            ("✗", Color::Red)
        };
        let mut counts = Vec::new();
        if !summary.failures.is_empty() {
            counts.push(talos9s_core::pluralize(
                summary.failures.len(),
                "failure",
                "failures",
            ));
        }
        if !summary.warnings.is_empty() {
            counts.push(talos9s_core::pluralize(
                summary.warnings.len(),
                "warning",
                "warnings",
            ));
        }
        let names: Vec<&str> = summary
            .failures
            .iter()
            .chain(&summary.warnings)
            .map(String::as_str)
            .collect();
        spans.push(Span::styled(
            format!("{symbol} {}: {}", counts.join(", "), names.join(", ")),
            Style::default().fg(color),
        ));
    }
    spans.push(Span::styled(
        format!("  {age} ago  [d]"),
        Style::default().dim(),
    ));
    Line::from(spans)
}

/// Color of a fill level (memory, storage pools), with the thresholds the
/// diagnostics judge memory by
pub(super) fn memory_color(pct: f32) -> Color {
    talos9s_core::memory_health(pct as f64).color()
}

/// LINSTOR node list (`linstor node list`) of a Piraeus cluster, or why the
/// controller couldn't be queried
pub(super) fn draw_linstor_table(
    buf: &mut Buffer,
    area: Rect,
    linstor: Result<Vec<&LinstorNode>, &str>,
) {
    let dim = Style::default().dim();
    let rows = match linstor {
        Ok(rows) => rows,
        Err(e) => {
            Paragraph::new(Line::from(vec![
                Span::styled("LINSTOR  ", Style::default().fg(Color::Gray).bold()),
                Span::styled(
                    format!("controller API unreachable: {e}"),
                    Style::default().fg(Color::Yellow),
                ),
            ]))
            .render(area, buf);
            return;
        }
    };

    let online = rows.iter().filter(|n| n.is_online()).count();
    let title = if rows.is_empty() {
        "LINSTOR (no nodes)".to_string()
    } else {
        format!("LINSTOR ({}/{} online)", online, rows.len())
    };
    let name_width = rows
        .iter()
        .map(|n| n.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count());
    let table_rows: Vec<Row> = rows
        .iter()
        .map(|node| {
            let mut state = node.state.clone();
            for flag in &node.flags {
                state.push_str(", ");
                state.push_str(flag);
            }
            let state_color = if node.is_online() {
                Color::Green
            } else if node.state.eq_ignore_ascii_case("ONLINE") {
                Color::Yellow
            } else {
                Color::Red
            };
            Row::new(vec![
                Cell::from(node.name.clone()),
                Cell::from(Span::styled(node.node_type.to_lowercase(), dim)),
                Cell::from(Span::styled(state, Style::default().fg(state_color))),
                Cell::from(pools_line(node.pools.as_deref())),
                Cell::from(Span::styled(node.addresses.join(", "), dim)),
            ])
        })
        .collect();
    let state_width = rows
        .iter()
        .map(|n| {
            n.state.chars().count() + n.flags.iter().map(|f| f.chars().count() + 2).sum::<usize>()
        })
        .max()
        .unwrap_or(0)
        .max(5);
    let pools_width = rows
        .iter()
        .map(|n| pools_line(n.pools.as_deref()).width())
        .max()
        .unwrap_or(0)
        .max(5);
    let widths = [
        Constraint::Length(name_width as u16),
        Constraint::Length(9),
        Constraint::Length(state_width as u16),
        Constraint::Length(pools_width as u16),
        Constraint::Fill(1),
    ];
    let header = Row::new(vec![title.as_str(), "Type", "State", "Pools", "Addresses"])
        .style(Style::default().fg(Color::Gray).bold());
    Table::new(table_rows, widths)
        .header(header)
        .column_spacing(2)
        .render(area, buf);
}

/// A node's LINSTOR storage pools, e.g. `datapool 287/840G 34%`: usage
/// colored like memory, a pool with error/warning reports in red. `?` when
/// the pool list couldn't be fetched, `-` for a node without pools.
pub(super) fn pools_line(pools: Option<&[StoragePool]>) -> Line<'static> {
    let dim = Style::default().dim();
    let Some(pools) = pools else {
        return Line::from(Span::styled("?", dim));
    };
    if pools.is_empty() {
        return Line::from(Span::styled("-", dim));
    }
    let mut spans = Vec::new();
    for (i, pool) in pools.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(", ", dim));
        }
        let name_style = if pool.has_problems {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        spans.push(Span::styled(pool.name.clone(), name_style));
        match (pool.free, pool.total, pool.used_pct()) {
            (Some(free), Some(total), Some(pct)) => {
                spans.push(Span::styled(
                    format!(" {}", used_of_total(total.saturating_sub(free), total)),
                    dim,
                ));
                spans.push(Span::styled(
                    format!(" {:.0}%", pct),
                    Style::default().fg(memory_color(pct)),
                ));
            }
            _ => spans.push(Span::styled(" ?", dim)),
        }
    }
    Line::from(spans)
}

/// `used/total` in the total's unit, e.g. `287/840G` or `0.4/6.8T`
pub(super) fn used_of_total(used: u64, total: u64) -> String {
    const TIB: f64 = (1u64 << 40) as f64;
    if total as f64 >= TIB {
        let precision = |v: f64| if v < 10.0 { 1 } else { 0 };
        let (u, t) = (used as f64 / TIB, total as f64 / TIB);
        format!("{u:.p$}/{t:.q$}T", p = precision(u), q = precision(t))
    } else {
        format!("{:.0}/{:.0}G", gib(used), gib(total))
    }
}

/// Bytes to GiB, for display
pub(super) fn gib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / 1024.0
}

/// Color for a 1-minute load average
/// Color of a load average for the node's core count (dim while unknown),
/// with the thresholds the diagnostics use
pub(super) fn load_color(load1: f64, cpu_count: Option<usize>) -> Color {
    talos9s_core::load_health(load1, cpu_count).color()
}

/// Render `label` followed by `items` joined with ", ", breaking only between
/// items so an address is never split. Continuation lines are indented to
/// align with the first item. Returns no lines when `items` is empty.
pub(super) fn wrap_list(
    label: &str,
    items: &[String],
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let indent = label.chars().count();
    // Commas stay with the item before them
    let pieces: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            if i + 1 < items.len() {
                format!("{item},")
            } else {
                item.clone()
            }
        })
        .collect();
    let rows = talos9s_core::wrap_words(
        pieces.iter().map(String::as_str),
        width.saturating_sub(indent),
    );
    rows.into_iter()
        .enumerate()
        .map(|(i, row)| {
            let prefix = if i == 0 {
                Span::styled(label.to_string(), Style::default().dim())
            } else {
                Span::raw(" ".repeat(indent))
            };
            Line::from(vec![prefix, Span::styled(row, style)])
        })
        .collect()
}

/// Extract the deepest, most actionable message from an error chain.
///
/// tonic's transport-error `Display` is just "transport error"; the real cause
/// (e.g. "Connection refused", "i/o timeout") lives at the bottom of the
/// `source()` chain.
pub(super) fn root_cause(e: &dyn std::error::Error) -> String {
    let mut deepest: &dyn std::error::Error = e;
    while let Some(src) = deepest.source() {
        deepest = src;
    }
    deepest.to_string()
}
