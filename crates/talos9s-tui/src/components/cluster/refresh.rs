//! Connecting to the clusters and refreshing them in the background

use super::*;

impl ClusterComponent {
    /// Initialize connection to all Talos clusters from talosconfig
    pub async fn connect(&mut self) -> Result<()> {
        // Install crypto provider (needed for rustls)
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Load talosconfig - use custom path if provided via --config flag
        let config = match TalosConfig::load(self.config_path.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to load talosconfig: {}", e);
                return Ok(());
            }
        };

        // Determine which contexts to load based on --context flag
        let context_names: Vec<String> = match &self.context_filter {
            Some(ctx_name) => {
                // Verify the context exists
                if config.contexts.contains_key(ctx_name) {
                    vec![ctx_name.clone()]
                } else {
                    tracing::error!(
                        "Context '{}' not found in talosconfig. Available contexts: {:?}",
                        ctx_name,
                        config.contexts.keys().collect::<Vec<_>>()
                    );
                    return Ok(());
                }
            }
            None => {
                // talosconfig contexts are a map, so their order is arbitrary:
                // list the current context first, then the rest by name
                let mut names: Vec<String> = config.contexts.keys().cloned().collect();
                names.sort_by_key(|name| (*name != config.context, name.clone()));
                names
            }
        };

        // Create ClusterData for each context. Nothing is dialled here: every
        // cluster connects and loads in its own background task (see
        // `start_background_refresh`), so the UI shows up immediately and a
        // reachable cluster never waits behind an unreachable one.
        self.abort_background_refresh();
        self.clusters.clear();
        for (idx, name) in context_names.iter().enumerate() {
            let mut cluster = ClusterData {
                name: name.clone(),
                expanded: idx == 0, // Expand first cluster by default
                controlplane_expanded: true,
                workers_expanded: true,
                loading: true,
                ..Default::default()
            };
            match config.get_context(name) {
                Ok(ctx) => {
                    cluster.endpoints = ctx.endpoints.clone();
                    cluster.context = Some(ctx.clone());
                }
                Err(e) => {
                    cluster.error = Some(e.to_string());
                    cluster.loading = false;
                }
            }
            self.clusters.push(cluster);
        }
        self.background_refresh = self.clusters.iter().map(|_| None).collect();
        self.refresh_progress = self.clusters.iter().map(|_| None).collect();
        self.cluster_refreshed_at = self.clusters.iter().map(|_| None).collect();
        self.start_background_refresh();

        // Set initial selection
        if !self.clusters.is_empty() {
            self.selected_item = NodeListItem::ClusterHeader(0);
            self.active_cluster = 0;
        }

        Ok(())
    }

    /// Refresh all cluster data now (`r`): restart every cluster's
    /// background refresh. Returns immediately; results land as each cluster
    /// finishes.
    pub async fn refresh(&mut self) -> Result<()> {
        // Older in-flight snapshots would overwrite the fresh results
        self.abort_background_refresh();
        self.cluster_refreshed_at.fill(None);
        self.start_background_refresh();
        Ok(())
    }

    pub(super) fn abort_background_refresh(&mut self) {
        for handle in self.background_refresh.iter_mut().filter_map(Option::take) {
            handle.abort();
        }
    }

    /// Start a background refresh for each cluster that is due and has none
    /// in flight.
    ///
    /// Each cluster connects (if needed) and refreshes on a clone in its own
    /// spawned task, so the UI keeps drawing and a slow or unreachable cluster
    /// never delays the others. This also rebuilds node lists periodically,
    /// so a node that dropped out while rebooting reappears, and retries
    /// clusters that could not be reached. `apply_background_refresh` merges
    /// the results back.
    pub fn start_background_refresh(&mut self) {
        if self.background_refresh.len() != self.clusters.len() {
            return;
        }
        let interval =
            std::time::Duration::from_secs(talos9s_core::constants::refresh_intervals::SLOW);
        for (idx, cluster) in self.clusters.iter().enumerate() {
            let (Some(running), Some(&refreshed_at)) = (
                self.background_refresh.get(idx),
                self.cluster_refreshed_at.get(idx),
            ) else {
                continue;
            };
            if running.is_some() || cluster.context.is_none() {
                continue;
            }
            // The first load always runs, and so does reconnecting a cluster
            // whose connection was dropped (views take their client from
            // here); otherwise only with auto-refresh on
            // The cluster being looked at gets the full node table; the others
            // only what the tree needs. Looking at a cluster whose last
            // refresh was the light one refreshes it right away.
            let detailed = idx == self.active_cluster;
            let due = match refreshed_at {
                None => true,
                Some(at) if cluster.client.is_none() => at.elapsed() >= RECONNECT_INTERVAL,
                Some(at) => {
                    (detailed && !cluster.detailed)
                        || (self.auto_refresh && at.elapsed() >= interval)
                }
            };
            if !due {
                continue;
            }
            let previous = cluster.clone();
            let mut cluster = cluster.clone();
            let config_path = self.config_path.clone();
            let (progress, progress_rx) = tokio::sync::watch::channel(None);
            if let Some(slot) = self.refresh_progress.get_mut(idx) {
                *slot = Some(progress_rx);
            }
            let Some(slot) = self.background_refresh.get_mut(idx) else {
                continue;
            };
            *slot = Some(tokio::spawn(async move {
                let refresh = cluster.refresh(config_path.as_deref(), detailed, Some(&progress));
                match tokio::time::timeout(CLUSTER_REFRESH_TIMEOUT, refresh).await {
                    Ok(()) => cluster,
                    // Half-done state is inconsistent: keep what was shown
                    Err(_) => previous.refresh_timed_out(),
                }
            }));
        }
    }

    /// Merge finished background refreshes, keeping UI state (expand/collapse)
    /// that the user may have changed while they ran.
    pub fn apply_background_refresh(&mut self) {
        let mut changed = false;
        for idx in 0..self.background_refresh.len() {
            let Some(running) = self
                .background_refresh
                .get(idx)
                .and_then(Option::as_ref)
                .map(|handle| !handle.is_finished())
            else {
                continue;
            };
            // A running refresh: show what it has so far
            if running {
                let snapshot = self
                    .refresh_progress
                    .get_mut(idx)
                    .and_then(Option::as_mut)
                    .filter(|rx| rx.has_changed().unwrap_or(false))
                    .and_then(|rx| rx.borrow_and_update().clone());
                if let Some(snapshot) = snapshot {
                    changed |= self.adopt_cluster(idx, snapshot);
                }
                continue;
            }
            if let Some(slot) = self.refresh_progress.get_mut(idx) {
                *slot = None;
            }
            let Some(handle) = self.background_refresh.get_mut(idx).and_then(Option::take) else {
                continue;
            };
            // Always mark the attempt so a failing refresh is retried on
            // schedule, not on every tick.
            if let Some(at) = self.cluster_refreshed_at.get_mut(idx) {
                *at = Some(std::time::Instant::now());
            }

            let fresh = match crate::task::poll_finished(handle) {
                Some(Ok(fresh)) => fresh,
                Some(Err(e)) => {
                    // A panicked refresh: don't leave the cluster "connecting…"
                    tracing::warn!("Background cluster refresh failed: {}", e);
                    if let Some(current) = self.clusters.get_mut(idx) {
                        current.refresh_failed(format!("Refresh failed — {e}"));
                    }
                    continue;
                }
                None => continue,
            };
            changed |= self.adopt_cluster(idx, fresh);
        }
        if changed {
            self.last_refresh = Some(std::time::Instant::now());
            self.resolve_pending_selection();
            self.ensure_selection_visible();
        }
    }

    /// Replace a cluster's data with a refresh's, keeping the UI state
    /// (expand/collapse) the user may have changed while it ran
    pub(super) fn adopt_cluster(&mut self, idx: usize, mut fresh: ClusterData) -> bool {
        let Some(current) = self.clusters.get(idx) else {
            return false;
        };
        if current.name != fresh.name {
            return false;
        }
        fresh.expanded = current.expanded;
        fresh.controlplane_expanded = current.controlplane_expanded;
        fresh.workers_expanded = current.workers_expanded;
        // The tree selects a node by its position in the list: a node added
        // or dropped by this refresh would move the cursor (and the view
        // following it) to another node. Keep it on the same name.
        let selected = match self.selected_item {
            NodeListItem::ControlPlaneNode(i, _) | NodeListItem::WorkerNode(i, _) if i == idx => {
                self.current_node_name()
            }
            _ => None,
        };
        if let Some(current) = self.clusters.get_mut(idx) {
            *current = fresh;
        }
        if let Some(name) = selected {
            self.reselect_node(idx, &name);
        }
        true
    }

    /// Point the selection at `name` in cluster `idx`, as a control plane
    /// or worker node, without unfolding anything. `false` when it isn't
    /// listed.
    pub(super) fn reselect_node(&mut self, idx: usize, name: &str) -> bool {
        let cp = self
            .controlplane_nodes_for(idx)
            .iter()
            .position(|(_, v)| v.node == name);
        let workers = self
            .worker_nodes_for(idx)
            .iter()
            .position(|(_, v)| v.node == name);
        self.selected_item = match (cp, workers) {
            (Some(i), _) => NodeListItem::ControlPlaneNode(idx, i),
            (None, Some(i)) => NodeListItem::WorkerNode(idx, i),
            _ => return false,
        };
        true
    }
}
