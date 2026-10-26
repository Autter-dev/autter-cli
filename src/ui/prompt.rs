//! Single-choice prompt, drawn by cliclack in autter's theme.

use std::io::Write;

use super::is_interactive;
use super::theme::{self, install_prompt_theme};

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

/// Ask the user to pick one of `items` with the arrow keys (or j/k),
/// confirming with Enter. Returns the chosen index. Esc or Ctrl-C cancels
/// and exits with status 130.
///
/// Falls back to a numbered "type 1-N" prompt when the terminal can't be
/// put in raw mode, and to `default` when stdin/stderr aren't terminals —
/// callers that already gate on `is_terminal()` keep their existing
/// non-interactive behavior.
pub fn select(question: &str, items: &[SelectItem], default: usize) -> usize {
    if items.is_empty() {
        return 0;
    }
    let default = default.min(items.len() - 1);
    if !is_interactive() {
        return default;
    }

    install_prompt_theme();
    let mut prompt = cliclack::select(question).initial_value(default);
    for (i, item) in items.iter().enumerate() {
        prompt = prompt.item(i, &item.label, &item.hint);
    }
    match prompt.interact() {
        Ok(choice) => choice,
        // The prompt has already collapsed to "Cancelled.".
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => std::process::exit(130),
        Err(_) => select_numbered(question, items, default),
    }
}

/// Let the user check any of `items` (space toggles, Enter confirms), with
/// the indices in `checked` checked to start. Returns the checked indices;
/// an empty result means "none". Esc or Ctrl-C cancels (exit 130).
///
/// Without a terminal this returns `checked` unchanged, so only pass
/// pre-checked items that are safe to apply without asking. Where raw mode
/// isn't available it asks a plain yes/no about the pre-checked set.
pub fn multiselect(question: &str, items: &[SelectItem], checked: &[usize]) -> Vec<usize> {
    if items.is_empty() {
        return Vec::new();
    }
    if !is_interactive() {
        return checked.to_vec();
    }

    install_prompt_theme();
    let mut prompt = cliclack::multiselect(question)
        .required(false)
        .initial_values(checked.to_vec());
    for (i, item) in items.iter().enumerate() {
        prompt = prompt.item(i, &item.label, &item.hint);
    }
    match prompt.interact() {
        Ok(chosen) => chosen,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => std::process::exit(130),
        Err(_) => multiselect_line(question, items, checked),
    }
}

fn multiselect_line(question: &str, items: &[SelectItem], checked: &[usize]) -> Vec<usize> {
    let caps = theme::caps();
    eprintln!("{}", caps.paint("1", question));
    for &i in checked {
        eprintln!("  - {}", items[i].label);
    }
    eprint!("All of these? [Y/n]: ");
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return Vec::new();
    }
    match input.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => checked.to_vec(),
        _ => Vec::new(),
    }
}

/// Masked single-line input, for secrets such as an access token. Returns
/// `None` without a terminal, or when the user backs out with Esc.
pub fn secret(question: &str) -> Option<String> {
    if !is_interactive() {
        return None;
    }
    install_prompt_theme();
    cliclack::password(question)
        .interact()
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Line-input fallback for terminals where raw mode isn't available.
fn select_numbered(question: &str, items: &[SelectItem], default: usize) -> usize {
    let caps = theme::caps();
    eprintln!("{}", caps.paint("1", question));
    for (i, item) in items.iter().enumerate() {
        let hint = caps.paint("2", &format!("— {}", item.hint));
        eprintln!("  {}) {} {hint}", i + 1, item.label);
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
