# Change Log

All notable changes to the "vscode-autter" extension will be documented in this file.

Check [Keep a Changelog](http://keepachangelog.com/) for recommendations on how to structure this file.

## [Unreleased]

## [0.2.0] - 2026-11-17

- Review findings in the editor: the findings of your branch's pull request appear as squiggles, Problems entries and (by default) the title after the line, on the lines of your working tree. Lines follow your edits; a finding whose line you have edited since the review is not shown in the code. Setting: `autter.findings.display` (`inline`, `problems`, `off`).
- Quick fixes: copy a fix prompt for your AI agent, open the finding in the Autter dashboard, or dismiss it.
- **Autter: Review My Changes** reviews your changes before a pull request exists (`autter review`); its findings show up the same way, labelled "local review".
- `.autter/findings.md` is kept up to date with every finding (`autter.findings.writeFile`).
- The status bar shows the count of findings, or **autter: Sign in** when you are signed out.
- Requires autter CLI 3.14.0 or later.

- Initial release