use crate::daemon::analyzers::{AnalysisView, CommandAnalyzer, command_args};
use crate::daemon::domain::{
    AnalysisResult, CommandClass, Confidence, NormalizedCommand, ResetKind, SemanticEvent,
};
use crate::error::AutterError;
use crate::git::cli_parser::{explicit_rebase_branch_arg, parse_git_cli_args};
use crate::git::repo_state::{
    HeadReflogLanding, head_reflog_entry_written_by, is_valid_git_oid,
    resolve_reflog_old_oid_for_ref_new_oid_in_worktree,
    resolve_worktree_head_reflog_old_oid_for_new_head, worktree_head_reflog_landings,
};
#[cfg(test)]
use std::fs;

#[derive(Default)]
pub struct HistoryAnalyzer;

impl CommandAnalyzer for HistoryAnalyzer {
    fn analyze(
        &self,
        cmd: &NormalizedCommand,
        state: AnalysisView<'_>,
    ) -> Result<AnalysisResult, AutterError> {
        let name = cmd.primary_command.as_deref().unwrap_or_default();
        let args = command_args(cmd);

        let mut events = Vec::new();
        match name {
            "commit" => {
                let amend = args.iter().any(|arg| arg == "--amend");
                let post_head =
                    non_empty_opt(cmd.post_repo.as_ref().and_then(|repo| repo.head.clone()));
                if let Some((mut old_head, new_head)) = head_change(cmd, state.refs) {
                    if amend
                        && (!is_valid_git_oid(&old_head) || is_zero_oid(&old_head))
                        && let Some(pre_head) =
                            non_empty_opt(cmd.pre_repo.as_ref().and_then(|repo| repo.head.clone()))
                        && is_valid_git_oid(&pre_head)
                        && !is_zero_oid(&pre_head)
                        && pre_head != new_head
                    {
                        old_head = pre_head;
                    }
                    if amend {
                        events.push(SemanticEvent::CommitAmended { old_head, new_head });
                    } else {
                        events.push(SemanticEvent::CommitCreated {
                            base: sanitize_base(Some(old_head), &new_head),
                            new_head,
                        });
                    }
                } else if cmd.exit_code == 0
                    && let Some(new_head) = post_head
                {
                    if amend {
                        let old_head = commit_base_hint(cmd, state.refs, &new_head);
                        if let Some(old_head) = old_head {
                            events.push(SemanticEvent::CommitAmended { old_head, new_head });
                        } else {
                            events.push(SemanticEvent::CommitCreated {
                                base: None,
                                new_head,
                            });
                        }
                    } else {
                        let base = commit_base_hint(cmd, state.refs, &new_head);
                        events.push(SemanticEvent::CommitCreated { base, new_head });
                    }
                }
            }
            "reset" => {
                if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                    events.push(SemanticEvent::Reset {
                        kind: infer_reset_kind(&args),
                        old_head,
                        new_head,
                    });
                }
            }
            "rebase" => {
                if args.iter().any(|arg| arg == "--abort") {
                    events.push(SemanticEvent::RebaseAbort {
                        head: cmd
                            .post_repo
                            .as_ref()
                            .and_then(|repo| repo.head.clone())
                            .unwrap_or_default(),
                    });
                } else if let Some((old_head, new_head)) = rebase_change(cmd, state.refs) {
                    events.push(SemanticEvent::RebaseComplete {
                        old_head,
                        new_head,
                        interactive: args.iter().any(|arg| arg == "-i" || arg == "--interactive"),
                    });
                }
            }
            "cherry-pick" => {
                if args.iter().any(|arg| arg == "--abort") {
                    events.push(SemanticEvent::CherryPickAbort {
                        head: cmd
                            .post_repo
                            .as_ref()
                            .and_then(|repo| repo.head.clone())
                            .unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "-n" || arg == "--no-commit") {
                    // `-n` applies the picked changes without committing, so
                    // there is no new commit to map. The daemon carries the
                    // sources into the next commit instead. A stale ref state
                    // would otherwise make head_change report a bogus span
                    // here, and mapping it fails.
                } else if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                    events.push(SemanticEvent::CherryPickComplete {
                        original_head: old_head,
                        new_head,
                    });
                }
            }
            "merge" => {
                if args.iter().any(|arg| arg == "--squash") {
                    let source_ref = merge_source_ref(&args).ok_or_else(|| {
                        AutterError::Generic("merge --squash missing source ref".to_string())
                    })?;
                    events.push(SemanticEvent::MergeSquash {
                        base_branch: cmd.pre_repo.as_ref().and_then(|repo| repo.branch.clone()),
                        base_head: cmd
                            .pre_repo
                            .as_ref()
                            .and_then(|repo| repo.head.clone())
                            .unwrap_or_default(),
                        source_ref,
                        source_head: cmd.merge_squash_source_head.clone().unwrap_or_default(),
                    });
                } else if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                    events.push(SemanticEvent::RefUpdated {
                        reference: "HEAD".to_string(),
                        old: old_head,
                        new: new_head,
                    });
                }
            }
            "update-ref" => {
                if let Some((ref_name, old_oid, new_oid)) =
                    parse_update_ref_heads(cmd, &args, state.refs)
                {
                    events.push(SemanticEvent::RefUpdated {
                        reference: ref_name,
                        old: old_oid,
                        new: new_oid,
                    });
                }
            }
            _ => unreachable!("registry should not route '{}' to HistoryAnalyzer", name),
        }

        if events.is_empty() {
            events.push(SemanticEvent::OpaqueCommand);
        }

        Ok(AnalysisResult {
            class: CommandClass::HistoryRewrite,
            events,
            confidence: if cmd.exit_code == 0 {
                Confidence::High
            } else {
                Confidence::Low
            },
        })
    }
}

fn merge_source_ref(args: &[String]) -> Option<String> {
    let mut invocation = vec!["merge".to_string()];
    invocation.extend(args.iter().cloned());
    parse_git_cli_args(&invocation).pos_command(0)
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn non_empty_opt(value: Option<String>) -> Option<String> {
    value.and_then(non_empty)
}

fn is_zero_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.chars().all(|c| c == '0')
}

fn sanitize_base(base: Option<String>, new_head: &str) -> Option<String> {
    base.filter(|candidate| candidate != new_head && !is_zero_oid(candidate))
}

fn head_change(
    cmd: &NormalizedCommand,
    refs: &std::collections::HashMap<String, String>,
) -> Option<(String, String)> {
    if let Some(branch_ref) = branch_ref_hint(cmd) {
        let branch_specific_span = cmd
            .ref_changes
            .iter()
            .filter(|change| {
                change.reference == branch_ref
                    && !change.new.trim().is_empty()
                    && change.old.trim() != change.new.trim()
            })
            .collect::<Vec<_>>();
        if let Some((old_head, new_head)) = change_span(&branch_specific_span) {
            return Some((old_head, new_head));
        }
    }

    let preferred_span = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference == "HEAD"
                && !change.new.trim().is_empty()
                && change.old.trim() != change.new.trim()
        })
        .collect::<Vec<_>>();
    if let Some((old_head, new_head)) = change_span(&preferred_span) {
        return Some((old_head, new_head));
    }

    let branch_span = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference.starts_with("refs/heads/")
                && !change.new.trim().is_empty()
                && change.old.trim() != change.new.trim()
        })
        .collect::<Vec<_>>();
    if let Some((old_head, new_head)) = change_span(&branch_span) {
        return Some((old_head, new_head));
    }

    let any_span = cmd
        .ref_changes
        .iter()
        .filter(|change| !change.new.trim().is_empty() && change.old.trim() != change.new.trim())
        .collect::<Vec<_>>();
    if let Some((old_head, new_head)) = change_span(&any_span) {
        return Some((old_head, new_head));
    }

    let new_head = non_empty_opt(cmd.post_repo.as_ref().and_then(|repo| repo.head.clone()))?;

    if let Some(orig_head) = cmd
        .ref_changes
        .iter()
        .find(|change| change.reference == "ORIG_HEAD")
        .and_then(|change| non_empty(change.new.clone()))
        && orig_head != new_head
    {
        return Some((orig_head, new_head));
    }

    if let Some(pre_head) = non_empty_opt(cmd.pre_repo.as_ref().and_then(|repo| repo.head.clone()))
        && pre_head != new_head
    {
        // The snapshots taken around the command show HEAD moving. The reflog
        // names the exact commit it moved from.
        let old_head = old_head_from_worktree_head_reflog(cmd, &new_head).unwrap_or(pre_head);
        return Some((old_head, new_head));
    }

    // From here on HEAD is where the "before" snapshot found it, or there is no
    // such snapshot. Either the command did not move HEAD, or the snapshot was
    // taken late, after the command had already moved it. Only the second case
    // is a HEAD move, and it has to be shown by something this command did:
    // any older reflog entry that left HEAD on the same commit (a `checkout`
    // back to the branch, an earlier `reset`) says nothing about this command.
    let tracked_head = old_head_from_refs(cmd, refs);
    if tracked_head.as_deref() == Some(new_head.as_str()) {
        // The branch was already here after the last command that moved it.
        return None;
    }

    let landings = head_reflog_landings(cmd, &new_head).unwrap_or_default();
    if landings.is_empty() {
        // The reflog has nothing on this commit. The ref state is all there is
        // to go on.
        return tracked_head.map(|old_head| (old_head, new_head));
    }
    let command = cmd.primary_command.as_deref().unwrap_or_default();
    for landing in landings {
        let own_entry = head_reflog_entry_written_by(command, &landing.message);
        if landing.old != new_head {
            // The newest entry that moved HEAD here. It is this command's move
            // only if this command can have written it.
            return own_entry.then_some((landing.old, new_head));
        }
        if own_entry && command == "reset" {
            // `git reset` writes an entry even when it stays on the commit.
            return None;
        }
        // An entry that left HEAD where it was: a branch created or switched
        // on the same commit, or the closing entry of a rebase. Look past it.
    }
    None
}

/// The HEAD reflog entries that left HEAD at `new_head`, newest first, entries
/// that did not move HEAD included. `None` when the reflog cannot be read.
fn head_reflog_landings(cmd: &NormalizedCommand, new_head: &str) -> Option<Vec<HeadReflogLanding>> {
    let worktree = cmd.worktree.as_deref()?;
    worktree_head_reflog_landings(worktree, new_head).ok()
}

fn branch_ref_hint(cmd: &NormalizedCommand) -> Option<String> {
    let branch = cmd
        .pre_repo
        .as_ref()
        .and_then(|repo| repo.branch.clone())
        .or_else(|| cmd.post_repo.as_ref().and_then(|repo| repo.branch.clone()))?;
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }
    if branch.starts_with("refs/") {
        Some(branch.to_string())
    } else {
        Some(format!("refs/heads/{}", branch))
    }
}

fn old_head_from_branch_ref_changes(cmd: &NormalizedCommand) -> Option<String> {
    let branch_ref = branch_ref_hint(cmd)?;
    cmd.ref_changes
        .iter()
        .find(|change| change.reference == branch_ref)
        .and_then(|change| non_empty(change.old.clone()))
        .filter(|old| !is_zero_oid(old))
}

fn old_head_from_refs(
    cmd: &NormalizedCommand,
    refs: &std::collections::HashMap<String, String>,
) -> Option<String> {
    non_empty_opt(
        cmd.pre_repo
            .as_ref()
            .and_then(|repo| repo.branch.as_deref())
            .and_then(|branch| refs.get(&format!("refs/heads/{}", branch)).cloned())
            .or_else(|| {
                cmd.post_repo
                    .as_ref()
                    .and_then(|repo| repo.branch.as_deref())
                    .and_then(|branch| refs.get(&format!("refs/heads/{}", branch)).cloned())
            }),
    )
}

fn commit_base_hint(
    cmd: &NormalizedCommand,
    refs: &std::collections::HashMap<String, String>,
    new_head: &str,
) -> Option<String> {
    sanitize_base(
        old_head_from_branch_ref_changes(cmd)
            .or_else(|| old_head_from_worktree_head_reflog(cmd, new_head))
            .or_else(|| non_empty_opt(cmd.pre_repo.as_ref().and_then(|repo| repo.head.clone())))
            .or_else(|| old_head_from_refs(cmd, refs)),
        new_head,
    )
}

fn old_head_from_worktree_head_reflog(cmd: &NormalizedCommand, new_head: &str) -> Option<String> {
    let worktree = cmd.worktree.as_deref()?;
    resolve_worktree_head_reflog_old_oid_for_new_head(worktree, new_head)
        .ok()
        .flatten()
}

fn rebase_change(
    cmd: &NormalizedCommand,
    refs: &std::collections::HashMap<String, String>,
) -> Option<(String, String)> {
    if let Some((old_head, new_head)) = explicit_rebase_branch_change(cmd) {
        return Some((old_head, new_head));
    }

    if let Some((old_head, new_head)) = inferred_rebase_branch_change(cmd) {
        return Some((old_head, new_head));
    }

    let from_changes = head_change(cmd, refs);
    let new_head = from_changes
        .as_ref()
        .map(|(_, new_head)| new_head.clone())
        .or_else(|| non_empty_opt(cmd.post_repo.as_ref().and_then(|repo| repo.head.clone())))?;

    if let Some((old_head, new_head_from_changes)) = from_changes
        && old_head != new_head_from_changes
    {
        return Some((old_head, new_head_from_changes));
    }

    non_empty_opt(cmd.pre_repo.as_ref().and_then(|repo| repo.head.clone()))
        .filter(|old_head| old_head != &new_head)
        .map(|old_head| (old_head, new_head))
}

fn inferred_rebase_branch_change(cmd: &NormalizedCommand) -> Option<(String, String)> {
    let mut candidates = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference.starts_with("refs/heads/")
                && !change.old.trim().is_empty()
                && !change.new.trim().is_empty()
                && change.old.trim() != change.new.trim()
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }

    let post_head = non_empty_opt(cmd.post_repo.as_ref().and_then(|repo| repo.head.clone()));
    if let Some(post_head) = post_head
        && let Some(change) = candidates
            .iter()
            .find(|change| change.new.trim() == post_head)
    {
        return Some((change.old.trim().to_string(), change.new.trim().to_string()));
    }

    if candidates.len() == 1 {
        let change = candidates.pop()?;
        return Some((change.old.trim().to_string(), change.new.trim().to_string()));
    }

    None
}

fn explicit_rebase_branch_change(cmd: &NormalizedCommand) -> Option<(String, String)> {
    let args = command_args(cmd);
    let branch = explicit_rebase_branch_arg(&args)?;
    let branch_ref = if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{}", branch)
    };
    cmd.ref_changes
        .iter()
        .find(|change| {
            change.reference == branch_ref
                && !change.old.trim().is_empty()
                && !change.new.trim().is_empty()
                && change.old.trim() != change.new.trim()
        })
        .map(|change| (change.old.trim().to_string(), change.new.trim().to_string()))
}

fn change_span(changes: &[&crate::daemon::domain::RefChange]) -> Option<(String, String)> {
    let first = changes.first()?;
    let last = changes.last()?;
    let old_head = first.old.trim();
    let new_head = last.new.trim();
    if old_head.is_empty() || new_head.is_empty() || old_head == new_head {
        return None;
    }
    Some((old_head.to_string(), new_head.to_string()))
}

/// Parse `update-ref <ref> <new_oid> [<old_oid>]` arguments, returning
/// (ref_name, old_oid, new_oid) for branch refs only.  Falls back to
/// `state.refs` for the old OID when the command provides only two positionals,
/// then to the reflog of the target ref.
fn parse_update_ref_heads(
    cmd: &NormalizedCommand,
    args: &[String],
    refs: &std::collections::HashMap<String, String>,
) -> Option<(String, String, String)> {
    let mut positionals = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "update-ref" => {
                i += 1;
                continue;
            }
            "--stdin" | "--batch-updates" | "-d" | "--delete" => return None,
            "-m" | "--message" => {
                i += 2;
                continue;
            }
            "--create-reflog" | "--no-deref" => {
                i += 1;
                continue;
            }
            _ if arg.starts_with("--message=") => {
                i += 1;
                continue;
            }
            _ if arg.starts_with('-') => return None,
            _ => {
                positionals.push(arg.clone());
                i += 1;
            }
        }
    }

    let (ref_name, new_oid, old_oid_arg) = match positionals.as_slice() {
        [ref_name, new_oid] => (ref_name.clone(), new_oid.clone(), None),
        [ref_name, new_oid, old_oid] => (ref_name.clone(), new_oid.clone(), Some(old_oid.clone())),
        _ => return None,
    };

    if !ref_name.starts_with("refs/heads/") {
        return None;
    }

    let old_oid = old_oid_arg
        .filter(|oid| !oid.is_empty() && !is_zero_oid(oid))
        .or_else(|| refs.get(&ref_name).cloned())
        .or_else(|| {
            // The command has already executed; read the old value from the
            // reflog entry that recorded this update-ref.
            let worktree = cmd.worktree.as_deref()?;
            resolve_reflog_old_oid_for_ref_new_oid_in_worktree(worktree, &ref_name, &new_oid)
                .or_else(|| {
                    resolve_worktree_head_reflog_old_oid_for_new_head(worktree, &new_oid)
                        .ok()
                        .flatten()
                })
        })
        .filter(|oid| !oid.is_empty() && !is_zero_oid(oid))?;

    if old_oid == new_oid {
        return None;
    }

    Some((ref_name, old_oid, new_oid))
}

fn infer_reset_kind(args: &[String]) -> ResetKind {
    if args.iter().any(|arg| arg == "--soft") {
        return ResetKind::Soft;
    }
    if args.iter().any(|arg| arg == "--mixed") {
        return ResetKind::Mixed;
    }
    if args.iter().any(|arg| arg == "--hard") {
        return ResetKind::Hard;
    }
    if args.iter().any(|arg| arg == "--merge") {
        return ResetKind::Merge;
    }
    if args.iter().any(|arg| arg == "--keep") {
        return ResetKind::Keep;
    }
    ResetKind::Mixed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::domain::{CommandScope, RefChange};
    use tempfile::tempdir;

    fn command(primary: &str, argv: &[&str]) -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Global,
            family_key: None,
            worktree: None,
            root_sid: "r".to_string(),
            raw_argv: argv.iter().map(|s| s.to_string()).collect(),
            primary_command: Some(primary.to_string()),
            invoked_command: Some(primary.to_string()),
            invoked_args: argv.iter().skip(2).map(|s| s.to_string()).collect(),
            observed_child_commands: Vec::new(),
            exit_code: 0,
            started_at_ns: 1,
            finished_at_ns: 2,
            pre_repo: None,
            post_repo: None,
            inflight_rebase_original_head: None,
            merge_squash_source_head: None,
            carryover_snapshot_id: None,
            stash_target_oid: None,
            ref_changes: vec![RefChange {
                reference: "HEAD".to_string(),
                old: "a".to_string(),
                new: "b".to_string(),
            }],
            confidence: Confidence::Low,
            wrapper_invocation_id: None,
            agent_env: None,
        }
    }

    #[test]
    fn commit_without_amend_emits_commit_created() {
        let analyzer = HistoryAnalyzer;
        let result = analyzer
            .analyze(
                &command("commit", &["git", "commit", "-m", "x"]),
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, SemanticEvent::CommitCreated { .. }))
        );
    }

    #[test]
    fn amend_prefers_pre_head_over_zero_old_reflog_change() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "--amend", "-m", "x"]);
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.ref_changes = vec![
            RefChange {
                reference: "refs/heads/main".to_string(),
                old: "0000000000000000000000000000000000000000".to_string(),
                new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            },
            RefChange {
                reference: "refs/heads/main".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
        ];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::CommitAmended { old_head, new_head }
                if old_head == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    && new_head == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )));
    }

    #[test]
    fn reset_emits_reset_kind() {
        let analyzer = HistoryAnalyzer;
        let result = analyzer
            .analyze(
                &command("reset", &["git", "reset", "--hard", "HEAD~1"]),
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::Reset {
                kind: ResetKind::Hard,
                ..
            }
        )));
    }

    #[test]
    fn commit_uses_pre_post_head_when_reflog_delta_is_empty() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes.clear();
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("old-head".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("new-head".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CommitCreated {
                    base,
                    new_head,
                } if base.as_deref() == Some("old-head") && new_head == "new-head"
            )),
            "expected commit-created event from pre/post head fallback, got {:?}",
            result.events
        );
    }

    #[test]
    fn commit_fallback_prefers_pre_head_over_family_refs() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes.clear();
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("old-head".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("new-head".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        let refs = std::collections::HashMap::from([(
            "refs/heads/main".to_string(),
            "wrong-family-head".to_string(),
        )]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();

        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CommitCreated {
                    base,
                    new_head
                } if base.as_deref() == Some("old-head") && new_head == "new-head"
            )),
            "expected commit-created event to prefer pre-head over family refs, got {:?}",
            result.events
        );
    }

    #[test]
    fn commit_emits_created_when_only_post_head_is_available() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes.clear();
        cmd.pre_repo = None;
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("new-head".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        let refs = std::collections::HashMap::from([(
            "refs/heads/main".to_string(),
            "old-head".to_string(),
        )]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();
        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CommitCreated {
                    base,
                    new_head
                } if base.as_deref() == Some("old-head") && new_head == "new-head"
            )),
            "expected commit-created event from post-head fallback, got {:?}",
            result.events
        );
    }

    #[test]
    fn commit_falls_back_to_head_reflog_when_pre_and_post_are_contaminated() {
        let analyzer = HistoryAnalyzer;
        let dir = tempdir().expect("tempdir");
        let worktree = dir.path();
        let git_dir = worktree.join(".git");
        fs::create_dir_all(git_dir.join("logs")).expect("create logs");
        fs::write(
            git_dir.join("logs").join("HEAD"),
            concat!(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb ",
                "Test User <test@example.com> 0 +0000\tcommit: first\n",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb ",
                "cccccccccccccccccccccccccccccccccccccccc ",
                "Test User <test@example.com> 0 +0000\tcommit: squash\n"
            ),
        )
        .expect("write HEAD reflog");

        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes.clear();
        cmd.worktree = Some(worktree.to_path_buf());
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("cccccccccccccccccccccccccccccccccccccccc".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("cccccccccccccccccccccccccccccccccccccccc".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CommitCreated { base, new_head }
                    if base.as_deref() == Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                        && new_head == "cccccccccccccccccccccccccccccccccccccccc"
            )),
            "expected commit-created event from HEAD reflog fallback, got {:?}",
            result.events
        );
    }

    #[test]
    fn commit_prefers_post_head_when_family_ref_changes_are_contaminated() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "-C", "/repo-b", "commit", "-m", "x"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "refs/heads/branch-a".to_string(),
                old: "0000000000000000000000000000000000000000".to_string(),
                new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            },
            RefChange {
                reference: "refs/heads/branch-b".to_string(),
                old: "0000000000000000000000000000000000000000".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "0000000000000000000000000000000000000000".to_string(),
                new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            },
        ];
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
            branch: Some("branch-b".to_string()),
            detached: false,
        });
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
            branch: Some("branch-b".to_string()),
            detached: false,
        });

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CommitCreated {
                    new_head,
                    ..
                } if new_head == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )),
            "expected commit-created event to use branch-b post head, got {:?}",
            result.events
        );
    }

    #[test]
    fn head_change_prefers_branch_hint_over_head_change() {
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: "old-head".to_string(),
                new: "wrong-head".to_string(),
            },
            RefChange {
                reference: "refs/heads/main".to_string(),
                old: "old-main".to_string(),
                new: "new-main".to_string(),
            },
        ];
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("old-main".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("new-main".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });

        let change = head_change(&cmd, &Default::default());
        assert_eq!(
            change,
            Some(("old-main".to_string(), "new-main".to_string())),
            "expected branch-specific change to win over generic HEAD change"
        );
    }

    #[test]
    fn rebase_continue_prefers_branch_ref_change_over_head_span() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("rebase", &["git", "rebase", "--continue"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "refs/heads/feature".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "cccccccccccccccccccccccccccccccccccccccc".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
        ];
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
            branch: Some("feature".to_string()),
            detached: false,
        });

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::RebaseComplete {
                    old_head,
                    new_head,
                    ..
                } if old_head == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    && new_head == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )),
            "expected rebase-complete to use branch ref span, got {:?}",
            result.events
        );
    }

    #[test]
    fn cherry_pick_uses_full_head_ref_change_span() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command(
            "cherry-pick",
            &["git", "cherry-pick", "source-1", "source-2", "source-3"],
        );
        cmd.ref_changes = vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: "a".to_string(),
                new: "b".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "b".to_string(),
                new: "c".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "c".to_string(),
                new: "d".to_string(),
            },
        ];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CherryPickComplete {
                    original_head,
                    new_head
                } if original_head == "a" && new_head == "d"
            )),
            "expected cherry-pick span event, got {:?}",
            result.events
        );
    }

    #[test]
    fn cherry_pick_no_commit_emits_no_complete_event() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("cherry-pick", &["git", "cherry-pick", "-n", "abc123"]);
        cmd.ref_changes.clear();
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("main-head".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.post_repo = cmd.pre_repo.clone();
        // The daemon's ref state still points at the branch checked out before.
        let refs = std::collections::HashMap::from([
            ("HEAD".to_string(), "feature-head".to_string()),
            ("refs/heads/main".to_string(), "older-main".to_string()),
        ]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, SemanticEvent::CherryPickComplete { .. })),
            "cherry-pick -n creates no commit, got {:?}",
            result.events
        );
    }

    const OID_MAIN: &str = "1111111111111111111111111111111111111111";
    const OID_FEATURE: &str = "2222222222222222222222222222222222222222";
    const OID_UNDONE: &str = "3333333333333333333333333333333333333333";

    /// A command on `main` whose before and after snapshots both show HEAD at
    /// `OID_MAIN`, in a worktree with the given HEAD reflog.
    fn command_on_unmoved_main(
        primary: &str,
        argv: &[&str],
        worktree: &std::path::Path,
        reflog: &[(&str, &str, &str)],
    ) -> NormalizedCommand {
        let logs = worktree.join(".git").join("logs");
        fs::create_dir_all(&logs).expect("create logs");
        let contents: String = reflog
            .iter()
            .map(|(old, new, message)| {
                format!("{old} {new} Test User <test@example.com> 0 +0000\t{message}\n")
            })
            .collect();
        fs::write(logs.join("HEAD"), contents).expect("write HEAD reflog");

        let mut cmd = command(primary, argv);
        cmd.ref_changes.clear();
        cmd.worktree = Some(worktree.to_path_buf());
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some(OID_MAIN.to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.post_repo = cmd.pre_repo.clone();
        cmd
    }

    fn reset_events(
        cmd: &NormalizedCommand,
        refs: &std::collections::HashMap<String, String>,
    ) -> Vec<SemanticEvent> {
        HistoryAnalyzer
            .analyze(cmd, AnalysisView { refs })
            .unwrap()
            .events
            .into_iter()
            .filter(|event| matches!(event, SemanticEvent::Reset { .. }))
            .collect()
    }

    #[test]
    fn reset_that_stays_on_head_is_not_a_move_from_the_branch_visited_before() {
        let dir = tempdir().expect("tempdir");
        // `git checkout main` after a commit on `feature`, then `git reset`.
        let cmd = command_on_unmoved_main(
            "reset",
            &["git", "reset"],
            dir.path(),
            &[
                (OID_MAIN, OID_FEATURE, "commit: on feature"),
                (
                    OID_FEATURE,
                    OID_MAIN,
                    "checkout: moving from feature to main",
                ),
                (OID_MAIN, OID_MAIN, "reset: moving to HEAD"),
            ],
        );

        let events = reset_events(&cmd, &Default::default());
        assert!(events.is_empty(), "HEAD did not move, got {:?}", events);
    }

    #[test]
    fn reset_of_paths_does_not_inherit_the_checkout_that_brought_head_here() {
        let dir = tempdir().expect("tempdir");
        // `git reset -- file` writes no reflog entry at all, so the newest entry
        // that left HEAD on this commit is the checkout.
        let cmd = command_on_unmoved_main(
            "reset",
            &["git", "reset", "--", "file.txt"],
            dir.path(),
            &[
                (OID_MAIN, OID_FEATURE, "commit: on feature"),
                (
                    OID_FEATURE,
                    OID_MAIN,
                    "checkout: moving from feature to main",
                ),
            ],
        );

        let events = reset_events(&cmd, &Default::default());
        assert!(events.is_empty(), "HEAD did not move, got {:?}", events);
    }

    #[test]
    fn reset_of_paths_does_not_repeat_an_earlier_reset_to_the_same_commit() {
        let dir = tempdir().expect("tempdir");
        // `git reset --soft HEAD~1`, then `git reset -- file`. The second reset
        // writes no reflog entry; the first one's was already applied, which
        // the ref state shows.
        let cmd = command_on_unmoved_main(
            "reset",
            &["git", "reset", "--", "file.txt"],
            dir.path(),
            &[(OID_UNDONE, OID_MAIN, "reset: moving to HEAD~1")],
        );
        let refs = std::collections::HashMap::from([(
            "refs/heads/main".to_string(),
            OID_MAIN.to_string(),
        )]);

        let events = reset_events(&cmd, &refs);
        assert!(events.is_empty(), "HEAD did not move, got {:?}", events);
    }

    #[test]
    fn reset_seen_late_is_still_read_from_its_own_reflog_entry() {
        let dir = tempdir().expect("tempdir");
        // The "before" snapshot was taken after `git reset --soft HEAD~1` had
        // already moved HEAD. The ref state still has the branch on the commit
        // that was undone, and the reflog entry is this reset's own.
        let cmd = command_on_unmoved_main(
            "reset",
            &["git", "reset", "--soft", "HEAD~1"],
            dir.path(),
            &[(OID_UNDONE, OID_MAIN, "reset: moving to HEAD~1")],
        );
        let refs = std::collections::HashMap::from([(
            "refs/heads/main".to_string(),
            OID_UNDONE.to_string(),
        )]);

        let events = reset_events(&cmd, &refs);
        assert!(
            matches!(
                events.as_slice(),
                [SemanticEvent::Reset {
                    kind: ResetKind::Soft,
                    old_head,
                    new_head,
                }] if old_head == OID_UNDONE && new_head == OID_MAIN
            ),
            "expected the reset from the undone commit, got {:?}",
            events
        );
    }

    #[test]
    fn reset_looks_past_a_branch_created_on_the_same_commit() {
        let dir = tempdir().expect("tempdir");
        // `git reset --soft HEAD~1` seen late, then `git checkout -b topic`
        // before the reset was analyzed. The checkout left HEAD on the commit.
        let cmd = command_on_unmoved_main(
            "reset",
            &["git", "reset", "--soft", "HEAD~1"],
            dir.path(),
            &[
                (OID_UNDONE, OID_MAIN, "reset: moving to HEAD~1"),
                (OID_MAIN, OID_MAIN, "checkout: moving from main to topic"),
            ],
        );

        let events = reset_events(&cmd, &Default::default());
        assert!(
            matches!(
                events.as_slice(),
                [SemanticEvent::Reset { old_head, new_head, .. }]
                    if old_head == OID_UNDONE && new_head == OID_MAIN
            ),
            "expected the reset from the undone commit, got {:?}",
            events
        );
    }

    #[test]
    fn rebase_seen_late_is_read_past_its_closing_reflog_entry() {
        let dir = tempdir().expect("tempdir");
        // A rebase ends with an entry that leaves HEAD where the last pick put
        // it. The move is the pick before it.
        let cmd = command_on_unmoved_main(
            "rebase",
            &["git", "rebase", "onto"],
            dir.path(),
            &[
                (OID_FEATURE, OID_UNDONE, "rebase (start): checkout onto"),
                (OID_UNDONE, OID_MAIN, "rebase (pick): message"),
                (
                    OID_MAIN,
                    OID_MAIN,
                    "rebase (finish): returning to refs/heads/main",
                ),
            ],
        );
        let refs = std::collections::HashMap::from([(
            "refs/heads/main".to_string(),
            OID_FEATURE.to_string(),
        )]);

        let result = HistoryAnalyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();
        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::RebaseComplete { new_head, .. } if new_head == OID_MAIN
            )),
            "expected the rebase to be reported, got {:?}",
            result.events
        );
    }

    #[test]
    fn merge_abort_is_not_a_head_move() {
        let dir = tempdir().expect("tempdir");
        // `git merge --abort` resets to HEAD, which writes an entry that does
        // not move HEAD.
        let cmd = command_on_unmoved_main(
            "merge",
            &["git", "merge", "--abort"],
            dir.path(),
            &[
                (
                    OID_FEATURE,
                    OID_MAIN,
                    "checkout: moving from feature to main",
                ),
                (OID_MAIN, OID_MAIN, "reset: moving to HEAD"),
            ],
        );

        let result = HistoryAnalyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, SemanticEvent::RefUpdated { .. })),
            "HEAD did not move, got {:?}",
            result.events
        );
    }

    #[test]
    fn reflog_entries_are_matched_to_the_command_that_writes_them() {
        assert!(head_reflog_entry_written_by("commit", "commit: message"));
        assert!(head_reflog_entry_written_by(
            "commit",
            "commit (amend): message"
        ));
        assert!(head_reflog_entry_written_by(
            "reset",
            "reset: moving to HEAD~1"
        ));
        assert!(head_reflog_entry_written_by(
            "cherry-pick",
            "cherry-pick: message"
        ));
        assert!(head_reflog_entry_written_by(
            "cherry-pick",
            "commit (cherry-pick): message"
        ));
        assert!(head_reflog_entry_written_by(
            "merge",
            "merge feature: Fast-forward"
        ));
        assert!(head_reflog_entry_written_by(
            "rebase",
            "rebase (finish): done"
        ));
        // A script's own action is accepted for any command.
        assert!(head_reflog_entry_written_by(
            "reset",
            "restack: moving branch"
        ));

        assert!(!head_reflog_entry_written_by(
            "reset",
            "checkout: moving from feature to main"
        ));
        assert!(!head_reflog_entry_written_by("reset", "commit: message"));
        assert!(!head_reflog_entry_written_by(
            "commit",
            "reset: moving to HEAD~1"
        ));
        assert!(!head_reflog_entry_written_by(
            "cherry-pick",
            "commit: message"
        ));
        assert!(!head_reflog_entry_written_by(
            "merge",
            "checkout: moving from feature to main"
        ));
    }

    #[test]
    fn cherry_pick_prefers_ref_state_when_pre_head_matches_post_head() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("cherry-pick", &["git", "cherry-pick", "--continue"]);
        cmd.ref_changes.clear();
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("new-head".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.post_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("new-head".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        let refs = std::collections::HashMap::from([
            ("HEAD".to_string(), "old-head".to_string()),
            ("refs/heads/main".to_string(), "old-head".to_string()),
        ]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();
        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CherryPickComplete {
                    original_head,
                    new_head
                } if original_head == "old-head" && new_head == "new-head"
            )),
            "expected cherry-pick complete event from ref-state fallback, got {:?}",
            result.events
        );
    }

    #[test]
    fn merge_squash_emits_resolved_source_ref_and_head() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("merge", &["git", "merge", "--squash", "feature"]);
        cmd.pre_repo = Some(crate::daemon::domain::RepoContext {
            head: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
            branch: Some("main".to_string()),
            detached: false,
        });
        cmd.merge_squash_source_head = Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string());

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::MergeSquash {
                    base_branch,
                    base_head,
                    source_ref,
                    source_head,
                } if base_branch.as_deref() == Some("main")
                    && base_head == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    && source_ref == "feature"
                    && source_head == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )),
            "expected merge-squash event with resolved source head, got {:?}",
            result.events
        );
    }
}
