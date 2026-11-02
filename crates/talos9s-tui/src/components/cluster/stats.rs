//! Background fetches of the selected node's stats and network summary

use super::*;

/// How often the selected node's network summary (with Tailscale, KubeSpan
/// peers and clock offset) is re-fetched. It changes rarely and costs seven
/// talosctl invocations per node, so this is much slower than the stats
/// auto-refresh.
pub(super) const NETWORK_INFO_REFRESH: std::time::Duration = std::time::Duration::from_secs(30);

impl ClusterComponent {
    /// Collect a finished network summary fetch and start one for the selected
    /// node(s) whose summary is missing or older than `NETWORK_INFO_REFRESH`.
    ///
    /// Runs in a spawned task so slow talosctl calls never block drawing.
    pub fn poll_network_info(&mut self) {
        if self.network_fetch.as_ref().is_some_and(|f| f.is_finished())
            && let Some(fetch) = self.network_fetch.take()
        {
            let now = std::time::Instant::now();
            let results = match crate::task::poll_finished(fetch) {
                Some(Ok(results)) => results,
                _ => Vec::new(),
            };
            for (key, result, clock) in results {
                // A failed clock read keeps the last known offset
                if let Some(clock) = clock {
                    self.clock_info.insert(key.clone(), clock);
                }
                let previous = self.network_info.remove(&key).and_then(|(_, info)| info);
                let info = match result {
                    Ok(info) => Some(info),
                    Err(e) => {
                        tracing::debug!("Network info for {:?} failed: {}", key, e);
                        previous
                    }
                };
                self.network_info.insert(key, (now, info));
            }
        }

        if !self.auto_refresh || self.network_fetch.is_some() {
            return;
        }
        let Some((cluster_idx, nodes)) = self.selected_nodes() else {
            return;
        };
        let Some(cluster) = self.clusters.get(cluster_idx) else {
            return;
        };
        let targets: Vec<((String, String), String)> = nodes
            .into_iter()
            .filter_map(|node| {
                let ip = cluster.node_ips.get(&node)?.clone();
                let key = (cluster.name.clone(), node);
                let fresh = self
                    .network_info
                    .get(&key)
                    .is_some_and(|(at, _)| at.elapsed() < NETWORK_INFO_REFRESH);
                (!fresh).then_some((key, ip))
            })
            .collect();
        if targets.is_empty() {
            return;
        }

        let context = cluster.name.clone();
        let config_path = self.config_path.clone();
        let client = cluster.client.clone();
        self.network_fetch = Some(tokio::spawn(async move {
            let fetches = targets.into_iter().map(|(key, ip)| {
                let context = &context;
                let config_path = config_path.as_deref();
                let node_client = client.as_ref().map(|c| c.with_node(&ip));
                async move {
                    let (result, clock) = tokio::join!(
                        talos_rs::get_node_network_info(context, &ip, config_path),
                        async {
                            match node_client {
                                Some(c) => fetch_node_clock(c).await,
                                None => None,
                            }
                        }
                    );
                    (key, result, clock)
                }
            });
            futures::future::join_all(fetches).await
        }));
    }

    /// Network summary for a node in a cluster, if fetched
    pub(super) fn get_node_network_info_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&NodeNetworkInfo> {
        let cluster = self.clusters.get(cluster_idx)?;
        self.network_info
            .get(&(cluster.name.clone(), node_name.to_string()))?
            .1
            .as_ref()
    }

    /// Clock offset of a node in a cluster, if fetched
    pub(super) fn get_node_clock_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&NodeClock> {
        let cluster = self.clusters.get(cluster_idx)?;
        self.clock_info
            .get(&(cluster.name.clone(), node_name.to_string()))
    }

    /// Network summary for a node in the active cluster, if fetched
    pub(super) fn get_node_network_info(&self, node_name: &str) -> Option<&NodeNetworkInfo> {
        self.get_node_network_info_for(self.active_cluster, node_name)
    }

    /// Refresh only the selected node's stats (memory, load, services).
    /// This is lighter weight than a full refresh.
    ///
    /// The calls run concurrently in a spawned task, each bounded by
    /// `NODE_STATS_TIMEOUT`, so a node with network trouble never blocks the
    /// event loop; `poll_selected_node_refresh` merges the results.
    pub fn refresh_selected_node(&mut self) {
        self.poll_selected_node_refresh();
        if self.node_stats_fetch.is_some() {
            return;
        }
        // Counts as an attempt, so a failing node is retried on schedule
        self.last_auto_refresh = Some(std::time::Instant::now());

        let Some(cluster) = self.clusters.get(self.active_cluster) else {
            return;
        };
        let Some(client) = cluster.client.clone() else {
            return;
        };
        let Some(node_name) = self.current_node_name() else {
            return;
        };
        let Some(node_ip) = cluster.node_ips.get(&node_name).cloned() else {
            return;
        };
        // Offline nodes are re-checked by the cluster refresh; querying one
        // here would only wait out a timeout.
        if cluster.offline.contains_key(&node_name) {
            return;
        }
        let context = cluster.name.clone();

        let node_client = client.with_node(&node_ip);
        self.node_stats_fetch = Some(tokio::spawn(async move {
            async fn bounded<T>(
                call: impl std::future::Future<Output = Result<T, talos_rs::TalosError>>,
            ) -> Option<T> {
                tokio::time::timeout(NODE_STATS_TIMEOUT, call)
                    .await
                    .ok()?
                    .ok()
            }
            let (services, memory, load_avg, system_stat, mounts) = tokio::join!(
                bounded(node_client.services()),
                bounded(node_client.memory()),
                bounded(node_client.load_avg()),
                bounded(node_client.system_stat()),
                bounded(node_client.mounts()),
            );
            NodeStatsFetch {
                context,
                node: node_name,
                services,
                memory,
                load_avg,
                system_stat,
                mounts,
            }
        }));
    }

    /// Merge a finished selected-node stats fetch into its cluster's data
    pub fn poll_selected_node_refresh(&mut self) {
        if !self
            .node_stats_fetch
            .as_ref()
            .is_some_and(|f| f.is_finished())
        {
            return;
        }
        let Some(Some(Ok(fetch))) = self.node_stats_fetch.take().map(crate::task::poll_finished)
        else {
            return;
        };
        let Some(cluster) = self.clusters.iter_mut().find(|c| c.name == fetch.context) else {
            return;
        };
        let node = fetch.node;

        if let Some(mut node_services) = fetch.services {
            for s in &mut node_services {
                s.node = node.clone();
            }
            cluster.services.retain(|s| s.node != node);
            cluster.services.extend(node_services);
        }
        if let Some(mut node_memory) = fetch.memory {
            for m in &mut node_memory {
                m.node = node.clone();
            }
            cluster.memory.retain(|m| m.node != node);
            cluster.memory.extend(node_memory);
        }
        if let Some(mut node_load) = fetch.load_avg {
            for l in &mut node_load {
                l.node = node.clone();
            }
            cluster.load_avg.retain(|l| l.node != node);
            cluster.load_avg.extend(node_load);
        }
        if let Some(mut node_stat) = fetch.system_stat {
            for s in &mut node_stat {
                s.node = node.clone();
            }
            cluster.system_stat.retain(|s| s.node != node);
            cluster.system_stat.extend(node_stat);
        }
        if let Some(mut node_mounts) = fetch.mounts {
            for m in &mut node_mounts {
                m.node = node.clone();
            }
            cluster.mounts.retain(|m| m.node != node);
            cluster.mounts.extend(node_mounts);
        }
    }

    /// Check if auto-refresh should trigger (every 5 seconds)
    pub fn should_auto_refresh(&self) -> bool {
        if !self.auto_refresh {
            return false;
        }
        match self.last_auto_refresh {
            None => true,
            Some(last) => {
                last.elapsed().as_secs() >= talos9s_core::constants::refresh_intervals::NORMAL
            }
        }
    }

    /// Get services for a node in a specific cluster
    pub(super) fn get_node_services_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&Vec<ServiceInfo>> {
        for_node(&self.clusters.get(cluster_idx)?.services, node_name).map(|s| &s.services)
    }

    /// Get services for a node in the active cluster
    pub(super) fn get_node_services(&self, node_name: &str) -> Option<&Vec<ServiceInfo>> {
        self.get_node_services_for(self.active_cluster, node_name)
    }

    /// Get memory for a node in a specific cluster
    pub(super) fn get_node_memory_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&MemInfo> {
        for_node(&self.clusters.get(cluster_idx)?.memory, node_name)?
            .meminfo
            .as_ref()
    }

    /// Get memory for a node in the active cluster
    pub(super) fn get_node_memory(&self, node_name: &str) -> Option<&MemInfo> {
        self.get_node_memory_for(self.active_cluster, node_name)
    }

    /// Get load average for a node in a specific cluster
    pub(super) fn get_node_load_avg_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&NodeLoadAvg> {
        for_node(&self.clusters.get(cluster_idx)?.load_avg, node_name)
    }

    /// Get load average for a node in the active cluster
    pub(super) fn get_node_load_avg(&self, node_name: &str) -> Option<&NodeLoadAvg> {
        self.get_node_load_avg_for(self.active_cluster, node_name)
    }

    /// Get system stats for a node in the active cluster
    pub(super) fn get_node_system_stat(&self, node_name: &str) -> Option<&NodeSystemStat> {
        self.get_node_system_stat_for(self.active_cluster, node_name)
    }

    /// Get system stats for a node in a specific cluster
    pub(super) fn get_node_system_stat_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&NodeSystemStat> {
        for_node(&self.clusters.get(cluster_idx)?.system_stat, node_name)
    }

    /// Get filesystem usage for a node in a specific cluster
    pub(super) fn get_node_mounts_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&NodeMounts> {
        for_node(&self.clusters.get(cluster_idx)?.mounts, node_name)
    }

    /// Get ZFS pools for a node in a specific cluster
    pub(super) fn get_node_zfs_pools_for(&self, cluster_idx: usize, node_name: &str) -> &[ZfsPool] {
        self.clusters
            .get(cluster_idx)
            .and_then(|c| c.zfs_pools.get(node_name))
            .map_or(&[], Vec::as_slice)
    }

    /// Get CPU info for a node in a specific cluster
    pub(super) fn get_node_cpu_info_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&NodeCpuInfo> {
        for_node(&self.clusters.get(cluster_idx)?.cpu_info, node_name)
    }

    /// Talos version a node reported (none for an offline placeholder)
    pub(super) fn get_node_talos_version_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&str> {
        self.clusters
            .get(cluster_idx)?
            .versions
            .iter()
            .find(|v| v.node == node_name && !v.version.is_empty())
            .map(|v| v.version.as_str())
    }

    /// Kubelet version from the Kubernetes Node object of the same name
    pub(super) fn get_node_kubelet_version_for(
        &self,
        cluster_idx: usize,
        node_name: &str,
    ) -> Option<&str> {
        let key = talos_rs::host_of(node_name);
        self.clusters
            .get(cluster_idx)?
            .kubelet_versions
            .get(&key.to_lowercase())
            .map(String::as_str)
    }

    /// Get CPU info for a node in the active cluster
    pub(super) fn get_node_cpu_info(&self, node_name: &str) -> Option<&NodeCpuInfo> {
        self.get_node_cpu_info_for(self.active_cluster, node_name)
    }
}
