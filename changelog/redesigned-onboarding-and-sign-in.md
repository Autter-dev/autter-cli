# Redesigned onboarding and sign-in

## Summary

`autter onboard` and `autter login` get a redesigned terminal UI (`specs/roadmap/tui-onboarding-design.md`). Setup opens with the autter logo and walks one step rail with numbered questions. Sign-in shows its code with a countdown and offers a way forward when it fails. Integrations can be chosen per tool. Prompts, spinners and the update prompt share one theme and degrade the same way in plain and legacy terminals.

## Added

- **Brand header**: `autter onboard` opens with the "a." logo drawn in block characters, with the version, a setup note and the current folder beside it. Legacy Windows consoles get a variant built from half blocks. Narrow terminals, non-terminals and ASCII-only locales get plain lines.
- **Sign-in screen** (`autter onboard`, `autter login`): the one-time code, the URL, and a live "expires in m:ss" countdown. Enter reopens the browser, `c` copies the code through the terminal (OSC 52), and Esc continues in local mode during onboarding or cancels `autter login`.
- **Recovery after a failed sign-in**: try again with a new code, paste an access token (masked, checked before it's saved), or continue in local mode (or cancel, for `autter login`). This replaces the block of fallback instructions.
- **Per-tool integrations**: onboarding lists the coding tools it finds plus "Git capture and background service", all checked. Enter installs everything, unchecking all skips, and `--yes` still installs everything without asking.
- **`src/ui/` module**: terminal detection (color depth, Unicode support, width), the theme, header, step rail, selector, multiselect, masked input, sign-in screen, timed confirm and spinner. Commands go through `crate::ui` instead of using prompt or color crates directly.
- **`cargo run --example ui_preview`**: walks the onboarding screens with sample data, including a simulated sign-in, without touching config or contacting the server.

## Changed

- **Onboarding layout**: one rail from start to finish. Questions show their place ("2/4"), answered questions collapse to one line, the telemetry question fits on one line with the details in its hint, and the closing summary ends the rail.
- **Selector**: drawn with cliclack in autter's theme (`◆` question, `●`/`○` options, the focused option's hint on its own line). Arrow keys, j/k and Enter work as before. Esc now cancels like Ctrl-C. The 1–9, Tab and Space shortcuts were removed.
- **Spinner outcome lines** (`✓`/`⚠`/`✗`/`○`, used by `autter install-hooks`): on a terminal they print to stderr. When stdout is piped, or when stderr is redirected, the same plain line as before still goes to stdout.
- **Update prompt**: same keys and 60-second timeout, drawn in the shared theme.

Non-interactive runs (scripted installs, CI) print the same information as before: no prompts, and the sign-in URL and code as plain lines.

## Fixed

- `autter login` and `autter onboard` wrote color codes unconditionally, ignoring `NO_COLOR` and `--no-color`.
- The onboarding welcome promised "two quick choices"; setup can ask up to five questions.
- Telemetry consent opened with "One last thing", although more questions followed.
