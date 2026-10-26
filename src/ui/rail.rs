//! The left rail that ties a multi-step flow together, in the same layout as
//! the cliclack prompts between them: finished steps as `◇  …`, notes as
//! `│  …`, warnings as `▲  …`, and the closing `└  …`.
//!
//! Each function renders for the given [`TermCaps`] so flows can be tested;
//! the `print_*` forms write to stderr for the current terminal.

use super::theme::{self, TermCaps};

/// A finished step: `◇  text`, with a dimmed counter such as `2/4` after it.
pub fn step(caps: &TermCaps, text: &str, counter: Option<&str>) -> String {
    let glyphs = caps.glyphs();
    match counter {
        Some(counter) => format!(
            "{}  {text}  {}\n",
            caps.paint("32", glyphs.submit),
            caps.paint("2", counter)
        ),
        None => format!("{}  {text}\n", caps.paint("32", glyphs.submit)),
    }
}

/// Dimmed lines hanging off the rail: `│  text`. Multi-line text keeps the
/// rail on every line.
pub fn note(caps: &TermCaps, text: &str) -> String {
    let bar = caps.paint("90", caps.glyphs().bar);
    text.lines()
        .map(|line| {
            if line.is_empty() {
                format!("{bar}\n")
            } else {
                format!("{bar}  {}\n", caps.paint("2", line))
            }
        })
        .collect()
}

/// An empty rail line, to separate groups.
pub fn gap(caps: &TermCaps) -> String {
    format!("{}\n", caps.paint("90", caps.glyphs().bar))
}

/// A step that did not complete: `▲  text` in yellow.
pub fn warn(caps: &TermCaps, text: &str) -> String {
    format!(
        "{}  {}\n",
        caps.paint("33", caps.glyphs().error),
        caps.paint("33", text)
    )
}

/// The closing line: `└  text`.
pub fn end(caps: &TermCaps, text: &str) -> String {
    format!("{}  {text}\n", caps.paint("90", caps.glyphs().bar_end))
}

/// A prompt title with its dimmed step counter, e.g. `Sign in  2/4`.
pub fn titled(caps: &TermCaps, title: &str, counter: Option<&str>) -> String {
    match counter {
        Some(counter) => format!("{title}  {}", caps.paint("2", counter)),
        None => title.to_string(),
    }
}

pub fn print_step(text: &str, counter: Option<&str>) {
    eprint!("{}", step(&theme::caps(), text, counter));
}

pub fn print_note(text: &str) {
    eprint!("{}", note(&theme::caps(), text));
}

pub fn print_gap() {
    eprint!("{}", gap(&theme::caps()));
}

pub fn print_warn(text: &str) {
    eprint!("{}", warn(&theme::caps(), text));
}

pub fn print_end(text: &str) {
    eprint!("{}", end(&theme::caps(), text));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Charset, ColorLevel};

    fn plain(charset: Charset) -> TermCaps {
        TermCaps {
            tty: true,
            color: ColorLevel::None,
            charset,
            width: 80,
        }
    }

    #[test]
    fn renders_the_rail_in_unicode() {
        let caps = plain(Charset::Unicode);
        let text = [
            step(&caps, "Signed in as you@company.com", Some("2/4")),
            note(&caps, "First line\n\nThird line"),
            warn(&caps, "Sign-in didn't finish"),
            end(&caps, "You're all set!"),
        ]
        .concat();
        assert_eq!(
            text,
            "◇  Signed in as you@company.com  2/4\n\
             │  First line\n\
             │\n\
             │  Third line\n\
             ▲  Sign-in didn't finish\n\
             └  You're all set!\n"
        );
    }

    #[test]
    fn falls_back_to_ascii() {
        let caps = plain(Charset::Legacy);
        assert_eq!(step(&caps, "Done", None), "o  Done\n");
        assert_eq!(note(&caps, "Hi"), "|  Hi\n");
        assert_eq!(end(&caps, "Bye"), "\\  Bye\n");
    }

    #[test]
    fn counter_is_optional() {
        let caps = plain(Charset::Unicode);
        assert_eq!(titled(&caps, "Sign in", Some("2/4")), "Sign in  2/4");
        assert_eq!(titled(&caps, "Sign in", None), "Sign in");
    }
}
