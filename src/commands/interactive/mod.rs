//! Full-screen views built on ratatui: `autter stats -i` to browse recent
//! commits by AI share, and `autter blame -i <file>` to read a file with its
//! authorship and the prompt behind each AI line.
//!
//! They need a terminal on both stdin and stdout, take over the alternate
//! screen, and give it back on exit or panic (`ratatui::init` installs the
//! restore hook). The plain commands are unchanged.

pub mod blame;
pub mod stats;

use std::io::IsTerminal;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{DefaultTerminal, Frame};

use crate::ui::theme::{Charset, ColorLevel};

/// Whether a full-screen view can run here.
pub fn available() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// A full-screen view: drawn every frame, fed keys, and polled for
/// background results between frames.
pub(crate) trait View {
    fn draw(&mut self, frame: &mut Frame);
    /// Handle a key press. Returns `false` to close the view.
    fn on_key(&mut self, key: KeyEvent) -> bool;
    /// Pick up work finished in the background.
    fn tick(&mut self) {}
}

/// Run `view` until it closes (or Ctrl-C), restoring the terminal after.
pub(crate) fn run(view: &mut impl View) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, view);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, view: &mut impl View) -> std::io::Result<()> {
    loop {
        view.tick();
        terminal.draw(|frame| view.draw(frame))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if !view.on_key(key) {
            return Ok(());
        }
    }
}

/// The theme's colors and glyphs as ratatui styles. With color off (for
/// `NO_COLOR`, `--no-color`) everything is plain except the selection, which
/// stays visible in reverse video.
pub(crate) struct Palette {
    pub ai: Style,
    pub human: Style,
    pub untracked: Style,
    pub dim: Style,
    pub title: Style,
    pub selected: Style,
    /// Gutter mark beside each line in the blame view.
    pub gutter: &'static str,
    pub bar_full: &'static str,
    pub bar_ai: &'static str,
    pub bar_untracked: &'static str,
}

impl Palette {
    pub fn new() -> Self {
        let caps = crate::ui::caps();
        let unicode = caps.charset == Charset::Unicode;
        let (gutter, bar_full, bar_ai, bar_untracked) = if unicode {
            ("\u{258C}", "\u{2588}", "\u{2591}", "\u{00B7}")
        } else {
            ("|", "#", "=", ".")
        };
        if caps.color == ColorLevel::None {
            let plain = Style::default();
            return Self {
                ai: plain,
                human: plain,
                untracked: plain,
                dim: plain,
                title: plain.add_modifier(Modifier::BOLD),
                selected: plain.add_modifier(Modifier::REVERSED),
                gutter,
                bar_full,
                bar_ai,
                bar_untracked,
            };
        }
        let teal = match caps.color {
            ColorLevel::TrueColor => Color::Rgb(46, 230, 197),
            ColorLevel::Ansi256 => Color::Indexed(43),
            _ => Color::Cyan,
        };
        Self {
            ai: Style::default().fg(teal),
            human: Style::default(),
            untracked: Style::default().fg(Color::DarkGray),
            dim: Style::default().fg(Color::DarkGray),
            title: Style::default().add_modifier(Modifier::BOLD),
            selected: Style::default()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
            gutter,
            bar_full,
            bar_ai,
            bar_untracked,
        }
    }

    /// The key hints shown on the bottom row.
    pub fn help(&self, hints: &[(&str, &str)]) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        for (i, (key, what)) in hints.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" \u{00B7} ", self.dim));
            }
            spans.push(Span::styled(key.to_string(), self.title));
            spans.push(Span::styled(format!(" {what}"), self.dim));
        }
        Line::from(spans)
    }
}

/// "just now", "5 minutes ago", "3 days ago", "2 years ago".
pub(crate) fn ago(seconds: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let (n, unit) = match seconds {
        s if s < MINUTE => return "just now".to_string(),
        s if s < HOUR => (s / MINUTE, "minute"),
        s if s < DAY => (s / HOUR, "hour"),
        s if s < 30 * DAY => (s / DAY, "day"),
        s if s < 365 * DAY => (s / (30 * DAY), "month"),
        s => (s / (365 * DAY), "year"),
    };
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// Keep `selected` inside a window of `height` rows starting at `offset`.
pub(crate) fn scroll_to_keep_visible(offset: usize, selected: usize, height: usize) -> usize {
    let height = height.max(1);
    if selected < offset {
        selected
    } else if selected >= offset + height {
        selected + 1 - height
    } else {
        offset
    }
}

/// Move a cursor by `delta` within `0..len`.
pub(crate) fn step(selected: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as isize + delta).clamp(0, len as isize - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_reads_naturally() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(3 * 3600), "3 hours ago");
        assert_eq!(ago(86_400), "1 day ago");
        assert_eq!(ago(65 * 86_400), "2 months ago");
        assert_eq!(ago(800 * 86_400), "2 years ago");
    }

    #[test]
    fn scrolling_keeps_the_cursor_on_screen() {
        assert_eq!(scroll_to_keep_visible(0, 3, 10), 0);
        assert_eq!(scroll_to_keep_visible(0, 12, 10), 3);
        assert_eq!(scroll_to_keep_visible(5, 2, 10), 2);
        assert_eq!(scroll_to_keep_visible(0, 0, 0), 0);
    }

    #[test]
    fn cursor_stays_in_bounds() {
        assert_eq!(step(0, -1, 5), 0);
        assert_eq!(step(4, 1, 5), 4);
        assert_eq!(step(2, 10, 5), 4);
        assert_eq!(step(0, 1, 0), 0);
    }
}
