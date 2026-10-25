//! Single-keystroke yes/no prompt for git's post-command path.
//!
//! Hand-rolled on crossterm rather than cliclack: it must give up after a
//! deadline so a prompt can never wedge a `git` invocation, and cliclack
//! prompts have no timeout. It is drawn in the same theme.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};

use super::theme;

/// How long a yes/no prompt waits for a keystroke before giving up and taking
/// the default. Callers run these from git's post-command path, so a prompt
/// must never be able to wedge a `git` invocation indefinitely.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

/// Restores the terminal even if a hand-drawn prompt errors out part-way.
pub(super) struct RawModeGuard;

impl RawModeGuard {
    pub(super) fn enable() -> Option<Self> {
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

/// Yes/no prompt in the style of a shell framework's update prompt
/// (`[Y/n]`), answered with a single keystroke: `y`/`n`, Enter for the
/// default, Esc or Ctrl-C to decline.
///
/// Returns `default` without reading stdin when stdin or stderr isn't a
/// terminal, when raw mode can't be enabled, or when nothing is typed within
/// [`CONFIRM_TIMEOUT`]. Unlike [`super::select`], a decline never exits the
/// process: this runs after git has already succeeded, so a non-zero exit
/// here would misreport git's status.
pub fn confirm(question: &str, default: bool) -> bool {
    confirm_with_timeout(question, default, CONFIRM_TIMEOUT)
}

fn confirm_with_timeout(question: &str, default: bool, timeout: Duration) -> bool {
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return default;
    }
    let Some(guard) = RawModeGuard::enable() else {
        return default;
    };

    let caps = theme::caps();
    let glyphs = caps.glyphs();
    let mut err = std::io::stderr();
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    if write!(
        err,
        "{}  {} {}",
        caps.accent(glyphs.active),
        caps.paint("1", question),
        caps.paint("2", hint)
    )
    .is_err()
        || err.flush().is_err()
    {
        drop(guard);
        return default;
    }

    let deadline = Instant::now() + timeout;
    let mut answer = None;
    let mut timed_out = false;
    while answer.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        match event::poll(remaining) {
            Ok(true) => {}
            Ok(false) => {
                timed_out = true;
                break;
            }
            Err(_) => break,
        }
        let Ok(Event::Key(key)) = event::read() else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        answer = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') => Some(false),
            KeyCode::Enter => Some(default),
            KeyCode::Esc => Some(false),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(false),
            _ => None,
        };
    }

    let answer = answer.unwrap_or(default);
    // Collapse the in-progress question into a single answered line.
    let _ = execute!(
        err,
        cursor::MoveToColumn(0),
        terminal::Clear(ClearType::CurrentLine)
    );
    drop(guard);

    eprintln!("{}", answered_line(&caps, question, answer, timed_out));
    answer
}

fn answered_line(caps: &theme::TermCaps, question: &str, answer: bool, timed_out: bool) -> String {
    let glyphs = caps.glyphs();
    let (mark, label) = if answer {
        (caps.paint("32", glyphs.submit), "yes")
    } else {
        (caps.paint("2", glyphs.submit), "no")
    };
    let suffix = if timed_out {
        format!(
            " {}",
            caps.paint("2", &format!("(no answer, assuming {label})"))
        )
    } else {
        String::new()
    };
    format!(
        "{mark}  {} {}{suffix}",
        caps.paint("1", question),
        caps.paint("2", &format!("· {label}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Charset, ColorLevel, TermCaps};

    #[test]
    fn confirm_returns_default_when_not_a_terminal() {
        assert!(confirm_with_timeout("go?", true, Duration::from_millis(10)));
        assert!(!confirm_with_timeout(
            "go?",
            false,
            Duration::from_millis(10)
        ));
    }

    #[test]
    fn answered_line_collapses_to_one_line() {
        let caps = TermCaps {
            tty: true,
            color: ColorLevel::None,
            charset: Charset::Unicode,
            width: 80,
        };
        assert_eq!(
            answered_line(&caps, "Update autter now?", true, false),
            "◇  Update autter now? · yes"
        );
        assert_eq!(
            answered_line(&caps, "Update autter now?", false, true),
            "◇  Update autter now? · no (no answer, assuming no)"
        );
    }
}
