# Stats Bar: Untracked Segment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `mixed` (▒) bar segment with an `untracked` (·) segment driven by `unknown_additions`, show it only when > 1%, and hyperlink the label in interactive shells.

**Architecture:** Single in-place edit to `write_stats_to_terminal` in `src/authorship/stats.rs`. Bar-segment math and percentage-line logic are replaced; all five existing terminal snapshots are updated via `cargo insta review`. No new files, no new functions.

**Tech Stack:** Rust, `insta` snapshot testing, OSC 8 terminal hyperlinks.

---

### Task 1: Create worktree and branch

**Files:** none

- [ ] **Step 1: Create a fresh git worktree**

```bash
git worktree add ../autter-stats-bar-untracked -b stats-bar-untracked
```

- [ ] **Step 2: Verify the worktree exists and is on the new branch**

```bash
git -C ../autter-stats-bar-untracked branch --show-current
```

Expected output: `stats-bar-untracked`

---

### Task 2: Write all new failing tests and update existing test calls

**Files:**
- Modify: `src/authorship/stats.rs` (test block starting at line ~777)

- [ ] **Step 1: Change all five existing `write_stats_to_terminal` calls in `test_terminal_stats_display` from `true` to `false`**

Find these five lines inside `test_terminal_stats_display` and change each `true` to `false`:

```rust
let mixed_output = write_stats_to_terminal(&stats, false);
// ...
let ai_only_output = write_stats_to_terminal(&ai_stats, false);
// ...
let human_only_output = write_stats_to_terminal(&human_stats, false);
// ...
let minimal_human_output = write_stats_to_terminal(&minimal_human_stats, false);
// ...
let deletion_only_output = write_stats_to_terminal(&deletion_only_stats, false);
```

Rationale: `is_interactive = true` will emit OSC 8 escape codes, which would clutter snapshot files. All snapshot tests use `false`; the hyperlink test below uses `true` directly.

- [ ] **Step 2: Add five new test cases immediately after the `deletion_only_output` snapshot assert**

```rust
        // --- New test cases for untracked segment ---

        // 18% human / 22% untracked / 60% AI — matches the design example
        let untracked_stats = CommitStats {
            human_additions: 180,
            unknown_additions: 220,
            mixed_additions: 0,
            ai_additions: 600,
            ai_accepted: 462,
            time_waiting_for_ai: 60,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 1000,
            total_ai_additions: 600,
            total_ai_deletions: 0,
            tool_model_breakdown: BTreeMap::new(),
        };
        let with_untracked_output = write_stats_to_terminal(&untracked_stats, false);
        assert_debug_snapshot!(with_untracked_output);

        // untracked exactly at the 1% threshold — should NOT show untracked segment
        let threshold_stats = CommitStats {
            human_additions: 49,
            unknown_additions: 1,
            mixed_additions: 0,
            ai_additions: 50,
            ai_accepted: 50,
            time_waiting_for_ai: 0,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            total_ai_additions: 50,
            total_ai_deletions: 0,
            tool_model_breakdown: BTreeMap::new(),
        };
        let untracked_at_threshold_output = write_stats_to_terminal(&threshold_stats, false);
        assert_debug_snapshot!(untracked_at_threshold_output);

        // untracked just above 1% threshold (~2%) — should show untracked segment
        let above_threshold_stats = CommitStats {
            human_additions: 97,
            unknown_additions: 2,
            mixed_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            time_waiting_for_ai: 0,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 99,
            total_ai_additions: 0,
            total_ai_deletions: 0,
            tool_model_breakdown: BTreeMap::new(),
        };
        let untracked_just_above_output = write_stats_to_terminal(&above_threshold_stats, false);
        assert_debug_snapshot!(untracked_just_above_output);

        // 100% untracked — entire bar is · chars
        let all_untracked_stats = CommitStats {
            human_additions: 0,
            unknown_additions: 100,
            mixed_additions: 0,
            ai_additions: 0,
            ai_accepted: 0,
            time_waiting_for_ai: 0,
            git_diff_deleted_lines: 0,
            git_diff_added_lines: 100,
            total_ai_additions: 0,
            total_ai_deletions: 0,
            tool_model_breakdown: BTreeMap::new(),
        };
        let all_untracked_output = write_stats_to_terminal(&all_untracked_stats, false);
        assert_debug_snapshot!(all_untracked_output);

        // OSC 8 hyperlink emitted when is_interactive = true
        // Not a snapshot test — asserts presence of the escape sequence directly.
        let hyperlink_output = write_stats_to_terminal(&untracked_stats, true);
        assert!(
            hyperlink_output.contains("\x1b]8;;https://autter.dev/docs\x1b\\"),
            "Expected OSC 8 hyperlink in interactive output, got: {:?}",
            hyperlink_output
        );
        assert!(
            hyperlink_output.contains("untracked"),
            "Expected 'untracked' label in interactive output"
        );
```

---

### Task 3: Run tests to confirm failures

**Files:** none (read-only)

- [ ] **Step 1: Run the display test**

```bash
cd ../autter-stats-bar-untracked && cargo test -p autter "authorship::stats::tests::test_terminal_stats_display" 2>&1 | tail -50
```

Expected: compilation succeeds (signature hasn't changed yet), then test failures:
- Existing 5 snapshots: "snapshot changed" (because `true`→`false` doesn't change output yet, but the snapshots will diverge once we change the implementation)
- Actually at this point, changing `true`→`false` doesn't yet cause snapshot failures since the function hasn't changed. The 4 new snapshot tests will fail with "snapshot not found", and the hyperlink assert will fail because the function doesn't yet emit OSC codes.

Look for: `FAILED` on `with_untracked_output`, `untracked_at_threshold_output`, `untracked_just_above_output`, `all_untracked_output`, and the hyperlink assertion.

---

### Task 4: Implement `write_stats_to_terminal`

**Files:**
- Modify: `src/authorship/stats.rs:97-301`

- [ ] **Step 1: Rename the `print` parameter to `is_interactive` in the function signature (line 97)**

Old:
```rust
pub fn write_stats_to_terminal(stats: &CommitStats, print: bool) -> String {
```

New:
```rust
pub fn write_stats_to_terminal(stats: &CommitStats, is_interactive: bool) -> String {
```

- [ ] **Step 2: Update the two `if print {` guards in the deletion-only path (lines ~115 and ~123)**

Replace both occurrences:
```rust
        if print {
```
with:
```rust
        if is_interactive {
```

(There are exactly two of these before the `return output;` on line ~127.)

- [ ] **Step 3: Replace the bar-calculation block**

Find the section that starts with:
```rust
    // Calculate total additions for the progress bar
    // Total = (known human + unknown) + mixed (AI-edited-by-human) + pure AI
    // unknown_additions are unattested lines — treated as human for display until
    // full KnownHuman attestation pipeline is in place.
    let display_human = stats.human_additions + stats.unknown_additions;
    let total_additions = display_human + stats.ai_additions;
```

And ends with:
```rust
    progress_bar.push_str(" ai");
```

Replace the entire block (from the `// Calculate total additions` comment through `progress_bar.push_str(" ai");`) with:

```rust
    // Calculate total additions: known human + unknown (untracked) + AI
    let total_additions = stats.human_additions + stats.unknown_additions + stats.ai_additions;

    // Calculate AI acceptance percentage (capped at 100%)
    let _ai_acceptance_percentage = if stats.ai_additions > 0 {
        ((stats.ai_accepted as f64 / stats.ai_additions as f64) * 100.0).min(100.0)
    } else {
        0.0
    };

    // Determine whether to show the untracked segment (raw float check, before rounding)
    let untracked_pct_raw = if total_additions > 0 {
        stats.unknown_additions as f64 / total_additions as f64 * 100.0
    } else {
        0.0
    };
    let show_untracked = untracked_pct_raw > 1.0;

    // Calculate human bar segment
    let human_bars = if total_additions > 0 {
        ((stats.human_additions as f64 / total_additions as f64) * bar_width as f64) as usize
    } else {
        0
    };

    // Ensure human contributions get at least 2 visible blocks if they have more than 1 line
    let min_human_bars = if stats.human_additions > 1 { 2 } else { 0 };
    let final_human_bars = human_bars.max(min_human_bars);

    // Distribute remaining width between untracked and AI proportionally.
    // When untracked is below the 1% threshold, all remaining width goes to AI.
    let remaining_width = bar_width.saturating_sub(final_human_bars);
    let (final_untracked_bars, final_ai_bars) = if show_untracked {
        let total_other = stats.unknown_additions + stats.ai_additions;
        let untracked_bars = if total_other > 0 {
            ((stats.unknown_additions as f64 / total_other as f64) * remaining_width as f64)
                as usize
        } else {
            0
        };
        (untracked_bars, remaining_width.saturating_sub(untracked_bars))
    } else {
        (0, remaining_width)
    };

    // Build the progress bar
    let mut progress_bar = String::new();
    progress_bar.push_str("you  ");
    progress_bar.push_str(&"█".repeat(final_human_bars));   // known human (attested)
    progress_bar.push_str(&"·".repeat(final_untracked_bars)); // untracked (no attestation)
    progress_bar.push_str(&"░".repeat(final_ai_bars));       // AI
    progress_bar.push_str(" ai");
```

Note: `·` is U+00B7 MIDDLE DOT.

- [ ] **Step 4: Replace the percentage calculation and print block**

Find the section that starts with:
```rust
    // Print the stats
    output.push_str(&progress_bar);
    output.push('\n');
    if print {
        println!("{}", progress_bar);
    }
    // Print percentage line with proper spacing (40 columns total)
```

And ends with the closing `}` of the `if mixed_percentage > 0 { ... } else { ... }` block (before the `// Only show AI stats if there was actually AI code` comment).

Replace with:

```rust
    // Calculate percentages for display
    let human_percentage = if total_additions > 0 {
        ((stats.human_additions as f64 / total_additions as f64) * 100.0).round() as u32
    } else {
        0
    };
    let ai_percentage = if total_additions > 0 {
        ((stats.ai_additions as f64 / total_additions as f64) * 100.0).round() as u32
    } else {
        0
    };

    // Print the stats
    output.push_str(&progress_bar);
    output.push('\n');
    if is_interactive {
        println!("{}", progress_bar);
    }

    // Percentage line: three anchors (human / untracked / AI) when untracked is visible,
    // two anchors (human / AI) otherwise.
    if show_untracked {
        let untracked_percentage = untracked_pct_raw.round() as u32;
        // When interactive, wrap "untracked" in an OSC 8 hyperlink so it is clickable in
        // supporting terminals (iTerm2, Warp, etc.). Spaces are constructed manually —
        // not via format-width padding on the label — so that invisible escape bytes do
        // not misalign the output.
        let untracked_label = if is_interactive {
            "\x1b]8;;https://autter.dev/docs\x1b\\untracked\x1b]8;;\x1b\\".to_string()
        } else {
            "untracked".to_string()
        };
        let percentage_line = format!(
            "     {:<3}{:>10}{} {:>3}%{:>10}{:>3}%",
            format!("{}%", human_percentage),
            "",
            untracked_label,
            untracked_percentage,
            "",
            ai_percentage
        );
        output.push_str(&percentage_line);
        output.push('\n');
        if is_interactive {
            println!("{}", percentage_line);
        }
    } else {
        let percentage_line = format!(
            "     {:<3}{:>33}{:>3}%",
            format!("{}%", human_percentage),
            "",
            ai_percentage
        );
        output.push_str(&percentage_line);
        output.push('\n');
        if is_interactive {
            println!("{}", percentage_line);
        }
    }
```

- [ ] **Step 5: Update the remaining `if print {` guard in the AI-stats line (line ~295)**

```rust
        if print {
            println!("{}", ai_acceptance_str);
        }
```

→

```rust
        if is_interactive {
            println!("{}", ai_acceptance_str);
        }
```

---

### Task 5: Run tests, accept snapshots, verify

**Files:**
- Modify (via insta): `src/authorship/snapshots/autter__authorship__stats__tests__terminal_stats_display.snap` through `-5.snap`, plus 4 new snapshot files

- [ ] **Step 1: Run the display test**

```bash
cargo test -p autter "authorship::stats::tests::test_terminal_stats_display" 2>&1 | tail -40
```

Expected: existing 5 snapshots fail with "snapshot changed"; 4 new cases fail with "snapshot not found"; hyperlink assert **passes**.

- [ ] **Step 2: Accept all updated and new snapshots**

```bash
cargo insta review
```

For each snapshot, verify it looks correct before accepting:

| Snapshot expression | Expected visual |
|---|---|
| `mixed_output` | `██████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░ ai` — no `▒`, no "mixed" label |
| `ai_only_output` | `░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ ai` — unchanged visually |
| `human_only_output` | `████████████████████████████████████████ ai` — unchanged |
| `minimal_human_output` | `██░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ ai` — unchanged |
| `deletion_only_output` | gray bar, "(no additions)" — unchanged |
| `with_untracked_output` | `███████········░░░░░░░░░░░░░░░░░░░░░░░░░ ai` + three-anchor pct line |
| `untracked_at_threshold_output` | no `·` chars, two-anchor pct line |
| `untracked_just_above_output` | `·` chars visible, three-anchor pct line with small untracked% |
| `all_untracked_output` | 40 `·` chars, three-anchor pct line |

- [ ] **Step 3: Run full test suite for regressions**

```bash
cargo test -p autter 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/authorship/stats.rs src/authorship/snapshots/
git commit -m "$(cat <<'EOF'
feat: replace mixed with untracked segment in stats bar

- Human bar now shows only attested human_additions (not +unknown)
- Replaces mixed (▒) segment with untracked (·) using unknown_additions
- Hides untracked when ≤ 1% of total additions
- Hyperlinks the untracked label (OSC 8) in interactive shells
- Placeholder URL https://autter.dev/docs — update manually

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Push and open PR

**Files:** none

- [ ] **Step 1: Push the branch**

```bash
git push -u origin stats-bar-untracked
```

- [ ] **Step 2: Open the PR**

```bash
gh pr create \
  --title "feat: replace mixed with untracked segment in stats bar" \
  --body "$(cat <<'EOF'
## Summary

- Human bar now shows only explicitly-attested `human_additions` (not `human + unknown`)
- Replaces the `mixed` (▒) segment with `untracked` (·) driven by `unknown_additions`
- Untracked segment is hidden when ≤ 1% of total additions (simplified two-anchor output)
- In interactive shells, the `untracked` label is an OSC 8 hyperlink — **placeholder URL `https://autter.dev/docs` needs updating manually**
- `mixed_additions` is ignored entirely by the display layer

## Test plan

- [ ] `cargo test -p autter authorship::stats` passes
- [ ] Snapshot diffs look right: human%, untracked%, AI% sum to ~100%
- [ ] Run `git commit` in a repo with `unknown_additions > 1%` — bar shows `·` chars
- [ ] In a terminal that supports OSC 8 (iTerm2, Warp, Ghostty), verify `untracked` is clickable
- [ ] Update the placeholder URL before merging

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Return the PR URL.
