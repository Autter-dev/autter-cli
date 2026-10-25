//! Progress spinner whose outcome lines follow the stream rules: progress
//! and outcomes go to stderr for a person at a terminal, while piped stdout
//! keeps the plain one-line-per-step results scripts already read.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use indicatif::{ProgressBar, ProgressStyle};

use super::theme;
use crate::commands::arg_parser::paint;

/// Whether spinners currently hang off a flow's rail (see [`on_rail`]).
static ON_RAIL: AtomicBool = AtomicBool::new(false);

/// While the returned guard lives, spinners and their stderr outcome lines
/// are indented under the step rail, so a flow that runs another command's
/// work (onboarding installing hooks) keeps one continuous rail.
pub fn on_rail() -> RailGuard {
    ON_RAIL.store(true, Ordering::Relaxed);
    RailGuard
}

pub struct RailGuard;

impl Drop for RailGuard {
    fn drop(&mut self) {
        ON_RAIL.store(false, Ordering::Relaxed);
    }
}

fn rail_prefix(caps: &theme::TermCaps) -> String {
    if ON_RAIL.load(Ordering::Relaxed) {
        format!("{}  ", caps.paint("90", caps.glyphs().bar))
    } else {
        String::new()
    }
}

/// Spinner UI component for showing progress
pub struct Spinner {
    pb: ProgressBar,
}

impl Spinner {
    pub fn new(message: &str) -> Self {
        Self::indented(message, 0)
    }

    /// A spinner drawn `indent` columns in, to sit under a heading.
    pub fn indented(message: &str, indent: usize) -> Self {
        // indicatif draws on stderr and hides itself when that isn't a terminal.
        let caps = theme::caps();
        let template = format!(
            "{}{}{{spinner:.green}} {{msg}}",
            rail_prefix(&caps),
            " ".repeat(indent)
        );
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template(&template)
                .unwrap()
                .tick_strings(caps.glyphs().spinner),
        );
        pb.set_message(message.to_string());
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        Self { pb }
    }

    pub fn start(&self) {
        // Spinner starts automatically when created
    }

    #[allow(dead_code)]
    pub fn update_message(&self, message: &str) {
        self.pb.set_message(message.to_string());
    }

    #[allow(dead_code)]
    pub async fn wait_for(&self, duration_ms: u64) {
        smol::Timer::after(std::time::Duration::from_millis(duration_ms)).await;
    }

    /// Clear the spinner without printing a status line, for callers that
    /// print their own outcome.
    pub fn stop(&self) {
        self.pb.finish_and_clear();
    }

    pub fn success(&self, message: &str) {
        self.finish(Outcome::Success, message);
    }

    pub fn pending(&self, message: &str) {
        self.finish(Outcome::Pending, message);
    }

    pub fn error(&self, message: &str) {
        self.finish(Outcome::Error, message);
    }

    #[allow(dead_code)]
    pub fn skipped(&self, message: &str) {
        self.finish(Outcome::Skipped, message);
    }

    fn finish(&self, outcome: Outcome, message: &str) {
        self.pb.finish_and_clear();
        let caps = theme::caps();
        let stdout_tty = std::io::stdout().is_terminal();
        if caps.tty {
            let glyph = outcome.glyph(caps.glyphs());
            eprintln!(
                "{}{}",
                rail_prefix(&caps),
                caps.paint(outcome.sgr(), &format!("{glyph} {message}"))
            );
        }
        // Unchanged stdout line for anything reading it: piped stdout, or a
        // terminal on stdout when stderr is redirected.
        // Its glyphs stay Unicode whatever the terminal, as they always were.
        if !(stdout_tty && caps.tty) {
            let glyph = outcome.glyph(&theme::UNICODE_GLYPHS);
            println!("{}", paint(outcome.sgr(), &format!("{glyph} {message}")));
        }
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Success,
    Pending,
    Error,
    Skipped,
}

impl Outcome {
    fn glyph(self, glyphs: &theme::Glyphs) -> &'static str {
        match self {
            Outcome::Success => glyphs.ok,
            Outcome::Pending => glyphs.warn,
            Outcome::Error => glyphs.fail,
            Outcome::Skipped => glyphs.skip,
        }
    }

    fn sgr(self) -> &'static str {
        match self {
            Outcome::Success => "1;32",
            Outcome::Pending => "1;33",
            Outcome::Error => "1;31",
            Outcome::Skipped => "90",
        }
    }
}
