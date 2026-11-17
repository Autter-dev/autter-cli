# Review findings in the editor

## Summary

`autter findings` shows the Autter review findings of the pull request open for your branch, on the lines of your working tree, and can keep them in `.autter/findings.md`. The VS Code extension uses it to show the findings inline, so you no longer have to switch to the dashboard to see them.

## Added

- **`autter findings`** prints the findings of the current branch's open pull request: severity, `file:line` and title, most severe first. `--branch` and `--pr` pick another pull request; `--repo` and `--org` work as in `autter ask`.
- **Lines follow your edits.** Findings are anchored to the commit Autter reviewed. The command diffs that commit against your working tree and moves each line by the lines added or removed above it. A finding whose own line you have edited since the review is reported as changed, with no line, so nothing points at code the review never saw. If the reviewed commit is missing locally, the command fetches the pull request's head without prompting for credentials (`--no-fetch` skips this) and otherwise reports the findings as unverified.
- **`--write`** keeps `.autter/findings.md` up to date: a summary, location, next steps and dashboard link for every finding, grouped into findings in your code, findings on lines you have changed since the review, and findings about the pull request. `.autter/` is added to `.git/info/exclude`. The file only changes when the findings do.
- **`--json`** prints the response with a `local` placement (`status`, `line`) on every finding.
- **VS Code extension**: findings appear as squiggles and Problems entries, with the title after the line (`autter.findings.display`: `inline`, `problems` or `off`). Quick fixes copy a fix prompt for your agent, open the finding in the dashboard, or dismiss it. A finding is dropped as soon as you edit its line, and moves when you add or remove lines above it. The status bar shows the count and opens the findings file; when you are signed out it reads **autter: Sign in** and runs `autter login` in a terminal.
