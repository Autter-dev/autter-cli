/// Short explanation printed when authorship attestations are missing.
pub const NO_AUTHORSHIP_DATA_MESSAGE: &str = "No authorship data found for this revision";

/// Footer appended to terminal `autter diff` output when lines lack attestations.
pub fn diff_missing_data_footer() -> String {
    let mut out = String::new();
    out.push('\n');
    out.push_str(
        "Some added lines show [no-data] because no checkpoint recorded who wrote them before commit.\n",
    );
    append_capture_steps(&mut out);
    out
}

/// Guidance printed under stats when every added line is unattested.
pub fn stats_all_untracked_footer() -> String {
    let mut out = String::new();
    out.push('\n');
    out.push_str(
        "No authorship was recorded for this commit, so added lines are shown as untracked.\n",
    );
    append_capture_steps(&mut out);
    out
}

/// Guidance for a single blame line with no attestation.
pub fn blame_line_missing_data_message() -> String {
    let mut out = String::from(
        "No authorship information is available for this line.\n\
         Autter only knows who wrote a line when a checkpoint ran before the change was committed.\n",
    );
    append_capture_steps(&mut out);
    out
}

/// Short notice when `autter blame` falls back to the git author for lines
/// without Autter attestations — so it does not contradict `autter stats`,
/// which reports those same lines as untracked.
pub fn blame_git_author_fallback_notice(unattested_lines: usize) -> String {
    if unattested_lines == 1 {
        "\nNote: 1 line has no Autter attestation, so blame shows the git author \
(same as `git blame`). `autter stats` counts that line as untracked. Use `--mark-unknown` to label it Unknown.\n"
            .to_string()
    } else {
        format!(
            "\nNote: {unattested_lines} lines have no Autter attestation, so blame shows the git author \
(same as `git blame`). `autter stats` counts those lines as untracked. Use `--mark-unknown` to label them Unknown.\n"
        )
    }
}

/// Guidance when `autter show` finds commits but no authorship notes.
pub fn show_missing_data_message() -> String {
    let mut out = format!("{NO_AUTHORSHIP_DATA_MESSAGE}.\n");
    out.push('\n');
    out.push_str("Autter stores authorship in git notes when checkpoints run before commits.\n");
    append_capture_steps(&mut out);
    out
}

fn append_capture_steps(out: &mut String) {
    out.push_str("\nLikely cause: agent/editor hooks or the git proxy did not checkpoint before this commit.\n");
    out.push_str("\nTo diagnose and start capturing authorship:\n");
    out.push_str("  autter install-hooks    # wire up agent and editor hooks\n");
    out.push_str("  autter install          # ensure git proxy + trace2 are configured\n");
    out.push_str("  autter doctor           # verify checkpoint → attribution round-trip\n");
    out.push_str("  autter daemon restart    # if doctor says checkpoints are not persisting\n");
    out.push_str("  autter debug            # full support dump if doctor still fails\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_steps_point_at_doctor_and_daemon() {
        let footer = diff_missing_data_footer();
        assert!(footer.contains("autter doctor"), "{footer}");
        assert!(footer.contains("autter daemon restart"), "{footer}");
        assert!(footer.contains("Likely cause:"), "{footer}");
        assert!(footer.contains("autter install-hooks"), "{footer}");
    }

    #[test]
    fn show_message_keeps_notes_explanation() {
        let msg = show_missing_data_message();
        assert!(msg.contains(NO_AUTHORSHIP_DATA_MESSAGE), "{msg}");
        assert!(msg.contains("git notes"), "{msg}");
        assert!(msg.contains("autter doctor"), "{msg}");
    }

    #[test]
    fn blame_fallback_notice_points_at_stats_and_mark_unknown() {
        let one = blame_git_author_fallback_notice(1);
        assert!(one.contains("1 line has"), "{one}");
        assert!(one.contains("autter stats"), "{one}");
        assert!(one.contains("--mark-unknown"), "{one}");

        let many = blame_git_author_fallback_notice(4);
        assert!(many.contains("4 lines have"), "{many}");
        assert!(many.contains("untracked"), "{many}");
    }
}
