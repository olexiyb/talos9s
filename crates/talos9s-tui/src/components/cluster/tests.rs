//! Tests of the cluster component

use super::*;
use talos_rs::{DiscoveryMember, NodeServices, ServiceInfo, VersionInfo};

fn ver(node: &str) -> VersionInfo {
    VersionInfo {
        node: node.to_string(),
        version: String::new(),
        sha: String::new(),
        built: String::new(),
        go_version: String::new(),
        os: String::new(),
        arch: String::new(),
        platform: String::new(),
    }
}

fn member(hostname: &str, ip: &str, machine_type: &str) -> DiscoveryMember {
    DiscoveryMember {
        id: hostname.to_string(),
        addresses: vec![ip.to_string()],
        hostname: hostname.to_string(),
        machine_type: machine_type.to_string(),
        operating_system: String::new(),
    }
}

fn services_with(node: &str, ids: &[&str]) -> NodeServices {
    NodeServices {
        node: node.to_string(),
        services: ids
            .iter()
            .map(|id| ServiceInfo {
                id: id.to_string(),
                state: "Running".to_string(),
                health: None,
            })
            .collect(),
    }
}

fn names(nodes: &[(usize, &VersionInfo)]) -> Vec<String> {
    nodes.iter().map(|(_, v)| v.node.clone()).collect()
}

/// A first load that never finishes or fails turns into an error and is
/// retried, instead of showing "connecting…" for good
#[test]
fn stuck_or_failed_first_load_is_an_error() {
    let connecting = || ClusterData {
        name: "kprod".to_string(),
        loading: true,
        ..Default::default()
    };

    let timed_out = connecting().refresh_timed_out();
    assert!(!timed_out.loading);
    assert!(!timed_out.connected);
    assert_eq!(
        timed_out.error.as_deref(),
        Some("No answer within 120s — retrying")
    );

    let mut failed = connecting();
    failed.refresh_failed("Refresh failed — panicked".to_string());
    assert!(!failed.loading);
    assert_eq!(failed.error.as_deref(), Some("Refresh failed — panicked"));
}

/// A refresh that times out after the first load keeps the data shown
#[test]
fn stuck_refresh_keeps_shown_data() {
    let loaded = ClusterData {
        name: "kprod".to_string(),
        connected: true,
        versions: vec![ver("cp1")],
        ..Default::default()
    };
    let timed_out = loaded.refresh_timed_out();
    assert_eq!(timed_out.versions.len(), 1);
    assert!(timed_out.client.is_none());
    assert!(timed_out.connected);
    assert_eq!(timed_out.error, None);
}

/// Discovery machineType is authoritative: a discovered worker is classified
/// as a worker even when no per-node service data is available. This is the
/// regression guard for "workers missing / shown only as control plane"
/// (the "worker nodes missing" reports) — workers must land in the worker group.
#[test]
fn classifies_by_discovery_machine_type() {
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        versions: vec![ver("cp1"), ver("worker1")],
        discovery_members: vec![
            member("cp1", "10.0.0.1", "controlplane"),
            member("worker1", "10.0.0.2", "worker"),
        ],
        ..Default::default()
    });

    assert_eq!(names(&comp.controlplane_nodes_for(0)), vec!["cp1"]);
    assert_eq!(names(&comp.worker_nodes_for(0)), vec!["worker1"]);
}

fn two_node_cluster() -> ClusterComponent {
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        versions: vec![ver("cp1"), ver("worker1")],
        discovery_members: vec![
            member("cp1", "10.0.0.1", "controlplane"),
            member("worker1", "10.0.0.2", "worker"),
        ],
        ..Default::default()
    });
    comp
}

/// A refresh that adds or drops a node keeps the cursor on the node it
/// was on, not on the same row
#[test]
fn selection_stays_on_the_node_across_refreshes() {
    let mut comp = ClusterComponent::default();
    let workers = |names: &[&str]| ClusterData {
        name: "test".to_string(),
        versions: names.iter().map(|n| ver(n)).collect(),
        discovery_members: names
            .iter()
            .enumerate()
            .map(|(i, n)| member(n, &format!("10.0.0.{}", i + 1), "worker"))
            .collect(),
        ..Default::default()
    };
    comp.clusters.push(workers(&["w1", "w2", "w3"]));
    assert!(comp.select_node("w2"));
    assert_eq!(comp.current_node_name().as_deref(), Some("w2"));

    // w1 is gone: w2 moved up a row
    assert!(comp.adopt_cluster(0, workers(&["w2", "w3"])));
    assert_eq!(comp.current_node_name().as_deref(), Some("w2"));

    // A new node in front of it
    assert!(comp.adopt_cluster(0, workers(&["w0", "w2", "w3"])));
    assert_eq!(comp.current_node_name().as_deref(), Some("w2"));
}

/// The workspace follows the selection by these keys, and node-only views
/// give a hint on a group instead of nothing
#[test]
fn selection_keys_and_views_per_target() {
    let mut comp = two_node_cluster();
    comp.selected_item = NodeListItem::ClusterHeader(0);
    assert_eq!(comp.target_key(), "test");
    assert!(matches!(
        comp.action_for(NavMenuItem::Containers),
        Some(Action::ShowHint(NavMenuItem::Containers, ref m)) if m == "Select a node to see its containers"
    ));
    assert!(matches!(
        comp.action_for(NavMenuItem::Network),
        Some(Action::ShowGroup(GroupKind::Network, ..))
    ));
    assert!(matches!(
        comp.action_for(NavMenuItem::Dashboard),
        Some(Action::ShowDashboard)
    ));
    assert!(matches!(
        comp.action_for(NavMenuItem::KubeSpan),
        Some(Action::ShowKubeSpanMatrix(ref nodes)) if nodes == &["cp1", "worker1"]
    ));

    assert!(comp.select_node("worker1"));
    assert_eq!(comp.target_key(), "test/worker1");
    assert_eq!(comp.cluster_key(), "test");
    assert!(matches!(
        comp.action_for(NavMenuItem::Containers),
        Some(Action::ShowNodeInfo(ref n, _, NodeInfoTab::Containers)) if n == "worker1"
    ));
    assert!(matches!(
        comp.action_for(NavMenuItem::Network),
        Some(Action::ShowNetwork(ref n, _)) if n == "worker1"
    ));
    assert!(matches!(
        comp.action_for(NavMenuItem::KubeSpan),
        Some(Action::ShowKubeSpan(ref n, _)) if n == "worker1"
    ));
    assert!(matches!(
        comp.action_for(NavMenuItem::Partitions),
        Some(Action::ShowStorage(ref n, _, StorageViewMode::Partitions)) if n == "worker1"
    ));
    assert!(!comp.select_node("missing"));
}

/// The dashboard line names what the last diagnostics run found; a CNI
/// summary that only repeats its checks is left out
#[test]
fn diagnostics_line_names_the_findings() {
    use crate::components::diagnostics::DiagnosticCheck;
    let categories = vec![
        (
            "CNI (Cilium)",
            vec![
                DiagnosticCheck::warn("hubble_relay", "Hubble Relay", "0/1 ready"),
                DiagnosticCheck::warn("cni", "CNI (Cilium)", "warnings in Hubble Relay"),
            ],
        ),
        (
            "System Health",
            vec![
                DiagnosticCheck::pass("memory", "Memory", "52%"),
                DiagnosticCheck::fail("etcd", "Etcd", "unhealthy", None),
            ],
        ),
    ];
    let summary = DiagnosticsSummary::of(&categories);
    assert_eq!(summary.failures, ["Etcd"]);
    assert_eq!(summary.warnings, ["Hubble Relay"]);
    let text: String = diagnostics_line(&summary)
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert!(
        text.starts_with(" Diag:   ✗ 1 failure, 1 warning: Etcd, Hubble Relay"),
        "{text}"
    );
    assert!(text.ends_with("ago  [d]"), "{text}");

    let clean = DiagnosticsSummary::of(&[(
        "System Health",
        vec![DiagnosticCheck::pass("memory", "Memory", "52%")],
    )]);
    let text: String = diagnostics_line(&clean)
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert!(text.contains("● all checks pass"), "{text}");
}

/// The dashboard's colors use the diagnostics' thresholds: load 5.5 is
/// fine on 32 cores
#[test]
fn dashboard_colors_use_shared_thresholds() {
    assert_eq!(load_color(5.5, Some(32)), Color::Green);
    assert_eq!(load_color(5.5, Some(2)), Color::Red);
    assert_eq!(memory_color(75.0), Color::Green);
    assert_eq!(memory_color(85.0), Color::Yellow);
}

/// Clicks in the nodes pane: a tree row, a menu entry, or empty space
#[test]
fn click_targets_in_the_nodes_pane() {
    use ratatui::{Terminal, backend::TestBackend};
    let mut comp = two_node_cluster();
    if let Some(c) = comp.clusters.get_mut(0) {
        c.expanded = true;
    }
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    let mut tree = Rect::default();
    terminal
        .draw(|f| tree = comp.draw_frame(f, f.area(), None, false, false, None).0)
        .unwrap();
    let (list, menu) = {
        let hits = comp.hits.borrow();
        (hits.node_list, hits.menu)
    };
    // First row of the list is the cluster header
    assert_eq!(comp.click_target(list.x + 2, list.y), ClickTarget::Row);
    // Menu row 0 is the separator, row 1 the first entry
    assert_eq!(comp.click_target(menu.x + 2, menu.y), ClickTarget::None);
    assert_eq!(comp.click_target(menu.x + 2, menu.y + 1), ClickTarget::Menu);
    // Empty space under the tree, and the pane's border
    assert_eq!(
        comp.click_target(list.x + 2, list.y + list.height - 1),
        ClickTarget::None
    );
    assert_eq!(comp.click_target(tree.x, tree.y), ClickTarget::None);
}

/// A refresh in progress: answered nodes show their data, the others
/// keep what was shown or get a placeholder row, and a node that didn't
/// answer is offline. Light refreshes keep the detailed columns.
#[test]
fn assembling_nodes_while_a_refresh_runs() {
    let before = ClusterData {
        versions: vec![ver("cp1")],
        services: vec![services_with("cp1", &["apid"])],
        load_avg: vec![NodeLoadAvg {
            node: "cp1".to_string(),
            load1: 1.0,
            load5: 1.0,
            load15: 1.0,
        }],
        ..Default::default()
    };
    let nodes = [
        ("cp1".to_string(), "10.0.0.1".to_string()),
        ("w1".to_string(), "10.0.0.2".to_string()),
        ("w2".to_string(), "10.0.0.3".to_string()),
    ];
    let answered = NodeFetch {
        version: Ok(vec![ver("10.0.0.2")]),
        services: Some(vec![services_with("10.0.0.2", &["kubelet"])]),
        memory: None,
        load_avg: None,
        cpu_info: None,
        system_stat: None,
        mounts: None,
    };
    let offline = NodeFetch {
        version: Err(None),
        ..answered.clone()
    };

    // Nothing answered yet: cp1 as before, w1 and w2 listed as placeholders
    let mut cluster = before.clone();
    let results = vec![None, None, None];
    cluster.assemble_nodes(&before, &nodes, &results, &[], false);
    let listed: Vec<&str> = cluster.versions.iter().map(|v| v.node.as_str()).collect();
    assert_eq!(listed, ["cp1", "w1", "w2"]);
    assert_eq!(cluster.versions[1].version, "");
    assert_eq!(cluster.services.len(), 1);
    assert!(cluster.offline.is_empty());

    // w1 answered (named by the roster), w2 didn't, cp1 still pending
    let results = vec![None, Some(answered), Some(offline)];
    cluster.assemble_nodes(&before, &nodes, &results, &[], false);
    assert_eq!(cluster.versions[1].node, "w1");
    assert!(cluster.services.iter().any(|s| s.node == "w1"));
    assert!(cluster.offline.contains_key("w2"));
    assert!(!cluster.offline.contains_key("w1"));
    // A light refresh keeps cp1's load
    assert_eq!(cluster.load_avg.len(), 1);
}

/// A view picked before the nodes are known shows the loading hint
/// instead of doing nothing; once nodes are listed it opens for real
#[test]
fn views_picked_while_nodes_load_say_so() {
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        loading: true,
        ..Default::default()
    });
    assert!(matches!(
        comp.action_for(NavMenuItem::Network),
        Some(Action::ShowHint(NavMenuItem::Network, ref m)) if m == LOADING_NODES_HINT
    ));

    let mut comp = two_node_cluster();
    comp.selected_item = NodeListItem::ClusterHeader(0);
    assert!(matches!(
        comp.action_for(NavMenuItem::Network),
        Some(Action::ShowGroup(GroupKind::Network, ..))
    ));
}

/// The service × node matrix gives way to one row per service when the
/// nodes don't fit side by side, with the unhealthy nodes by name
#[test]
fn services_matrix_becomes_a_list_when_the_nodes_do_not_fit() {
    let mut comp = two_node_cluster();
    comp.clusters[0].connected = true;
    comp.clusters[0].services = vec![
        services_with("cp1", &["apid", "etcd"]),
        NodeServices {
            node: "worker1".to_string(),
            services: vec![ServiceInfo {
                id: "apid".to_string(),
                state: "Failed".to_string(),
                health: None,
            }],
        },
    ];
    comp.selected_item = NodeListItem::ClusterHeader(0);
    let nodes = ["cp1".to_string(), "worker1".to_string()];
    let render = |comp: &ClusterComponent, width: u16| {
        let area = Rect::new(0, 0, width, 4);
        let mut buf = Buffer::empty(area);
        comp.draw_services_matrix(&mut buf, area, 0, &nodes);
        (0..4)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    // Wide enough: the matrix, the failed service first
    let wide = render(&comp, 60);
    assert!(
        wide[0].contains("cp1") && wide[0].contains("worker1"),
        "{wide:?}"
    );
    assert!(
        wide[1].starts_with("apid") && wide[1].contains('✗'),
        "{wide:?}"
    );
    // Too narrow for a column per node: healthy count and the failed node
    let narrow = render(&comp, 30);
    assert!(narrow[0].contains("Healthy"), "{narrow:?}");
    assert!(
        narrow[1].contains("1/2") && narrow[1].contains("✗ worker1"),
        "{narrow:?}"
    );
    assert!(
        narrow[2].contains("etcd") && narrow[2].contains("1/1"),
        "{narrow:?}"
    );
}

/// On the dashboard, clicking another node keeps the services list
/// focused; empty space in the pane focuses the tree
#[test]
fn dashboard_clicks_keep_the_services_focus() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    let mut comp = two_node_cluster();
    if let Some(c) = comp.clusters.get_mut(0) {
        c.expanded = true;
    }
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
    terminal.draw(|f| comp.draw(f, f.area()).unwrap()).unwrap();
    let list = comp.hits.borrow().node_list;
    let click = |column, row| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };

    comp.set_focused_pane(FocusedPane::Services);
    comp.handle_mouse_event(click(list.x + 2, list.y)).unwrap();
    assert_eq!(comp.focused_pane(), FocusedPane::Services);

    comp.handle_mouse_event(click(list.x + 2, list.y + list.height - 1))
        .unwrap();
    assert_eq!(comp.focused_pane(), FocusedPane::Nodes);
}

/// Full when the view keeps 100 columns, compact otherwise; hidden is 0
#[test]
fn nodes_pane_widths() {
    let comp = two_node_cluster();
    let (mode, full) = comp.resolve_pane_width(220, None);
    assert_eq!(mode, PaneWidth::Full);
    assert!(full >= NODES_PANE_MIN_WIDTH);
    let (mode, compact) = comp.resolve_pane_width(120, None);
    assert_eq!(mode, PaneWidth::Compact);
    assert!((COMPACT_PANE_MIN_WIDTH..=COMPACT_PANE_MAX_WIDTH).contains(&compact));
    assert_eq!(
        comp.resolve_pane_width(220, Some(PaneWidth::Hidden)),
        (PaneWidth::Hidden, 0)
    );
}

/// Every width draws without overlap: the view starts where the pane ends
#[test]
fn workspace_frame_at_several_widths() {
    use ratatui::{Terminal, backend::TestBackend};
    for columns in [100, 160, 220] {
        for width in [
            None,
            Some(PaneWidth::Full),
            Some(PaneWidth::Compact),
            Some(PaneWidth::Hidden),
        ] {
            let mut comp = two_node_cluster();
            let mut terminal = Terminal::new(TestBackend::new(columns, 40)).unwrap();
            let mut areas = (Rect::default(), Rect::default());
            terminal
                .draw(|f| areas = comp.draw_frame(f, f.area(), width, false, false, None))
                .unwrap();
            let (tree, view) = areas;
            let expected = comp.resolve_pane_width(columns, width).1;
            assert_eq!(tree.width, expected, "{columns} cols, {width:?}");
            assert_eq!(view.x, tree.x + tree.width);
            assert_eq!(tree.width + view.width, columns);
        }
    }
}

/// When discovery is unavailable (empty members), fall back to the
/// etcd-service heuristic so control plane nodes are still identified.
#[test]
fn offline_nodes_keep_their_group_and_status() {
    let mut comp = ClusterComponent::default();
    let offline = |controlplane| OfflineNode {
        since: std::time::Instant::now(),
        controlplane,
    };
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        versions: vec![ver("cp1"), ver("cp2"), ver("cp3"), ver("w1"), ver("w2")],
        services: vec![services_with("cp1", &["etcd"])],
        // cp2 left discovery while rebooting but is still an etcd member
        etcd_members: vec![EtcdMemberInfo {
            id: 2,
            hostname: "cp2".to_string(),
            peer_urls: vec!["https://10.0.0.2:2380".to_string()],
            client_urls: Vec::new(),
            is_learner: false,
        }],
        offline: HashMap::from([
            ("cp2".to_string(), offline(false)),
            ("cp3".to_string(), offline(true)),
            ("w2".to_string(), offline(false)),
        ]),
        ..Default::default()
    });

    assert_eq!(
        names(&comp.controlplane_nodes_for(0)),
        vec!["cp1", "cp2", "cp3"]
    );
    assert_eq!(names(&comp.worker_nodes_for(0)), vec!["w1", "w2"]);

    let (symbol, color, note) = comp.node_row_status(0, "cp3");
    assert_eq!((symbol, color), ("○", Color::Red));
    assert!(note.contains("offline"), "{note}");
    assert_eq!(comp.node_row_status(0, "w1").0, "●");
}

#[test]
fn falls_back_to_etcd_service_without_discovery() {
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        versions: vec![ver("n1"), ver("n2")],
        services: vec![
            services_with("n1", &["etcd", "kubelet"]),
            services_with("n2", &["kubelet"]),
        ],
        ..Default::default()
    });

    assert_eq!(names(&comp.controlplane_nodes_for(0)), vec!["n1"]);
    assert_eq!(names(&comp.worker_nodes_for(0)), vec!["n2"]);
}

/// Control-plane and worker groups must partition the node set: every node
/// lands in exactly one group, never both or neither.
#[test]
fn groups_are_complementary() {
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        versions: vec![ver("a"), ver("b"), ver("c")],
        discovery_members: vec![
            member("a", "10.0.0.1", "controlplane"),
            member("b", "10.0.0.2", "worker"),
            // "c" is intentionally absent from discovery and has no services
        ],
        ..Default::default()
    });

    let cp = names(&comp.controlplane_nodes_for(0));
    let workers = names(&comp.worker_nodes_for(0));
    assert_eq!(cp.len() + workers.len(), 3, "every node classified once");
    // Unknown node "c" defaults to worker (safe default, still visible).
    assert!(workers.contains(&"c".to_string()));
    assert!(cp.contains(&"a".to_string()));
    assert!(workers.contains(&"b".to_string()));
}

/// End-to-end for the discovery-disabled path: nodes enumerated via the
/// Kubernetes API classify into the right groups, so workers appear even
/// when Talos discovery is off. Regression guard for the "workers missing
/// when discovery disabled" bug.
#[test]
fn k8s_derived_members_populate_worker_group() {
    use crate::components::diagnostics::k8s::{K8sNodeInfo, k8s_nodes_to_discovery_members};
    let members = k8s_nodes_to_discovery_members(vec![
        K8sNodeInfo {
            name: "cp1".into(),
            internal_ip: Some("10.0.0.1".into()),
            is_control_plane: true,
        },
        K8sNodeInfo {
            name: "w1".into(),
            internal_ip: Some("10.0.0.2".into()),
            is_control_plane: false,
        },
        K8sNodeInfo {
            name: "w2".into(),
            internal_ip: Some("10.0.0.3".into()),
            is_control_plane: false,
        },
    ]);
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        versions: vec![ver("cp1"), ver("w1"), ver("w2")],
        discovery_members: members,
        ..Default::default()
    });
    assert_eq!(names(&comp.controlplane_nodes_for(0)), vec!["cp1"]);
    assert_eq!(names(&comp.worker_nodes_for(0)), vec!["w1", "w2"]);
}

/// `control_plane_ip` picks a real control plane address to fetch a
/// cluster-correct kubeconfig from. It prefers an etcd member (etcd only
/// runs on control planes), then a roster `controlplane` member, then the
/// endpoint. Guards the fix that pins the K8s client to the launched
/// `--context` instead of an ambient KUBECONFIG.
#[test]
fn clock_offsets_are_formatted_and_colored_by_size() {
    let clock = |offset_secs| NodeClock {
        offset_secs,
        server: "ntp1.hetzner.de".to_string(),
    };
    let span = clock_span(&clock(0.012));
    assert_eq!(span.content, "+12ms");
    assert_eq!(span.style.fg, Some(Color::Green));
    let span = clock_span(&clock(-0.25));
    assert_eq!(span.content, "-250ms");
    assert_eq!(span.style.fg, Some(Color::Yellow));
    let span = clock_span(&clock(3.0));
    assert_eq!(span.content, "+3.0s");
    assert_eq!(span.style.fg, Some(Color::Red));
}

#[test]
fn tailnet_ip_prefers_ipv4_without_prefix() {
    let ts = talos_rs::TailscaleLink {
        up: true,
        addresses: vec![
            "fd7a:115c:a1e0::e/128".to_string(),
            "100.64.0.14/32".to_string(),
        ],
        ..Default::default()
    };
    assert_eq!(tailnet_ip(&ts), Some("100.64.0.14"));
    assert_eq!(tailnet_ip(&talos_rs::TailscaleLink::default()), None);
}

#[test]
fn control_plane_ip_prefers_etcd_then_roster_then_endpoint() {
    let etcd_member = EtcdMemberInfo {
        id: 1,
        hostname: "cp1".into(),
        peer_urls: vec!["https://10.0.0.1:2380".into()],
        client_urls: vec!["https://10.0.0.1:2379".into()],
        is_learner: false,
    };
    let cp_roster = DiscoveryMember {
        id: "cp2".into(),
        addresses: vec!["10.0.0.9".into()],
        hostname: "cp2".into(),
        machine_type: "controlplane".into(),
        operating_system: String::new(),
    };
    let worker_roster = DiscoveryMember {
        id: "w1".into(),
        addresses: vec!["10.0.0.2".into()],
        hostname: "w1".into(),
        machine_type: "worker".into(),
        operating_system: String::new(),
    };

    // etcd member wins over roster and endpoint.
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        etcd_members: vec![etcd_member],
        discovery_members: vec![cp_roster.clone(), worker_roster.clone()],
        endpoints: vec!["10.0.0.5:50000".to_string()],
        ..Default::default()
    });
    assert_eq!(comp.control_plane_ip().as_deref(), Some("10.0.0.1"));

    // No etcd: a roster controlplane member wins over a worker and endpoint.
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        discovery_members: vec![worker_roster, cp_roster],
        endpoints: vec!["10.0.0.5:50000".to_string()],
        ..Default::default()
    });
    assert_eq!(comp.control_plane_ip().as_deref(), Some("10.0.0.9"));

    // Nothing but an endpoint: fall back to it, port stripped.
    let mut comp = ClusterComponent::default();
    comp.clusters.push(ClusterData {
        name: "test".to_string(),
        endpoints: vec!["10.0.0.5:50000".to_string()],
        ..Default::default()
    });
    assert_eq!(comp.control_plane_ip().as_deref(), Some("10.0.0.5"));
}

#[test]
fn wrap_list_breaks_between_items() {
    let text = |lines: Vec<Line>| -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    };
    let items: Vec<String> = ["100.64.0.8/32", "65.21.17.62/26", "fd7a:115c:a1e0::8/128"]
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    assert_eq!(
        text(wrap_list(" IP: ", &items, 200, Style::default())),
        vec![" IP: 100.64.0.8/32, 65.21.17.62/26, fd7a:115c:a1e0::8/128"]
    );
    assert_eq!(
        text(wrap_list(" IP: ", &items, 40, Style::default())),
        vec![
            " IP: 100.64.0.8/32, 65.21.17.62/26,",
            "     fd7a:115c:a1e0::8/128"
        ]
    );
    assert!(wrap_list(" IP: ", &[], 40, Style::default()).is_empty());
}

#[test]
fn finished_one_shot_service_is_ok() {
    let svc = |state: &str, health: Option<(bool, bool)>| ServiceInfo {
        id: "svc".to_string(),
        state: state.to_string(),
        health: health.map(|(unknown, healthy)| talos_rs::ServiceHealth {
            unknown,
            healthy,
            last_message: String::new(),
        }),
    };
    assert!(service_ok(&svc("Finished", None)));
    assert!(service_ok(&svc("Running", Some((true, false)))));
    assert!(service_ok(&svc("Running", Some((false, true)))));
    assert!(!service_ok(&svc("Running", Some((false, false)))));
    assert!(!service_ok(&svc("Failed", None)));
    assert!(service_ok(&svc("Skipped", None)));
}

#[test]
fn node_dot_agrees_with_the_services_matrix() {
    let svc = |state: &str| ServiceInfo {
        id: "kubelet".to_string(),
        state: state.to_string(),
        health: None,
    };
    // A stopped service with no health check is a problem in the
    // matrix, so the dot isn't green either
    assert!(!node_row_ok(Some(40.0), Some(&[svc("Failed")])));
    assert!(node_row_ok(Some(40.0), Some(&[svc("Running")])));
    // Memory: critical only past the shared threshold
    assert!(node_row_ok(Some(89.0), Some(&[svc("Running")])));
    assert!(!node_row_ok(Some(95.0), Some(&[svc("Running")])));
    // Memory not answered yet doesn't make it unhealthy
    assert!(node_row_ok(None, Some(&[svc("Running")])));
}

#[test]
fn kubespan_links_count_peers_and_name_the_ones_not_up() {
    let peer = |label: &str, state: &str| talos_rs::KubeSpanPeerStatus {
        id: label.to_string(),
        label: label.to_string(),
        state: state.to_string(),
        ..Default::default()
    };
    let links = kubespan_links(&[
        peer("kpd2", "down"),
        peer("kpc1", "up"),
        peer("kpc2", "unknown"),
        peer("kpc3", "up"),
    ]);
    assert_eq!((links.up, links.total), (2, 4));
    assert_eq!(
        links.down,
        vec![
            ("kpc2".to_string(), "unknown".to_string()),
            ("kpd2".to_string(), "down".to_string())
        ]
    );

    // Name column (at least the title) plus one column per node, two apart
    let nodes = ["kpc1".to_string(), "worker-long-name".to_string()];
    assert_eq!(kubespan_matrix_width(&nodes), 16 + (4 + 2) + (16 + 2));
}

fn line_texts(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// The network section names the KubeSpan peers that aren't up and says
/// when the tailnet link is down
#[test]
fn network_lines_name_what_is_down() {
    let peer = |label: &str, state: &str| talos_rs::KubeSpanPeerStatus {
        id: label.to_string(),
        label: label.to_string(),
        state: state.to_string(),
        ..Default::default()
    };
    let net = NodeNetworkInfo {
        addresses: vec!["10.0.0.1/24".to_string()],
        connectivity_ready: Some(false),
        tailscale: Some(talos_rs::TailscaleLink::default()),
        kubespan_peers: Some(vec![peer("cp2", "up"), peer("w1", "down")]),
        ..Default::default()
    };
    let text = line_texts(&details::network_lines(&net, 80));
    assert_eq!(
        text,
        [
            " Addrs:  10.0.0.1/24",
            " Conn:   ✗ Not ready",
            " TS:     down",
            " KSpan:  1/2 peers up  w1 down",
        ]
    );
}

/// A failed connection lists the endpoints tried and the cause; the retry
/// hint follows auto-refresh
#[test]
fn connection_error_lines_show_endpoints_cause_and_retry() {
    let cluster = ClusterData {
        name: "test".to_string(),
        endpoints: vec!["10.0.0.1".to_string(), "10.0.0.2".to_string()],
        ..Default::default()
    };
    let text = line_texts(&details::connection_error_lines(
        &cluster,
        "Connection refused",
        false,
    ))
    .join("\n");
    assert!(text.contains("Cluster:  test"), "{text}");
    assert!(text.contains("- 10.0.0.1\n    - 10.0.0.2"), "{text}");
    assert!(text.contains("Connection refused"), "{text}");
    assert!(text.ends_with("Press r to retry."), "{text}");

    let text = line_texts(&details::connection_error_lines(&cluster, "x", true)).join("\n");
    assert!(text.contains("Retrying every"), "{text}");
}

/// The services list marks the selected service only while it has focus
#[test]
fn services_lines_mark_the_selection_when_focused() {
    let mut comp = two_node_cluster();
    let services = services_with("cp1", &["apid", "etcd"]).services;
    comp.selected_service = 1;
    let text = line_texts(&comp.services_lines(&services));
    assert_eq!(text[0], " Services (2/2)");
    assert!(!text.iter().any(|l| l.contains('▸')), "{text:?}");

    comp.set_focused_pane(FocusedPane::Services);
    let text = line_texts(&comp.services_lines(&services));
    assert!(
        text[2].starts_with(" ▸") && text[2].contains("etcd"),
        "{text:?}"
    );
}

#[test]
fn group_columns_follow_the_features_present() {
    use super::group_table::GroupCol;
    let titles = |cols: Vec<GroupCol>| cols.into_iter().map(GroupCol::title).collect::<Vec<_>>();
    assert_eq!(
        titles(GroupCol::shown(false, false, false, false)),
        [
            "Node", "Role", "Talos", "K8s", "Mem", "Disk", "Load", "Uptime", "Conn", "Svc",
            "Clock", "GW"
        ]
    );
    let all = titles(GroupCol::shown(true, true, true, true));
    assert_eq!(all.len(), 16);
    // Optional columns sit in a fixed order
    let pos = |t: &str| all.iter().position(|c| *c == t).unwrap();
    assert!(pos("ZFS") < pos("Load") && pos("Cores") < pos("Uptime"));
    assert!(pos("Svc") < pos("KSpan") && pos("Tailscale") < pos("Clock"));
}

fn press(comp: &mut ClusterComponent, code: KeyCode) {
    comp.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
        .unwrap();
}

/// Menu hotkeys are single keys, distinct, and don't shadow the tree's own
/// keys or the app's layout keys
#[test]
fn menu_hotkeys_are_unique_and_free() {
    let reserved = ["q", "r", "j", "k", "l", "o", "O", "a", " ", "Z", "[", "]"];
    let mut seen = std::collections::HashSet::new();
    for item in NavMenuItem::ALL {
        let key = item.hotkey();
        assert_eq!(key.chars().count(), 1, "{item:?}: {key}");
        assert!(!reserved.contains(&key), "{item:?} takes reserved {key}");
        assert!(seen.insert(key), "{key} is used twice");
    }
}

/// The menu cursor stops at its first and last entries (index 0 is the
/// tree, entries are 1-based)
#[test]
fn menu_cursor_stays_in_bounds() {
    let mut comp = two_node_cluster();
    comp.focused_pane = FocusedPane::Menu;
    comp.selected_menu_item = 1;
    press(&mut comp, KeyCode::Up);
    assert_eq!(comp.selected_menu_item, 1);
    for _ in 0..NavMenuItem::ALL.len() + 5 {
        press(&mut comp, KeyCode::Char('j'));
    }
    assert_eq!(comp.selected_menu_item, NavMenuItem::ALL.len());
}

/// Tab and Shift+Tab cycle the windows both ways; entering the menu puts
/// the cursor on its first entry
#[test]
fn tab_cycles_the_windows() {
    let mut comp = two_node_cluster();
    comp.focused_pane = FocusedPane::Nodes;
    comp.selected_menu_item = 0;
    let mut order = Vec::new();
    for _ in 0..3 {
        press(&mut comp, KeyCode::Tab);
        order.push(comp.focused_pane);
        if comp.focused_pane == FocusedPane::Menu {
            assert_eq!(comp.selected_menu_item, 1);
        }
    }
    assert_eq!(
        order,
        [FocusedPane::Menu, FocusedPane::Services, FocusedPane::Nodes]
    );
    for expected in [FocusedPane::Services, FocusedPane::Menu, FocusedPane::Nodes] {
        press(&mut comp, KeyCode::BackTab);
        assert_eq!(comp.focused_pane, expected);
    }
}

/// j/k walk the rows shown: a folded group's nodes are skipped, and the
/// cursor stops at both ends
#[test]
fn tree_navigation_skips_folded_groups() {
    let mut comp = two_node_cluster();
    let cluster = &mut comp.clusters[0];
    cluster.expanded = true;
    cluster.controlplane_expanded = false;
    cluster.workers_expanded = true;
    comp.focused_pane = FocusedPane::Nodes;
    comp.selected_item = NodeListItem::ClusterHeader(0);
    let mut walked = vec![comp.selected_item.clone()];
    for _ in 0..4 {
        press(&mut comp, KeyCode::Char('j'));
        walked.push(comp.selected_item.clone());
    }
    assert_eq!(
        walked,
        [
            NodeListItem::ClusterHeader(0),
            NodeListItem::ControlPlaneHeader(0),
            NodeListItem::WorkersHeader(0),
            NodeListItem::WorkerNode(0, 0),
            NodeListItem::WorkerNode(0, 0),
        ]
    );
    for _ in 0..4 {
        press(&mut comp, KeyCode::Up);
    }
    assert_eq!(comp.selected_item, NodeListItem::ClusterHeader(0));
    // Unfolding shows the control plane's node between the two headers
    comp.selected_item = NodeListItem::ControlPlaneHeader(0);
    press(&mut comp, KeyCode::Char(' '));
    press(&mut comp, KeyCode::Down);
    assert_eq!(comp.selected_item, NodeListItem::ControlPlaneNode(0, 0));
}

/// Discovery, else etcd, else the last version answers; etcd members
/// discovery misses are added, and nameless members go by IP
#[test]
fn nodes_to_query_fall_back_through_the_sources() {
    let etcd = |hostname: &str, ip: &str| EtcdMemberInfo {
        id: 1,
        hostname: hostname.to_string(),
        peer_urls: vec![format!("https://{ip}:2380")],
        client_urls: Vec::new(),
        is_learner: false,
    };
    let discovery = [
        member("cp1", "10.0.0.1", "controlplane"),
        member("", "10.0.0.2", "worker"),
    ];
    let members = [
        etcd("cp1", "10.0.0.1"),
        etcd("cp2", "10.0.0.3"),
        etcd("", "10.0.0.4"),
    ];
    let versions = [ver("10.0.0.9")];
    let pairs = |v: Vec<(String, String)>| {
        v.into_iter()
            .map(|(n, i)| format!("{n}={i}"))
            .collect::<Vec<_>>()
    };

    // A rebooting cp2 dropped out of discovery but is still an etcd member;
    // a nameless etcd member isn't added
    assert_eq!(
        pairs(nodes_to_query(&discovery, &members, &versions)),
        ["cp1=10.0.0.1", "10.0.0.2=10.0.0.2", "cp2=10.0.0.3"]
    );
    assert_eq!(
        pairs(nodes_to_query(&[], &members, &versions)),
        ["cp1=10.0.0.1", "cp2=10.0.0.3", "10.0.0.4=10.0.0.4"]
    );
    assert_eq!(
        pairs(nodes_to_query(&[], &[], &versions)),
        ["10.0.0.9=10.0.0.9"]
    );
    assert!(nodes_to_query(&[], &[], &[]).is_empty());
}

/// `a` toggles auto-refresh, as in every view that has it
#[test]
fn a_toggles_auto_refresh() {
    use crate::components::Component;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut c = ClusterComponent::default();
    let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    assert!(c.auto_refresh);
    c.handle_key_event(a).unwrap();
    assert!(!c.auto_refresh);
    c.handle_key_event(a).unwrap();
    assert!(c.auto_refresh);
}
