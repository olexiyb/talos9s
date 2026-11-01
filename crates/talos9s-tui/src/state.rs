//! What talos9s remembers between runs, in `~/.talos9s/state.yaml`: where
//! you were (cluster, group or node, the menu entry, the pane width, folded
//! groups) and view settings (log levels, sort orders, toggles).
//!
//! Written only by talos9s, on every change, through a temporary file and a
//! rename so a kill never leaves it half-written. Text filters, selections
//! in tables and the packet capture filter are deliberately not kept, and
//! nothing from the clusters is stored. A missing or unreadable file, or a
//! value from an older version, means the default for that part.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything remembered. Enum values are stored by name so a renamed or
/// removed variant drops that one setting, not the whole file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// Selected tree row
    pub selection: Option<Selection>,
    /// Menu entry shown on the right (`NavMenuItem` name)
    pub view: Option<String>,
    /// Width of the left pane (`PaneWidth` name); none for automatic
    pub pane_width: Option<String>,
    /// Unfolded state of each cluster and its groups
    pub tree: Vec<Folds>,
    /// Cluster auto-refresh (`a` in the tree)
    pub auto_refresh: Option<bool>,
    /// Log levels shown, lower case (`error`, `warn`, `info`, `debug`, `other`)
    pub log_levels: Option<Vec<String>>,
    pub volumes: VolumesSettings,
    /// Processes sort (`processes::SortBy` name)
    pub processes_sort: Option<String>,
    pub network: NetworkSettings,
}

/// A tree row by name: the cluster, optionally a group (`control-plane` or
/// `workers`) or a node
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Selection {
    pub cluster: String,
    pub group: Option<String>,
    pub node: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Folds {
    pub cluster: String,
    pub expanded: bool,
    pub control_plane: bool,
    pub workers: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VolumesSettings {
    pub show_all: Option<bool>,
    /// `TypeFilter` name
    pub type_filter: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSettings {
    /// Interfaces sort (`network::SortBy` name)
    pub sort: Option<String>,
    /// Connections sort (`ConnSortBy` name)
    pub connections_sort: Option<String>,
    pub listening_only: Option<bool>,
}

/// Log level names in the logs view's panel order
pub const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "other"];

/// The variant of `all` whose `Debug` name is `name`
pub fn by_name<T: std::fmt::Debug + Copy>(all: &[T], name: Option<&str>) -> Option<T> {
    let name = name?;
    all.iter().copied().find(|v| format!("{v:?}") == name)
}

/// `Debug` name of an enum value, as stored
pub fn name_of<T: std::fmt::Debug>(value: T) -> String {
    format!("{value:?}")
}

/// `~/.talos9s/state.yaml`, or none without a home directory
pub fn default_path() -> Option<PathBuf> {
    Some(dirs_next::home_dir()?.join(".talos9s").join("state.yaml"))
}

/// The remembered state; defaults when the file is missing or unreadable
pub fn load(path: &Path) -> UiState {
    let Ok(text) = std::fs::read_to_string(path) else {
        return UiState::default();
    };
    serde_saphyr::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!("Ignoring {}: {}", path.display(), e);
        UiState::default()
    })
}

/// Write the state: to a temporary file next to it, then renamed over it
pub fn save(path: &Path, state: &UiState) -> std::io::Result<()> {
    let text = serde_saphyr::to_string(state).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_unknown_values() {
        let dir = std::env::temp_dir().join(format!("talos9s-state-{}", std::process::id()));
        let path = dir.join("state.yaml");
        let state = UiState {
            selection: Some(Selection {
                cluster: "kprod".into(),
                group: None,
                node: Some("kpw1".into()),
            }),
            view: Some("Network".into()),
            log_levels: Some(vec!["error".into(), "warn".into()]),
            ..Default::default()
        };
        save(&path, &state).unwrap();
        assert_eq!(load(&path), state);

        // Unknown keys are ignored, a garbled file means defaults
        std::fs::write(&path, "view: Logs\nsomething_new: 1\n").unwrap();
        assert_eq!(load(&path).view.as_deref(), Some("Logs"));
        std::fs::write(&path, "view: [unclosed").unwrap();
        assert_eq!(load(&path), UiState::default());
        let _ = std::fs::remove_dir_all(dir);

        #[derive(Debug, Clone, Copy, PartialEq)]
        enum Mode {
            Fast,
            Slow,
        }
        assert_eq!(
            by_name(&[Mode::Fast, Mode::Slow], Some("Slow")),
            Some(Mode::Slow)
        );
        assert_eq!(by_name(&[Mode::Fast, Mode::Slow], Some("Gone")), None);
        assert_eq!(name_of(Mode::Fast), "Fast");
    }
}
