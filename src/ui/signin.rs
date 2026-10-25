//! The browser sign-in screen: shows the one-time code and URL, then waits
//! for approval while a background poll runs. Enter reopens the browser, `c`
//! copies the code, Esc steps away. Hand-drawn on crossterm like `confirm`,
//! because it redraws a countdown while it waits.

use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};

use super::clipboard;
use super::confirm::RawModeGuard;
use super::theme::{self, TermCaps};

pub struct SigninScreen<'a> {
    /// Prompt title, already carrying its step counter.
    pub title: String,
    pub code: &'a str,
    pub url: &'a str,
    pub browser_opened: bool,
    pub expires_in: Duration,
    /// What Esc does, e.g. "continue in local mode" or "cancel".
    pub escape_label: &'a str,
}

pub enum SigninEnd<T> {
    Approved(T),
    /// The user pressed Esc.
    Escaped,
    Failed(String),
}

/// Show the screen and wait for the poll's result on `results`. `reopen`
/// opens the browser again and reports whether it could. The screen is
/// cleared when this returns, so the caller can print the outcome in its
/// place. Ctrl-C cancels the whole command (exit 130).
pub fn wait<T>(
    screen: &SigninScreen,
    results: Receiver<Result<T, String>>,
    reopen: impl Fn() -> bool,
) -> SigninEnd<T> {
    let caps = theme::caps();
    let lines = static_lines(screen, &caps);
    let Some(guard) = RawModeGuard::enable() else {
        return wait_without_keys(&lines, screen.expires_in, results);
    };

    let mut err = std::io::stderr();
    let _ = execute!(err, cursor::Hide);
    for line in &lines {
        let _ = write!(err, "{line}\r\n");
    }
    let width = terminal::size().map(|(cols, _)| cols).unwrap_or(caps.width);
    let rows: u16 = lines.iter().map(|line| screen_rows(line, width)).sum();

    let glyphs = caps.glyphs();
    let frames = &glyphs.spinner[..glyphs.spinner.len() - 1];
    let deadline = Instant::now() + screen.expires_in;
    let mut status = String::new();
    let mut tick = 0usize;

    let end = loop {
        match results.try_recv() {
            Ok(Ok(value)) => break SigninEnd::Approved(value),
            Ok(Err(error)) => break SigninEnd::Failed(error),
            Err(TryRecvError::Disconnected) => {
                break SigninEnd::Failed("sign-in stopped unexpectedly".to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break SigninEnd::Failed("the code expired".to_string());
        }

        let frame = frames[(tick / 2) % frames.len()];
        let live = waiting_line(&caps, frame, remaining, &status);
        let _ = execute!(
            err,
            cursor::MoveToColumn(0),
            terminal::Clear(ClearType::CurrentLine)
        );
        let _ = write!(err, "{live}");
        let _ = err.flush();
        tick += 1;

        if !event::poll(Duration::from_millis(100)).unwrap_or(false) {
            continue;
        }
        let Ok(Event::Key(key)) = event::read() else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                clear(&mut err, rows);
                drop(guard);
                eprint!("{}", super::rail::end(&caps, "Cancelled."));
                std::process::exit(130);
            }
            KeyCode::Enter => {
                status = if reopen() {
                    "browser reopened".to_string()
                } else {
                    "couldn't open the browser".to_string()
                };
            }
            KeyCode::Char('c') => {
                status = if clipboard::request_copy(screen.code) {
                    "copy requested".to_string()
                } else {
                    "couldn't copy".to_string()
                };
            }
            KeyCode::Esc => break SigninEnd::Escaped,
            _ => {}
        }
    };

    clear(&mut err, rows);
    drop(guard);
    end
}

/// Remove the live line and the static block above it.
fn clear(err: &mut std::io::Stderr, rows: u16) {
    let _ = execute!(
        err,
        cursor::MoveToColumn(0),
        terminal::Clear(ClearType::CurrentLine),
        cursor::MoveUp(rows),
        terminal::Clear(ClearType::FromCursorDown)
    );
}

/// Raw mode is unavailable: print the code and URL and wait for the poll,
/// with no keys and no countdown.
fn wait_without_keys<T>(
    lines: &[String],
    expires_in: Duration,
    results: Receiver<Result<T, String>>,
) -> SigninEnd<T> {
    // The last line lists keys that won't work here.
    for line in &lines[..lines.len() - 1] {
        eprintln!("{line}");
    }
    match results.recv_timeout(expires_in) {
        Ok(Ok(value)) => SigninEnd::Approved(value),
        Ok(Err(error)) => SigninEnd::Failed(error),
        Err(RecvTimeoutError::Timeout) => SigninEnd::Failed("the code expired".to_string()),
        Err(RecvTimeoutError::Disconnected) => {
            SigninEnd::Failed("sign-in stopped unexpectedly".to_string())
        }
    }
}

fn static_lines(screen: &SigninScreen, caps: &TermCaps) -> Vec<String> {
    let glyphs = caps.glyphs();
    let bar = caps.paint("90", glyphs.bar);
    let url = caps.paint("36", screen.url);
    let browser = if screen.browser_opened {
        format!("{bar}  Opened {url} in your browser")
    } else {
        format!("{bar}  Open {url} in your browser")
    };
    let keys = format!(
        "enter reopen browser \u{00B7} c copy code \u{00B7} esc {}",
        screen.escape_label
    );
    vec![
        format!("{}  {}", caps.accent(glyphs.active), screen.title),
        format!("{bar}  Code   {}", caps.paint("1", screen.code)),
        browser,
        bar.clone(),
        format!("{bar}  {}", caps.paint("2", &keys)),
    ]
}

fn waiting_line(caps: &TermCaps, frame: &str, remaining: Duration, status: &str) -> String {
    let mut line = format!(
        "{}  Waiting for approval\u{2026}  {}",
        caps.accent(frame),
        caps.paint("2", &format!("expires in {}", countdown(remaining)))
    );
    if !status.is_empty() {
        line.push_str(&caps.paint("2", &format!(" \u{00B7} {status}")));
    }
    line
}

/// `m:ss`, rounded up so the display never shows 0:00 while time remains.
fn countdown(remaining: Duration) -> String {
    let secs = remaining.as_millis().div_ceil(1000);
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// How many terminal rows `line` occupies at `width` columns, ignoring SGR
/// color sequences.
fn screen_rows(line: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let visible = visible_width(line);
    visible.div_ceil(width).max(1) as u16
}

fn visible_width(text: &str) -> usize {
    let mut count = 0;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip "ESC [ ... m".
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Charset, ColorLevel};

    fn caps(color: ColorLevel) -> TermCaps {
        TermCaps {
            tty: true,
            color,
            charset: Charset::Unicode,
            width: 80,
        }
    }

    fn screen(opened: bool) -> SigninScreen<'static> {
        SigninScreen {
            title: "Sign in  2/4".to_string(),
            code: "WDJB-MJHT",
            url: "https://app.autter.dev/cli/authorize?code=WDJB-MJHT",
            browser_opened: opened,
            expires_in: Duration::from_secs(900),
            escape_label: "continue in local mode",
        }
    }

    #[test]
    fn lays_out_code_url_and_keys() {
        assert_eq!(
            static_lines(&screen(true), &caps(ColorLevel::None)),
            [
                "◆  Sign in  2/4",
                "│  Code   WDJB-MJHT",
                "│  Opened https://app.autter.dev/cli/authorize?code=WDJB-MJHT in your browser",
                "│",
                "│  enter reopen browser · c copy code · esc continue in local mode",
            ]
        );
        assert_eq!(
            static_lines(&screen(false), &caps(ColorLevel::None))[2],
            "│  Open https://app.autter.dev/cli/authorize?code=WDJB-MJHT in your browser"
        );
    }

    #[test]
    fn waiting_line_shows_countdown_and_status() {
        let line = waiting_line(
            &caps(ColorLevel::None),
            "⠋",
            Duration::from_secs(872),
            "copy requested",
        );
        assert_eq!(
            line,
            "⠋  Waiting for approval…  expires in 14:32 · copy requested"
        );
    }

    #[test]
    fn countdown_rounds_up() {
        assert_eq!(countdown(Duration::from_millis(59_001)), "1:00");
        assert_eq!(countdown(Duration::from_millis(500)), "0:01");
        assert_eq!(countdown(Duration::from_secs(900)), "15:00");
    }

    #[test]
    fn rows_ignore_color_and_wrap_long_lines() {
        let colored = caps(ColorLevel::TrueColor).accent("abcd");
        assert_eq!(visible_width(&colored), 4);
        assert_eq!(screen_rows(&"x".repeat(80), 80), 1);
        assert_eq!(screen_rows(&"x".repeat(81), 80), 2);
        assert_eq!(screen_rows("", 80), 1);
    }
}
