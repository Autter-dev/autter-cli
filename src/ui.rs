//! Small interactive terminal prompts shared by user-facing commands.
//!
//! Hand-rolled on crossterm (already a dependency of the `autter log` pager)
//! rather than pulling in a prompt crate: all we need is a single-choice
//! arrow-key selector with graceful fallbacks. Everything renders on stderr
//! so command output on stdout stays scriptable.

use std::io::{IsTerminal, Write};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};

pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const CYAN: &str = "\x1b[36m";
pub const GREEN: &str = "\x1b[32m";
pub const RESET: &str = "\x1b[0m";

/// One selectable row: a short label plus a dimmed one-line explanation.
pub struct SelectItem {
    pub label: String,
    pub hint: String,
}

impl SelectItem {
    pub fn new(label: &str, hint: &str) -> Self {
        Self {
            label: label.to_string(),
            hint: hint.to_string(),
        }
    }
}

/// Ask the user to pick one of `items` with the arrow keys (j/k and 1-9 also
/// work), confirming with Enter. Returns the chosen index.
///
/// Falls back to a numbered "type 1-N" prompt when raw mode can't be enabled,
/// and to `default` when stdin/stderr aren't terminals — callers that already
/// gate on `is_terminal()` keep their existing non-interactive behavior.
pub fn select(question: &str, items: &[SelectItem], default: usize) -> usize {
    if items.is_empty() {
        return 0;
    }
    let default = default.min(items.len() - 1);
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return default;
    }
    match select_raw(question, items, default) {
        Some(choice) => choice,
        None => select_numbered(question, items, default),
    }
}

/// Restores the terminal even if the selector errors out part-way.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Option<Self> {
        terminal::enable_raw_mode().ok()?;
        Some(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stderr(), cursor::Show);
    }
}

/// The arrow-key selector. Returns `None` when raw mode is unavailable or a
/// terminal write fails, so the caller can fall back to line input.
fn select_raw(question: &str, items: &[SelectItem], default: usize) -> Option<usize> {
    let guard = RawModeGuard::enable()?;
    let mut err = std::io::stderr();
    execute!(err, cursor::Hide).ok()?;

    // In raw mode `\n` no longer implies a carriage return, so lines are
    // written explicitly with `\r\n`.
    write!(err, "{BOLD}{question}{RESET}\r\n").ok()?;

    // Each item renders as two lines (label + hint), plus one key-hint footer.
    let block_lines = (items.len() * 2 + 1) as u16;
    let mut selected = default;
    draw_items(&mut err, items, selected).ok()?;

    loop {
        let key = match event::read().ok()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                key
            }
            _ => continue,
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    items.len() - 1
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                selected = (selected + 1) % items.len();
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = c as usize - '1' as usize;
                if idx < items.len() {
                    selected = idx;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                execute!(
                    err,
                    cursor::MoveUp(block_lines + 1),
                    terminal::Clear(ClearType::FromCursorDown)
                )
                .ok()?;
                drop(guard);
                eprintln!("Cancelled.");
                std::process::exit(130);
            }
            _ => {}
        }
        execute!(
            err,
            cursor::MoveUp(block_lines),
            terminal::Clear(ClearType::FromCursorDown)
        )
        .ok()?;
        draw_items(&mut err, items, selected).ok()?;
    }

    // Collapse the whole block (question included) into one confirmation line.
    execute!(
        err,
        cursor::MoveUp(block_lines + 1),
        terminal::Clear(ClearType::FromCursorDown)
    )
    .ok()?;
    drop(guard);
    eprintln!(
        "{GREEN}✓{RESET} {BOLD}{question}{RESET} {DIM}·{RESET} {}",
        items[selected].label
    );
    Some(selected)
}

fn draw_items(err: &mut impl Write, items: &[SelectItem], selected: usize) -> std::io::Result<()> {
    for (i, item) in items.iter().enumerate() {
        if i == selected {
            write!(err, "  {CYAN}{BOLD}❯ {}{RESET}\r\n", item.label)?;
        } else {
            write!(err, "    {}\r\n", item.label)?;
        }
        write!(err, "      {DIM}{}{RESET}\r\n", item.hint)?;
    }
    write!(err, "  {DIM}↑/↓ move · enter confirm{RESET}\r\n")?;
    err.flush()
}

/// Line-input fallback for terminals where raw mode isn't available.
fn select_numbered(question: &str, items: &[SelectItem], default: usize) -> usize {
    eprintln!("{BOLD}{question}{RESET}");
    for (i, item) in items.iter().enumerate() {
        eprintln!("  {}) {} {DIM}— {}{RESET}", i + 1, item.label, item.hint);
    }
    eprint!("Choose [1-{}] (default {}): ", items.len(), default + 1);
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return default;
    }
    match input.trim().parse::<usize>() {
        Ok(n) if (1..=items.len()).contains(&n) => n - 1,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_returns_default_when_not_a_terminal() {
        // Under `cargo test` stdin is not a terminal, so this exercises the
        // non-interactive path.
        let items = [
            SelectItem::new("a", "first"),
            SelectItem::new("b", "second"),
        ];
        assert_eq!(select("pick", &items, 1), 1);
    }

    #[test]
    fn select_clamps_default_and_handles_empty() {
        let items = [SelectItem::new("only", "one")];
        assert_eq!(select("pick", &items, 5), 0);
        assert_eq!(select("pick", &[], 3), 0);
    }
}
