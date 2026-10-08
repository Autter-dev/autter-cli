//! Local history rewrites: attribution must follow the rewritten commits.
//!
//! These complement `rebase.rs`, `amend.rs` and `squash_merge.rs`, which mostly
//! assert blame at HEAD. Here we also assert the note on every rewritten SHA
//! and compare it against the note on the original SHA, using the explicit
//! pre/post checkpoint flow (`human` -> edit -> `mock_ai`) rather than
//! `TestFile::set_contents`.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use autter::authorship::authorship_log::LineRange;
use autter::authorship::authorship_log_serialization::AuthorshipLog;
use std::collections::BTreeSet;
use std::fs;

/// Mimic an AI agent preset: untracked pre-edit checkpoint, edit, AI post-edit checkpoint.
fn ai_edit(repo: &TestRepo, rel: &str, content: &str) {
    repo.autter(&["checkpoint", "human", rel]).unwrap();
    fs::write(repo.path().join(rel), content).unwrap();
    repo.autter(&["checkpoint", "mock_ai", rel]).unwrap();
}

/// Mimic an editor extension reporting a real human edit.
fn human_edit(repo: &TestRepo, rel: &str, content: &str) {
    fs::write(repo.path().join(rel), content).unwrap();
    repo.autter(&["checkpoint", "mock_known_human", rel])
        .unwrap();
}

fn commit_all(repo: &TestRepo, message: &str) -> String {
    repo.git(&["add", "-A"]).unwrap();
    repo.commit(message).unwrap().commit_sha
}

fn rev_parse(repo: &TestRepo, rev: &str) -> String {
    repo.git(&["rev-parse", rev]).unwrap().trim().to_string()
}

/// AI-attributed line numbers recorded for `file` in a raw authorship note.
pub(crate) fn ai_lines_in_note(note: &str, file: &str) -> Vec<u32> {
    let log = AuthorshipLog::deserialize_from_string(note).expect("authorship note should parse");
    let mut lines = BTreeSet::new();
    for attestation in log.attestations.iter().filter(|a| a.file_path == file) {
        for entry in attestation
            .entries
            .iter()
            .filter(|e| !e.hash.starts_with("h_"))
        {
            for range in &entry.line_ranges {
                match range {
                    LineRange::Single(n) => {
                        lines.insert(*n);
                    }
                    LineRange::Range(start, end) => lines.extend(*start..=*end),
                }
            }
        }
    }
    lines.into_iter().collect()
}

fn ai_lines(repo: &TestRepo, sha: &str, file: &str) -> Vec<u32> {
    let note = repo
        .read_authorship_note(sha)
        .unwrap_or_else(|| panic!("commit {sha} should carry an authorship note"));
    ai_lines_in_note(&note, file)
}

#[test]
fn test_history_rewrite_local_rebase_moves_notes_to_each_rewritten_commit() {
    let repo = TestRepo::new();
    human_edit(&repo, "app.txt", "base 1\nbase 2\n");
    commit_all(&repo, "base");
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    ai_edit(&repo, "feature.txt", "ai 1\nai 2\n");
    let old_c1 = commit_all(&repo, "AI commit 1");
    let mut feature = repo.filename("feature.txt");
    feature.assert_committed_lines(lines!["ai 1".ai(), "ai 2".ai()]);

    human_edit(&repo, "app.txt", "base 1\nbase 2\nhuman feature\n");
    ai_edit(&repo, "feature.txt", "ai 1\nai 2\nai 3\n");
    let old_c2 = commit_all(&repo, "AI commit 2");
    feature.assert_committed_lines(lines!["ai 1".ai(), "ai 2".ai(), "ai 3".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    human_edit(&repo, "other.txt", "main advance\n");
    commit_all(&repo, "main advances");

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main]).unwrap();

    let new_c2 = rev_parse(&repo, "HEAD");
    let new_c1 = rev_parse(&repo, "HEAD~1");
    assert_ne!(new_c1, old_c1, "rebase should rewrite commit 1");
    assert_ne!(new_c2, old_c2, "rebase should rewrite commit 2");

    for (old, new) in [(&old_c1, &new_c1), (&old_c2, &new_c2)] {
        let before = ai_lines(&repo, old, "feature.txt");
        assert!(!before.is_empty(), "original {old} should have AI lines");
        assert_eq!(
            ai_lines(&repo, new, "feature.txt"),
            before,
            "rewritten {new} should carry the same AI lines as {old}"
        );
        assert!(ai_lines(&repo, new, "app.txt").is_empty());
    }

    feature.assert_committed_lines(lines!["ai 1".ai(), "ai 2".ai(), "ai 3".ai()]);
    let mut app = repo.filename("app.txt");
    app.assert_committed_lines(lines![
        "base 1".human(),
        "base 2".human(),
        "human feature".human()
    ]);
}

#[test]
fn test_history_rewrite_local_rebase_conflict_resolved_by_user() {
    let repo = TestRepo::new();
    human_edit(&repo, "app.txt", "line 1\nline 2\nline 3\n");
    commit_all(&repo, "base");
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    ai_edit(
        &repo,
        "app.txt",
        "line 1\nAI line 2\nline 3\nAI tail 1\nAI tail 2\n",
    );
    commit_all(&repo, "AI edits");
    let mut app = repo.filename("app.txt");
    app.assert_committed_lines(lines![
        "line 1".human(),
        "AI line 2".ai(),
        "line 3".human(),
        "AI tail 1".ai(),
        "AI tail 2".ai()
    ]);

    repo.git(&["checkout", &main]).unwrap();
    human_edit(&repo, "app.txt", "line 1\nmain line 2\nline 3\n");
    commit_all(&repo, "main edits line 2");

    repo.git(&["checkout", "feature"]).unwrap();
    assert!(
        repo.git(&["rebase", &main]).is_err(),
        "rebase should stop on the line-2 conflict"
    );

    // The user resolves by hand (no checkpoint), keeping main's line 2.
    fs::write(
        repo.path().join("app.txt"),
        "line 1\nmain line 2\nline 3\nAI tail 1\nAI tail 2\n",
    )
    .unwrap();
    repo.git(&["add", "app.txt"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let new_head = rev_parse(&repo, "HEAD");
    assert_eq!(
        ai_lines(&repo, &new_head, "app.txt"),
        vec![4, 5],
        "only the non-conflicting AI lines should stay AI after a human resolution"
    );
    app.assert_committed_lines(lines![
        "line 1".human(),
        "main line 2".human(),
        "line 3".human(),
        "AI tail 1".ai(),
        "AI tail 2".ai()
    ]);
}

#[test]
fn test_history_rewrite_amend_moves_note_to_amended_commit() {
    let repo = TestRepo::new();
    human_edit(&repo, "README.md", "readme\n");
    commit_all(&repo, "base");

    ai_edit(&repo, "notes.txt", "ai a\nai b\n");
    let original = commit_all(&repo, "AI commit");
    let mut notes = repo.filename("notes.txt");
    notes.assert_committed_lines(lines!["ai a".ai(), "ai b".ai()]);
    assert_eq!(ai_lines(&repo, &original, "notes.txt"), vec![1, 2]);

    human_edit(&repo, "notes.txt", "ai a\nai b\nhuman c\n");
    ai_edit(&repo, "notes.txt", "ai a\nai b\nhuman c\nai d\n");
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    let amended = rev_parse(&repo, "HEAD");
    assert_ne!(amended, original);
    assert_eq!(ai_lines(&repo, &amended, "notes.txt"), vec![1, 2, 4]);
    notes.assert_committed_lines(lines![
        "ai a".ai(),
        "ai b".ai(),
        "human c".human(),
        "ai d".ai()
    ]);

    // Message-only amend: content unchanged, note must still follow the new SHA.
    repo.git(&["commit", "--amend", "-m", "reworded"]).unwrap();
    let reworded = rev_parse(&repo, "HEAD");
    assert_ne!(reworded, amended);
    assert_eq!(ai_lines(&repo, &reworded, "notes.txt"), vec![1, 2, 4]);
    notes.assert_committed_lines(lines![
        "ai a".ai(),
        "ai b".ai(),
        "human c".human(),
        "ai d".ai()
    ]);
}

#[test]
fn test_history_rewrite_merge_squash_combines_attribution_from_all_commits() {
    let repo = TestRepo::new();
    human_edit(&repo, "README.md", "readme\n");
    commit_all(&repo, "base");
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    ai_edit(&repo, "a.txt", "a1\na2\n");
    commit_all(&repo, "AI a");
    human_edit(&repo, "b.txt", "b1\n");
    commit_all(&repo, "human b");
    ai_edit(&repo, "a.txt", "a1\na2\na3\n");
    ai_edit(&repo, "c.txt", "c1\n");
    commit_all(&repo, "AI a + c");

    repo.git(&["checkout", &main]).unwrap();
    human_edit(&repo, "d.txt", "d\n");
    commit_all(&repo, "main advances");

    repo.git(&["merge", "--squash", "feature"]).unwrap();
    let squash = repo.commit("Squashed feature").unwrap().commit_sha;

    assert_eq!(ai_lines(&repo, &squash, "a.txt"), vec![1, 2, 3]);
    assert_eq!(ai_lines(&repo, &squash, "c.txt"), vec![1]);
    assert!(ai_lines(&repo, &squash, "b.txt").is_empty());
    assert!(ai_lines(&repo, &squash, "d.txt").is_empty());

    let mut a = repo.filename("a.txt");
    a.assert_committed_lines(lines!["a1".ai(), "a2".ai(), "a3".ai()]);
    let mut b = repo.filename("b.txt");
    b.assert_committed_lines(lines!["b1".human()]);
    let mut c = repo.filename("c.txt");
    c.assert_committed_lines(lines!["c1".ai()]);
}
