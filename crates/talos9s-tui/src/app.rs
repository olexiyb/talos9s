//! Application state and main loop

use crate::action::Action;
use crate::audit::{audit_failure, audit_start, audit_success};
use crate::components::cluster::{ClickTarget, FocusedPane, NavMenuItem, PaneWidth, TreeSelection};
use crate::components::rolling_operations::RollingNodeInfo;
use crate::components::wizard::{WizardComponent, WizardState};
use crate::components::{
    ClusterComponent, Component, DiagnosticsComponent, EtcdComponent, GroupComponent,
    LifecycleComponent, MultiLogsComponent, NetworkStatsComponent, NodeInfoComponent,
    NodeOperationsComponent, ProcessesComponent, RollingOperationsComponent, SecurityComponent,
    StorageComponent, VolumesComponent, WorkloadHealthComponent,
};
use crate::components::{GroupKind, NodeInfoTab, StorageViewMode};
use crate::tui::{self, Tui};
use color_eyre::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use view::{View, ViewComponent, ViewSettings};

mod view;

/// Whether a key event is Ctrl+C
fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
}

/// Two clicks on the same cell within this long open the row, like Enter
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// How long the selection must rest before the view follows it, so moving
/// through ten nodes loads one; logs restart streams, so they wait longer
const FOLLOW_DELAY: Duration = Duration::from_millis(300);
const FOLLOW_DELAY_LOGS: Duration = Duration::from_millis(500);
/// Views kept for coming back to them
const VIEW_CACHE_SIZE: usize = 4;

/// How long the wizard waits for `talosctl apply-config` / `bootstrap`
const APPLY_TIMEOUT: Duration = Duration::from_secs(60);
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// Nodes of the KubeSpan matrix (`View::KubeSpanMatrix`)
    kubespan_nodes: Vec<String>,
    /// Last left click in the view, for double-clicks
    last_click: Option<(Instant, u16, u16)>,
    /// Title of the view's window: the view and what it shows
    pane_title: String,
    /// A node view opened from a group view (Enter in its table): the group
    /// row to reselect on Esc, so the group view comes back
    drill_return: Option<(TreeSelection, String)>,
    /// View under an operations overlay, shown again when it closes
    overlay_return: View,
    /// Nodes pane and view areas of the last frame, for the mouse
    tree_area: Rect,
    view_area: Rect,
    /// Recently replaced views by (menu entry, selection), oldest first
    view_cache: Vec<(String, ViewComponent)>,
    /// The key being handled opens a view from the menu or services list,
    /// which then takes the keyboard
    focus_after_show: bool,
    /// Cluster component: the tree, the dashboard and the frame
    cluster: ClusterComponent,
    /// Component of the view on the right (`None` for the dashboard and hints)
    component: Option<ViewComponent>,
    /// View settings that carry over to the next visit (and the next run)
    settings: ViewSettings,
    /// Where the state between runs lives, and what was last written there
    state_path: Option<std::path::PathBuf>,
    saved_state: crate::state::UiState,
    /// The new-release check: off by flag, its result, and the request
    /// while it runs
    update_check_enabled: bool,
    update: crate::state::UpdateCheck,
    update_job: Option<tokio::task::JoinHandle<Result<String, String>>>,
    /// Node operations component (overlay for node operations)
    node_operations: Option<NodeOperationsComponent>,
    /// Rolling operations component (overlay for multi-node operations)
    rolling_operations: Option<RollingOperationsComponent>,
    /// Number of log lines to fetch per service
    tail_lines: i32,
    /// Tick rate for animations (ms)
    tick_rate: Duration,
    /// Custom config file path (from --config flag)
    config_path: Option<String>,
    /// In-flight background refresh of the view's component, on a clone
    /// (`ViewComponent::spawn_refresh`)
    refresh_job: Option<JoinHandle<ViewComponent>>,
    /// In-flight Kubernetes client creation for rolling operations
    rolling_k8s_fetch: Option<JoinHandle<Option<kube::Client>>>,
    /// Whether running in insecure mode (no TLS)
    insecure: bool,
    /// Endpoint for insecure mode
    insecure_endpoint: Option<String>,
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
        // Operations log to ~/.talos9s/audit.log; the node of each entry
        // tells the cluster apart when no context is given
        let audit_scope = match (&context, insecure) {
            (Some(ctx), _) => ctx.clone(),
            (None, true) => "maintenance".to_string(),
            (None, false) => "-".to_string(),
        };
        crate::audit::init_audit_logger(&audit_scope);

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
            kubespan_nodes: Vec::new(),
            last_click: None,
            pane_title: String::new(),
            drill_return: None,
            overlay_return: View::Cluster,
            tree_area: Rect::default(),
            view_area: Rect::default(),
            view_cache: Vec::new(),
            focus_after_show: false,
            cluster: ClusterComponent::new(config_path.clone(), context),
            component: None,
            settings: ViewSettings::default(),
            state_path: None,
            saved_state: Default::default(),
            update_check_enabled: true,
            update: Default::default(),
            update_job: None,
            node_operations: None,
            rolling_operations: None,
            tail_lines,
            tick_rate: Duration::from_millis(100),
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
        tui::exit_on_signal();

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
            .ok_or_else(|| color_eyre::eyre::eyre!("insecure mode needs --endpoint"))?;

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
                wizard.set_error(format!("Failed to generate config: {e}"));
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
            let disk = wizard
                .data()
                .selected_disk
                .as_ref()
                .map_or_else(|| "-".to_string(), |d| d.dev_path.clone());
            audit_start(
                "INSTALL",
                &endpoint,
                &format!("cluster {cluster_name}, disk {disk}, config {path}"),
            );
            let applied =
                tokio::time::timeout(APPLY_TIMEOUT, apply_config_insecure(&endpoint, &path))
                    .await
                    .unwrap_or_else(|_| {
                        Err(talos_rs::TalosError::Connection(format!(
                            "timed out after {}s",
                            APPLY_TIMEOUT.as_secs()
                        )))
                    });
            match applied {
                Ok(result) => {
                    if result.success {
                        audit_success("INSTALL", &endpoint, "config applied, node installing");
                        // Start waiting for reboot
                        wizard.data_mut().wait_started = Some(Instant::now());
                        wizard.data_mut().context_name = Some(cluster_name);
                        wizard.transition(WizardState::WaitingReboot);
                    } else {
                        audit_failure("INSTALL", &endpoint, &result.message);
                        wizard.set_error(result.message);
                    }
                }
                Err(e) => {
                    let msg = format!("Failed to apply config: {e}");
                    audit_failure("INSTALL", &endpoint, &msg);
                    wizard.set_error(msg);
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
            let endpoint = wizard.data().endpoint.clone();
            audit_start("BOOTSTRAP", &endpoint, &format!("context {ctx}"));
            let output = tokio::time::timeout(
                BOOTSTRAP_TIMEOUT,
                Command::new("talosctl")
                    .args(["--context", &ctx, "bootstrap"])
                    .kill_on_drop(true)
                    .output(),
            )
            .await;

            let failure = match output {
                Ok(Ok(out)) if out.status.success() => {
                    audit_success("BOOTSTRAP", &endpoint, "etcd bootstrapped");
                    wizard.data_mut().wait_started = Some(Instant::now());
                    wizard.transition(WizardState::WaitingHealthy);
                    None
                }
                Ok(Ok(out)) => Some(format!(
                    "Bootstrap failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                )),
                Ok(Err(e)) => Some(format!("Failed to run bootstrap: {e}")),
                Err(_) => Some(format!(
                    "Bootstrap timed out after {}s",
                    BOOTSTRAP_TIMEOUT.as_secs()
                )),
            };
            if let Some(msg) = failure {
                audit_failure("BOOTSTRAP", &endpoint, &msg);
                wizard.set_error(msg);
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

    /// Turn the daily new-release check off (`--no-update-check`)
    pub fn disable_update_check(&mut self) {
        self.update_check_enabled = false;
    }

    /// Ask GitHub for the latest release when the last answer is a day old
    fn start_update_check(&mut self) {
        if !self.update_check_enabled
            || crate::update::do_not_track()
            || !self.update.due(unix_now(), crate::update::CHECK_INTERVAL)
        {
            return;
        }
        self.update_job = Some(tokio::spawn(crate::update::fetch_latest()));
    }

    /// Take the check's answer when it is in; a failure is only logged and
    /// tried again next start
    fn poll_update_check(&mut self) {
        if !self
            .update_job
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            return;
        }
        let Some(job) = self.update_job.take() else {
            return;
        };
        match crate::task::poll_finished(job) {
            Some(Ok(Ok(latest))) => {
                tracing::info!("Latest talos9s release: {latest}");
                self.update = crate::state::UpdateCheck {
                    checked_at: Some(unix_now()),
                    latest: Some(latest),
                };
                self.cluster
                    .set_newer_version(self.update.newer().map(str::to_string));
            }
            Some(Ok(Err(e))) => tracing::debug!("New-release check failed: {e}"),
            _ => {}
        }
    }

    /// Apply what the last run left in the state file; the menu entry it
    /// showed, for the caller to open
    fn restore_state(&mut self) -> Option<NavMenuItem> {
        use crate::components::{network, volumes};
        use crate::state::{LOG_LEVELS, by_name};
        let state = crate::state::load(self.state_path.as_deref()?);
        self.cluster.apply_saved(&state);
        self.pane_width = by_name(state.pane_width.as_deref());
        if let Some(levels) = state.log_levels.as_ref().filter(|l| !l.is_empty()) {
            self.settings.log_levels = LOG_LEVELS.map(|l| levels.iter().any(|x| x == l));
        }
        if state.volumes.show_all.is_some() || state.volumes.type_filter.is_some() {
            self.settings.volumes = Some(volumes::VolumesViewState {
                show_all: state.volumes.show_all.unwrap_or(true),
                type_filter: by_name(state.volumes.type_filter.as_deref()).unwrap_or_default(),
                ..Default::default()
            });
        }
        self.settings.processes_sort = by_name(state.processes_sort.as_deref()).unwrap_or_default();
        self.settings.network = network::NetworkSettings {
            sort_by: by_name(state.network.sort.as_deref()).unwrap_or_default(),
            conn_sort_by: by_name(state.network.connections_sort.as_deref()).unwrap_or_default(),
            listening_only: state.network.listening_only.unwrap_or(false),
        };
        let item = by_name(state.view.as_deref());
        self.update = state.update.clone();
        self.cluster
            .set_newer_version(self.update.newer().map(str::to_string));
        self.saved_state = state;
        item
    }

    /// What to remember now: the open views' own settings when they are
    /// shown, otherwise the ones kept from their last visit
    fn current_state(&self) -> crate::state::UiState {
        use crate::state::{LOG_LEVELS, name_of};
        let mut tree = self.saved_state.tree.clone();
        for folds in self.cluster.saved_folds() {
            tree.retain(|t| t.cluster != folds.cluster);
            tree.push(folds);
        }
        tree.sort_by(|a, b| a.cluster.cmp(&b.cluster));
        let mut settings = self.settings.clone();
        if let Some(c) = &self.component {
            c.carry_over(&mut settings);
        }
        let ViewSettings {
            log_levels: levels,
            volumes,
            processes_sort,
            network,
        } = settings;
        crate::state::UiState {
            // Nothing listed yet (no talosconfig): keep the remembered one
            selection: self
                .cluster
                .saved_selection()
                .or_else(|| self.saved_state.selection.clone()),
            view: Some(name_of(self.pane)),
            focus: Some(if self.view_has_focus && self.view != View::Cluster {
                "view".to_string()
            } else {
                name_of(self.cluster.focused_pane())
            }),
            pane_width: self.pane_width.map(name_of),
            tree,
            auto_refresh: Some(self.cluster.auto_refresh()),
            log_levels: Some(
                LOG_LEVELS
                    .iter()
                    .zip(levels)
                    .filter(|(_, shown)| *shown)
                    .map(|(name, _)| name.to_string())
                    .collect(),
            ),
            volumes: crate::state::VolumesSettings {
                show_all: volumes.as_ref().map(|v| v.show_all),
                type_filter: volumes.map(|v| name_of(v.type_filter)),
            },
            processes_sort: Some(name_of(processes_sort)),
            network: crate::state::NetworkSettings {
                sort: Some(name_of(network.sort_by)),
                connections_sort: Some(name_of(network.conn_sort_by)),
                listening_only: Some(network.listening_only),
            },
            update: self.update.clone(),
        }
    }

    /// Write the state file when something worth remembering changed
    fn persist_state(&mut self) {
        let Some(path) = self.state_path.clone() else {
            return;
        };
        let state = self.current_state();
        if state == self.saved_state {
            return;
        }
        if let Err(e) = crate::state::save(&path, &state) {
            tracing::warn!("Could not write {}: {}", path.display(), e);
        }
        // Also on failure: retrying every frame would only repeat the warning
        self.saved_state = state;
    }

    /// Main event loop
    async fn main_loop(&mut self, terminal: &mut Tui) -> Result<()> {
        // Load the cluster list from talosconfig. This does no network I/O:
        // each cluster connects in the background and fills in as it becomes
        // reachable, so the UI is usable at once even if some are down.
        self.cluster.connect().await?;
        // Back where the last run left off; a view of a node waits (with a
        // loading note) until the cluster lists its nodes
        self.state_path = crate::state::default_path();
        if let Some(item) = self.restore_state() {
            let focus = self.saved_state.focus.clone();
            // The keyboard goes back where it was: the view once it shows,
            // or the tree's side
            match crate::state::by_name(focus.as_deref()) {
                Some(pane) => self.cluster.set_focused_pane(pane),
                None => self.focus_after_show = focus.as_deref() == Some("view"),
            }
            if item != NavMenuItem::Dashboard
                && let Some(action) = self.cluster.action_for(item)
            {
                self.handle_action(action).await?;
            }
            self.focus_after_show = false;
        }
        self.start_update_check();

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
            self.poll_update_check();
            self.persist_state();

            // Check async results (non-blocking)
            self.poll_refresh_job();
            self.poll_view_setup();
            self.follow_selection().await?;

            // Check if we should quit
            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// Hand a finished background refresh back to the view's component
    fn poll_refresh_job(&mut self) {
        if !self
            .refresh_job
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            return;
        }
        let Some(job) = self.refresh_job.take() else {
            return;
        };
        let Some(component) = &mut self.component else {
            return;
        };
        // A panicked task is shown as the view's error: that ends its loading
        // state, so auto-refresh retries instead of "Loading..." forever.
        // (Aborted jobs never get here: cancelling takes them out first.)
        match crate::task::poll_finished(job) {
            Some(Ok(refreshed)) => component.adopt_refresh(refreshed),
            Some(Err(e)) => {
                tracing::error!("View refresh task failed: {}", e);
                component.set_error(format!("Refresh failed: {e}"));
            }
            None => {}
        }
        self.publish_diagnostics();
    }

    /// Give the dashboard what the last diagnostics run found per node
    fn publish_diagnostics(&mut self) {
        match &self.component {
            Some(ViewComponent::Diagnostics(d)) => {
                if let Some((node, summary)) = d.summary() {
                    self.cluster.set_diagnostics_summary(node, summary);
                }
            }
            Some(ViewComponent::Group(group)) => {
                for (node, summary) in group.diagnostics_summaries() {
                    self.cluster.set_diagnostics_summary(node, summary);
                }
            }
            _ => {}
        }
    }

    /// Finish view setup that ran in the background: the Kubernetes client
    /// of rolling operations
    fn poll_view_setup(&mut self) {
        if self
            .rolling_k8s_fetch
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
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
        // Where each key goes, for the debug log (--debug): tree or view,
        // and which view
        tracing::debug!(
            "key {:?} {:?} -> {} ({:?}, tree pane {:?}, captures text {})",
            key.code,
            key.modifiers,
            if self.view == View::Cluster || !self.view_has_focus {
                "tree"
            } else {
                "view"
            },
            self.view,
            self.cluster.focused_pane(),
            self.view_captures_text()
        );
        if self.view.is_overlay() {
            return self.dispatch_view_key(key);
        }
        if !self.view_captures_text() && self.layout_key(key) {
            return Ok(None);
        }
        if self.view == View::Cluster || !self.view_has_focus {
            return self.dispatch_tree_key(key);
        }
        // Left returns to the tree unless the view is using it (a scrolled
        // config, a Yes/No dialog); Right is the view's
        if key.code == KeyCode::Left && !self.view_uses_horizontal_keys() {
            self.view_has_focus = false;
            return Ok(None);
        }
        // Tab is the next window (Nodes → menu → view → Nodes) unless the
        // view has a tab bar of its own
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && !self.view_uses_tab_key() {
            self.view_has_focus = false;
            self.cluster.set_focused_pane(if key.code == KeyCode::Tab {
                FocusedPane::Nodes
            } else {
                FocusedPane::Menu
            });
            return Ok(None);
        }
        // q in a view means back, like Esc: some terminals (IDEA's) never
        // deliver Esc. q in the tree quits.
        self.dispatch_view_key(key)
    }

    /// The view's component, unless an overlay or the dashboard is shown
    fn shown(&self) -> Option<&dyn Component> {
        match self.view {
            View::Cluster | View::Hint | View::KubeSpanMatrix => None,
            _ if self.view.is_overlay() => None,
            _ => self.component.as_ref().map(ViewComponent::component),
        }
    }

    /// The component that gets the view's keys and actions: the overlay,
    /// the dashboard, or the view's component
    fn active_mut(&mut self) -> Option<&mut dyn Component> {
        match self.view {
            View::Cluster => Some(&mut self.cluster),
            View::Hint | View::KubeSpanMatrix => None,
            View::NodeOperations => self
                .node_operations
                .as_mut()
                .map(|c| c as &mut dyn Component),
            View::RollingOperations => self
                .rolling_operations
                .as_mut()
                .map(|c| c as &mut dyn Component),
            _ => self.component.as_mut().map(ViewComponent::component_mut),
        }
    }

    fn view_uses_tab_key(&self) -> bool {
        self.shown()
            .is_some_and(super::components::Component::uses_tab_key)
    }

    fn view_uses_horizontal_keys(&self) -> bool {
        self.shown()
            .is_some_and(super::components::Component::uses_horizontal_keys)
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
        self.shown()
            .is_some_and(super::components::Component::captures_text)
    }

    /// Keys for the tree and menu. Where the dashboard has its services list,
    /// Tab / Shift+Tab move into the view instead.
    fn dispatch_tree_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        let before = self.cluster.selection();
        let action = self.dispatch_tree_key_inner(key);
        self.cluster.selection_moved(&before);
        action
    }

    fn dispatch_tree_key_inner(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        let pane = self.cluster.focused_pane();
        // Esc or Left in the dashboard's services list hands the keyboard back
        // to the tree side it came from, as in any other view
        if self.view == View::Cluster
            && pane == FocusedPane::Services
            && matches!(key.code, KeyCode::Esc | KeyCode::Left)
        {
            self.cluster.leave_services();
            return Ok(None);
        }
        // Right from the tree goes into the view (the dashboard's services list)
        let into_view = match key.code {
            KeyCode::Right => pane != FocusedPane::Services,
            // Enter on any tree row (group rows too: Space folds them)
            KeyCode::Tab => self.view != View::Cluster && pane == FocusedPane::Menu,
            KeyCode::BackTab | KeyCode::Enter => {
                self.view != View::Cluster && pane == FocusedPane::Nodes
            }
            _ => false,
        };
        if into_view {
            self.focus_view();
            return Ok(None);
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
            // The tree keeps its side (Nodes or menu) for Left / Esc to return to
            self.view_has_focus = true;
        }
    }

    /// Route a key press to the component on the right (or the overlay)
    fn dispatch_view_key(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if matches!(self.view, View::Hint | View::KubeSpanMatrix) {
            return Ok(match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Some(Action::Back),
                _ => None,
            });
        }
        match self.active_mut() {
            Some(c) => c.handle_key_event(key),
            None => Ok(None),
        }
    }

    /// Route a mouse event to the pane under the pointer; a click also gives
    /// that pane the keyboard. The tree, the dashboard and the node info view
    /// handle the mouse themselves; other views get the wheel as Up/Down.
    fn dispatch_mouse(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        let overlay = self.view.is_overlay();
        let at = |r: Rect| {
            mouse.column >= r.x
                && mouse.column < r.x + r.width
                && mouse.row >= r.y
                && mouse.row < r.y + r.height
        };
        let click = matches!(mouse.kind, MouseEventKind::Down(_));
        if !overlay && (self.view == View::Cluster || at(self.tree_area)) {
            // Clicking a node or group row keeps the keyboard where it was
            // (the view follows the selection); a menu entry opens its view
            // with the keyboard in it, like Enter; empty space in the pane
            // hands the keyboard to the tree
            if click {
                match self.cluster.click_target(mouse.column, mouse.row) {
                    ClickTarget::Menu => self.focus_after_show = true,
                    // The dashboard's services focus is the cluster's own
                    ClickTarget::Row => {}
                    ClickTarget::None => self.view_has_focus = false,
                }
            }
            let before = self.cluster.selection();
            let action = self.cluster.handle_mouse_event(mouse);
            self.cluster.selection_moved(&before);
            if !matches!(action, Ok(Some(_))) {
                self.focus_after_show = false;
            }
            return action;
        }
        if !overlay && !at(self.view_area) {
            return Ok(None);
        }
        if click && !overlay {
            self.focus_view();
        }
        if self.view == View::NodeInfo
            && let Some(ViewComponent::NodeInfo(node_info)) = &mut self.component
        {
            return node_info.handle_mouse_event(mouse);
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) && !overlay {
            // The click selects the row under it; a second click on the same
            // cell opens it, like Enter
            self.view_mouse(mouse)?;
            if self.is_double_click(mouse.column, mouse.row) {
                return self.dispatch_view_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            }
            return Ok(None);
        }
        let code = match mouse.kind {
            MouseEventKind::ScrollUp => KeyCode::Up,
            MouseEventKind::ScrollDown => KeyCode::Down,
            _ => return Ok(None),
        };
        self.dispatch_view_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Hand a click to the open view (rows, column headers); views without
    /// a handler ignore it
    fn view_mouse(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        if self.shown().is_none() {
            return Ok(None);
        }
        match self.active_mut() {
            Some(c) => c.handle_mouse_event(mouse),
            None => Ok(None),
        }
    }

    /// The keys the view on the right acts on right now, for the bottom line
    fn view_key_hints(&self) -> Option<Line<'static>> {
        self.shown()
            .and_then(super::components::Component::key_hints)
    }

    /// Whether a left click at (column, row) is the second of a double-click
    fn is_double_click(&mut self, column: u16, row: u16) -> bool {
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(at, c, r)| c == column && r == row && now - at <= DOUBLE_CLICK);
        self.last_click = if double {
            None
        } else {
            Some((now, column, row))
        };
        double
    }

    /// Header, nodes pane and key line from the cluster component, the
    /// current view on the right, and an operations overlay on top
    fn draw_workspace(&mut self, frame: &mut Frame, area: Rect) {
        let overlay = self.view.is_overlay();
        let base = if overlay {
            self.overlay_return.clone()
        } else {
            self.view.clone()
        };
        let dashboard = base == View::Cluster;
        let view_focused = self.view_has_focus && !dashboard && !overlay;
        let hints = if view_focused {
            self.view_key_hints()
        } else {
            None
        };
        let (tree, view_area) =
            self.cluster
                .draw_frame(frame, area, self.pane_width, view_focused, dashboard, hints);
        self.tree_area = tree;
        self.view_area = view_area;
        // Every view sits in a window like the dashboard's: titled with what
        // it shows, cyan while it has the keyboard
        let right = if dashboard {
            view_area
        } else {
            let accent = if view_focused {
                Color::Cyan
            } else {
                Color::DarkGray
            };
            // The title says when a refresh is in flight (streams aside)
            let refreshing = self.refresh_job.is_some()
                || matches!(&self.component, Some(ViewComponent::Network(n)) if n.refreshing());
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent))
                .title(crate::ui_ext::title_with_refresh(
                    &self.pane_title,
                    refreshing,
                    if view_focused {
                        Color::Cyan
                    } else {
                        Color::White
                    },
                ));
            let inner = block.inner(view_area);
            frame.render_widget(block, view_area);
            inner
        };
        match base {
            View::Cluster => self.cluster.draw_details_pane(frame, right),
            View::Hint => {
                let text = Paragraph::new(format!("\n  {}", self.hint.as_deref().unwrap_or("")))
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(text, right);
            }
            View::KubeSpanMatrix => {
                self.cluster
                    .draw_kubespan_matrix_pane(frame, right, &self.kubespan_nodes);
            }
            _ => {
                if let Some(c) = &mut self.component {
                    let _ = c.component_mut().draw(frame, right);
                }
            }
        }
        if overlay && let Some(c) = self.active_mut() {
            let _ = c.draw(frame, area);
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
        self.drill_return = None;
        self.cluster.set_back_hint(None);
        self.cluster.set_active_menu(item);
        let cluster_wide = matches!(
            item,
            NavMenuItem::Etcd | NavMenuItem::Certs | NavMenuItem::Lifecycle
        );
        self.pane_title = format!(
            "{} · {}",
            item.label(),
            self.cluster.target_title(cluster_wide)
        );
        self.applied_target = self.scope_key(item);
        self.pending_target = None;
        if self.view == View::Cluster {
            // Opened from the menu: the dashboard's view is its services list
            self.view_has_focus = false;
            if std::mem::take(&mut self.focus_after_show) {
                self.cluster.set_focused_pane(FocusedPane::Services);
            }
        } else {
            if self.cluster.focused_pane() == FocusedPane::Services {
                self.cluster.leave_services();
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
        let Some(component) = self.component.take() else {
            return;
        };
        component.carry_over(&mut self.settings);
        match component {
            // Streams don't survive being put aside: logs start over
            ViewComponent::MultiLogs(mut logs) => logs.stop_streaming(),
            cached => cache_insert(&mut self.view_cache, key, cached),
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
            // A view picked while the nodes were loading opens once they
            // are known; the selection hasn't changed, so nothing else would
            if self.view == View::Hint
                && self.hint.as_deref() == Some(crate::components::cluster::LOADING_NODES_HINT)
                && !matches!(
                    self.cluster.action_for(self.pane),
                    None | Some(Action::ShowHint(..))
                )
            {
                return self.retarget().await;
            }
            return Ok(());
        }
        let delay = if self.pane == NavMenuItem::Logs {
            FOLLOW_DELAY_LOGS
        } else {
            FOLLOW_DELAY
        };
        let (pending, due) = follow_step(self.pending_target.take(), key, Instant::now(), delay);
        self.pending_target = pending;
        if due { self.retarget().await } else { Ok(()) }
    }

    /// Show the current menu view for the selection: from the cache when it
    /// was shown recently (then refreshed), otherwise built anew
    async fn retarget(&mut self) -> Result<()> {
        // The node view keeps the tab the user switched to
        let item = match &self.component {
            Some(ViewComponent::NodeInfo(n)) if self.view == View::NodeInfo => match n.tab() {
                NodeInfoTab::Containers => NavMenuItem::Containers,
                NodeInfoTab::Config => NavMenuItem::Config,
                NodeInfoTab::Extensions => NavMenuItem::Extensions,
            },
            Some(ViewComponent::Storage(st)) if self.view == View::Storage => {
                match st.view_mode() {
                    StorageViewMode::Partitions => NavMenuItem::Partitions,
                    StorageViewMode::Disks => NavMenuItem::Disks,
                }
            }
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
    fn restore_view(&mut self, item: NavMenuItem, cached: ViewComponent) {
        let view = cached.view();
        self.component = Some(cached);
        self.show_in_pane(item, view);
    }

    /// Show `component` as menu entry `item`, starting its first refresh in
    /// the background when `load`
    fn show_component(&mut self, item: NavMenuItem, component: ViewComponent, load: bool) {
        if load {
            self.refresh_job = component.spawn_refresh();
        }
        let view = component.view();
        self.component = Some(component);
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
                // Back to the group view a node was opened from, else the
                // view hands the keyboard back to the tree and stays shown
                _ => {
                    if let Some((selection, _)) = self.drill_return.take() {
                        self.cluster.restore_selection(selection);
                        self.cluster.set_back_hint(None);
                        return Box::pin(self.retarget()).await;
                    }
                    self.view_has_focus = false;
                }
            },
            Action::ShowKubeSpan(hostname, address) => {
                self.leave_view();
                let mut network = NetworkStatsComponent::new_kubespan(hostname, address.clone());
                network.set_settings(self.settings.network);
                if let Some(client) = self.cluster.client() {
                    network.set_client(client.with_node(&address));
                    network.set_talos_context(
                        self.cluster.current_context_name().map(str::to_string),
                        self.config_path.clone(),
                    );
                    network.refresh();
                }
                self.show_component(
                    NavMenuItem::KubeSpan,
                    ViewComponent::Network(Box::new(network)),
                    false,
                );
            }
            Action::ShowKubeSpanMatrix(nodes) => {
                self.leave_view();
                self.kubespan_nodes = nodes;
                self.show_in_pane(NavMenuItem::KubeSpan, View::KubeSpanMatrix);
            }
            Action::ShowDashboard => {
                if self.view != View::Cluster {
                    self.leave_view();
                }
                // Also when already shown: chosen from the menu, its services
                // list takes the keyboard
                self.show_in_pane(NavMenuItem::Dashboard, View::Cluster);
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
                // The cluster ticked above
                if self.view != View::Cluster
                    && let Some(c) = self.active_mut()
                    && let Some(next_action) = c.update(Action::Tick)?
                {
                    Box::pin(self.handle_action(next_action)).await?;
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
                match (&self.view, &mut self.component) {
                    (View::Cluster, _) => self.cluster.refresh().await?,
                    (View::NodeOperations, _) => {
                        if let Some(node_ops) = &mut self.node_operations {
                            node_ops.refresh();
                        }
                    }
                    // Nothing of their own to refresh
                    (View::RollingOperations | View::Hint | View::KubeSpanMatrix, _) => {}
                    // Starts a pending restart / file view / capture and a
                    // stats fetch in the background
                    (_, Some(ViewComponent::Network(network))) => network.refresh(),
                    // Slow refreshes (Kubernetes API, talosctl, several
                    // nodes) run on a clone off the event loop, one at a time
                    (_, Some(component)) if self.refresh_job.is_none() => {
                        match component {
                            // The cluster's current connection: one it
                            // redialled replaces a dead one, and one opened
                            // while it was reconnecting gets one at all
                            ViewComponent::Etcd(etcd) => {
                                if let Some(client) = self.cluster.client() {
                                    etcd.set_client(client.clone());
                                }
                            }
                            ViewComponent::Group(group) => {
                                if let Some(client) = self.cluster.client() {
                                    group.set_client(client.clone());
                                }
                            }
                            ViewComponent::Processes(processes) => {
                                let cluster = &self.cluster;
                                processes.set_node_basics(|n| cluster.node_basics(n));
                            }
                            _ => {}
                        }
                        self.refresh_job = component.spawn_refresh();
                    }
                    _ => {}
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
                multi_logs.set_level_filter(self.settings.log_levels);

                // Each stream targets its node through the cluster's client and
                // starts with the last `tail_lines` lines, so there is no
                // separate first fetch
                if let Some(client) = self.cluster.client() {
                    multi_logs.set_client(client.clone(), self.tail_lines);
                    multi_logs.start_streaming();
                } else {
                    multi_logs.set_error("Not connected to the cluster".to_string());
                }

                self.show_component(
                    NavMenuItem::Logs,
                    ViewComponent::MultiLogs(Box::new(multi_logs)),
                    false,
                );
            }
            Action::ShowDiagnostics(hostname, address, role, cp_endpoint) => {
                // A node opened from a group view: the tree follows
                let from_group = (self.view == View::Group).then(|| {
                    let group = (self.cluster.selection(), self.cluster.target_title(false));
                    self.cluster.select_node(&hostname);
                    group
                });
                self.leave_view();
                let mut load = false;
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
                diagnostics.set_talos_context(
                    self.cluster
                        .current_context_name()
                        .map(std::string::ToString::to_string),
                );

                // Set the client and refresh data
                if let Some(client) = self.cluster.client() {
                    // Create a client configured for this specific node
                    let node_client = client.with_node(&address);
                    diagnostics.set_client(node_client);
                    load = true;
                }

                self.show_component(
                    NavMenuItem::Diagnostics,
                    ViewComponent::Diagnostics(Box::new(diagnostics)),
                    load,
                );
                if let Some((selection, title)) = from_group {
                    self.cluster.set_back_hint(Some(title.clone()));
                    self.drill_return = Some((selection, title));
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

                self.show_component(NavMenuItem::Etcd, ViewComponent::Etcd(Box::new(etcd)), true);
            }
            Action::ShowProcesses(title, nodes) => {
                self.leave_view();
                let mut load = false;
                tracing::info!("ShowProcesses: '{}' ({} node(s))", title, nodes.len());

                let mut processes = ProcessesComponent::new(title, nodes);
                processes.set_sort_by(self.settings.processes_sort);
                processes.set_node_basics(|n| self.cluster.node_basics(n));
                match self.cluster.client() {
                    Some(client) => {
                        processes.set_client(client.clone());
                        // First load in the background; the view shows
                        // "Loading processes..." until it lands
                        load = true;
                    }
                    None => processes.set_error("Not connected".to_string()),
                }

                self.show_component(
                    NavMenuItem::Processes,
                    ViewComponent::Processes(Box::new(processes)),
                    load,
                );
            }
            Action::ShowNetwork(hostname, address) => {
                let from_group = (self.view == View::Group).then(|| {
                    let group = (self.cluster.selection(), self.cluster.target_title(false));
                    self.cluster.select_node(&hostname);
                    group
                });
                self.leave_view();
                // Switch to network stats view for a node
                tracing::info!(
                    "ShowNetwork: hostname='{}', address='{}'",
                    hostname,
                    address
                );

                // Create network component
                let mut network = NetworkStatsComponent::new(hostname.clone(), address.clone());
                network.set_settings(self.settings.network);
                network.set_controlplane(self.cluster.is_controlplane(&hostname));

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

                self.show_component(
                    NavMenuItem::Network,
                    ViewComponent::Network(Box::new(network)),
                    false,
                );
                if let Some((selection, title)) = from_group {
                    self.cluster.set_back_hint(Some(title.clone()));
                    self.drill_return = Some((selection, title));
                }
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

                self.show_component(
                    NavMenuItem::Certs,
                    ViewComponent::Security(Box::new(security)),
                    true,
                );
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

                self.show_component(
                    NavMenuItem::Lifecycle,
                    ViewComponent::Lifecycle(Box::new(lifecycle)),
                    true,
                );
            }
            Action::ShowWorkloads(title, nodes) => {
                self.leave_view();
                let mut load = false;
                tracing::info!(
                    "ShowWorkloads: '{}' ({})",
                    title,
                    nodes
                        .as_ref()
                        .map(|n| format!("{} node(s)", n.len()))
                        .unwrap_or_else(|| "whole cluster".to_string())
                );

                let mut workloads = WorkloadHealthComponent::new(nodes);

                // The K8s client is created by the first refresh, which runs in
                // the background; the view shows "Loading workloads..." until
                // it lands
                let cp_ip = self.cluster.control_plane_ip();
                if let Some(talos_client) = self.cluster.client() {
                    workloads.set_k8s_source(talos_client.clone(), cp_ip);
                    workloads.start_loading();
                    load = true;
                } else {
                    workloads.set_error("Not connected".to_string());
                }

                self.show_component(
                    NavMenuItem::Workloads,
                    ViewComponent::Workloads(Box::new(workloads)),
                    load,
                );
            }
            Action::ShowVolumes(title, nodes) => {
                self.leave_view();
                let mut load = false;
                tracing::info!("ShowVolumes: '{}'", title);

                let mut volumes = VolumesComponent::new(nodes);
                volumes.set_context(self.cluster.current_context_name().map(str::to_string));
                if let Some(state) = self.settings.volumes.clone() {
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
                    load = true;
                } else {
                    volumes.set_error("Not connected".to_string());
                }

                self.show_component(
                    NavMenuItem::Volumes,
                    ViewComponent::Volumes(Box::new(volumes)),
                    load,
                );
            }
            Action::ShowGroup(kind, title, nodes, cp_endpoint) => {
                self.leave_view();
                let item = match kind {
                    GroupKind::Network => NavMenuItem::Network,
                    GroupKind::Storage => NavMenuItem::Disks,
                    GroupKind::Partitions => NavMenuItem::Partitions,
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
                self.show_component(item, ViewComponent::Group(Box::new(group)), true);
            }
            Action::ShowStorage(hostname, address, mode) => {
                let from_group = (self.view == View::Group).then(|| {
                    let group = (self.cluster.selection(), self.cluster.target_title(false));
                    self.cluster.select_node(&hostname);
                    group
                });
                self.leave_view();
                // Switch to storage view for a node
                tracing::info!(
                    "ShowStorage: hostname='{}', address='{}'",
                    hostname,
                    address
                );

                // Get context and config from cluster component
                let context = self
                    .cluster
                    .current_context_name()
                    .map(std::string::ToString::to_string);
                let config_path = self
                    .cluster
                    .config_path()
                    .map(std::string::ToString::to_string);

                // Create storage component with context for authentication
                let mut storage = StorageComponent::new(hostname, address, context, config_path);
                storage.set_view_mode(mode);

                let item = match mode {
                    StorageViewMode::Disks => NavMenuItem::Disks,
                    StorageViewMode::Partitions => NavMenuItem::Partitions,
                };
                // Storage reads through talosctl, with the context
                let load = self.cluster.client().is_some();
                self.show_component(item, ViewComponent::Storage(Box::new(storage)), load);
                if let Some((selection, title)) = from_group {
                    self.cluster.set_back_hint(Some(title.clone()));
                    self.drill_return = Some((selection, title));
                }
            }
            Action::ShowNodeInfo(hostname, address, tab) => {
                self.leave_view();
                let mut load = false;
                let item = match tab {
                    NodeInfoTab::Containers => NavMenuItem::Containers,
                    NodeInfoTab::Config => NavMenuItem::Config,
                    NodeInfoTab::Extensions => NavMenuItem::Extensions,
                };
                let context = self
                    .cluster
                    .current_context_name()
                    .map(std::string::ToString::to_string);
                let config_path = self
                    .cluster
                    .config_path()
                    .map(std::string::ToString::to_string);
                let mut node_info =
                    NodeInfoComponent::new(hostname, address.clone(), context, config_path, tab);
                if let Some(client) = self.cluster.client() {
                    node_info.set_client(client.with_node(&address));
                    load = true;
                } else {
                    node_info.set_error("No Talos client configured".to_string());
                }
                self.show_component(item, ViewComponent::NodeInfo(Box::new(node_info)), load);
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
                if !self.view.is_overlay() {
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
                        crate::components::diagnostics::k8s::connect(
                            &talos_client,
                            cp_ip.as_deref(),
                        )
                        .await
                        .ok()
                        .map(|(k8s, _source)| k8s)
                    }));
                }

                self.rolling_operations = Some(rolling_ops);
                if !self.view.is_overlay() {
                    self.overlay_return = self.view.clone();
                }
                self.view = View::RollingOperations;
            }
            _ => {
                // Forward to current component
                if let Some(c) = self.active_mut()
                    && let Some(next_action) = c.update(action)?
                {
                    Box::pin(self.handle_action(next_action)).await?;
                }
            }
        }
        Ok(())
    }
}

/// Seconds since the Unix epoch
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One tick of following the selection to `key`: a new selection starts
/// waiting `delay`, the same one is due once that passed. The pending
/// selection to keep, and whether to show it now.
fn follow_step(
    pending: Option<(String, Instant)>,
    key: String,
    now: Instant,
    delay: Duration,
) -> (Option<(String, Instant)>, bool) {
    match pending {
        Some((pending, due)) if pending == key && now < due => (Some((pending, due)), false),
        Some((pending, _)) if pending == key => (None, true),
        _ => (Some((key, now + delay)), false),
    }
}

/// Put a view aside under `key`, replacing an older copy, the oldest
/// dropped past `VIEW_CACHE_SIZE`
fn cache_insert<T>(cache: &mut Vec<(String, T)>, key: String, view: T) {
    cache.retain(|(k, _)| *k != key);
    cache.push((key, view));
    if cache.len() > VIEW_CACHE_SIZE {
        cache.remove(0);
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

    /// Choosing the dashboard from the menu (Enter or a click) puts the
    /// keyboard in its services list, as other views take it
    #[tokio::test]
    async fn dashboard_from_the_menu_takes_the_keyboard() {
        let mut app = app_with_view(View::Hint);
        app.cluster.set_focused_pane(FocusedPane::Menu);
        app.cluster.set_active_menu(NavMenuItem::Dashboard);
        let action = app.dispatch_key(key(KeyCode::Enter)).unwrap();
        assert!(matches!(action, Some(Action::ShowDashboard)));
        app.handle_action(Action::ShowDashboard).await.unwrap();
        assert_eq!(app.view, View::Cluster);
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Services);

        // Already on the dashboard: the same
        app.cluster.set_focused_pane(FocusedPane::Menu);
        let action = app.dispatch_key(key(KeyCode::Enter)).unwrap();
        assert!(matches!(action, Some(Action::ShowDashboard)));
        app.handle_action(Action::ShowDashboard).await.unwrap();
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Services);

        // Esc in the services list hands the keyboard back to the tree
        assert!(app.dispatch_key(key(KeyCode::Esc)).unwrap().is_none());
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Nodes);

        // Esc in the tree (not from the menu) keeps the tree's focus
        app.cluster.set_focused_pane(FocusedPane::Nodes);
        app.focus_after_show = false;
        app.handle_action(Action::ShowDashboard).await.unwrap();
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Nodes);
    }

    /// Right from the tree enters the view, Left returns to the side of the
    /// tree that had focus (here the menu), so menu → view → menu is two keys
    #[tokio::test]
    async fn left_and_right_move_between_tree_and_view() {
        let mut app = app_with_view(View::Hint);
        app.cluster.set_focused_pane(FocusedPane::Menu);
        assert!(app.dispatch_key(key(KeyCode::Right)).unwrap().is_none());
        assert!(app.view_has_focus);
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Menu);

        assert!(app.dispatch_key(key(KeyCode::Left)).unwrap().is_none());
        assert!(!app.view_has_focus);
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Menu);

        // On the dashboard, Right enters the services list and Left leaves it
        let mut app = app_with_view(View::Cluster);
        app.cluster.set_focused_pane(FocusedPane::Nodes);
        assert!(app.dispatch_key(key(KeyCode::Right)).unwrap().is_none());
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Services);
        assert!(app.dispatch_key(key(KeyCode::Left)).unwrap().is_none());
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Nodes);
    }

    /// Two clicks on one cell within the double-click time count as one
    /// double-click; a click elsewhere, or a late one, starts over
    #[test]
    fn double_click_detection() {
        let mut app = app_with_view(View::Hint);
        assert!(!app.is_double_click(40, 9));
        assert!(app.is_double_click(40, 9));
        // Consumed: a third click starts a new pair
        assert!(!app.is_double_click(40, 9));
        assert!(!app.is_double_click(41, 9));
        app.last_click = Some((Instant::now() - DOUBLE_CLICK * 2, 41, 9));
        assert!(!app.is_double_click(41, 9));
    }

    /// Tab cycles the windows: from a view (without its own tabs) to the
    /// Nodes list, Shift+Tab back to the menu
    #[test]
    fn tab_cycles_windows() {
        let mut app = app_with_view(View::Hint);
        app.cluster.set_focused_pane(FocusedPane::Menu);
        assert!(app.dispatch_key(key(KeyCode::Tab)).unwrap().is_none());
        assert!(app.view_has_focus);
        assert!(app.dispatch_key(key(KeyCode::Tab)).unwrap().is_none());
        assert!(!app.view_has_focus);
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Nodes);

        app.view_has_focus = true;
        assert!(app.dispatch_key(key(KeyCode::BackTab)).unwrap().is_none());
        assert!(!app.view_has_focus);
        assert_eq!(app.cluster.focused_pane(), FocusedPane::Menu);
    }

    /// q in a view goes back like Esc (Esc never arrives in some
    /// terminals); q in the tree quits
    #[tokio::test]
    async fn q_is_back_in_a_view_and_quit_in_the_tree() {
        let mut app = app_with_view(View::Hint);
        app.view_has_focus = true;
        assert!(matches!(
            app.dispatch_key(key(KeyCode::Char('q'))).unwrap(),
            Some(Action::Back)
        ));
        app.handle_action(Action::Back).await.unwrap();
        assert!(!app.view_has_focus);
        assert!(matches!(
            app.dispatch_key(key(KeyCode::Char('q'))).unwrap(),
            Some(Action::Quit)
        ));
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

    fn processes(sort: crate::components::processes::SortBy) -> ViewComponent {
        let mut p = ProcessesComponent::new("bc1".to_string(), Vec::new());
        p.set_sort_by(sort);
        ViewComponent::Processes(Box::new(p))
    }

    #[tokio::test]
    async fn leaving_a_view_keeps_its_settings_and_caches_it() {
        use crate::components::processes::SortBy;
        let mut app = app_with_view(View::Cluster);
        app.show_component(NavMenuItem::Processes, processes(SortBy::Mem), false);
        assert_eq!(app.view, View::Processes);
        assert!(app.shown().is_some());

        app.leave_view();
        assert!(app.component.is_none());
        assert_eq!(app.settings.processes_sort, SortBy::Mem);
        assert_eq!(app.view_cache.len(), 1);
        // The saved state has the setting whether or not the view is open
        assert_eq!(app.current_state().processes_sort.as_deref(), Some("Mem"));

        let (_, cached) = app.view_cache.pop().unwrap();
        app.restore_view(NavMenuItem::Processes, cached);
        assert_eq!(app.view, View::Processes);
        assert!(matches!(app.component, Some(ViewComponent::Processes(_))));
    }

    #[tokio::test]
    async fn a_refresh_lands_only_in_the_view_it_ran_for() {
        use crate::components::processes::SortBy;
        let mut app = app_with_view(View::Cluster);
        app.show_component(
            NavMenuItem::Etcd,
            ViewComponent::Etcd(Box::default()),
            false,
        );
        app.refresh_job = Some(tokio::spawn(async { processes(SortBy::Mem) }));
        while !app.refresh_job.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        app.poll_refresh_job();
        assert!(app.refresh_job.is_none());
        assert!(matches!(app.component, Some(ViewComponent::Etcd(_))));

        // Streams and the network view refresh on their own
        let network = NetworkStatsComponent::new("bc1".to_string(), "10.0.0.1".to_string());
        assert!(
            ViewComponent::Network(Box::new(network))
                .spawn_refresh()
                .is_none()
        );
    }

    #[test]
    fn following_waits_for_the_selection_to_rest() {
        let t0 = Instant::now();
        let delay = Duration::from_millis(300);
        let ms = |n| t0 + Duration::from_millis(n);
        // A new selection waits
        let (pending, due) = follow_step(None, "bc/bc1".into(), t0, delay);
        assert!(!due);
        assert_eq!(pending, Some(("bc/bc1".to_string(), ms(300))));
        // Still the same, not long enough
        let (pending, due) = follow_step(pending, "bc/bc1".into(), ms(299), delay);
        assert!(!due && pending.is_some());
        // Moving on restarts the wait for the new one
        let (pending, due) = follow_step(pending, "bc/bc2".into(), ms(299), delay);
        assert!(!due);
        assert_eq!(pending, Some(("bc/bc2".to_string(), ms(599))));
        // Rested: shown, nothing pending
        let (pending, due) = follow_step(pending, "bc/bc2".into(), ms(599), delay);
        assert!(due && pending.is_none());
    }

    #[test]
    fn view_cache_replaces_and_evicts_the_oldest() {
        let mut cache = Vec::new();
        for (i, key) in ["a", "b", "c", "d"].into_iter().enumerate() {
            cache_insert(&mut cache, key.to_string(), i);
        }
        // Shown again: the newer copy replaces the old one and is newest
        cache_insert(&mut cache, "a".to_string(), 10);
        let keys =
            |c: &Vec<(String, usize)>| c.iter().map(|(k, v)| (k.clone(), *v)).collect::<Vec<_>>();
        assert_eq!(
            keys(&cache),
            [
                ("b".into(), 1),
                ("c".into(), 2),
                ("d".into(), 3),
                ("a".into(), 10)
            ]
        );
        cache_insert(&mut cache, "e".to_string(), 4);
        assert_eq!(cache.len(), VIEW_CACHE_SIZE);
        assert_eq!(cache[0].0, "c");
        assert_eq!(cache.last().unwrap().0, "e");
    }
}
