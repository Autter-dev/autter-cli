//! One label set across `stats`, `blame`, `show`, `log` and `status`, and an
//! explanation (not a troubleshooting dump) for lines without attribution.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use autter::authorship::authorship_log_serialization::AuthorshipLog;
use autter::authorship::stats::CommitStats;
use std::fs;

fn json_object(output: &str) -> serde_json::Value {
    let start = output.find('{').expect("json object start");
    let end = output.rfind('}').expect("json object end");
    serde_json::from_str(&output[start..=end]).expect("valid json")
}

fn stats_json(repo: &TestRepo, args: &[&str]) -> serde_json::Value {
    let mut full = vec!["stats"];
    full.extend_from_slice(args);
    full.push("--json");
    json_object(&repo.autter(&full).expect("autter stats --json"))
}

const REPAIR_COMMANDS: [&str; 4] = [
    "install-hooks",
    "bg restart",
    "Likely cause",
    "autter debug",
];

fn assert_no_troubleshooting(output: &str) {
    for needle in REPAIR_COMMANDS {
        assert!(
            !output.contains(needle),
            "unexpected troubleshooting `{needle}` in:\n{output}"
        );
    }
}

/// A commit Autter recorded but no agent touched gets "No AI activity
/// detected" and nothing to fix, in `stats`, `log` and `show`.
#[test]
fn plain_human_commit_reads_as_no_ai_activity() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("notes.md");

    fs::write(&file_path, "Typed by hand\n").unwrap();
    repo.stage_all_and_commit("Human commit").unwrap();
    let mut file = repo.filename("notes.md");
    file.assert_committed_lines(lines!["Typed by hand".unattributed_human()]);

    let stats = repo.autter(&["stats"]).expect("autter stats");
    assert!(
        stats.contains("No AI activity detected"),
        "stats should explain a plain human commit:\n{stats}"
    );
    assert!(
        stats.contains("1 untracked (100%)"),
        "stats should show counts:\n{stats}"
    );
    assert!(!stats.contains("Next:"), "nothing to do here:\n{stats}");
    assert_no_troubleshooting(&stats);

    let json = stats_json(&repo, &[]);
    assert_eq!(json["record"]["status"], "recorded", "{json}");
    assert_eq!(json["unknown_additions"], 1, "{json}");

    let log = repo.autter(&["log", "-n", "1"]).expect("autter log");
    assert!(log.contains("Autter stats:"), "{log}");
    assert!(!log.contains("no Autter record"), "{log}");
    assert_no_troubleshooting(&log);
}

/// A commit made before Autter recorded anything here says it predates
/// Autter, labels its lines "unknown", and suggests no repairs.
#[test]
fn commit_before_first_record_predates_autter() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("history.txt");

    fs::write(&file_path, "Old line\n").unwrap();
    repo.git_og(&["add", "."]).unwrap();
    repo.git_og_with_env(
        &["commit", "-m", "Before Autter"],
        &[
            ("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z"),
        ],
    )
    .unwrap();
    let mut file = repo.filename("history.txt");
    file.assert_committed_lines(lines!["Old line".unattributed_human()]);

    fs::write(&file_path, "Old line\nAI line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "history.txt"])
        .unwrap();
    repo.stage_all_and_commit("With Autter").unwrap();
    file.assert_committed_lines(lines!["Old line".unattributed_human(), "AI line".ai()]);

    let stats = repo
        .autter(&["stats", "HEAD~1"])
        .expect("autter stats HEAD~1");
    assert!(
        stats.contains("predates Autter in this repository"),
        "stats should say the commit predates Autter:\n{stats}"
    );
    assert!(stats.contains("1 unknown (100%)"), "{stats}");
    assert!(!stats.contains("Next:"), "{stats}");
    assert_no_troubleshooting(&stats);

    let json = stats_json(&repo, &["HEAD~1"]);
    assert_eq!(json["record"]["status"], "predates_autter", "{json}");
    // Existing field names are unchanged.
    assert_eq!(json["unknown_additions"], 1, "{json}");

    let show = repo
        .autter(&["show", "HEAD~1"])
        .expect("autter show HEAD~1");
    assert!(show.contains("predates Autter"), "{show}");
    assert_no_troubleshooting(&show);

    let log = repo.autter(&["log"]).expect("autter log");
    assert!(
        log.contains("no Autter record: predates Autter in this repository"),
        "{log}"
    );
    assert_no_troubleshooting(&log);

    // Blame markers: AI for the agent's line, unknown for the pre-Autter one.
    let blame = repo
        .autter(&["blame", "--markers", "history.txt"])
        .expect("autter blame --markers");
    let body: Vec<&str> = blame.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(body[0].starts_with("unknown "), "{blame}");
    assert!(body[1].starts_with("AI "), "{blame}");
    assert!(
        blame.contains("AI = written by an AI agent; unknown = the commit has no Autter record"),
        "blame should end with a legend for the markers shown:\n{blame}"
    );

    // Piped blame stays git-compatible: no marker column, no legend.
    let plain = repo
        .autter(&["blame", "history.txt"])
        .expect("autter blame");
    assert!(!plain.contains("unknown "), "{plain}");
    assert!(!plain.contains(" = "), "{plain}");
}

/// Known-human, untracked and AI lines each get their own marker.
#[test]
fn blame_markers_distinguish_human_untracked_and_ai() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("mixed.txt");

    fs::write(&file_path, "Untracked line\n").unwrap();
    repo.stage_all_and_commit("Untracked").unwrap();
    let mut file = repo.filename("mixed.txt");
    file.assert_committed_lines(lines!["Untracked line".unattributed_human()]);

    fs::write(&file_path, "Untracked line\nHuman line\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "mixed.txt"])
        .unwrap();
    repo.autter(&["checkpoint", "human", "mixed.txt"]).unwrap();
    fs::write(&file_path, "Untracked line\nHuman line\nAI line\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "mixed.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human and AI").unwrap();
    file.assert_committed_lines(lines![
        "Untracked line".unattributed_human(),
        "Human line".human(),
        "AI line".ai(),
    ]);

    let blame = repo
        .autter(&["blame", "--markers", "mixed.txt"])
        .expect("autter blame --markers");
    let body: Vec<&str> = blame.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(body[0].starts_with("untracked "), "{blame}");
    assert!(body[1].starts_with("human "), "{blame}");
    assert!(body[2].starts_with("AI "), "{blame}");

    let stats = repo.autter(&["stats"]).expect("autter stats");
    assert!(stats.contains("1 human (50%)"), "{stats}");
    assert!(stats.contains("1 AI (50%)"), "{stats}");
    assert!(
        stats.contains("AI % is the share of added lines an agent wrote into the file"),
        "stats should say what the AI share measures:\n{stats}"
    );
}

/// `stats A..B` prints the same numbers piped as `--json` reports, with no
/// leading blank lines, and counts known-human lines as human.
#[test]
fn range_stats_print_when_piped() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("range.txt");

    fs::write(&file_path, "Base\n").unwrap();
    let base = repo.stage_all_and_commit("Base").unwrap();
    let mut file = repo.filename("range.txt");
    file.assert_committed_lines(lines!["Base".unattributed_human()]);

    fs::write(&file_path, "Base\nAI one\nAI two\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "range.txt"])
        .unwrap();
    repo.stage_all_and_commit("AI").unwrap();
    file.assert_committed_lines(lines![
        "Base".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
    ]);

    fs::write(&file_path, "Base\nAI one\nAI two\nHuman\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "range.txt"])
        .unwrap();
    repo.stage_all_and_commit("Human").unwrap();
    file.assert_committed_lines(lines![
        "Base".unattributed_human(),
        "AI one".ai(),
        "AI two".ai(),
        "Human".human(),
    ]);

    let range = format!("{}..HEAD", base.commit_sha);
    let text = repo.autter(&["stats", &range]).expect("autter stats range");
    assert!(
        !text.starts_with('\n'),
        "range output should not start with blank lines: {text:?}"
    );
    assert!(text.contains("2 AI (67%)"), "{text}");
    assert!(text.contains("1 human (33%)"), "{text}");
    assert!(!text.contains("Committers are not using autter"), "{text}");

    let json = stats_json(&repo, &[&range]);
    assert_eq!(json["range_stats"]["ai_additions"], 2, "{json}");
    assert_eq!(json["range_stats"]["human_additions"], 1, "{json}");
}

/// The note's per-session `accepted_lines` counts only lines committed in
/// that commit, matching `stats --json` `ai_accepted`.
#[test]
fn note_accepted_lines_match_stats_after_partial_commit() {
    let repo = TestRepo::new();
    let committed = repo.path().join("committed.txt");
    let left_out = repo.path().join("left_out.txt");

    fs::write(&committed, "Seed\n").unwrap();
    fs::write(&left_out, "Seed\n").unwrap();
    repo.stage_all_and_commit("Seed").unwrap();
    let mut committed_file = repo.filename("committed.txt");
    committed_file.assert_committed_lines(lines!["Seed".unattributed_human()]);

    repo.autter(&["checkpoint", "human", "committed.txt", "left_out.txt"])
        .unwrap();
    fs::write(&committed, "Seed\nA1\nA2\nA3\n").unwrap();
    fs::write(&left_out, "Seed\nB1\nB2\nB3\nB4\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "committed.txt", "left_out.txt"])
        .unwrap();

    // Commit only one of the two files the agent edited.
    repo.git(&["add", "committed.txt"]).unwrap();
    let commit = repo.commit("Partial").unwrap();
    committed_file.assert_committed_lines(lines![
        "Seed".unattributed_human(),
        "A1".ai(),
        "A2".ai(),
        "A3".ai(),
    ]);

    let stats: CommitStats =
        serde_json::from_value(stats_json(&repo, &[])).expect("stats json as CommitStats");
    assert_eq!(stats.ai_accepted, 3);

    let note = repo
        .git_og(&["notes", "--ref=ai", "show", &commit.commit_sha])
        .expect("note exists");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("valid note");
    let attested: std::collections::HashSet<String> = log
        .attestations
        .iter()
        .flat_map(|file| file.entries.iter())
        .map(|entry| entry.hash.split("::").next().unwrap().to_string())
        .collect();
    let accepted_for_attested: u32 = log
        .metadata
        .prompts
        .iter()
        .filter(|(id, _)| attested.contains(*id))
        .map(|(_, record)| record.accepted_lines)
        .sum();
    assert_eq!(
        accepted_for_attested, stats.ai_accepted,
        "note accepted_lines should match stats ai_accepted\n{note}"
    );
    for (id, record) in &log.metadata.prompts {
        assert!(
            record.accepted_lines <= stats.ai_accepted,
            "{id} accepted_lines {} counts uncommitted lines\n{note}",
            record.accepted_lines
        );
    }
}

/// A quiet working tree is not a problem: no install advice.
#[test]
fn status_with_no_checkpoints_suggests_nothing() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("a.txt"), "One\n").unwrap();
    repo.stage_all_and_commit("Initial").unwrap();
    let mut file = repo.filename("a.txt");
    file.assert_committed_lines(lines!["One".unattributed_human()]);

    let status = repo.autter(&["status"]).expect("autter status");
    assert!(
        status.contains("No AI activity recorded since the last commit"),
        "{status}"
    );
    assert_no_troubleshooting(&status);
}

/// Outside a repository the error is one plain sentence, not git's
/// `rev-parse` failure.
#[test]
fn stats_outside_a_repository_says_so_plainly() {
    let repo = TestRepo::new();
    let outside = tempfile::tempdir().unwrap();
    let output = repo
        .autter_command(&["stats"])
        .current_dir(outside.path())
        .env("GIT_CEILING_DIRECTORIES", outside.path())
        .output()
        .expect("run autter stats");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("`autter stats` must be run inside a git repository"),
        "{stderr}"
    );
    assert!(!stderr.contains("rev-parse"), "{stderr}");
}

/// `stats --coverage` counts recorded, pre-Autter and missing commits.
#[test]
fn coverage_reports_recorded_and_predating_commits() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("cov.txt");

    fs::write(&file_path, "Old\n").unwrap();
    repo.git_og(&["add", "."]).unwrap();
    repo.git_og_with_env(
        &["commit", "-m", "Before Autter"],
        &[
            ("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z"),
        ],
    )
    .unwrap();
    let mut file = repo.filename("cov.txt");
    file.assert_committed_lines(lines!["Old".unattributed_human()]);

    fs::write(&file_path, "Old\nNew\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "cov.txt"]).unwrap();
    repo.stage_all_and_commit("AI").unwrap();
    file.assert_committed_lines(lines!["Old".unattributed_human(), "New".ai()]);

    let json = json_object(
        &repo
            .autter(&["stats", "--coverage", "--json"])
            .expect("autter stats --coverage --json"),
    );
    assert_eq!(json["commits"], 2, "{json}");
    assert_eq!(json["recorded"], 1, "{json}");
    assert_eq!(json["recorded_with_ai"], 1, "{json}");
    assert_eq!(json["predates_autter"], 1, "{json}");
    assert_eq!(json["missing"], 0, "{json}");

    let text = repo
        .autter(&["stats", "--coverage"])
        .expect("autter stats --coverage");
    assert!(text.contains("Attribution coverage:"), "{text}");
    assert!(text.contains("predate Autter in this repository"), "{text}");
}

/// The scenario the next tests share. `mixed.txt` ends up with, in order: an
/// untracked line, a known-human line, two AI lines and an untracked line
/// (commits `recorded`), then two lines from a commit capture missed
/// (`unrecorded`). A last recorded commit adds a hand-typed file.
struct MixedHistory {
    repo: TestRepo,
    base: String,
    recorded: String,
    unrecorded: String,
}

fn mixed_history() -> MixedHistory {
    let repo = TestRepo::new();
    let path = repo.path().join("mixed.txt");

    fs::write(&path, "Base 1\n").unwrap();
    let base = repo.stage_all_and_commit("Base").unwrap().commit_sha;
    let mut file = repo.filename("mixed.txt");
    file.assert_committed_lines(lines!["Base 1".unattributed_human()]);

    // One commit with a known-human line, two AI lines and a line typed with
    // nothing recording it.
    fs::write(&path, "Base 1\nKnown human line\n").unwrap();
    repo.autter(&["checkpoint", "mock_known_human", "mixed.txt"])
        .unwrap();
    repo.autter(&["checkpoint", "human", "mixed.txt"]).unwrap();
    fs::write(&path, "Base 1\nKnown human line\nAI one\nAI two\n").unwrap();
    repo.autter(&["checkpoint", "mock_ai", "mixed.txt"])
        .unwrap();
    fs::write(
        &path,
        "Base 1\nKnown human line\nAI one\nAI two\nTyped, untracked\n",
    )
    .unwrap();
    let recorded = repo.stage_all_and_commit("Mixed").unwrap().commit_sha;
    file.assert_committed_lines(lines![
        "Base 1".unattributed_human(),
        "Known human line".human(),
        "AI one".ai(),
        "AI two".ai(),
        "Typed, untracked".unattributed_human(),
    ]);

    // A commit capture missed: plain git, which the service never sees.
    fs::write(
        &path,
        "Base 1\nKnown human line\nAI one\nAI two\nTyped, untracked\nNo record 1\nNo record 2\n",
    )
    .unwrap();
    repo.git_og(&["add", "-A"]).unwrap();
    repo.git_og(&["commit", "-m", "No record"]).unwrap();
    let unrecorded = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert!(repo.read_authorship_note(&unrecorded).is_none());
    file.assert_committed_lines(lines![
        "Base 1".unattributed_human(),
        "Known human line".human(),
        "AI one".ai(),
        "AI two".ai(),
        "Typed, untracked".unattributed_human(),
        "No record 1".unattributed_human(),
        "No record 2".unattributed_human(),
    ]);

    // A recorded commit typed by hand.
    fs::write(repo.path().join("hand.txt"), "By hand\n").unwrap();
    repo.stage_all_and_commit("Human only, recorded").unwrap();
    let mut hand = repo.filename("hand.txt");
    hand.assert_committed_lines(lines!["By hand".unattributed_human()]);
    assert!(
        repo.read_authorship_note(&unrecorded).is_none(),
        "the missed commit still has no record"
    );

    MixedHistory {
        repo,
        base,
        recorded,
        unrecorded,
    }
}

/// In a range, lines from a commit with no record are `unknown`, as
/// `stats <that commit>` and `blame --markers` call them. They used to be
/// counted as `untracked` whenever another commit in the range had a record.
#[test]
fn range_stats_call_lines_of_an_unrecorded_commit_unknown() {
    let history = mixed_history();
    let repo = &history.repo;

    // The single commit and blame agree the two lines are unknown.
    let single = stats_json(repo, &[&history.unrecorded]);
    assert_eq!(single["lines"]["unknown"], 2, "{single}");
    assert_eq!(single["lines"]["untracked"], 0, "{single}");
    let blame = repo
        .autter(&["blame", "--markers", "mixed.txt"])
        .expect("autter blame --markers");
    let markers: Vec<&str> = blame
        .lines()
        .filter(|line| line.contains("No record"))
        .collect();
    assert_eq!(markers.len(), 2, "{blame}");
    assert!(
        markers.iter().all(|line| line.starts_with("unknown ")),
        "{blame}"
    );

    // The range over all three commits says the same.
    let range = format!("{}..HEAD", history.base);
    let json = stats_json(repo, &[&range]);
    assert_eq!(json["lines"]["added"], 7, "{json}");
    assert_eq!(json["lines"]["ai"], 2, "{json}");
    assert_eq!(json["lines"]["human"], 1, "{json}");
    assert_eq!(json["lines"]["untracked"], 2, "{json}");
    assert_eq!(json["lines"]["unknown"], 2, "{json}");
    assert_eq!(json["range_stats"]["unknown_additions"], 4, "{json}");
    assert_eq!(json["capture"]["status"], "captured", "{json}");

    let text = repo.autter(&["stats", &range]).expect("autter stats range");
    assert!(text.contains("2 untracked (29%)"), "{text}");
    assert!(text.contains("2 unknown (29%)"), "{text}");
    assert!(
        text.contains("1 of 3 commits has no Autter record (its lines count as unknown):"),
        "{text}"
    );
    assert!(!text.contains("commit has no"), "{text}");
    assert!(!text.contains("count as untracked"), "{text}");

    // A range that is only the unrecorded commit is all unknown.
    let only = format!("{}..{}", history.recorded, history.unrecorded);
    let json = stats_json(repo, &[&only]);
    assert_eq!(json["lines"]["unknown"], 2, "{json}");
    assert_eq!(json["lines"]["untracked"], 0, "{json}");
    assert_eq!(json["capture"]["status"], "nothing_captured", "{json}");
}
