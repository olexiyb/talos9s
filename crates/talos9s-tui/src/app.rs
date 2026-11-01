//! Application state and main loop

use crate::action::Action;
use crate::components::BackgroundRefresh;
use crate::components::cluster::{FocusedPane, NavMenuItem, PaneWidth};
use crate::components::rolling_operations::RollingNodeInfo;
use crate::components::wizard::{WizardComponent, WizardState};
use crate::components::{
    ClusterComponent, Component, DiagnosticsComponent, EtcdComponent, GroupComponent,
    LifecycleComponent, MultiLogsComponent, NetworkStatsComponent, NodeInfoComponent,
    NodeOperationsComponent, ProcessesComponent, RollingOperationsComponent, SecurityComponent,
    StorageComponent, VolumesComponent, WorkloadHealthComponent,
};
use crate::components::{GroupKind, NodeInfoTab};
use crate::tui::{self, Tui};
use color_eyre::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A view refresh running on a clone of the view's component
enum RefreshJob {
    Diagnostics(JoinHandle<DiagnosticsComponent>),
    Security(JoinHandle<SecurityComponent>),
    Lifecycle(JoinHandle<LifecycleComponent>),
    Workloads(JoinHandle<WorkloadHealthComponent>),
    Storage(JoinHandle<StorageComponent>),
    NodeInfo(JoinHandle<NodeInfoComponent>),
    Processes(JoinHandle<ProcessesComponent>),
    Etcd(JoinHandle<EtcdComponent>),
    Volumes(JoinHandle<VolumesComponent>),
    Group(JoinHandle<GroupComponent>),
}

impl RefreshJob {
    fn is_finished(&self) -> bool {
        match self {
            RefreshJob::Diagnostics(h) => h.is_finished(),
            RefreshJob::Security(h) => h.is_finished(),
            RefreshJob::Lifecycle(h) => h.is_finished(),
            RefreshJob::Workloads(h) => h.is_finished(),
            RefreshJob::Storage(h) => h.is_finished(),
            RefreshJob::NodeInfo(h) => h.is_finished(),
            RefreshJob::Processes(h) => h.is_finished(),
            RefreshJob::Etcd(h) => h.is_finished(),
            RefreshJob::Volumes(h) => h.is_finished(),
            RefreshJob::Group(h) => h.is_finished(),
        }
    }

    fn abort(&self) {
        match self {
            RefreshJob::Diagnostics(h) => h.abort(),
            RefreshJob::Security(h) => h.abort(),
            RefreshJob::Lifecycle(h) => h.abort(),
            RefreshJob::Workloads(h) => h.abort(),
            RefreshJob::Storage(h) => h.abort(),
            RefreshJob::NodeInfo(h) => h.abort(),
            RefreshJob::Processes(h) => h.abort(),
            RefreshJob::Etcd(h) => h.abort(),
            RefreshJob::Volumes(h) => h.abort(),
            RefreshJob::Group(h) => h.abort(),
        }
    }
}

/// Run `refresh()` on a clone of a component in a spawned task, recording a
/// failure as the clone's error, and evaluate to the task's handle. (A macro
/// because each component's `refresh` is an inherent async method.)
macro_rules! spawn_refresh {
    ($component:expr) => {{
        let mut component = $component.clone();
        tokio::spawn(async move {
            if let Err(e) = component.refresh().await {
                component.set_error(e.to_string());
            }
            component
        })
    }};
}

/// Whether a key event is Ctrl+C
fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
}

/// Current view in the application
#[derive(Debug, Clone, PartialEq)]
enum View {
    Cluster,
    MultiLogs,
    Etcd,
    Processes,
    Network,
    Diagnostics,
    Security,
    Lifecycle,
    Workloads,
    Volumes,
    Storage,
    NodeInfo,
    NodeOperations,
    RollingOperations,
    Group,
    /// A menu view with nothing to show for the selection (`App::hint`)
    Hint,
}

/// How long the selection must rest before the view follows it, so moving
/// through ten nodes loads one; logs restart streams, so they wait longer
const FOLLOW_DELAY: Duration = Duration::from_millis(300);
const FOLLOW_DELAY_LOGS: Duration = Duration::from_millis(500);
/// Views kept for coming back to them
const VIEW_CACHE_SIZE: usize = 4;

/// A view put aside when another replaced it, by (view, selection)
enum CachedView {
    Network(Box<NetworkStatsComponent>),
    Storage(Box<StorageComponent>),
    NodeInfo(Box<NodeInfoComponent>),
    Processes(Box<ProcessesComponent>),
    Diagnostics(Box<DiagnosticsComponent>),
    Group(Box<GroupComponent>),
    Volumes(Box<VolumesComponent>),
    Workloads(Box<WorkloadHealthComponent>),
    Etcd(Box<EtcdComponent>),
    Security(Box<SecurityComponent>),
    Lifecycle(Box<LifecycleComponent>),
}

/// Main application state
pub struct App {
    /// Whether the application should quit
    should_quit: bool,
    /// Component shown on the right (or an operations overlay over everything)
    view: View,
    /// Menu entry the right side shows; `view` is the component realizing it
    /// for the selection (e.g. Network is a node's or a group's view)
    pane: NavMenuItem,
    /// The keyboard is in the view on the right rather than the tree
    view_has_focus: bool,
    /// Nodes pane width chosen with `[` `]` `Z`; `None` fits it to the screen
    pane_width: Option<PaneWidth>,
    /// Width to return to when zoom is turned off
    width_before_zoom: Option<Option<PaneWidth>>,
    /// Selection the right side shows, and a newer one waiting out `FOLLOW_DELAY`
    applied_target: String,
    pending_target: Option<(String, Instant)>,
    /// Why the menu view has nothing to show (`View::Hint`)
    hint: Option<String>,
    /// View under an operations overlay, shown again when it closes
    overlay_return: View,
    /// Nodes pane and view areas of the last frame, for the mouse
    tree_area: Rect,
    view_area: Rect,
    /// Recently replaced views, oldest first
    view_cache: Vec<(String, CachedView)>,
    /// The key being handled opens a view from the menu or services list,
    /// which then takes the keyboard
    focus_after_show: bool,
    /// Cluster component
    cluster: ClusterComponent,
    /// Multi-service logs component (created when viewing logs)
    multi_logs: Option<MultiLogsComponent>,
    /// Log levels last shown in the logs view, restored when it is reopened
    log_level_filter: crate::components::multi_logs::LevelFilter,
    /// Volumes view toggle, filter and selection, kept across visits
    volumes_view_state: Option<crate::components::volumes::VolumesViewState>,
    /// Etcd status component (created when viewing etcd)
    etcd: Option<EtcdComponent>,
    /// Processes component (created when viewing processes)
    processes: Option<ProcessesComponent>,
    /// Network stats component (created when viewing network)
    network: Option<NetworkStatsComponent>,
    /// Diagnostics component (created when viewing diagnostics)
    diagnostics: Option<DiagnosticsComponent>,
    /// Security component (created when viewing certificates)
    security: Option<SecurityComponent>,
    /// Lifecycle component (created when viewing versions)
    lifecycle: Option<LifecycleComponent>,
    /// Workload health component (created when viewing workloads)
    workloads: Option<WorkloadHealthComponent>,
    /// Volumes component (created when viewing volumes)
    volumes: Option<VolumesComponent>,
    /// Storage component (created when viewing disks/volumes)
    storage: Option<StorageComponent>,
    /// Containers, machine config and extensions of a node
    node_info: Option<NodeInfoComponent>,
    /// Node operations component (overlay for node operations)
    node_operations: Option<NodeOperationsComponent>,
    /// Rolling operations component (overlay for multi-node operations)
    rolling_operations: Option<RollingOperationsComponent>,
    /// Diagnostics / storage / network of a node group
    group: Option<GroupComponent>,
    /// Number of log lines to fetch per service
    tail_lines: i32,
    /// Tick rate for animations (ms)
    tick_rate: Duration,
    /// Channel for async action results
    action_rx: mpsc::UnboundedReceiver<AsyncResult>,
    #[allow(dead_code)] // Will be used for background log streaming
    action_tx: mpsc::UnboundedSender<AsyncResult>,
    /// Custom config file path (from --config flag)
    config_path: Option<String>,
    /// In-flight background refresh of the current view (see `start_refresh_job`)
    refresh_job: Option<RefreshJob>,
    /// In-flight Kubernetes client creation for rolling operations
    rolling_k8s_fetch: Option<JoinHandle<Option<kube::Client>>>,
    /// Whether running in insecure mode (no TLS)
    insecure: bool,
    /// Endpoint for insecure mode
    insecure_endpoint: Option<String>,
}

/// Results from async operations
#[derive(Debug)]
#[allow(dead_code)]
enum AsyncResult {
    Connected,
    Refreshed,
    LogsLoaded(String),
    Error(String),
}

impl Default for App {
    fn default() -> Self {
        Self::new(None, None, 500, false, None)
    }
}

impl App {
    pub fn new(
        config_path: Option<String>,
        context: Option<String>,
        tail_lines: i32,
        insecure: bool,
        insecure_endpoint: Option<String>,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        Self {
            should_quit: false,
            view: View::Cluster,
            pane: NavMenuItem::Dashboard,
            view_has_focus: false,
            pane_width: None,
            width_before_zoom: None,
            applied_target: String::new(),
            pending_target: None,
            hint: None,
            overlay_return: View::Cluster,
            tree_area: Rect::default(),
            view_area: Rect::default(),
            view_cache: Vec::new(),
            focus_after_show: false,
            cluster: ClusterComponent::new(config_path.clone(), context),
            multi_logs: None,
            log_level_filter: crate::components::multi_logs::ALL_LEVELS,
            volumes_view_state: None,
            etcd: None,
            processes: None,
            network: None,
            diagnostics: None,
            security: None,
            lifecycle: None,
            workloads: None,
            volumes: None,
            storage: None,
            node_info: None,
            node_operations: None,
            rolling_operations: None,
            group: None,
            tail_lines,
            tick_rate: Duration::from_millis(100),
            action_rx,
            action_tx,
            config_path,
            refresh_job: None,
            rolling_k8s_fetch: None,
            insecure,
            insecure_endpoint,
        }
    }

    /// Run the application
    pub async fn run(&mut self) -> Result<()> {
        // Install panic hook
        tui::install_panic_hook();

        // Initialize terminal
        let mut terminal = tui::init()?;

        // Main loop - choose based on mode
        let result = if self.insecure {
            self.insecure_loop(&mut terminal).await
        } else {
            self.main_loop(&mut terminal).await
        };

        // Restore terminal
        tui::restore()?;

        result
    }

    /// Insecure mode event loop - Bootstrap Wizard
    async fn insecure_loop(&mut self, terminal: &mut Tui) -> Result<()> {
        let endpoint = self
            .insecure_endpoint
            .clone()
            .expect("Insecure mode requires endpoint");

        let mut wizard = WizardComponent::new(endpoint);

        // Connect on startup
        wizard.connect().await?;

        // Polling interval for wait states
        let poll_interval = Duration::from_secs(5);
        let mut last_poll = std::time::Instant::now();

        // Spinner animation interval
        let spinner_interval = Duration::from_millis(100);
        let mut last_spinner = std::time::Instant::now();

        loop {
            // Advance spinner for animations
            if last_spinner.elapsed() >= spinner_interval {
                last_spinner = std::time::Instant::now();
                wizard.data_mut().advance_spinner();
            }

            // Draw
            terminal.draw(|frame| {
                let _ = wizard.draw(frame, frame.area());
            })?;

            // Handle events with timeout
            if event::poll(self.tick_rate)? {
                match event::read()? {
                    Event::Key(key) if is_ctrl_c(&key) => {
                        self.should_quit = true;
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = wizard.handle_key_event(key)? {
                            match action {
                                Action::Quit => {
                                    self.should_quit = true;
                                }
                                Action::WizardGenConfig => {
                                    self.wizard_generate_config(&mut wizard).await;
                                }
                                Action::WizardApplyConfig => {
                                    self.wizard_apply_config(&mut wizard).await;
                                }
                                Action::WizardBootstrap => {
                                    self.wizard_bootstrap(&mut wizard).await;
                                }
                                Action::WizardRetry => {
                                    wizard.connect().await?;
                                }
                                Action::WizardComplete(context) => {
                                    // Exit wizard - print instructions
                                    self.should_quit = true;
                                    if let Some(ctx) = context {
                                        tracing::info!("Wizard complete. Context: {}", ctx);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Event::Resize(_, _) => {
                        // Terminal will automatically resize on next draw
                    }
                    _ => {}
                }
            }

            // Polling for wait states
            if last_poll.elapsed() >= poll_interval {
                last_poll = std::time::Instant::now();
                self.wizard_poll(&mut wizard).await;
            }

            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// Generate config in wizard
    async fn wizard_generate_config(&self, wizard: &mut WizardComponent) {
        use talos_rs::gen_config;

        // Extract data we need before any mutations
        let cluster_name = wizard.data().cluster_name.clone();
        let k8s_endpoint = wizard.data().k8s_endpoint.clone();
        let output_dir = wizard.data().output_dir.clone();
        let endpoint = wizard.data().endpoint.clone();
        // TODO: Use disk with gen_config_with_disk when --install-disk flag support is added
        let _disk = wizard
            .data()
            .selected_disk
            .as_ref()
            .map(|d| d.dev_path.clone());

        // Build additional SANs
        let sans: Vec<&str> = vec![&endpoint, "127.0.0.1"];

        // Generate config
        // Note: For now we use gen_config without disk selection
        // TODO: Add gen_config_with_disk that uses --install-disk flag
        match gen_config(&cluster_name, &k8s_endpoint, &output_dir, Some(&sans), true).await {
            Ok(result) => {
                // Merge talosconfig and set endpoint/node
                let merge_success = self
                    .wizard_merge_config(&result.talosconfig_path, &cluster_name, &endpoint)
                    .await;

                if merge_success {
                    wizard.data_mut().config_result = Some(result);
                    wizard.data_mut().context_name = Some(cluster_name);
                    wizard.transition(WizardState::ConfigReady);
                } else {
                    wizard.set_error("Failed to merge talosconfig".to_string());
                }
            }
            Err(e) => {
                wizard.set_error(format!("Failed to generate config: {}", e));
            }
        }
    }

    /// Merge talosconfig into user's config
    async fn wizard_merge_config(
        &self,
        config_path: &str,
        context_name: &str,
        endpoint: &str,
    ) -> bool {
        use tokio::process::Command;

        // Run: talosctl config merge <path>
        let merge_output = Command::new("talosctl")
            .args(["config", "merge", config_path])
            .output()
            .await;

        if let Err(e) = &merge_output {
            tracing::warn!("Failed to run talosctl config merge: {}", e);
            return false;
        }

        if let Ok(out) = &merge_output
            && !out.status.success()
        {
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::warn!("Failed to merge config: {}", stderr);
            return false;
        }

        // Set the endpoint on the context
        let endpoint_output = Command::new("talosctl")
            .args(["--context", context_name, "config", "endpoint", endpoint])
            .output()
            .await;

        if let Err(e) = &endpoint_output {
            tracing::warn!("Failed to set endpoint: {}", e);
            return false;
        }

        // Set the node on the context
        let node_output = Command::new("talosctl")
            .args(["--context", context_name, "config", "node", endpoint])
            .output()
            .await;

        if let Err(e) = &node_output {
            tracing::warn!("Failed to set node: {}", e);
            return false;
        }

        true
    }

    /// Apply config in wizard
    async fn wizard_apply_config(&self, wizard: &mut WizardComponent) {
        use std::time::Instant;
        use talos_rs::apply_config_insecure;

        wizard.transition(WizardState::Applying);

        // Extract values we need before mutating
        let endpoint = wizard.data().endpoint.clone();
        let cluster_name = wizard.data().cluster_name.clone();
        let config_path = {
            let data = wizard.data();
            data.config_result.as_ref().map(|r| match data.node_type {
                crate::components::wizard::NodeType::Controlplane => r.controlplane_path.clone(),
                crate::components::wizard::NodeType::Worker => r.worker_path.clone(),
            })
        };

        if let Some(path) = config_path {
            match apply_config_insecure(&endpoint, &path).await {
                Ok(result) => {
                    if result.success {
                        // Start waiting for reboot
                        wizard.data_mut().wait_started = Some(Instant::now());
                        wizard.data_mut().context_name = Some(cluster_name);
                        wizard.transition(WizardState::WaitingReboot);
                    } else {
                        wizard.set_error(result.message);
                    }
                }
                Err(e) => {
                    wizard.set_error(format!("Failed to apply config: {}", e));
                }
            }
        } else {
            wizard.set_error("No config generated".to_string());
        }
    }

    /// Bootstrap cluster in wizard
    async fn wizard_bootstrap(&self, wizard: &mut WizardComponent) {
        use std::time::Instant;
        use tokio::process::Command;

        wizard.transition(WizardState::Bootstrapping);

        let context = wizard.data().context_name.clone();

        if let Some(ctx) = context {
            let output = Command::new("talosctl")
                .args(["--context", &ctx, "bootstrap"])
                .output()
                .await;

            match output {
                Ok(out) if out.status.success() => {
                    wizard.data_mut().wait_started = Some(Instant::now());
                    wizard.transition(WizardState::WaitingHealthy);
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    wizard.set_error(format!("Bootstrap failed: {}", stderr));
                }
                Err(e) => {
                    wizard.set_error(format!("Failed to run bootstrap: {}", e));
                }
            }
        } else {
            wizard.set_error("No context available for bootstrap".to_string());
        }
    }

    /// Poll for state changes in wait states
    async fn wizard_poll(&self, wizard: &mut WizardComponent) {
        use tokio::process::Command;

        match wizard.state() {
            WizardState::WaitingReboot => {
                // Increment poll attempts
                wizard.data_mut().poll_attempts += 1;

                // Check if node is back online (with TLS)
                if let Some(ctx) = wizard.data().context_name.clone() {
                    let output = Command::new("talosctl")
                        .args(["--context", &ctx, "version"])
                        .output()
                        .await;

                    match output {
                        Ok(out) if out.status.success() => {
                            wizard.data_mut().last_poll_error = None;
                            wizard.transition(WizardState::ReadyToBootstrap);
                        }
                        Ok(out) => {
                            // Command ran but failed - capture error
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            wizard.data_mut().last_poll_error =
                                Some(stderr.lines().next().unwrap_or("Unknown error").to_string());
                        }
                        Err(e) => {
                            wizard.data_mut().last_poll_error = Some(e.to_string());
                        }
                    }
                }

                // Check for timeout (5 minutes)
                if let Some(started) = wizard.data().wait_started
                    && started.elapsed().as_secs() > 300
                {
                    wizard.set_error("Timeout waiting for node to reboot".to_string());
                }
            }
            WizardState::WaitingHealthy => {
                // Increment poll attempts
                wizard.data_mut().poll_attempts += 1;

                // Check if cluster is healthy
                if let Some(ctx) = wizard.data().context_name.clone() {
                    // Check etcd health
                    let output = Command::new("talosctl")
                        .args(["--context", &ctx, "etcd", "status"])
                        .output()
                        .await;

                    match output {
                        Ok(out) if out.status.success() => {
                            wizard.data_mut().last_poll_error = None;
                            wizard.transition(WizardState::Complete);
                        }
                        Ok(out) => {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            wizard.data_mut().last_poll_error =
                                Some(stderr.lines().next().unwrap_or("Unknown error").to_string());
                        }
                        Err(e) => {
                            wizard.data_mut().last_poll_error = Some(e.to_string());
                        }
                    }
                }

                // Check for timeout (5 minutes)
                if let Some(started) = wizard.data().wait_started
                    && started.elapsed().as_secs() > 300
                {
                    wizard.set_error("Timeout waiting for cluster to become healthy".to_string());
                }
            }
            _ => {}
        }
    }

    /// Main event loop
    async fn main_loop(&mut self, terminal: &mut Tui) -> Result<()> {
        // Load the cluster list from talosconfig. This does no network I/O:
        // each cluster connects in the background and fills in as it becomes
        // reachable, so the UI is usable at once even if some are down.
        self.cluster.connect().await?;

        loop {
            // Draw current view
            terminal.draw(|frame| {
                let area = frame.area();
                // Skip drawing into a zero-sized area (e.g. a 0x0 terminal or a
                // momentary 0-height during resize) — ratatui panics otherwise.
                if area.width == 0 || area.height == 0 {
                    return;
                }
                self.draw_workspace(frame, area);
            })?;

            // Handle events with timeout
            if event::poll(self.tick_rate)? {
                match event::read()? {
                    // The terminal is in raw mode, so Ctrl+C arrives as a key
                    // rather than SIGINT: quit from any view
                    Event::Key(key) if is_ctrl_c(&key) => {
                        self.should_quit = true;
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = self.dispatch_key(key)? {
                            self.handle_action(action).await?;
                        }
                        self.focus_after_show = false;
                    }
                    Event::Mouse(mouse) => {
                        if let Some(action) = self.dispatch_mouse(mouse)? {
                            self.handle_action(action).await?;
                        }
                    }
                    Event::Resize(w, h) => {
                        self.handle_action(Action::Resize(w, h)).await?;
                    }
                    _ => {}
                }
            } else {
                // Tick for animations
                self.handle_action(Action::Tick).await?;
            }

            // Hand control to tokio once per loop. The loop otherwise never
            // returns Pending, so tokio's cooperative budget for this task
            // never refills, and once it is spent every await on a budgeted
            // resource (and every poll of a finished task) reads as pending.
            tokio::task::yield_now().await;

            // Check async results (non-blocking)
            self.poll_refresh_job();
            self.poll_view_setup();
            self.follow_selection().await?;

            while let Ok(result) = self.action_rx.try_recv() {
                match result {
                    AsyncResult::Connected => {
                        tracing::info!("Connected to Talos cluster");
                    }
                    AsyncResult::Refreshed => {
                        tracing::info!("Data refreshed");
                    }
                    AsyncResult::LogsLoaded(_content) => {
                        // Legacy - multi_logs uses set_logs directly
                    }
                    AsyncResult::Error(e) => {
                        tracing::error!("Async error: {}", e);
                        if let Some(multi_logs) = &mut self.multi_logs {
                            multi_logs.set_error(e);
                        }
                    }
                }
            }

            // Check if we should quit
            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// Handle an action
    /// Hand a finished background refresh back to its component
    fn poll_refresh_job(&mut self) {
        if !self.refresh_job.as_ref().is_some_and(|j| j.is_finished()) {
            return;
        }
        let Some(job) = self.refresh_job.take() else {
            return;
        };
        // A panicked task is shown as the view's error: that ends its loading
        // state, so auto-refresh retries instead of "Loading..." forever.
        // (Aborted jobs never get here: cancelling takes them out first.)
        macro_rules! finish {
            ($slot:expr, $handle:expr) => {
                if let Some(c) = &mut $slot {
                    match crate::task::poll_finished($handle) {
                        Some(Ok(refreshed)) => c.adopt_refresh(refreshed),
                        Some(Err(e)) => {
                            tracing::error!("View refresh task failed: {}", e);
                            c.set_error(format!("Refresh failed: {e}"));
                        }
                        None => {}
                    }
                }
            };
        }
        match job {
            RefreshJob::Diagnostics(h) => finish!(self.diagnostics, h),
            RefreshJob::Security(h) => finish!(self.security, h),
            RefreshJob::Lifecycle(h) => finish!(self.lifecycle, h),
            RefreshJob::Workloads(h) => finish!(self.workloads, h),
            RefreshJob::Storage(h) => finish!(self.storage, h),
            RefreshJob::NodeInfo(h) => finish!(self.node_info, h),
            RefreshJob::Processes(h) => finish!(self.processes, h),
            RefreshJob::Etcd(h) => finish!(self.etcd, h),
            RefreshJob::Volumes(h) => finish!(self.volumes, h),
            RefreshJob::Group(h) => finish!(self.group, h),
        }
    }

    /// Finish view setup that ran in the background: the Kubernetes client
    /// of rolling operations
    fn poll_view_setup(&mut self) {
        if self
            .rolling_k8s_fetch
            .as_ref()
            .is_some_and(|h| h.is_finished())
            && let Some(Some(Ok(Some(k8s)))) = self
                .rolling_k8s_fetch
                .take()
                .map(crate::task::poll_finished)
            && let Some(rolling_ops) = &mut self.rolling_operations
        {
            rolling_ops.set_k8s_client(k8s);
        }
    }

    /// Drop an in-flight background refresh, e.g. because its view is being
    /// replaced by one for another node; its result would be stale.
    fn cancel_refresh_job(&mut self) {
        if let Some(job) = self.refresh_job.take() {
            job.abort();
        }
    }

    /// Route a key press: overlays first, then the layout keys, then the
    /// tree or the view on the right, whichever has the keyboard
    fn dispatch_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if matches!(self.view, View::NodeOperations | View::RollingOperations) {
            return self.dispatch_view_key(key);
        }
        if !self.view_captures_text() && self.layout_key(key) {
            return Ok(None);
        }
        if self.view == View::Cluster || !self.view_has_focus {
            return self.dispatch_tree_key(key);
        }
        self.dispatch_view_key(key)
    }

    /// `Z` zoom, `[` narrower and `]` wider nodes pane; true when handled
    fn layout_key(&mut self, key: KeyEvent) -> bool {
        let total = self.tree_area.width + self.view_area.width;
        let (current, _) = self.cluster.resolve_pane_width(total, self.pane_width);
        self.pane_width = match key.code {
            KeyCode::Char('Z') if current == PaneWidth::Hidden => {
                self.width_before_zoom.take().flatten()
            }
            KeyCode::Char('Z') => {
                self.width_before_zoom = Some(self.pane_width);
                Some(PaneWidth::Hidden)
            }
            KeyCode::Char('[') => Some(match current {
                PaneWidth::Full => PaneWidth::Compact,
                PaneWidth::Compact | PaneWidth::Hidden => PaneWidth::Hidden,
            }),
            KeyCode::Char(']') => Some(match current {
                PaneWidth::Hidden => PaneWidth::Compact,
                PaneWidth::Compact | PaneWidth::Full => PaneWidth::Full,
            }),
            _ => return false,
        };
        true
    }

    /// A view is taking typed text (a filter or search), so letters are its
    fn view_captures_text(&self) -> bool {
        match self.view {
            View::MultiLogs => self.multi_logs.as_ref().is_some_and(|c| c.captures_text()),
            View::Processes => self.processes.as_ref().is_some_and(|c| c.captures_text()),
            View::Volumes => self.volumes.as_ref().is_some_and(|c| c.captures_text()),
            View::NodeInfo => self.node_info.as_ref().is_some_and(|c| c.captures_text()),
            _ => false,
        }
    }

    /// Keys for the tree and menu. Where the dashboard has its services list,
    /// Tab / Shift+Tab move into the view instead.
    fn dispatch_tree_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        let pane = self.cluster.focused_pane();
        if self.view != View::Cluster {
            // Enter on any tree row (group rows too: Space folds them)
            let into_view = match key.code {
                KeyCode::Tab => pane == FocusedPane::Menu,
                KeyCode::BackTab | KeyCode::Enter => pane == FocusedPane::Nodes,
                _ => false,
            };
            if into_view {
                self.focus_view();
                return Ok(None);
            }
        }
        self.focus_after_show =
            key.code == KeyCode::Enter && matches!(pane, FocusedPane::Menu | FocusedPane::Services);
        self.cluster.handle_key_event(key)
    }

    /// Give the keyboard to the view on the right (the dashboard's is its
    /// services list)
    fn focus_view(&mut self) {
        if self.view == View::Cluster {
            self.cluster.set_focused_pane(FocusedPane::Services);
        } else {
            self.view_has_focus = true;
            self.cluster.set_focused_pane(FocusedPane::Nodes);
        }
    }

    /// Route a key press to the component on the right (or the overlay)
    fn dispatch_view_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        Ok(match self.view {
            View::Cluster => self.cluster.handle_key_event(key)?,
            View::Hint => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Some(Action::Back),
                _ => None,
            },
            View::MultiLogs => {
                if let Some(multi_logs) = &mut self.multi_logs {
                    multi_logs.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Etcd => {
                if let Some(etcd) = &mut self.etcd {
                    etcd.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Processes => {
                if let Some(processes) = &mut self.processes {
                    processes.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Network => {
                if let Some(network) = &mut self.network {
                    network.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Diagnostics => {
                if let Some(diagnostics) = &mut self.diagnostics {
                    diagnostics.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Security => {
                if let Some(security) = &mut self.security {
                    security.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Lifecycle => {
                if let Some(lifecycle) = &mut self.lifecycle {
                    lifecycle.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Volumes => {
                if let Some(volumes) = &mut self.volumes {
                    volumes.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Workloads => {
                if let Some(workloads) = &mut self.workloads {
                    workloads.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Storage => {
                if let Some(storage) = &mut self.storage {
                    storage.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::NodeInfo => {
                if let Some(node_info) = &mut self.node_info {
                    node_info.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::NodeOperations => {
                if let Some(node_ops) = &mut self.node_operations {
                    node_ops.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::RollingOperations => {
                if let Some(rolling_ops) = &mut self.rolling_operations {
                    rolling_ops.handle_key_event(key)?
                } else {
                    None
                }
            }
            View::Group => {
                if let Some(group) = &mut self.group {
                    group.handle_key_event(key)?
                } else {
                    None
                }
            }
        })
    }

    /// Route a mouse event to the pane under the pointer; a click also gives
    /// that pane the keyboard. The tree, the dashboard and the node info view
    /// handle the mouse themselves; other views get the wheel as Up/Down.
    fn dispatch_mouse(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        let overlay = matches!(self.view, View::NodeOperations | View::RollingOperations);
        let at = |r: Rect| {
            mouse.column >= r.x
                && mouse.column < r.x + r.width
                && mouse.row >= r.y
                && mouse.row < r.y + r.height
        };
        let click = matches!(mouse.kind, MouseEventKind::Down(_));
        if !overlay && (self.view == View::Cluster || at(self.tree_area)) {
            if click {
                self.view_has_focus = false;
            }
            return self.cluster.handle_mouse_event(mouse);
        }
        if !overlay && !at(self.view_area) {
            return Ok(None);
        }
        if click && !overlay {
            self.focus_view();
        }
        if self.view == View::NodeInfo
            && let Some(node_info) = &mut self.node_info
        {
            return node_info.handle_mouse_event(mouse);
        }
        let code = match mouse.kind {
            MouseEventKind::ScrollUp => KeyCode::Up,
            MouseEventKind::ScrollDown => KeyCode::Down,
            _ => return Ok(None),
        };
        self.dispatch_view_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Header, nodes pane and key line from the cluster component, the
    /// current view on the right, and an operations overlay on top
    fn draw_workspace(&mut self, frame: &mut Frame, area: Rect) {
        let overlay = matches!(self.view, View::NodeOperations | View::RollingOperations);
        let base = if overlay {
            self.overlay_return.clone()
        } else {
            self.view.clone()
        };
        let dashboard = base == View::Cluster;
        let view_focused = self.view_has_focus && !dashboard && !overlay;
        let (tree, right) =
            self.cluster
                .draw_frame(frame, area, self.pane_width, view_focused, dashboard);
        self.tree_area = tree;
        self.view_area = right;
        macro_rules! draw {
            ($slot:expr) => {
                if let Some(c) = &mut $slot {
                    let _ = c.draw(frame, right);
                }
            };
        }
        match base {
            View::Cluster => self.cluster.draw_details_pane(frame, right),
            View::Hint => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(format!(" {} ", self.pane.label()));
                let text = Paragraph::new(format!("\n  {}", self.hint.as_deref().unwrap_or("")))
                    .style(Style::default().fg(Color::DarkGray))
                    .block(block);
                frame.render_widget(text, right);
            }
            View::MultiLogs => draw!(self.multi_logs),
            View::Etcd => draw!(self.etcd),
            View::Processes => draw!(self.processes),
            View::Network => draw!(self.network),
            View::Diagnostics => draw!(self.diagnostics),
            View::Security => draw!(self.security),
            View::Lifecycle => draw!(self.lifecycle),
            View::Workloads => draw!(self.workloads),
            View::Volumes => draw!(self.volumes),
            View::Storage => draw!(self.storage),
            View::NodeInfo => draw!(self.node_info),
            View::Group => draw!(self.group),
            View::NodeOperations | View::RollingOperations => {}
        }
        match self.view {
            View::NodeOperations => {
                if let Some(c) = &mut self.node_operations {
                    let _ = c.draw(frame, area);
                }
            }
            View::RollingOperations => {
                if let Some(c) = &mut self.rolling_operations {
                    let _ = c.draw(frame, area);
                }
            }
            _ => {}
        }
    }

    /// Selection key of what the menu view `item` shows: cluster-wide views
    /// change only with the cluster
    fn scope_key(&self, item: NavMenuItem) -> String {
        match item {
            NavMenuItem::Etcd | NavMenuItem::Certs | NavMenuItem::Lifecycle => {
                self.cluster.cluster_key()
            }
            _ => self.cluster.target_key(),
        }
    }

    /// Bookkeeping once `view` shows menu entry `item` for the selection
    fn show_in_pane(&mut self, item: NavMenuItem, view: View) {
        self.pane = item;
        self.view = view;
        self.cluster.set_active_menu(item);
        self.applied_target = self.scope_key(item);
        self.pending_target = None;
        if self.view == View::Cluster {
            self.view_has_focus = false;
        } else {
            if self.cluster.focused_pane() == FocusedPane::Services {
                self.cluster.set_focused_pane(FocusedPane::Nodes);
            }
            if std::mem::take(&mut self.focus_after_show) {
                self.view_has_focus = true;
            }
        }
    }

    /// Close the view on the right before another replaces it: stop log
    /// streams, keep settings that carry over, and cache the rest so coming
    /// back shows it at once
    fn leave_view(&mut self) {
        self.cancel_refresh_job();
        self.hint = None;
        let key = format!("{:?}|{}", self.pane, self.applied_target);
        let cached = match self.view {
            View::MultiLogs => {
                if let Some(mut logs) = self.multi_logs.take() {
                    logs.stop_streaming();
                    self.log_level_filter = logs.level_filter();
                }
                None
            }
            View::Volumes => self.volumes.take().map(|v| {
                self.volumes_view_state = Some(v.view_state());
                CachedView::Volumes(Box::new(v))
            }),
            View::Network => self
                .network
                .take()
                .map(|c| CachedView::Network(Box::new(c))),
            View::Storage => self
                .storage
                .take()
                .map(|c| CachedView::Storage(Box::new(c))),
            View::NodeInfo => self
                .node_info
                .take()
                .map(|c| CachedView::NodeInfo(Box::new(c))),
            View::Processes => self
                .processes
                .take()
                .map(|c| CachedView::Processes(Box::new(c))),
            View::Diagnostics => self
                .diagnostics
                .take()
                .map(|c| CachedView::Diagnostics(Box::new(c))),
            View::Group => self.group.take().map(|c| CachedView::Group(Box::new(c))),
            View::Workloads => self
                .workloads
                .take()
                .map(|c| CachedView::Workloads(Box::new(c))),
            View::Etcd => self.etcd.take().map(|c| CachedView::Etcd(Box::new(c))),
            View::Security => self
                .security
                .take()
                .map(|c| CachedView::Security(Box::new(c))),
            View::Lifecycle => self
                .lifecycle
                .take()
                .map(|c| CachedView::Lifecycle(Box::new(c))),
            _ => None,
        };
        if let Some(cached) = cached {
            self.view_cache.retain(|(k, _)| *k != key);
            self.view_cache.push((key, cached));
            if self.view_cache.len() > VIEW_CACHE_SIZE {
                self.view_cache.remove(0);
            }
        }
    }

    /// Show the view for the current selection once it has rested for
    /// `FOLLOW_DELAY`
    async fn follow_selection(&mut self) -> Result<()> {
        if matches!(
            self.view,
            View::Cluster | View::NodeOperations | View::RollingOperations
        ) {
            return Ok(());
        }
        let key = self.scope_key(self.pane);
        if key == self.applied_target {
            self.pending_target = None;
            return Ok(());
        }
        let now = Instant::now();
        match &self.pending_target {
            Some((pending, due)) if *pending == key => {
                if now < *due {
                    return Ok(());
                }
            }
            _ => {
                let delay = if self.pane == NavMenuItem::Logs {
                    FOLLOW_DELAY_LOGS
                } else {
                    FOLLOW_DELAY
                };
                self.pending_target = Some((key, now + delay));
                return Ok(());
            }
        }
        self.pending_target = None;
        self.retarget().await
    }

    /// Show the current menu view for the selection: from the cache when it
    /// was shown recently (then refreshed), otherwise built anew
    async fn retarget(&mut self) -> Result<()> {
        // The node view keeps the tab the user switched to
        let item = match (&self.view, &self.node_info) {
            (View::NodeInfo, Some(n)) => match n.tab() {
                NodeInfoTab::Containers => NavMenuItem::Containers,
                NodeInfoTab::Config => NavMenuItem::Config,
                NodeInfoTab::Extensions => NavMenuItem::Extensions,
            },
            _ => self.pane,
        };
        let key = format!("{:?}|{}", item, self.scope_key(item));
        if self.view_cache.iter().any(|(k, _)| *k == key) {
            self.leave_view();
            if let Some(i) = self.view_cache.iter().position(|(k, _)| *k == key) {
                let (_, cached) = self.view_cache.remove(i);
                self.restore_view(item, cached);
                return Box::pin(self.handle_action(Action::Refresh)).await;
            }
        }
        match self.cluster.action_for(item) {
            Some(action) => Box::pin(self.handle_action(action)).await,
            None => {
                self.leave_view();
                self.hint = Some("Nothing to show for this selection".to_string());
                self.show_in_pane(item, View::Hint);
                Ok(())
            }
        }
    }

    /// Put a cached view back on the right
    fn restore_view(&mut self, item: NavMenuItem, cached: CachedView) {
        let view = match cached {
            CachedView::Network(c) => {
                self.network = Some(*c);
                View::Network
            }
            CachedView::Storage(c) => {
                self.storage = Some(*c);
                View::Storage
            }
            CachedView::NodeInfo(c) => {
                self.node_info = Some(*c);
                View::NodeInfo
            }
            CachedView::Processes(c) => {
                self.processes = Some(*c);
                View::Processes
            }
            CachedView::Diagnostics(c) => {
                self.diagnostics = Some(*c);
                View::Diagnostics
            }
            CachedView::Group(c) => {
                self.group = Some(*c);
                View::Group
            }
            CachedView::Volumes(c) => {
                self.volumes = Some(*c);
                View::Volumes
            }
            CachedView::Workloads(c) => {
                self.workloads = Some(*c);
                View::Workloads
            }
            CachedView::Etcd(c) => {
                self.etcd = Some(*c);
                View::Etcd
            }
            CachedView::Security(c) => {
                self.security = Some(*c);
                View::Security
            }
            CachedView::Lifecycle(c) => {
                self.lifecycle = Some(*c);
                View::Lifecycle
            }
        };
        self.show_in_pane(item, view);
    }

    async fn handle_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Quit => {
                self.should_quit = true;
            }
            Action::Back => match self.view {
                // An overlay closes onto the view under it
                View::NodeOperations => {
                    self.node_operations = None;
                    self.view = std::mem::replace(&mut self.overlay_return, View::Cluster);
                }
                View::RollingOperations => {
                    self.rolling_operations = None;
                    self.view = std::mem::replace(&mut self.overlay_return, View::Cluster);
                }
                // A view hands the keyboard back to the tree and stays shown
                _ => self.view_has_focus = false,
            },
            Action::ShowDashboard => {
                if self.view != View::Cluster {
                    self.leave_view();
                    self.show_in_pane(NavMenuItem::Dashboard, View::Cluster);
                }
            }
            Action::ShowHint(item, message) => {
                self.leave_view();
                self.hint = Some(message);
                self.show_in_pane(item, View::Hint);
            }
            Action::FocusView => self.focus_view(),
            Action::Tick => {
                // The tree stays on screen with every view: keep it current
                if let Some(next_action) = self.cluster.update(Action::Tick)? {
                    Box::pin(self.handle_action(next_action)).await?;
                }
                // Rebuild the node list in the background so nodes that
                // went away (e.g. rebooted) come back without pressing r
                self.cluster.apply_background_refresh();
                self.cluster.start_background_refresh();
                self.cluster.poll_network_info();
                // Auto-refresh selected node stats every 5 seconds, in
                // the background so a struggling node can't freeze the UI
                self.cluster.poll_selected_node_refresh();
                if self.cluster.should_auto_refresh() {
                    self.cluster.refresh_selected_node();
                }
                match self.view {
                    View::Cluster | View::Hint => {}
                    View::MultiLogs => {
                        if let Some(multi_logs) = &mut self.multi_logs
                            && let Some(next_action) = multi_logs.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Etcd => {
                        if let Some(etcd) = &mut self.etcd
                            && let Some(next_action) = etcd.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Processes => {
                        if let Some(processes) = &mut self.processes
                            && let Some(next_action) = processes.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Network => {
                        if let Some(network) = &mut self.network
                            && let Some(next_action) = network.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Diagnostics => {
                        if let Some(diagnostics) = &mut self.diagnostics
                            && let Some(next_action) = diagnostics.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Security => {
                        if let Some(security) = &mut self.security
                            && let Some(next_action) = security.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Lifecycle => {
                        if let Some(lifecycle) = &mut self.lifecycle
                            && let Some(next_action) = lifecycle.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Workloads => {
                        if let Some(workloads) = &mut self.workloads
                            && let Some(next_action) = workloads.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Volumes => {
                        if let Some(volumes) = &mut self.volumes
                            && let Some(next_action) = volumes.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Storage => {
                        if let Some(storage) = &mut self.storage
                            && let Some(next_action) = storage.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::NodeInfo => {
                        if let Some(node_info) = &mut self.node_info
                            && let Some(next_action) = node_info.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::NodeOperations => {
                        if let Some(node_ops) = &mut self.node_operations
                            && let Some(next_action) = node_ops.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::RollingOperations => {
                        if let Some(rolling_ops) = &mut self.rolling_operations
                            && let Some(next_action) = rolling_ops.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Group => {
                        if let Some(group) = &mut self.group
                            && let Some(next_action) = group.update(Action::Tick)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                }
            }
            Action::Resize(_w, _h) => {
                // Terminal will automatically resize on next draw
            }
            Action::Refresh => {
                // Views ask on every tick while a background refresh is in flight
                tracing::debug!("Refresh requested");
                // Refreshes run on the event loop, so their duration is how
                // long the UI is frozen. Log slow ones to find the culprit.
                let started = std::time::Instant::now();
                match self.view {
                    View::Cluster => {
                        self.cluster.refresh().await?;
                    }
                    View::Etcd => {
                        if self.refresh_job.is_none()
                            && let Some(etcd) = &mut self.etcd
                        {
                            // The cluster's current connection: one it
                            // redialled replaces a dead one, and one opened
                            // while it was reconnecting gets one at all
                            if let Some(client) = self.cluster.client() {
                                etcd.set_client(client.clone());
                            }
                            self.refresh_job = Some(RefreshJob::Etcd(spawn_refresh!(etcd)));
                        }
                    }
                    View::Processes => {
                        // Several nodes, each call bounded at 10s: refresh off
                        // the event loop so a slow node can't freeze the UI
                        if self.refresh_job.is_none()
                            && let Some(processes) = &self.processes
                        {
                            self.refresh_job =
                                Some(RefreshJob::Processes(spawn_refresh!(processes)));
                        }
                    }
                    View::Network => {
                        // Starts a pending restart / file view / capture and a
                        // stats fetch in the background
                        if let Some(network) = &mut self.network {
                            network.refresh();
                        }
                    }
                    View::MultiLogs => {
                        // Multi-logs handles its own streaming refresh
                    }
                    View::Diagnostics => {
                        // Slow (Kubernetes API heavy): refresh off the event loop
                        if self.refresh_job.is_none()
                            && let Some(diagnostics) = &self.diagnostics
                        {
                            self.refresh_job =
                                Some(RefreshJob::Diagnostics(spawn_refresh!(diagnostics)));
                        }
                    }
                    View::Security => {
                        // Slow (Kubernetes API heavy): refresh off the event loop
                        if self.refresh_job.is_none()
                            && let Some(security) = &self.security
                        {
                            self.refresh_job = Some(RefreshJob::Security(spawn_refresh!(security)));
                        }
                    }
                    View::Lifecycle => {
                        // Slow (Kubernetes API heavy): refresh off the event loop
                        if self.refresh_job.is_none()
                            && let Some(lifecycle) = &self.lifecycle
                        {
                            self.refresh_job =
                                Some(RefreshJob::Lifecycle(spawn_refresh!(lifecycle)));
                        }
                    }
                    View::Workloads => {
                        // Slow (Kubernetes API heavy): refresh off the event loop
                        if self.refresh_job.is_none()
                            && let Some(workloads) = &self.workloads
                        {
                            self.refresh_job =
                                Some(RefreshJob::Workloads(spawn_refresh!(workloads)));
                        }
                    }
                    View::Volumes => {
                        // Hundreds of kernel reads: refresh off the event loop
                        if self.refresh_job.is_none()
                            && let Some(volumes) = &self.volumes
                        {
                            self.refresh_job = Some(RefreshJob::Volumes(spawn_refresh!(volumes)));
                        }
                    }
                    View::Storage => {
                        // Slow (Kubernetes API heavy): refresh off the event loop
                        if self.refresh_job.is_none()
                            && let Some(storage) = &self.storage
                        {
                            self.refresh_job = Some(RefreshJob::Storage(spawn_refresh!(storage)));
                        }
                    }
                    View::NodeInfo => {
                        // talosctl subprocesses: refresh off the event loop
                        if self.refresh_job.is_none()
                            && let Some(node_info) = &self.node_info
                        {
                            self.refresh_job =
                                Some(RefreshJob::NodeInfo(spawn_refresh!(node_info)));
                        }
                    }
                    View::NodeOperations => {
                        if let Some(node_ops) = &mut self.node_operations {
                            node_ops.refresh();
                        }
                    }
                    View::RollingOperations | View::Hint => {
                        // Rolling operations doesn't have a refresh method
                    }
                    View::Group => {
                        if self.refresh_job.is_none()
                            && let Some(group) = &mut self.group
                        {
                            // The cluster's current connection (it may have
                            // redialled since the view opened)
                            if let Some(client) = self.cluster.client() {
                                group.set_client(client.clone());
                            }
                            self.refresh_job = Some(RefreshJob::Group(spawn_refresh!(group)));
                        }
                    }
                }
                let elapsed = started.elapsed();
                if elapsed >= Duration::from_millis(500) {
                    tracing::warn!("{:?} refresh blocked the UI for {:?}", self.view, elapsed);
                }
            }
            Action::ShowMultiLogs(title, subtitle, nodes, active_services, all_services) => {
                tracing::info!("Viewing logs of {} ({} nodes)", title, nodes.len());
                self.leave_view();

                // Create multi-logs component with all services, marking active ones
                let mut multi_logs =
                    MultiLogsComponent::new(title, subtitle, nodes, active_services, all_services);
                multi_logs.set_level_filter(self.log_level_filter);

                // Each stream targets its node through the cluster's client and
                // starts with the last `tail_lines` lines, so there is no
                // separate first fetch
                if let Some(client) = self.cluster.client() {
                    multi_logs.set_client(client.clone(), self.tail_lines);
                    multi_logs.start_streaming();
                } else {
                    multi_logs.set_error("Not connected to the cluster".to_string());
                }

                self.multi_logs = Some(multi_logs);
                self.show_in_pane(NavMenuItem::Logs, View::MultiLogs);
            }
            Action::ShowNodeDetails(_, _) => {
                // Legacy - no longer used, we use ShowMultiLogs now
            }
            Action::ShowDiagnostics(hostname, address, role, cp_endpoint) => {
                // A node opened from a group view: the tree follows
                if self.view == View::Group {
                    self.cluster.select_node(&hostname);
                }
                self.leave_view();
                // Switch to diagnostics view for a node
                tracing::info!(
                    "ShowDiagnostics: hostname='{}', address='{}', role='{}', cp_endpoint={:?}",
                    hostname,
                    address,
                    role,
                    cp_endpoint
                );

                // Create diagnostics component
                let mut diagnostics = DiagnosticsComponent::new(
                    hostname,
                    address.clone(),
                    role,
                    self.config_path.clone(),
                );

                // Set the control plane endpoint for worker nodes to fetch kubeconfig
                diagnostics.set_controlplane_endpoint(cp_endpoint);
                diagnostics
                    .set_talos_context(self.cluster.current_context_name().map(|s| s.to_string()));

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    // Create a client configured for this specific node
                    let node_client = client.with_node(&address);
                    diagnostics.set_client(node_client);
                    self.refresh_job = Some(RefreshJob::Diagnostics(spawn_refresh!(diagnostics)));
                }

                self.diagnostics = Some(diagnostics);
                self.show_in_pane(NavMenuItem::Diagnostics, View::Diagnostics);
            }
            Action::ApplyDiagnosticFix => {
                // Apply a diagnostic fix (from confirmation dialog)
                // Applied on a clone in the background, then refreshed; the
                // view shows the fix as in progress until it lands
                self.cancel_refresh_job();
                if let Some(diagnostics) = &mut self.diagnostics {
                    let mut worker = diagnostics.clone();
                    diagnostics.begin_apply_fix();
                    self.refresh_job = Some(RefreshJob::Diagnostics(tokio::spawn(async move {
                        if let Err(e) = worker.apply_pending_fix().await {
                            worker.set_error(e.to_string());
                        }
                        if let Err(e) = worker.refresh().await {
                            worker.set_error(e.to_string());
                        }
                        worker
                    })));
                }
            }
            Action::ShowEtcd => {
                self.leave_view();
                // Switch to etcd status view
                tracing::info!("Viewing etcd cluster status");

                // Create etcd component
                let mut etcd = EtcdComponent::new();

                // Set the client and refresh data
                etcd.set_context(self.cluster.current_context_name().map(str::to_string));
                etcd.set_config_path(self.config_path.clone());
                // Without a client (the cluster is reconnecting) the refresh
                // shows that instead of a blank view, and retries
                if let Some(client) = self.cluster.client() {
                    etcd.set_client(client.clone());
                }
                self.refresh_job = Some(RefreshJob::Etcd(spawn_refresh!(etcd)));

                self.etcd = Some(etcd);
                self.show_in_pane(NavMenuItem::Etcd, View::Etcd);
            }
            Action::ShowProcesses(title, nodes) => {
                self.leave_view();
                tracing::info!("ShowProcesses: '{}' ({} node(s))", title, nodes.len());

                let mut processes = ProcessesComponent::new(title, nodes);
                match self.cluster.client() {
                    Some(client) => {
                        processes.set_client(client.clone());
                        // First load in the background; the view shows
                        // "Loading processes..." until it lands
                        self.refresh_job = Some(RefreshJob::Processes(spawn_refresh!(processes)));
                    }
                    None => processes.set_error("Not connected".to_string()),
                }

                self.processes = Some(processes);
                self.show_in_pane(NavMenuItem::Processes, View::Processes);
            }
            Action::ShowNetwork(hostname, address) => {
                if self.view == View::Group {
                    self.cluster.select_node(&hostname);
                }
                self.leave_view();
                // Switch to network stats view for a node
                tracing::info!(
                    "ShowNetwork: hostname='{}', address='{}'",
                    hostname,
                    address
                );

                // Create network component
                let mut network = NetworkStatsComponent::new(hostname, address.clone());

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    // Create a client configured for this specific node
                    let node_client = client.with_node(&address);
                    tracing::info!("Created node client for network: '{}'", address);
                    network.set_client(node_client);
                    network.set_talos_context(
                        self.cluster.current_context_name().map(str::to_string),
                        self.config_path.clone(),
                    );
                    network.refresh();
                }

                self.network = Some(network);
                self.show_in_pane(NavMenuItem::Network, View::Network);
            }
            Action::ShowSecurity => {
                self.leave_view();
                // Switch to security/certificates view
                tracing::info!("Viewing security/certificates");

                // Create security component
                let mut security = SecurityComponent::new(
                    self.cluster
                        .current_context_name()
                        .unwrap_or_default()
                        .to_string(),
                    self.config_path.clone(),
                );

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    security.set_client(client.clone());
                }
                security.set_query_node(self.cluster.control_plane_ip());
                self.refresh_job = Some(RefreshJob::Security(spawn_refresh!(security)));

                self.security = Some(security);
                self.show_in_pane(NavMenuItem::Certs, View::Security);
            }
            Action::ShowLifecycle => {
                self.leave_view();
                // Switch to lifecycle/version view
                tracing::info!("Viewing lifecycle/versions");

                // Create lifecycle component
                let mut lifecycle = LifecycleComponent::new(
                    self.cluster
                        .current_context_name()
                        .unwrap_or_default()
                        .to_string(),
                    self.config_path.clone(),
                );

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    lifecycle.set_client(client.clone());
                }

                self.refresh_job = Some(RefreshJob::Lifecycle(spawn_refresh!(lifecycle)));

                self.lifecycle = Some(lifecycle);
                self.show_in_pane(NavMenuItem::Lifecycle, View::Lifecycle);
            }
            Action::ShowWorkloads(title, nodes) => {
                self.leave_view();
                tracing::info!(
                    "ShowWorkloads: '{}' ({})",
                    title,
                    nodes
                        .as_ref()
                        .map(|n| format!("{} node(s)", n.len()))
                        .unwrap_or_else(|| "whole cluster".to_string())
                );

                let mut workloads = WorkloadHealthComponent::new(title, nodes);

                // The K8s client is created by the first refresh, which runs in
                // the background; the view shows "Loading workloads..." until
                // it lands
                let cp_ip = self.cluster.control_plane_ip();
                if let Some(talos_client) = self.cluster.client() {
                    workloads.set_k8s_source(talos_client.clone(), cp_ip);
                    workloads.start_loading();
                    self.refresh_job = Some(RefreshJob::Workloads(spawn_refresh!(workloads)));
                } else {
                    workloads.set_error("Not connected".to_string());
                }

                self.workloads = Some(workloads);
                self.show_in_pane(NavMenuItem::Workloads, View::Workloads);
            }
            Action::ShowVolumes(title, nodes) => {
                self.leave_view();
                tracing::info!("ShowVolumes: '{}'", title);

                let mut volumes = VolumesComponent::new(title, nodes);
                volumes.set_context(self.cluster.current_context_name().map(str::to_string));
                if let Some(state) = self.volumes_view_state.clone() {
                    volumes.set_view_state(state);
                }
                let cp_ip = self.cluster.control_plane_ip();
                if let Some(talos_client) = self.cluster.client() {
                    volumes.set_clients(
                        talos_client.clone(),
                        self.cluster.active_node_ips(),
                        cp_ip,
                    );
                    // First scan in the background; the view shows a
                    // scanning message until it lands
                    self.refresh_job = Some(RefreshJob::Volumes(spawn_refresh!(volumes)));
                } else {
                    volumes.set_error("Not connected".to_string());
                }

                self.volumes = Some(volumes);
                self.show_in_pane(NavMenuItem::Volumes, View::Volumes);
            }
            Action::ShowGroup(kind, title, nodes, cp_endpoint) => {
                self.leave_view();
                let item = match kind {
                    GroupKind::Network => NavMenuItem::Network,
                    GroupKind::Storage => NavMenuItem::Storage,
                    GroupKind::Diagnostics => NavMenuItem::Diagnostics,
                };
                tracing::info!(
                    "ShowGroup: {:?} of '{}' ({} node(s))",
                    kind,
                    title,
                    nodes.len()
                );
                let mut group = GroupComponent::new(
                    kind,
                    title,
                    nodes,
                    self.cluster.current_context_name().map(str::to_string),
                    self.cluster.config_path().map(str::to_string),
                    cp_endpoint,
                );
                // Without a client the refresh says so (and retries)
                if let Some(client) = self.cluster.client() {
                    group.set_client(client.clone());
                }
                self.refresh_job = Some(RefreshJob::Group(spawn_refresh!(group)));
                self.group = Some(group);
                self.show_in_pane(item, View::Group);
            }
            Action::ShowStorage(hostname, address) => {
                if self.view == View::Group {
                    self.cluster.select_node(&hostname);
                }
                self.leave_view();
                // Switch to storage view for a node
                tracing::info!(
                    "ShowStorage: hostname='{}', address='{}'",
                    hostname,
                    address
                );

                // Get context and config from cluster component
                let context = self.cluster.current_context_name().map(|s| s.to_string());
                let config_path = self.cluster.config_path().map(|s| s.to_string());

                // Create storage component with context for authentication
                let mut storage =
                    StorageComponent::new(hostname, address.clone(), context, config_path);

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    // Create a client configured for this specific node
                    let node_client = client.with_node(&address);
                    storage.set_client(node_client);
                    self.refresh_job = Some(RefreshJob::Storage(spawn_refresh!(storage)));
                }

                self.storage = Some(storage);
                self.show_in_pane(NavMenuItem::Storage, View::Storage);
            }
            Action::ShowNodeInfo(hostname, address, tab) => {
                self.leave_view();
                let item = match tab {
                    NodeInfoTab::Containers => NavMenuItem::Containers,
                    NodeInfoTab::Config => NavMenuItem::Config,
                    NodeInfoTab::Extensions => NavMenuItem::Extensions,
                };
                let context = self.cluster.current_context_name().map(|s| s.to_string());
                let config_path = self.cluster.config_path().map(|s| s.to_string());
                let mut node_info =
                    NodeInfoComponent::new(hostname, address.clone(), context, config_path, tab);
                if let Some(client) = self.cluster.client() {
                    node_info.set_client(client.with_node(&address));
                    self.refresh_job = Some(RefreshJob::NodeInfo(spawn_refresh!(node_info)));
                } else {
                    node_info.set_error("No Talos client configured".to_string());
                }
                self.node_info = Some(node_info);
                self.show_in_pane(item, View::NodeInfo);
            }
            Action::ShowNodeOperations(hostname, address, is_controlplane) => {
                // Show node operations overlay
                tracing::info!("Viewing node operations for: {} ({})", hostname, address);

                // Create node operations component
                let mut node_ops = NodeOperationsComponent::new(hostname, address, is_controlplane);

                // Set the Talos client
                if let Some(talos_client) = self.cluster.client() {
                    node_ops.set_client(talos_client.clone());
                }

                // Load safety checks (in the background)
                node_ops.refresh();

                self.node_operations = Some(node_ops);
                if !matches!(self.view, View::NodeOperations | View::RollingOperations) {
                    self.overlay_return = self.view.clone();
                }
                self.view = View::NodeOperations;
            }
            Action::ShowRollingOperations(nodes) => {
                // Show rolling operations overlay
                tracing::info!("Viewing rolling operations for {} nodes", nodes.len());

                // Create rolling operations component
                let mut rolling_ops = RollingOperationsComponent::new();

                // Convert node tuples to RollingNodeInfo
                let node_infos: Vec<RollingNodeInfo> = nodes
                    .into_iter()
                    .map(|(hostname, address, is_controlplane)| RollingNodeInfo {
                        hostname,
                        address,
                        is_controlplane,
                        selection_order: None,
                    })
                    .collect();

                rolling_ops.set_nodes(node_infos);

                // Set clients
                let cp_ip = self.cluster.control_plane_ip();
                if let Some(talos_client) = self.cluster.client() {
                    rolling_ops.set_talos_client(talos_client.clone());

                    // Pin the K8s client to a control plane node of the launched
                    // context. Rolling ops cordon/drain by node name through this
                    // client — targeting the wrong cluster (an ambient KUBECONFIG)
                    // would drain the wrong nodes.
                    //
                    // Created in the background (see `poll_view_setup`).
                    let talos_client = talos_client.clone();
                    if let Some(h) = self.rolling_k8s_fetch.take() {
                        h.abort();
                    }
                    self.rolling_k8s_fetch = Some(tokio::spawn(async move {
                        let created = tokio::time::timeout(
                            Duration::from_secs(15),
                            crate::components::diagnostics::k8s::create_k8s_client_with_source(
                                &talos_client,
                                cp_ip.as_deref(),
                                None,
                            ),
                        )
                        .await;
                        match created {
                            Ok(Ok((k8s, _source))) => Some(k8s),
                            _ => None,
                        }
                    }));
                }

                self.rolling_operations = Some(rolling_ops);
                if !matches!(self.view, View::NodeOperations | View::RollingOperations) {
                    self.overlay_return = self.view.clone();
                }
                self.view = View::RollingOperations;
            }
            _ => {
                // Forward to current component
                match self.view {
                    View::Hint => {}
                    View::Cluster => {
                        if let Some(next_action) = self.cluster.update(action)? {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::MultiLogs => {
                        if let Some(multi_logs) = &mut self.multi_logs
                            && let Some(next_action) = multi_logs.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Etcd => {
                        if let Some(etcd) = &mut self.etcd
                            && let Some(next_action) = etcd.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Processes => {
                        if let Some(processes) = &mut self.processes
                            && let Some(next_action) = processes.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Network => {
                        if let Some(network) = &mut self.network
                            && let Some(next_action) = network.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Diagnostics => {
                        if let Some(diagnostics) = &mut self.diagnostics
                            && let Some(next_action) = diagnostics.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Security => {
                        if let Some(security) = &mut self.security
                            && let Some(next_action) = security.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Lifecycle => {
                        if let Some(lifecycle) = &mut self.lifecycle
                            && let Some(next_action) = lifecycle.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Volumes => {
                        if let Some(volumes) = &mut self.volumes
                            && let Some(next_action) = volumes.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Workloads => {
                        if let Some(workloads) = &mut self.workloads
                            && let Some(next_action) = workloads.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Storage => {
                        if let Some(storage) = &mut self.storage
                            && let Some(next_action) = storage.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::NodeInfo => {
                        if let Some(node_info) = &mut self.node_info
                            && let Some(next_action) = node_info.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::NodeOperations => {
                        if let Some(node_ops) = &mut self.node_operations
                            && let Some(next_action) = node_ops.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::RollingOperations => {
                        if let Some(rolling_ops) = &mut self.rolling_operations
                            && let Some(next_action) = rolling_ops.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                    View::Group => {
                        if let Some(group) = &mut self.group
                            && let Some(next_action) = group.update(action)?
                        {
                            Box::pin(self.handle_action(next_action)).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_view(view: View) -> App {
        App {
            view,
            tree_area: Rect::new(0, 2, 30, 40),
            view_area: Rect::new(30, 2, 170, 40),
            ..Default::default()
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn layout_keys_zoom_and_step_the_pane_width() {
        let mut app = app_with_view(View::Hint);
        assert!(app.layout_key(key(KeyCode::Char('Z'))));
        assert_eq!(app.pane_width, Some(PaneWidth::Hidden));
        assert!(app.layout_key(key(KeyCode::Char('Z'))));
        assert_eq!(app.pane_width, None);

        assert!(app.layout_key(key(KeyCode::Char('['))));
        assert_eq!(app.pane_width, Some(PaneWidth::Compact));
        assert!(app.layout_key(key(KeyCode::Char('['))));
        assert_eq!(app.pane_width, Some(PaneWidth::Hidden));
        assert!(app.layout_key(key(KeyCode::Char(']'))));
        assert_eq!(app.pane_width, Some(PaneWidth::Compact));
        assert!(app.layout_key(key(KeyCode::Char(']'))));
        assert_eq!(app.pane_width, Some(PaneWidth::Full));
        assert!(!app.layout_key(key(KeyCode::Char('n'))));
    }

    /// Enter in the tree gives the view the keyboard; Esc in the view hands
    /// it back without closing the view
    #[tokio::test]
    async fn focus_moves_between_tree_and_view() {
        let mut app = app_with_view(View::Hint);
        app.cluster.set_focused_pane(FocusedPane::Nodes);
        assert!(app.dispatch_key(key(KeyCode::Enter)).unwrap().is_none());
        assert!(app.view_has_focus);

        let action = app.dispatch_key(key(KeyCode::Esc)).unwrap();
        assert!(matches!(action, Some(Action::Back)));
        app.handle_action(Action::Back).await.unwrap();
        assert!(!app.view_has_focus);
        assert_eq!(app.view, View::Hint);

        // Tab from the menu goes into the view too
        app.cluster.set_focused_pane(FocusedPane::Menu);
        assert!(app.dispatch_key(key(KeyCode::Tab)).unwrap().is_none());
        assert!(app.view_has_focus);
    }

    /// Esc in the tree returns to the dashboard; a hint shows its reason
    #[tokio::test]
    async fn dashboard_and_hints() {
        let mut app = app_with_view(View::Cluster);
        app.handle_action(Action::ShowHint(
            NavMenuItem::Containers,
            "Select a node".to_string(),
        ))
        .await
        .unwrap();
        assert_eq!(app.view, View::Hint);
        assert_eq!(app.pane, NavMenuItem::Containers);
        assert_eq!(app.hint.as_deref(), Some("Select a node"));

        let action = app.dispatch_key(key(KeyCode::Esc)).unwrap();
        assert!(matches!(action, Some(Action::ShowDashboard)));
        app.handle_action(Action::ShowDashboard).await.unwrap();
        assert_eq!(app.view, View::Cluster);
        assert_eq!(app.pane, NavMenuItem::Dashboard);
        assert_eq!(app.hint, None);
    }
}
