use crate::authorship::authorship_log_serialization::{
    AuthorshipLog, generate_session_id, generate_short_hash,
};
use crate::authorship::ignore::{
    build_ignore_matcher, effective_ignore_patterns, should_ignore_file_with_matcher,
};
use crate::authorship::stats::{stats_for_commit_stats_from_hunks, write_stats_to_terminal};
use crate::authorship::virtual_attribution::VirtualAttributions;
use crate::authorship::working_log::{Checkpoint, CheckpointKind, WorkingLogEntry};
use crate::config::Config;
use crate::error::AutterError;
use crate::git::notes_api::write_note as notes_add;
use crate::git::repository::Repository;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;

/// Skip expensive post-commit stats when this threshold is exceeded.
/// High hunk density is the strongest predictor of slow diff_ai_accepted_stats.
#[doc(hidden)]
pub const STATS_SKIP_MAX_HUNKS: usize = 1000;
/// Skip expensive stats for very large net additions even if hunks are moderate.
#[doc(hidden)]
pub const STATS_SKIP_MAX_ADDED_LINES: usize = 6000;
/// Skip expensive stats for extremely wide commits touching many added-line files.
#[doc(hidden)]
pub const STATS_SKIP_MAX_FILES_WITH_ADDITIONS: usize = 200;
/// Skip expensive stats for commits that delete a large number of lines.
/// Deletion-heavy commits (e.g. removing many files) trigger the same expensive
/// diff-parsing path as large addition commits, but the added-lines estimate is
/// near zero, so the cost was previously invisible to the estimator.
#[doc(hidden)]
pub const STATS_SKIP_MAX_DELETED_LINES: usize = 6000;

#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct StatsCostEstimate {
    pub files_with_additions: usize,
    pub added_lines: usize,
    pub hunk_ranges: usize,
    pub deleted_lines: usize,
}

fn checkpoint_entry_requires_post_processing(
    checkpoint: &Checkpoint,
    entry: &WorkingLogEntry,
) -> bool {
    if checkpoint.kind != CheckpointKind::Human {
        return true;
    }

    entry
        .line_attributions
        .iter()
        .any(|attr| attr.author_id != CheckpointKind::Human.to_str() || attr.overrode.is_some())
        || entry
            .attributions
            .iter()
            .any(|attr| attr.author_id != CheckpointKind::Human.to_str())
}

pub fn post_commit(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    supress_output: bool,
) -> Result<(String, AuthorshipLog), AutterError> {
    post_commit_with_final_state(
        repo,
        base_commit,
        commit_sha,
        human_author,
        supress_output,
        None,
    )
}

pub fn post_commit_with_final_state(
    repo: &Repository,
    base_commit: Option<String>,
    commit_sha: String,
    human_author: String,
    supress_output: bool,
    final_state_override: Option<&HashMap<String, String>>,
) -> Result<(String, AuthorshipLog), AutterError> {
    // Use base_commit parameter if provided, otherwise use "initial" for empty repos
    // This matches the convention in checkpoint.rs
    let parent_sha = base_commit.unwrap_or_else(|| "initial".to_string());

    // Initialize the new storage system
    let repo_storage = &repo.storage;
    let working_log = repo_storage.working_log_for_base_commit(&parent_sha)?;

    let parent_working_log = working_log.read_all_checkpoints()?;

    // Create VirtualAttributions from working log (fast path - no blame)
    // We don't need to run blame because we only care about the working log data
    // that was accumulated since the parent commit
    let working_va = if let Some(snapshot) = final_state_override {
        VirtualAttributions::from_working_log_snapshot(
            repo.clone(),
            parent_sha.clone(),
            Some(human_author.clone()),
            snapshot,
        )?
    } else {
        VirtualAttributions::from_just_working_log(
            repo.clone(),
            parent_sha.clone(),
            Some(human_author.clone()),
        )?
    };

    // Build pathspecs from AI-relevant checkpoint entries only.
    // Human-only entries with no AI attribution do not affect authorship output and should not
    // trigger expensive post-commit diff work across large commits.
    let mut pathspecs: HashSet<String> = HashSet::new();
    for checkpoint in &parent_working_log {
        for entry in &checkpoint.entries {
            if checkpoint_entry_requires_post_processing(checkpoint, entry) {
                pathspecs.insert(entry.file.clone());
            }
        }
    }

    // Also include files from INITIAL attributions (uncommitted files from previous commits)
    // These files may not have checkpoints but still need their attribution preserved
    // when they are finally committed. See issue #356.
    let initial_attributions_for_pathspecs = working_log.read_initial_attributions();
    for file_path in initial_attributions_for_pathspecs.files.keys() {
        pathspecs.insert(file_path.clone());
    }

    let (mut authorship_log, initial_attributions) = working_va
        .to_authorship_log_and_initial_working_log(
            repo,
            &parent_sha,
            &commit_sha,
            Some(&pathspecs),
            final_state_override,
        )?;

    authorship_log.metadata.base_commit_sha = commit_sha.clone();

    // No-hooks background agents (Devin, Codex Cloud, etc.) may not fire checkpoints
    // for all edits. Attribute any committed lines that have no existing attestation
    // ("holes") to the detected agent, preserving explicit attributions.
    if !matches!(
        crate::authorship::background_agent::detect(),
        crate::authorship::background_agent::BackgroundAgent::None
            | crate::authorship::background_agent::BackgroundAgent::WithHooks { .. }
    ) {
        let diff_base = if parent_sha == "initial" {
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        } else {
            &parent_sha
        };
        if let Ok(added_lines) = repo.diff_added_lines(diff_base, &commit_sha, None) {
            let committed_hunks: HashMap<
                String,
                Vec<crate::authorship::authorship_log::LineRange>,
            > = added_lines
                .into_iter()
                .filter(|(_, lines)| !lines.is_empty())
                .map(|(path, lines)| {
                    (
                        path,
                        crate::authorship::authorship_log::LineRange::compress_lines(&lines),
                    )
                })
                .collect();
            crate::authorship::background_agent::fill_unattributed_lines(
                &mut authorship_log,
                &committed_hunks,
                &human_author,
            );
        }
    }

    // Long-lived daemon processes should read a fresh config snapshot.
    // Always use Config::fresh() to support runtime config updates
    // (especially important for daemon mode, but also good for consistency)
    let config = Config::fresh();
    let custom_attrs = config.custom_attributes().clone();

    // Inject custom attributes into all PromptRecords and SessionRecords.
    if !custom_attrs.is_empty() {
        for pr in authorship_log.metadata.prompts.values_mut() {
            pr.custom_attributes = Some(custom_attrs.clone());
        }
        for sr in authorship_log.metadata.sessions.values_mut() {
            sr.custom_attributes = Some(custom_attrs.clone());
        }
    }

    // Bridge each AI session's prompt transcript into the CAS queue and record
    // the resulting `cas:<hash>` on its PromptRecord/SessionRecord so the note
    // links back to the conversation that produced the code. In `Default` mode
    // the daemon uploads queued objects to the cloud; in `Local` mode they stay
    // on disk only. `Notes` mode stores transcripts in git notes instead.
    let storage_mode = config.effective_prompt_storage(&Some(repo.clone()));
    if matches!(
        storage_mode,
        crate::config::PromptStorageMode::Default | crate::config::PromptStorageMode::Local
    ) {
        crate::authorship::cas_bridge::enqueue_prompt_transcripts(
            &mut authorship_log.metadata.prompts,
            &mut authorship_log.metadata.sessions,
            &parent_working_log,
        );
    }

    let authorship_note_str = authorship_log
        .serialize_to_string()
        .map_err(|_| AutterError::Generic("Failed to serialize authorship log".to_string()))?;

    notes_add(repo, &commit_sha, &authorship_note_str)?;

    // Compute stats once (needed for both metrics and terminal output), unless preflight
    // estimate predicts this would be too expensive for the commit hook path.
    let mut stats: Option<crate::authorship::stats::CommitStats> = None;
    let is_merge_commit = repo
        .find_commit(commit_sha.clone())
        .map(|commit| commit.parent_count().unwrap_or(0) > 1)
        .unwrap_or(false);
    let ignore_patterns = effective_ignore_patterns(repo, &[], &[]);
    let skip_reason = if is_merge_commit {
        Some(StatsSkipReason::MergeCommit)
    } else {
        estimate_stats_cost(repo, &parent_sha, &commit_sha, &ignore_patterns)
            .ok()
            .and_then(|estimate| {
                if should_skip_expensive_post_commit_stats(&estimate) {
                    Some(StatsSkipReason::Expensive(estimate))
                } else {
                    None
                }
            })
    };

    if skip_reason.is_none() {
        let diff_base = if parent_sha == "initial" {
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        } else {
            &parent_sha
        };

        let diff_hunks =
            crate::commands::diff::get_diff_with_line_numbers(repo, diff_base, &commit_sha)?;

        let computed = stats_for_commit_stats_from_hunks(
            repo,
            &commit_sha,
            &ignore_patterns,
            &diff_hunks,
            Some(&authorship_log),
        )?;

        let hunks_json = crate::commands::diff::build_diff_artifacts_from_hunks(
            repo,
            diff_hunks,
            &commit_sha,
            Some(&authorship_log),
        )
        .ok()
        .and_then(|artifacts| serde_json::to_string(&artifacts.json_hunks).ok());

        // Record metrics only when we have full stats.
        record_commit_metrics(
            repo,
            &commit_sha,
            &parent_sha,
            &human_author,
            &authorship_note_str,
            &computed,
            &parent_working_log,
            hunks_json.as_deref(),
        );
        upload_commit_authorship_summary(
            repo,
            &commit_sha,
            &parent_sha,
            &human_author,
            &computed,
            &authorship_log,
            &parent_working_log,
            hunks_json.as_deref(),
        );
        stats = Some(computed);
    } else {
        match skip_reason.as_ref() {
            Some(StatsSkipReason::MergeCommit) => {
                tracing::debug!("Skipping post-commit stats for merge commit {}", commit_sha);
            }
            Some(StatsSkipReason::Expensive(estimate)) => {
                tracing::debug!(
                    "Skipping expensive post-commit stats for {} (files_with_additions={}, added_lines={}, deleted_lines={}, hunks={})",
                    commit_sha,
                    estimate.files_with_additions,
                    estimate.added_lines,
                    estimate.deleted_lines,
                    estimate.hunk_ranges
                );
            }
            None => {}
        }
    }

    // Write INITIAL file for uncommitted AI attributions (if any)
    if !initial_attributions.files.is_empty() {
        let new_working_log = repo_storage.working_log_for_base_commit(&commit_sha)?;
        let initial_file_contents =
            working_va.snapshot_contents_for_files(initial_attributions.files.keys());
        new_working_log.write_initial_attributions_with_contents(
            initial_attributions.files,
            initial_attributions.prompts,
            initial_attributions.humans,
            initial_file_contents,
            initial_attributions.sessions,
        )?;
    }

    // // Clean up old working log
    repo_storage.delete_working_log_for_base_commit(&parent_sha)?;

    // Use Config::fresh() to support runtime config updates
    if !supress_output && !Config::fresh().is_quiet() {
        // Only print stats if we're in an interactive terminal and quiet mode is disabled
        let is_interactive = std::io::stdout().is_terminal();
        if let Some(stats) = stats.as_ref() {
            write_stats_to_terminal(stats, is_interactive);
        } else {
            match skip_reason.as_ref() {
                Some(StatsSkipReason::MergeCommit) => {
                    eprintln!(
                        "[autter] Skipped autter stats for merge commit {}.",
                        commit_sha
                    );
                }
                Some(StatsSkipReason::Expensive(estimate)) => {
                    eprintln!(
                        "[autter] Skipped autter stats for large commit (files_with_additions={}, added_lines={}, deleted_lines={}, hunks={}). Run `autter stats {}` to compute stats on demand.",
                        estimate.files_with_additions,
                        estimate.added_lines,
                        estimate.deleted_lines,
                        estimate.hunk_ranges,
                        commit_sha
                    );
                }
                None => {}
            }
        }
    }
    Ok((commit_sha.to_string(), authorship_log))
}

#[derive(Debug, Clone)]
enum StatsSkipReason {
    MergeCommit,
    Expensive(StatsCostEstimate),
}

#[doc(hidden)]
pub fn should_skip_expensive_post_commit_stats(estimate: &StatsCostEstimate) -> bool {
    estimate.hunk_ranges >= STATS_SKIP_MAX_HUNKS
        || estimate.added_lines >= STATS_SKIP_MAX_ADDED_LINES
        || estimate.files_with_additions >= STATS_SKIP_MAX_FILES_WITH_ADDITIONS
        || estimate.deleted_lines >= STATS_SKIP_MAX_DELETED_LINES
}

/// Public result of the stats cost estimate for a commit, used by the async
/// wrapper path to decide whether to skip expensive stats computation.
pub struct StatsSkipEstimate {
    should_skip: bool,
}

impl StatsSkipEstimate {
    pub fn should_skip(&self) -> bool {
        self.should_skip
    }
}

/// Estimate whether stats computation for `commit_sha` would be too expensive.
/// Resolves the parent commit automatically. Intended for callers outside the
/// normal post-commit flow (e.g. the async wrapper path).
pub fn estimate_stats_cost_for_head(
    repo: &Repository,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<StatsSkipEstimate, AutterError> {
    let commit = repo.find_commit(commit_sha.to_string())?;
    let parent_sha = if commit.parent_count().unwrap_or(0) > 0 {
        commit
            .parent(0)
            .map(|p| p.id())
            .unwrap_or_else(|_| "initial".to_string())
    } else {
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
    };
    estimate_stats_cost_for_commit_range(repo, &parent_sha, commit_sha, ignore_patterns)
}

pub fn estimate_stats_cost_for_commit_range(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<StatsSkipEstimate, AutterError> {
    let estimate = estimate_stats_cost(repo, parent_sha, commit_sha, ignore_patterns)?;
    Ok(StatsSkipEstimate {
        should_skip: should_skip_expensive_post_commit_stats(&estimate),
    })
}

fn estimate_stats_cost(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    ignore_patterns: &[String],
) -> Result<StatsCostEstimate, AutterError> {
    let (mut added_lines_by_file, total_deleted_lines) =
        repo.diff_added_lines_with_deleted_count(parent_sha, commit_sha)?;
    let ignore_matcher = build_ignore_matcher(ignore_patterns);
    added_lines_by_file
        .retain(|file_path, _| !should_ignore_file_with_matcher(file_path, &ignore_matcher));

    let files_with_additions = added_lines_by_file
        .values()
        .filter(|lines| !lines.is_empty())
        .count();

    let mut added_lines = 0usize;
    let mut hunk_ranges = 0usize;

    for (_file, lines) in added_lines_by_file {
        if lines.is_empty() {
            continue;
        }
        added_lines += lines.len();
        hunk_ranges += count_line_ranges(&lines);
    }

    Ok(StatsCostEstimate {
        files_with_additions,
        added_lines,
        hunk_ranges,
        deleted_lines: total_deleted_lines,
    })
}

#[doc(hidden)]
pub fn count_line_ranges(lines: &[u32]) -> usize {
    if lines.is_empty() {
        return 0;
    }

    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut ranges = 1usize;
    let mut prev = sorted[0];
    for &line in &sorted[1..] {
        if line != prev + 1 {
            ranges += 1;
        }
        prev = line;
    }
    ranges
}

/// Record metrics for a committed change.
/// This is a best-effort operation - failures are silently ignored.
#[allow(clippy::too_many_arguments)]
fn record_commit_metrics(
    repo: &Repository,
    commit_sha: &str,
    parent_sha: &str,
    human_author: &str,
    authorship_note: &str,
    stats: &crate::authorship::stats::CommitStats,
    checkpoints: &[Checkpoint],
    hunks_json: Option<&str>,
) {
    use crate::metrics::{CommittedValues, EventAttributes, record};

    // Never emit telemetry for mock_ai (test preset).  If every tool in the
    // breakdown is mock_ai the entire committed event is test data.
    let only_mock_ai = !stats.tool_model_breakdown.is_empty()
        && stats
            .tool_model_breakdown
            .keys()
            .all(|k| k.starts_with("mock_ai::"));
    if only_mock_ai {
        return;
    }

    // Subtract mock_ai contributions from the aggregates so the "all" entry
    // only reflects real tools.
    let mut agg_ai = stats.ai_additions;
    let mut agg_accepted = stats.ai_accepted;
    for (key, ts) in &stats.tool_model_breakdown {
        if key.starts_with("mock_ai::") {
            agg_ai = agg_ai.saturating_sub(ts.ai_additions);
            agg_accepted = agg_accepted.saturating_sub(ts.ai_accepted);
        }
    }

    // Build parallel arrays: index 0 = "all" (aggregate), index 1+ = per tool/model
    let mut tool_model_pairs: Vec<String> = vec!["all".to_string()];
    let mut ai_additions: Vec<u32> = vec![agg_ai];
    let mut ai_accepted: Vec<u32> = vec![agg_accepted];

    // Add per-tool/model breakdown, skipping mock_ai (test preset)
    for (tool_model, tool_stats) in &stats.tool_model_breakdown {
        if tool_model.starts_with("mock_ai::") {
            continue;
        }
        tool_model_pairs.push(tool_model.clone());
        ai_additions.push(tool_stats.ai_additions);
        ai_accepted.push(tool_stats.ai_accepted);
    }

    // Build values with all stats
    let values = CommittedValues::new()
        .human_additions(stats.human_additions)
        .git_diff_deleted_lines(stats.git_diff_deleted_lines)
        .git_diff_added_lines(stats.git_diff_added_lines)
        .tool_model_pairs(tool_model_pairs)
        .ai_additions(ai_additions)
        .ai_accepted(ai_accepted);

    // Add first checkpoint timestamp (null if no checkpoints)
    let values = if let Some(first) = checkpoints.first() {
        values.first_checkpoint_ts(first.timestamp)
    } else {
        values.first_checkpoint_ts_null()
    };

    // Add commit subject and body
    let values = if let Ok(commit) = repo.find_commit(commit_sha.to_string()) {
        let subject = commit.summary().unwrap_or_default();
        let values = values.commit_subject(subject);
        let body = commit.body().unwrap_or_default();
        if body.is_empty() {
            values.commit_body_null()
        } else {
            values.commit_body(body)
        }
    } else {
        values.commit_subject_null().commit_body_null()
    };

    let values = values.authorship_note(authorship_note);

    let values = if let Some(hunks) = hunks_json {
        values.hunks(hunks)
    } else {
        values.hunks_null()
    };

    // Build attributes - start with version and extract session_id from first AI checkpoint
    // session_id links this commit to the AI agent conversation that produced it
    // Note: session_id removed from committed events - commits can contain code from multiple AI sessions
    let mut attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"));

    attrs = attrs
        .author(human_author)
        .commit_sha(commit_sha)
        .base_commit_sha(parent_sha);

    // Get repo URL from default remote
    if let Ok(Some(remote_name)) = repo.get_default_remote()
        && let Ok(remotes) = repo.remotes_with_urls()
        && let Some((_, url)) = remotes.into_iter().find(|(n, _)| n == &remote_name)
        && let Ok(normalized) = crate::repo_url::normalize_repo_url(&url)
    {
        attrs = attrs.repo_url(normalized);
    }

    // Get current branch
    if let Ok(head_ref) = repo.head()
        && let Ok(short_branch) = head_ref.shorthand()
    {
        attrs = attrs.branch(short_branch);
    }

    // Attach custom attributes using Config::fresh() to support runtime config updates
    attrs = attrs.custom_attributes_map(Config::fresh().custom_attributes());

    // Record the metric
    record(values, attrs);
}

#[allow(clippy::too_many_arguments)]
fn upload_commit_authorship_summary(
    repo: &Repository,
    commit_sha: &str,
    parent_sha: &str,
    human_author: &str,
    stats: &crate::authorship::stats::CommitStats,
    authorship_log: &AuthorshipLog,
    checkpoints: &[Checkpoint],
    hunks_json: Option<&str>,
) {
    use crate::api::client::{
        ApiClient, ApiContext, access_token_for_org, resolve_org_for_repo_cached,
    };
    use crate::api::org_db::{self, CommitAuthorshipSummaryRow};

    let repo_url = current_repo_url(repo);
    let client = if let Some(repo_url) = repo_url.as_deref()
        && let Some(org) = resolve_org_for_repo_cached(repo_url)
    {
        let Some(token) = access_token_for_org(&org) else {
            return;
        };
        ApiClient::new(ApiContext::with_auth(None, token))
    } else {
        ApiClient::new(ApiContext::new(None))
    };

    if !client.is_logged_in() {
        return;
    }

    let Ok(identity) = client.org_identity() else {
        return;
    };

    let total = stats.git_diff_added_lines as f64;
    let percent = |count: u32| {
        if total <= 0.0 {
            0.0
        } else {
            ((count as f64 / total) * 10000.0).round() / 100.0
        }
    };

    let prompts = build_commit_prompt_summaries(authorship_log, checkpoints);
    let hunks = hunks_json
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    let tool_model_breakdown =
        serde_json::to_value(&stats.tool_model_breakdown).unwrap_or_else(|_| serde_json::json!({}));

    let row = CommitAuthorshipSummaryRow {
        commit_sha: commit_sha.to_string(),
        repo_url,
        branch: current_branch(repo),
        base_commit_sha: parent_sha.to_string(),
        human_author: human_author.to_string(),
        git_diff_added_lines: stats.git_diff_added_lines as u64,
        git_diff_deleted_lines: stats.git_diff_deleted_lines as u64,
        human_additions: stats.human_additions as u64,
        ai_additions: stats.ai_additions as u64,
        ai_accepted: stats.ai_accepted as u64,
        unknown_additions: stats.unknown_additions as u64,
        human_percent: percent(stats.human_additions),
        ai_percent: percent(stats.ai_accepted),
        unknown_percent: percent(stats.unknown_additions),
        tool_model_breakdown,
        prompts,
        hunks,
    };

    if let Err(e) = org_db::upsert_commit_authorship_summary(
        &identity,
        &row,
        &crate::config::get_or_create_distinct_id(),
    ) {
        tracing::warn!(%e, commit_sha, "commit authorship summary upload failed");
    }
}

fn current_repo_url(repo: &Repository) -> Option<String> {
    repo.get_default_remote()
        .ok()
        .flatten()
        .and_then(|remote_name| {
            repo.remotes_with_urls()
                .ok()
                .and_then(|remotes| remotes.into_iter().find(|(name, _)| name == &remote_name))
                .map(|(_, url)| url)
        })
        .and_then(|url| crate::repo_url::normalize_repo_url(&url).ok())
}

fn current_branch(repo: &Repository) -> Option<String> {
    repo.head().ok().and_then(|head_ref| head_ref.shorthand().ok())
}

fn transcript_paths_by_prompt_key(checkpoints: &[Checkpoint]) -> HashMap<String, String> {
    let mut transcript_paths = HashMap::new();
    for checkpoint in checkpoints {
        let (Some(agent_id), Some(metadata)) = (&checkpoint.agent_id, &checkpoint.agent_metadata)
        else {
            continue;
        };
        let Some(path) = metadata
            .get("transcript_path")
            .or_else(|| metadata.get("chat_session_path"))
            .or_else(|| metadata.get("session_path"))
        else {
            continue;
        };
        transcript_paths.insert(
            generate_short_hash(&agent_id.id, &agent_id.tool),
            path.clone(),
        );
        transcript_paths.insert(
            generate_session_id(&agent_id.id, &agent_id.tool),
            path.clone(),
        );
    }
    transcript_paths
}

fn first_user_prompts_by_path(paths: impl Iterator<Item = String>) -> HashMap<String, String> {
    let mut prompts = HashMap::new();
    for path in paths {
        if prompts.contains_key(&path) {
            continue;
        }
        if let Ok(Some(prompt)) =
            crate::authorship::cas_bridge::first_user_prompt_from_transcript_file(&path)
        {
            prompts.insert(path, prompt);
        }
    }
    prompts
}

fn build_commit_prompt_summaries(
    authorship_log: &AuthorshipLog,
    checkpoints: &[Checkpoint],
) -> serde_json::Value {
    let transcript_paths = transcript_paths_by_prompt_key(checkpoints);
    let first_prompts = first_user_prompts_by_path(transcript_paths.values().cloned());
    let mut rows = Vec::new();

    for (prompt_id, prompt_record) in &authorship_log.metadata.prompts {
        let transcript_path = transcript_paths.get(prompt_id);
        rows.push(serde_json::json!({
            "id": prompt_id,
            "kind": "prompt",
            "agent_id": prompt_record.agent_id,
            "human_author": prompt_record.human_author,
            "messages_url": prompt_record.messages_url,
            "original_user_prompt": transcript_path.and_then(|path| first_prompts.get(path)),
            "total_additions": prompt_record.total_additions,
            "total_deletions": prompt_record.total_deletions,
            "accepted_lines": prompt_record.accepted_lines,
            "overriden_lines": prompt_record.overriden_lines,
        }));
    }

    let sessions: BTreeMap<_, _> = authorship_log.metadata.sessions.iter().collect();
    for (session_id, session_record) in sessions {
        let transcript_path = transcript_paths.get(session_id);
        rows.push(serde_json::json!({
            "id": session_id,
            "kind": "session",
            "agent_id": session_record.agent_id,
            "human_author": session_record.human_author,
            "messages_url": session_record.messages_url,
            "original_user_prompt": transcript_path.and_then(|path| first_prompts.get(path)),
        }));
    }

    serde_json::Value::Array(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log::SessionRecord;
    use crate::authorship::working_log::AgentId;

    #[test]
    fn test_count_line_ranges_handles_scattered_and_contiguous_lines() {
        assert_eq!(count_line_ranges(&[]), 0);
        assert_eq!(count_line_ranges(&[1]), 1);
        assert_eq!(count_line_ranges(&[1, 2, 3]), 1);
        assert_eq!(count_line_ranges(&[1, 3, 5]), 3);
        // Includes unsorted and duplicate values.
        assert_eq!(count_line_ranges(&[5, 3, 3, 4, 10]), 2);
    }

    #[test]
    fn test_should_skip_expensive_post_commit_stats_thresholds() {
        let below_threshold = StatsCostEstimate {
            files_with_additions: STATS_SKIP_MAX_FILES_WITH_ADDITIONS - 1,
            added_lines: STATS_SKIP_MAX_ADDED_LINES - 1,
            hunk_ranges: STATS_SKIP_MAX_HUNKS - 1,
            deleted_lines: STATS_SKIP_MAX_DELETED_LINES - 1,
        };
        assert!(!should_skip_expensive_post_commit_stats(&below_threshold));

        let by_hunks = StatsCostEstimate {
            files_with_additions: 1,
            added_lines: 1,
            hunk_ranges: STATS_SKIP_MAX_HUNKS,
            deleted_lines: 0,
        };
        assert!(should_skip_expensive_post_commit_stats(&by_hunks));

        let by_added_lines = StatsCostEstimate {
            files_with_additions: 1,
            added_lines: STATS_SKIP_MAX_ADDED_LINES,
            hunk_ranges: 1,
            deleted_lines: 0,
        };
        assert!(should_skip_expensive_post_commit_stats(&by_added_lines));

        let by_files = StatsCostEstimate {
            files_with_additions: STATS_SKIP_MAX_FILES_WITH_ADDITIONS,
            added_lines: 1,
            hunk_ranges: 1,
            deleted_lines: 0,
        };
        assert!(should_skip_expensive_post_commit_stats(&by_files));

        let by_deleted_lines = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: 0,
            hunk_ranges: 0,
            deleted_lines: STATS_SKIP_MAX_DELETED_LINES,
        };
        assert!(should_skip_expensive_post_commit_stats(&by_deleted_lines));
    }

    #[test]
    fn test_count_line_ranges_single_element() {
        assert_eq!(count_line_ranges(&[42]), 1);
    }

    #[test]
    fn test_count_line_ranges_all_contiguous() {
        assert_eq!(count_line_ranges(&[1, 2, 3, 4, 5]), 1);
    }

    #[test]
    fn test_count_line_ranges_all_scattered() {
        assert_eq!(count_line_ranges(&[1, 10, 20, 30]), 4);
    }

    #[test]
    fn test_count_line_ranges_duplicates() {
        assert_eq!(count_line_ranges(&[5, 5, 5]), 1);
    }

    #[test]
    fn test_count_line_ranges_unsorted() {
        // After sort+dedup: [1, 2, 5, 6, 10] -> ranges: [1,2], [5,6], [10]
        assert_eq!(count_line_ranges(&[10, 5, 6, 1, 2]), 3);
    }

    #[test]
    fn test_count_line_ranges_two_ranges() {
        assert_eq!(count_line_ranges(&[1, 2, 3, 10, 11, 12]), 2);
    }

    #[test]
    fn test_should_skip_stats_exactly_at_thresholds() {
        // Exactly at the hunks threshold alone should trigger skip.
        let at_hunks = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: 0,
            hunk_ranges: STATS_SKIP_MAX_HUNKS,
            deleted_lines: 0,
        };
        assert!(
            should_skip_expensive_post_commit_stats(&at_hunks),
            "Exactly at hunk threshold should skip"
        );

        // Exactly at added-lines threshold alone should trigger skip.
        let at_added = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: STATS_SKIP_MAX_ADDED_LINES,
            hunk_ranges: 0,
            deleted_lines: 0,
        };
        assert!(
            should_skip_expensive_post_commit_stats(&at_added),
            "Exactly at added-lines threshold should skip"
        );

        // Exactly at files-with-additions threshold alone should trigger skip.
        let at_files = StatsCostEstimate {
            files_with_additions: STATS_SKIP_MAX_FILES_WITH_ADDITIONS,
            added_lines: 0,
            hunk_ranges: 0,
            deleted_lines: 0,
        };
        assert!(
            should_skip_expensive_post_commit_stats(&at_files),
            "Exactly at files-with-additions threshold should skip"
        );

        // Exactly at deleted-lines threshold alone should trigger skip.
        let at_deleted = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: 0,
            hunk_ranges: 0,
            deleted_lines: STATS_SKIP_MAX_DELETED_LINES,
        };
        assert!(
            should_skip_expensive_post_commit_stats(&at_deleted),
            "Exactly at deleted-lines threshold should skip"
        );

        // All at zero should NOT skip.
        let all_zero = StatsCostEstimate {
            files_with_additions: 0,
            added_lines: 0,
            hunk_ranges: 0,
            deleted_lines: 0,
        };
        assert!(
            !should_skip_expensive_post_commit_stats(&all_zero),
            "All zero values should not skip"
        );
    }

    #[test]
    fn test_build_commit_prompt_summaries_extracts_original_user_prompt() {
        let temp = tempfile::NamedTempFile::new().expect("temp transcript");
        std::fs::write(
            temp.path(),
            r#"{"type":"user","message":{"content":"Build me a todo app"},"timestamp":"2026-01-01T00:00:00Z"}
{"type":"assistant","message":{"content":"Sure."},"timestamp":"2026-01-01T00:00:01Z"}
"#,
        )
        .expect("write transcript");

        let agent_id = AgentId {
            tool: "claude".to_string(),
            id: "session-123".to_string(),
            model: "sonnet".to_string(),
        };
        let session_id = generate_session_id(&agent_id.id, &agent_id.tool);

        let mut authorship_log = AuthorshipLog::new();
        authorship_log.metadata.sessions.insert(
            session_id.clone(),
            SessionRecord {
                agent_id: agent_id.clone(),
                human_author: Some("Alice <alice@example.com>".to_string()),
                messages_url: Some("cas:test".to_string()),
                custom_attributes: None,
            },
        );

        let mut checkpoint = Checkpoint::new(
            CheckpointKind::AiAgent,
            "hash".to_string(),
            "claude".to_string(),
            Vec::new(),
        );
        checkpoint.agent_id = Some(agent_id);
        checkpoint.agent_metadata = Some(HashMap::from([(
            "transcript_path".to_string(),
            temp.path().to_string_lossy().into_owned(),
        )]));

        let summaries = build_commit_prompt_summaries(&authorship_log, &[checkpoint]);
        let rows = summaries.as_array().expect("summaries array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], session_id);
        assert_eq!(rows[0]["kind"], "session");
        assert_eq!(rows[0]["original_user_prompt"], "Build me a todo app");
        assert_eq!(rows[0]["messages_url"], "cas:test");
    }
}
