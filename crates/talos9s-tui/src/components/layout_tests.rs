//! Every view drawn into the right pane at the narrow widths it gets (a
//! 100-120 column terminal with the nodes pane open): nothing panics, and
//! its key line fits beside the workspace keys

use super::*;
use ratatui::{Terminal, backend::TestBackend};

fn views() -> Vec<(&'static str, Box<dyn Component>)> {
    let node = || ("bc1".to_string(), "10.0.0.1".to_string());
    let group_nodes = || {
        vec![GroupNode {
            hostname: "bc1".into(),
            address: "10.0.0.1".into(),
            role: "controlplane".into(),
        }]
    };
    let group = |kind| {
        Box::new(GroupComponent::new(
            kind,
            "Control Plane".into(),
            group_nodes(),
            None,
            None,
            None,
        )) as Box<dyn Component>
    };
    let (h, a) = node();
    vec![
        (
            "network",
            Box::new(NetworkStatsComponent::new(h.clone(), a.clone())),
        ),
        (
            "storage",
            Box::new(StorageComponent::new(h.clone(), a.clone(), None, None)),
        ),
        (
            "diagnostics",
            Box::new(DiagnosticsComponent::new(
                h.clone(),
                a.clone(),
                "controlplane".into(),
                None,
            )),
        ),
        (
            "processes",
            Box::new(ProcessesComponent::new(
                "bc1".into(),
                vec![ProcessNode {
                    hostname: h.clone(),
                    address: a.clone(),
                }],
            )),
        ),
        (
            "node info",
            Box::new(NodeInfoComponent::new(
                h,
                &a,
                None,
                None,
                NodeInfoTab::Config,
            )),
        ),
        (
            "logs",
            Box::new(MultiLogsComponent::new(
                "bc1".into(),
                String::new(),
                // Two nodes: the longest key line (the nodes pane key too)
                ["bc1", "bc2"]
                    .map(|n| LogNode {
                        name: n.into(),
                        ip: a.clone(),
                        services: Some(vec!["apid".into()]),
                    })
                    .to_vec(),
                &["apid".into()],
                vec!["apid".into()],
            )),
        ),
        ("etcd", Box::new(EtcdComponent::new())),
        (
            "lifecycle",
            Box::new(LifecycleComponent::new("bc".into(), None)),
        ),
        (
            "security",
            Box::new(SecurityComponent::new("bc".into(), None)),
        ),
        ("volumes", Box::new(VolumesComponent::new(None))),
        ("workloads", Box::new(WorkloadHealthComponent::new(None))),
        ("group diagnostics", group(GroupKind::Diagnostics)),
        ("group storage", group(GroupKind::Storage)),
        ("group partitions", group(GroupKind::Partitions)),
        ("group network", group(GroupKind::Network)),
    ]
}

#[test]
fn views_draw_at_narrow_widths() {
    // The right pane of a 100 and a 120 column screen with the full or the
    // compact nodes pane open
    for width in [70u16, 78, 98, 120] {
        for (name, mut view) in views() {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal
                .draw(|f| {
                    view.draw(f, f.area())
                        .unwrap_or_else(|e| panic!("{name} at {width}: {e}"));
                })
                .unwrap();
        }
    }
}

#[test]
fn key_lines_fit_a_100_column_screen() {
    // The bottom line spans the screen; "<Esc/q/←> tree" sits on its right
    let room = 100 - ratatui::text::Line::from("<Esc/q/←> tree").width() - 1;
    let mut too_long = Vec::new();
    for (name, view) in views() {
        if let Some(hints) = view.key_hints() {
            let width = hints.width();
            if width > room {
                too_long.push(format!("{name}: {width}"));
            }
        }
    }
    assert!(
        too_long.is_empty(),
        "key lines wider than {room}: {too_long:?}"
    );
}
