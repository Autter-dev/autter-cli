# Stats Bar: Untracked Segment Design

**Date:** 2026-04-13
**Status:** Approved

## Background

We recently introduced "known human" attribution: code is marked human only when the IDE extension explicitly attests it. Lines with no attestation are now `unknown_additions`, not assumed-human. The visual stats bar has not yet reflected this — it still lumps `human + unknown` together as the human segment and shows a `mixed` segment for AI-edited-by-human lines.

This spec updates the terminal stats bar to:
1. Show only explicitly-attested human lines as the human segment.
2. Replace `mixed` with `untracked` (the `unknown_additions` count).
3. Hide untracked entirely when `unknown_additions as f64 / total as f64 * 100.0 <= 1.0` (raw float check, before rounding).
4. Hyperlink the `untracked` label to IDE integration docs in interactive shells.

## Data Model

No struct changes. The three `CommitStats` fields that drive the new bar:

| Field | Segment | Character |
|---|---|---|
| `human_additions` | human | `█` (U+2588 FULL BLOCK) |
| `unknown_additions` | untracked | `·` (U+00B7 MIDDLE DOT) |
| `ai_additions` | AI | `░` (U+2591 LIGHT SHADE) |

`mixed_additions` is ignored entirely by `write_stats_to_terminal`.

Total denominator: `human_additions + unknown_additions + ai_additions`.

## Bar Rendering

### Segment sizing (40-char bar)

```
human_bars     = floor(human_additions    / total * 40)
untracked_bars = floor(unknown_additions  / total * 40)
ai_bars        = 40 - human_bars - untracked_bars
```

Minimum visibility (preserved from current behaviour):
- If `human_additions > 1`: `human_bars = max(human_bars, 2)`, and remaining width is redistributed between untracked and AI proportionally.

### Bar line

```
you  ███████·········░░░░░░░░░░░░░░░░░░░░░░░░ ai
```

### Percentage line — untracked > 1%

```
     18%          untracked  22%          60%
```

The `untracked` label is plain text when `is_interactive = false`. When `is_interactive = true` (interactive shell), it is wrapped in an OSC 8 hyperlink:

```
\x1b]8;;https://autter.dev/docs\x1b\untracked\x1b]8;;\x1b\
```

The surrounding spaces are constructed manually (not via format-width padding) so the invisible escape bytes don't misalign the output.

### Percentage line — untracked ≤ 1%

```
     18%                                  60%
```

Same simplified two-anchor format as the current no-mixed path.

### AI stats line (unchanged)

```
     77% AI code accepted | waited 1m for ai
```

Only shown when `ai_additions > 0`.

## Function Signature

```rust
// Before
pub fn write_stats_to_terminal(stats: &CommitStats, print: bool) -> String

// After
pub fn write_stats_to_terminal(stats: &CommitStats, is_interactive: bool) -> String
```

`is_interactive` controls both stdout printing and hyperlink emission. All existing call sites already pass `std::io::stdout().is_terminal()` or `true` (CLI stats command), so no logic changes at callers.

## Implementation Steps

1. Rename `print` → `is_interactive` throughout `write_stats_to_terminal`.
2. Replace `display_human = human_additions + unknown_additions` with `display_human = human_additions`.
3. Update `total_additions` to `human_additions + unknown_additions + ai_additions`.
4. Replace `mixed_bars`/`▒` section with `untracked_bars`/`·` section.
5. Update percentage line:
   - Compute `untracked_pct_raw = unknown_additions as f64 / total as f64 * 100.0`.
   - If `untracked_pct_raw > 1.0`: build three-anchor line with untracked label (hyperlinked if `is_interactive`), display rounded integer percentage.
   - Else: build two-anchor line (human% and ai% only).
6. Remove all references to `mixed_additions` and `mixed_percentage`.
7. Update snapshot files via `cargo insta review`.

## Test Plan

### Existing cases — updated snapshots

All five `test_terminal_stats_display` snapshot files change because the bar calculation changes. Run `cargo insta review` after implementing to accept the new output.

| Snapshot expression | Data | Expected change |
|---|---|---|
| `mixed_output` | human=50, unknown=0, ai=100 | `▒` bars gone; untracked=0% so no untracked label |
| `ai_only_output` | human=0, unknown=0, ai=100 | Snapshot updates (calculation path changes) |
| `human_only_output` | human=75, unknown=0, ai=0 | Snapshot updates |
| `minimal_human_output` | human=2, unknown=0, ai=100 | Snapshot updates |
| `deletion_only_output` | deletions only | No change |

### New cases added to `test_terminal_stats_display`

All new snapshot-based cases call `write_stats_to_terminal(&stats, false)` (non-interactive) to keep snapshots free of OSC escape codes.

| Case name | Data | Purpose |
|---|---|---|
| `with_untracked` | human=180, unknown=220, ai=600 | Matches 18%/22%/60% example; verifies `·` chars and "untracked  22%" label |
| `untracked_at_threshold` | human=99, unknown=1, ai=0 | untracked=1% exactly → no untracked section shown |
| `untracked_just_above_threshold` | human=97, unknown=2, ai=0 | untracked≈2% → untracked section shown |
| `all_untracked` | human=0, unknown=100, ai=0 | 100% `·` chars; no AI stats line |
| `untracked_with_hyperlink` | Same as `with_untracked`, `is_interactive=true` | Assert `output.contains("\x1b]8;;https://autter.dev/docs")` — no snapshot |

### Out of scope

`write_stats_to_markdown` is not changed by this spec.
