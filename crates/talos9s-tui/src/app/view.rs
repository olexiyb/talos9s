//! The view on the right of the workspace and its one dispatch point

use crate::components::multi_logs::{ALL_LEVELS, LevelFilter};
use crate::components::network::NetworkSettings;
use crate::components::processes;
use crate::components::volumes::VolumesViewState;
use crate::components::{
    BackgroundRefresh, Component, DiagnosticsComponent, EtcdComponent, GroupComponent,
    LifecycleComponent, MultiLogsComponent, NetworkStatsComponent, NodeInfoComponent,
    ProcessesComponent, SecurityComponent, StorageComponent, VolumesComponent,
    WorkloadHealthComponent,
};
use tokio::task::JoinHandle;

/// What the right side shows; the views with a component carry it in
/// [`ViewComponent`]
#[derive(Debug, Clone, PartialEq)]
pub(super) enum View {
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
    /// The KubeSpan peer matrix of `App::kubespan_nodes`, drawn by the
    /// cluster component from its own data
    KubeSpanMatrix,
}

impl View {
    /// An operations overlay, drawn over the whole screen
    pub(super) fn is_overlay(&self) -> bool {
        matches!(self, View::NodeOperations | View::RollingOperations)
    }
}

/// The component of the view on the right. Overlays (operations) and the
/// dashboard live outside it: they aren't replaced, cached or refreshed in
/// the background like these.
pub(super) enum ViewComponent {
    MultiLogs(Box<MultiLogsComponent>),
    Etcd(Box<EtcdComponent>),
    Processes(Box<ProcessesComponent>),
    Network(Box<NetworkStatsComponent>),
    Diagnostics(Box<DiagnosticsComponent>),
    Security(Box<SecurityComponent>),
    Lifecycle(Box<LifecycleComponent>),
    Workloads(Box<WorkloadHealthComponent>),
    Volumes(Box<VolumesComponent>),
    Storage(Box<StorageComponent>),
    NodeInfo(Box<NodeInfoComponent>),
    Group(Box<GroupComponent>),
}

/// `$body` with `$c` bound to the component, whichever view it is
macro_rules! each {
    ($value:expr, $c:ident => $body:expr) => {
        match $value {
            ViewComponent::MultiLogs($c) => $body,
            ViewComponent::Etcd($c) => $body,
            ViewComponent::Processes($c) => $body,
            ViewComponent::Network($c) => $body,
            ViewComponent::Diagnostics($c) => $body,
            ViewComponent::Security($c) => $body,
            ViewComponent::Lifecycle($c) => $body,
            ViewComponent::Workloads($c) => $body,
            ViewComponent::Volumes($c) => $body,
            ViewComponent::Storage($c) => $body,
            ViewComponent::NodeInfo($c) => $body,
            ViewComponent::Group($c) => $body,
        }
    };
}

/// Run `refresh()` on a clone in a spawned task, recording a failure as the
/// clone's error, and hand the clone back as `$variant`. (A macro because
/// each component's `refresh` is an inherent async method.)
macro_rules! spawn_refresh {
    ($component:expr, $variant:path) => {{
        let mut component = $component.clone();
        tokio::spawn(async move {
            if let Err(e) = component.refresh().await {
                component.set_error(e.to_string());
            }
            $variant(component)
        })
    }};
}

/// View settings that outlive the view: the next one of its kind starts
/// with them, and they are saved for the next run
#[derive(Debug, Clone)]
pub(super) struct ViewSettings {
    pub(super) log_levels: LevelFilter,
    pub(super) volumes: Option<VolumesViewState>,
    pub(super) processes_sort: processes::SortBy,
    pub(super) network: NetworkSettings,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            log_levels: ALL_LEVELS,
            volumes: None,
            processes_sort: Default::default(),
            network: Default::default(),
        }
    }
}

impl ViewComponent {
    /// Record the settings of this view that carry over
    pub(super) fn carry_over(&self, settings: &mut ViewSettings) {
        match self {
            ViewComponent::MultiLogs(l) => settings.log_levels = l.level_filter(),
            ViewComponent::Volumes(v) => settings.volumes = Some(v.view_state()),
            ViewComponent::Processes(p) => settings.processes_sort = p.sort_by(),
            ViewComponent::Network(n) => settings.network = n.settings(),
            _ => {}
        }
    }

    pub(super) fn view(&self) -> View {
        match self {
            ViewComponent::MultiLogs(_) => View::MultiLogs,
            ViewComponent::Etcd(_) => View::Etcd,
            ViewComponent::Processes(_) => View::Processes,
            ViewComponent::Network(_) => View::Network,
            ViewComponent::Diagnostics(_) => View::Diagnostics,
            ViewComponent::Security(_) => View::Security,
            ViewComponent::Lifecycle(_) => View::Lifecycle,
            ViewComponent::Workloads(_) => View::Workloads,
            ViewComponent::Volumes(_) => View::Volumes,
            ViewComponent::Storage(_) => View::Storage,
            ViewComponent::NodeInfo(_) => View::NodeInfo,
            ViewComponent::Group(_) => View::Group,
        }
    }

    pub(super) fn component(&self) -> &dyn Component {
        each!(self, c => c.as_ref())
    }

    pub(super) fn component_mut(&mut self) -> &mut dyn Component {
        each!(self, c => c.as_mut())
    }

    pub(super) fn set_error(&mut self, error: String) {
        each!(self, c => c.set_error(error));
    }

    /// Start a refresh on a clone, off the event loop. Logs stream and
    /// network refreshes in the background on its own: they have none.
    pub(super) fn spawn_refresh(&self) -> Option<JoinHandle<ViewComponent>> {
        Some(match self {
            ViewComponent::MultiLogs(_) | ViewComponent::Network(_) => return None,
            ViewComponent::Etcd(c) => spawn_refresh!(c, ViewComponent::Etcd),
            ViewComponent::Processes(c) => spawn_refresh!(c, ViewComponent::Processes),
            ViewComponent::Diagnostics(c) => spawn_refresh!(c, ViewComponent::Diagnostics),
            ViewComponent::Security(c) => spawn_refresh!(c, ViewComponent::Security),
            ViewComponent::Lifecycle(c) => spawn_refresh!(c, ViewComponent::Lifecycle),
            ViewComponent::Workloads(c) => spawn_refresh!(c, ViewComponent::Workloads),
            ViewComponent::Volumes(c) => spawn_refresh!(c, ViewComponent::Volumes),
            ViewComponent::Storage(c) => spawn_refresh!(c, ViewComponent::Storage),
            ViewComponent::NodeInfo(c) => spawn_refresh!(c, ViewComponent::NodeInfo),
            ViewComponent::Group(c) => spawn_refresh!(c, ViewComponent::Group),
        })
    }

    /// Take what a finished refresh loaded; one of another view (the view
    /// changed while it ran) is dropped
    pub(super) fn adopt_refresh(&mut self, refreshed: ViewComponent) {
        use ViewComponent as V;
        match (self, refreshed) {
            (V::Etcd(c), V::Etcd(r)) => c.adopt_refresh(*r),
            (V::Processes(c), V::Processes(r)) => c.adopt_refresh(*r),
            (V::Diagnostics(c), V::Diagnostics(r)) => c.adopt_refresh(*r),
            (V::Security(c), V::Security(r)) => c.adopt_refresh(*r),
            (V::Lifecycle(c), V::Lifecycle(r)) => c.adopt_refresh(*r),
            (V::Workloads(c), V::Workloads(r)) => c.adopt_refresh(*r),
            (V::Volumes(c), V::Volumes(r)) => c.adopt_refresh(*r),
            (V::Storage(c), V::Storage(r)) => c.adopt_refresh(*r),
            (V::NodeInfo(c), V::NodeInfo(r)) => c.adopt_refresh(*r),
            (V::Group(c), V::Group(r)) => c.adopt_refresh(*r),
            _ => {}
        }
    }
}
