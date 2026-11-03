//! The help overlay (`?`): every key that works now, in three parts. The
//! view's own keys come from its `key_hints()` (the same line as at the
//! bottom, so the two can't disagree), then the tree's and the keys that
//! mean the same everywhere (`.claude/rules/key-bindings.md`).

use crate::components::cluster::NavMenuItem;
use crate::ui_ext::{KEY, KEY_NO, KEY_YES};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

/// The keys of a hint line as (key, what it does): a key span (`KEY`,
/// `KEY_YES`, `KEY_NO`) starts a pair, the spans after it are its label
pub fn hint_pairs(line: &Line<'_>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for span in &line.spans {
        let is_key = [KEY, KEY_YES, KEY_NO]
            .iter()
            .any(|s| s.fg.is_some() && s.fg == span.style.fg && span.content.contains('<'));
        if is_key {
            pairs.push((span.content.trim().to_string(), String::new()));
        } else if let Some((_, label)) = pairs.last_mut() {
            label.push_str(&span.content);
        }
    }
    for (_, label) in &mut pairs {
        *label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    pairs
}

/// The tree's keys
const TREE: &[(&str, &str)] = &[
    ("<j/k>", "move through nodes, groups and clusters"),
    ("<Enter/→>", "give the keyboard to the view"),
    ("<Space>", "fold a group"),
    ("<r>", "refresh"),
    ("<a>", "auto-refresh on/off"),
    ("<Esc/q>", "back to the dashboard; on it, quit"),
];

/// Keys that mean the same in every view
const EVERYWHERE: &[(&str, &str)] = &[
    ("<?>", "this help"),
    ("<Tab>", "next window (nodes, menu, view)"),
    (
        "<Esc/q/←>",
        "back: close a filter or dialog, then to the tree",
    ),
    ("<Z>", "hide the nodes pane"),
    ("<[> <]>", "narrower / wider nodes pane"),
    ("</>", "filter or search; n/N next/previous match"),
    ("<g/G>", "top / bottom"),
    ("<PgUp/PgDn>", "page up / down"),
    ("<y>", "copy"),
    ("<V>", "select lines (then j/k, y)"),
    ("<w>", "wrap long lines"),
    ("<A>", "show all / issues only"),
    ("<Ctrl+C>", "quit"),
    ("<Alt/Shift+drag>", "select text with the mouse"),
];

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(format!(" {title}"), Style::default().bold()))
}

fn pair_line(key: &str, label: &str, key_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("   {key:<key_width$}"), KEY),
        Span::styled(format!("  {label}"), Style::default().dim()),
    ])
}

/// The overlay's lines: the view's keys (when one is shown), the menu
/// hotkeys, the tree's and everywhere's
pub fn help_lines(view: Option<(&str, Vec<(String, String)>)>) -> Vec<Line<'static>> {
    let key_width = 16;
    let mut lines = Vec::new();
    if let Some((title, pairs)) = view {
        lines.push(section(&format!(
            "{title} (keys while it has the keyboard)"
        )));
        if pairs.is_empty() {
            lines.push(Line::from(Span::styled(
                "   no keys of its own",
                Style::default().dim(),
            )));
        }
        for (k, l) in &pairs {
            lines.push(pair_line(k, l, key_width));
        }
        lines.push(Line::default());
    }
    lines.push(section("Views (letters in the tree)"));
    // Four per line: key and name
    for chunk in NavMenuItem::ALL.chunks(4) {
        let mut spans = vec![Span::raw("   ")];
        for item in chunk {
            spans.push(Span::styled(format!("<{}>", item.hotkey()), KEY));
            spans.push(Span::styled(
                format!(" {:<16}", item.label()),
                Style::default().dim(),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines.push(section("Tree"));
    for (k, l) in TREE {
        lines.push(pair_line(k, l, key_width));
    }
    lines.push(Line::default());
    lines.push(section("Everywhere"));
    for (k, l) in EVERYWHERE {
        lines.push(pair_line(k, l, key_width));
    }
    lines
}

/// Draw the overlay over `area`, scrolled by `scroll` lines; returns the
/// largest useful scroll
pub fn draw(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, scroll: u16) -> u16 {
    let width = 92.min(area.width);
    let total = talos9s_core::convert::to_u16(lines.len());
    let height = (total + 3).min(area.height);
    let popup = crate::ui_ext::centered(area, width, height);
    let inner_height = height.saturating_sub(3);
    let max_scroll = total.saturating_sub(inner_height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(" Keys ", Style::default().bold()))
        .title_bottom(Line::from(vec![
            Span::styled(" <j/k>", KEY),
            Span::styled(" scroll  ", Style::default().dim()),
            Span::styled("<Esc/q/?>", KEY),
            Span::styled(" close ", Style::default().dim()),
        ]));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll.min(max_scroll), 0)),
        popup,
    );
    max_scroll
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_from_a_hint_line() {
        let line = Line::from(vec![
            Span::styled("<Enter>", KEY),
            Span::raw(" open  "),
            Span::styled("<y>", KEY_YES),
            Span::raw(" yes "),
            Span::raw("  more"),
            Span::styled("<n/Esc>", KEY_NO),
            Span::raw(" no"),
        ]);
        assert_eq!(
            hint_pairs(&line),
            [
                ("<Enter>".to_string(), "open".to_string()),
                ("<y>".to_string(), "yes more".to_string()),
                ("<n/Esc>".to_string(), "no".to_string()),
            ]
        );
    }

    #[test]
    fn lists_views_and_menu() {
        let text: String = help_lines(Some(("Logs", vec![("<w>".into(), "wrap".into())])))
            .iter()
            .map(|l| l.to_string() + "\n")
            .collect();
        assert!(text.contains("Logs (keys"));
        assert!(text.contains("<w>"));
        assert!(text.contains("<o> Operations"));
        assert!(text.contains("<?>"));
    }
}
