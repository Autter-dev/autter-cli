# Status screens and interactive views

## Summary

The terminal redesign (`specs/roadmap/tui-onboarding-design.md`) reaches the commands people use day to day. Bare `autter` shows your setup at a glance, `whoami` and `doctor` get readable layouts, and two new full-screen views let you browse authorship: `autter stats -i` and `autter blame -i`.

## Added

- **`autter stats -i [commit]`**: browse the last 200 commits by AI share, with each commit's AI, human and untracked lines and its breakdown by tool and model. The list opens immediately and fills in as each commit is counted. ↑/↓ or j/k to move, g/G for first/last, q to quit.
- **`autter blame -i <file>`**: read a file with a colored mark beside every line (AI, human, untracked) and details for the line under the cursor: tool, model, commit, author and age. `n`/`N` jump between AI changes, and Enter shows the prompt transcript behind an AI line. Accepts the same git blame options as `autter blame`.

## Changed

- **Bare `autter`** at a terminal shows a status card instead of the command list: mode and account, the latest commit's AI share with the tool behind it, pending uploads, and commands to try. It reads only local state, so it's instant. `autter help` still lists every command, and piped output is unchanged.
- **`autter whoami`** at a terminal shows a card with your account, sign-in state (with how long the session stays valid), organizations, cloud sync status and API. Piped output keeps the `Key: value` lines.
- **`autter doctor`** at a terminal shows each check with a spinner while it runs, resolving in place, under bold section titles. `--json` and piped output are unchanged.
- **`autter install-hooks`** at a terminal lists each tool on a rail with a closing "N coding tools set up". Piped output and `-v` are unchanged.

Both interactive views need a terminal. Elsewhere they print a note and show the plain command. They honor `NO_COLOR` and restore the terminal on exit, Ctrl-C or a crash.
