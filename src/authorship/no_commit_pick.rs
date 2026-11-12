//! Carries the attribution of `git cherry-pick -n` into the commit that
//! follows it.
//!
//! One pick can apply several commits. Every picked commit contributes its
//! attribution, oldest pick first; a later pick only fills lines no earlier
//! pick and no live work already claims.

use crate::authorship::rebase_authorship::{
    merge_working_log_after_squash_from_final_state,
    prepare_working_log_after_squash_from_final_state,
};
use crate::error::AutterError;
use crate::git::repository::Repository;
use crate::git::rewrite_log::MergeSquashEvent;
use std::collections::HashMap;

/// Brings the attribution of the commits picked by one `git cherry-pick -n`
/// into the working log of `base_commit`, lined up with `final_state` (the
/// contents being committed).
///
/// `replace_working_log` says the working log tracks nothing yet, so the first
/// picked commit can write it from scratch. Everything after that is merged
/// into what is there, and what is there wins.
pub fn carry_no_commit_pick_into_working_log(
    repo: &Repository,
    pick: &MergeSquashEvent,
    base_commit: &str,
    final_state: &HashMap<String, String>,
    author: &str,
    replace_working_log: bool,
) -> Result<(), AutterError> {
    let mut replace = replace_working_log;
    for source_commit in pick.source_commits() {
        if replace {
            prepare_working_log_after_squash_from_final_state(
                repo,
                &source_commit,
                base_commit,
                final_state,
                author,
            )?;
            replace = false;
        } else {
            merge_working_log_after_squash_from_final_state(
                repo,
                &source_commit,
                base_commit,
                final_state,
                author,
            )?;
        }
    }
    Ok(())
}
