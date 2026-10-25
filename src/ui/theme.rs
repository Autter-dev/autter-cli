//! Terminal capabilities and the shared visual theme.
//!
//! Every interactive surface (prompts, spinners, the brand header) takes its
//! glyphs and colors from here, so autter looks the same everywhere and
//! degrades the same way: no color under `NO_COLOR`/`--no-color` or when
//! stderr isn't a terminal, and ASCII glyphs where Unicode won't render.

use std::io::IsTerminal;
use std::sync::{Once, OnceLock};

use cliclack::{Theme, ThemeState};
use console::Style;

/// The brand teal of the dot in the "a." logo.
const BRAND_TEAL: (u8, u8, u8) = (46, 230, 197);
/// Nearest xterm-256 color to [`BRAND_TEAL`].
const BRAND_TEAL_256: u8 = 43;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// Modern terminal: box drawing, geometric shapes and quadrant blocks.
    Unicode,
    /// Legacy Windows console or Linux VT: half and full blocks only, so
    /// glyphs fall back to ASCII and the logo uses its legacy variant.
    Legacy,
    /// No reliable Unicode at all.
    Ascii,
}

/// What the terminal behind stderr can display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermCaps {
    pub tty: bool,
    pub color: ColorLevel,
    pub charset: Charset,
    pub width: u16,
}

/// Status and prompt symbols for one [`Charset`].
pub struct Glyphs {
    pub active: &'static str,
    pub submit: &'static str,
    pub cancel: &'static str,
    pub error: &'static str,
    pub bar: &'static str,
    pub bar_end: &'static str,
    pub radio_on: &'static str,
    pub radio_off: &'static str,
    pub checkbox_on: &'static str,
    pub checkbox_off: &'static str,
    pub mask: char,
    pub ok: &'static str,
    pub warn: &'static str,
    pub fail: &'static str,
    pub skip: &'static str,
    /// Spinner frames; indicatif shows the last one when finished.
    pub spinner: &'static [&'static str],
}

pub(crate) const UNICODE_GLYPHS: Glyphs = Glyphs {
    active: "◆",
    submit: "◇",
    cancel: "■",
    error: "▲",
    bar: "│",
    bar_end: "└",
    radio_on: "●",
    radio_off: "○",
    checkbox_on: "◼",
    checkbox_off: "◻",
    mask: '▪',
    ok: "✓",
    warn: "⚠",
    fail: "✗",
    skip: "○",
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
};

const ASCII_GLYPHS: Glyphs = Glyphs {
    active: "*",
    submit: "o",
    cancel: "x",
    error: "!",
    bar: "|",
    bar_end: "\\",
    radio_on: ">",
    radio_off: " ",
    checkbox_on: "[x]",
    checkbox_off: "[ ]",
    mask: '*',
    ok: "OK",
    warn: "!",
    fail: "x",
    skip: "-",
    spinner: &["|", "/", "-", "\\", "|"],
};

impl TermCaps {
    /// Detect the capabilities of stderr in this process.
    pub fn detect() -> Self {
        let tty = std::io::stderr().is_terminal();
        let colors_on = crate::commands::arg_parser::use_color_stderr();
        let width = console::Term::stderr()
            .size_checked()
            .map(|(_, cols)| cols)
            .unwrap_or(80);
        Self::from_env(cfg!(windows), tty, colors_on, width, |key| {
            std::env::var(key).ok()
        })
    }

    /// Detection from explicit inputs, so every terminal can be tested.
    /// `colors_on` is the project-wide stderr color decision.
    pub(crate) fn from_env(
        windows: bool,
        tty: bool,
        colors_on: bool,
        width: u16,
        var: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let color = if colors_on {
            color_level(windows, &var)
        } else {
            ColorLevel::None
        };
        Self {
            tty,
            color,
            charset: charset(windows, &var),
            width,
        }
    }

    pub fn glyphs(&self) -> &'static Glyphs {
        match self.charset {
            Charset::Unicode => &UNICODE_GLYPHS,
            Charset::Legacy | Charset::Ascii => &ASCII_GLYPHS,
        }
    }

    /// Wrap `text` in an SGR sequence (e.g. `"1"` bold, `"2"` dim) when
    /// color is on; otherwise return it unchanged.
    pub fn paint(&self, sgr: &str, text: &str) -> String {
        if self.color == ColorLevel::None || text.is_empty() {
            text.to_string()
        } else {
            format!("\x1b[{sgr}m{text}\x1b[0m")
        }
    }

    /// Paint `text` in the brand teal, at the best depth the terminal has.
    pub fn accent(&self, text: &str) -> String {
        let (r, g, b) = BRAND_TEAL;
        match self.color {
            ColorLevel::None => text.to_string(),
            ColorLevel::Ansi16 => self.paint("36", text),
            ColorLevel::Ansi256 => self.paint(&format!("38;5;{BRAND_TEAL_256}"), text),
            ColorLevel::TrueColor => self.paint(&format!("38;2;{r};{g};{b}"), text),
        }
    }

    fn accent_style(&self) -> Style {
        let (r, g, b) = BRAND_TEAL;
        match self.color {
            ColorLevel::TrueColor => Style::new().true_color(r, g, b),
            ColorLevel::Ansi256 => Style::new().color256(BRAND_TEAL_256),
            ColorLevel::Ansi16 | ColorLevel::None => Style::new().cyan(),
        }
    }
}

fn color_level(windows: bool, var: &impl Fn(&str) -> Option<String>) -> ColorLevel {
    if matches!(var("COLORTERM").as_deref(), Some("truecolor" | "24bit")) {
        return ColorLevel::TrueColor;
    }
    // Every Windows console that passed the VT check behind `colors_on`
    // (Windows 10 1703+) renders 24-bit color.
    if windows {
        return ColorLevel::TrueColor;
    }
    if var("TERM").is_some_and(|term| term.contains("256color")) {
        ColorLevel::Ansi256
    } else {
        ColorLevel::Ansi16
    }
}

fn charset(windows: bool, var: &impl Fn(&str) -> Option<String>) -> Charset {
    if var("TERM").as_deref() == Some("dumb") {
        return Charset::Ascii;
    }
    if windows {
        // Windows Terminal sets WT_SESSION; other modern hosts (VS Code, the
        // Claude desktop panel) set TERM_PROGRAM or COLORTERM instead. Only a
        // console with none of them is the legacy conhost.
        let modern = var("WT_SESSION").is_some()
            || var("TERM_PROGRAM").is_some()
            || matches!(var("COLORTERM").as_deref(), Some("truecolor" | "24bit"));
        return if modern {
            Charset::Unicode
        } else {
            Charset::Legacy
        };
    }
    if var("TERM").as_deref() == Some("linux") {
        return Charset::Legacy;
    }
    // POSIX precedence: the first locale variable that is set decides.
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|key| var(key).filter(|value| !value.is_empty()));
    match locale {
        Some(locale)
            if !locale
                .to_ascii_uppercase()
                .replace('-', "")
                .contains("UTF8") =>
        {
            Charset::Ascii
        }
        _ => Charset::Unicode,
    }
}

/// The capabilities of stderr, detected once per process (after global flags
/// such as `--no-color` have been parsed).
pub fn caps() -> TermCaps {
    static CAPS: OnceLock<TermCaps> = OnceLock::new();
    *CAPS.get_or_init(TermCaps::detect)
}

/// Point cliclack (and the `console` crate under it) at autter's theme and
/// color decision. Called before every cliclack prompt; runs once.
pub(crate) fn install_prompt_theme() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let caps = caps();
        console::set_colors_enabled_stderr(caps.color != ColorLevel::None);
        console::set_true_colors_enabled_stderr(caps.color == ColorLevel::TrueColor);
        cliclack::set_theme(PromptTheme {
            glyphs: caps.glyphs(),
            accent: caps.accent_style(),
        });
    });
}

/// cliclack's clack layout with autter's glyphs and brand accent. cliclack
/// picks its own glyphs from `WT_SESSION` alone, which shows ASCII in modern
/// Windows hosts, so every method that draws a glyph is overridden here.
struct PromptTheme {
    glyphs: &'static Glyphs,
    accent: Style,
}

impl Theme for PromptTheme {
    fn bar_color(&self, state: &ThemeState) -> Style {
        match state {
            ThemeState::Active => self.accent.clone(),
            ThemeState::Cancel => Style::new().red(),
            ThemeState::Submit => Style::new().bright().black(),
            ThemeState::Error(_) => Style::new().yellow(),
        }
    }

    fn state_symbol(&self, state: &ThemeState) -> String {
        let symbol = match state {
            ThemeState::Active => self.glyphs.active,
            ThemeState::Cancel => self.glyphs.cancel,
            ThemeState::Submit => self.glyphs.submit,
            ThemeState::Error(_) => self.glyphs.error,
        };
        self.state_symbol_color(state).apply_to(symbol).to_string()
    }

    fn radio_symbol(&self, state: &ThemeState, selected: bool) -> String {
        match state {
            ThemeState::Active if selected => {
                self.accent.apply_to(self.glyphs.radio_on).to_string()
            }
            ThemeState::Active => Style::new()
                .dim()
                .apply_to(self.glyphs.radio_off)
                .to_string(),
            _ => String::new(),
        }
    }

    fn format_header(&self, state: &ThemeState, prompt: &str) -> String {
        let bar = self.bar_color(state).apply_to(self.glyphs.bar);
        prompt
            .lines()
            .enumerate()
            .map(|(i, line)| {
                if i == 0 {
                    format!("{}  {line}\n", self.state_symbol(state))
                } else {
                    format!("{bar}  {line}\n")
                }
            })
            .collect()
    }

    fn format_footer_with_message(&self, state: &ThemeState, message: &str) -> String {
        let (bar, end) = (self.glyphs.bar, self.glyphs.bar_end);
        let line = match state {
            ThemeState::Active => format!("{end}  {message}"),
            ThemeState::Cancel => format!("{end}  Cancelled."),
            ThemeState::Submit => bar.to_string(),
            ThemeState::Error(err) => format!("{end}  {err}"),
        };
        // Styling swallows a trailing newline, so it goes outside.
        format!("{}\n", self.bar_color(state).apply_to(line))
    }

    /// One row per option. Unlike clack, which shows the hint inline in
    /// parentheses, the focused option's hint gets its own dimmed line: our
    /// hints are full sentences.
    fn format_select_item(
        &self,
        state: &ThemeState,
        selected: bool,
        label: &str,
        hint: &str,
    ) -> String {
        if matches!(state, ThemeState::Cancel | ThemeState::Submit) && !selected {
            return String::new();
        }
        let bar = self.bar_color(state).apply_to(self.glyphs.bar);
        let mut row = format!("{bar}  {}\n", self.radio_item(state, selected, label, ""));
        if matches!(state, ThemeState::Active | ThemeState::Error(_))
            && selected
            && !hint.is_empty()
        {
            row.push_str(&format!("{bar}    {}\n", Style::new().dim().apply_to(hint)));
        }
        row
    }

    fn checkbox_symbol(&self, state: &ThemeState, selected: bool, active: bool) -> String {
        if !matches!(state, ThemeState::Active | ThemeState::Error(_)) {
            return String::new();
        }
        if selected {
            self.accent.apply_to(self.glyphs.checkbox_on).to_string()
        } else if active {
            self.glyphs.checkbox_off.to_string()
        } else {
            Style::new()
                .dim()
                .apply_to(self.glyphs.checkbox_off)
                .to_string()
        }
    }

    /// Same layout as [`Self::format_select_item`]: the focused row's hint
    /// goes on its own dimmed line.
    fn format_multiselect_item(
        &self,
        state: &ThemeState,
        selected: bool,
        active: bool,
        label: &str,
        hint: &str,
    ) -> String {
        if matches!(state, ThemeState::Cancel | ThemeState::Submit) && !selected {
            return String::new();
        }
        let bar = self.bar_color(state).apply_to(self.glyphs.bar);
        let mut row = format!(
            "{bar}  {}\n",
            self.checkbox_item(state, selected, active, label, "")
        );
        if matches!(state, ThemeState::Active | ThemeState::Error(_)) && active && !hint.is_empty()
        {
            let indent = " ".repeat(self.glyphs.checkbox_on.chars().count() + 3);
            row.push_str(&format!(
                "{bar}{indent}{}\n",
                Style::new().dim().apply_to(hint)
            ));
        }
        row
    }

    fn format_input(&self, state: &ThemeState, cursor: &cliclack::StringCursor) -> String {
        let style = self.input_style(state);
        let mut input = match state {
            ThemeState::Active | ThemeState::Error(_) => self.cursor_with_style(cursor, &style),
            _ => cursor.to_string(),
        };
        if input.ends_with('\n') {
            input.push('\n');
        }
        let bar = self.bar_color(state).apply_to(self.glyphs.bar);
        input
            .lines()
            .map(|line| format!("{bar}  {}\n", style.apply_to(line)))
            .collect()
    }

    fn password_mask(&self) -> char {
        self.glyphs.mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn detect(windows: bool, env: &[(&str, &str)]) -> TermCaps {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        TermCaps::from_env(windows, true, true, 120, |key| env.get(key).cloned())
    }

    #[test]
    fn claude_desktop_panel_is_modern_without_wt_session() {
        let caps = detect(
            true,
            &[
                ("TERM_PROGRAM", "claude-desktop"),
                ("COLORTERM", "truecolor"),
            ],
        );
        assert_eq!(caps.charset, Charset::Unicode);
        assert_eq!(caps.color, ColorLevel::TrueColor);
    }

    #[test]
    fn windows_terminal_is_modern() {
        assert_eq!(
            detect(true, &[("WT_SESSION", "abc")]).charset,
            Charset::Unicode
        );
    }

    #[test]
    fn bare_conhost_is_legacy() {
        let caps = detect(true, &[]);
        assert_eq!(caps.charset, Charset::Legacy);
        assert_eq!(caps.glyphs().ok, "OK");
    }

    #[test]
    fn unix_locale_decides_unicode() {
        assert_eq!(
            detect(false, &[("LANG", "en_US.UTF-8")]).charset,
            Charset::Unicode
        );
        assert_eq!(
            detect(false, &[("LANG", "C.utf8")]).charset,
            Charset::Unicode
        );
        assert_eq!(detect(false, &[("LANG", "C")]).charset, Charset::Ascii);
        // LC_ALL wins over LANG.
        assert_eq!(
            detect(false, &[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")]).charset,
            Charset::Ascii
        );
        // No locale at all (common on macOS): assume a modern terminal.
        assert_eq!(detect(false, &[]).charset, Charset::Unicode);
    }

    #[test]
    fn linux_vt_is_legacy_and_dumb_is_ascii() {
        assert_eq!(detect(false, &[("TERM", "linux")]).charset, Charset::Legacy);
        assert_eq!(detect(false, &[("TERM", "dumb")]).charset, Charset::Ascii);
    }

    #[test]
    fn color_depth_follows_colorterm_and_term() {
        assert_eq!(
            detect(false, &[("COLORTERM", "24bit")]).color,
            ColorLevel::TrueColor
        );
        assert_eq!(
            detect(false, &[("TERM", "xterm-256color")]).color,
            ColorLevel::Ansi256
        );
        assert_eq!(
            detect(false, &[("TERM", "xterm")]).color,
            ColorLevel::Ansi16
        );
    }

    #[test]
    fn colors_off_paints_nothing() {
        let caps = TermCaps::from_env(false, true, false, 120, |_| Some("truecolor".to_string()));
        assert_eq!(caps.color, ColorLevel::None);
        assert_eq!(caps.paint("1", "x"), "x");
        assert_eq!(caps.accent("x"), "x");
    }

    #[test]
    fn accent_uses_the_deepest_available_teal() {
        let mut caps = detect(false, &[("COLORTERM", "truecolor")]);
        assert_eq!(caps.accent("x"), "\x1b[38;2;46;230;197mx\x1b[0m");
        caps.color = ColorLevel::Ansi256;
        assert_eq!(caps.accent("x"), "\x1b[38;5;43mx\x1b[0m");
        caps.color = ColorLevel::Ansi16;
        assert_eq!(caps.accent("x"), "\x1b[36mx\x1b[0m");
    }
}
