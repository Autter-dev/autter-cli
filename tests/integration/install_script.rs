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

/// A stand-in for the autter binary: it answers `--version`, logs every other
/// invocation to `calls.log` beside itself, and fails `exchange-nonce` when
/// `FAKE_EXCHANGE_FAILS=1`.
fn fake_autter(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-autter");
    let log = dir.join("calls.log");
    std::fs::write(
        &path,
        format!(
            "#!/bin/bash\n\
             if [ \"$1\" = \"--version\" ]; then echo \"9.9.9\"; exit 0; fi\n\
             echo \"$1\" >> '{}'\n\
             if [ \"$1\" = \"exchange-nonce\" ] && [ \"${{FAKE_EXCHANGE_FAILS:-}}\" = \"1\" ]; then exit 1; fi\n\
             exit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Install the stand-in binary with no controlling terminal (a new session),
/// so the installer takes its no-terminal path wherever the tests run.
fn install_without_a_terminal(envs: &[(&str, &str)]) -> (Output, Vec<String>) {
    install_without_a_terminal_in(&home_with_zshrc(), envs)
}

fn install_without_a_terminal_in(
    home: &tempfile::TempDir,
    envs: &[(&str, &str)],
) -> (Output, Vec<String>) {
    use std::os::unix::process::CommandExt;
    let tools = tempfile::tempdir().unwrap();
    let binary = fake_autter(tools.path());
    let mut command = Command::new("bash");
    command.arg(install_script());
    // SAFETY: setsid is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut all = vec![
        ("AUTTER_LOCAL_BINARY", binary.to_str().unwrap()),
        ("AUTTER_NO_INSTALL_PING", "1"),
    ];
    all.extend_from_slice(envs);
    let output = run(command, home.path(), &all, None);
    let calls = std::fs::read_to_string(tools.path().join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    (output, calls)
}

#[test]
fn failed_install_token_exchange_says_how_to_sign_in() {
    // The install command from the app carries a one-time token. When the
    // exchange failed the installer set a flag nothing read, and went on
    // signed out without a word.
    let (output, calls) = install_without_a_terminal(&[
        ("INSTALL_NONCE", "expired"),
        ("API_BASE", "http://127.0.0.1:9"),
        ("FAKE_EXCHANGE_FAILS", "1"),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the install itself still succeeds: {stderr}"
    );
    assert!(
        stderr.contains("could not sign you in automatically") && stderr.contains("autter login"),
        "must say sign-in failed and what to run: {stderr}"
    );
    // No terminal: login is not started (it would wait for a person), and
    // onboarding still runs after the exchange.
    assert_eq!(calls, vec!["exchange-nonce", "onboard"]);
}

#[test]
fn install_token_exchange_that_works_or_is_absent_stays_quiet() {
    let (output, calls) = install_without_a_terminal(&[
        ("INSTALL_NONCE", "good"),
        ("API_BASE", "http://127.0.0.1:9"),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("sign you in"), "{stderr}");
    assert_eq!(calls, vec!["exchange-nonce", "onboard"]);

    // No token in the install command: nothing to exchange, nothing to say.
    let (output, calls) = install_without_a_terminal(&[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("sign you in"), "{stderr}");
    assert_eq!(calls, vec!["onboard"]);
}

/// Installing with this script over an npm install hands the binary to the
/// script: the npm package's marker goes, so the background service updates
/// the binary again and `autter upgrade` no longer points at npm.
#[test]
fn installing_over_an_npm_install_removes_the_npm_marker() {
    let home = home_with_zshrc();
    let marker = home.path().join(".autter").join("npm-install.json");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, r#"{"installer":"npm","package_version":"3.7.0"}"#).unwrap();

    let (output, _) = install_without_a_terminal_in(&home, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.path().join(".autter/bin/autter").is_file());
    assert!(!marker.exists(), "the npm marker must be removed");
}
