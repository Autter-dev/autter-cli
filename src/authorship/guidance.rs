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

/// Guidance when `autter show` finds commits but no authorship notes.
pub fn show_missing_data_message() -> String {
    let mut out = format!("{NO_AUTHORSHIP_DATA_MESSAGE}.\n");
    out.push('\n');
    out.push_str("Autter stores authorship in git notes when checkpoints run before commits.\n");
    append_capture_steps(&mut out);
    out
}

fn append_capture_steps(out: &mut String) {
    out.push_str("\nTo start capturing authorship:\n");
    out.push_str("  autter install-hooks    # wire up agent and editor hooks\n");
    out.push_str("  autter debug            # verify each editor/agent can checkpoint\n");
}
