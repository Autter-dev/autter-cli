//! Interactive terminal UI shared by user-facing commands: terminal
//! detection and the theme, the brand header, the step rail, prompts, the
//! browser sign-in screen, and spinners.
//!
//! Everything renders on stderr so command output on stdout stays
//! scriptable, and every prompt returns its default without a terminal.
//! Linear flows use cliclack in autter's theme (see `theme`); call sites go
//! through this module rather than importing cliclack directly. Color goes
//! through `caps().paint(..)`, which honors `NO_COLOR` and `--no-color`.

pub mod clipboard;
pub mod confirm;
pub mod header;
pub mod progress;
pub mod prompt;
pub mod rail;
pub mod signin;
pub mod theme;

use std::io::IsTerminal;

pub use confirm::confirm;
pub use header::Header;
pub use prompt::{SelectItem, multiselect, secret, select};
pub use theme::{TermCaps, caps};

/// Whether prompts can be shown: both stdin and stderr must be terminals.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    /// Setup flows color through the theme so `NO_COLOR` and `--no-color`
    /// are honored; a raw escape in them bypasses both.
    #[test]
    fn setup_flows_have_no_raw_ansi_escapes() {
        for (file, source) in [
            ("commands/login.rs", include_str!("../commands/login.rs")),
            (
                "commands/onboard.rs",
                include_str!("../commands/onboard.rs"),
            ),
            ("mdm/spinner.rs", include_str!("../mdm/spinner.rs")),
            ("ui/progress.rs", include_str!("progress.rs")),
            ("ui/prompt.rs", include_str!("prompt.rs")),
            ("ui/confirm.rs", include_str!("confirm.rs")),
            ("ui/header.rs", include_str!("header.rs")),
            ("ui/rail.rs", include_str!("rail.rs")),
            ("ui/signin.rs", include_str!("signin.rs")),
        ] {
            assert!(
                !source.contains("\\x1b["),
                "{file} writes a raw ANSI escape; use ui::caps().paint(..) instead"
            );
        }
    }
}
