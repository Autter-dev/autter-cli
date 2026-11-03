# `autter wrapped`

## Summary

A Spotify-Wrapped-style summary of your year of AI-assisted coding in a repository, drawn as a 60-column card made for screenshots, with a JSON form that the share-image pipeline consumes.

## Added

- **`autter wrapped [--year <yyyy>] [--author <email> | --all-authors] [--json] [--include-prompt-text] [--no-color]`**: walks the year's commits (yours by default, from `git user.email`), reads each authorship note and folds the per-commit stats into one summary:
  - the AI share of added lines, overall and by month;
  - the top agent and top model by accepted lines, with the full ranked lists;
  - the file with the most distinct prompts attached to it, with the top five;
  - the longest first prompt of the year by character count;
  - the longest streak of days with an AI-attributed commit and the busiest weekday.
- The terminal card shows the headline percentage in block digits, a twelve-bar monthly chart, the rows above and an `autter.dev/wrapped` footer. It honors `NO_COLOR` and `--no-color`, falls back to ASCII on terminals without Unicode, and is deterministic for a given summary so screenshots come out the same every time.
- With fewer than three noted commits in the year, the command prints a short note instead of a card.

## Privacy

Nothing leaves the machine. The longest prompt is measured from transcripts already in the local cache; prompts that only exist remotely are skipped. Its text is never printed or included in the JSON unless `--include-prompt-text` is passed, and then only the first 120 characters.

## JSON

`autter wrapped --json` prints a `wrapped/1.0.0` document: `year`, `author`, `commits`, `commits_with_authorship`, `added_lines`, `ai_lines`, `human_lines`, `untracked_lines`, `ai_percent`, `by_month` (12 entries of `month`, `ai_accepted`, `added_lines`, `ai_percent`, `commits`), `top_agent`, `top_model`, `agents`, `models`, `most_prompted_file`, `files`, `prompts_total`, `prompts_with_transcript`, `longest_prompt` (`chars`, `words`, `agent`, `model`, `commit`, `snippet`), `busiest_day` and `streak_days`. The walk stops after 5,000 commits and says so in `capped`.
