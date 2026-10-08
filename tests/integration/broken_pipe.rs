//! A reader that closes a pipe early (e.g. `autter blame | head`) must not
//! crash the CLI. Rust ignores SIGPIPE before `main`, which turns the closed
//! pipe into an EPIPE that makes `println!` panic; the panic hook recognizes
//! that broken-pipe panic and exits quietly instead of crashing.

#[cfg(unix)]
#[test]
fn blame_into_closed_pipe_does_not_panic() {
    use crate::repos::test_repo::TestRepo;
    use std::io::Read;
    use std::process::Stdio;

    let repo = TestRepo::new();

    // A large file makes blame output overflow the pipe buffer, so autter keeps
    // writing after the reader closes and would hit EPIPE.
    let big: String = (0..40_000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(repo.path().join("big.txt"), &big).unwrap();
    repo.stage_all_and_commit("add big file").unwrap();

    let mut child = repo
        .autter_command(&["blame", "big.txt"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn autter blame");

    // Read a little, then close the read end to break the pipe.
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = [0u8; 64];
    let _ = stdout.read(&mut buf);
    drop(stdout);

    let output = child.wait_with_output().expect("wait for autter blame");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("panicked") && !stderr.contains("failed printing to stdout"),
        "autter blame panicked on a closed pipe:\n{stderr}"
    );
    // The panic hook turns the broken-pipe panic into a quiet exit 0, rather
    // than the crash's exit code 101.
    assert_eq!(
        output.status.code(),
        Some(0),
        "autter blame did not exit cleanly on a closed pipe (stderr:\n{stderr})"
    );
}
