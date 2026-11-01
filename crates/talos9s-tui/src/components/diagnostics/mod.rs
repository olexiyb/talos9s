//! Diagnostics component for Talos node health checks
//!
//! Provides system health checks, Kubernetes component status,
//! and actionable fixes for common issues.
//!
//! Architecture:
//! - `core.rs` - Core checks that run on any Talos cluster
//! - `cni/` - CNI-specific checks (Flannel, Cilium, Calico)
//! - `addons/` - Addon-specific checks (cert-manager, etc.)
//! - `types.rs` - Shared types

pub mod addons;
pub mod cni;
pub mod core;
pub mod k8s;
pub mod pki;
pub mod types;

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState},
};
use std::time::{Duration, Instant};
use talos_rs::{ApplyConfigResult, ApplyMode, TalosClient};
use talos9s_core::AsyncState;

use crate::ui_ext::CheckStatusExt;
pub use types::*;

/// Default auto-refresh interval in seconds
const AUTO_REFRESH_INTERVAL_SECS: u64 = 10;

/// Data loaded asynchronously for the diagnostics component
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsData {
    /// Node hostname
    pub hostname: String,
    /// Node IP address
    pub address: String,

    /// Diagnostic context (platform, CNI type, etc.)
    pub context: DiagnosticContext,

    /// System health checks
    pub system_checks: Vec<DiagnosticCheck>,
    /// Kubernetes component checks
    pub kubernetes_checks: Vec<DiagnosticCheck>,
    /// Service health checks
    pub service_checks: Vec<DiagnosticCheck>,
    /// CNI-specific checks
    pub cni_checks: Vec<DiagnosticCheck>,
    /// Addon-specific checks
    pub addon_checks: Vec<DiagnosticCheck>,
    /// Detected addons
    pub detected_addons: addons::DetectedAddons,
}

/// Diagnostics component for node health checks
#[derive(Clone)]
pub struct DiagnosticsComponent {
    /// Async state for loaded data
    state: AsyncState<DiagnosticsData>,

    /// Currently selected category
    selected_category: usize,
    /// Currently selected check within category
    selected_check: usize,
    /// Table state for rendering
    table_state: TableState,
    /// Where each category's checks were drawn, for clicks
    category_rows: [crate::ui_ext::RowHits; 5],

    /// Pending action (waiting for confirmation)
    pending_action: Option<PendingAction>,
    /// Whether we're in the confirmation dialog
    show_confirmation: bool,
    /// Confirmation dialog selection (0 = Cancel, 1 = Apply; for host commands: 0 = Copy, 1 = Close)
    confirmation_selection: usize,
    /// Time when command was copied (for showing feedback)
    copy_feedback_until: Option<Instant>,

    /// Whether we're showing a details popup (for checks without fixes)
    show_details: bool,
    /// Title of the details popup
    details_title: String,
    /// Content of the details popup
    details_content: String,

    /// Whether we're applying a fix
    applying_fix: bool,
    /// Result of the last apply
    apply_result: Option<Result<Vec<ApplyConfigResult>, String>>,

    /// Auto-refresh enabled
    auto_refresh: bool,

    /// Client for API calls
    client: Option<TalosClient>,
    /// Control plane endpoint for fetching kubeconfig (used for worker nodes)
    controlplane_endpoint: Option<String>,
    /// Custom config file path (from --config flag)
    config_path: Option<String>,
    /// Talosconfig context of the active cluster (for talosctl commands)
    talos_context: Option<String>,
}

impl Default for DiagnosticsComponent {
    fn default() -> Self {
        Self::new("".to_string(), "".to_string(), "".to_string(), None)
    }
}

impl DiagnosticsComponent {
    pub fn new(
        hostname: String,
        address: String,
        node_role: String,
        config_path: Option<String>,
    ) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        let mut context = DiagnosticContext::new();
        context.node_role = node_role.clone();
        context.hostname = hostname.clone();
        context.node_endpoint = Some(address.clone());
        context.config_path = config_path.clone();

        let initial_data = DiagnosticsData {
            hostname,
            address,
            context,
            ..Default::default()
        };

        Self {
            // Loading until the first (background) refresh lands
            state: {
                let mut state = AsyncState::with_data(initial_data);
                state.start_loading();
                state
            },
            selected_category: 0,
            selected_check: 0,
            table_state,
            category_rows: Default::default(),
            pending_action: None,
            show_confirmation: false,
            confirmation_selection: 1,
            copy_feedback_until: None,
            show_details: false,
            details_title: String::new(),
            details_content: String::new(),
            applying_fix: false,
            apply_result: None,
            auto_refresh: true,
            client: None,
            controlplane_endpoint: None,
            config_path,
            talos_context: None,
        }
    }

    /// Access loaded data immutably
    fn data(&self) -> Option<&DiagnosticsData> {
        self.state.data()
    }

    /// Access loaded data mutably
    fn data_mut(&mut self) -> Option<&mut DiagnosticsData> {
        self.state.data_mut()
    }

    /// Set the client for making API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    /// Set the control plane endpoint for fetching kubeconfig (used for worker nodes)
    pub fn set_controlplane_endpoint(&mut self, endpoint: Option<String>) {
        self.controlplane_endpoint = endpoint;
    }

    /// Set the talosconfig context used for talosctl commands
    pub fn set_talos_context(&mut self, context: Option<String>) {
        if let Some(data) = self.data_mut() {
            data.context.talos_context = context.clone();
        }
        self.talos_context = context;
    }

    /// Checks by category after a refresh, for the group view; the error
    /// when the run failed
    pub fn checks_by_category(&self) -> Result<Vec<(&'static str, Vec<DiagnosticCheck>)>, String> {
        if let Some(error) = self.state.error() {
            return Err(error.to_string());
        }
        let data = self.data().ok_or_else(|| "no data".to_string())?;
        let checks = [
            &data.system_checks,
            &data.kubernetes_checks,
            &data.cni_checks,
            &data.service_checks,
            &data.addon_checks,
        ];
        Ok(checks
            .into_iter()
            .enumerate()
            .map(|(idx, checks)| (self.category_title(idx), checks.clone()))
            .collect())
    }

    /// The node and a summary of its last completed run, for the dashboard
    pub fn summary(&self) -> Option<(String, DiagnosticsSummary)> {
        let categories = self.checks_by_category().ok()?;
        let hostname = self.data()?.hostname.clone();
        Some((hostname, DiagnosticsSummary::of(&categories)))
    }

    /// Set an error message
    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    /// Get all checks in the current category
    fn current_checks(&self) -> &[DiagnosticCheck] {
        let Some(data) = self.data() else {
            return &[];
        };
        match self.selected_category {
            0 => &data.system_checks,
            1 => &data.kubernetes_checks,
            2 => &data.cni_checks,
            3 => &data.service_checks,
            4 => &data.addon_checks,
            _ => &[],
        }
    }

    /// Get the currently selected check
    fn selected_check(&self) -> Option<&DiagnosticCheck> {
        self.current_checks().get(self.selected_check)
    }

    /// Get total number of categories
    fn category_count(&self) -> usize {
        5 // System, Kubernetes, CNI, Services, Addons
    }

    /// Select next category
    fn next_category(&mut self) {
        self.selected_category = (self.selected_category + 1) % self.category_count();
        self.selected_check = 0;
        self.update_table_state();
    }

    /// Select previous category
    fn prev_category(&mut self) {
        self.selected_category = if self.selected_category == 0 {
            self.category_count() - 1
        } else {
            self.selected_category - 1
        };
        self.selected_check = 0;
        self.update_table_state();
    }

    /// Select next check in current category (clamps at end, no wrapping)
    fn next_check(&mut self) {
        let count = self.current_checks().len();
        if count > 0 && self.selected_check < count - 1 {
            self.selected_check += 1;
            self.update_table_state();
        }
    }

    /// Select previous check in current category (clamps at start, no wrapping)
    fn prev_check(&mut self) {
        if self.selected_check > 0 {
            self.selected_check -= 1;
            self.update_table_state();
        }
    }

    /// Update table state to match selection
    fn update_table_state(&mut self) {
        self.table_state.select(Some(self.selected_check));
    }

    /// Ensure selected_check is within bounds for current category
    fn ensure_valid_selection(&mut self) {
        let count = self.current_checks().len();
        if count == 0 {
            self.selected_check = 0;
        } else if self.selected_check >= count {
            self.selected_check = count - 1;
        }
        self.update_table_state();
    }

    /// Initiate a fix action or show details for the currently selected check
    fn initiate_fix(&mut self) {
        // Extract info from check first to avoid borrow issues
        let check_info = self.selected_check().map(|check| {
            (
                check.id.clone(),
                check.name.clone(),
                check.fix.clone(),
                check.details.clone(),
            )
        });

        if let Some((check_id, check_name, fix_opt, details_opt)) = check_info {
            if let Some(fix) = fix_opt {
                // Has a fix - show confirmation dialog
                let preview = match &fix.action {
                    FixAction::AddKernelModule(name) => Some(format!(
                        "machine:\n  kernel:\n    modules:\n      - name: {}",
                        name
                    )),
                    FixAction::ApplyConfigPatch { yaml, .. } => Some(yaml.clone()),
                    _ => None,
                };

                let is_host_cmd = fix.action.is_host_command();

                self.pending_action = Some(PendingAction {
                    check_id,
                    fix,
                    preview,
                });
                self.show_confirmation = true;
                self.confirmation_selection = if is_host_cmd { 0 } else { 1 };
                self.copy_feedback_until = None;
            } else if let Some(details) = details_opt {
                // No fix but has details - show details popup
                self.details_title = check_name;
                self.details_content = details;
                self.show_details = true;
            }
        }
    }

    /// Show the pending fix as being applied, while a clone applies it in
    /// the background; `adopt_refresh` takes over its result
    pub fn begin_apply_fix(&mut self) {
        self.applying_fix = true;
        self.show_confirmation = false;
        self.pending_action = None;
    }

    /// Apply the pending fix action
    pub async fn apply_pending_fix(&mut self) -> Result<()> {
        tracing::info!("apply_pending_fix called");

        let Some(pending) = self.pending_action.take() else {
            tracing::info!("No pending action to apply");
            return Ok(());
        };

        let Some(client) = &self.client else {
            tracing::error!("No client configured");
            self.set_error("No client configured".to_string());
            return Ok(());
        };

        // Get address for talosctl commands
        let address = self.data().map(|d| d.address.clone()).unwrap_or_default();

        self.applying_fix = true;
        self.show_confirmation = false;

        match &pending.fix.action {
            FixAction::AddKernelModule(name) => {
                tracing::info!("Applying kernel module fix: {}", name);
                let patch_yaml =
                    format!("machine:\n  kernel:\n    modules:\n      - name: {}", name);

                // Without a context talosctl would fall back to its default
                // context, which may be a different cluster than the one shown.
                let Some(context) = self.talos_context.as_deref() else {
                    tracing::error!("No talosconfig context for patch");
                    self.apply_result = Some(Err("No talosconfig context available".to_string()));
                    self.applying_fix = false;
                    return Ok(());
                };

                match talos_rs::patch_machineconfig_for_node(
                    context,
                    &address,
                    self.config_path.as_deref(),
                    &patch_yaml,
                )
                .await
                {
                    Ok(stdout) => {
                        tracing::info!("Patch succeeded: {}", stdout);
                        self.apply_result = Some(Ok(vec![]));
                    }
                    Err(e) => {
                        tracing::error!("Patch failed: {}", e);
                        self.apply_result = Some(Err(e.to_string()));
                    }
                }
            }
            FixAction::ApplyConfigPatch {
                yaml,
                requires_reboot,
            } => {
                let mode = if *requires_reboot {
                    ApplyMode::Reboot
                } else {
                    ApplyMode::Auto
                };
                match client.apply_configuration(yaml, mode, false).await {
                    Ok(results) => {
                        self.apply_result = Some(Ok(results));
                    }
                    Err(e) => {
                        self.apply_result = Some(Err(e.to_string()));
                    }
                }
            }
            FixAction::RestartService(service) => match client.service_restart(service).await {
                Ok(_) => {
                    self.apply_result = Some(Ok(vec![]));
                }
                Err(e) => {
                    self.apply_result = Some(Err(e.to_string()));
                }
            },
            FixAction::ShowDetails(_)
            | FixAction::InstallCilium
            | FixAction::HostCommand { .. } => {
                // These don't apply directly
            }
        }

        self.applying_fix = false;
        Ok(())
    }

    /// Refresh diagnostics data from the node
    pub async fn refresh(&mut self) -> Result<()> {
        let client = match &self.client {
            Some(c) => c.clone(),
            None => {
                self.set_error("No client configured".to_string());
                return Ok(());
            }
        };

        self.state.start_loading();

        let timeout = std::time::Duration::from_secs(15);
        // Where a refresh spends its time, for the debug log
        let addr = self.data().map(|d| d.address.clone()).unwrap_or_default();
        let started = std::time::Instant::now();
        let mut lap = started;

        // Fetch platform info first
        if let Ok(versions) = client.version().await
            && let Some(v) = versions.first()
            && let Some(data) = self.data_mut()
        {
            data.context.platform = v.platform.clone();
            data.context.is_container = v.platform == "container";
            tracing::info!("Detected platform: {}", data.context.platform);
        }

        log_stage(&addr, "version", &mut lap);

        // Get CPU count for load threshold scaling
        if let Ok(cpu_info) = client.cpu_info().await
            && let Some(info) = cpu_info.first()
            && let Some(data) = self.data_mut()
        {
            data.context.cpu_count = Some(info.cpu_count).filter(|c| *c > 0);
            tracing::info!("Detected {:?} CPUs", data.context.cpu_count);
        }

        log_stage(&addr, "cpu info", &mut lap);

        // One Kubernetes client for all checks, pinned to this node's cluster:
        // its kubeconfig comes from a control plane (the node itself, or the
        // one given for a worker), and a KUBECONFIG context is used only if
        // it names the same cluster. Unpinned, the ambient current context
        // would answer, possibly for another cluster.
        let pin = self.controlplane_endpoint.clone().or_else(|| {
            self.data()
                .filter(|d| d.context.node_role == "controlplane")
                .and_then(|d| d.context.node_endpoint.clone())
        });
        if let Some(cp) = &pin {
            tracing::info!("Kubernetes client pinned to control plane {}", cp);
        }
        let k8s_client = match k8s::create_k8s_client_with_source(&client, pin.as_deref(), None)
            .await
            .map(|(client, _source)| client)
        {
            Ok(client) => {
                tracing::info!("K8s client created successfully");
                if let Some(data) = self.data_mut() {
                    data.context.k8s_error = None;
                }
                Some(client)
            }
            Err(e) => {
                let error_msg = format!("{}", e);
                tracing::warn!(
                    "Failed to create K8s client: {} - K8s-based checks will be limited",
                    error_msg
                );
                if let Some(data) = self.data_mut() {
                    data.context.k8s_error = Some(error_msg);
                }
                None
            }
        };

        log_stage(&addr, "kubernetes client", &mut lap);

        // CNI, the node's pods and the cluster's addons don't depend on each
        // other: ask at once
        let hostname = self
            .data()
            .map(|d| d.context.hostname.clone())
            .unwrap_or_default();
        let ((cni_type, cni_info), pod_health, detected_addons) = tokio::join!(
            cni::detect_cni_with_client(&client, k8s_client.as_ref()),
            async {
                match &k8s_client {
                    Some(kc) => Some(k8s::check_pod_health(kc, Some(&hostname)).await),
                    None => None,
                }
            },
            async {
                match &k8s_client {
                    Some(kc) => Some(addons::detect_addons(kc).await),
                    None => None,
                }
            },
        );
        log_stage(&addr, "CNI, pods and addons", &mut lap);
        if let Some(data) = self.data_mut() {
            data.context.cni_type = cni_type;
            data.context.cni_info = cni_info.clone();
            tracing::info!("Detected CNI: {:?}", data.context.cni_type);
        }

        match pod_health {
            Some(Ok(health)) => {
                // Convert k8s::PodHealthInfo to types::PodHealthInfo
                let pod_health = PodHealthInfo {
                    crashing: health
                        .crashing
                        .iter()
                        .map(|p| UnhealthyPodInfo {
                            name: p.name.clone(),
                            namespace: p.namespace.clone(),
                            state: p.state.clone(),
                            restart_count: p.restart_count,
                        })
                        .collect(),
                    image_pull_errors: health
                        .image_pull_errors
                        .iter()
                        .map(|p| UnhealthyPodInfo {
                            name: p.name.clone(),
                            namespace: p.namespace.clone(),
                            state: p.state.clone(),
                            restart_count: p.restart_count,
                        })
                        .collect(),
                    total_pods: health.total_pods,
                };
                tracing::info!("Pod health check complete: {} pods", health.total_pods);
                if let Some(data) = self.data_mut() {
                    data.context.pod_health = Some(pod_health);
                }
            }
            Some(Err(e)) => tracing::warn!("Failed to check pod health via K8s API: {}", e),
            None => {}
        }
        if let Some(detected_addons) = detected_addons
            && let Some(data) = self.data_mut()
        {
            data.detected_addons = detected_addons;
        }

        // Get context and detected_addons for use in async block
        let context = self.data().map(|d| d.context.clone()).unwrap_or_default();
        let detected_addons = self
            .data()
            .map(|d| d.detected_addons.clone())
            .unwrap_or_default();

        let result = tokio::time::timeout(timeout, async {
            // Independent checks: run at once
            let mut lap = std::time::Instant::now();
            let (
                mut system_checks,
                kubernetes_checks,
                service_checks,
                cert_checks,
                cni_checks,
                addon_checks,
            ) = tokio::join!(
                core::run_system_checks(&client, &context),
                core::run_kubernetes_checks(&client, &context),
                core::run_service_checks(&client, &context),
                core::run_certificate_checks(&client, &context, self.config_path.as_deref()),
                cni::run_cni_checks(&client, &context, k8s_client.as_ref()),
                addons::run_addon_checks(k8s_client.as_ref(), &detected_addons, &context),
            );
            system_checks.extend(cert_checks);
            log_stage(&addr, "checks", &mut lap);

            (
                system_checks,
                kubernetes_checks,
                service_checks,
                cni_checks,
                addon_checks,
            )
        })
        .await;

        tracing::debug!("diagnostics {}: refresh took {:?}", addr, started.elapsed());
        match result {
            Ok((system, kubernetes, services, cni, addons_result)) => {
                if let Some(data) = self.data_mut() {
                    data.system_checks = system;
                    data.kubernetes_checks = kubernetes;
                    data.service_checks = services;
                    data.cni_checks = cni;
                    data.addon_checks = addons_result;
                }
                // Ensure selection is valid after checks change
                self.ensure_valid_selection();
                self.state.mark_loaded();
            }
            Err(_) => {
                self.set_error("Timeout fetching diagnostics".to_string());
            }
        }

        Ok(())
    }

    /// Get category title
    fn category_title(&self, idx: usize) -> &'static str {
        match idx {
            0 => "System Health",
            1 => "Kubernetes Components",
            2 => {
                let cni_type = self
                    .data()
                    .map(|d| d.context.cni_type.clone())
                    .unwrap_or(CniType::Unknown);
                match cni_type {
                    CniType::Flannel => "CNI (Flannel)",
                    CniType::Cilium => "CNI (Cilium)",
                    CniType::Calico => "CNI (Calico)",
                    _ => "CNI",
                }
            }
            3 => "Services",
            4 => "Addons",
            _ => "Unknown",
        }
    }

    /// Render a category section
    fn render_category(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        category_idx: usize,
        checks: &[DiagnosticCheck],
        is_selected: bool,
    ) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.category_title(category_idx))
            .border_style(if is_selected {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if checks.is_empty() {
            let loading = Paragraph::new("Loading...").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, inner);
            return;
        }

        let rows: Vec<Row> = checks
            .iter()
            .enumerate()
            .map(|(i, check)| {
                let (indicator, color) = check.status.indicator();
                let is_current = is_selected && i == self.selected_check;

                let style = if is_current {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                Row::new(vec![
                    ratatui::widgets::Cell::from(Span::styled(
                        indicator,
                        Style::default().fg(color),
                    )),
                    ratatui::widgets::Cell::from(check.name.clone()),
                    ratatui::widgets::Cell::from(Span::styled(
                        check.message.clone(),
                        Style::default().fg(if check.status == CheckStatus::Pass {
                            Color::Green
                        } else if check.status == CheckStatus::Fail {
                            Color::Red
                        } else if check.status == CheckStatus::Warn {
                            Color::Yellow
                        } else {
                            Color::DarkGray
                        }),
                    )),
                ])
                .style(style)
            })
            .collect();

        let widths = [
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Fill(1),
        ];

        self.category_rows[category_idx] =
            crate::ui_ext::RowHits::new(inner, false, 0, 0, checks.len());
        let table = Table::new(rows, widths);
        frame.render_widget(table, inner);
    }

    /// Render the confirmation dialog
    fn render_confirmation(&self, frame: &mut Frame, area: Rect) {
        let Some(pending) = &self.pending_action else {
            return;
        };

        let dialog_width = 70.min(area.width.saturating_sub(4));
        let dialog_height = 15.min(area.height.saturating_sub(4));
        let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
        let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

        let dialog_area = Rect::new(
            area.x + dialog_x,
            area.y + dialog_y,
            dialog_width,
            dialog_height,
        );

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Confirm Action ")
            .border_style(Style::default().fg(Color::Yellow));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let is_host_command = pending.fix.action.is_host_command();

        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("Fix: {}", pending.fix.description),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];

        if let FixAction::HostCommand { command, .. } = &pending.fix.action {
            lines.push(Line::from(Span::styled(
                "Run this command on your Docker host:",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
            // Display each line of the command
            for cmd_line in command.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", cmd_line),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(Line::from(""));
        } else if let Some(preview) = &pending.preview {
            lines.push(Line::from("This will apply the following configuration:"));
            lines.push(Line::from(""));
            for line in preview.lines().take(5) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", line),
                    Style::default().fg(Color::Cyan),
                )));
            }
            lines.push(Line::from(""));
        }

        if !is_host_command && pending.fix.action.requires_reboot() {
            lines.push(Line::from(Span::styled(
                "⚠ This requires a node reboot to take effect.",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
        }

        if is_host_command {
            let show_copied = self
                .copy_feedback_until
                .map(|t| t.elapsed() < std::time::Duration::from_secs(2))
                .unwrap_or(false);

            let copy_style = if show_copied {
                Style::default()
                    .bg(Color::Green)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if self.confirmation_selection == 0 {
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };

            let close_style = if self.confirmation_selection == 1 {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let copy_text = if show_copied { " Copied! " } else { " Copy " };

            lines.push(Line::from(vec![
                Span::raw("         "),
                Span::styled(copy_text, copy_style),
                Span::raw("     "),
                Span::styled(" Close ", close_style),
            ]));
        } else {
            let cancel_style = if self.confirmation_selection == 0 {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let apply_style = if self.confirmation_selection == 1 {
                Style::default()
                    .bg(Color::Green)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };

            lines.push(Line::from(vec![
                Span::raw("         "),
                Span::styled(" Cancel ", cancel_style),
                Span::raw("     "),
                Span::styled(
                    if pending.fix.action.requires_reboot() {
                        " Apply & Reboot "
                    } else {
                        " Apply "
                    },
                    apply_style,
                ),
            ]));
        }

        let content = Paragraph::new(lines);
        frame.render_widget(content, inner);
    }

    /// Render the details popup
    fn render_details(&self, frame: &mut Frame, area: Rect) {
        if !self.show_details {
            return;
        }

        // Calculate dialog size based on content
        let content_lines: Vec<&str> = self.details_content.lines().collect();
        let max_line_len = content_lines.iter().map(|l| l.len()).max().unwrap_or(40);

        let dialog_width = (max_line_len as u16 + 6)
            .clamp(50, 80)
            .min(area.width.saturating_sub(4));
        let dialog_height = (content_lines.len() as u16 + 6)
            .min(20)
            .min(area.height.saturating_sub(4));
        let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
        let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

        let dialog_area = Rect::new(
            area.x + dialog_x,
            area.y + dialog_y,
            dialog_width,
            dialog_height,
        );

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.details_title))
            .border_style(Style::default().fg(Color::Cyan));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let mut lines = vec![Line::from("")];

        for line in self.details_content.lines() {
            lines.push(Line::from(Span::styled(
                format!(" {}", line),
                Style::default().fg(Color::White),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Press Enter or Esc to close ",
            Style::default().fg(Color::DarkGray),
        )));

        let content = Paragraph::new(lines);
        frame.render_widget(content, inner);
    }
}

/// What a node's last diagnostics run found, shown on the dashboard
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticsSummary {
    /// Names of the checks that failed / warned
    pub failures: Vec<String>,
    pub warnings: Vec<String>,
    pub checked_at: std::time::Instant,
}

impl DiagnosticsSummary {
    /// From a run's checks by category. A category summary that only
    /// repeats the checks beside it ("CNI (Cilium): warnings in …") is left out.
    pub fn of(categories: &[(&str, Vec<DiagnosticCheck>)]) -> Self {
        let mut failures = Vec::new();
        let mut warnings = Vec::new();
        for (_, checks) in categories {
            let others_flagged = |summary: &DiagnosticCheck| {
                checks
                    .iter()
                    .any(|c| c.id != summary.id && c.status != CheckStatus::Pass)
            };
            for check in checks {
                if check.id == "cni" && others_flagged(check) {
                    continue;
                }
                let list = match check.status {
                    CheckStatus::Fail => &mut failures,
                    CheckStatus::Warn => &mut warnings,
                    _ => continue,
                };
                if !list.contains(&check.name) {
                    list.push(check.name.clone());
                }
            }
        }
        Self {
            failures,
            warnings,
            checked_at: std::time::Instant::now(),
        }
    }
}

/// Log how long a refresh stage took since `lap`, and restart the lap
fn log_stage(address: &str, stage: &str, lap: &mut std::time::Instant) {
    let now = std::time::Instant::now();
    tracing::debug!("diagnostics {}: {} took {:?}", address, stage, now - *lap);
    *lap = now;
}

impl Component for DiagnosticsComponent {
    /// A click on a check selects it and its category
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if self.show_confirmation || self.show_details {
            return Ok(None);
        }
        let hit = self
            .category_rows
            .iter()
            .enumerate()
            .find_map(|(category, rows)| rows.clicked(&mouse).map(|i| (category, i)));
        if let Some((category, check)) = hit {
            self.selected_category = category;
            self.selected_check = check;
            self.update_table_state();
        }
        Ok(None)
    }

    fn key_hints(&self) -> Option<Line<'static>> {
        let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Cyan));
        let spans = if self.show_details {
            vec![key("[Esc]"), Span::raw(" close")]
        } else if self.show_confirmation {
            vec![
                key("[←/→]"),
                Span::raw(" choose  "),
                key("[Enter]"),
                Span::raw(" confirm  "),
                key("[Esc]"),
                Span::raw(" cancel"),
            ]
        } else {
            vec![
                key("[j/k]"),
                Span::raw(" navigate  "),
                key("[Tab]"),
                Span::raw(" section  "),
                key("[Enter]"),
                Span::raw(" details/fix  "),
                key("[r]"),
                Span::raw(" refresh"),
            ]
        };
        Some(Line::from(spans))
    }

    fn uses_tab_key(&self) -> bool {
        true
    }

    fn uses_horizontal_keys(&self) -> bool {
        // The fix dialog picks Yes/No with Left/Right
        self.show_confirmation
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        // Handle details popup (if showing)
        if self.show_details {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.show_details = false;
                    return Ok(None);
                }
                _ => return Ok(None),
            }
        }

        if self.show_confirmation {
            let is_host_command = self
                .pending_action
                .as_ref()
                .map(|p| p.fix.action.is_host_command())
                .unwrap_or(false);

            match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.confirmation_selection = 0;
                    return Ok(None);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.confirmation_selection = 1;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    if is_host_command {
                        if self.confirmation_selection == 0 {
                            if let Some(pending) = &self.pending_action
                                && let FixAction::HostCommand { command, .. } = &pending.fix.action
                            {
                                // Spawn a thread to copy to clipboard and keep it alive
                                // This prevents the "clipboard dropped quickly" warning
                                let cmd = command.clone();
                                std::thread::spawn(move || {
                                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                        let _ = clipboard.set_text(cmd);
                                        // Keep clipboard alive for a bit so clipboard managers can read
                                        std::thread::sleep(std::time::Duration::from_millis(100));
                                    }
                                });
                                self.copy_feedback_until = Some(Instant::now());
                            }
                        } else {
                            self.show_confirmation = false;
                            self.pending_action = None;
                        }
                    } else if self.confirmation_selection == 0 {
                        self.show_confirmation = false;
                        self.pending_action = None;
                    } else {
                        return Ok(Some(Action::ApplyDiagnosticFix));
                    }
                    return Ok(None);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.show_confirmation = false;
                    self.pending_action = None;
                    return Ok(None);
                }
                _ => return Ok(None),
            }
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                return Ok(Some(Action::Back));
            }
            KeyCode::Char('r') => {
                return Ok(Some(Action::Refresh));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.next_check();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.prev_check();
            }
            KeyCode::Tab => {
                self.next_category();
            }
            KeyCode::BackTab => {
                self.prev_category();
            }
            KeyCode::Enter => {
                self.initiate_fix();
            }
            _ => {}
        }

        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        // Only what this frame draws is clickable (e.g. no hidden Addons)
        self.category_rows = Default::default();
        let chunks = Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).split(area);

        // Get header info from data
        let (hostname, address, cni_label) = self
            .data()
            .map(|d| {
                (
                    d.hostname.clone(),
                    d.address.clone(),
                    d.context.cni_type.name(),
                )
            })
            .unwrap_or_else(|| (String::new(), String::new(), "Unknown"));

        // Header
        let header_text = format!(" Diagnostics: {} ({}) [{}] ", hostname, address, cni_label);
        let header = Paragraph::new(header_text)
            .style(Style::default().add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::BOTTOM));
        frame.render_widget(header, chunks[0]);

        if let Some(error) = self.state.error() {
            let error_msg =
                Paragraph::new(format!("Error: {}", error)).style(Style::default().fg(Color::Red));
            frame.render_widget(error_msg, chunks[1]);
        } else if self.state.is_loading()
            && self
                .data()
                .is_none_or(|d| d.system_checks.is_empty() && d.service_checks.is_empty())
        {
            let loading = Paragraph::new("Running diagnostics...")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, chunks[1]);
        } else if let Some(data) = self.data() {
            // Dynamically size Addons section based on whether addons are detected
            let addons_height = if data.detected_addons.any_detected() {
                Constraint::Length(5)
            } else {
                Constraint::Length(0)
            };

            let content_chunks = Layout::vertical([
                Constraint::Length(7), // System Health (Memory, CPU, 3 certs = 5 items + 2 border)
                Constraint::Length(4), // Kubernetes Components (etcd, pod_health = 2 items + 2 border)
                Constraint::Length(5), // CNI
                Constraint::Fill(1),   // Services
                addons_height,         // Addons (if any)
            ])
            .split(chunks[1]);

            // Clone checks for rendering to avoid borrow issues
            let system_checks = data.system_checks.clone();
            let kubernetes_checks = data.kubernetes_checks.clone();
            let cni_checks = data.cni_checks.clone();
            let service_checks = data.service_checks.clone();
            let addon_checks = data.addon_checks.clone();
            let any_addons = data.detected_addons.any_detected();

            self.render_category(
                frame,
                content_chunks[0],
                0,
                &system_checks,
                self.selected_category == 0,
            );
            self.render_category(
                frame,
                content_chunks[1],
                1,
                &kubernetes_checks,
                self.selected_category == 1,
            );
            self.render_category(
                frame,
                content_chunks[2],
                2,
                &cni_checks,
                self.selected_category == 2,
            );
            self.render_category(
                frame,
                content_chunks[3],
                3,
                &service_checks,
                self.selected_category == 3,
            );

            // Only render Addons section if addons are detected
            if any_addons {
                self.render_category(
                    frame,
                    content_chunks[4],
                    4,
                    &addon_checks,
                    self.selected_category == 4,
                );
            }
        }

        if self.show_confirmation {
            self.render_confirmation(frame, area);
        }

        if self.show_details {
            self.render_details(frame, area);
        }

        Ok(())
    }
}

impl crate::components::BackgroundRefresh for DiagnosticsComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        // A fix applied in the background (see `begin_apply_fix`)
        if self.applying_fix {
            self.applying_fix = false;
            self.apply_result = refreshed.apply_result;
        }
        self.state = refreshed.state;
        self.ensure_valid_selection();
    }
}
