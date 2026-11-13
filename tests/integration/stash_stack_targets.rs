//! Stash operations on a stack of more than one stash restore the attribution
//! of the stash they act on.
//!
//! The daemon learns about a `git stash pop` from git's trace events, which it
//! reads while git is already running. Resolving `stash@{0}` from the repo at
//! that point can see the stack after git moved it, i.e. the entry that was
//! `stash@{1}`, and restore that stash's attribution instead. Each test here
//! puts two stashes with different AI lines (in different files) on the stack
//! and checks that every pop, apply, drop and `stash branch` restores the one
//! it names.
use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

const FILES: [&str; 2] = ["first.txt", "second.txt"];

/// Commit `first.txt` and `second.txt` with one untracked line each, then
/// stash one AI edit per file: `first.txt` goes into the older stash
/// (`stash@{1}`), `second.txt` into the newer one (`stash@{0}`). Each stash is
/// followed by a commit of another file, so every stash is popped onto a
/// commit other than the one it was made on. Returns the stash SHAs, older
/// first.
fn two_stashes(repo: &TestRepo) -> [String; 2] {
    for file in FILES {
        fs::write(repo.path().join(file), "base\n").unwrap();
    }
    repo.stage_all_and_commit("base").unwrap();
    assert_base(repo, &FILES);

    let mut shas = Vec::new();
    for (index, file) in FILES.iter().enumerate() {
        let path = repo.path().join(file);
        let n = index + 1;
        fs::write(&path, format!("base\nuntracked {n}\n")).unwrap();
        // The agent preset's pre-edit checkpoint.
        repo.autter(&["checkpoint", "human", file]).unwrap();
        fs::write(&path, format!("base\nuntracked {n}\nai {n}\n")).unwrap();
        repo.autter(&["checkpoint", "mock_ai", file]).unwrap();
        repo.git(&["stash", "push", "-m", &format!("stash {n}")])
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "base\n");
        repo.sync_daemon();
        shas.push(rev(repo, "refs/stash"));
        // Move HEAD on, so the stash's AI lines are not also in the working
        // log of the commit they are popped onto: only the stash note has them.
        let other = (1..=n).map(|k| format!("other {k}\n")).collect::<String>();
        fs::write(repo.path().join("other.txt"), other).unwrap();
        repo.stage_all_and_commit(&format!("move HEAD on {n}"))
            .unwrap();
        let expected = (1..=n)
            .map(|k| format!("other {k}").unattributed_human())
            .collect::<Vec<_>>();
        repo.filename("other.txt").assert_committed_lines(expected);
    }
    assert_ne!(shas[0], shas[1]);
    assert_eq!(rev(repo, "stash@{1}"), shas[0]);
    [shas[0].clone(), shas[1].clone()]
}

fn rev(repo: &TestRepo, r: &str) -> String {
    repo.git_og(&["rev-parse", r]).unwrap().trim().to_string()
}

fn commit_all(repo: &TestRepo, message: &str) {
    repo.sync_daemon();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit(message).unwrap();
}

fn assert_base(repo: &TestRepo, files: &[&str]) {
    for file in files {
        repo.filename(file)
            .assert_committed_lines(crate::lines!["base".unattributed_human()]);
    }
}

/// `file` holds the AI edit of stash `n` (1 = older, 2 = newer), with its AI
/// line attributed to AI.
fn assert_restored(repo: &TestRepo, file: &str, n: usize) {
    repo.filename(file).assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        format!("untracked {n}").unattributed_human(),
        format!("ai {n}").ai(),
    ]);
}

fn stash_count(repo: &TestRepo) -> usize {
    repo.git_og(&["stash", "list"]).unwrap().lines().count()
}

#[test]
fn test_stash_pop_twice_restores_each_stash_in_order() {
    let repo = TestRepo::new();
    two_stashes(&repo);

    repo.git(&["stash", "pop"]).unwrap();
    assert_eq!(stash_count(&repo), 1);
    commit_all(&repo, "pop newer stash");
    assert_base(&repo, &["first.txt"]);
    assert_restored(&repo, "second.txt", 2);

    repo.git(&["stash", "pop"]).unwrap();
    assert_eq!(stash_count(&repo), 0);
    commit_all(&repo, "pop older stash");
    assert_restored(&repo, "first.txt", 1);
    assert_restored(&repo, "second.txt", 2);
}

/// The top stash has no AI attribution at all (no checkpoint ran) and holds,
/// in the same file, the same line text the AI wrote into the stash below it.
/// Popping the top must not restore the AI lines of the stash below.
#[test]
fn test_stash_pop_untracked_top_stash_does_not_restore_ai_stash_below() {
    let repo = TestRepo::new();
    let path = repo.path().join("first.txt");
    fs::write(&path, "base\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    assert_base(&repo, &["first.txt"]);

    fs::write(&path, "base\nuntracked 1\n").unwrap();
    repo.autter(&["checkpoint", "human", "first.txt"]).unwrap();
    fs::write(&path, "base\nuntracked 1\nai 1\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "first.txt"])
        .unwrap();
    repo.git(&["stash"]).unwrap();
    repo.sync_daemon();
    let ai_stash = rev(&repo, "refs/stash");

    fs::write(repo.path().join("second.txt"), "other\n").unwrap();
    repo.stage_all_and_commit("move HEAD on").unwrap();
    repo.filename("second.txt")
        .assert_committed_lines(crate::lines!["other".unattributed_human()]);

    // No checkpoint at all for this edit.
    fs::write(&path, "base\nanother line\nai 1\n").unwrap();
    repo.git(&["stash"]).unwrap();
    repo.sync_daemon();
    assert_ne!(rev(&repo, "refs/stash"), ai_stash);

    repo.git(&["stash", "pop"]).unwrap();
    repo.sync_daemon();
    assert_eq!(rev(&repo, "refs/stash"), ai_stash);
    commit_all(&repo, "pop untracked stash");
    repo.filename("first.txt")
        .assert_committed_lines(crate::lines![
            "base".unattributed_human(),
            "another line".unattributed_human(),
            "ai 1".unattributed_human(),
        ]);
}

#[test]
fn test_stash_pop_explicit_top_entry_restores_top_stash() {
    let repo = TestRepo::new();
    two_stashes(&repo);

    repo.git(&["stash", "pop", "stash@{0}"]).unwrap();
    commit_all(&repo, "pop stash@{0}");
    assert_base(&repo, &["first.txt"]);
    assert_restored(&repo, "second.txt", 2);

    repo.git(&["stash", "pop", "stash@{0}"]).unwrap();
    commit_all(&repo, "pop stash@{0} again");
    assert_restored(&repo, "first.txt", 1);
    assert_restored(&repo, "second.txt", 2);
}

#[test]
fn test_stash_pop_older_entry_first_restores_older_stash() {
    let repo = TestRepo::new();
    two_stashes(&repo);

    repo.git(&["stash", "pop", "stash@{1}"]).unwrap();
    assert_eq!(stash_count(&repo), 1);
    commit_all(&repo, "pop stash@{1}");
    assert_restored(&repo, "first.txt", 1);
    assert_base(&repo, &["second.txt"]);

    repo.git(&["stash", "pop"]).unwrap();
    commit_all(&repo, "pop remaining stash");
    assert_restored(&repo, "first.txt", 1);
    assert_restored(&repo, "second.txt", 2);
}

#[test]
fn test_stash_drop_top_then_pop_restores_older_stash() {
    let repo = TestRepo::new();
    let [older, _] = two_stashes(&repo);

    repo.git(&["stash", "drop"]).unwrap();
    repo.sync_daemon();
    assert_eq!(rev(&repo, "refs/stash"), older);

    repo.git(&["stash", "pop"]).unwrap();
    commit_all(&repo, "pop older stash");
    assert_restored(&repo, "first.txt", 1);
    assert_base(&repo, &["second.txt"]);
}

#[test]
fn test_stash_drop_older_entry_then_pop_restores_newer_stash() {
    let repo = TestRepo::new();
    let [_, newer] = two_stashes(&repo);

    repo.git(&["stash", "drop", "stash@{1}"]).unwrap();
    repo.sync_daemon();
    assert_eq!(rev(&repo, "refs/stash"), newer);

    repo.git(&["stash", "pop"]).unwrap();
    commit_all(&repo, "pop newer stash");
    assert_base(&repo, &["first.txt"]);
    assert_restored(&repo, "second.txt", 2);
}

#[test]
fn test_stash_apply_top_then_older_entry_restores_each_stash() {
    let repo = TestRepo::new();
    two_stashes(&repo);

    repo.git(&["stash", "apply"]).unwrap();
    assert_eq!(stash_count(&repo), 2);
    commit_all(&repo, "apply newer stash");
    assert_base(&repo, &["first.txt"]);
    assert_restored(&repo, "second.txt", 2);

    repo.git(&["stash", "apply", "stash@{1}"]).unwrap();
    commit_all(&repo, "apply older stash");
    assert_restored(&repo, "first.txt", 1);
    assert_restored(&repo, "second.txt", 2);
}

#[test]
fn test_stash_apply_by_sha_restores_named_stash() {
    let repo = TestRepo::new();
    let [older, newer] = two_stashes(&repo);

    repo.git(&["stash", "apply", &older]).unwrap();
    commit_all(&repo, "apply older stash by sha");
    assert_restored(&repo, "first.txt", 1);
    assert_base(&repo, &["second.txt"]);

    repo.git(&["stash", "apply", &newer]).unwrap();
    commit_all(&repo, "apply newer stash by sha");
    assert_restored(&repo, "first.txt", 1);
    assert_restored(&repo, "second.txt", 2);
}

#[test]
fn test_stash_branch_restores_top_stash() {
    let repo = TestRepo::new();
    two_stashes(&repo);

    repo.git(&["stash", "branch", "from-top"]).unwrap();
    assert_eq!(stash_count(&repo), 1);
    commit_all(&repo, "stash branch from top");
    assert_base(&repo, &["first.txt"]);
    assert_restored(&repo, "second.txt", 2);

    repo.git(&["stash", "pop"]).unwrap();
    commit_all(&repo, "pop older stash");
    assert_restored(&repo, "first.txt", 1);
    assert_restored(&repo, "second.txt", 2);
}

#[test]
fn test_stash_branch_from_older_entry_restores_older_stash() {
    let repo = TestRepo::new();
    two_stashes(&repo);

    repo.git(&["stash", "branch", "from-older", "stash@{1}"])
        .unwrap();
    assert_eq!(stash_count(&repo), 1);
    commit_all(&repo, "stash branch from stash@{1}");
    assert_restored(&repo, "first.txt", 1);
    assert_base(&repo, &["second.txt"]);

    repo.git(&["stash", "pop"]).unwrap();
    commit_all(&repo, "pop newer stash");
    assert_restored(&repo, "first.txt", 1);
    assert_restored(&repo, "second.txt", 2);
}

crate::reuse_tests_in_worktree!(
    test_stash_pop_twice_restores_each_stash_in_order,
    test_stash_pop_untracked_top_stash_does_not_restore_ai_stash_below,
    test_stash_pop_explicit_top_entry_restores_top_stash,
    test_stash_pop_older_entry_first_restores_older_stash,
    test_stash_drop_top_then_pop_restores_older_stash,
    test_stash_drop_older_entry_then_pop_restores_newer_stash,
    test_stash_apply_top_then_older_entry_restores_each_stash,
    test_stash_apply_by_sha_restores_named_stash,
    test_stash_branch_restores_top_stash,
    test_stash_branch_from_older_entry_restores_older_stash,
);
