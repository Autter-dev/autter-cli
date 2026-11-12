//! Carries the attribution of `git cherry-pick -n` into the commit that
//! follows it.
//!
//! One pick can apply several commits, and it can stop on a conflict. Every
//! picked commit contributes its attribution, oldest pick first; a later pick
//! only fills lines no earlier pick and no live work already claims.
//!
//! What the pick put in the tree is worked out from the commits alone: the
//! base commit's version of each file, with every applied commit merged in
//! (`git merge-file`, settling each conflict for the side that was there before
//! the pick). That does not depend on when this runs, on the index or on what
//! the working tree holds by then. Picked attribution is kept only on committed
//! lines that came out of that merge unchanged; lines written to resolve a
//! conflict, or typed again after the pick was thrown away, belong to whoever
//! wrote them, which the working log's own checkpoints say. A file the person
//! resolved with `git checkout --theirs` also keeps the picked attribution of
//! the lines it took from the picked side.

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
use crate::git::rewrite_log::MergeSquashEvent;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

/// File in the base commit's working log listing the paths resolved with
/// `git checkout --theirs` (or `git restore --theirs`) while a pick was
/// pending.
const PICKED_SIDE_PATHS_FILE: &str = "no_commit_pick_picked_side_paths.json";

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
    let applied = AppliedPick::new(repo, pick, base_commit);
    let picked_side_paths = read_picked_side_paths(&working_log);

    // Every committed file, not only the ones the pick changed: the picked
    // commits' attribution is read from their whole line of history, and must
    // not land on lines the pick never applied.
    let mut committed: Vec<(&String, &String)> = final_state.iter().collect();
    committed.sort();
    let mut files: Vec<PickedFile<'_>> = Vec::new();
    for (path, final_content) in committed {
        let taken_from_picked_side = path_matches_any(path, &picked_side_paths);
        let Some(origins) = applied.line_origins(
            repo,
            &working_log,
            path,
            final_content,
            taken_from_picked_side,
        ) else {
            continue;
        };
        let tracked_before = release_known_human_claims_on_picked_lines(
            &working_log,
            path,
            final_content,
            &origins,
        )?;
        files.push(PickedFile {
            path,
            final_content,
            origins,
            tracked_before,
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

    for file in &files {
        keep_picked_attribution_where_the_pick_put_lines(
            repo,
            pick,
            base_commit,
            &working_log,
            file,
        )?;
    }

    Ok(())
}

/// Whether the content `read_content` gives for the files a pending pick
/// changed still holds any line the pick brought. `read_content` returns
/// `None` for a file that is not there.
pub fn no_commit_pick_lines_remain(
    repo: &Repository,
    pick: &MergeSquashEvent,
    base_commit: &str,
    read_content: impl Fn(&str) -> Option<String>,
) -> Result<bool, AutterError> {
    let working_log = repo.storage.working_log_for_base_commit(base_commit)?;
    let applied = AppliedPick::new(repo, pick, base_commit);
    let picked_side_paths = read_picked_side_paths(&working_log);
    for path in &applied.changed_files {
        let content = read_content(path).unwrap_or_default();
        let taken_from_picked_side = path_matches_any(path, &picked_side_paths);
        match applied.line_origins(repo, &working_log, path, &content, taken_from_picked_side) {
            Some(origins) => {
                if origins.values().any(|origin| *origin == LineOrigin::Picked) {
                    return Ok(true);
                }
            }
            // A file that cannot be read as text: assume the pick is still
            // there rather than forget it.
            None => return Ok(true),
        }
    }
    Ok(false)
}

/// Records that `pathspecs` (relative to the repository root, `.` for all of
/// it) were resolved by taking the picked side, for the pick pending on
/// `base_commit`.
pub fn record_paths_taken_from_picked_side(
    repo: &Repository,
    base_commit: &str,
    pathspecs: &[String],
) -> Result<(), AutterError> {
    if pathspecs.is_empty() {
        return Ok(());
    }
    let working_log = repo.storage.working_log_for_base_commit(base_commit)?;
    let mut paths = read_picked_side_paths(&working_log);
    paths.extend(pathspecs.iter().cloned());
    let json = serde_json::to_string(&paths)?;
    std::fs::write(working_log.dir.join(PICKED_SIDE_PATHS_FILE), json)?;
    Ok(())
}

/// Forgets the paths [`record_paths_taken_from_picked_side`] recorded for
/// `base_commit`: the pick they belong to was committed or thrown away.
pub fn forget_paths_taken_from_picked_side(repo: &Repository, base_commit: &str) {
    if !repo.storage.has_working_log(base_commit) {
        return;
    }
    let path = repo
        .storage
        .working_logs
        .join(base_commit)
        .join(PICKED_SIDE_PATHS_FILE);
    let _ = std::fs::remove_file(path);
}

fn read_picked_side_paths(working_log: &PersistedWorkingLog) -> BTreeSet<String> {
    std::fs::read_to_string(working_log.dir.join(PICKED_SIDE_PATHS_FILE))
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn path_matches_any(path: &str, pathspecs: &BTreeSet<String>) -> bool {
    pathspecs.iter().any(|spec| {
        let spec = spec.trim_start_matches("./").trim_end_matches('/');
        spec.is_empty()
            || spec == "."
            || path == spec
            || path
                .strip_prefix(spec)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Where a line of some content came from, as far as a pick is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOrigin {
    /// A line the pick brought: it is in what the pick put in the tree and
    /// not in the base commit.
    Picked,
    /// A line of the base commit the pick left alone.
    Base,
}

/// The commits one pick applied, read once for all of its files.
struct AppliedPick {
    /// The tree of the commit the pick was made on; `None` when unreadable.
    base_tree: Option<String>,
    /// `(parent tree, tree)` of each applied commit, in the order applied. A
    /// root commit has no parent tree.
    commits: Vec<(Option<String>, String)>,
    /// Every file an applied commit changed.
    changed_files: BTreeSet<String>,
}

impl AppliedPick {
    fn new(repo: &Repository, pick: &MergeSquashEvent, base_commit: &str) -> Self {
        let tree_of =
            |commit: &crate::git::repository::Commit<'_>| commit.tree().ok().map(|tree| tree.id());
        let base_tree = repo
            .find_commit(base_commit.to_string())
            .ok()
            .and_then(|commit| tree_of(&commit));
        let mut commits = Vec::new();
        let mut changed_files = BTreeSet::new();
        for commit_sha in pick.applied_commits() {
            let Ok(commit) = repo.find_commit(commit_sha.clone()) else {
                continue;
            };
            let Some(tree) = tree_of(&commit) else {
                continue;
            };
            let parent = commit.parent(0).ok();
            if let Some(parent) = parent.as_ref()
                && let Ok(files) = repo.diff_changed_files(&parent.id(), &commit_sha)
            {
                changed_files.extend(files);
            }
            let parent_tree = match parent {
                Some(parent) => match tree_of(&parent) {
                    Some(parent_tree) => Some(parent_tree),
                    None => continue,
                },
                None => None,
            };
            commits.push((parent_tree, tree));
        }
        Self {
            base_tree,
            commits,
            changed_files,
        }
    }

    /// The origin of each line of `content` that the pick accounts for, by
    /// line number. Lines missing from the map are neither the pick's nor
    /// the base commit's. `None` when a version of the file is not text.
    fn line_origins(
        &self,
        repo: &Repository,
        working_log: &PersistedWorkingLog,
        path: &str,
        content: &str,
        taken_from_picked_side: bool,
    ) -> Option<BTreeMap<u32, LineOrigin>> {
        let base = text_at_tree(repo, self.base_tree.as_deref()?, path)?;
        if !self.changed_files.contains(path) {
            return Some(line_origins_through(&base, &base, content));
        }
        let mut merged = base.clone();
        let mut picked_versions = Vec::new();
        for (parent_tree, tree) in &self.commits {
            let before = match parent_tree {
                Some(parent_tree) => text_at_tree(repo, parent_tree, path)?,
                None => String::new(),
            };
            let after = text_at_tree(repo, tree, path)?;
            if before == after {
                continue;
            }
            merged = merge_file_favoring_ours(repo, working_log, &merged, &before, &after)?;
            picked_versions.push(after);
        }

        let mut origins = line_origins_through(&base, &merged, content);
        if taken_from_picked_side {
            for version in &picked_versions {
                for (line, origin) in line_origins_through(&base, version, content) {
                    let known = origins.entry(line).or_insert(origin);
                    if origin == LineOrigin::Picked {
                        *known = LineOrigin::Picked;
                    }
                }
            }
        }
        Some(origins)
    }
}

/// The lines of `content` that are unchanged lines of `version`, each marked
/// with whether `version` has it from `base` or brought it.
fn line_origins_through(base: &str, version: &str, content: &str) -> BTreeMap<u32, LineOrigin> {
    const BASE: &str = "base";
    const PICKED: &str = "picked";
    let from_base = lines_carried_over(base, version);
    let version_lines: Vec<LineAttribution> = (1..=version.lines().count() as u32)
        .map(|line| LineAttribution {
            start_line: line,
            end_line: line,
            author_id: if from_base.contains(&line) {
                BASE
            } else {
                PICKED
            }
            .to_string(),
            overrode: None,
        })
        .collect();
    diff_based_line_attribution_transfer(version, content, &version_lines)
        .into_iter()
        .flat_map(|attr| {
            let origin = if attr.author_id == BASE {
                LineOrigin::Base
            } else {
                LineOrigin::Picked
            };
            (attr.start_line..=attr.end_line).map(move |line| (line, origin))
        })
        .collect()
}

/// `path` as `tree` has it: empty when there is no such file, `None` when it
/// is not text.
fn text_at_tree(repo: &Repository, tree: &str, path: &str) -> Option<String> {
    match repo.read_file_blob_at_tree(tree, Path::new(path)) {
        Ok(bytes) => String::from_utf8(bytes).ok(),
        Err(_) => Some(String::new()),
    }
}

struct PickedFile<'a> {
    path: &'a str,
    final_content: &'a str,
    origins: BTreeMap<u32, LineOrigin>,
    /// The working log's attribution for the file before the pick's went in,
    /// by line of `final_content`. `None` when it could not be read.
    tracked_before: Option<BTreeMap<u32, String>>,
}

/// A known-human checkpoint of a file the pick changed, taken while the pick
/// was pending and with no checkpoint of the file before it, compared the file
/// with the base commit: it claimed every line the pick brought along with the
/// ones the person actually wrote. Takes those claims back from the lines the
/// pick brought, unchanged, so the picked attribution fills them.
///
/// Returns what the working log tracks for the file afterwards, by line, with
/// untracked (`human`) lines left out since they claim nothing.
fn release_known_human_claims_on_picked_lines(
    working_log: &PersistedWorkingLog,
    path: &str,
    content: &str,
    origins: &BTreeMap<u32, LineOrigin>,
) -> Result<Option<BTreeMap<u32, String>>, AutterError> {
    let Some(tracked) = tracked_line_attributions(working_log, path, content) else {
        return Ok(None);
    };
    let mut by_line = author_by_line(&tracked);
    let tracked_lines = by_line.len();
    by_line.retain(|line, author_id| {
        !(is_known_human(author_id) && origins.get(line) == Some(&LineOrigin::Picked))
    });
    if by_line.len() != tracked_lines {
        write_tracked_line_attributions(
            working_log,
            path,
            content,
            line_attributions_from(&by_line),
        )?;
    }
    by_line.retain(|_, author_id| author_id != "human");
    Ok(Some(by_line))
}

fn is_known_human(author_id: &str) -> bool {
    author_id.starts_with("h_")
}

/// After the pick's attribution went in, settles every line the working log
/// did not already track:
/// - a line the pick brought gets the picked commits' own attribution. The
///   squash merge favours the base commit, which can hand a resolved line the
///   attribution of the base line it replaced;
/// - a base line the pick left alone keeps what the merge gave it;
/// - any other line, written after the pick, gets nothing from it.
///
/// Attribution the working log already had is left alone.
fn keep_picked_attribution_where_the_pick_put_lines(
    repo: &Repository,
    pick: &MergeSquashEvent,
    base_commit: &str,
    working_log: &PersistedWorkingLog,
    file: &PickedFile<'_>,
) -> Result<(), AutterError> {
    let (Some(before), Some(after)) = (
        file.tracked_before.as_ref(),
        tracked_line_attributions(working_log, file.path, file.final_content),
    ) else {
        return Ok(());
    };

    let after = author_by_line(&after);
    let picked_lines: Vec<u32> = file
        .origins
        .iter()
        .filter(|(line, origin)| **origin == LineOrigin::Picked && !before.contains_key(line))
        .map(|(line, _)| *line)
        .collect();
    let picked_authors = if picked_lines.is_empty() {
        BTreeMap::new()
    } else {
        picked_commits_attribution(repo, pick, base_commit, file.path, file.final_content)
    };

    let mut kept = BTreeMap::new();
    for (line, author_id) in &after {
        if before.contains_key(line) || file.origins.get(line) == Some(&LineOrigin::Base) {
            kept.insert(*line, author_id.clone());
        }
    }
    for line in picked_lines {
        match picked_authors.get(&line) {
            Some(author_id) => kept.insert(line, author_id.clone()),
            None => kept.remove(&line),
        };
    }
    if kept == after {
        return Ok(());
    }

    write_tracked_line_attributions(
        working_log,
        file.path,
        file.final_content,
        line_attributions_from(&kept),
    )
}

/// The attribution the picked commits themselves give the lines of `content`,
/// read the way the squash reads it (blame from where each picked commit's
/// line of history left the base), earlier picks first.
fn picked_commits_attribution(
    repo: &Repository,
    pick: &MergeSquashEvent,
    base_commit: &str,
    path: &str,
    content: &str,
) -> BTreeMap<u32, String> {
    use crate::authorship::virtual_attribution::VirtualAttributions;

    let mut by_line = BTreeMap::new();
    for source in pick.source_commits() {
        let merge_base = repo
            .merge_base(source.clone(), base_commit.to_string())
            .ok()
            .flatten();
        let Ok(source_va) = smol::block_on(VirtualAttributions::new_for_base_commit(
            repo.clone(),
            source,
            &[path.to_string()],
            merge_base,
        )) else {
            continue;
        };
        let (Some(attrs), Some(source_content)) = (
            source_va.get_line_attributions(path),
            source_va.get_file_content(path),
        ) else {
            continue;
        };
        for attr in diff_based_line_attribution_transfer(source_content, content, attrs) {
            if attr.author_id == "human" {
                continue;
            }
            for line in attr.start_line..=attr.end_line {
                by_line
                    .entry(line)
                    .or_insert_with(|| attr.author_id.clone());
            }
        }
    }
    by_line
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

/// `git merge-file --ours` over three contents: what merges cleanly is
/// applied, and every conflict keeps "ours". Uses files rather than
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

    #[test]
    fn line_origins_tell_picked_lines_from_base_lines_and_new_ones() {
        let base = "one\ntwo\n";
        let picked = "one\ntwo\npicked\n";
        let committed = "typed\none\ntwo\npicked\n";

        assert_eq!(
            line_origins_through(base, picked, committed),
            BTreeMap::from([
                (2, LineOrigin::Base),
                (3, LineOrigin::Base),
                (4, LineOrigin::Picked),
            ])
        );
    }

    #[test]
    fn picked_side_pathspecs_match_files_and_directories() {
        let specs = BTreeSet::from(["src/".to_string(), "a.txt".to_string()]);
        assert!(path_matches_any("a.txt", &specs));
        assert!(path_matches_any("src/lib.rs", &specs));
        assert!(!path_matches_any("srcx/lib.rs", &specs));
        assert!(!path_matches_any("b.txt", &specs));
        assert!(path_matches_any(
            "b.txt",
            &BTreeSet::from([".".to_string()])
        ));
    }
}
