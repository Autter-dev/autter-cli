//! First-run experience: what someone trying autter in a brand-new repository
//! sees before anything is committed.

use crate::repos::test_repo::TestRepo;

#[test]
fn status_and_stats_in_a_repository_with_no_commits_say_so() {
    // A fresh `git init`: no commit, so no HEAD to report on.
    let repo = TestRepo::new();
    std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();

    for command in ["status", "stats"] {
        let error = repo
            .autter(&[command])
            .expect_err("there is nothing to report before the first commit");
        assert!(
            error.contains("This repository has no commits yet"),
            "`autter {command}` must say there are no commits yet:\n{error}"
        );
        assert!(
            error.contains(&format!("`autter {command}`")) && error.contains("git commit"),
            "`autter {command}` must say what to do next:\n{error}"
        );
        // Not git's own error for a branch that does not exist yet.
        for raw in ["ambiguous argument", "Git CLI (", "rev-parse"] {
            assert!(
                !error.contains(raw),
                "`autter {command}` printed git's raw error (`{raw}`):\n{error}"
            );
        }
    }

    // After a first commit both commands work as before.
    repo.stage_all_and_commit("first commit").unwrap();
    repo.autter(&["status"]).expect("status after a commit");
    repo.autter(&["stats"]).expect("stats after a commit");
}
