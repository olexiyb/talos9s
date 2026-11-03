//! The namespace picker (`W`): warp a view into namespaces, or all
//!
//! Lists all namespaces (`0`), the recent warps (`1`-`4` pick them), then the
//! namespaces the view has, with how many of its items each holds. Typing
//! filters; a filter ending in `*` is a prefix and offered as a warp
//! (`kolmena-staging*`). `Space` marks several namespaces, Enter warps into
//! the marked ones or the selected entry, Esc cancels. `↑`/`↓` move:
//! letters type.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};
use talos9s_core::namespaces::{PickerEntry, picker_entries};

/// What a key did in the picker
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerEvent {
    Stay,
    Close,
    /// Warp into these namespaces and prefixes (empty: all)
    Pick(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct NamespacePicker {
    counts: Vec<(String, usize)>,
    recent: Vec<String>,
    /// The warp now, marked
    current: Vec<String>,
    /// What the counts count, e.g. `pods`
    what: &'static str,
    filter: String,
    selected: usize,
    /// Namespaces marked with `Space`
    marked: Vec<String>,
}

impl NamespacePicker {
    pub fn new(
        counts: Vec<(String, usize)>,
        recent: Vec<String>,
        current: Vec<String>,
        what: &'static str,
    ) -> Self {
        let mut picker = Self {
            counts,
            recent,
            current,
            what,
            filter: String::new(),
            selected: 0,
            marked: Vec::new(),
        };
        // Start on the warp now
        picker.selected = picker
            .entries()
            .iter()
            .position(|e| e.warp == picker.current)
            .unwrap_or(0);
        picker
    }

    fn entries(&self) -> Vec<PickerEntry> {
        picker_entries(&self.counts, &self.recent, &self.filter)
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> PickerEvent {
        use crate::ui_ext::{TextEdit, edit_text};
        let entries = self.entries();
        match key.code {
            KeyCode::Esc => return PickerEvent::Close,
            KeyCode::Enter => {
                if !self.marked.is_empty() {
                    let mut marked = self.marked.clone();
                    marked.sort();
                    return PickerEvent::Pick(marked);
                }
                return entries
                    .get(self.selected)
                    .map_or(PickerEvent::Close, |e| PickerEvent::Pick(e.warp.clone()));
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(entries.len().saturating_sub(1));
            }
            // Mark one namespace, or unmark; on "all", unmark them all
            KeyCode::Char(' ') => match entries.get(self.selected) {
                Some(e) if e.warp.is_empty() => self.marked.clear(),
                Some(e) => {
                    if let Some(ns) = e.single() {
                        if let Some(i) = self.marked.iter().position(|m| m == ns) {
                            self.marked.remove(i);
                        } else {
                            self.marked.push(ns.to_string());
                        }
                        self.selected = (self.selected + 1).min(entries.len().saturating_sub(1));
                    }
                }
                None => {}
            },
            // Before typing, `0` is all namespaces and `1`-`4` a recent warp
            KeyCode::Char('0') if self.filter.is_empty() => return PickerEvent::Pick(Vec::new()),
            KeyCode::Char(c @ '1'..='4') if self.filter.is_empty() => {
                let n = usize::from(u8::try_from(c).unwrap_or(b'1') - b'0');
                if let Some(e) = entries.iter().find(|e| e.recent == Some(n)) {
                    return PickerEvent::Pick(e.warp.clone());
                }
            }
            _ => {
                if matches!(edit_text(&mut self.filter, key), TextEdit::Changed) {
                    self.selected = 0;
                }
            }
        }
        PickerEvent::Stay
    }

    pub fn key_hints(&self) -> Line<'static> {
        let key = |k: &'static str| Span::styled(k, crate::ui_ext::KEY);
        Line::from(vec![
            Span::raw("type to filter ("),
            key("*"),
            Span::raw(" prefix)  "),
            key("<↑/↓>"),
            Span::raw(" select  "),
            key("<Space>"),
            Span::raw(" mark  "),
            key("<Enter>"),
            Span::raw(" warp  "),
            key("<0-4>"),
            Span::raw(" all/recent  "),
            key("<Esc>"),
            Span::raw(" cancel"),
        ])
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let entries = self.entries();
        let width = 60.min(area.width);
        let height = talos9s_core::convert::to_u16(entries.len() + 3)
            .clamp(5, 22)
            .min(area.height);
        let popup = crate::ui_ext::centered(area, width, height);
        frame.render_widget(Clear, popup);
        let filter = if self.filter.is_empty() {
            Span::styled(" type to filter, * for a prefix ", Style::default().dim())
        } else {
            Span::styled(
                format!(" /{}█ ", self.filter),
                Style::default().fg(Color::Cyan),
            )
        };
        let mut title = vec![Span::styled(
            " Warp to namespaces ",
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if !self.marked.is_empty() {
            title.push(Span::styled(
                format!("· {} marked, Enter warps ", self.marked.len()),
                Style::default().fg(Color::Magenta),
            ));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Line::from(title))
            .title_bottom(Line::from(filter));
        // Borders, the pointer, the digit and the box take 9; the count 12
        let name_width = usize::from(width.saturating_sub(21));
        let singular = self.what.strip_suffix('s').unwrap_or(self.what);
        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| {
                let digit = match e.recent {
                    Some(n) => Span::styled(format!("{n} "), crate::ui_ext::KEY),
                    None if e.warp.is_empty() => Span::styled("0 ", crate::ui_ext::KEY),
                    None => Span::raw("  "),
                };
                let marked = e
                    .single()
                    .is_some_and(|ns| self.marked.iter().any(|m| m == ns));
                let current = e.warp == self.current;
                // Only single namespaces can be marked; the current warp
                // shows by its name in green
                let mark = if e.single().is_some() {
                    crate::ui_ext::check_box(marked)
                } else {
                    Span::raw("    ")
                };
                let name = if e.warp.is_empty() {
                    "all namespaces".to_string()
                } else {
                    e.warp.join(", ")
                };
                let name: String = name.chars().take(name_width).collect();
                let count = e.count.map_or_else(String::new, |c| {
                    talos9s_core::pluralize(c, singular, self.what)
                });
                let style = if e.warp.is_empty() || e.warp.iter().any(|w| w.ends_with('*')) {
                    Style::default().add_modifier(Modifier::ITALIC)
                } else {
                    Style::default()
                };
                let style = if current {
                    style.fg(Color::Green)
                } else {
                    style
                };
                ListItem::new(Line::from(vec![
                    digit,
                    mark,
                    Span::styled(format!("{name:name_width$}"), style),
                    Span::styled(format!(" {count:>11}"), Style::default().dim()),
                ]))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selected));
        let list = List::new(items)
            .block(block)
            .highlight_symbol(crate::ui_ext::PICK_POINTER)
            .highlight_style(crate::ui_ext::PICK_CURSOR);
        frame.render_stateful_widget(list, popup, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(p: &mut NamespacePicker, code: KeyCode) -> PickerEvent {
        p.handle_key(&KeyEvent::from(code))
    }

    fn picker() -> NamespacePicker {
        let counts = vec![
            ("db".to_string(), 1),
            ("web".to_string(), 2),
            ("web2".to_string(), 1),
        ];
        NamespacePicker::new(counts, vec!["web".to_string()], vec!["web".into()], "pods")
    }

    /// Starts on the warp now; a digit picks a recent one; typing filters
    /// (then digits type); Enter warps into the selected one
    #[test]
    fn pick_recent_filter_and_enter() {
        let mut p = picker();
        assert_eq!(p.selected, 1);
        assert_eq!(
            press(&mut p, KeyCode::Char('1')),
            PickerEvent::Pick(vec!["web".into()])
        );
        assert_eq!(
            press(&mut p, KeyCode::Char('0')),
            PickerEvent::Pick(Vec::new()),
            "all"
        );
        for c in "web2".chars() {
            press(&mut p, KeyCode::Char(c));
        }
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            PickerEvent::Pick(vec!["web2".into()])
        );
        let mut p = NamespacePicker::new(Vec::new(), Vec::new(), vec!["x".into()], "pods");
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            PickerEvent::Pick(Vec::new()),
            "all"
        );
        assert_eq!(press(&mut p, KeyCode::Esc), PickerEvent::Close);
    }

    /// Space marks several, Enter warps into them; a `*` filter warps into
    /// the prefix
    #[test]
    fn mark_several_or_a_prefix() {
        let mut p = picker();
        // all, 1 web, db, web2: mark db and web2
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Char(' '));
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(p.marked, ["db", "web2"]);
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            PickerEvent::Pick(vec!["db".into(), "web2".into()])
        );
        let mut p = picker();
        for c in "we*".chars() {
            press(&mut p, KeyCode::Char(c));
        }
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            PickerEvent::Pick(vec!["we*".into()])
        );
    }
}
