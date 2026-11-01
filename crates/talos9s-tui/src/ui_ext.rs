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

#[cfg(test)]
mod tests {
    use super::*;

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
}
