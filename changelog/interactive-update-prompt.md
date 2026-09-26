# CLI 2.1.0: Interactive Update Prompt

## Summary

Autter noticed new releases and told you to go run `autter upgrade` yourself. It now offers to do it in place, in the style of a shell framework's update prompt:

```
[autter] Update available: v2.0.9 → v2.1.0 (latest)
Update autter now? [Y/n] y
✓ Update autter now? yes

[autter] Running autter upgrade
```

Answer with a single keystroke: `y`/`n`, or Enter to take the default. Declining silences the prompt for a week; accepting installs the release and clears the nudge.

## Added

- **`ui::confirm`** (`src/ui.rs`): a `[Y/n]` prompt in the same visual language as the existing `select` selector — bold question, dimmed hint, then a single collapsed `✓ … yes` line.
- **Update prompt** (`src/commands/upgrade.rs`): `maybe_prompt_to_upgrade` replaces the passive "run `autter upgrade`" notice whenever an interactive terminal is available. The passive notice is still what non-interactive callers get.
- **`disable_update_prompt`** config key, and the `AUTTER_DISABLE_UPDATE_PROMPT=1` environment variable, to keep the prompt silent and fall back to the passive notice. `disable_version_checks` continues to suppress version messaging entirely.

## Safety Properties

The prompt runs from the git proxy's post-command path, so it is written to never interfere with the command that triggered it:

| Concern | Behaviour |
|---|---|
| Never wedge a git command | The keystroke wait is bounded by a 60s deadline; no answer means the default is taken and the notice path continues. |
| Never misreport git's status | The upgrade runs in a child `autter upgrade` process, so its exit code can't become the git command's. A declined prompt, a failed download, or Ctrl-C all leave git's exit code untouched. |
| Never block a non-interactive run | Requires a terminal on both stdin and stderr, and is skipped in CI, inside AI agents, and for background upgrade workers, daemon-driven upgrades, and hook-suppressed runs. |
| Never nag | Declining writes a snooze stamp (`~/.autter/internal/update_prompt_snooze`) for seven days. Accepting clears it. |
| Windows | The child is marked upgrade-initiated so the "don't upgrade from a git proxy" guard doesn't block it, and the existing detached-installer path is unchanged. |

## Fixes

Alongside the feature, several latent defects surfaced and are fixed in this release:

- **Panic reports lost their message.** `panic_message` took a `&(dyn Any + Send)`; downcasting a reference produced by an unsizing coercion reports the trait-object type instead of the concrete payload, so every caught panic reached Sentry as `"unknown panic"`. It now takes the `Box` and downcasts correctly.
- **Notes-database test isolation was order-dependent.** `NotesDatabase::global()` is a `OnceLock`, so `AUTTER_TEST_NOTES_DB_PATH` only reached whichever test initialized it first. When that wasn't one of the notes tests, they wrote to the real `~/.autter/internal/notes-db` and the running daemon drained rows out from under their assertions. A test-only `install_test_db_at` now swaps a throwaway DB into the already-initialized mutex.
- **`--dry-run=false` was silently ignored** by `autter install-hooks`; the parser matched only `--dry-run` and `--dry-run=true`.
