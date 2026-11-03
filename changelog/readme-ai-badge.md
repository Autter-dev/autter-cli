# `autter badge`: a README badge with the repository's AI-written share

`autter badge` prints the share of the repository's added lines that were written by AI and a README badge that shows it: `AI-written | 72%`.

- The share is AI-accepted lines over added lines, summed over the commits that carry an authorship note. It is the same ratio every commit summary uploads as `ai_percent`.
- By default it counts the last 90 days of non-merge commits on the current branch. `--since <date|duration>` takes anything `git log --since` accepts, and `--all-time` counts every commit reachable from HEAD. At most 2,000 commits are walked, and the output says when the cap was hit.
- Commits without a note add nothing to the share, but they are counted, so the output reports coverage. Files matching `.autter-ignore` and `--ignore <pattern>` are left out, as in `autter stats`.
- The Markdown snippet is printed and copied to the clipboard at a terminal. Which badge it uses depends on the setup:

  | Setup | Badge |
  |---|---|
  | Repository linked to Autter, signed in | A live badge served by Autter at `app.autter.dev/api/public/repos/<token>/badge.svg`. It is always current and links to autter.dev. |
  | `--write` on a GitHub repository | A shields.io endpoint badge that reads the committed `.autter/badge.json`, so it updates whenever the file does. |
  | Anything else | A shields.io static badge with today's number. |

- `--json` prints the shields.io endpoint payload (`schemaVersion`, `label`, `message`, `color`) plus the figures behind it. `--write [path]` stores the same payload, by default in `.autter/badge.json`.
- Colors: no data is grey; under 25% light blue; 25-49% blue; 50-74% indigo; 75% and up Autter teal. The live badge uses the same label, rounding and colors.

The live badge needs the updated Autter API (Autter-dev/autter-monorepo#641). Without it, or when the lookup takes more than 5 seconds, the command prints the static badge and one line saying why.
