//! `autter install --system` sets up git capture honestly: it merges its
//! trace2 keys into the user's config instead of replacing the section, and
//! never prints "Nothing to install" after writing global git config.

use crate::repos::test_repo::TestRepo;
use autter::daemon::DaemonConfig;

#[test]
fn install_system_merges_trace2_and_reports_what_it_set_up() {
    let repo = TestRepo::new();
    // The user's own trace2 settings, which install used to wipe.
    let perf_target = repo.test_home_path().join("perf.log");
    let perf_target = perf_target.to_string_lossy().to_string();
    repo.git_og(&["config", "--global", "trace2.perfTarget", &perf_target])
        .expect("write trace2.perfTarget");
    // eventBrief strips fields autter reads, so install removes it.
    repo.git_og(&["config", "--global", "trace2.eventBrief", "true"])
        .expect("write trace2.eventBrief");

    let output = repo
        .autter(&["install", "--system"])
        .expect("install --system succeeds");
    assert!(
        !output.contains("Nothing to install"),
        "install --system wrote git config, so it must not say nothing happened: {output}"
    );
    assert!(output.contains("Git capture set up"), "{output}");

    let expected = DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());
    let target = repo
        .git_og(&["config", "--global", "--get-all", "trace2.eventTarget"])
        .expect("trace2.eventTarget set");
    assert_eq!(target.trim(), expected);
    let nesting = repo
        .git_og(&["config", "--global", "trace2.eventNesting"])
        .expect("trace2.eventNesting set");
    assert_eq!(nesting.trim(), "10");
    let perf = repo
        .git_og(&["config", "--global", "trace2.perfTarget"])
        .expect("the user's trace2.perfTarget survives install");
    assert_eq!(perf.trim(), perf_target);
    assert!(
        repo.git_og(&["config", "--global", "trace2.eventBrief"])
            .is_err(),
        "trace2.eventBrief should be removed"
    );

    // Running it again changes nothing and says so.
    let again = repo
        .autter(&["install", "--system"])
        .expect("second install --system succeeds");
    assert!(again.contains("Git capture verified"), "{again}");
}
