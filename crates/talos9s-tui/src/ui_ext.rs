//! UI extensions for talos9s-core types
//!
//! Provides ratatui-specific extensions for core types, bridging the gap
//! between the UI-agnostic core library and the TUI presentation layer.

use ratatui::style::Color;
use talos9s_core::{
    CheckStatus, ConnectionState, HasHealth, HealthIndicator, QuorumState, SafetyStatus,
};

/// Draw `content` at its full height into an off-screen buffer and show the
/// part of it that fits `area`, `scroll` lines down: a scrollable area for
/// any widgets. Returns how far it can scroll (0 when everything fits).
pub fn render_scrolled(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    content_height: u16,
    scroll: u16,
    content: impl FnOnce(&mut ratatui::buffer::Buffer, ratatui::layout::Rect),
) -> u16 {
    use ratatui::layout::{Position, Rect};
    let full = Rect::new(area.x, area.y, area.width, content_height.max(area.height));
    let mut buf = ratatui::buffer::Buffer::empty(full);
    content(&mut buf, full);
    let max_scroll = full.height - area.height;
    let scroll = scroll.min(max_scroll);
    let target = frame.buffer_mut();
    for y in 0..area.height {
        for x in 0..area.width {
            let src = buf.cell(Position::new(area.x + x, area.y + y + scroll));
            let dst = target.cell_mut(Position::new(area.x + x, area.y + y));
            if let (Some(src), Some(dst)) = (src, dst) {
                *dst = src.clone();
            }
        }
    }
    // Over the border column right of the area, so no content width is lost
    draw_scrollbar(
        frame,
        Rect::new(area.x + area.width, area.y, 1, area.height),
        full.height,
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

/// A spinner frame for "in progress" markers, advancing with wall time so
/// every spinner on screen turns together and nothing has to track ticks
pub fn spinner() -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES[(millis / 100 % FRAMES.len() as u128) as usize]
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

/// A list cursor move
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Prev,
    Next,
    First,
    Last,
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
    })
}

/// A `width` x `height` dialog centered in `area`, never larger than it
pub fn centered(area: ratatui::layout::Rect, width: u16, height: u16) -> ratatui::layout::Rect {
    use ratatui::layout::Constraint;
    area.centered(
        Constraint::Length(width.min(area.width)),
        Constraint::Length(height.min(area.height)),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_moves_stay_in_the_list() {
        assert_eq!(moved(0, 3, Move::Prev), Some(0));
        assert_eq!(moved(1, 3, Move::Prev), Some(0));
        assert_eq!(moved(2, 3, Move::Next), Some(2));
        assert_eq!(moved(0, 3, Move::Last), Some(2));
        // A list that shrank under the cursor
        assert_eq!(moved(9, 3, Move::Prev), Some(2));
        assert_eq!(moved(0, 0, Move::Next), None);
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
