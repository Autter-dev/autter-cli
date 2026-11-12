//! Carries the attribution of `git cherry-pick -n` into the commit that
//! follows it.
//!
//! One pick can apply several commits, and it can stop on a conflict. Every
//! picked commit contributes its attribution, oldest pick first; a later pick
//! only fills lines no earlier pick and no live work already claims. In a file
//! the pick left conflicted, the picked attribution is kept only for lines that
//! merged cleanly: whatever was written to resolve the conflict belongs to
//! whoever resolved it, which the working log's own checkpoints say.

use crate::authorship::attribution_tracker::{
    LineAttribution, attributions_to_line_attributions, line_attributions_to_attributions,
};
use crate::authorship::rebase_authorship::{
    diff_based_line_attribution_transfer, merge_working_log_after_squash_from_final_state,
    prepare_working_log_after_squash_from_final_state,
};
use crate::error::AutterError;
use crate::git::repo_storage::PersistedWorkingLog;
use crate::git::repository::{Repository, exec_git_allow_nonzero};
use crate::git::rewrite_log::{ConflictedFileStages, MergeSquashEvent};
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
    let working_log = repo.storage.working_log_for_base_commit(base_commit)?;

    // What the pick merged cleanly into each conflicted file, and what the
    // working log already said about those files before the pick's attribution
    // goes in.
    let mut conflicted: Vec<ConflictedFile<'_>> = Vec::new();
    for (path, stages) in &pick.conflicted_files {
        let Some(final_content) = final_state.get(path) else {
            continue;
        };
        conflicted.push(ConflictedFile {
            path,
            final_content,
            cleanly_merged: cleanly_merged_content(repo, &working_log, stages),
            tracked_before: tracked_line_attributions(&working_log, path, final_content),
        });
    }

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

    for file in &conflicted {
        drop_picked_attribution_outside_clean_merge(&working_log, file)?;
    }

    Ok(())
}

struct ConflictedFile<'a> {
    path: &'a str,
    final_content: &'a str,
    /// The file as the pick merged it, with every conflicted hunk left as it
    /// was before the pick. `None` when that could not be worked out.
    cleanly_merged: Option<String>,
    /// The working log's attribution for the file before the pick's went in,
    /// lined up with `final_content`. `None` when it could not be read.
    tracked_before: Option<Vec<LineAttribution>>,
}

/// After the pick's attribution was merged into a conflicted file, removes it
/// from every line that did not come out of the clean part of the merge.
/// Attribution the working log already had is left alone.
fn drop_picked_attribution_outside_clean_merge(
    working_log: &PersistedWorkingLog,
    file: &ConflictedFile<'_>,
) -> Result<(), AutterError> {
    let (Some(before), Some(after)) = (
        file.tracked_before.as_ref(),
        tracked_line_attributions(working_log, file.path, file.final_content),
    ) else {
        return Ok(());
    };

    // Without the clean merge there is no telling picked lines from resolved
    // ones, so none of the file's lines keep picked attribution.
    let clean_lines = file
        .cleanly_merged
        .as_deref()
        .map(|clean| lines_carried_over(clean, file.final_content))
        .unwrap_or_default();

    let before = author_by_line(before);
    let mut kept = author_by_line(&after);
    let tracked_lines = kept.len();
    kept.retain(|line, _| before.contains_key(line) || clean_lines.contains(line));
    if kept.len() == tracked_lines {
        return Ok(());
    }

    write_tracked_line_attributions(
        working_log,
        file.path,
        file.final_content,
        line_attributions_from(&kept),
    )
}

/// The line numbers of `new_content` that are unchanged lines of `old_content`.
fn lines_carried_over(old_content: &str, new_content: &str) -> BTreeSet<u32> {
    let old_line_count = old_content.lines().count() as u32;
    if old_line_count == 0 {
        return BTreeSet::new();
    }
    let every_old_line = [LineAttribution {
        start_line: 1,
        end_line: old_line_count,
        author_id: String::new(),
        overrode: None,
    }];
    diff_based_line_attribution_transfer(old_content, new_content, &every_old_line)
        .iter()
        .flat_map(|attr| attr.start_line..=attr.end_line)
        .collect()
}

fn author_by_line(attrs: &[LineAttribution]) -> BTreeMap<u32, String> {
    let mut by_line = BTreeMap::new();
    for attr in attrs {
        for line in attr.start_line..=attr.end_line {
            by_line.insert(line, attr.author_id.clone());
        }
    }
    by_line
}

fn line_attributions_from(by_line: &BTreeMap<u32, String>) -> Vec<LineAttribution> {
    let mut attrs: Vec<LineAttribution> = Vec::new();
    for (line, author_id) in by_line {
        if let Some(last) = attrs.last_mut()
            && last.end_line + 1 == *line
            && last.author_id == *author_id
        {
            last.end_line = *line;
            continue;
        }
        attrs.push(LineAttribution {
            start_line: *line,
            end_line: *line,
            author_id: author_id.clone(),
            overrode: None,
        });
    }
    attrs
}

/// The attribution the working log holds for `path`, lined up with `content`.
///
/// When a commit is built, the newest checkpoint entry with attribution data
/// for a file stands in for INITIAL, so that entry is read when there is one
/// and INITIAL otherwise. Returns `None` when the entry's content is missing.
fn tracked_line_attributions(
    working_log: &PersistedWorkingLog,
    path: &str,
    content: &str,
) -> Option<Vec<LineAttribution>> {
    let checkpoints = working_log.read_all_checkpoints().unwrap_or_default();
    let live_entry = checkpoints
        .iter()
        .rev()
        .flat_map(|checkpoint| checkpoint.entries.iter())
        .find(|entry| entry_tracks_file(entry, path));
    if let Some(entry) = live_entry {
        let live_content = working_log.get_file_version(&entry.blob_sha).ok()?;
        let live_attrs = if entry.line_attributions.is_empty() {
            attributions_to_line_attributions(&entry.attributions, &live_content)
        } else {
            entry.line_attributions.clone()
        };
        return Some(diff_based_line_attribution_transfer(
            &live_content,
            content,
            &live_attrs,
        ));
    }

    let initial = working_log.read_initial_attributions();
    let existing = initial.files.get(path).cloned().unwrap_or_default();
    if existing.is_empty() {
        return Some(existing);
    }
    Some(
        match working_log.stored_initial_file_content_from(&initial, path) {
            Some(stored) => diff_based_line_attribution_transfer(&stored, content, &existing),
            None => existing,
        },
    )
}

/// Replaces the attribution the working log holds for `path`, in the place
/// [`tracked_line_attributions`] reads it from. `content` is what `attrs` are
/// lined up with.
fn write_tracked_line_attributions(
    working_log: &PersistedWorkingLog,
    path: &str,
    content: &str,
    attrs: Vec<LineAttribution>,
) -> Result<(), AutterError> {
    let mut checkpoints = working_log.read_all_checkpoints().unwrap_or_default();
    let live_entry = checkpoints
        .iter_mut()
        .rev()
        .flat_map(|checkpoint| checkpoint.entries.iter_mut())
        .find(|entry| entry_tracks_file(entry, path));
    if let Some(entry) = live_entry {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        entry.attributions = line_attributions_to_attributions(&attrs, content, now_ms);
        entry.line_attributions = attrs;
        entry.blob_sha = working_log.persist_file_version(content)?;
        return working_log.write_all_checkpoints(&checkpoints);
    }

    let mut initial = working_log.read_initial_attributions();
    if attrs.is_empty() {
        initial.files.remove(path);
        initial.file_blobs.remove(path);
    } else {
        initial.files.insert(path.to_string(), attrs);
        initial
            .file_blobs
            .insert(path.to_string(), working_log.persist_file_version(content)?);
    }
    // Checkpoint entries of other files may point at the picked commits'
    // sessions, whose records only INITIAL holds.
    working_log.write_initial_keeping_records(initial)
}

fn entry_tracks_file(entry: &crate::authorship::working_log::WorkingLogEntry, path: &str) -> bool {
    entry.file == path && !(entry.line_attributions.is_empty() && entry.attributions.is_empty())
}

/// The content a conflicted file would have had if only the hunks that merged
/// cleanly had been applied: a three-way merge of the index stages with every
/// conflict settled for "ours", the side that was there before the pick.
fn cleanly_merged_content(
    repo: &Repository,
    working_log: &PersistedWorkingLog,
    stages: &ConflictedFileStages,
) -> Option<String> {
    let Some(ours) = stages.ours.as_deref() else {
        // Deleted on our side: nothing of the pick merged into it.
        return Some(String::new());
    };
    let ours = blob_text(repo, ours)?;
    let Some(theirs) = stages.theirs.as_deref() else {
        // Deleted by the pick: it brought no lines.
        return Some(ours);
    };
    let theirs = blob_text(repo, theirs)?;
    let base = match stages.base.as_deref() {
        Some(base) => blob_text(repo, base)?,
        None => String::new(),
    };
    merge_file_favoring_ours(repo, working_log, &ours, &base, &theirs)
}

fn blob_text(repo: &Repository, oid: &str) -> Option<String> {
    let bytes = repo.find_blob(oid.to_string()).ok()?.content().ok()?;
    String::from_utf8(bytes).ok()
}

/// `git merge-file --ours` over three contents. Uses files rather than
/// `--object-id` so it works with every git version.
fn merge_file_favoring_ours(
    repo: &Repository,
    working_log: &PersistedWorkingLog,
    ours: &str,
    base: &str,
    theirs: &str,
) -> Option<String> {
    let scratch = working_log.dir.join(format!(
        "merge-file-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&scratch).ok()?;
    let merged = (|| {
        let ours_path = scratch.join("ours");
        let base_path = scratch.join("base");
        let theirs_path = scratch.join("theirs");
        std::fs::write(&ours_path, ours).ok()?;
        std::fs::write(&base_path, base).ok()?;
        std::fs::write(&theirs_path, theirs).ok()?;

        let mut args = repo.global_args_for_exec();
        args.extend(
            [
                "merge-file".to_string(),
                "-p".to_string(),
                "--ours".to_string(),
            ]
            .into_iter()
            .chain(
                [&ours_path, &base_path, &theirs_path]
                    .into_iter()
                    .map(|path| path.to_string_lossy().to_string()),
            ),
        );
        let output = exec_git_allow_nonzero(&args).ok()?;
        // The exit code counts the conflicts; anything else is a failure.
        match output.status.code() {
            Some(0..=127) => String::from_utf8(output.stdout).ok(),
            _ => None,
        }
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(start_line: u32, end_line: u32, author_id: &str) -> LineAttribution {
        LineAttribution {
            start_line,
            end_line,
            author_id: author_id.to_string(),
            overrode: None,
        }
    }

    #[test]
    fn lines_carried_over_skips_lines_written_to_resolve_a_conflict() {
        // The pick merged "picked" cleanly and conflicted on line 2, where
        // "ours" was kept. The resolution then replaced line 2 by hand.
        let cleanly_merged = "one\nours\nthree\npicked\n";
        let committed = "one\nresolved by hand\nthree\npicked\n";

        assert_eq!(
            lines_carried_over(cleanly_merged, committed),
            BTreeSet::from([1, 3, 4])
        );
    }

    #[test]
    fn lines_carried_over_is_empty_without_a_clean_merge() {
        assert!(lines_carried_over("", "one\ntwo\n").is_empty());
    }

    #[test]
    fn line_attributions_round_trip_through_the_line_map() {
        let attrs = vec![attr(1, 2, "a"), attr(3, 3, "b"), attr(5, 6, "a")];
        assert_eq!(line_attributions_from(&author_by_line(&attrs)), attrs);
    }
}
