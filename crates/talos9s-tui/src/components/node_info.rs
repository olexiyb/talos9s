//! Node info component - containers, machine config and system extensions
//!
//! - Containers: Talos system services (containerd `system` namespace) and
//!   Kubernetes containers (CRI), from the Containers API
//! - Config: the machine config in effect (`talosctl get machineconfig`),
//!   read-only, with secrets masked before it is kept or shown
//! - Extensions: installed system extensions (`talosctl get extensions`)

use crate::action::Action;
use crate::components::Component;
use color_eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use talos_rs::{
    ContainerInfo, ContainerKind, ExtensionInfo, TalosClient, get_extensions_for_node,
    get_machine_config_text_for_node,
};
use talos9s_core::AsyncState;

const AUTO_REFRESH_INTERVAL_SECS: u64 = talos9s_core::constants::refresh_intervals::VERY_SLOW;
/// Upper bound for one Containers call
const CONTAINERS_TIMEOUT: Duration = Duration::from_secs(10);

/// Tabs of the node info view
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeInfoTab {
    #[default]
    Containers,
    Config,
    Extensions,
}

impl NodeInfoTab {}

/// One value per tab, indexed by the tab
#[derive(Debug, Clone, Default)]
pub struct PerTab<T>([T; 3]);

impl<T> PerTab<T> {
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }
}

impl<T> std::ops::Index<NodeInfoTab> for PerTab<T> {
    type Output = T;
    fn index(&self, tab: NodeInfoTab) -> &T {
        let [containers, config, extensions] = &self.0;
        match tab {
            NodeInfoTab::Containers => containers,
            NodeInfoTab::Config => config,
            NodeInfoTab::Extensions => extensions,
        }
    }
}

impl<T> std::ops::IndexMut<NodeInfoTab> for PerTab<T> {
    fn index_mut(&mut self, tab: NodeInfoTab) -> &mut T {
        let [containers, config, extensions] = &mut self.0;
        match tab {
            NodeInfoTab::Containers => containers,
            NodeInfoTab::Config => config,
            NodeInfoTab::Extensions => extensions,
        }
    }
}

/// A machine config line as shown: secrets already replaced
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLine {
    pub text: String,
    /// A value on this line was hidden
    pub masked: bool,
}

/// A part of the config to pick from the outline: a document, a top-level
/// key, one of its keys, or a named list item (an inline manifest, a file)
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSection {
    pub label: String,
    /// 0 document, 1 top-level key, 2 its key, 3 named list item
    pub depth: u8,
    /// Lines `start..end` of the config
    pub start: usize,
    pub end: usize,
}

/// The machine config with its secrets masked
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaskedConfig {
    pub lines: Vec<ConfigLine>,
    /// Outline of `lines`
    pub sections: Vec<ConfigSection>,
    /// Values hidden
    pub hidden: usize,
    pub version: String,
    pub updated: Option<String>,
    pub staged_differs: bool,
}

/// Loaded node info (wrapped by AsyncState). Each source fails on its own.
#[derive(Debug, Clone, Default)]
pub struct NodeInfoData {
    pub containers: Vec<ContainerInfo>,
    pub containers_error: Option<String>,
    pub config: Option<MaskedConfig>,
    pub config_error: Option<String>,
    pub extensions: Vec<ExtensionInfo>,
    pub extensions_error: Option<String>,
    /// Per tab: a load of its source finished, with data or an error
    pub attempted: PerTab<bool>,
}

/// Masked configs fetched this session, by context and node. A large config
/// (inline manifests) takes a minute to fetch and rarely changes, so coming
/// back shows it at once; `r` fetches it again. Masked, and memory only.
static CONFIG_CACHE: LazyLock<Mutex<HashMap<String, MaskedConfig>>> =
    LazyLock::new(Default::default);

fn cached_config(key: &str) -> Option<MaskedConfig> {
    CONFIG_CACHE.lock().ok()?.get(key).cloned()
}

fn cache_config(key: String, config: &MaskedConfig) {
    if let Ok(mut cache) = CONFIG_CACHE.lock() {
        cache.insert(key, config.clone());
    }
}

/// Indentation and the key of a `key:` line (none for list items, comments,
/// blank lines)
fn key_of(text: &str) -> Option<(usize, &str)> {
    let trimmed = text.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return None;
    }
    let (key, _) = trimmed.split_once(':')?;
    (!key.contains(' ')).then_some((text.len() - trimmed.len(), key))
}

/// The value of `key:` on a line, quotes trimmed
fn value_of<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let rest = text.trim_start().trim_start_matches("- ").trim_start();
    let value = rest.strip_prefix(key)?.strip_prefix(':')?.trim();
    (!value.is_empty()).then(|| value.trim_matches(|c| c == '"' || c == '\''))
}

/// Outline of a (masked) machine config: each document; in large ones each
/// top-level key, its keys, and list items under them that have a `name` or
/// `path` (inline manifests, extra manifests, files)
pub fn config_outline(lines: &[ConfigLine]) -> Vec<ConfigSection> {
    let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    let mut out = Vec::new();
    let mut doc_start = 0;
    let mut documents = Vec::new();
    for (i, text) in texts.iter().enumerate() {
        // Only an unindented separator: an inline manifest's own documents
        // are separated by indented ones inside its block
        if text.trim_end() == "---" {
            if i > doc_start {
                documents.push((doc_start, i));
            }
            doc_start = i + 1;
        }
    }
    if texts.len() > doc_start {
        documents.push((doc_start, texts.len()));
    }
    for (n, &(start, end)) in documents.iter().enumerate() {
        let top = |key: &str| {
            texts
                .get(start..end)
                .unwrap_or_default()
                .iter()
                .find(|t| key_of(t).is_some_and(|(indent, k)| indent == 0 && k == key))
                .and_then(|t| value_of(t, key))
        };
        let label = match (top("kind"), top("name"), top("version")) {
            (Some(kind), Some(name), _) => format!("{kind} {name}"),
            (Some(kind), None, _) => kind.to_string(),
            (None, _, Some(version)) => version.to_string(),
            _ => format!("Document {}", n + 1),
        };
        out.push(ConfigSection {
            label,
            depth: 0,
            start,
            end,
        });
        // Small documents read fine whole
        if end - start < 40 {
            continue;
        }
        let children = |from: usize, to: usize, indent: usize| -> Vec<(usize, &str)> {
            (from..to)
                .filter_map(|i| {
                    texts
                        .get(i)
                        .and_then(|t| key_of(t))
                        .filter(|(ind, _)| *ind == indent)
                        .map(|(_, k)| (i, k))
                })
                .collect()
        };
        let ranges = |keys: &[(usize, &str)], to: usize| -> Vec<(usize, usize, String)> {
            keys.iter()
                .enumerate()
                .map(|(i, &(line, key))| {
                    let next = keys.get(i + 1).map_or(to, |&(l, _)| l);
                    (line, next, key.to_string())
                })
                .collect()
        };
        let first_indent = |from: usize, to: usize| {
            (from..to).find_map(|i| {
                let t = texts.get(i)?;
                let trimmed = t.trim_start();
                (!trimmed.is_empty() && !trimmed.starts_with('#')).then(|| t.len() - trimmed.len())
            })
        };
        for (s1, e1, k1) in ranges(&children(start, end, 0), end) {
            // Only keys holding more than a line
            let Some(indent1) = first_indent(s1 + 1, e1).filter(|i| *i > 0) else {
                continue;
            };
            out.push(ConfigSection {
                label: k1.clone(),
                depth: 1,
                start: s1,
                end: e1,
            });
            for (s2, e2, k2) in ranges(&children(s1 + 1, e1, indent1), e1) {
                // One-line values read fine in their parent
                if e2 - s2 < 2 {
                    continue;
                }
                out.push(ConfigSection {
                    label: format!("{k1} › {k2}"),
                    depth: 2,
                    start: s2,
                    end: e2,
                });
                // Named list items, e.g. each inline manifest
                let Some(item_indent) = first_indent(s2 + 1, e2) else {
                    continue;
                };
                let items: Vec<usize> = (s2 + 1..e2)
                    .filter(|&i| {
                        texts.get(i).is_some_and(|t| {
                            t.len() - t.trim_start().len() == item_indent
                                && t.trim_start().starts_with("- ")
                        })
                    })
                    .collect();
                for (j, &item) in items.iter().enumerate() {
                    let item_end = items.get(j + 1).copied().unwrap_or(e2);
                    let name = (item..item_end).find_map(|i| {
                        let t = texts.get(i)?;
                        value_of(t, "name").or_else(|| value_of(t, "path"))
                    });
                    if let Some(name) = name {
                        out.push(ConfigSection {
                            label: name.to_string(),
                            depth: 3,
                            start: item,
                            end: item_end,
                        });
                    }
                }
            }
        }
    }
    out
}

/// A byte size in at most 5 columns: `812B`, `14K`, `1.9M`
fn short_size(bytes: usize) -> String {
    match bytes {
        0..=999 => format!("{bytes}B"),
        1_000..=999_999 => format!("{}K", bytes / 1000),
        _ => format!("{:.1}M", bytes as f64 / 1e6),
    }
}

/// Placeholder for a hidden value
const HIDDEN: &str = "••••••";

/// Whether a key's value is a secret: keys, tokens, passwords, certificates'
/// private parts, registry auth, encryption secrets
fn secret_key(key: &str) -> bool {
    let key = key.trim_matches(|c| c == '"' || c == '\'').to_lowercase();
    key.ends_with("key")
        || key.ends_with("keys")
        || key == "auth"
        || ["token", "secret", "password", "passphrase", "credential"]
            .iter()
            .any(|s| key.contains(s))
}

/// Keys whose children are hidden whole: a Kubernetes Secret's values in an
/// inline manifest are keyed by arbitrary names
fn hide_children(key: &str) -> bool {
    matches!(key, "data" | "stringData")
}

/// Mask the secrets of a machine config, line by line, so it works the same
/// on the config's YAML and on the YAML, TOML or INI held in its block
/// scalars (inline manifests, `machine.files`):
///
/// - `key: value` and `key = value` with a secret key name: the value
/// - a secret key with a nested value or block scalar: everything under it
/// - `data:` / `stringData:` (Secret manifests): everything under them
/// - PEM private key blocks anywhere
// Slices end at the indent (spaces) or at a found ':' / '=': never inside a char
#[allow(clippy::string_slice)]
pub fn mask_config(text: &str) -> (Vec<ConfigLine>, usize) {
    let mut lines: Vec<ConfigLine> = Vec::new();
    let mut hidden = 0;
    // Hiding lines indented deeper than this, into the placeholder line at
    // the index, counting them
    let mut hiding: Option<(usize, usize, usize)> = None;
    let mut in_pem = false;

    let finish = |lines: &mut Vec<ConfigLine>, hiding: Option<(usize, usize, usize)>| {
        if let Some((_, at, count)) = hiding
            && count > 0
            && let Some(placeholder) = lines.get_mut(at)
        {
            placeholder
                .text
                .push_str(&format!("  ({count} lines hidden)"));
        }
    };

    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        if let Some((depth, at, count)) = hiding {
            // A list may sit at its key's indent
            let child = indent > depth || (indent == depth && line[indent..].starts_with("- "));
            if line.trim().is_empty() || child {
                hiding = Some((depth, at, count + 1));
                continue;
            }
            finish(&mut lines, hiding);
            hiding = None;
        }
        if in_pem {
            if line.contains("-----END") {
                in_pem = false;
            }
            continue;
        }
        // PEM (`EC PRIVATE KEY-----`) and PGP (`PGP PRIVATE KEY BLOCK-----`)
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            in_pem = !line.contains("-----END");
            hidden += 1;
            lines.push(ConfigLine {
                text: format!("{}{HIDDEN}  (private key hidden)", &line[..indent]),
                masked: true,
            });
            continue;
        }

        let trimmed = &line[indent..];
        let (item, body) = match trimmed.strip_prefix("- ") {
            Some(rest) => ("- ", rest),
            None => ("", trimmed),
        };
        let column = indent + item.len();
        let yaml = body
            .split_once(':')
            .filter(|(k, v)| {
                !k.is_empty()
                    && !k.starts_with('#')
                    && !k.contains(' ')
                    && (v.is_empty() || v.starts_with(' '))
            })
            .map(|(k, v)| (k, v.trim(), ": "));
        let ini = || {
            body.split_once('=')
                .map(|(k, v)| (k.trim(), v.trim(), " = "))
                // A plain name: `{"a":"b=="}` is JSON, not `key = value`
                .filter(|(k, v, _)| {
                    !k.is_empty() && !k.contains([' ', '{', ':', ',']) && !v.is_empty()
                })
        };
        let Some((key, value, sep)) = yaml.or_else(ini) else {
            lines.push(mask_inline(line, &mut hidden));
            continue;
        };

        let nested = value.is_empty() || value.starts_with('|') || value.starts_with('>');
        if secret_key(key) || (nested && hide_children(key)) {
            hidden += 1;
            let prefix = &line[..column];
            lines.push(ConfigLine {
                text: format!("{prefix}{key}{sep}{HIDDEN}"),
                masked: true,
            });
            if nested {
                hiding = Some((column, lines.len() - 1, 0));
            }
        } else {
            lines.push(mask_inline(line, &mut hidden));
        }
    }
    finish(&mut lines, hiding);
    (lines, hidden)
}

/// A line with no secret key of its own, with the secrets inside it hidden:
/// secret keys of JSON objects and YAML flow maps (`{"auth": "…"}`,
/// `{password: …}`) and passwords in URLs (`https://user:pass@host`)
fn mask_inline(line: &str, hidden: &mut usize) -> ConfigLine {
    let (text, flow) = mask_flow_values(line);
    let (text, urls) = mask_url_passwords(&text);
    *hidden += flow + urls;
    ConfigLine {
        text,
        masked: flow + urls > 0,
    }
}

/// The index just past the string starting with the quote at `start`
fn string_end(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        match b.get(i) {
            Some(b'\\') => i += 2,
            Some(b'"') => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// Values of secret keys in `{…}` objects, at any depth; the count hidden
// Every index is at an ASCII byte found by the scan, a char boundary
#[allow(clippy::string_slice)]
fn mask_flow_values(line: &str) -> (String, usize) {
    let b = line.as_bytes();
    let skip_ws = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };
    let (mut out, mut last, mut count, mut i) = (String::new(), 0, 0, 0);
    while i < b.len() {
        match b.get(i) {
            Some(b'"') => {
                i = string_end(b, i);
                continue;
            }
            Some(b'{' | b',') => {}
            _ => {
                i += 1;
                continue;
            }
        }
        // A key after `{` or `,`, quoted or bare
        let k = skip_ws(i + 1);
        let (key, after_key) = if b.get(k) == Some(&b'"') {
            let end = string_end(b, k);
            (&line[k + 1..end.saturating_sub(1).max(k + 1)], end)
        } else {
            let end = (k..b.len())
                .find(|&j| {
                    b.get(j)
                        .is_none_or(|c| !(c.is_ascii_alphanumeric() || b"_-.".contains(c)))
                })
                .unwrap_or(b.len());
            (&line[k..end], end)
        };
        let sep = skip_ws(after_key);
        if key.is_empty() || !matches!(b.get(sep), Some(b':' | b'=')) {
            i += 1;
            continue;
        }
        let start = skip_ws(sep + 1);
        // Nested objects and lists are scanned in turn
        if matches!(b.get(start), None | Some(b'{' | b'[')) {
            i = start;
            continue;
        }
        let quoted = b.get(start) == Some(&b'"');
        let end = if quoted {
            string_end(b, start)
        } else {
            let end = (start..b.len())
                .find(|&j| b.get(j).is_some_and(|c| b",}]".contains(c)))
                .unwrap_or(b.len());
            start + line[start..end].trim_end().len()
        };
        if secret_key(key) && end > start {
            out.push_str(&line[last..start]);
            out.push_str(if quoted {
                "\"••••••\""
            } else {
                HIDDEN
            });
            last = end;
            count += 1;
        }
        i = end.max(i + 1);
    }
    out.push_str(&line[last..]);
    (out, count)
}

/// The password of `scheme://user:password@host`; the count hidden
// Every index is at an ASCII byte found by the scan, a char boundary
#[allow(clippy::string_slice)]
fn mask_url_passwords(line: &str) -> (String, usize) {
    let (mut out, mut rest, mut count) = (String::new(), line, 0);
    while let Some(pos) = rest.find("://") {
        let start = pos + 3;
        let authority_end = rest[start..]
            .find(|c: char| c == '/' || c.is_whitespace() || "\"',}]".contains(c))
            .map_or(rest.len(), |e| start + e);
        let authority = &rest[start..authority_end];
        match authority.rfind('@').and_then(|at| {
            authority[..at]
                .find(':')
                .map(|colon| (start + colon + 1, start + at))
        }) {
            Some((from, to)) if to > from => {
                out.push_str(&rest[..from]);
                out.push_str(HIDDEN);
                count += 1;
                out.push_str(&rest[to..authority_end]);
            }
            _ => out.push_str(&rest[..authority_end]),
        }
        rest = &rest[authority_end..];
    }
    out.push_str(rest);
    (out, count)
}

/// Text being typed for the filter (lists) or search (config)
#[derive(Debug, Clone, Default)]
struct Query {
    text: String,
    editing: bool,
}

/// Node info component
#[derive(Clone)]
pub struct NodeInfoComponent {
    state: AsyncState<NodeInfoData>,
    hostname: String,
    tab: NodeInfoTab,
    client: Option<TalosClient>,
    node_address: String,
    context: Option<String>,
    config_path: Option<String>,
    /// Reload the config and extensions on the next refresh, not only the
    /// containers
    reload_all: bool,
    auto_refresh: bool,
    /// Per tab
    queries: PerTab<Query>,
    containers_table: TableState,
    extensions_table: TableState,
    /// First config line shown, and the first column
    config_scroll: usize,
    config_hscroll: usize,
    /// Config lines shown at once, from the last draw
    config_page: usize,
    /// Where the open list was last drawn, for mouse clicks
    content_area: Rect,
    /// Config: the outline entry shown, whether the keyboard is in the
    /// content (else the outline), the outline's first row shown, and where
    /// the outline was drawn
    config_section: usize,
    config_in_content: bool,
    outline_offset: usize,
    outline_area: Rect,
    /// When the view opened, for the time a slow config load has taken
    opened: Instant,
}

impl Default for NodeInfoComponent {
    fn default() -> Self {
        Self::new(
            String::new(),
            String::new(),
            None,
            None,
            NodeInfoTab::default(),
        )
    }
}

impl NodeInfoComponent {
    pub fn new(
        hostname: String,
        address: String,
        context: Option<String>,
        config_path: Option<String>,
        tab: NodeInfoTab,
    ) -> Self {
        let mut containers_table = TableState::default();
        containers_table.select(Some(0));
        let mut extensions_table = TableState::default();
        extensions_table.select(Some(0));
        Self {
            // Loading until the first (background) refresh lands
            state: {
                let mut state = AsyncState::with_data(NodeInfoData::default());
                state.start_loading();
                state
            },
            hostname,
            tab,
            client: None,
            node_address: talos_rs::host_of(&address).to_string(),
            context,
            config_path,
            reload_all: true,
            auto_refresh: true,
            queries: Default::default(),
            containers_table,
            extensions_table,
            config_scroll: 0,
            config_hscroll: 0,
            config_page: 20,
            content_area: Rect::default(),
            config_section: 0,
            config_in_content: false,
            outline_offset: 0,
            outline_area: Rect::default(),
            opened: Instant::now(),
        }
        .with_cached_config()
    }

    /// The config fetched earlier this session, shown at once; `r` reloads
    fn with_cached_config(mut self) -> Self {
        if let Some(config) = cached_config(&self.cache_key())
            && let Some(data) = self.state.data_mut()
        {
            data.config = Some(config);
            data.attempted[NodeInfoTab::Config] = true;
            // The containers and extensions still load
            self.reload_all = false;
        }
        self
    }

    fn cache_key(&self) -> String {
        format!(
            "{}/{}",
            self.context.as_deref().unwrap_or_default(),
            self.node_address
        )
    }

    /// Set the client (pointed at this node) for API calls
    pub fn set_client(&mut self, client: TalosClient) {
        self.client = Some(client);
    }

    pub fn set_error(&mut self, error: String) {
        self.state.set_error(error);
    }

    fn data(&self) -> Option<&NodeInfoData> {
        self.state.data()
    }

    /// Load the open tab's source first, then the others; afterwards reload
    /// the containers each time, and the config and extensions on `r` or
    /// after a failure
    pub async fn refresh(&mut self) -> Result<()> {
        self.state.start_loading();
        let Some(client) = &self.client else {
            self.state.set_error("No Talos client configured");
            return Ok(());
        };
        let mut data = self.state.take_data().unwrap_or_default();

        let first_pass = !data.attempted.iter().any(|a| *a);
        let want = |tab: NodeInfoTab| {
            if first_pass {
                return tab == self.tab;
            }
            match tab {
                NodeInfoTab::Containers => true,
                NodeInfoTab::Config => {
                    self.reload_all
                        || data.config_error.is_some()
                        || !data.attempted[NodeInfoTab::Config]
                }
                NodeInfoTab::Extensions => {
                    self.reload_all
                        || data.extensions_error.is_some()
                        || !data.attempted[NodeInfoTab::Extensions]
                }
            }
        };
        let (want_containers, want_config, want_extensions) = (
            want(NodeInfoTab::Containers),
            want(NodeInfoTab::Config),
            want(NodeInfoTab::Extensions),
        );

        let containers = async {
            if !want_containers {
                return None;
            }
            let (system, kubernetes) = tokio::join!(
                tokio::time::timeout(CONTAINERS_TIMEOUT, client.containers(ContainerKind::System)),
                tokio::time::timeout(
                    CONTAINERS_TIMEOUT,
                    client.containers(ContainerKind::Kubernetes)
                ),
            );
            let flatten = |r: std::result::Result<_, _>| match r {
                Ok(Ok(list)) => Ok(list),
                Ok(Err(e)) => Err(format!("{e}")),
                Err(_) => Err(format!("timed out after {}s", CONTAINERS_TIMEOUT.as_secs())),
            };
            Some((flatten(system), flatten(kubernetes)))
        };
        let context = self.context.as_deref();
        let config_path = self.config_path.as_deref();
        let node = self.node_address.as_str();
        let config = async {
            if want_config {
                Some(get_machine_config_text_for_node(context, node, config_path).await)
            } else {
                None
            }
        };
        let extensions = async {
            if want_extensions {
                Some(get_extensions_for_node(context, node, config_path).await)
            } else {
                None
            }
        };
        let (containers, config, extensions) = tokio::join!(containers, config, extensions);

        // Kubernetes containers are missing on a node without kubelet; the
        // system ones alone still make the tab useful
        if let Some((system, kubernetes)) = containers {
            match (system, kubernetes) {
                (Ok(mut system), Ok(kubernetes)) => {
                    system.extend(kubernetes);
                    data.containers = system;
                    data.containers_error = None;
                }
                (Ok(system), Err(e)) => {
                    data.containers = system;
                    data.containers_error = Some(format!("Kubernetes containers: {e}"));
                }
                (Err(e), Ok(kubernetes)) => {
                    data.containers = kubernetes;
                    data.containers_error = Some(format!("System containers: {e}"));
                }
                (Err(e), Err(_)) => data.containers_error = Some(e),
            }
            sort_containers(&mut data.containers);
            data.attempted[NodeInfoTab::Containers] = true;
        }
        if let Some(config) = config {
            match config {
                Ok(config) => {
                    let (lines, hidden) = mask_config(&config.active);
                    let sections = config_outline(&lines);
                    let masked = MaskedConfig {
                        lines,
                        sections,
                        hidden,
                        version: config.version,
                        updated: config.updated,
                        staged_differs: config.staged_differs,
                    };
                    cache_config(self.cache_key(), &masked);
                    data.config = Some(masked);
                    data.config_error = None;
                }
                Err(e) => data.config_error = Some(e.to_string()),
            }
            data.attempted[NodeInfoTab::Config] = true;
        }
        if let Some(extensions) = extensions {
            match extensions {
                Ok(extensions) => {
                    data.extensions = extensions;
                    data.extensions_error = None;
                }
                Err(e) => data.extensions_error = Some(e.to_string()),
            }
            data.attempted[NodeInfoTab::Extensions] = true;
        }
        if want_config && want_extensions {
            self.reload_all = false;
        }

        self.state.set_data(data);
        Ok(())
    }

    /// The open tab
    pub fn tab(&self) -> NodeInfoTab {
        self.tab
    }

    fn query(&self) -> &Query {
        &self.queries[self.tab]
    }

    fn query_mut(&mut self) -> &mut Query {
        &mut self.queries[self.tab]
    }

    /// Containers shown: pod sandboxes left out, filtered
    fn visible_containers(&self) -> Vec<&ContainerInfo> {
        let filter = self.queries[NodeInfoTab::Containers].text.to_lowercase();
        self.data()
            .map(|d| {
                d.containers
                    .iter()
                    .filter(|c| !c.sandbox)
                    .filter(|c| {
                        filter.is_empty()
                            || [&c.id, &c.pod, &c.name, &c.image, &c.status]
                                .iter()
                                .any(|f| f.to_lowercase().contains(&filter))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visible_extensions(&self) -> Vec<&ExtensionInfo> {
        let filter = self.queries[NodeInfoTab::Extensions].text.to_lowercase();
        self.data()
            .map(|d| {
                d.extensions
                    .iter()
                    .filter(|e| {
                        filter.is_empty()
                            || [&e.name, &e.version, &e.author, &e.description]
                                .iter()
                                .any(|f| f.to_lowercase().contains(&filter))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn config_lines(&self) -> &[ConfigLine] {
        self.data()
            .and_then(|d| d.config.as_ref())
            .map_or(&[], |c| c.lines.as_slice())
    }

    fn config_sections(&self) -> &[ConfigSection] {
        self.data()
            .and_then(|d| d.config.as_ref())
            .map_or(&[], |c| c.sections.as_slice())
    }

    /// Lines of the outline entry shown (the whole config without one)
    fn section_range(&self) -> (usize, usize) {
        self.config_sections()
            .get(self.config_section)
            .map_or((0, self.config_lines().len()), |s| (s.start, s.end))
    }

    /// Show outline entry `index`, from its first line
    fn select_section(&mut self, index: usize) {
        let count = self.config_sections().len();
        if count == 0 {
            return;
        }
        self.config_section = index.min(count - 1);
        self.config_scroll = self.section_range().0;
        self.config_hscroll = 0;
    }

    /// The narrowest outline entry holding line `line`
    fn section_for_line(&self, line: usize) -> Option<usize> {
        self.config_sections()
            .iter()
            .enumerate()
            .filter(|(_, s)| s.start <= line && line < s.end)
            .min_by_key(|(_, s)| s.end - s.start)
            .map(|(i, _)| i)
    }

    /// Move the selection or scroll by `delta` rows
    fn move_by(&mut self, delta: isize) {
        let step = |table: &mut TableState, len: usize| {
            if len == 0 {
                return;
            }
            let i = table.selected().unwrap_or(0).saturating_add_signed(delta);
            table.select(Some(i.min(len - 1)));
        };
        match self.tab {
            NodeInfoTab::Containers => {
                let len = self.visible_containers().len();
                step(&mut self.containers_table, len);
            }
            NodeInfoTab::Extensions => {
                let len = self.visible_extensions().len();
                step(&mut self.extensions_table, len);
            }
            NodeInfoTab::Config if self.config_in_content => {
                let (start, end) = self.section_range();
                let last = end.saturating_sub(1).max(start);
                self.config_scroll = self
                    .config_scroll
                    .saturating_add_signed(delta)
                    .clamp(start, last);
            }
            NodeInfoTab::Config => {
                let count = self.config_sections().len();
                if count > 0 {
                    let i = self
                        .config_section
                        .saturating_add_signed(delta)
                        .min(count - 1);
                    self.select_section(i);
                }
            }
        }
    }

    /// Scroll the config to the next (or previous) line matching the search
    fn search_config(&mut self, forward: bool) {
        let needle = self.queries[NodeInfoTab::Config].text.to_lowercase();
        if needle.is_empty() {
            return;
        }
        let lines = self.config_lines();
        let n = lines.len();
        if n == 0 {
            return;
        }
        let found = (1..=n)
            .map(|i| {
                if forward {
                    (self.config_scroll + i) % n
                } else {
                    (self.config_scroll + n - i % n) % n
                }
            })
            .find(|&i| {
                lines
                    .get(i)
                    .is_some_and(|l| l.text.to_lowercase().contains(&needle))
            });
        if let Some(i) = found {
            if let Some(section) = self.section_for_line(i) {
                self.config_section = section;
            }
            self.config_scroll = i;
            self.config_in_content = true;
        }
    }

    fn draw_error(frame: &mut Frame, area: Rect, title: &str, error: &str) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title.to_string());
        let text = Paragraph::new(format!("Could not read: {error}"))
            .style(Style::default().fg(Color::Red))
            .block(block);
        frame.render_widget(text, area);
    }

    fn draw_containers(&mut self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };
        let (mut running, mut exited) = (0, 0);
        for c in data.containers.iter().filter(|c| !c.sandbox) {
            if c.status.ends_with("RUNNING") {
                running += 1;
            } else if c.status.ends_with("EXITED") {
                exited += 1;
            }
        }
        let error = data.containers_error.clone();
        let visible = self.visible_containers();
        let header = Row::new(
            ["NAMESPACE", "POD", "CONTAINER", "IMAGE", "PID", "STATUS"]
                .map(|h| Cell::from(h).style(Style::default().add_modifier(Modifier::BOLD))),
        );
        let rows: Vec<Row> = visible
            .iter()
            .map(|c| {
                let (namespace, pod) = match c.kind {
                    ContainerKind::System => ("system", ""),
                    ContainerKind::Kubernetes => {
                        c.pod.split_once('/').unwrap_or(("", c.pod.as_str()))
                    }
                };
                let name = if c.kind == ContainerKind::System {
                    c.id.as_str()
                } else {
                    c.name.as_str()
                };
                let status = c
                    .status
                    .strip_prefix("CONTAINER_")
                    .unwrap_or(&c.status)
                    .to_string();
                let color = if c.status.ends_with("RUNNING") {
                    Color::Green
                } else if c.status.ends_with("EXITED") {
                    // Finished jobs and init containers exit normally
                    Color::DarkGray
                } else {
                    Color::Yellow
                };
                Row::new(vec![
                    Cell::from(namespace.to_string()),
                    Cell::from(pod.to_string()),
                    Cell::from(name.to_string()),
                    Cell::from(c.image.clone()),
                    Cell::from(if c.pid == 0 {
                        String::new()
                    } else {
                        c.pid.to_string()
                    }),
                    Cell::from(status).style(Style::default().fg(color)),
                ])
            })
            .collect();
        let mut title = format!(" Containers: {running} running, {exited} exited ");
        let filter = &self.queries[NodeInfoTab::Containers].text;
        if !filter.is_empty() {
            title.push_str(&format!("· {} match \"{filter}\" ", visible.len()));
        }
        if let Some(e) = &error {
            title.push_str(&format!("· {e} "));
        }
        let widths = [
            Constraint::Length(20),
            Constraint::Percentage(28),
            Constraint::Percentage(18),
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(14),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_style(Style::default().fg(if error.is_some() {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    })),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(table, area, &mut self.containers_table);
        self.content_area = area;
    }

    fn draw_config(&mut self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };
        let Some(config) = &data.config else {
            match &data.config_error {
                Some(e) => Self::draw_error(frame, area, " Machine config ", e),
                None => frame.render_widget(
                    Paragraph::new("Loading machine config...")
                        .style(Style::default().fg(Color::DarkGray)),
                    area,
                ),
            }
            return;
        };
        let mut title = vec![Span::styled(
            format!(
                " Machine config, revision {}{} · {} secrets hidden ",
                config.version,
                config
                    .updated
                    .as_deref()
                    .map(|u| format!(" (updated {u})"))
                    .unwrap_or_default(),
                config.hidden
            ),
            Style::default().fg(Color::Cyan),
        )];
        if config.staged_differs {
            title.push(Span::styled(
                "· a staged change waits for a reboot ",
                Style::default().fg(Color::Yellow),
            ));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(title));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Outline on the left, the picked part on the right
        let sections = config.sections.clone();
        let label_width = sections
            .iter()
            .map(|s| s.label.chars().count() + 2 * s.depth as usize)
            .max()
            .unwrap_or(10);
        let outline_width =
            talos9s_core::convert::to_u16(label_width + 9).clamp(20, (inner.width / 3).max(20));
        let [outline_area, content_area] =
            Layout::horizontal([Constraint::Length(outline_width), Constraint::Min(10)])
                .areas(inner);
        let outline_block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(Color::DarkGray));
        let outline_inner = outline_block.inner(outline_area);
        frame.render_widget(outline_block, outline_area);
        let rows = outline_inner.height as usize;
        let selected = self.config_section.min(sections.len().saturating_sub(1));
        // Keep the picked entry in view (stored once the config is drawn)
        let mut offset = self.outline_offset;
        if selected < offset {
            offset = selected;
        } else if rows > 0 && selected >= offset + rows {
            offset = selected + 1 - rows;
        }
        let width = outline_inner.width as usize;
        let outline: Vec<Line> = sections
            .iter()
            .enumerate()
            .skip(offset)
            .take(rows)
            .map(|(i, section)| {
                // Size, not lines: a manifest can be one enormous line
                let count = short_size(
                    config
                        .lines
                        .get(section.start..section.end)
                        .unwrap_or_default()
                        .iter()
                        .map(|l| l.text.len() + 1)
                        .sum(),
                );
                let indent = "  ".repeat(section.depth as usize);
                let room = width.saturating_sub(indent.len() + count.len() + 2);
                let mut label: String = section.label.chars().take(room).collect();
                if section.label.chars().count() > room && room > 0 {
                    label.pop();
                    label.push('…');
                }
                let gap = width.saturating_sub(indent.len() + label.chars().count() + count.len());
                let style = match (i == selected, self.config_in_content) {
                    (true, false) => Style::default().add_modifier(Modifier::REVERSED),
                    (true, true) => Style::default().fg(Color::Cyan).bold(),
                    (false, _) if section.depth == 0 => Style::default().fg(Color::Magenta),
                    _ => Style::default(),
                };
                Line::from(vec![
                    Span::styled(format!("{indent}{label}"), style),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(count, Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(outline), outline_inner);

        let content = Rect {
            x: content_area.x + 1,
            width: content_area.width.saturating_sub(1),
            ..content_area
        };
        let page = content.height as usize;
        let (start, end) = sections
            .get(selected)
            .map_or((0, config.lines.len()), |s| (s.start, s.end));
        let scroll = self
            .config_scroll
            .clamp(start, end.saturating_sub(1).max(start));
        let total = config.lines.len();
        let needle = self.queries[NodeInfoTab::Config].text.to_lowercase();
        let number_width = total.to_string().len();
        let lines: Vec<Line> = config
            .lines
            .get(..end)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .skip(scroll)
            .take(page)
            .map(|(i, line)| {
                let text: String = line.text.chars().skip(self.config_hscroll).collect();
                let mut spans = vec![Span::styled(
                    format!("{:>number_width$} ", i + 1),
                    Style::default().fg(Color::DarkGray),
                )];
                spans.extend(config_spans(&text, line.masked));
                let mut rendered = Line::from(spans);
                if !needle.is_empty() && line.text.to_lowercase().contains(&needle) {
                    rendered = rendered.style(Style::default().bg(Color::DarkGray));
                }
                rendered
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), content);
        self.content_area = content;
        self.outline_area = outline_inner;
        self.outline_offset = offset;
        self.config_scroll = scroll;
        self.config_page = page.max(1);
    }

    fn draw_extensions(&mut self, frame: &mut Frame, area: Rect) {
        let Some(data) = self.data() else {
            return;
        };
        if let (Some(e), true) = (&data.extensions_error, data.extensions.is_empty()) {
            Self::draw_error(frame, area, " Extensions ", e);
            return;
        }
        let total = data.extensions.len();
        let visible = self.visible_extensions();
        let selected = self
            .extensions_table
            .selected()
            .and_then(|i| visible.get(i))
            .map(|e| (*e).clone());
        let [table_area, detail_area] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(5)]).areas(area);
        let header = Row::new(
            ["NAME", "VERSION", "AUTHOR"]
                .map(|h| Cell::from(h).style(Style::default().add_modifier(Modifier::BOLD))),
        );
        let rows: Vec<Row> = visible
            .iter()
            .map(|e| {
                Row::new(vec![
                    Cell::from(e.name.clone()).style(Style::default().fg(Color::Green)),
                    Cell::from(e.version.clone()),
                    Cell::from(e.author.clone()),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(35),
                Constraint::Min(15),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Extensions: {total} installed "))
                .title_style(Style::default().fg(Color::Cyan)),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(table, table_area, &mut self.extensions_table);
        self.content_area = table_area;

        let detail = match selected {
            Some(e) => vec![
                Line::from(e.description),
                Line::from(Span::styled(
                    format!("image {}", e.image),
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            None => vec![Line::from("No extensions installed")],
        };
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(ratatui::widgets::Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Details ")
                        .title_style(Style::default().fg(Color::Yellow)),
                ),
            detail_area,
        );
    }

    fn key_hints_line(&self) -> Line<'static> {
        let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Cyan));
        let query = self.query();
        let spans = if query.editing {
            let what = if self.tab == NodeInfoTab::Config {
                "Search"
            } else {
                "Filter"
            };
            vec![
                Span::raw(format!(" {what}: ")),
                Span::styled(format!("{}▏", query.text), Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                key("Enter"),
                Span::raw(" done  "),
                key("Esc"),
                Span::raw(" clear"),
            ]
        } else {
            let mut spans = vec![
                key(" ↑/↓"),
                Span::raw(match (self.tab, self.config_in_content) {
                    (NodeInfoTab::Config, true) => " scroll  ",
                    (NodeInfoTab::Config, false) => " part  ",
                    _ => " navigate  ",
                }),
            ];
            if self.tab == NodeInfoTab::Config && !self.config_in_content {
                spans.extend([key("Enter/→"), Span::raw(" read  ")]);
            }
            if self.tab == NodeInfoTab::Config && self.config_in_content {
                spans.extend([
                    key("g/G"),
                    Span::raw(" top/bottom  "),
                    key("←/→"),
                    Span::raw(" pan  "),
                    key("Esc"),
                    Span::raw(" outline  "),
                ]);
            }
            spans.push(key("/"));
            if self.tab == NodeInfoTab::Config {
                spans.extend([
                    Span::raw(" search all  "),
                    key("n/N"),
                    Span::raw(" next/prev  "),
                ]);
            } else {
                spans.push(Span::raw(" filter  "));
            }
            spans.extend([key("r"), Span::raw(" refresh")]);
            spans
        };
        Line::from(spans)
    }
}

/// Containers in a stable order: system services, then Kubernetes ones by
/// pod and name
fn sort_containers(containers: &mut [ContainerInfo]) {
    containers.sort_by(|a, b| {
        let kind = |c: &ContainerInfo| c.kind == ContainerKind::Kubernetes;
        kind(a)
            .cmp(&kind(b))
            .then_with(|| a.pod.cmp(&b.pod))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// A config line colored: keys, comments, document separators, masked values
fn config_spans(text: &str, masked: bool) -> Vec<Span<'static>> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('#') {
        return vec![Span::styled(
            text.to_string(),
            Style::default().fg(Color::DarkGray),
        )];
    }
    if trimmed == "---" {
        return vec![Span::styled(
            text.to_string(),
            Style::default().fg(Color::Magenta),
        )];
    }
    let value_style = if masked {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    match text
        .split_once(": ")
        .or_else(|| text.strip_suffix(':').map(|k| (k, "")))
    {
        Some((key, value))
            if !key.trim_start().contains(' ') || key.trim_start().starts_with("- ") =>
        {
            let mut spans = vec![Span::styled(
                format!("{key}:"),
                Style::default().fg(Color::Cyan),
            )];
            if !value.is_empty() {
                spans.push(Span::styled(format!(" {value}"), value_style));
            }
            spans
        }
        _ => vec![Span::styled(text.to_string(), value_style)],
    }
}

impl Component for NodeInfoComponent {
    fn key_hints(&self) -> Option<Line<'static>> {
        Some(self.key_hints_line())
    }

    fn captures_text(&self) -> bool {
        self.query().editing
    }

    fn uses_horizontal_keys(&self) -> bool {
        // Scrolled config: Left scrolls back; at column 0 it leaves the view
        // The content uses Left (pan, back to the outline); from the outline
        // it leaves the view
        self.tab == NodeInfoTab::Config && self.config_in_content
    }

    /// Click a row to select it; the wheel scrolls
    fn handle_mouse_event(&mut self, mouse: MouseEvent) -> Result<Option<Action>> {
        let at = |r: &Rect| {
            mouse.column >= r.x
                && mouse.column < r.right()
                && mouse.row >= r.y
                && mouse.row < r.bottom()
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.tab == NodeInfoTab::Config {
                    // An outline row shows that part; the content takes the keys
                    if at(&self.outline_area) {
                        let row = self.outline_offset + (mouse.row - self.outline_area.y) as usize;
                        if row < self.config_sections().len() {
                            self.select_section(row);
                        }
                        self.config_in_content = false;
                    } else if at(&self.content_area) {
                        self.config_in_content = true;
                    }
                    return Ok(None);
                }
                // Table rows start below the border and the header
                let area = self.content_area;
                if !at(&area) || mouse.row < area.y + 2 {
                    return Ok(None);
                }
                let (table, len) = match self.tab {
                    NodeInfoTab::Containers => {
                        let len = self.visible_containers().len();
                        (&mut self.containers_table, len)
                    }
                    _ => {
                        let len = self.visible_extensions().len();
                        (&mut self.extensions_table, len)
                    }
                };
                let row = table.offset() + (mouse.row - area.y - 2) as usize;
                if row < len {
                    table.select(Some(row));
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // The wheel scrolls the config pane under the pointer
                if self.tab == NodeInfoTab::Config {
                    self.config_in_content = !at(&self.outline_area);
                }
                let step = if self.tab == NodeInfoTab::Config && !self.config_in_content {
                    1
                } else {
                    3
                };
                self.move_by(if mouse.kind == MouseEventKind::ScrollUp {
                    -step
                } else {
                    step
                });
            }
            _ => {}
        }
        Ok(None)
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Result<Option<Action>> {
        if self.query().editing {
            use crate::ui_ext::{TextEdit, edit_text};
            match edit_text(&mut self.query_mut().text, &key) {
                TextEdit::Submit => {
                    self.query_mut().editing = false;
                    if self.tab == NodeInfoTab::Config {
                        self.search_config(true);
                    }
                }
                TextEdit::Cancel => *self.query_mut() = Query::default(),
                TextEdit::Changed | TextEdit::Ignored => {}
            }
            // The list shrinks as the filter narrows
            self.containers_table.select(Some(0));
            self.extensions_table.select(Some(0));
            return Ok(None);
        }
        let page = isize::try_from(self.config_page).unwrap_or(isize::MAX);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if !self.query().text.is_empty() {
                    *self.query_mut() = Query::default();
                } else if self.tab == NodeInfoTab::Config && self.config_in_content {
                    // Back to the outline first
                    self.config_in_content = false;
                    self.config_hscroll = 0;
                } else {
                    return Ok(Some(Action::Back));
                }
            }
            KeyCode::Enter if self.tab == NodeInfoTab::Config => {
                self.config_in_content = true;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            // In the content Left pans back, then returns to the outline;
            // Right opens the picked part, then pans
            KeyCode::Left if self.tab == NodeInfoTab::Config => {
                if self.config_hscroll > 0 {
                    self.config_hscroll = self.config_hscroll.saturating_sub(8);
                } else {
                    self.config_in_content = false;
                }
            }
            KeyCode::Right if self.tab == NodeInfoTab::Config => {
                if self.config_in_content {
                    self.config_hscroll += 8;
                } else {
                    self.config_in_content = true;
                }
            }
            KeyCode::Char('/') => {
                *self.query_mut() = Query {
                    text: String::new(),
                    editing: true,
                };
            }
            KeyCode::Char('n') if self.tab == NodeInfoTab::Config => self.search_config(true),
            KeyCode::Char('N') if self.tab == NodeInfoTab::Config => self.search_config(false),
            KeyCode::Char('r') => {
                self.reload_all = true;
                return Ok(Some(Action::Refresh));
            }
            _ => {}
        }
        Ok(None)
    }

    fn update(&mut self, action: Action) -> Result<Option<Action>> {
        if let Action::Tick = action {
            // The first refresh loaded only the open tab; load the rest now
            if self
                .data()
                .is_some_and(|d| d.attempted.iter().any(|a| *a) && !d.attempted.iter().all(|a| *a))
                && !self.state.is_loading()
            {
                return Ok(Some(Action::Refresh));
            }
            let interval = Duration::from_secs(AUTO_REFRESH_INTERVAL_SECS);
            if self.state.should_auto_refresh(self.auto_refresh, interval) {
                return Ok(Some(Action::Refresh));
            }
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> Result<()> {
        if let Some(err) = self.state.error() {
            frame.render_widget(
                Paragraph::new(format!("Error: {err}")).style(Style::default().fg(Color::Red)),
                area,
            );
            return Ok(());
        }
        // Containers, Machine config and Extensions are separate menu
        // entries: no tab bar, and the window title names the node
        let attempted = self.data().is_some_and(|d| d.attempted[self.tab]);
        if !attempted {
            let what = match self.tab {
                NodeInfoTab::Containers => "containers",
                NodeInfoTab::Config => "machine config",
                NodeInfoTab::Extensions => "extensions",
            };
            let waited = self.opened.elapsed().as_secs();
            let mut text = format!("Loading {what} of {}...", self.hostname);
            // talosctl sends the whole config: with inline manifests that
            // takes a while (up to 2 minutes before giving up)
            if self.tab == NodeInfoTab::Config && waited >= 3 {
                text.push_str(&format!(
                    " {waited}s (a large config takes a while; it is kept for this session)"
                ));
            }
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
                area,
            );
        } else {
            match self.tab {
                NodeInfoTab::Containers => self.draw_containers(frame, area),
                NodeInfoTab::Config => self.draw_config(frame, area),
                NodeInfoTab::Extensions => self.draw_extensions(frame, area),
            }
        }
        Ok(())
    }
}

impl crate::components::BackgroundRefresh for NodeInfoComponent {
    fn adopt_refresh(&mut self, refreshed: Self) {
        self.state = refreshed.state;
        // A reload asked for during the refresh still runs next time
        self.reload_all &= refreshed.reload_all;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(text: &str) -> (Vec<String>, usize) {
        let (lines, hidden) = mask_config(text);
        (lines.into_iter().map(|l| l.text).collect(), hidden)
    }

    /// Documents, their sections and each named list item (inline manifests,
    /// files) become outline entries
    #[test]
    fn outlines_documents_sections_and_named_items() {
        let mut text = String::from(
            "version: v1alpha1\ndebug: false\nmachine:\n    type: controlplane\n    network:\n        hostname: bc1\n    files:\n        - content: x\n          path: /var/etc/a.conf\ncluster:\n    clusterName: bc\n    inlineManifests:\n        - name: cilium\n          contents: |\n",
        );
        for i in 0..40 {
            text.push_str(&format!("            line{i}: v\n"));
        }
        text.push_str("        - name: extra\n          contents: |\n            a: b\n            ---\n            c: d\n---\napiVersion: v1alpha1\nkind: KubeSpanEndpointsConfig\nextraAnnouncedEndpoints:\n    - 1.2.3.4:51820\n");
        let (lines, _) = mask_config(&text);
        let outline = config_outline(&lines);
        let labels: Vec<(u8, &str)> = outline
            .iter()
            .map(|s| (s.depth, s.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [
                (0, "v1alpha1"),
                (1, "machine"),
                (2, "machine › network"),
                (2, "machine › files"),
                (3, "/var/etc/a.conf"),
                (1, "cluster"),
                (2, "cluster › inlineManifests"),
                (3, "cilium"),
                (3, "extra"),
                (0, "KubeSpanEndpointsConfig"),
            ]
        );
        // The cilium manifest spans its lines, up to the next item
        let cilium = &outline[7];
        assert!(lines[cilium.start].text.contains("name: cilium"));
        assert!(lines[cilium.end].text.contains("name: extra"));
    }

    #[test]
    fn masks_secret_values_and_keeps_the_rest() {
        let config = "\
version: v1alpha1
machine:
    type: controlplane
    token: abc.def
    ca:
        crt: LS0tPUBLIC
        key: LS0tPRIVATE
    certSANs:
        - 10.0.0.1
cluster:
    secretboxEncryptionSecret: c2VjcmV0
    aggregatorCA:
        crt: LS0tPUBLIC
    serviceAccount:
        key: |
            -----BEGIN EC PRIVATE KEY-----
            MHcCAQEEIOK
            -----END EC PRIVATE KEY-----
    clusterName: bc
";
        let (lines, hidden) = masked(config);
        let text = lines.join("\n");
        for secret in [
            "abc.def",
            "LS0tPRIVATE",
            "c2VjcmV0",
            "MHcCAQEEIOK",
            "PRIVATE KEY",
        ] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(text.contains("    token: ••••••"));
        assert!(text.contains("        key: ••••••  (3 lines hidden)"));
        assert!(text.contains("crt: LS0tPUBLIC"));
        assert!(text.contains("- 10.0.0.1"));
        assert!(text.contains("    clusterName: bc"));
        assert_eq!(hidden, 4);
    }

    #[test]
    fn masks_secrets_in_inline_manifests_and_files() {
        let config = "\
    inlineManifests:
        - name: creds
          contents: |
            apiVersion: v1
            kind: Secret
            metadata:
              name: hcloud
            stringData:
              token: hc-123
              network: net-1
            ---
            kind: ConfigMap
            data:
              ccm.yaml: plain
    files:
        - content: |
            [plugins.\"io.containerd.grpc.v1.cri\".registry.configs.\"r\".auth]
              password = \"hunter2\"
              username = \"me\"
          path: /etc/cri/conf.d/20-customization.part
---
apiVersion: v1alpha1
kind: RegistryAuthConfig
name: ghcr.io
username: me
password: hunter2
auth: bWU6aHVudGVyMg==
";
        let (lines, _) = masked(config);
        let text = lines.join("\n");
        for secret in ["hc-123", "net-1", "hunter2", "bWU6aHVudGVyMg==", "plain"] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(text.contains("            stringData: ••••••  (2 lines hidden)"));
        assert!(text.contains("              password = ••••••"));
        assert!(text.contains("              username = \"me\""));
        assert!(text.contains("username: me"));
        assert!(text.contains("path: /etc/cri/conf.d/20-customization.part"));
        assert!(text.contains("kind: RegistryAuthConfig"));
    }

    /// Each case: secrets that must not appear in the masked text
    fn assert_hidden(config: &str, secrets: &[&str]) {
        let (lines, _) = masked(config);
        let text = lines.join("\n");
        for secret in secrets {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
    }

    #[test]
    fn masks_registry_auth_in_single_line_json() {
        // A dockerconfigjson file written by machine.files, on one line
        assert_hidden(
            r#"    files:
        - content: |
            {"auths":{"ghcr.io":{"username":"me","password":"hunter2","auth":"bWU6aHVudGVyMg=="}}}
          path: /var/lib/kubelet/config.json
"#,
            &["hunter2", "bWU6aHVudGVyMg=="],
        );
        // As a JSON value of a YAML key
        assert_hidden(
            r#"    .dockerconfigjson: {"auths":{"r":{"auth":"c2VjcmV0"}}}
"#,
            &["c2VjcmV0"],
        );
    }

    #[test]
    fn masks_pgp_private_key_blocks() {
        assert_hidden(
            "    files:
        - content: |
            -----BEGIN PGP PRIVATE KEY BLOCK-----

            lQOYBGPGPSECRET
            =abcd
            -----END PGP PRIVATE KEY BLOCK-----
          path: /var/etc/signing.asc
",
            &["lQOYBGPGPSECRET", "=abcd"],
        );
    }

    #[test]
    fn masks_credentials_in_urls() {
        assert_hidden(
            "machine:
    registries:
        mirrors:
            docker.io:
                endpoints:
                    - https://robot:s3cr3t@mirror.example.com
    env:
        https_proxy: http://proxyuser:pr0xyp4ss@proxy:3128
",
            &["s3cr3t", "pr0xyp4ss"],
        );
        let (lines, _) = masked(
            "    - https://robot:s3cr3t@mirror.example.com/v2\n    - https://mirror.example.com:5000/v2\n",
        );
        assert_eq!(lines[0], "    - https://robot:••••••@mirror.example.com/v2");
        assert_eq!(lines[1], "    - https://mirror.example.com:5000/v2");
    }

    #[test]
    fn masks_secrets_in_flow_maps() {
        assert_hidden(
            "    auth: {username: me, password: hunter2}
    registry: {username: me, password: hunter3}
    creds: {token: tk-123}
    - {name: x, secret: s-456}
",
            &["hunter2", "hunter3", "tk-123", "s-456"],
        );
        // Non-ASCII text and unterminated strings neither panic nor leak
        assert_hidden(
            "    {ключ: значение, password: пароль}\n    {\"token\": \"abc\n    https://ü:pässwört@höst/\n",
            &["пароль", "abc", "pässwört"],
        );
        let (lines, _) = masked(
            "    registry: {username: me, password: hunter3}\n    {\"name\": \"a\", \"auth\": \"x\\\"y\"}\n",
        );
        assert_eq!(lines[0], "    registry: {username: me, password: ••••••}");
        assert_eq!(lines[1], "    {\"name\": \"a\", \"auth\": \"••••••\"}");
    }

    #[test]
    fn plain_text_is_unchanged() {
        let config = "# comment: with colon\nmachine:\n    install:\n        disk: /dev/sda\n";
        let (lines, hidden) = masked(config);
        assert_eq!(lines.join("\n") + "\n", config);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn orders_system_containers_first() {
        let container = |kind, pod: &str, name: &str| ContainerInfo {
            kind,
            id: name.to_string(),
            name: name.to_string(),
            pod: pod.to_string(),
            sandbox: false,
            image: String::new(),
            pid: 1,
            status: "RUNNING".to_string(),
        };
        let mut list = vec![
            container(ContainerKind::Kubernetes, "kube-system/b", "x"),
            container(ContainerKind::System, "", "trustd"),
            container(ContainerKind::Kubernetes, "kube-system/a", "y"),
            container(ContainerKind::System, "", "apid"),
        ];
        sort_containers(&mut list);
        let names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["apid", "trustd", "y", "x"]);
    }
}
