//! The namespace picker (`W`): warp a view into one namespace, or all
//!
//! Lists all namespaces, the recent ones (`1`-`4` pick them), then the
//! others the view has, with how many of its items each holds. Typing
//! filters; `↑`/`↓` move (letters type), Enter warps, Esc cancels.

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
    /// Warp into this namespace (`None`: all)
    Pick(Option<String>),
}

#[derive(Debug, Clone)]
pub struct NamespacePicker {
    counts: Vec<(String, usize)>,
    recent: Vec<String>,
    /// The namespace warped into now, marked
    current: Option<String>,
    /// What the counts count, e.g. `pods`
    what: &'static str,
    filter: String,
    selected: usize,
}

impl NamespacePicker {
    pub fn new(
        counts: Vec<(String, usize)>,
        recent: Vec<String>,
        current: Option<String>,
        what: &'static str,
    ) -> Self {
        let mut picker = Self {
            counts,
            recent,
            current,
            what,
            filter: String::new(),
            selected: 0,
        };
        // Start on the namespace warped into
        picker.selected = picker
            .entries()
            .iter()
            .position(|e| e.namespace == picker.current)
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
                return entries.get(self.selected).map_or(PickerEvent::Close, |e| {
                    PickerEvent::Pick(e.namespace.clone())
                });
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(entries.len().saturating_sub(1));
            }
            // Before typing, a digit picks a recent namespace
            KeyCode::Char(c @ '1'..='4') if self.filter.is_empty() => {
                let n = usize::from(u8::try_from(c).unwrap_or(b'1') - b'0');
                if let Some(e) = entries.iter().find(|e| e.recent == Some(n)) {
                    return PickerEvent::Pick(e.namespace.clone());
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
            Span::raw("type to filter  "),
            key("<↑/↓>"),
            Span::raw(" select  "),
            key("<Enter>"),
            Span::raw(" warp  "),
            key("<1-4>"),
            Span::raw(" recent  "),
            key("<Esc>"),
            Span::raw(" cancel"),
        ])
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let entries = self.entries();
        let width = 56.min(area.width);
        let height = talos9s_core::convert::to_u16(entries.len() + 3)
            .clamp(5, 22)
            .min(area.height);
        let popup = crate::ui_ext::centered(area, width, height);
        frame.render_widget(Clear, popup);
        let filter = if self.filter.is_empty() {
            Span::styled(" type to filter ", Style::default().dim())
        } else {
            Span::styled(
                format!(" /{}█ ", self.filter),
                Style::default().fg(Color::Cyan),
            )
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Line::from(Span::styled(
                " Warp to namespace ",
                Style::default().add_modifier(Modifier::BOLD),
            )))
            .title_bottom(Line::from(filter));
        // Borders, the recent digit and the mark take 6; the count 12
        let name_width = usize::from(width.saturating_sub(18));
        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| {
                let current = e.namespace == self.current;
                let mark = match e.recent {
                    Some(n) => Span::styled(format!("{n} "), crate::ui_ext::KEY),
                    None => Span::raw("  "),
                };
                let name = e.namespace.as_deref().unwrap_or("all namespaces");
                let singular = self.what.strip_suffix('s').unwrap_or(self.what);
                let count = e.count.map_or_else(String::new, |c| {
                    talos9s_core::pluralize(c, singular, self.what)
                });
                let style = if e.namespace.is_none() {
                    Style::default().add_modifier(Modifier::ITALIC)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    mark,
                    Span::styled(
                        if current { "● " } else { "  " },
                        Style::default().fg(Color::Green),
                    ),
                    Span::styled(format!("{name:name_width$}"), style),
                    Span::styled(format!(" {count:>11}"), Style::default().dim()),
                ]))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selected));
        let list = List::new(items).block(block).highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(list, popup, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(p: &mut NamespacePicker, code: KeyCode) -> PickerEvent {
        p.handle_key(&KeyEvent::from(code))
    }

    /// Starts on the current warp; a digit picks a recent one; typing
    /// filters (then digits type); Enter warps into the selected one
    #[test]
    fn pick_recent_filter_and_enter() {
        let counts = vec![
            ("db".to_string(), 1),
            ("web".to_string(), 2),
            ("web2".to_string(), 1),
        ];
        let recent = vec!["web".to_string()];
        let mut p = NamespacePicker::new(counts, recent, Some("web".into()), "pods");
        assert_eq!(p.selected, 1);
        assert_eq!(
            press(&mut p, KeyCode::Char('1')),
            PickerEvent::Pick(Some("web".into()))
        );
        press(&mut p, KeyCode::Char('w'));
        press(&mut p, KeyCode::Char('e'));
        press(&mut p, KeyCode::Char('b'));
        press(&mut p, KeyCode::Char('2'));
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            PickerEvent::Pick(Some("web2".into()))
        );
        let mut p = NamespacePicker::new(Vec::new(), Vec::new(), Some("x".into()), "pods");
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            PickerEvent::Pick(None),
            "all"
        );
        assert_eq!(press(&mut p, KeyCode::Esc), PickerEvent::Close);
    }
}
