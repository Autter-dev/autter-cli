//! The shell installer (`install.sh`) as people run it: piped into `sh`, run
//! as a file, and with `--dry-run`. No test downloads or installs anything.
#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn install_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh")
}

/// dash is `sh` on Debian and Ubuntu and ships with macOS; the tests that need
/// a non-bash POSIX shell are skipped where it is missing.
fn dash() -> Option<PathBuf> {
    ["/bin/dash", "/usr/bin/dash"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// Everything under `home`, so a test can show the installer wrote nothing.
fn files_under(home: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![home.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path.strip_prefix(home).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

/// Run a shell with a clean environment and `home` as HOME. `stdin` is the
/// script when it is piped in.
fn run(mut command: Command, home: &Path, envs: &[(&str, &str)], stdin: Option<&[u8]>) -> Output {
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("SHELL", "/bin/zsh")
        .envs(envs.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn shell");
    if let Some(script) = stdin {
        // The shell may exit before reading the whole script (that is what
        // the non-bash path does), which closes the pipe.
        let _ = child.stdin.take().unwrap().write_all(script);
    }
    child.wait_with_output().expect("wait for shell")
}

fn home_with_zshrc() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".zshrc"), "# mine\n").unwrap();
    home
}

#[test]
fn piped_into_a_non_bash_sh_says_to_use_bash_and_changes_nothing() {
    let Some(dash) = dash() else {
        eprintln!("skipped: no dash on this machine");
        return;
    };
    let home = home_with_zshrc();
    let script = std::fs::read(install_script()).unwrap();

    // `curl -fsSL …/install.sh | sh` where sh is dash.
    let output = run(Command::new(dash), home.path(), &[], Some(&script));
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        !stderr.contains("Illegal option"),
        "the shell's own error must not be what the user sees: {stderr}"
    );
    assert!(
        stderr.contains("needs bash") && stderr.contains("| bash"),
        "must name bash and the command to run: {stderr}"
    );
    assert!(stderr.contains("Nothing was installed"), "{stderr}");
    assert_eq!(files_under(home.path()), vec![".zshrc".to_string()]);
}

#[test]
fn run_as_a_file_with_a_non_bash_sh_hands_over_to_bash() {
    let Some(dash) = dash() else {
        eprintln!("skipped: no dash on this machine");
        return;
    };
    let home = home_with_zshrc();

    // `sh install.sh --dry-run` where sh is dash: bash takes over and the dry
    // run (bash-only code) prints its plan.
    let mut command = Command::new(dash);
    command.arg(install_script()).arg("--dry-run");
    let output = run(command, home.path(), &[], None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("Dry run:"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn dry_run_prints_the_plan_and_touches_nothing() {
    let home = home_with_zshrc();
    let script = std::fs::read(install_script()).unwrap();

    // `curl -fsSL …/install.sh | bash -s -- --dry-run`, pinned to a release
    // so the plan names the exact download.
    let mut command = Command::new("bash");
    command.args(["-s", "--", "--dry-run"]);
    let output = run(
        command,
        home.path(),
        &[("AUTTER_RELEASE_TAG", "v3.5.1")],
        Some(&script),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("Dry run: nothing is downloaded, written or started."),
        "{stdout}"
    );
    // Each thing a real install does is named: the download and its check,
    // the binary path, the shell file it edits, the ping, and onboarding.
    assert!(
        stdout.contains("/releases/download/v3.5.1/autter-") && stdout.contains("SHA-256"),
        "{stdout}"
    );
    assert!(stdout.contains(".autter/bin/autter"), "{stdout}");
    assert!(
        stdout.contains(&home.path().join(".zshrc").display().to_string()),
        "{stdout}"
    );
    assert!(stdout.contains("anonymous install ping"), "{stdout}");
    assert!(
        stdout.contains("autter onboard") && stdout.contains("asks before"),
        "{stdout}"
    );
    assert!(stdout.contains("autter uninstall"), "{stdout}");

    // Nothing was created, and the shell file was not edited.
    assert_eq!(files_under(home.path()), vec![".zshrc".to_string()]);
    assert_eq!(
        std::fs::read_to_string(home.path().join(".zshrc")).unwrap(),
        "# mine\n"
    );
}

#[test]
fn dry_run_by_environment_variable_and_unknown_options() {
    let home = home_with_zshrc();

    let mut by_env = Command::new("bash");
    by_env.arg(install_script());
    let output = run(
        by_env,
        home.path(),
        &[("AUTTER_INSTALL_DRY_RUN", "1")],
        None,
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Dry run:"));

    // A typo never falls through to a real install.
    let mut typo = Command::new("bash");
    typo.arg(install_script()).arg("--dryrun");
    let output = run(typo, home.path(), &[], None);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option '--dryrun'"));

    let mut help = Command::new("bash");
    help.arg(install_script()).arg("--help");
    let output = run(help, home.path(), &[], None);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("--dry-run"));

    assert_eq!(files_under(home.path()), vec![".zshrc".to_string()]);
}

#[test]
fn download_failures_point_at_npm() {
    // The three downloads (latest release, checksums, binary) each end in an
    // error that names the URL and the npm route; a bare "HTTP error" left
    // people with a 502 and nowhere to go.
    let script = std::fs::read_to_string(install_script()).unwrap();
    assert!(script.contains("npm install -g @autter/cli"));
    for failure in [
        "Failed to resolve the latest release from",
        "Failed to download release checksums from",
        "Failed to download the autter binary from",
    ] {
        let line = script
            .lines()
            .find(|line| line.contains(failure))
            .unwrap_or_else(|| panic!("no error for: {failure}"));
        assert!(line.contains("NPM_FALLBACK_HINT"), "{line}");
    }
    // The binary download shows progress at a terminal.
    assert!(script.contains("--progress-bar"));
    assert!(
        !script.contains("--location --silent --show-error -o"),
        "the binary download must not be forced silent"
    );
}
