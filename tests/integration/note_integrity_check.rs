//! The integrity check of authorship notes (`autter verify`,
//! src/authorship/note_integrity.rs).
//!
//! Three groups:
//! * tampering: notes copied, moved or edited after autter wrote them fail
//!   the check, and stats and blame stop trusting them;
//! * rewrites: every history rewrite autter handles leaves notes that verify;
//! * warnings: notes git copied but autter has not rewritten yet, notes
//!   written before the check existed, and notes rebound by an older autter.
use crate::repos::test_file::{ExpectedLineExt, TestFile};
use crate::repos::test_repo::TestRepo;
use autter::authorship::note_integrity::{read_integrity, seal_note, strip_integrity};
use serde_json::Value;
use std::fs;

const FAILURES: &[&str] = &[
    "wrong_commit",
    "content_mismatch",
    "range_out_of_bounds",
    "digest_mismatch",
    "carried_failure",
    "stamp_removed",
];

const NOTICE_PREFIX: &str = "Autter's record for";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rev(repo: &TestRepo, r: &str) -> String {
    repo.git_og(&["rev-parse", r]).unwrap().trim().to_string()
}

pub(crate) fn raw_note(repo: &TestRepo, commit: &str) -> String {
    repo.sync_daemon();
    repo.git_og(&["notes", "--ref=ai", "show", commit])
        .unwrap_or_else(|e| panic!("commit {commit} should have a note: {e}"))
}

/// Replace `commit`'s note with `content`, byte for byte (no git cleanup).
pub(crate) fn write_raw_note(repo: &TestRepo, commit: &str, content: &str) {
    let git_dir = repo
        .git_og(&["rev-parse", "--absolute-git-dir"])
        .unwrap()
        .trim()
        .to_string();
    let path = std::path::Path::new(&git_dir).join("autter-test-note");
    fs::write(&path, content).unwrap();
    let blob = repo
        .git_og(&["hash-object", "-w", path.to_str().unwrap()])
        .unwrap();
    repo.git_og(&["notes", "--ref=ai", "add", "-f", "-C", blob.trim(), commit])
        .unwrap();
}

/// Run `autter verify <target> --json <extra>`: (exit code, document, stderr).
fn verify_json(repo: &TestRepo, target: &str, extra: &[&str]) -> (i32, Value, String) {
    repo.sync_daemon();
    let mut args = vec!["verify", target, "--json"];
    args.extend_from_slice(extra);
    let out = repo.autter_command(&args).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON from autter verify {target}\n{stdout}\n{stderr}"));
    let doc: Value = serde_json::from_str(line).unwrap();
    (out.status.code().unwrap_or(-1), doc, stderr)
}

fn status_of(repo: &TestRepo, commit: &str) -> (String, i32, Value) {
    let (code, doc, _) = verify_json(repo, commit, &[]);
    let status = doc["results"][0]["status"]
        .as_str()
        .unwrap_or_else(|| panic!("no status in {doc}"))
        .to_string();
    (status, code, doc)
}

/// `commit` has `expected` status, and `autter verify` exits 5 exactly when
/// that status is a failure.
fn assert_status(repo: &TestRepo, commit: &str, expected: &str) {
    let (status, code, doc) = status_of(repo, commit);
    assert_eq!(status, expected, "autter verify {commit}: {doc}");
    let want_code = if FAILURES.contains(&expected) { 5 } else { 0 };
    assert_eq!(
        code, want_code,
        "exit code of autter verify {commit}: {doc}"
    );
    if want_code == 5 {
        assert_eq!(doc["error"]["code"], "integrity_failed", "{doc}");
        assert_eq!(doc["error"]["exit_code"], 5, "{doc}");
    }
}

fn assert_verified(repo: &TestRepo, commits: &[&str]) {
    for commit in commits {
        assert_status(repo, &rev(repo, commit), "verified");
    }
}

/// `autter blame` of `file` credits no line to AI and says why, once.
fn assert_blame_distrusts_note(repo: &TestRepo, file: &TestFile<'_>, name: &str) {
    let out = repo.autter(&["blame", name]).unwrap();
    let notices = out.lines().filter(|l| l.starts_with(NOTICE_PREFIX)).count();
    assert_eq!(
        notices, 1,
        "blame should explain the failed note once:\n{out}"
    );
    for line in out
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with(NOTICE_PREFIX))
    {
        let (author, _) = file.parse_blame_line(line);
        assert!(
            !file.is_ai_author(&author),
            "a failed note must not credit AI:\n{out}"
        );
    }
}

fn ai_additions(repo: &TestRepo, commit: &str) -> u64 {
    repo.sync_daemon();
    let out = repo
        .autter_command(&["stats", commit, "--json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no stats JSON: {stdout}"));
    let doc: Value = serde_json::from_str(line).unwrap();
    doc["ai_additions"].as_u64().unwrap()
}

/// `note` as an autter older than the integrity check wrote it: no
/// `integrity` member, and `autter_version` set to `version` (or removed,
/// for `None`). Notes from before 3.8.0 look like this, and so does a
/// stamped note whose stamp was removed and version edited.
pub(crate) fn as_unstamped_note(note: &str, version: Option<&str>) -> String {
    let body = strip_integrity(note);
    let key = "\"autter_version\": \"";
    let start = body
        .find(key)
        .unwrap_or_else(|| panic!("note has no autter_version:\n{body}"));
    let value_start = start + key.len();
    let value_end = value_start + body[value_start..].find('"').unwrap();
    match version {
        Some(version) => format!("{}{}{}", &body[..value_start], version, &body[value_end..]),
        None => {
            let line_start = body[..start].rfind('\n').map_or(0, |i| i + 1);
            let line_end = value_end + body[value_end..].find('\n').map_or(0, |i| i + 1);
            format!("{}{}", &body[..line_start], &body[line_end..])
        }
    }
}

/// Replace every note in `refs/notes/ai` with `as_unstamped_note(note,
/// version)`: a repository whose notes all predate the integrity check.
pub(crate) fn make_every_note_unstamped(repo: &TestRepo, version: &str) {
    repo.sync_daemon();
    let list = repo.git_og(&["notes", "--ref=ai", "list"]).unwrap();
    for line in list.lines() {
        let Some(commit) = line.split_whitespace().nth(1) else {
            continue;
        };
        let note = raw_note(repo, commit);
        write_raw_note(repo, commit, &as_unstamped_note(&note, Some(version)));
    }
}

/// The attestation section's first range line with its ranges replaced.
fn replace_first_range(note: &str, ranges: &str) -> String {
    let divider = note.find("\n---\n").expect("note has a divider");
    let (attestations, rest) = note.split_at(divider);
    let mut done = false;
    let edited: Vec<String> = attestations
        .lines()
        .map(|line| {
            if !done && line.starts_with("  ") {
                done = true;
                let hash = line.split_whitespace().next().unwrap();
                format!("  {hash} {ranges}")
            } else {
                line.to_string()
            }
        })
        .collect();
    assert!(done, "note has no range line:\n{note}");
    format!("{}{}", edited.join("\n"), rest)
}

/// Main has `base.txt`; branch `ai` adds `shared.txt` written by AI; main
/// then adds a different `shared.txt` by hand. Returns (ai commit, human commit).
fn ai_and_human_versions_of_one_file(repo: &TestRepo) -> (String, String) {
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "ai"]).unwrap();
    let mut shared = repo.filename("shared.txt");
    shared.set_contents(crate::lines!["ai one".ai(), "ai two".ai()]);
    let ai = repo.stage_all_and_commit("ai version").unwrap().commit_sha;
    shared.assert_committed_lines(crate::lines!["ai one".ai(), "ai two".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    let mut shared = repo.filename("shared.txt");
    shared.set_contents(crate::lines!["human one", "human two"]);
    let human = repo
        .stage_all_and_commit("human version")
        .unwrap()
        .commit_sha;
    shared.assert_committed_lines(crate::lines!["human one".human(), "human two".human()]);
    (ai, human)
}

/// Main has `base.txt`; branch `feature` adds two commits with AI lines in
/// `one.txt` and `two.txt`; main then gains `main.txt`. Leaves `feature`
/// checked out. Returns the main branch name.
fn feature_behind_main(repo: &TestRepo) -> String {
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    base.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut one = repo.filename("one.txt");
    one.set_contents(crate::lines!["human one", "ai one".ai()]);
    repo.stage_all_and_commit("feature one").unwrap();
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);

    let mut two = repo.filename("two.txt");
    two.set_contents(crate::lines!["ai two".ai(), "human two"]);
    repo.stage_all_and_commit("feature two").unwrap();
    two.assert_committed_lines(crate::lines!["ai two".ai(), "human two".human()]);

    repo.git(&["checkout", &main]).unwrap();
    let mut on_main = repo.filename("main.txt");
    on_main.set_contents(crate::lines!["main line"]);
    repo.stage_all_and_commit("main moves on").unwrap();
    on_main.assert_committed_lines(crate::lines!["main line".human()]);

    repo.git(&["checkout", "feature"]).unwrap();
    main
}

fn assert_feature_lines(repo: &TestRepo) {
    let mut one = repo.filename("one.txt");
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
    let mut two = repo.filename("two.txt");
    two.assert_committed_lines(crate::lines!["ai two".ai(), "human two".human()]);
}

/// Run `git rebase -i <onto>` with a todo list edited by `sed_script`.
#[cfg(unix)]
fn rebase_interactive(repo: &TestRepo, onto: &str, sed_script: &str) {
    let editor = format!("sed -i.bak -e '{sed_script}'");
    repo.git_with_env(
        &["rebase", "-i", onto],
        &[
            ("GIT_SEQUENCE_EDITOR", editor.as_str()),
            ("GIT_EDITOR", "true"),
        ],
        None,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Notes autter writes
// ---------------------------------------------------------------------------

#[test]
fn test_note_written_at_commit_is_verified() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["human line", "ai line".ai()]);
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["human line".human(), "ai line".ai()]);

    let note = raw_note(&repo, &commit);
    let integrity = read_integrity(&note).expect("new notes carry an integrity member");
    assert_eq!(integrity.commit, commit);
    assert_eq!(integrity.tree, rev(&repo, &format!("{commit}^{{tree}}")));
    assert_eq!(
        integrity.files.get("app.txt").cloned().flatten(),
        Some(rev(&repo, &format!("{commit}:app.txt")))
    );
    assert!(note.contains("\"schema_version\": \"authorship/3.0.0\""));
    assert_status(&repo, &commit, "verified");

    let (code, doc, _) = verify_json(&repo, "HEAD", &["--strict"]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["counts"]["verified"], 1);
    assert_eq!(doc["failed"], 0);
}

// ---------------------------------------------------------------------------
// Tampering
// ---------------------------------------------------------------------------

/// Tamper case 1: an AI note copied onto a commit that did not write it.
#[test]
fn test_note_copied_to_another_commit_fails_and_is_not_trusted() {
    let repo = TestRepo::new();
    let (ai, human) = ai_and_human_versions_of_one_file(&repo);
    let shared = repo.filename("shared.txt");
    assert_eq!(ai_additions(&repo, &human), 0);

    repo.git_og(&["notes", "--ref=ai", "copy", "-f", &ai, &human])
        .unwrap();

    assert_status(&repo, &human, "wrong_commit");
    let (_, _, doc) = status_of(&repo, &human);
    assert_eq!(doc["results"][0]["recorded_commit"], ai.as_str());
    assert_blame_distrusts_note(&repo, &shared, "shared.txt");
    assert_eq!(
        ai_additions(&repo, &human),
        0,
        "stats must not count AI lines"
    );
    // The commit the note was written for is untouched.
    assert_status(&repo, &ai, "verified");
}

/// Tamper case 2: line ranges edited by hand.
#[test]
fn test_edited_ranges_fail_with_digest_mismatch() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["ai one".ai(), "human two", "human three"]);
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines![
        "ai one".ai(),
        "human two".human(),
        "human three".human()
    ]);

    let note = raw_note(&repo, &commit);
    write_raw_note(&repo, &commit, &replace_first_range(&note, "1-3"));

    assert_status(&repo, &commit, "digest_mismatch");
    assert_blame_distrusts_note(&repo, &file, "app.txt");
    assert_eq!(ai_additions(&repo, &commit), 0);
}

/// Tamper case 3: metadata (the agent's name) edited by hand.
#[test]
fn test_edited_metadata_fails_with_digest_mismatch() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["ai one".ai()]);
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["ai one".ai()]);

    let note = raw_note(&repo, &commit);
    assert!(note.contains("mock_ai"), "{note}");
    write_raw_note(&repo, &commit, &note.replace("mock_ai", "other_ai"));

    assert_status(&repo, &commit, "digest_mismatch");
}

/// Tamper case 4: a note moved to another commit with its `commit` field and
/// digest rewritten, but the tree and blobs left as they were.
#[test]
fn test_moved_note_with_new_digest_fails_with_content_mismatch() {
    let repo = TestRepo::new();
    let (ai, human) = ai_and_human_versions_of_one_file(&repo);
    let shared = repo.filename("shared.txt");

    let note = raw_note(&repo, &ai).replace(&ai, &human);
    let mut integrity = read_integrity(&raw_note(&repo, &ai)).unwrap();
    integrity.commit = human.clone();
    write_raw_note(&repo, &human, &seal_note(&note, &integrity).unwrap());

    assert_status(&repo, &human, "content_mismatch");
    assert_blame_distrusts_note(&repo, &shared, "shared.txt");
}

/// Tamper case 5: a note without an integrity member (as older releases
/// wrote) claiming lines past the end of the file.
#[test]
fn test_legacy_note_with_lines_past_the_file_fails_range_check() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["ai one".ai(), "human two"]);
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);

    let legacy = as_unstamped_note(&raw_note(&repo, &commit), Some("3.7.1"));
    write_raw_note(&repo, &commit, &replace_first_range(&legacy, "1-40"));

    assert_status(&repo, &commit, "range_out_of_bounds");
    assert_blame_distrusts_note(&repo, &file, "app.txt");
}

/// An edit whose digest was recomputed passes this stage: anyone who can write
/// to the repository can do it. Catching it is the server-side stage's job.
#[test]
fn test_edit_with_recomputed_digest_still_verifies() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["ai one".ai(), "human two"]);
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);

    let note = raw_note(&repo, &commit);
    let integrity = read_integrity(&note).unwrap();
    let edited = seal_note(&replace_first_range(&note, "1-2"), &integrity).unwrap();
    write_raw_note(&repo, &commit, &edited);

    assert_status(&repo, &commit, "verified");
}

// ---------------------------------------------------------------------------
// Warnings
// ---------------------------------------------------------------------------

/// A note written before the integrity check (3.7.1 and earlier) reads as
/// legacy and is still used. `--strict` warns but passes for now.
#[test]
fn test_note_without_integrity_member_is_legacy() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["ai one".ai(), "human two"]);
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);

    let note = raw_note(&repo, &commit);
    write_raw_note(&repo, &commit, &as_unstamped_note(&note, Some("3.7.1")));

    assert_status(&repo, &commit, "legacy");
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);
    let (code, doc, stderr) = verify_json(&repo, &commit, &["--strict"]);
    assert_eq!(code, 0, "{doc}");
    assert!(
        stderr.contains("a later release will fail them"),
        "{stderr}"
    );
}

/// An older autter rewrites a note by replacing `base_commit_sha` in the
/// text, which leaves the integrity member naming the old commit.
#[test]
fn test_note_rebound_by_older_autter_warns() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["ai one".ai(), "human two"]);
    let original = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);

    let note = raw_note(&repo, &original);
    repo.git_og(&["commit", "--amend", "-m", "reworded"])
        .unwrap();
    let reworded = rev(&repo, "HEAD");
    assert_ne!(original, reworded);
    let field = format!("\"base_commit_sha\": \"{original}\"");
    assert!(note.contains(&field), "{note}");
    let remapped = note.replace(&field, &format!("\"base_commit_sha\": \"{reworded}\""));
    write_raw_note(&repo, &reworded, &remapped);
    // The old commit's note stays where it was; only the text was copied.
    assert!(read_integrity(&raw_note(&repo, &reworded)).unwrap().commit == original);

    assert_status(&repo, &reworded, "rebound_by_older_autter");
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);
}

/// With the background service stopped, git copies notes during a rebase
/// (notes.rewriteRef) and autter never rewrites them: the copies are
/// pending_rewrite, a warning, and are still used.
#[test]
fn test_rebase_while_service_is_stopped_is_pending_rewrite() {
    let repo = TestRepo::new();
    repo.git_og(&["config", "notes.rewriteRef", "refs/notes/ai"])
        .unwrap();
    let main = feature_behind_main(&repo);
    let before_one = rev(&repo, "HEAD~1");
    let before_two = rev(&repo, "HEAD");

    // Plain git: no hooks, no trace2, so autter does not see the rebase.
    repo.git_og(&["rebase", &main]).unwrap();
    let after_one = rev(&repo, "HEAD~1");
    let after_two = rev(&repo, "HEAD");
    assert_ne!(before_one, after_one);
    assert_ne!(before_two, after_two);

    assert_status(&repo, &after_one, "pending_rewrite");
    assert_status(&repo, &after_two, "pending_rewrite");
    assert_feature_lines(&repo);
}

// ---------------------------------------------------------------------------
// Rewrites autter handles end verified
// ---------------------------------------------------------------------------

#[test]
fn test_rebase_notes_verify() {
    let repo = TestRepo::new();
    // Git copies notes first; autter then rewrites them.
    repo.git_og(&["config", "notes.rewriteRef", "refs/notes/ai"])
        .unwrap();
    let main = feature_behind_main(&repo);
    repo.git(&["rebase", &main]).unwrap();
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD~1", "HEAD"]);
}

#[test]
fn test_rebase_without_rewrite_ref_notes_verify() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    repo.git(&["rebase", &main]).unwrap();
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD~1", "HEAD"]);
}

#[cfg(unix)]
#[test]
fn test_rebase_reorder_notes_verify() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    // Swap the two picks.
    rebase_interactive(&repo, &main, "1{h;d;};2{G;}");
    assert_eq!(
        repo.git_og(&["log", "-1", "--format=%s"]).unwrap().trim(),
        "feature one"
    );
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD~1", "HEAD"]);
}

#[cfg(unix)]
#[test]
fn test_rebase_squash_notes_verify() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    rebase_interactive(&repo, &main, "2s/^pick/squash/");
    assert_eq!(rev(&repo, "HEAD~1"), rev(&repo, &main));
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD"]);
}

#[cfg(unix)]
#[test]
fn test_rebase_fixup_notes_verify() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    rebase_interactive(&repo, &main, "2s/^pick/fixup/");
    assert_eq!(rev(&repo, "HEAD~1"), rev(&repo, &main));
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD"]);
}

#[test]
fn test_rebase_with_rename_notes_verify() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    repo.git(&["mv", "two.txt", "renamed.txt"]).unwrap();
    let mut renamed = repo.filename("renamed.txt");
    renamed.set_contents(crate::lines!["ai two".ai(), "human two", "ai three".ai()]);
    repo.stage_all_and_commit("rename and extend").unwrap();
    renamed.assert_committed_lines(crate::lines![
        "ai two".ai(),
        "human two".human(),
        "ai three".ai()
    ]);

    repo.git(&["rebase", &main]).unwrap();
    renamed.assert_committed_lines(crate::lines![
        "ai two".ai(),
        "human two".human(),
        "ai three".ai()
    ]);
    assert_verified(&repo, &["HEAD~2", "HEAD~1", "HEAD"]);
}

#[test]
fn test_amend_note_verifies() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["human one", "ai one".ai()]);
    let first = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);

    file.set_contents(crate::lines!["human one", "ai one".ai(), "ai two".ai()]);
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    let amended = rev(&repo, "HEAD");
    assert_ne!(first, amended);
    file.assert_committed_lines(crate::lines![
        "human one".human(),
        "ai one".ai(),
        "ai two".ai()
    ]);
    assert_status(&repo, &amended, "verified");
}

#[test]
fn test_cherry_pick_note_verifies() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let picked = rev(&repo, "HEAD~1");
    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["cherry-pick", &picked]).unwrap();
    let mut one = repo.filename("one.txt");
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
    assert_verified(&repo, &["HEAD"]);
}

#[test]
fn test_cherry_pick_no_commit_note_verifies() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let picked = rev(&repo, "HEAD");
    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["cherry-pick", "-n", &picked]).unwrap();
    repo.commit("picked without committing").unwrap();
    let mut two = repo.filename("two.txt");
    two.assert_committed_lines(crate::lines!["ai two".ai(), "human two".human()]);
    assert_verified(&repo, &["HEAD"]);
}

#[test]
fn test_reset_and_recommit_note_verifies() {
    let repo = TestRepo::new();
    feature_behind_main(&repo);
    repo.git(&["reset", "--soft", "HEAD~2"]).unwrap();
    repo.commit("both features at once").unwrap();
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD"]);
}

#[test]
fn test_stash_pop_and_commit_note_verifies() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    file.assert_committed_lines(crate::lines!["base".human()]);

    file.set_contents(crate::lines!["base", "ai line".ai()]);
    repo.git(&["stash"]).unwrap();
    repo.git(&["stash", "pop"]).unwrap();
    repo.stage_all_and_commit("after stash").unwrap();
    file.assert_committed_lines(crate::lines!["base".human(), "ai line".ai()]);
    assert_verified(&repo, &["HEAD"]);
}

/// Bare clone as `origin` so `autter ci local merge` can push notes.
fn add_bare_origin(repo: &TestRepo) -> tempfile::TempDir {
    let origin_dir = tempfile::tempdir().unwrap();
    let origin_path = origin_dir.path().join("origin.git");
    repo.git_og(&[
        "clone",
        "--bare",
        repo.path().to_str().unwrap(),
        origin_path.to_str().unwrap(),
    ])
    .unwrap();
    repo.git_og(&["remote", "add", "origin", origin_path.to_str().unwrap()])
        .unwrap();
    origin_dir
}

fn ci_local_merge(repo: &TestRepo, merge_sha: &str, head_sha: &str, base_sha: &str, main: &str) {
    let output = repo
        .autter(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            merge_sha,
            "--head-ref",
            "feature",
            "--head-sha",
            head_sha,
            "--base-ref",
            main,
            "--base-sha",
            base_sha,
            "--skip-fetch-notes",
            "--skip-fetch-base",
        ])
        .expect("ci local merge should succeed");
    assert!(
        output.contains("authorship rewritten successfully"),
        "{output}"
    );
}

#[test]
fn test_ci_squash_merge_note_verifies() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let head_sha = rev(&repo, "HEAD");
    let base_sha = rev(&repo, &format!("{main}~1"));
    repo.git_og(&["checkout", &main]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    repo.git_og(&["commit", "-m", "squash merge"]).unwrap();
    let merge_sha = rev(&repo, "HEAD");
    let _origin = add_bare_origin(&repo);

    ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha, &main);
    assert_feature_lines(&repo);
    assert_status(&repo, &merge_sha, "verified");
}

#[test]
fn test_ci_rebase_merge_notes_verify() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let head_sha = rev(&repo, "HEAD");
    let base_sha = rev(&repo, &format!("{main}~1"));
    repo.git_og(&["rebase", &main]).unwrap();
    repo.git_og(&["checkout", &main]).unwrap();
    repo.git_og(&["merge", "--ff-only", "feature"]).unwrap();
    let merge_sha = rev(&repo, "HEAD");
    let _origin = add_bare_origin(&repo);

    ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha, &main);
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD~1", "HEAD"]);
}

// ---------------------------------------------------------------------------
// Ranges
// ---------------------------------------------------------------------------

/// A range checks every commit with one batch of object lookups and lists
/// commits without a note only with --strict.
#[test]
fn test_verify_range_counts_statuses() {
    let repo = TestRepo::new();
    let (ai, human) = ai_and_human_versions_of_one_file(&repo);
    repo.git_og(&["notes", "--ref=ai", "remove", &human])
        .unwrap();
    let base = rev(&repo, &format!("{human}~1"));

    let (code, doc, _) = verify_json(&repo, &format!("{base}..{human}"), &[]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["commits"], 1);
    assert_eq!(doc["counts"]["missing"], 1);

    let (code, doc, _) = verify_json(&repo, &format!("{base}..ai"), &[]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["counts"]["verified"], 1);
    assert_eq!(doc["results"][0]["commit"], ai.as_str());

    let out = repo
        .autter_command(&["verify", &format!("{base}..{human}"), "--strict"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("missing"), "{stdout}");
}

// ---------------------------------------------------------------------------
// Rewrites do not launder a note that fails the check
// ---------------------------------------------------------------------------
//
// A rewrite autter performs verifies the note it carries attribution from.
// When autter writes the rewritten commit's note itself (rebase, amend,
// cherry-pick, `autter ci`), a failing source makes the new note
// `carried_failure`, naming the source commit. When the attribution goes to
// the working log of a commit not made yet (`cherry-pick -n`, reset), the
// failing note's lines are not carried and count as unknown.

/// Edit `commit`'s note after autter wrote it (the agent's name), so it fails
/// with digest_mismatch while its line ranges still fit the commit.
fn tamper_note(repo: &TestRepo, commit: &str) {
    let note = raw_note(repo, commit);
    assert!(note.contains("mock_ai"), "{note}");
    write_raw_note(repo, commit, &note.replace("mock_ai", "forged_ai"));
    assert_status(repo, commit, "digest_mismatch");
}

/// `commit`'s note was derived from `source`'s failing note: it fails with
/// `carried_failure`, names `source`, and its AI lines are not trusted.
fn assert_carried_failure(repo: &TestRepo, commit: &str, source: &str) {
    assert_status(repo, commit, "carried_failure");
    let (_, _, doc) = status_of(repo, commit);
    assert_eq!(doc["results"][0]["recorded_commit"], source, "{doc}");
    assert_eq!(
        ai_additions(repo, commit),
        0,
        "stats must not count AI lines"
    );
}

#[test]
fn test_rebase_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    repo.git_og(&["config", "notes.rewriteRef", "refs/notes/ai"])
        .unwrap();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD~1");
    tamper_note(&repo, &tampered);

    repo.git(&["rebase", &main]).unwrap();
    let rebased = rev(&repo, "HEAD~1");
    assert_ne!(tampered, rebased);
    assert_carried_failure(&repo, &rebased, &tampered);
    assert_blame_distrusts_note(&repo, &repo.filename("one.txt"), "one.txt");
    // The untampered commit rebased with it still verifies.
    let mut two = repo.filename("two.txt");
    two.assert_committed_lines(crate::lines!["ai two".ai(), "human two".human()]);
    assert_verified(&repo, &["HEAD"]);
}

#[test]
fn test_rebase_without_rewrite_ref_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD");
    tamper_note(&repo, &tampered);

    repo.git(&["rebase", &main]).unwrap();
    assert_carried_failure(&repo, &rev(&repo, "HEAD"), &tampered);
    let mut one = repo.filename("one.txt");
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
    assert_verified(&repo, &["HEAD~1"]);
}

/// With the background service stopped, git copies a tampered note byte for
/// byte; the copy is not pending_rewrite (a warning) but still fails.
#[test]
fn test_rebase_while_service_is_stopped_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    repo.git_og(&["config", "notes.rewriteRef", "refs/notes/ai"])
        .unwrap();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD");
    tamper_note(&repo, &tampered);

    // Plain git: no hooks, no trace2, so autter does not see the rebase.
    repo.git_og(&["rebase", &main]).unwrap();
    assert_status(&repo, &rev(&repo, "HEAD"), "digest_mismatch");
    assert_status(&repo, &rev(&repo, "HEAD~1"), "pending_rewrite");
    let mut one = repo.filename("one.txt");
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
}

#[cfg(unix)]
#[test]
fn test_rebase_reorder_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD");
    tamper_note(&repo, &tampered);

    // Swap the two picks: "feature two" (tampered) now comes first.
    rebase_interactive(&repo, &main, "1{h;d;};2{G;}");
    assert_eq!(
        repo.git_og(&["log", "-1", "--format=%s", "HEAD~1"])
            .unwrap()
            .trim(),
        "feature two"
    );
    assert_carried_failure(&repo, &rev(&repo, "HEAD~1"), &tampered);
    let mut one = repo.filename("one.txt");
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
    assert_verified(&repo, &["HEAD"]);
}

#[cfg(unix)]
#[test]
fn test_rebase_squash_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD");
    tamper_note(&repo, &tampered);

    rebase_interactive(&repo, &main, "2s/^pick/squash/");
    assert_eq!(rev(&repo, "HEAD~1"), rev(&repo, &main));
    // The squashed note took lines from the failing note: all of it fails.
    assert_carried_failure(&repo, &rev(&repo, "HEAD"), &tampered);
    assert_blame_distrusts_note(&repo, &repo.filename("two.txt"), "two.txt");
}

#[test]
fn test_amend_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("app.txt");
    fs::write(&file_path, "human one\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "app.txt"])
        .unwrap();
    fs::write(&file_path, "human one\nai one\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    let first = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
    tamper_note(&repo, &first);

    repo.autter(&["checkpoint", "human", "app.txt"]).unwrap();
    fs::write(&file_path, "human one\nai one\nai two\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    let amended = rev(&repo, "HEAD");
    assert_ne!(first, amended);
    assert_carried_failure(&repo, &amended, &first);
    assert_blame_distrusts_note(&repo, &file, "app.txt");
}

#[test]
fn test_cherry_pick_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD~1");
    let clean = rev(&repo, "HEAD");
    tamper_note(&repo, &tampered);

    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["cherry-pick", &tampered, &clean]).unwrap();
    assert_carried_failure(&repo, &rev(&repo, "HEAD~1"), &tampered);
    assert_blame_distrusts_note(&repo, &repo.filename("one.txt"), "one.txt");
    let mut two = repo.filename("two.txt");
    two.assert_committed_lines(crate::lines!["ai two".ai(), "human two".human()]);
    assert_verified(&repo, &["HEAD"]);
}

/// `cherry-pick -n` leaves the picked lines in the working log; the commit
/// that follows is a new note, which does not take the failing note's lines.
#[test]
fn test_cherry_pick_no_commit_does_not_carry_a_tampered_note() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD");
    tamper_note(&repo, &tampered);

    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["cherry-pick", "-n", &tampered]).unwrap();
    let picked = repo.commit("picked without committing").unwrap().commit_sha;
    let mut two = repo.filename("two.txt");
    two.assert_committed_lines(crate::lines![
        "ai two".unattributed_human(),
        "human two".unattributed_human()
    ]);
    assert_eq!(ai_additions(&repo, &picked), 0);
    let (status, code, doc) = status_of(&repo, &picked);
    assert!(
        status == "verified" || status == "missing",
        "the new note must not fail or credit the tampered lines: {doc}"
    );
    assert_eq!(code, 0, "{doc}");
}

#[test]
fn test_ci_squash_merge_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD~1");
    tamper_note(&repo, &tampered);
    let head_sha = rev(&repo, "HEAD");
    let base_sha = rev(&repo, &format!("{main}~1"));
    repo.git_og(&["checkout", &main]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    repo.git_og(&["commit", "-m", "squash merge"]).unwrap();
    let merge_sha = rev(&repo, "HEAD");
    let _origin = add_bare_origin(&repo);

    ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha, &main);
    assert_carried_failure(&repo, &merge_sha, &tampered);
}

#[test]
fn test_ci_rebase_merge_keeps_a_tampered_note_failing() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD~1");
    tamper_note(&repo, &tampered);
    let head_sha = rev(&repo, "HEAD");
    let base_sha = rev(&repo, &format!("{main}~1"));
    repo.git_og(&["rebase", &main]).unwrap();
    repo.git_og(&["checkout", &main]).unwrap();
    repo.git_og(&["merge", "--ff-only", "feature"]).unwrap();
    let merge_sha = rev(&repo, "HEAD");
    let _origin = add_bare_origin(&repo);

    ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha, &main);
    assert_carried_failure(&repo, &rev(&repo, "HEAD~1"), &tampered);
    let mut two = repo.filename("two.txt");
    two.assert_committed_lines(crate::lines!["ai two".ai(), "human two".human()]);
    assert_verified(&repo, &["HEAD"]);
}

/// A reset rebuilds the working log through blame, which already skips a
/// failing note: the recommitted lines are not credited to AI.
#[test]
fn test_reset_does_not_carry_a_tampered_note() {
    let repo = TestRepo::new();
    feature_behind_main(&repo);
    let tampered = rev(&repo, "HEAD");
    tamper_note(&repo, &tampered);

    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();
    let recommitted = repo.commit("feature two again").unwrap().commit_sha;
    let mut two = repo.filename("two.txt");
    two.assert_committed_lines(crate::lines![
        "ai two".unattributed_human(),
        "human two".unattributed_human()
    ]);
    assert_eq!(ai_additions(&repo, &recommitted), 0);
    let (_, code, doc) = status_of(&repo, &recommitted);
    assert_eq!(code, 0, "{doc}");
}

// ---------------------------------------------------------------------------
// Notes rebound by an older autter: what can still be checked is checked
// ---------------------------------------------------------------------------
//
// A rebound note's text changed, so its digest cannot hold. Its stamp's
// claims about the commit it names (tree, file blobs) and that commit's own
// note can still be checked, and `--strict` fails the note.

/// Commit `ai one` / `human two` with explicit checkpoints, then reword it
/// with plain git, so the first commit keeps its note. Returns (original,
/// reworded); `reworded` has no note yet.
fn ai_commit_then_reword(repo: &TestRepo) -> (String, String) {
    let file_path = repo.path().join("app.txt");
    fs::write(&file_path, "human two\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "app.txt"])
        .unwrap();
    fs::write(&file_path, "ai one\nhuman two\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    let original = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);

    repo.git_og(&["commit", "--amend", "-m", "reworded"])
        .unwrap();
    let reworded = rev(repo, "HEAD");
    assert_ne!(original, reworded);
    (original, reworded)
}

/// What an older autter writes for `to` from `from`'s note: the text with
/// `base_commit_sha` replaced, the integrity member kept.
fn rebind_like_older_autter(repo: &TestRepo, from: &str, to: &str) {
    let note = raw_note(repo, from);
    let field = format!("\"base_commit_sha\": \"{from}\"");
    assert!(note.contains(&field), "{note}");
    write_raw_note(
        repo,
        to,
        &note.replace(&field, &format!("\"base_commit_sha\": \"{to}\"")),
    );
    assert_eq!(read_integrity(&raw_note(repo, to)).unwrap().commit, from);
}

/// A genuine rebind warns (exit 0) without `--strict` and fails with it.
#[test]
fn test_note_rebound_by_older_autter_fails_under_strict() {
    let repo = TestRepo::new();
    let (original, reworded) = ai_commit_then_reword(&repo);
    rebind_like_older_autter(&repo, &original, &reworded);

    assert_status(&repo, &reworded, "rebound_by_older_autter");
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["ai one".ai(), "human two".human()]);

    let (code, doc, stderr) = verify_json(&repo, &reworded, &["--strict"]);
    assert_eq!(code, 5, "{doc}");
    assert_eq!(doc["error"]["code"], "integrity_failed", "{doc}");
    assert_eq!(doc["failed"], 1, "{doc}");
    assert_eq!(doc["results"][0]["status"], "rebound_by_older_autter");

    // The human report marks it FAIL and says why.
    let out = repo
        .autter_command(&["verify", &reworded, "--strict"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("FAIL") && stdout.contains("rebound_by_older_autter"),
        "{stdout}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--strict: a note rebound"),
        "{stderr}"
    );
}

/// A stamp whose claims about the commit it names are false is not a rebind:
/// it fails, also without `--strict`.
#[test]
fn test_rebound_note_whose_stamp_misdescribes_its_commit_fails() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    let file_path = repo.path().join("app.txt");
    fs::write(&file_path, "base\n").unwrap();
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["base".unattributed_human()]);

    repo.autter(&["checkpoint", "human", "app.txt"]).unwrap();
    fs::write(&file_path, "base\nai one\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines!["base".unattributed_human(), "ai one".ai()]);

    // Make the note look rebound: its stamp names `base` but still records
    // this commit's tree and blob, which are not `base`'s.
    let note = raw_note(&repo, &commit);
    let mut integrity = read_integrity(&note).unwrap();
    integrity.commit = base.clone();
    let forged = seal_note(&replace_first_range(&note, "1-2"), &integrity).unwrap();
    write_raw_note(&repo, &commit, &forged);

    assert_status(&repo, &commit, "content_mismatch");
    let (_, _, doc) = status_of(&repo, &commit);
    assert_eq!(doc["results"][0]["recorded_commit"], base.as_str(), "{doc}");
    assert_blame_distrusts_note(&repo, &file, "app.txt");
    assert_eq!(ai_additions(&repo, &commit), 0);
}

/// An older autter rebinding a note that fails the check carries the failure.
#[test]
fn test_note_rebound_from_a_tampered_note_fails() {
    let repo = TestRepo::new();
    let (original, reworded) = ai_commit_then_reword(&repo);
    tamper_note(&repo, &original);
    rebind_like_older_autter(&repo, &original, &reworded);

    assert_carried_failure(&repo, &reworded, &original);
    assert_blame_distrusts_note(&repo, &repo.filename("app.txt"), "app.txt");
}

// ---------------------------------------------------------------------------
// Stash notes (refs/notes/ai-stash)
// ---------------------------------------------------------------------------
//
// `git stash` saves the working log as a note on the stash commit, and
// `stash pop` restores it; the next commit stamps it. A stash note is stamped
// for its stash, and one that fails is not restored.

fn stash_sha(repo: &TestRepo) -> String {
    repo.sync_daemon();
    rev(repo, "refs/stash")
}

fn raw_stash_note(repo: &TestRepo, stash: &str) -> String {
    repo.sync_daemon();
    repo.git_og(&["notes", "--ref=ai-stash", "show", stash])
        .unwrap_or_else(|e| panic!("stash {stash} should have a note: {e}"))
}

fn write_raw_stash_note(repo: &TestRepo, stash: &str, content: &str) {
    let git_dir = repo
        .git_og(&["rev-parse", "--absolute-git-dir"])
        .unwrap()
        .trim()
        .to_string();
    let path = std::path::Path::new(&git_dir).join("autter-test-stash-note");
    fs::write(&path, content).unwrap();
    let blob = repo
        .git_og(&["hash-object", "-w", path.to_str().unwrap()])
        .unwrap();
    repo.git_og(&[
        "notes",
        "--ref=ai-stash",
        "add",
        "-f",
        "-C",
        blob.trim(),
        stash,
    ])
    .unwrap();
}

/// `app.txt` committed as `base`, then an untracked line and an AI line
/// (checkpointed as an agent would), stashed. HEAD then moves on (a commit of
/// another file), so the working log of the commit the stash was made on
/// plays no part when it is popped: only the stash note can restore the AI
/// line. Returns the stash commit.
fn stash_an_ai_edit(repo: &TestRepo) -> String {
    let file_path = repo.path().join("app.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["base".unattributed_human()]);

    fs::write(&file_path, "base\nuntracked line\n").unwrap();
    repo.autter(&["checkpoint", "human", "app.txt"]).unwrap();
    fs::write(&file_path, "base\nuntracked line\nai line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    repo.git(&["stash"]).unwrap();
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "base\n");
    let stash = stash_sha(repo);

    fs::write(repo.path().join("other.txt"), "other\n").unwrap();
    repo.stage_all_and_commit("move HEAD on").unwrap();
    let mut other = repo.filename("other.txt");
    other.assert_committed_lines(crate::lines!["other".unattributed_human()]);
    stash
}

/// Pop the top stash and commit it; returns the new commit.
fn pop_and_commit(repo: &TestRepo) -> String {
    repo.git(&["stash", "pop"]).unwrap();
    repo.sync_daemon();
    repo.git(&["add", "."]).unwrap();
    repo.commit("after stash").unwrap().commit_sha
}

#[test]
fn test_stash_note_is_stamped_and_restores() {
    let repo = TestRepo::new();
    let stash = stash_an_ai_edit(&repo);

    let note = raw_stash_note(&repo, &stash);
    let integrity = read_integrity(&note).expect("stash notes carry an integrity member");
    assert_eq!(integrity.commit, stash);
    assert_eq!(integrity.tree, rev(&repo, &format!("{stash}^{{tree}}")));
    assert_eq!(
        integrity.files.get("app.txt").cloned().flatten(),
        Some(rev(&repo, &format!("{stash}:app.txt")))
    );

    let commit = pop_and_commit(&repo);
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "untracked line".unattributed_human(),
        "ai line".ai()
    ]);
    assert_status(&repo, &commit, "verified");
}

/// A stash note edited before the pop (an untracked line claimed for AI) is
/// not restored: its lines count as untracked, and the next commit's note
/// does not credit them.
#[test]
fn test_edited_stash_note_is_not_restored() {
    let repo = TestRepo::new();
    let stash = stash_an_ai_edit(&repo);
    let note = raw_stash_note(&repo, &stash);
    write_raw_stash_note(&repo, &stash, &replace_first_range(&note, "2-3"));

    let commit = pop_and_commit(&repo);
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "untracked line".unattributed_human(),
        "ai line".unattributed_human()
    ]);
    assert_eq!(ai_additions(&repo, &commit), 0);
    let (status, code, doc) = status_of(&repo, &commit);
    assert!(status == "verified" || status == "missing", "{doc}");
    assert_eq!(code, 0, "{doc}");
}

/// Another stash's note copied onto this stash is not restored.
#[test]
fn test_stash_note_copied_to_another_stash_is_not_restored() {
    let repo = TestRepo::new();
    let ai_stash = stash_an_ai_edit(&repo);

    // A second stash with no checkpoint at all, so autter saves no note
    // for it.
    let file_path = repo.path().join("app.txt");
    fs::write(&file_path, "base\nanother line\nai line\n").unwrap();
    repo.git(&["stash"]).unwrap();
    let plain_stash = stash_sha(&repo);
    assert_ne!(ai_stash, plain_stash);
    repo.git_og(&[
        "notes",
        "--ref=ai-stash",
        "copy",
        "-f",
        &ai_stash,
        &plain_stash,
    ])
    .unwrap();

    repo.git(&["stash", "pop"]).unwrap();
    repo.sync_daemon();
    assert_eq!(stash_sha(&repo), ai_stash);
    repo.git(&["add", "."]).unwrap();
    let commit = repo.commit("after stash").unwrap().commit_sha;
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "another line".unattributed_human(),
        "ai line".unattributed_human()
    ]);
    assert_eq!(ai_additions(&repo, &commit), 0);
}

/// A stash note written before stash notes were stamped (no integrity
/// member) still restores, so stashes made before an upgrade keep their AI
/// lines.
#[test]
fn test_unstamped_stash_note_from_older_autter_still_restores() {
    let repo = TestRepo::new();
    let stash = stash_an_ai_edit(&repo);
    let note = raw_stash_note(&repo, &stash);
    write_raw_stash_note(&repo, &stash, &as_unstamped_note(&note, Some("3.8.1")));
    assert!(read_integrity(&raw_stash_note(&repo, &stash)).is_none());

    let commit = pop_and_commit(&repo);
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "untracked line".unattributed_human(),
        "ai line".ai()
    ]);
    assert_status(&repo, &commit, "verified");
}

// ---------------------------------------------------------------------------
// Notes backfill from the notes database
// ---------------------------------------------------------------------------
//
// `autter notes backfill` copies notes that exist only in the notes database
// into refs/notes/ai. It writes them as stored, without a new stamp, so a
// note changed in the database still fails and a legacy note stays legacy.

/// Commit an AI line, then move its note from git into the notes database as
/// `db_note(note)`. Returns the commit.
fn note_only_in_db(repo: &TestRepo, db_note: impl Fn(&str) -> String) -> String {
    let file_path = repo.path().join("app.txt");
    fs::write(&file_path, "human one\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "app.txt"])
        .unwrap();
    fs::write(&file_path, "human one\nai one\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "app.txt"]).unwrap();
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);

    let note = raw_note(repo, &commit);
    repo.git_og(&["notes", "--ref=ai", "remove", &commit])
        .unwrap();
    let db_path = repo
        .test_home_path()
        .join(".autter")
        .join("internal")
        .join("notes-db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let mut db = autter::notes::db::NotesDatabase::open_at_path(&db_path).unwrap();
    db.cache_synced_notes(&[(commit.clone(), db_note(&note))])
        .unwrap();
    drop(db);
    commit
}

/// Run `autter notes backfill`: (stdout, stderr).
fn backfill(repo: &TestRepo) -> (String, String) {
    let out = repo
        .autter_command(&["notes", "backfill"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn test_backfilled_note_verifies() {
    let repo = TestRepo::new();
    let commit = note_only_in_db(&repo, |note| note.to_string());
    let db_note = raw_note_from_db_seed(&repo, &commit);

    let (stdout, stderr) = backfill(&repo);
    assert!(stdout.contains("Backfilled 1 authorship note "), "{stdout}");
    assert!(!stderr.contains("integrity check"), "{stderr}");
    assert_eq!(raw_note(&repo, &commit).trim(), db_note.trim());
    assert_status(&repo, &commit, "verified");
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
}

/// The note as `note_only_in_db` seeded it.
fn raw_note_from_db_seed(repo: &TestRepo, commit: &str) -> String {
    let db_path = repo
        .test_home_path()
        .join(".autter")
        .join("internal")
        .join("notes-db");
    let db = autter::notes::db::NotesDatabase::open_at_path(&db_path).unwrap();
    db.get_notes(&[commit]).unwrap().remove(commit).unwrap()
}

/// A note changed in the notes database is written unchanged and still fails:
/// it is not stamped valid on the way into git.
#[test]
fn test_backfilled_tampered_note_still_fails() {
    let repo = TestRepo::new();
    let commit = note_only_in_db(&repo, |note| {
        assert!(note.contains("mock_ai"), "{note}");
        note.replace("mock_ai", "forged_ai")
    });

    let (stdout, stderr) = backfill(&repo);
    assert!(stdout.contains("Backfilled 1 authorship note "), "{stdout}");
    let note = raw_note(&repo, &commit);
    assert!(note.contains("forged_ai"), "{note}");
    assert_status(&repo, &commit, "digest_mismatch");
    assert!(
        stderr.contains("1 of them fails the integrity check") && stderr.contains(&commit),
        "{stderr}"
    );
    assert_blame_distrusts_note(&repo, &repo.filename("app.txt"), "app.txt");
    assert_eq!(ai_additions(&repo, &commit), 0);
}

/// A note written before the integrity check stays legacy: stamping it on
/// the way into git would make an unchecked note verified. Later commits
/// with stamped notes do not change that: the old commit comes before them.
#[test]
fn test_backfilled_legacy_note_stays_legacy() {
    let repo = TestRepo::new();
    let commit = note_only_in_db(&repo, |note| as_unstamped_note(note, Some("3.7.1")));

    let (stdout, _) = backfill(&repo);
    assert!(stdout.contains("Backfilled 1 authorship note "), "{stdout}");
    assert!(read_integrity(&raw_note(&repo, &commit)).is_none());
    assert_status(&repo, &commit, "legacy");
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);

    // Stamping begins after the backfilled commit.
    let mut later = repo.filename("later.txt");
    later.set_contents(crate::lines!["ai later".ai()]);
    let stamped = repo.stage_all_and_commit("later").unwrap().commit_sha;
    later.assert_committed_lines(crate::lines!["ai later".ai()]);
    assert_status(&repo, &stamped, "verified");
    assert_status(&repo, &commit, "legacy");
    let (code, doc, _) = verify_json(&repo, &commit, &["--strict"]);
    assert_eq!(code, 0, "{doc}");
    file.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
}

/// A stamped note copied into the notes database with its stamp removed is
/// written as stored by the backfill, and fails like any note whose stamp
/// was removed.
#[test]
fn test_backfilled_note_with_its_stamp_removed_fails() {
    let repo = TestRepo::new();
    let commit = note_only_in_db(&repo, strip_integrity);

    let (stdout, stderr) = backfill(&repo);
    assert!(stdout.contains("Backfilled 1 authorship note "), "{stdout}");
    assert_status(&repo, &commit, "stamp_removed");
    assert!(
        stderr.contains("1 of them fails the integrity check") && stderr.contains(&commit),
        "{stderr}"
    );
    assert_eq!(ai_additions(&repo, &commit), 0);
}

// ---------------------------------------------------------------------------
// Notes whose stamp was removed
// ---------------------------------------------------------------------------
//
// A note without an `integrity` member used to read as legacy whatever it
// said, so removing the stamp from a note and editing it passed `autter
// verify`, also with `--strict`. An unstamped note now passes as legacy only
// when it can be one: its `autter_version` names a release older than 3.8.0
// (autter 3.8.0 and later stamp every note), and, when its commit descends
// from a commit whose note is stamped, it is a warning
// (`unstamped_by_older_autter`, a failure under `--strict`).

fn stats_json(repo: &TestRepo, commit: &str) -> Value {
    repo.sync_daemon();
    let out = repo
        .autter_command(&["stats", commit, "--json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no stats JSON: {stdout}"));
    serde_json::from_str(line).unwrap()
}

/// An AI commit (stamped) that comes before the commits under test, so
/// stamping began before them.
fn stamped_base_commit(repo: &TestRepo) -> String {
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base ai".ai(), "base human"]);
    let commit = repo
        .stage_all_and_commit("stamped base")
        .unwrap()
        .commit_sha;
    base.assert_committed_lines(crate::lines!["base ai".ai(), "base human".human()]);
    assert_status(repo, &commit, "verified");
    commit
}

/// Five AI lines and one human line in `app.txt` (83% AI). Returns the commit.
fn five_of_six_ai_lines(repo: &TestRepo) -> String {
    let mut file = repo.filename("app.txt");
    file.set_contents(crate::lines![
        "ai 1".ai(),
        "ai 2".ai(),
        "ai 3".ai(),
        "ai 4".ai(),
        "ai 5".ai(),
        "human 6"
    ]);
    let commit = repo.stage_all_and_commit("ai commit").unwrap().commit_sha;
    file.assert_committed_lines(crate::lines![
        "ai 1".ai(),
        "ai 2".ai(),
        "ai 3".ai(),
        "ai 4".ai(),
        "ai 5".ai(),
        "human 6".human()
    ]);
    commit
}

/// The reported case: a 3.9.1 note's stamp removed and an attestation edited
/// (five AI lines down to two, 83% to 33%) passed as legacy, also with
/// `--strict`, and stats reported 33% AI. It now fails with `stamp_removed`
/// (exit 5, with and without `--strict`) and its lines count as unknown.
#[test]
fn test_note_with_its_stamp_removed_and_ranges_edited_fails() {
    let repo = TestRepo::new();
    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["readme"]);
    repo.stage_all_and_commit("readme").unwrap();
    readme.assert_committed_lines(crate::lines!["readme".human()]);
    let base = stamped_base_commit(&repo);
    let commit = five_of_six_ai_lines(&repo);
    assert_status(&repo, &commit, "verified");
    let stats = stats_json(&repo, &commit);
    assert_eq!(stats["ai_additions"], 5, "{stats}");

    let note = raw_note(&repo, &commit);
    assert!(
        note.contains("\"autter_version\": \"development:"),
        "{note}"
    );
    let edited = replace_first_range(&strip_integrity(&note), "1-2");
    assert!(edited.contains(" 1-2\n"), "{edited}");
    write_raw_note(&repo, &commit, &edited);

    assert_status(&repo, &commit, "stamp_removed");
    let (code, doc, _) = verify_json(&repo, &commit, &["--strict"]);
    assert_eq!(code, 5, "{doc}");
    assert_eq!(doc["results"][0]["status"], "stamp_removed", "{doc}");
    assert_eq!(doc["error"]["code"], "integrity_failed", "{doc}");
    let detail = doc["results"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("stamp was removed"), "{detail}");

    // Not 2 of 6 (33%) AI: the note is not trusted, so all six are unknown.
    let stats = stats_json(&repo, &commit);
    assert_eq!(stats["ai_additions"], 0, "{stats}");
    assert_eq!(stats["unknown_additions"], 6, "{stats}");
    assert_blame_distrusts_note(&repo, &repo.filename("app.txt"), "app.txt");
    let out = repo.autter_command(&["stats", &commit]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(NOTICE_PREFIX) && stderr.contains("had its integrity stamp removed"),
        "{stderr}"
    );

    // A range over both commits fails on the tampered one only.
    let (code, doc, _) = verify_json(&repo, &format!("{base}~1..{commit}"), &[]);
    assert_eq!(code, 5, "{doc}");
    assert_eq!(doc["counts"]["verified"], 1, "{doc}");
    assert_eq!(doc["counts"]["stamp_removed"], 1, "{doc}");
    assert_eq!(doc["failed"], 1, "{doc}");
    assert_status(&repo, &base, "verified");
}

/// The very first stamped commit has no stamped commit before it; its note
/// says which autter wrote it, and every autter since 3.8.0 stamps.
#[test]
fn test_first_stamped_note_with_its_stamp_removed_fails() {
    let repo = TestRepo::new();
    let commit = five_of_six_ai_lines(&repo);
    let note = raw_note(&repo, &commit);
    write_raw_note(&repo, &commit, &strip_integrity(&note));

    assert_status(&repo, &commit, "stamp_removed");
    assert_eq!(ai_additions(&repo, &commit), 0);
}

/// Removing the stamp and the version too: the note no longer says who wrote
/// it, but its commit comes after a stamped commit, so it was not written
/// before stamping began.
#[test]
fn test_stamp_and_version_removed_after_stamping_fails() {
    let repo = TestRepo::new();
    stamped_base_commit(&repo);
    let commit = five_of_six_ai_lines(&repo);
    let note = raw_note(&repo, &commit);
    let edited = replace_first_range(&as_unstamped_note(&note, None), "1-2");
    assert!(!edited.contains("autter_version"), "{edited}");
    write_raw_note(&repo, &commit, &edited);

    assert_status(&repo, &commit, "stamp_removed");
    assert_eq!(ai_additions(&repo, &commit), 0);
    assert_blame_distrusts_note(&repo, &repo.filename("app.txt"), "app.txt");
}

/// A repository whose notes all predate the integrity check (no stamps,
/// autter 3.7.1) verifies as legacy, also with `--strict`, and its lines are
/// still used. A note with no version at all is legacy there too: nothing
/// in the repository is stamped.
#[test]
fn test_repository_with_only_pre_3_8_notes_is_legacy() {
    let repo = TestRepo::new();
    let mut one = repo.filename("one.txt");
    one.set_contents(crate::lines!["ai one".ai(), "human one"]);
    let first = repo.stage_all_and_commit("one").unwrap().commit_sha;
    one.assert_committed_lines(crate::lines!["ai one".ai(), "human one".human()]);
    let mut two = repo.filename("two.txt");
    two.set_contents(crate::lines!["human two", "ai two".ai()]);
    let second = repo.stage_all_and_commit("two").unwrap().commit_sha;
    two.assert_committed_lines(crate::lines!["human two".human(), "ai two".ai()]);

    make_every_note_unstamped(&repo, "3.7.1");
    let note = raw_note(&repo, &second);
    write_raw_note(&repo, &second, &as_unstamped_note(&note, None));

    assert_status(&repo, &first, "legacy");
    assert_status(&repo, &second, "legacy");
    let (code, doc, stderr) = verify_json(&repo, &format!("{first}..{second}"), &["--strict"]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["counts"]["legacy"], 1, "{doc}");
    assert_eq!(doc["failed"], 0, "{doc}");
    assert!(
        stderr.contains("a later release will fail them"),
        "{stderr}"
    );
    let (code, doc, _) = verify_json(&repo, &first, &["--strict"]);
    assert_eq!(code, 0, "{doc}");
    one.assert_committed_lines(crate::lines!["ai one".ai(), "human one".human()]);
    two.assert_committed_lines(crate::lines!["human two".human(), "ai two".ai()]);
    assert_eq!(ai_additions(&repo, &second), 1);
}

/// A teammate still on autter 3.7 commits after this machine started
/// stamping: their unstamped notes say 3.7.1. That is
/// `unstamped_by_older_autter`, a warning whose lines still count, and a
/// failure only under `--strict`, like `rebound_by_older_autter`: the version
/// cannot be checked, and someone who removes a stamp can edit it too.
///
/// "After" is topology, not dates: a 3.7.1 note on a branch that forked
/// before the first stamped commit stays legacy even though it was committed
/// later.
#[test]
fn test_older_autter_teammate_after_stamping_warns_and_fails_only_under_strict() {
    let repo = TestRepo::new();
    // A history from before the upgrade: every note from 3.7.1.
    let mut old = repo.filename("old.txt");
    old.set_contents(crate::lines!["old ai".ai(), "old human"]);
    let fork_point = repo.stage_all_and_commit("old").unwrap().commit_sha;
    old.assert_committed_lines(crate::lines!["old ai".ai(), "old human".human()]);
    make_every_note_unstamped(&repo, "3.7.1");
    let main = repo.current_branch();
    repo.git(&["branch", "side"]).unwrap();

    // This machine upgrades: the next commit's note is stamped.
    let stamped = stamped_base_commit(&repo);

    // A teammate on 3.7 commits on top of it.
    let mut team = repo.filename("team.txt");
    team.set_contents(crate::lines!["team ai".ai(), "team human"]);
    let teammate = repo.stage_all_and_commit("teammate").unwrap().commit_sha;
    team.assert_committed_lines(crate::lines!["team ai".ai(), "team human".human()]);
    let note = raw_note(&repo, &teammate);
    write_raw_note(&repo, &teammate, &as_unstamped_note(&note, Some("3.7.1")));

    // Another teammate on 3.7 commits later, on a branch from before stamping.
    repo.git(&["checkout", "side"]).unwrap();
    let mut side = repo.filename("side.txt");
    side.set_contents(crate::lines!["side ai".ai(), "side human"]);
    let side_commit = repo.stage_all_and_commit("side").unwrap().commit_sha;
    side.assert_committed_lines(crate::lines!["side ai".ai(), "side human".human()]);
    let note = raw_note(&repo, &side_commit);
    write_raw_note(
        &repo,
        &side_commit,
        &as_unstamped_note(&note, Some("3.7.1")),
    );
    repo.git(&["checkout", &main]).unwrap();

    assert_status(&repo, &fork_point, "legacy");
    assert_status(&repo, &stamped, "verified");
    assert_status(&repo, &side_commit, "legacy");
    assert_status(&repo, &teammate, "unstamped_by_older_autter");
    let (_, _, doc) = status_of(&repo, &teammate);
    let detail = doc["results"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("3.7.1"), "{detail}");

    // Its lines are still used, as a warning's are.
    team.assert_committed_lines(crate::lines!["team ai".ai(), "team human".human()]);
    assert_eq!(ai_additions(&repo, &teammate), 1);

    // --strict fails it, and only it.
    let (code, doc, _) = verify_json(&repo, &teammate, &["--strict"]);
    assert_eq!(code, 5, "{doc}");
    assert_eq!(doc["failed"], 1, "{doc}");
    let (code, doc, _) = verify_json(&repo, &side_commit, &["--strict"]);
    assert_eq!(code, 0, "{doc}");
    let out = repo
        .autter_command(&["verify", &teammate, "--strict"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("FAIL") && stdout.contains("unstamped_by_older_autter"),
        "{stdout}"
    );

    // When the stamped commit's own note is changed back to a 3.7.1 one,
    // nothing in the history is stamped any more and the teammate's note is
    // legacy again (the cached floor is rebuilt, not reused).
    let note = raw_note(&repo, &stamped);
    write_raw_note(&repo, &stamped, &as_unstamped_note(&note, Some("3.7.1")));
    assert_status(&repo, &stamped, "legacy");
    assert_status(&repo, &teammate, "legacy");
    old.assert_committed_lines(crate::lines!["old ai".ai(), "old human".human()]);
}

/// Rewrites stamp what they write: a branch whose notes are all from 3.7.1,
/// rebased onto commits with stamped notes, gets notes that verify (not
/// `stamp_removed` or `unstamped_by_older_autter`); amending and
/// cherry-picking a stamped commit leave notes that verify.
#[test]
fn test_rewrites_after_stamping_began_verify() {
    let repo = TestRepo::new();
    let main = feature_behind_main(&repo);
    make_every_note_unstamped(&repo, "3.7.1");
    repo.git(&["checkout", &main]).unwrap();
    let stamped = stamped_base_commit(&repo);
    repo.git(&["checkout", "feature"]).unwrap();
    assert_status(&repo, &rev(&repo, "HEAD"), "legacy");

    repo.git(&["rebase", &main]).unwrap();
    repo.sync_daemon();
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD~1", "HEAD"]);
    assert_status(&repo, &stamped, "verified");

    // Amend a stamped commit.
    repo.git(&["commit", "--amend", "-m", "feature two, reworded"])
        .unwrap();
    repo.sync_daemon();
    assert_feature_lines(&repo);
    assert_verified(&repo, &["HEAD"]);

    // Cherry-pick a stamped commit onto main.
    let picked = rev(&repo, "HEAD~1");
    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["cherry-pick", &picked]).unwrap();
    repo.sync_daemon();
    let mut one = repo.filename("one.txt");
    one.assert_committed_lines(crate::lines!["human one".human(), "ai one".ai()]);
    assert_verified(&repo, &["HEAD"]);
}

/// A stash note with its stamp removed is not restored: the release that
/// wrote it stamps every stash note. (Until now it restored like a note
/// from 3.8.1.)
#[test]
fn test_stash_note_with_its_stamp_removed_is_not_restored() {
    let repo = TestRepo::new();
    let stash = stash_an_ai_edit(&repo);
    let note = raw_stash_note(&repo, &stash);
    write_raw_stash_note(&repo, &stash, &strip_integrity(&note));

    let commit = pop_and_commit(&repo);
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "untracked line".unattributed_human(),
        "ai line".unattributed_human()
    ]);
    assert_eq!(ai_additions(&repo, &commit), 0);
}

/// A stash note with its stamp and version removed, made on a commit after
/// stamping began, is not restored either.
#[test]
fn test_stash_note_without_stamp_or_version_after_stamping_is_not_restored() {
    let repo = TestRepo::new();
    stamped_base_commit(&repo);
    let stash = stash_an_ai_edit(&repo);
    let note = raw_stash_note(&repo, &stash);
    write_raw_stash_note(&repo, &stash, &as_unstamped_note(&note, None));

    let commit = pop_and_commit(&repo);
    let mut file = repo.filename("app.txt");
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "untracked line".unattributed_human(),
        "ai line".unattributed_human()
    ]);
    assert_eq!(ai_additions(&repo, &commit), 0);
}
