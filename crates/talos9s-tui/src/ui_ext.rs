//! UI extensions for talos9s-core types
//!
//! Provides ratatui-specific extensions for core types, bridging the gap
//! between the UI-agnostic core library and the TUI presentation layer.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use talos9s_core::{
    CheckStatus, ConnectionState, HasHealth, HealthIndicator, QuorumState, SafetyStatus,
};

/// Draw `content` at its full height into a scroll view and show the part
/// of it that fits `area`, `scroll` lines down: a scrollable area for any
/// widgets. Returns how far it can scroll (0 when everything fits).
pub fn render_scrolled(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    content_height: u16,
    scroll: u16,
    content: impl FnOnce(&mut ratatui::buffer::Buffer, ratatui::layout::Rect),
) -> u16 {
    use ratatui::layout::{Position, Size};
    use tui_scrollview::{ScrollView, ScrollViewState, ScrollbarVisibility};
    let height = content_height.max(area.height);
    let max_scroll = height - area.height;
    if area.is_empty() {
        return max_scroll;
    }
    // Its own scrollbar has arrows and can't be restyled: drawn here instead,
    // like every other scrollbar
    let mut view = ScrollView::new(Size::new(area.width, height))
        .scrollbars_visibility(ScrollbarVisibility::Never);
    let full = view.area();
    content(view.buf_mut(), full);
    let scroll = scroll.min(max_scroll);
    let mut state = ScrollViewState::with_offset(Position::new(0, scroll));
    frame.render_stateful_widget(view, area, &mut state);
    // Over the border column right of the area, so no content width is lost
    draw_scrollbar(
        frame,
        ratatui::layout::Rect::new(area.x + area.width, area.y, 1, area.height),
        height,
        scroll,
    );
    max_scroll
}

/// A vertical scrollbar in `column` (a 1-wide area, usually the window's
/// right border) for content `content_height` tall scrolled `scroll` lines
/// down; nothing when the content fits
pub fn draw_scrollbar(
    frame: &mut ratatui::Frame,
    column: ratatui::layout::Rect,
    content_height: u16,
    scroll: u16,
) {
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
    if content_height <= column.height || column.height == 0 {
        return;
    }
    let mut state =
        ScrollbarState::new((content_height - column.height) as usize).position(scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None),
        column,
        &mut state,
    );
}

/// A `width`-cell bar filled to `pct` percent (clamped to 0..=100), for
/// usage shown inline in a line of text, where a `LineGauge` (which needs
/// its own area) doesn't fit
pub fn usage_bar(pct: f32, width: usize) -> String {
    let filled = talos9s_core::convert::to_usize(talos9s_core::convert::float_to_u64(
        f64::from((pct.clamp(0.0, 100.0) / 100.0) * width as f32).round(),
    ))
    .min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// The scroll offset that keeps line `selected` in a `height`-line view:
/// `scroll` unchanged when it's already visible, otherwise just far enough
/// to bring it to the top or bottom edge
pub fn scroll_into_view(scroll: usize, selected: usize, height: usize) -> usize {
    if selected < scroll {
        selected
    } else if selected >= scroll + height {
        (selected + 1).saturating_sub(height)
    } else {
        scroll
    }
}

/// Color of a load average for the node's core count (dim while unknown),
/// with the thresholds the dashboard, Processes and diagnostics share
pub fn load_color(load1: f64, cpu_count: Option<usize>) -> Color {
    HealthIndicatorExt::color(&talos9s_core::load_health(load1, cpu_count))
}

/// Seconds since the Unix epoch (0 if the clock is before it)
pub fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A spinner frame for "in progress" markers, advancing with wall time so
/// every spinner on screen turns together and nothing has to track ticks
pub fn spinner() -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES
        .get((millis / 100 % FRAMES.len() as u128) as usize)
        .copied()
        .unwrap_or(FRAMES[0])
}

/// A window title with a "refreshing" marker appended while `refreshing`
pub fn title_with_refresh(
    title: &str,
    refreshing: bool,
    color: Color,
) -> ratatui::text::Line<'static> {
    use ratatui::style::Style;
    use ratatui::text::Span;
    let mut spans = vec![Span::styled(
        format!(" {title} "),
        Style::default().fg(color),
    )];
    if refreshing {
        spans.push(Span::styled(
            format!("{} refreshing ", spinner()),
            Style::default().dim(),
        ));
    }
    ratatui::text::Line::from(spans)
}

/// Extension trait for HealthIndicator to provide ratatui colors
pub trait HealthIndicatorExt {
    /// Get the ratatui Color for this indicator
    fn color(&self) -> Color;

    /// Get symbol and color together (common pattern)
    fn symbol_and_color(&self) -> (&'static str, Color);
}

impl HealthIndicatorExt for HealthIndicator {
    fn color(&self) -> Color {
        match self {
            HealthIndicator::Healthy => Color::Green,
            HealthIndicator::Warning => Color::Yellow,
            HealthIndicator::Error => Color::Red,
            HealthIndicator::Pending => Color::Cyan,
            HealthIndicator::Info => Color::Blue,
            HealthIndicator::Unknown => Color::DarkGray,
        }
    }

    fn symbol_and_color(&self) -> (&'static str, Color) {
        (self.symbol(), self.color())
    }
}

/// Extension trait for QuorumState to provide ratatui colors
pub trait QuorumStateExt {
    /// Get indicator symbol and color
    fn indicator_with_color(&self) -> (&'static str, Color);

    /// Get display text and color
    fn display_with_color(&self) -> (&'static str, Color);
}

impl QuorumStateExt for QuorumState {
    fn indicator_with_color(&self) -> (&'static str, Color) {
        use talos9s_core::HasHealth;
        let health = self.health();
        (health.symbol(), health.color())
    }

    fn display_with_color(&self) -> (&'static str, Color) {
        let (text, _) = self.display();
        let color = match self {
            QuorumState::Healthy => Color::Green,
            QuorumState::Degraded { .. } => Color::Yellow,
            QuorumState::NoQuorum { .. } => Color::Red,
            QuorumState::Unknown => Color::DarkGray,
        };
        (text, color)
    }
}

/// Extension trait for SafetyStatus to provide ratatui colors
pub trait SafetyStatusExt {
    /// Get indicator symbol and color
    fn indicator_with_color(&self) -> (&'static str, Color);
}

impl SafetyStatusExt for SafetyStatus {
    fn indicator_with_color(&self) -> (&'static str, Color) {
        use talos9s_core::HasHealth;
        let health = self.health();
        (health.symbol(), health.color())
    }
}

/// Extension trait for ConnectionState to provide ratatui colors
pub trait ConnectionStateExt {
    /// Get the ratatui Color for this state
    fn color(&self) -> Color;

    /// Get symbol and color together
    fn symbol_and_color(&self) -> (&'static str, Color);
}

impl ConnectionStateExt for ConnectionState {
    fn color(&self) -> Color {
        match self {
            ConnectionState::Connected => Color::Green,
            ConnectionState::Partial => Color::Yellow,
            ConnectionState::Disconnected => Color::Red,
            ConnectionState::Unknown => Color::DarkGray,
        }
    }

    fn symbol_and_color(&self) -> (&'static str, Color) {
        (self.symbol(), self.color())
    }
}

/// Extension trait for CheckStatus to provide ratatui colors
pub trait CheckStatusExt {
    /// Get indicator symbol and color for this check status
    fn indicator(&self) -> (&'static str, Color);
}

impl CheckStatusExt for CheckStatus {
    fn indicator(&self) -> (&'static str, Color) {
        let health = self.health();
        (
            if matches!(self, CheckStatus::Checking) {
                "◌" // Special spinner symbol for checking state
            } else {
                health.symbol()
            },
            health.color(),
        )
    }
}

/// What a key did to a text field being typed into
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEdit {
    /// A char was added or removed
    Changed,
    /// Enter
    Submit,
    /// Esc
    Cancel,
    /// Any other key
    Ignored,
}

/// Apply a key to `text`: chars append, Backspace deletes, Enter and Esc
/// are reported for the view to act on
pub fn edit_text(text: &mut String, key: &crossterm::event::KeyEvent) -> TextEdit {
    use crossterm::event::KeyCode;
    match key.code {
        KeyCode::Char(c) => {
            text.push(c);
            TextEdit::Changed
        }
        KeyCode::Backspace => {
            text.pop();
            TextEdit::Changed
        }
        KeyCode::Enter => TextEdit::Submit,
        KeyCode::Esc => TextEdit::Cancel,
        _ => TextEdit::Ignored,
    }
}

/// A list cursor move
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Prev,
    Next,
    First,
    Last,
    /// Up (negative) or down by this many rows: page keys
    By(isize),
}

/// Rows `PageUp`/`PageDown` move in lists that don't track their height
pub const PAGE_ROWS: isize = 10;

impl Move {
    /// The move a list key means in every view: `j`/`k` and arrows,
    /// `g`/`G` and `Home`/`End`, `PageUp`/`PageDown`
    pub fn from_key(key: &crossterm::event::KeyEvent) -> Option<Self> {
        use crossterm::event::KeyCode;
        Some(match key.code {
            KeyCode::Up | KeyCode::Char('k') => Self::Prev,
            KeyCode::Down | KeyCode::Char('j') => Self::Next,
            KeyCode::Home | KeyCode::Char('g') => Self::First,
            KeyCode::End | KeyCode::Char('G') => Self::Last,
            KeyCode::PageUp => Self::By(-PAGE_ROWS),
            KeyCode::PageDown => Self::By(PAGE_ROWS),
            _ => return None,
        })
    }
}

/// The cursor after `step` in a list of `len`, kept inside it; `None` for
/// an empty list
pub fn moved(cursor: usize, len: usize, step: Move) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match step {
        Move::Prev => cursor.saturating_sub(1).min(last),
        Move::Next => (cursor + 1).min(last),
        Move::First => 0,
        Move::Last => last,
        Move::By(delta) => cursor.saturating_add_signed(delta).min(last),
    })
}

/// Draw the "Loading ..." or "Error: ..." placeholder a view shows instead of
/// its content; `true` when one was drawn and the view should stop there
pub fn draw_load_gate(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    loading: bool,
    error: Option<&str>,
    loading_text: &str,
) -> bool {
    use ratatui::style::{Color, Style};
    use ratatui::widgets::Paragraph;
    let placeholder = if loading {
        Paragraph::new(loading_text.to_string()).style(Style::default().fg(Color::DarkGray))
    } else if let Some(err) = error {
        Paragraph::new(format!("Error: {err}")).style(Style::default().fg(Color::Red))
    } else {
        return false;
    };
    frame.render_widget(placeholder, area);
    true
}

/// A `width` x `height` dialog centered in `area`, never larger than it
pub fn centered(area: ratatui::layout::Rect, width: u16, height: u16) -> ratatui::layout::Rect {
    use ratatui::layout::Constraint;
    area.centered(
        Constraint::Length(width.min(area.width)),
        Constraint::Length(height.min(area.height)),
    )
}

/// Clear and return the area of a `width` x `height` dialog centered in
/// `area`, leaving a margin of 2 cells around it on small screens
pub fn open_dialog(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    width: u16,
    height: u16,
) -> ratatui::layout::Rect {
    let dialog = centered(
        area,
        width.min(area.width.saturating_sub(4)),
        height.min(area.height.saturating_sub(4)),
    );
    frame.render_widget(ratatui::widgets::Clear, dialog);
    dialog
}

/// Is `mouse` a left button press?
pub fn is_left_click(mouse: &crossterm::event::MouseEvent) -> bool {
    matches!(
        mouse.kind,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
    )
}

/// Where a table or list was drawn, to turn a click into a row index.
/// Record it right after rendering, with the widget state's offset:
///
/// ```ignore
/// frame.render_stateful_widget(table, area, &mut self.table_state);
/// self.rows = RowHits::new(area, false, 2, self.table_state.offset(), len);
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowHits {
    /// Columns and lines the rows can be on
    rows_area: ratatui::layout::Rect,
    /// Index of the row on the first line (rows scrolled away)
    offset: usize,
    rows: usize,
}

impl RowHits {
    /// A widget drawn at `area`; `bordered` when its block has borders,
    /// `header` lines above the first row (header height plus its bottom
    /// margin), `offset` rows scrolled away, `rows` rows in all
    pub fn new(
        area: ratatui::layout::Rect,
        bordered: bool,
        header: u16,
        offset: usize,
        rows: usize,
    ) -> Self {
        let border = u16::from(bordered);
        let x = area.x + border;
        let y = area.y + border + header;
        let width = area.width.saturating_sub(2 * border);
        let height = area
            .height
            .saturating_sub(2 * border)
            .saturating_sub(header);
        Self {
            rows_area: ratatui::layout::Rect::new(x, y, width, height),
            offset,
            rows,
        }
    }

    /// The row a left click landed on
    pub fn clicked(&self, mouse: &crossterm::event::MouseEvent) -> Option<usize> {
        if !is_left_click(mouse) {
            return None;
        }
        self.row_at(mouse.column, mouse.row)
    }

    /// The row at a screen position
    pub fn row_at(&self, column: u16, line: u16) -> Option<usize> {
        let r = self.rows_area;
        if column < r.x || column >= r.right() || line < r.y || line >= r.bottom() {
            return None;
        }
        let index = self.offset + (line - r.y) as usize;
        (index < self.rows).then_some(index)
    }
}

/// A table's header line and what clicking each column does (e.g. the sort
/// it selects), to sort by clicking a column. Build it from the same
/// widths the table was drawn with, right after drawing:
///
/// ```ignore
/// self.header = ColumnHits::new(area, &widths, &[None, Some(Sort::Name)]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnHits<T> {
    /// The header line's row on screen
    line: u16,
    /// Each column's first screen column, width and target
    columns: Vec<(u16, u16, T)>,
}

impl<T> Default for ColumnHits<T> {
    fn default() -> Self {
        Self {
            line: u16::MAX,
            columns: Vec::new(),
        }
    }
}

impl<T: Copy> ColumnHits<T> {
    /// A `Table` (no border, no highlight symbol, column spacing 1) drawn at
    /// `area` with `widths`; `targets[i]` is what a click on column `i`
    /// does, `None` for nothing. The table lays its columns out the same way.
    pub fn new(
        area: ratatui::layout::Rect,
        widths: &[ratatui::layout::Constraint],
        targets: &[Option<T>],
    ) -> Self {
        let line = ratatui::layout::Rect { height: 1, ..area };
        let rects = ratatui::layout::Layout::horizontal(widths.iter().copied())
            .spacing(1)
            .split(line);
        let columns = rects
            .iter()
            .zip(targets)
            .filter_map(|(r, t)| t.map(|t| (r.x, r.width, t)))
            .collect();
        Self {
            line: area.y,
            columns,
        }
    }

    /// The target of the column a left click on the header landed on
    pub fn clicked(&self, mouse: &crossterm::event::MouseEvent) -> Option<T> {
        if !is_left_click(mouse) || mouse.row != self.line {
            return None;
        }
        self.columns
            .iter()
            .find(|(x, width, _)| mouse.column >= *x && mouse.column < x.saturating_add(*width))
            .map(|(_, _, t)| *t)
    }
}

/// Lines of a text list and what each one shows (an item, or nothing for a
/// heading or blank line), to turn a click into an item. Push one entry per
/// drawn line while building the lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHits<T> {
    area: ratatui::layout::Rect,
    scroll: usize,
    lines: Vec<Option<T>>,
}

impl<T> Default for LineHits<T> {
    fn default() -> Self {
        Self {
            area: ratatui::layout::Rect::default(),
            scroll: 0,
            lines: Vec::new(),
        }
    }
}

impl<T: Clone> LineHits<T> {
    /// Lines drawn at `area`, the first `scroll` of them scrolled away
    pub fn new(area: ratatui::layout::Rect, scroll: usize) -> Self {
        Self {
            area,
            scroll,
            lines: Vec::new(),
        }
    }

    /// The next line shows `item` (`None` for a heading or blank line)
    pub fn push(&mut self, item: Option<T>) {
        self.lines.push(item);
    }

    /// The item a left click landed on
    pub fn clicked(&self, mouse: &crossterm::event::MouseEvent) -> Option<T> {
        if !is_left_click(mouse) {
            return None;
        }
        let r = self.area;
        if mouse.column < r.x || mouse.column >= r.right() {
            return None;
        }
        if mouse.row < r.y || mouse.row >= r.bottom() {
            return None;
        }
        let line = self.scroll + (mouse.row - r.y) as usize;
        self.lines.get(line).cloned().flatten()
    }
}

/// A key in a hint line or dialog (`<r>`): one colour in every view
pub const KEY: Style = Style::new().fg(Color::Cyan);
/// The answer keys of a confirmation dialog: `<y>` goes ahead, `<n>` backs out
pub const KEY_YES: Style = Style::new().fg(Color::Green);
pub const KEY_NO: Style = Style::new().fg(Color::Red);

/// `line` cut to `width` columns with `…` at the end when it is wider, so a
/// narrow pane shows that something was cut ("Control Pl…")
pub fn ellipsize(line: Line<'_>, width: usize) -> Line<'_> {
    if line.width() <= width {
        return line;
    }
    let budget = width.saturating_sub(1);
    let style = line.style;
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let w = span.width();
        if used + w <= budget {
            used += w;
            spans.push(span);
            continue;
        }
        let mut text = String::new();
        for c in span.content.chars() {
            let cw = Span::raw(c.to_string()).width();
            if used + cw > budget {
                break;
            }
            used += cw;
            text.push(c);
        }
        spans.push(Span::styled(text, span.style));
        break;
    }
    spans.push(Span::raw("…"));
    Line::from(spans).style(style)
}

#[cfg(test)]
mod tests {
    #[test]
    fn scroll_into_view_moves_only_when_needed() {
        use super::scroll_into_view;
        assert_eq!(scroll_into_view(5, 7, 4), 5); // visible
        assert_eq!(scroll_into_view(5, 2, 4), 2); // above: to the top edge
        assert_eq!(scroll_into_view(5, 9, 4), 6); // below: to the bottom edge
        assert_eq!(scroll_into_view(5, 9, 0), 10); // no room: still in range
    }

    #[test]
    fn render_scrolled_shows_the_scrolled_part_and_a_bar_in_the_border() {
        use ratatui::widgets::Widget;
        use ratatui::{Terminal, backend::TestBackend, layout::Rect, widgets::Paragraph};
        let mut terminal = Terminal::new(TestBackend::new(6, 4)).unwrap();
        let mut max = 0;
        terminal
            .draw(|frame| {
                // 5 wide, plus the border column at x = 5
                max = super::render_scrolled(frame, Rect::new(0, 0, 5, 4), 10, 3, |buf, full| {
                    let lines: Vec<_> = (0..10)
                        .map(|i| format!("row{i}").into())
                        .collect::<Vec<ratatui::text::Line>>();
                    Paragraph::new(lines).render(full, buf);
                });
            })
            .unwrap();
        assert_eq!(max, 6);
        let buf = terminal.backend().buffer();
        let row = |y: u16| (0..5).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert_eq!(row(0), "row3 ");
        assert_eq!(row(3), "row6 ");
        assert_ne!(buf[(5, 0)].symbol(), " ", "scrollbar in the border column");
        assert_ne!(buf[(5, 0)].symbol(), "▲", "no arrows, like draw_scrollbar");

        // Content that fits: no scrolling, the border column untouched
        let mut terminal = Terminal::new(TestBackend::new(6, 4)).unwrap();
        terminal
            .draw(|frame| {
                max = super::render_scrolled(frame, Rect::new(0, 0, 5, 4), 2, 3, |buf, full| {
                    Paragraph::new("a\nb").render(full, buf);
                });
            })
            .unwrap();
        assert_eq!(max, 0);
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "a");
        assert_eq!(terminal.backend().buffer()[(5, 0)].symbol(), " ");
    }

    #[test]
    fn usage_bar_fills_by_percent() {
        assert_eq!(super::usage_bar(0.0, 10), "░░░░░░░░░░");
        assert_eq!(super::usage_bar(54.0, 10), "█████░░░░░");
        assert_eq!(super::usage_bar(100.0, 4), "████");
        assert_eq!(super::usage_bar(250.0, 4), "████");
        assert_eq!(super::usage_bar(-5.0, 4), "░░░░");
    }

    use super::*;

    #[test]
    fn ellipsize_cuts_wide_lines_only() {
        let line = || Line::from(vec![Span::raw("  ▼ "), Span::raw("Control Plane (3)")]);
        assert_eq!(ellipsize(line(), 40).to_string(), "  ▼ Control Plane (3)");
        let cut = ellipsize(line(), 12);
        assert_eq!(cut.to_string(), "  ▼ Control…");
        assert_eq!(cut.width(), 12);
        assert_eq!(ellipsize(line(), 21).to_string(), "  ▼ Control Plane (3)");
    }

    #[test]
    fn cursor_moves_stay_in_the_list() {
        assert_eq!(moved(0, 3, Move::Prev), Some(0));
        assert_eq!(moved(1, 3, Move::Prev), Some(0));
        assert_eq!(moved(2, 3, Move::Next), Some(2));
        assert_eq!(moved(0, 3, Move::Last), Some(2));
        // A list that shrank under the cursor
        assert_eq!(moved(9, 3, Move::Prev), Some(2));
        assert_eq!(moved(0, 0, Move::Next), None);
        // Pages stop at the ends, no wrap-around
        assert_eq!(moved(5, 30, Move::By(-PAGE_ROWS)), Some(0));
        assert_eq!(moved(25, 30, Move::By(PAGE_ROWS)), Some(29));
    }

    #[test]
    fn list_keys_mean_the_same_move_everywhere() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let step = |code| Move::from_key(&KeyEvent::new(code, KeyModifiers::NONE));
        assert!(matches!(step(KeyCode::Char('j')), Some(Move::Next)));
        assert!(matches!(step(KeyCode::Up), Some(Move::Prev)));
        assert!(matches!(step(KeyCode::Char('G')), Some(Move::Last)));
        assert!(matches!(step(KeyCode::Home), Some(Move::First)));
        assert!(matches!(step(KeyCode::PageDown), Some(Move::By(PAGE_ROWS))));
        assert!(step(KeyCode::Char('x')).is_none());
    }

    #[test]
    fn dialogs_center_in_their_area_and_fit_it() {
        use ratatui::layout::Rect;
        // A pane that doesn't start at the screen's corner
        let pane = Rect::new(30, 2, 100, 40);
        assert_eq!(centered(pane, 50, 10), Rect::new(55, 17, 50, 10));
        assert_eq!(
            centered(Rect::new(0, 0, 20, 5), 50, 10),
            Rect::new(0, 0, 20, 5)
        );
    }

    #[test]
    fn test_health_indicator_colors() {
        assert_eq!(HealthIndicator::Healthy.color(), Color::Green);
        assert_eq!(HealthIndicator::Warning.color(), Color::Yellow);
        assert_eq!(HealthIndicator::Error.color(), Color::Red);
        assert_eq!(HealthIndicator::Unknown.color(), Color::DarkGray);
    }

    #[test]
    fn test_health_indicator_symbol_and_color() {
        let (symbol, color) = HealthIndicator::Healthy.symbol_and_color();
        assert_eq!(symbol, "●");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn test_quorum_state_colors() {
        let (_, color) = QuorumState::Healthy.display_with_color();
        assert_eq!(color, Color::Green);

        let (_, color) = QuorumState::NoQuorum {
            healthy: 1,
            total: 3,
        }
        .display_with_color();
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn clicks_map_to_rows_and_lines() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Bordered table, one header line: rows start at y + 2
        let hits = RowHits::new(Rect::new(10, 5, 30, 10), true, 1, 0, 3);
        assert_eq!(hits.clicked(&click(12, 6)), None); // header
        assert_eq!(hits.clicked(&click(12, 7)), Some(0));
        assert_eq!(hits.clicked(&click(12, 9)), Some(2));
        assert_eq!(hits.clicked(&click(12, 10)), None); // past the rows
        assert_eq!(hits.clicked(&click(10, 7)), None); // on the border
        // Scrolled: the first visible row is row 40
        let hits = RowHits::new(Rect::new(0, 0, 30, 10), false, 2, 40, 100);
        assert_eq!(hits.clicked(&click(1, 2)), Some(40));
        assert_eq!(hits.clicked(&click(1, 9)), Some(47));
        // A wheel event is no click
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            ..click(1, 2)
        };
        assert_eq!(hits.clicked(&wheel), None);

        let mut lines = LineHits::new(Rect::new(0, 0, 20, 5), 1);
        for item in [None, Some("a"), None, Some("b"), Some("c")] {
            lines.push(item);
        }
        // Line 0 is scrolled away: screen line 0 shows "a"
        assert_eq!(lines.clicked(&click(1, 0)), Some("a"));
        assert_eq!(lines.clicked(&click(1, 1)), None);
        assert_eq!(lines.clicked(&click(1, 3)), Some("c"));
        assert_eq!(lines.clicked(&click(1, 4)), None);
    }
}
