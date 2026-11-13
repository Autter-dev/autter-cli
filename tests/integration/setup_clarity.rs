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

/// `log`, `blame` and `show` say the same thing there, with the same exit
/// code, instead of git's own error for a branch that does not exist yet.
#[test]
fn log_blame_and_show_in_a_repository_with_no_commits_say_so() {
    let repo = TestRepo::new();
    std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();

    let status_code = repo
        .autter_command(&["status"])
        .output()
        .expect("run autter status")
        .status
        .code();
    assert_eq!(status_code, Some(1), "what `status` exits with here");

    for args in [
        &["log"][..],
        &["log", "-n", "3"][..],
        &["log", "--json"][..],
        &["blame", "README.md"][..],
        &["blame", "README.md", "--json"][..],
        &["show", "HEAD"][..],
        &["show", "HEAD", "--json"][..],
    ] {
        let command = args[0];
        let output = repo
            .autter_command(args)
            .output()
            .unwrap_or_else(|e| panic!("run autter {args:?}: {e}"));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.status.code(),
            status_code,
            "`autter {}` must exit like `autter status`:\n{text}",
            args.join(" ")
        );
        assert!(
            text.contains("This repository has no commits yet"),
            "`autter {}` must say there are no commits yet:\n{text}",
            args.join(" ")
        );
        assert!(
            text.contains(&format!("`autter {command}`")) && text.contains("git commit"),
            "`autter {}` must say what to do next:\n{text}",
            args.join(" ")
        );
        for raw in [
            "ambiguous argument",
            "Git CLI (",
            "rev-parse",
            "does not have any commits yet",
            "no such ref",
            "exited with status",
            "unknown revision",
        ] {
            assert!(
                !text.contains(raw),
                "`autter {}` printed git's raw error (`{raw}`):\n{text}",
                args.join(" ")
            );
        }
    }

    // After a first commit all three work.
    repo.stage_all_and_commit("first commit").unwrap();
    repo.autter(&["log"]).expect("log after a commit");
    repo.autter(&["blame", "README.md"])
        .expect("blame after a commit");
    repo.autter(&["show", "HEAD"]).expect("show after a commit");
}
