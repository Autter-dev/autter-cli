//! The brand header: the "a." logo drawn in block characters, with three
//! lines of context beside it. Shown at the top of commands that set autter
//! up or show the user's own setup — never on hot paths or piped output.

use std::path::Path;

use super::theme::{self, Charset, TermCaps};

/// Below this width the logo and text would wrap, so the header goes plain.
const MIN_WIDTH: u16 = 50;

/// Quadrant blocks for modern terminals.
const FULL_ART: [&str; 3] = ["▗▟▀▀▙▟▌", "▐▌  ▐█▌", "▝▜▄▄▛▜▌"];
/// Half and full blocks only, for fonts without quadrants (legacy conhost).
const LEGACY_ART: [&str; 3] = ["▄█▀▀█▄█", "█    ██", "▀█▄▄█▀█"];
const DOT: &str = "██";

pub struct Header {
    title: String,
    subtitle: String,
    lines: [String; 2],
}

impl Header {
    /// `title` renders bold and `subtitle` dimmed on the first line, beside
    /// the top of the logo; `line2` and `line3` are dimmed context lines.
    pub fn new(
        title: impl Into<String>,
        subtitle: impl Into<String>,
        line2: impl Into<String>,
        line3: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            subtitle: subtitle.into(),
            lines: [line2.into(), line3.into()],
        }
    }

    /// Print to stderr for the current terminal.
    pub fn print(&self) {
        eprint!("{}", self.render(&theme::caps()));
    }

    pub fn render(&self, caps: &TermCaps) -> String {
        let first = if self.subtitle.is_empty() {
            caps.paint("1", &self.title)
        } else {
            format!(
                "{} {}",
                caps.paint("1", &self.title),
                caps.paint("2", &self.subtitle)
            )
        };

        if !caps.tty || caps.width < MIN_WIDTH || caps.charset == Charset::Ascii {
            let mut out = format!("{first}\n");
            for line in self.lines.iter().filter(|line| !line.is_empty()) {
                out.push_str(&caps.paint("2", line));
                out.push('\n');
            }
            return out;
        }

        let art = if caps.charset == Charset::Unicode {
            FULL_ART
        } else {
            LEGACY_ART
        };
        let row = |left: String, text: &str| {
            if text.is_empty() {
                format!("  {}\n", left.trim_end())
            } else {
                format!("  {left}  {text}\n")
            }
        };
        let mut out = row(format!("{}   ", art[0]), &first);
        out.push_str(&row(
            format!("{}   ", art[1]),
            &caps.paint("2", &self.lines[0]),
        ));
        out.push_str(&row(
            format!("{} {}", art[2], caps.accent(DOT)),
            &caps.paint("2", &self.lines[1]),
        ));
        out
    }
}

/// The current directory for display, with the home directory as `~`.
pub fn display_cwd() -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return String::new();
    };
    display_path(&cwd, dirs::home_dir().as_deref())
}

fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = path.strip_prefix(home)
    {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::ColorLevel;

    fn header() -> Header {
        Header::new(
            "Autter",
            "v2.3.0",
            "Connected · you@company.com · acme",
            "~/code/app · capturing in 3 tools",
        )
    }

    fn caps(color: ColorLevel, charset: Charset, width: u16) -> TermCaps {
        TermCaps {
            tty: true,
            color,
            charset,
            width,
        }
    }

    #[test]
    fn full_truecolor() {
        insta::assert_snapshot!(header().render(&caps(
            ColorLevel::TrueColor,
            Charset::Unicode,
            120
        )));
    }

    #[test]
    fn full_256_color() {
        insta::assert_snapshot!(header().render(&caps(ColorLevel::Ansi256, Charset::Unicode, 120)));
    }

    #[test]
    fn full_16_color() {
        insta::assert_snapshot!(header().render(&caps(ColorLevel::Ansi16, Charset::Unicode, 120)));
    }

    #[test]
    fn full_without_color() {
        insta::assert_snapshot!(header().render(&caps(ColorLevel::None, Charset::Unicode, 120)));
    }

    #[test]
    fn legacy_console() {
        insta::assert_snapshot!(header().render(&caps(ColorLevel::None, Charset::Legacy, 120)));
    }

    #[test]
    fn narrow_terminal_is_plain() {
        insta::assert_snapshot!(header().render(&caps(ColorLevel::None, Charset::Unicode, 49)));
    }

    #[test]
    fn not_a_terminal_is_plain() {
        let mut caps = caps(ColorLevel::None, Charset::Unicode, 120);
        caps.tty = false;
        assert_eq!(
            header().render(&caps),
            "Autter v2.3.0\nConnected · you@company.com · acme\n~/code/app · capturing in 3 tools\n"
        );
    }

    #[test]
    fn empty_lines_leave_only_the_logo() {
        let text = Header::new("Autter", "", "", "").render(&caps(
            ColorLevel::None,
            Charset::Unicode,
            120,
        ));
        assert_eq!(text, "  ▗▟▀▀▙▟▌     Autter\n  ▐▌  ▐█▌\n  ▝▜▄▄▛▜▌ ██\n");
    }

    #[test]
    fn home_directory_shows_as_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(display_path(Path::new("/home/me"), Some(home)), "~");
        assert_eq!(
            display_path(&home.join("code").join("app"), Some(home)),
            format!("~{0}code{0}app", std::path::MAIN_SEPARATOR)
        );
        assert_eq!(display_path(Path::new("/srv/app"), Some(home)), "/srv/app");
        assert_eq!(display_path(Path::new("/srv/app"), None), "/srv/app");
    }
}
