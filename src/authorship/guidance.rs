//! Messages shown when lines or commits have no Autter attestation.
//!
//! Each message says what the missing data means. Troubleshooting is never
//! attached by default: a commit typed by hand, or one that predates Autter,
//! is normal. [`crate::authorship::record_status`] decides when a commit
//! really needs a next step, and supplies exactly one.

use crate::authorship::labels::LineCategory;
use crate::authorship::record_status::{RecordStatus, explain};
use crate::authorship::stats::CommitStats;

/// Short explanation printed when authorship attestations are missing.
pub const NO_AUTHORSHIP_DATA_MESSAGE: &str = "No authorship data found for this revision";

/// Footer appended to terminal `autter diff` output when lines lack attestations.
pub fn diff_missing_data_footer() -> String {
    format!(
        "\n[no-data] lines have no Autter attestation: {} ({}), or {} ({}). `autter stats <commit>` says which and why.\n",
        LineCategory::Untracked.label(),
        LineCategory::Untracked.meaning(),
        LineCategory::Unknown.label(),
        LineCategory::Unknown.meaning(),
    )
}

/// Guidance for a single blame line with no attestation (`blame --why`).
pub fn blame_line_missing_data_message() -> String {
    format!(
        "No authorship information is available for this line: {}.\n\
         `autter stats <commit>` says why its commit has no record.\n",
        LineCategory::Unknown.meaning()
    )
}

/// `autter show` for a commit with no authorship note: why, and at most one
/// next step.
pub fn show_missing_record_message(record: &RecordStatus) -> String {
    match explain(record, &CommitStats::default()) {
        Some(explanation) => explanation.render(),
        None => format!("{NO_AUTHORSHIP_DATA_MESSAGE}.\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::record_status::MissingCause;

    #[test]
    fn diff_footer_explains_labels_without_repair_commands() {
        let footer = diff_missing_data_footer();
        assert!(footer.contains("untracked"), "{footer}");
        assert!(footer.contains("unknown"), "{footer}");
        assert!(!footer.contains("install-hooks"), "{footer}");
        assert!(!footer.contains("bg restart"), "{footer}");
    }

    #[test]
    fn show_message_gives_one_reason_and_at_most_one_step() {
        let predates = show_missing_record_message(&RecordStatus::PredatesAutter {
            first_recorded_at: None,
        });
        assert!(
            predates.contains("No commit in this repository"),
            "{predates}"
        );
        assert!(!predates.contains("Next:"), "{predates}");

        let failed = show_missing_record_message(&RecordStatus::Missing {
            cause: MissingCause::CaptureFailed,
        });
        assert!(failed.starts_with("Capture failed"), "{failed}");
        assert_eq!(failed.matches("Next:").count(), 1, "{failed}");
    }
}
