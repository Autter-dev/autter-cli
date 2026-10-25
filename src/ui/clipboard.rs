//! Copy text to the clipboard through the terminal (OSC 52), with no
//! platform clipboard dependency. The terminal never reports whether it
//! honored the request, so callers should say "copy requested", not "copied".

use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Ask the terminal behind stderr to put `text` on the clipboard. Returns
/// whether the request could be written.
pub fn request_copy(text: &str) -> bool {
    let inside_tmux = std::env::var_os("TMUX").is_some();
    let mut err = std::io::stderr();
    err.write_all(osc52(text, inside_tmux).as_bytes()).is_ok() && err.flush().is_ok()
}

/// The OSC 52 sequence. Inside tmux it is wrapped in tmux's passthrough so it
/// reaches the outer terminal (tmux needs `allow-passthrough` or
/// `set-clipboard on` for this to land).
fn osc52(text: &str, inside_tmux: bool) -> String {
    let sequence = format!("\x1b]52;c;{}\x07", STANDARD.encode(text));
    if inside_tmux {
        format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
    } else {
        sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_text_as_osc52() {
        assert_eq!(osc52("WDJB-MJHT", false), "\x1b]52;c;V0RKQi1NSkhU\x07");
    }

    #[test]
    fn wraps_for_tmux_passthrough() {
        assert_eq!(
            osc52("WDJB-MJHT", true),
            "\x1bPtmux;\x1b\x1b]52;c;V0RKQi1NSkhU\x07\x1b\\"
        );
    }
}
