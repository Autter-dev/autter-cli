use crate::daemon::daemon_log_file_path;
use crate::daemon::{
    ControlRequest, DaemonConfig, local_socket_connects_with_timeout, read_daemon_pid,
    remove_stale_daemon_files, send_control_request,
};
use crate::utils::LockFile;
#[cfg(windows)]
use crate::utils::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(windows)]
use std::{ffi::OsStr, path::Path};

pub fn handle_daemon(args: &[String]) {
    // Help in any position (`bg stop --help`) prints help and never acts.
    if args.is_empty() || is_help(args[0].as_str()) || crate::commands::arg_parser::wants_help(args)
    {
        print_help();
        std::process::exit(0);
    }

    match args[0].as_str() {
        "start" => {
            if let Err(e) = handle_start(&args[1..]) {
                eprintln!(
                    "Failed to start: {}",
                    explain_service_error(cfg!(windows), &e)
                );
                std::process::exit(1);
            }
        }
        "run" => {
            if let Err(e) = handle_run(&args[1..]) {
                eprintln!("Failed to run: {}", e);
                std::process::exit(1);
            }
        }
        "status" => {
            let repo = parse_repo_arg(&args[1..]).unwrap_or_else(default_repo_path);
            if let Err(e) = handle_status(repo) {
                eprintln!("Failed to get status: {}", e);
                std::process::exit(1);
            }
        }
        "shutdown" => {
            if let Err(e) = handle_shutdown(&args[1..]) {
                eprintln!(
                    "Failed to shut down: {}",
                    explain_service_error(cfg!(windows), &e)
                );
                std::process::exit(1);
            }
        }
        "restart" => {
            if let Err(e) = handle_restart(&args[1..]) {
                eprintln!(
                    "Failed to restart: {}",
                    explain_service_error(cfg!(windows), &e)
                );
                std::process::exit(1);
            }
        }
        "tail" => {
            if let Err(e) = handle_tail(&args[1..]) {
                eprintln!("Failed to tail log: {}", e);
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("Unknown subcommand: {}", args[0]);
            print_help();
            std::process::exit(crate::commands::EXIT_USAGE_ERROR);
        }
    }
}

fn handle_start(args: &[String]) -> Result<(), String> {
    if has_flag(args, "--mode") {
        return Err("--mode is no longer supported; daemon always runs in write mode".to_string());
    }
    ensure_daemon_running_attached(daemon_startup_timeout()).map(|_| ())
}

fn daemon_startup_timeout() -> Duration {
    first_readiness_timeout(
        cfg!(windows),
        std::env::var_os("AUTTER_TEST_DB_PATH").is_some() || std::env::var_os("CI").is_some(),
    )
}

/// Spawn a daemon and wait for it to become healthy. Used by explicit CLI
/// commands (`bg start`, `bg restart`) — NOT guarded for test builds.
///
/// On Unix, spawns with piped stderr so startup failures are surfaced to the
/// user. On Windows, spawns fully detached (null stdio) because piped handles
/// cause the parent to hang when the daemon outlives it.
fn ensure_daemon_running_attached(timeout: Duration) -> Result<DaemonConfig, String> {
    let config = daemon_config_from_env_or_default_paths()?;
    if daemon_is_up(&config) {
        return Ok(config);
    }

    remove_stale_daemon_files(&config);

    if daemon_startup_is_blocked(&config) {
        // Takes over a stale lock, or says who holds it.
        resolve_held_lock(&config)?;
    }

    #[cfg(not(windows))]
    {
        let mut child = spawn_daemon_run_with_piped_stderr(&config)?;
        let deadline = Instant::now() + timeout;
        loop {
            if daemon_is_up(&config) {
                return Ok(config);
            }
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => {
                    let mut stderr_buf = String::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        use std::io::Read;
                        let _ = stderr.read_to_string(&mut stderr_buf);
                    }
                    let detail = if stderr_buf.trim().is_empty() {
                        format!("daemon process exited with {}", status)
                    } else {
                        stderr_buf.trim().to_string()
                    };
                    return Err(format!("daemon failed to start: {}", detail));
                }
                Ok(Some(_)) => {
                    return Err("daemon process exited before sockets were ready".to_string());
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return attached_start_timed_out(config, timeout);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    {
        spawn_daemon_run_detached(&config)?;
        if wait_for_daemon_up(&config, timeout) {
            return Ok(config);
        }
        attached_start_timed_out(config, timeout)
    }
}

/// `bg start` / `bg restart` ran out of time waiting for the sockets. A
/// service that holds its lock is running and still getting ready: say so and
/// do not call it a failed start (UNVERIFIED ON WINDOWS, where this was
/// reported). One that does not is a failed start.
fn attached_start_timed_out(
    config: DaemonConfig,
    timeout: Duration,
) -> Result<DaemonConfig, String> {
    let outcome = startup_wait_outcome(daemon_is_up(&config), daemon_startup_is_blocked(&config));
    match outcome {
        StartWait::Ready => Ok(config),
        StartWait::StartedNotReady => {
            eprintln!("{}", startup_wait_message(outcome, timeout, &config));
            Ok(config)
        }
        StartWait::NotStarted => Err(startup_wait_message(outcome, timeout, &config)),
    }
}

fn daemon_config_from_env_or_default_paths() -> Result<DaemonConfig, String> {
    DaemonConfig::from_env_or_default_paths().map_err(|e| e.to_string())
}

fn handle_run(args: &[String]) -> Result<(), String> {
    if has_flag(args, "--mode") {
        return Err("--mode is no longer supported; daemon always runs in write mode".to_string());
    }
    let config = daemon_config_from_env_or_default_paths()?;
    let runtime_dir = daemon_runtime_dir(&config)?;
    std::env::set_current_dir(&runtime_dir).map_err(|e| {
        format!(
            "failed to set daemon runtime cwd to {}: {}",
            runtime_dir.display(),
            e
        )
    })?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let exit_action = runtime
        .block_on(async move { crate::daemon::run_daemon(config).await })
        .map_err(|e| e.to_string())?;

    match exit_action {
        crate::daemon::DaemonExitAction::Stop => {}
        crate::daemon::DaemonExitAction::Restart => {
            ensure_daemon_running(Duration::from_secs(5)).map(|_| ())?;
        }
        crate::daemon::DaemonExitAction::RestartAfterUpdate => {
            // Daemon is fully dead (lock released, sockets removed, threads joined).
            // Now safe to self-update — if the install cannot proceed, bring the
            // daemon back so a failed update does not leave the service down.
            match crate::daemon::daemon_run_pending_self_update() {
                crate::daemon::DaemonSelfUpdateOutcome::Installed => {
                    #[cfg(not(windows))]
                    {
                        ensure_daemon_running(Duration::from_secs(5)).map(|_| ())?;
                    }
                }
                crate::daemon::DaemonSelfUpdateOutcome::NoUpdate
                | crate::daemon::DaemonSelfUpdateOutcome::Failed => {
                    ensure_daemon_running(Duration::from_secs(5)).map(|_| ())?;
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn ensure_daemon_running(timeout: Duration) -> Result<DaemonConfig, String> {
    let config = daemon_config_from_env_or_default_paths()?;
    if daemon_is_up(&config) {
        return Ok(config);
    }

    // In test builds, never auto-spawn a daemon. The test harness manages
    // daemon lifecycle via DaemonProcess::start / shared daemon pool.
    // Without this guard, parallel test threads that see a briefly-unavailable
    // daemon each call spawn_daemon_run_detached, creating a process storm.
    // A daemon whose sockets are on disk but that is only slow to accept on a
    // loaded machine gets until `timeout`, the same grace a production caller
    // gets for a fresh spawn. With no socket files there is no daemon to wait
    // for, so that still fails at once.
    #[cfg(any(test, feature = "test-support"))]
    {
        let sockets_exist =
            config.control_socket_path.exists() && config.trace_socket_path.exists();
        if sockets_exist && wait_for_daemon_up(&config, timeout) {
            Ok(config)
        } else {
            Err("daemon not running (test build: auto-spawn disabled)".to_string())
        }
    }

    #[cfg(not(any(test, feature = "test-support")))]
    {
        if std::env::var("_AUTTER_INTERNAL_DISABLE_WRAPPER_DAEMON_AUTOSPAWN")
            .is_ok_and(|v| v == "1" || v == "true")
        {
            return Err(
                "daemon auto-spawn disabled (_AUTTER_INTERNAL_DISABLE_WRAPPER_DAEMON_AUTOSPAWN)"
                    .to_string(),
            );
        }

        start_daemon_detached_with_config(config, timeout)
    }
}

fn daemon_startup_is_blocked(config: &DaemonConfig) -> bool {
    if let Some(parent) = config.lock_path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }

    match LockFile::try_acquire(&config.lock_path) {
        Some(lock) => {
            drop(lock);
            false
        }
        None => true,
    }
}

/// Whether a process still exists, as far as this user can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProcessState {
    Alive,
    Gone,
    Unknown,
}

#[cfg(unix)]
pub(crate) fn process_state(pid: u32) -> ProcessState {
    if pid == 0 || pid > i32::MAX as u32 {
        return ProcessState::Unknown;
    }
    // kill(pid, 0) checks existence without sending a signal.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return ProcessState::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => ProcessState::Gone,
        // It exists but belongs to someone else (root).
        Some(libc::EPERM) => ProcessState::Alive,
        _ => ProcessState::Unknown,
    }
}

/// UNVERIFIED ON WINDOWS: written on macOS, where it is neither compiled nor
/// run (Windows CI only compiles it). An exited
/// process can still be opened while a handle to it is open, so the exit
/// code decides; a process this user may not open (elevated, another user)
/// exists.
#[cfg(windows)]
pub(crate) fn process_state(pid: u32) -> ProcessState {
    type Handle = *mut std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_INVALID_PARAMETER: u32 = 87;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return match GetLastError() {
                ERROR_INVALID_PARAMETER => ProcessState::Gone,
                ERROR_ACCESS_DENIED => ProcessState::Alive,
                _ => ProcessState::Unknown,
            };
        }
        let mut code: u32 = 0;
        let read = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        match (read, code) {
            (0, _) => ProcessState::Unknown,
            (_, STILL_ACTIVE) => ProcessState::Alive,
            _ => ProcessState::Gone,
        }
    }
}

/// What a service lock that cannot be taken means, when the service does not
/// answer on its sockets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HeldLock {
    /// The process named in the pid file is gone, yet the lock file cannot
    /// be opened. Taken over: the file is removed and the lock taken again.
    Stale,
    /// The holder is alive and Windows denies this user its pipes: it runs
    /// elevated (started from an Administrator window).
    Elevated,
    /// Alive, or not known to be gone: still starting, busy or hung.
    Busy,
}

/// UNVERIFIED ON WINDOWS (best guess from a "lock held" report, one of them
/// after an install from an Administrator window).
///
/// On macOS and Linux the lock is an `flock`, which the kernel releases when
/// its holder dies, so a held lock always has a live holder (or one that is
/// starting and has not written its pid yet) and is never stale. On Windows
/// the lock is the file opened without sharing; a crash closes that handle
/// too, but the file can still be unopenable afterwards (left by an elevated
/// service with permissions a normal window lacks, or held by a scanner),
/// which used to read as "lock held" forever. `pipe_access_denied` is whether
/// connecting to the service's pipe failed with "Access is denied".
pub(crate) fn classify_held_lock(
    windows: bool,
    holder: ProcessState,
    pipe_access_denied: bool,
) -> HeldLock {
    match holder {
        _ if windows && pipe_access_denied => HeldLock::Elevated,
        ProcessState::Gone if windows => HeldLock::Stale,
        _ => HeldLock::Busy,
    }
}

/// The service lock as `autter doctor --report` and the start path see it.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct LockInspection {
    pub path: PathBuf,
    pub exists: bool,
    pub free: bool,
    pub holder_pid: Option<u32>,
    pub holder: Option<ProcessState>,
    /// Why the lock file could not be opened, when it could not.
    pub open_error: Option<String>,
    pub pipe_error: Option<String>,
    pub verdict: Option<HeldLock>,
}

pub(crate) fn inspect_lock(config: &DaemonConfig) -> LockInspection {
    // Taken and released at once (the `Ok` lock is dropped by `err()`).
    let open = LockFile::try_acquire_or_error(&config.lock_path);
    let free = open.is_ok();
    let open_error = open.err().map(|e| e.to_string());
    let holder_pid = read_daemon_pid(config).ok();
    let holder = holder_pid.map(process_state);
    let pipe_error = (!free)
        .then(|| {
            local_socket_connects_with_timeout(
                &config.control_socket_path,
                Duration::from_millis(500),
            )
            .err()
            .map(|e| e.to_string())
        })
        .flatten();
    let pipe_access_denied = pipe_error
        .as_deref()
        .is_some_and(|e| windows_service_failure_hint(e) == Some(WINDOWS_ELEVATED_SERVICE_HINT));
    LockInspection {
        path: config.lock_path.clone(),
        exists: config.lock_path.exists(),
        free,
        holder_pid,
        holder,
        open_error,
        // Held by a service that answers on its pipe is the normal case.
        verdict: (!free && pipe_error.is_some()).then(|| {
            classify_held_lock(
                cfg!(windows),
                holder.unwrap_or(ProcessState::Unknown),
                pipe_access_denied,
            )
        }),
        pipe_error,
    }
}

/// The lock is held and the service does not answer: take over a stale lock
/// (`Ok`), or say what is holding it (`Err`, the message for the user).
fn resolve_held_lock(config: &DaemonConfig) -> Result<(), String> {
    let lock = inspect_lock(config);
    match lock.verdict {
        None => Ok(()),
        Some(HeldLock::Stale) => {
            tracing::warn!(
                "taking over a stale background service lock at {} (pid {:?} is gone)",
                config.lock_path.display(),
                lock.holder_pid
            );
            let _ = std::fs::remove_file(&config.lock_path);
            if daemon_startup_is_blocked(config) {
                Err(format!(
                    "daemon startup blocked: lock held at {} (its previous holder, pid {}, is gone, but the file could not be removed: {})",
                    config.lock_path.display(),
                    lock.holder_pid.map(|p| p.to_string()).unwrap_or_default(),
                    lock.open_error.unwrap_or_default()
                ))
            } else {
                Ok(())
            }
        }
        Some(HeldLock::Elevated) => Err(format!(
            "{WINDOWS_ELEVATED_SERVICE_HINT} [lock held at {}{}]",
            config.lock_path.display(),
            lock.holder_pid
                .map(|pid| format!(" by pid {pid}"))
                .unwrap_or_default()
        )),
        Some(HeldLock::Busy) => Err(format!(
            "daemon startup blocked: lock held at {}{}",
            config.lock_path.display(),
            match (lock.holder_pid, lock.holder) {
                (Some(pid), Some(ProcessState::Alive)) =>
                    format!(" by pid {pid}, which is running but not answering"),
                _ => String::new(),
            }
        )),
    }
}

/// How long a fresh start may take before both sockets accept connections.
///
/// UNVERIFIED ON WINDOWS: Windows 10 users saw the start time out while
/// `autter bg status` called the service running. Creating the named pipes
/// and starting the first git.exe children, each scanned by antivirus, can
/// take several seconds there, so Windows gets 15s (20s in CI and tests)
/// where it had 5s (12s).
pub(crate) fn first_readiness_timeout(windows: bool, test_or_ci: bool) -> Duration {
    match (windows, test_or_ci) {
        (true, true) => Duration::from_secs(20),
        (true, false) => Duration::from_secs(15),
        (false, _) => Duration::from_secs(2),
    }
}

/// Where a start stands when its wait for the sockets runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartWait {
    /// Both sockets accept connections.
    Ready,
    /// The new service holds its lock (so it is running) but its sockets do
    /// not accept connections yet. Not a start failure: it is still starting.
    StartedNotReady,
    /// No service holds the lock: it did not start, or exited.
    NotStarted,
}

pub(crate) fn startup_wait_outcome(sockets_up: bool, lock_held: bool) -> StartWait {
    match (sockets_up, lock_held) {
        (true, _) => StartWait::Ready,
        (false, true) => StartWait::StartedNotReady,
        (false, false) => StartWait::NotStarted,
    }
}

fn startup_wait_message(outcome: StartWait, timeout: Duration, config: &DaemonConfig) -> String {
    match outcome {
        StartWait::Ready => "background service is running".to_string(),
        StartWait::StartedNotReady => format!(
            "the background service started, but its sockets were not ready after {:.0?}; it is still starting. Check with `{STATUS_COMMAND}` in a few seconds",
            timeout
        ),
        StartWait::NotStarted => format!(
            "timed out after {:?} waiting for daemon sockets {} and {}",
            timeout,
            config.control_socket_path.display(),
            config.trace_socket_path.display()
        ),
    }
}

/// The one command every user-facing message suggests for a background
/// service that is stopped, unresponsive, or running an outdated binary.
/// `bg restart` also starts a service that is not running, so it covers every
/// unhealthy state. Use [`RESTART_COMMAND`] in `format!`, or this macro inside
/// `concat!` for `const` strings.
macro_rules! restart_command {
    () => {
        "autter bg restart"
    };
}
pub(crate) use restart_command;

/// Windows refused to start a program because its antivirus flagged it
/// (ERROR_VIRUS_INFECTED 225, ERROR_VIRUS_DELETED 226). Restarting cannot
/// help until the file is allowed.
const WINDOWS_ANTIVIRUS_HINT: &str = "Windows Security (or another antivirus) stopped autter from starting its background service, so restarting will not help. Open Windows Security > Virus & threat protection > Protection history, allow the blocked autter.exe entry (or add an exclusion for %USERPROFILE%\\.autter\\bin), then run `autter bg restart` and `autter doctor`. Each release publishes the SHA-256 of autter.exe if you want to check the file first";

/// The background service runs elevated and this terminal does not, so
/// Windows denies reaching, stopping or replacing it.
const WINDOWS_ELEVATED_SERVICE_HINT: &str = "Windows denied access to the background service. The usual cause: it was started from an Administrator window, so this normal window is not allowed to reach or stop it. Open PowerShell with \"Run as administrator\", run `autter bg shutdown --hard`, close that window, then run `autter bg restart` from a normal window. Install and run autter from a normal window from now on";

/// A cause that the usual "restart the background service" advice cannot
/// fix, read from the text of a Windows error: an antivirus block, or a
/// service running elevated. `None` for anything else.
pub(crate) fn windows_service_failure_hint(error_text: &str) -> Option<&'static str> {
    let text = error_text.to_ascii_lowercase();
    if text.contains("os error 225")
        || text.contains("os error 226")
        || text.contains("contains a virus")
        || text.contains("potentially unwanted software")
    {
        return Some(WINDOWS_ANTIVIRUS_HINT);
    }
    if text.contains("access is denied") || text.contains("(os error 5)") {
        return Some(WINDOWS_ELEVATED_SERVICE_HINT);
    }
    None
}

/// `error` with the Windows cause and fix in front when there is one, the
/// raw error kept in brackets for support. Unchanged off Windows, where
/// those error codes mean something else.
pub(crate) fn explain_service_error(windows: bool, error: &str) -> String {
    match windows
        .then(|| windows_service_failure_hint(error))
        .flatten()
    {
        Some(hint) => format!("{hint} [{error}]"),
        None => error.to_string(),
    }
}

/// Canonical restart suggestion. See [`restart_command!`].
pub const RESTART_COMMAND: &str = restart_command!();

/// Canonical "show me the service state" command.
pub const STATUS_COMMAND: &str = "autter bg status";

/// Socket connect budget for [`probe_daemon_health`]. The readiness loops use
/// [`daemon_is_up`] with a 100ms budget, which is too tight to *judge* health:
/// a busy Windows named pipe can take longer to accept, which made `doctor`
/// call a working service broken.
const HEALTH_PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(1000);

/// How long [`probe_daemon_health`] waits for the reply to its `ping` on the
/// control socket. The service's own health check allows the same.
const HEALTH_PROBE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Health of the background service, as reported by both `autter doctor` and
/// `autter bg status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonHealthState {
    /// Both sockets accept connections and the binary is current.
    Healthy,
    /// Running fine, but started before the installed autter binary was
    /// written, so it runs old code.
    Outdated,
    /// A service holds the lock or has one socket up, but does not accept
    /// connections on both sockets, or accepts them on its control socket
    /// without answering a request.
    Unresponsive,
    /// No service process and no sockets.
    NotRunning,
}

/// Result of [`probe_daemon_health`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DaemonHealth {
    pub state: DaemonHealthState,
    pub control_socket: bool,
    /// The control socket answered a `ping`: requests are served, not just
    /// connections accepted.
    pub control_answers: bool,
    pub trace_socket: bool,
    pub lock_held: bool,
    pub pid: Option<u32>,
    pub binary_outdated: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

impl DaemonHealth {
    pub fn is_healthy(&self) -> bool {
        self.state == DaemonHealthState::Healthy
    }

    /// Lines describing what the probe saw, for `doctor`/`debug` details.
    pub fn details(&self, config: &DaemonConfig) -> Vec<String> {
        vec![
            format!(
                "control socket: {} ({})",
                config.control_socket_path.display(),
                match (self.control_socket, self.control_answers) {
                    (true, true) => "accepting, answers requests",
                    (true, false) => "accepting, but did not answer a request",
                    (false, _) => "not accepting",
                }
            ),
            format!(
                "trace2 socket: {} ({})",
                config.trace_socket_path.display(),
                if self.trace_socket {
                    "accepting"
                } else {
                    "not accepting"
                }
            ),
            format!(
                "lock: {} ({})",
                config.lock_path.display(),
                if self.lock_held { "held" } else { "free" }
            ),
            format!(
                "pid: {}",
                self.pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            ),
        ]
    }
}

/// Pure classification behind [`probe_daemon_health`].
pub(crate) fn classify_daemon_health(
    control_socket: bool,
    control_answers: bool,
    trace_socket: bool,
    lock_held: bool,
    binary_outdated: bool,
) -> DaemonHealthState {
    match (control_socket, trace_socket) {
        // Accepting connections and dropping them is not serving.
        (true, true) if !control_answers => DaemonHealthState::Unresponsive,
        (true, true) if binary_outdated => DaemonHealthState::Outdated,
        (true, true) => DaemonHealthState::Healthy,
        (false, false) if !lock_held => DaemonHealthState::NotRunning,
        _ => DaemonHealthState::Unresponsive,
    }
}

fn daemon_health_summary(state: DaemonHealthState, accepts_but_silent: bool) -> &'static str {
    match state {
        DaemonHealthState::Healthy => "background service is running",
        DaemonHealthState::Outdated => {
            "background service is running an older autter binary than the one installed"
        }
        DaemonHealthState::Unresponsive if accepts_but_silent => {
            "background service accepts connections but does not answer requests"
        }
        DaemonHealthState::Unresponsive => {
            "background service is not accepting connections on both of its sockets"
        }
        DaemonHealthState::NotRunning => "background service is not running",
    }
}

/// Read-only health probe shared by `autter doctor` and `autter bg status`,
/// so the two always reach the same verdict. Never starts, stops or restarts
/// anything.
///
/// On the control socket it sends a `ping` and waits for the reply, so a
/// service that accepts connections but serves none reads as unresponsive.
/// The start-up and shutdown polling loops keep using the connect-only
/// [`daemon_is_up`]: a round trip there could time out on a service that is
/// merely busy and trigger a restart.
pub fn probe_daemon_health(config: &DaemonConfig) -> DaemonHealth {
    probe_daemon_health_within(
        config,
        HEALTH_PROBE_CONNECT_TIMEOUT,
        HEALTH_PROBE_RESPONSE_TIMEOUT,
    )
}

/// [`probe_daemon_health`] with explicit budgets, for `autter doctor`'s
/// slower confirmation probes before it restarts a service.
pub(crate) fn probe_daemon_health_within(
    config: &DaemonConfig,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> DaemonHealth {
    let socket_accepts = |path: &std::path::Path| {
        path.exists() && local_socket_connects_with_timeout(path, connect_timeout).is_ok()
    };
    let control_answers = config.control_socket_path.exists()
        && control_socket_answers(&config.control_socket_path, response_timeout);
    let control_socket = control_answers || socket_accepts(&config.control_socket_path);
    let trace_socket = socket_accepts(&config.trace_socket_path);
    let lock_held = daemon_startup_is_blocked(config);
    let binary_outdated = control_socket
        && trace_socket
        && crate::diagnostics::daemon_binary_is_stale(config).unwrap_or(false);
    let pid = read_daemon_pid(config).ok();
    let state = classify_daemon_health(
        control_socket,
        control_answers,
        trace_socket,
        lock_held,
        binary_outdated,
    );
    DaemonHealth {
        state,
        control_socket,
        control_answers,
        trace_socket,
        lock_held,
        pid,
        binary_outdated,
        summary: daemon_health_summary(state, control_socket && !control_answers).to_string(),
        remediation: (state != DaemonHealthState::Healthy)
            .then(|| format!("run `{RESTART_COMMAND}`")),
    }
}

/// Send a `ping` on the control socket and wait for any reply that is not a
/// refusal. A service too old to know `ping` answers with an error, which
/// proves the same thing: it reads and answers requests.
fn control_socket_answers(control_socket_path: &std::path::Path, timeout: Duration) -> bool {
    crate::daemon::send_control_request_with_timeout(
        control_socket_path,
        &ControlRequest::Ping,
        timeout,
    )
    .is_ok_and(|response| !response.is_refusal())
}

pub(crate) fn daemon_is_up(config: &DaemonConfig) -> bool {
    if !config.control_socket_path.exists() || !config.trace_socket_path.exists() {
        return false;
    }
    local_socket_connects_with_timeout(&config.control_socket_path, Duration::from_millis(100))
        .is_ok()
        && local_socket_connects_with_timeout(&config.trace_socket_path, Duration::from_millis(100))
            .is_ok()
}

fn wait_for_daemon_up(config: &DaemonConfig, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if daemon_is_up(config) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(any(test, feature = "test-support")))]
fn start_daemon_detached_with_config(
    config: DaemonConfig,
    timeout: Duration,
) -> Result<DaemonConfig, String> {
    if daemon_is_up(&config) {
        return Ok(config);
    }

    remove_stale_daemon_files(&config);

    if daemon_startup_is_blocked(&config) {
        // A service holds the lock but didn't answer the quick probe: it is
        // still starting, or busy. Give it the caller's full budget before
        // calling it down (a 100ms probe used to send commits made right after
        // a restart down the no-attribution path).
        if wait_for_daemon_up(&config, timeout) {
            return Ok(config);
        }
        // A stale lock is taken over and a fresh service started below; an
        // elevated holder cannot be restarted from here, so say so.
        let lock = inspect_lock(&config);
        if matches!(
            lock.verdict,
            Some(HeldLock::Stale) | Some(HeldLock::Elevated)
        ) {
            resolve_held_lock(&config)?;
            return spawn_and_wait(config, timeout);
        }
        // Still silent: it is hung. Restart it in the background so this
        // command isn't held up, and so the next one finds a working service.
        let scheduled = schedule_hung_daemon_restart(&config);
        return Err(format!(
            "the background service is running but not responding (lock held at {}){}",
            config.lock_path.display(),
            if scheduled {
                "; restarting it in the background"
            } else {
                ""
            }
        ));
    }

    spawn_and_wait(config, timeout)
}

#[cfg(not(any(test, feature = "test-support")))]
fn spawn_and_wait(config: DaemonConfig, timeout: Duration) -> Result<DaemonConfig, String> {
    spawn_daemon_run_detached(&config)?;
    if wait_for_daemon_up(&config, timeout) {
        return Ok(config);
    }
    // Callers need working sockets, so a service that is still starting is
    // still an error here, but one that says what is going on.
    let outcome = startup_wait_outcome(false, daemon_startup_is_blocked(&config));
    Err(startup_wait_message(outcome, timeout, &config))
}

fn daemon_runtime_dir(config: &DaemonConfig) -> Result<PathBuf, String> {
    config.ensure_parent_dirs().map_err(|e| e.to_string())?;
    config
        .lock_path
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "daemon lock path has no parent".to_string())
}

#[cfg(windows)]
fn powershell_single_quote_literal(value: &OsStr) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "''"))
}

/// Start the background service from a clean environment, whatever shell runs
/// `autter bg restart` or `autter doctor`.
fn sanitize_daemon_child_env(child: &mut Command) {
    // The daemon is repository-agnostic; variables like GIT_DIR override the -C
    // flag and cause repository resolution failures.
    for var in crate::daemon::GIT_ENV_VARS_TO_SANITIZE {
        child.env_remove(var);
    }
    // AUTTER controls debug routing in the binary (AUTTER=git → handle_git). A
    // daemon that inherits this would route "bg run" to the git proxy instead of
    // starting as a daemon.
    child.env_remove("AUTTER");
    // Cloud-agent markers describe the shell, not the machine. Attribution reads
    // them per command (trace2), so the long-lived service must not carry a copy.
    for var in daemon_agent_env_vars_to_remove(std::env::vars().map(|(name, _)| name)) {
        child.env_remove(var);
    }
}

/// Agent-detection variables (except HOSTNAME, which identifies the machine) plus
/// any `CLOUD_AGENT_*` marker present in `names`.
fn daemon_agent_env_vars_to_remove(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut vars: Vec<String> = crate::authorship::background_agent::DETECTION_ENV_VARS
        .iter()
        .filter(|name| **name != "HOSTNAME")
        .map(|name| name.to_string())
        .collect();
    vars.extend(names.filter(|name| name.starts_with("CLOUD_AGENT_")));
    vars
}

#[cfg(any(windows, not(any(test, feature = "test-support"))))]
fn spawn_daemon_run_detached(config: &DaemonConfig) -> Result<(), String> {
    spawn_autter_bg_detached(config, "run")
}

/// How often a hung service may be restarted automatically.
#[cfg(not(any(test, feature = "test-support")))]
const HUNG_DAEMON_RESTART_INTERVAL_SECS: u64 = 300;

/// Start `autter bg restart` detached, at most once per
/// [`HUNG_DAEMON_RESTART_INTERVAL_SECS`]. Returns whether one was started.
#[cfg(not(any(test, feature = "test-support")))]
fn schedule_hung_daemon_restart(config: &DaemonConfig) -> bool {
    if !crate::authorship::stranded_commits::should_warn_now(
        "auto_restart_hung_service",
        HUNG_DAEMON_RESTART_INTERVAL_SECS,
    ) {
        return false;
    }
    tracing::warn!("background service is not responding; restarting it in the background");
    spawn_autter_bg_detached(config, "restart").is_ok()
}

#[cfg(any(windows, not(any(test, feature = "test-support"))))]
fn spawn_autter_bg_detached(config: &DaemonConfig, subcommand: &str) -> Result<(), String> {
    // Use current_autter_exe() instead of current_exe() to resolve through
    // symlinks. When the current exe is the git shim (e.g. ~/.local/bin/git),
    // current_exe() would spawn `git daemon run` which re-enters handle_git()
    // instead of handle_autter(), causing a fork bomb in async mode.
    let exe = crate::utils::current_autter_exe().map_err(|e| e.to_string())?;
    let runtime_dir = daemon_runtime_dir(config)?;

    #[cfg(windows)]
    {
        let script = format!(
            "Start-Process -FilePath {} -ArgumentList @('bg','{}') -WorkingDirectory {} -WindowStyle Hidden",
            powershell_single_quote_literal(exe.as_os_str()),
            subcommand,
            powershell_single_quote_literal(Path::new(&runtime_dir).as_os_str())
        );
        let mut child = Command::new("powershell.exe");
        child
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-WindowStyle")
            .arg("Hidden")
            .arg("-Command")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        sanitize_daemon_child_env(&mut child);

        let preferred_flags =
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB;
        child.creation_flags(preferred_flags);
        match child.spawn() {
            Ok(_) => Ok(()),
            Err(preferred_err) => {
                tracing::debug!(
                    "detached daemon spawn with CREATE_BREAKAWAY_FROM_JOB failed, retrying without it: {}",
                    preferred_err
                );
                child.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
                child.spawn().map(|_| ()).map_err(|fallback_err| {
                    format!(
                        "failed to spawn detached daemon with flags {:#x}: {}; retry without CREATE_BREAKAWAY_FROM_JOB also failed: {}",
                        preferred_flags, preferred_err, fallback_err
                    )
                })
            }
        }
    }

    #[cfg(not(windows))]
    {
        let mut child = Command::new(exe);
        child
            .arg("bg")
            .arg(subcommand)
            .current_dir(&runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        sanitize_daemon_child_env(&mut child);
        child.spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

#[cfg(not(windows))]
fn spawn_daemon_run_with_piped_stderr(
    config: &DaemonConfig,
) -> Result<std::process::Child, String> {
    let exe = crate::utils::current_autter_exe().map_err(|e| e.to_string())?;
    let runtime_dir = daemon_runtime_dir(config)?;
    let mut child = Command::new(exe);
    child
        .arg("bg")
        .arg("run")
        .current_dir(&runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    sanitize_daemon_child_env(&mut child);
    child.spawn().map_err(|e| e.to_string())
}

fn handle_status(repo_working_dir: String) -> Result<(), String> {
    let config = daemon_config_from_env_or_default_paths()?;
    let cloud_sync = crate::auth::notice::collect_cloud_sync_status();
    // The same probe `autter doctor` uses for its "background service" check,
    // so the two commands can no longer disagree about service health.
    let health = probe_daemon_health(&config);
    let daemon_running = health.control_socket || health.trace_socket || health.lock_held;
    // A healthy service records nothing unless git's trace2 config points at
    // it. Report that next to service health so `bg status` can't read as
    // "all good" while doctor fails trace2.
    let git_capture = git_capture_status();

    // Check if the path is inside a git repository before contacting the daemon.
    // When run outside a git repo, still report service health but skip the
    // family-level status query which requires a valid repo.
    let in_repo = crate::git::find_repository_in_path(&repo_working_dir).is_ok();
    if !in_repo || !health.control_socket {
        let response = serde_json::json!({
            "ok": health.is_healthy(),
            "git_repo": in_repo,
            "daemon_running": daemon_running,
            "health": health,
            "git_capture": git_capture,
            "cloud_sync": cloud_sync,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?
        );
        if !health.is_healthy() {
            eprintln!("{}. Fix: run `{RESTART_COMMAND}`.", health.summary);
            std::process::exit(1);
        }
        warn_if_git_capture_off(&git_capture);
        return Ok(());
    }

    let request = ControlRequest::StatusFamily { repo_working_dir };
    let daemon_response =
        send_control_request(&config.control_socket_path, &request).map_err(|e| e.to_string())?;
    let ok = daemon_response.ok && health.is_healthy();
    let response = serde_json::json!({
        "ok": ok,
        "daemon_running": daemon_running,
        "health": health,
        "seq": daemon_response.seq,
        "data": daemon_response.data,
        "error": daemon_response.error,
        "git_capture": git_capture,
        "cloud_sync": cloud_sync,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?
    );
    if !health.is_healthy() {
        eprintln!("{}. Fix: run `{RESTART_COMMAND}`.", health.summary);
        std::process::exit(1);
    }
    warn_if_git_capture_off(&git_capture);
    Ok(())
}

/// Whether git's global trace2 config points at this service, for `bg status`.
/// Reads git config only; the slower live event probe stays in `autter doctor`.
fn git_capture_status() -> serde_json::Value {
    let git_cmd = crate::config::Config::get().git_cmd().to_string();
    let check = crate::diagnostics::check_trace2_global_config(
        &crate::diagnostics::GitDiagnosticTarget::new("configured git", &git_cmd),
    );
    let configured = check.status == crate::diagnostics::DiagnosticStatus::Passed;
    serde_json::json!({
        "trace2_configured": configured,
        "summary": check.summary,
        "fix": (!configured).then_some(check.remediation).flatten(),
    })
}

fn warn_if_git_capture_off(git_capture: &serde_json::Value) {
    if git_capture["trace2_configured"] == serde_json::Value::Bool(false) {
        eprintln!(
            "The background service is running, but git capture (trace2) is not configured, so commits are not recorded. Fix: run `autter install --system`, then `autter doctor`."
        );
    }
}

fn handle_tail(args: &[String]) -> Result<(), String> {
    let config = daemon_config_from_env_or_default_paths()?;
    if !daemon_is_up(&config) {
        return Err("background service is not running".to_string());
    }

    let log_path =
        daemon_log_file_path(&config).map_err(|e| format!("cannot locate log: {}", e))?;
    if !log_path.exists() {
        return Err(format!("log file not found: {}", log_path.display()));
    }

    let full = has_flag(args, "--full");
    let follow = has_flag(args, "--follow") || has_flag(args, "-f");
    let lines: usize = parse_number_arg(args, "-n")
        .or_else(|| parse_number_arg(args, "--lines"))
        .unwrap_or(20);

    let file = std::fs::File::open(&log_path)
        .map_err(|e| format!("cannot open {}: {}", log_path.display(), e))?;

    if full {
        // Print entire file then continue tailing.
        let reader = BufReader::new(&file);
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            println!("{}", line);
        }
    } else {
        // Print last N lines.
        print_last_n_lines(&file, lines).map_err(|e| e.to_string())?;
    }

    if follow {
        tail_file(file).map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

fn parse_number_arg(args: &[String], flag: &str) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            return args[i + 1].parse().ok();
        }
        i += 1;
    }
    None
}

fn print_last_n_lines(file: &std::fs::File, n: usize) -> Result<(), std::io::Error> {
    use std::io::Read;
    let metadata = file.metadata()?;
    let file_size = metadata.len();
    if file_size == 0 {
        return Ok(());
    }

    // Read up to 64KB from the end to find the last N lines.
    let read_size = file_size.min(64 * 1024) as usize;
    let mut buf = vec![0u8; read_size];
    let mut f = file;
    f.seek(SeekFrom::End(-(read_size as i64)))?;
    f.read_exact(&mut buf)?;

    let text = String::from_utf8_lossy(&buf);
    let all_lines: Vec<&str> = text.lines().collect();
    let start = all_lines.len().saturating_sub(n);
    for line in &all_lines[start..] {
        println!("{}", line);
    }

    // Seek to end so tail_file can pick up from here.
    f.seek(SeekFrom::End(0))?;
    Ok(())
}

fn tail_file(file: std::fs::File) -> Result<(), std::io::Error> {
    let mut reader = BufReader::new(file);
    // Seek to end in case print_last_n_lines didn't (full mode).
    reader.seek(SeekFrom::End(0))?;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n > 0 {
            print!("{}", line);
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Timeout for graceful shutdown before a hard kill during restart.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn handle_shutdown(args: &[String]) -> Result<(), String> {
    let config = daemon_config_from_env_or_default_paths()?;
    if has_flag(args, "--hard") {
        if !daemon_is_up(&config) && !daemon_startup_is_blocked(&config) {
            return Err("background service is not running".to_string());
        }
        hard_kill_daemon(&config)
    } else {
        soft_shutdown_daemon(&config)
    }
}

fn handle_restart(args: &[String]) -> Result<(), String> {
    let config = daemon_config_from_env_or_default_paths()?;
    let hard = has_flag(args, "--hard");

    // Only attempt shutdown if daemon appears to be running.
    let was_running = daemon_is_up(&config) || daemon_startup_is_blocked(&config);
    if was_running {
        // Read the PID before shutdown so we can verify the process actually dies.
        let old_pid = read_daemon_pid(&config).ok();

        if hard {
            hard_kill_daemon(&config)?;
        } else {
            // Attempt soft shutdown; escalate to hard kill on timeout.
            let _ = send_control_request(&config.control_socket_path, &ControlRequest::Shutdown);
            if !wait_for_daemon_dead(&config, GRACEFUL_SHUTDOWN_TIMEOUT) {
                eprintln!("graceful shutdown timed out, force-killing daemon");
                hard_kill_daemon(&config)?;
            }
        }

        // Even after lock+sockets are gone, the process may still be alive
        // (e.g. tokio runtime draining blocking tasks). Verify and force-kill.
        if let Some(pid) = old_pid {
            wait_for_process_exit(pid, Duration::from_secs(2));
        }
    }

    // Start a fresh daemon.
    ensure_daemon_running_attached(daemon_startup_timeout()).map(|_| ())
}

fn soft_shutdown_daemon(config: &DaemonConfig) -> Result<(), String> {
    let response = send_control_request(&config.control_socket_path, &ControlRequest::Shutdown)
        .map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&response).map_err(|e| e.to_string())?
    );
    Ok(())
}

#[cfg(unix)]
fn hard_kill_daemon(config: &DaemonConfig) -> Result<(), String> {
    let pid = read_daemon_pid(config).map_err(|e| format!("cannot read daemon pid: {}", e))?;
    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            // Process already dead — not an error.
            return Ok(());
        }
        return Err(format!("kill -9 {} failed: {}", pid, err));
    }
    // Wait briefly for the OS to reap the process and release the lock.
    let _ = wait_for_daemon_dead(config, Duration::from_secs(2));
    Ok(())
}

#[cfg(windows)]
fn hard_kill_daemon(config: &DaemonConfig) -> Result<(), String> {
    let pid = read_daemon_pid(config).map_err(|e| format!("cannot read daemon pid: {}", e))?;
    let output = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("failed to run taskkill: {}", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Process already dead is not an error.
        if !stderr.contains("not found") {
            return Err(format!(
                "taskkill /F /T /PID {} failed: {}",
                pid,
                stderr.trim()
            ));
        }
    }
    let _ = wait_for_daemon_dead(config, Duration::from_secs(2));
    Ok(())
}

fn wait_for_daemon_dead(config: &DaemonConfig, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let sockets_down = !daemon_is_up(config);
        let lock_free = LockFile::try_acquire(&config.lock_path)
            .map(|l| {
                drop(l);
                true
            })
            .unwrap_or(false);
        if sockets_down && lock_free {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for a process to exit, force-killing it if it doesn't die within timeout.
/// This handles the case where the daemon lock/sockets are gone but the process
/// is still alive (e.g. tokio runtime draining blocking tasks).
///
/// Note: relies on PID liveness only. Theoretically susceptible to PID reuse if
/// the process is reaped and the PID recycled within the timeout window, but on
/// macOS/Linux with ~100k PID space this is not a realistic concern.
#[cfg(unix)]
fn wait_for_process_exit(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret != 0 {
            return; // Process is dead
        }
        if Instant::now() >= deadline {
            // Process still alive after timeout — force kill
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn wait_for_process_exit(_pid: u32, _timeout: Duration) {
    // On Windows, hard_kill_daemon uses taskkill /F which is synchronous.
}

/// Whether the background service is running or still starting up: what
/// `stop_daemon` would act on.
pub(crate) fn daemon_is_running_or_starting(config: &DaemonConfig) -> bool {
    daemon_is_up(config) || daemon_startup_is_blocked(config)
}

/// Shut down the running daemon (soft then hard) and wait for it to fully exit.
/// Used by internal callers (install-hooks, upgrade) that need the daemon stopped
/// before proceeding.
pub(crate) fn stop_daemon(config: &DaemonConfig, timeout: Duration) -> Result<(), String> {
    // Nothing to do if daemon isn't running.
    if !daemon_is_up(config) && !daemon_startup_is_blocked(config) {
        return Ok(());
    }

    // Attempt soft shutdown via control socket if reachable.
    if local_socket_connects_with_timeout(&config.control_socket_path, Duration::from_millis(100))
        .is_ok()
    {
        let _ = send_control_request(&config.control_socket_path, &ControlRequest::Shutdown);
    }

    if wait_for_daemon_dead(config, timeout) {
        return Ok(());
    }

    // Soft shutdown didn't work — escalate.
    hard_kill_daemon(config)
}

/// Shut down the running daemon and start a fresh one. Escalates to hard kill
/// if the soft shutdown doesn't complete within GRACEFUL_SHUTDOWN_TIMEOUT.
pub(crate) fn restart_daemon(config: &DaemonConfig) -> Result<(), String> {
    // Test builds never start a daemon (see ensure_daemon_running), so a
    // restart would only stop one: `autter doctor` restarting a slow service
    // took a shared test daemon down for every later test using it.
    #[cfg(any(test, feature = "test-support"))]
    if daemon_is_up(config) || daemon_startup_is_blocked(config) {
        return Err(
            "restart skipped in a test build: the test harness owns the background service"
                .to_string(),
        );
    }

    let was_running = daemon_is_up(config) || daemon_startup_is_blocked(config);
    if was_running {
        stop_daemon(config, GRACEFUL_SHUTDOWN_TIMEOUT)?;
    }
    ensure_daemon_running(Duration::from_secs(5).max(daemon_startup_timeout())).map(|_| ())
}

fn parse_repo_arg(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--repo" && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn default_repo_path() -> String {
    PathBuf::from(".")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

fn is_help(value: &str) -> bool {
    value == "help" || value == "--help" || value == "-h"
}

fn print_help() {
    eprintln!("autter bg - run and control autter background service");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  autter bg start");
    eprintln!("  autter bg run");
    eprintln!("  autter bg status [--repo <path>]");
    eprintln!();
    eprintln!("  `bg status` JSON includes a `cloud_sync` object (queue depth,");
    eprintln!("  upload health). See also `autter sync status`.");
    eprintln!("  autter bg shutdown [--hard]");
    eprintln!("  autter bg restart [--hard]");
    eprintln!("  autter bg tail [-n <lines>] [--full] [-f | --follow]");
}

#[cfg(test)]
mod health_tests {
    use super::*;

    #[test]
    fn windows_antivirus_block_is_named_instead_of_suggesting_a_restart() {
        // What starting the service got on Windows 11 while the binary's
        // SHA-256 matched the release.
        let spawn = "failed to spawn detached daemon with flags 0x1000208: Operation did not complete successfully because the file contains a virus or potentially unwanted software. (os error 225); retry without CREATE_BREAKAWAY_FROM_JOB also failed: Operation did not complete successfully because the file contains a virus or potentially unwanted software. (os error 225)";
        let hint = windows_service_failure_hint(spawn).unwrap();
        assert!(hint.contains("Protection history"), "{hint}");
        // The restart comes after allowing the file, not instead of it.
        assert!(hint.contains("restarting will not help"), "{hint}");
        assert!(
            hint.find("Protection history").unwrap() < hint.find(RESTART_COMMAND).unwrap(),
            "{hint}"
        );
        // The localized message differs; the code does not.
        assert!(windows_service_failure_hint("spawn failed (os error 226)").is_some());

        let shown = explain_service_error(true, spawn);
        assert!(shown.starts_with("Windows Security"), "{shown}");
        assert!(shown.ends_with(&format!("[{spawn}]")), "{shown}");
    }

    #[test]
    fn windows_elevated_service_is_named_instead_of_a_raw_taskkill_error() {
        let taskkill = "taskkill /F /T /PID 4312 failed: ERROR: The process with PID 4312 (child process of PID 912) could not be terminated.\nReason: Access is denied.";
        let hint = windows_service_failure_hint(taskkill).unwrap();
        assert!(hint.contains("Run as administrator"), "{hint}");
        assert!(hint.contains("autter bg shutdown --hard"), "{hint}");
        assert!(hint.contains("normal window"), "{hint}");
        assert_eq!(
            windows_service_failure_hint("failed to connect: Access is denied. (os error 5)"),
            Some(hint)
        );
        assert!(explain_service_error(true, taskkill).contains("Reason: Access is denied."));
    }

    #[test]
    fn other_errors_and_other_platforms_are_left_alone() {
        for error in [
            "timed out after 5s waiting for daemon sockets a and b",
            "daemon startup blocked: lock held at C:\\Users\\dev\\.autter\\internal\\daemon.lock",
            "The system cannot find the file specified. (os error 2)",
            // 225 and 5 inside other numbers are not those errors.
            "exit status 2250",
            "(os error 50)",
            "",
        ] {
            assert_eq!(windows_service_failure_hint(error), None, "{error}");
            assert_eq!(explain_service_error(true, error), error);
        }
        // On Unix, error 5 is EIO and "access is denied" is not an elevation
        // problem: the text is passed through.
        let unix = "kill -9 77 failed: Input/output error (os error 5)";
        assert_eq!(explain_service_error(false, unix), unix);
    }

    #[test]
    fn daemon_spawn_drops_agent_markers_but_keeps_hostname() {
        let vars = daemon_agent_env_vars_to_remove(
            ["PATH", "CLOUD_AGENT_SESSION", "HOME"]
                .into_iter()
                .map(str::to_string),
        );
        assert!(vars.iter().any(|v| v == "AUTTER_CLOUD_AGENT"));
        assert!(
            vars.iter()
                .any(|v| v == "CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
        );
        assert!(vars.iter().any(|v| v == "CLOUD_AGENT_SESSION"));
        assert!(
            !vars
                .iter()
                .any(|v| v == "HOSTNAME" || v == "PATH" || v == "HOME")
        );
    }

    #[test]
    fn classify_daemon_health_covers_every_state() {
        use DaemonHealthState::*;
        assert_eq!(
            classify_daemon_health(true, true, true, true, false),
            Healthy
        );
        assert_eq!(
            classify_daemon_health(true, true, true, true, true),
            Outdated
        );
        assert_eq!(
            classify_daemon_health(false, false, false, false, false),
            NotRunning
        );
        // Lock held but no socket accepts: a hung or half-started service.
        assert_eq!(
            classify_daemon_health(false, false, false, true, false),
            Unresponsive
        );
        // Only one socket accepting (e.g. the trace2 socket is wedged): the
        // old `bg status` reported this as healthy while doctor failed it.
        assert_eq!(
            classify_daemon_health(true, true, false, true, false),
            Unresponsive
        );
        assert_eq!(
            classify_daemon_health(false, false, true, true, false),
            Unresponsive
        );
        // Both sockets accept, but the control socket drops every request:
        // a connect-only probe called this healthy.
        assert_eq!(
            classify_daemon_health(true, false, true, true, false),
            Unresponsive
        );
        assert_eq!(
            classify_daemon_health(true, false, true, true, true),
            Unresponsive
        );
    }

    // Items below are best guesses for Windows reports that cannot be
    // reproduced here; the tests pin the decisions, not Windows behaviour.

    #[test]
    fn a_held_lock_is_stale_only_on_windows_with_its_holder_gone() {
        use ProcessState::*;
        assert_eq!(classify_held_lock(true, Gone, false), HeldLock::Stale);
        // flock is released by the kernel when its holder dies: a held lock
        // with no pid holder is a service that is starting.
        assert_eq!(classify_held_lock(false, Gone, false), HeldLock::Busy);
        // A live holder, or one we cannot see, is never taken over.
        assert_eq!(classify_held_lock(true, Alive, false), HeldLock::Busy);
        assert_eq!(classify_held_lock(true, Unknown, false), HeldLock::Busy);
        // Its pipe refuses this user: an elevated service, with the fix.
        assert_eq!(classify_held_lock(true, Alive, true), HeldLock::Elevated);
        assert_eq!(classify_held_lock(true, Unknown, true), HeldLock::Elevated);
        assert_eq!(classify_held_lock(false, Alive, true), HeldLock::Busy);
    }

    #[test]
    fn windows_waits_longer_for_a_fresh_start() {
        assert_eq!(
            first_readiness_timeout(false, false),
            Duration::from_secs(2)
        );
        assert_eq!(first_readiness_timeout(false, true), Duration::from_secs(2));
        assert!(first_readiness_timeout(true, false) >= Duration::from_secs(15));
        assert!(first_readiness_timeout(true, true) >= first_readiness_timeout(true, false));
    }

    #[test]
    fn a_started_service_that_is_not_ready_is_not_a_failed_start() {
        assert_eq!(startup_wait_outcome(true, true), StartWait::Ready);
        assert_eq!(startup_wait_outcome(true, false), StartWait::Ready);
        assert_eq!(
            startup_wait_outcome(false, true),
            StartWait::StartedNotReady
        );
        assert_eq!(startup_wait_outcome(false, false), StartWait::NotStarted);

        let config = DaemonConfig::from_home(std::path::Path::new("/home/dev"));
        let starting =
            startup_wait_message(StartWait::StartedNotReady, Duration::from_secs(15), &config);
        assert!(starting.contains("still starting"), "{starting}");
        assert!(!starting.contains("timed out"), "{starting}");
        let failed = startup_wait_message(StartWait::NotStarted, Duration::from_secs(15), &config);
        assert!(failed.starts_with("timed out after 15s"), "{failed}");
    }

    #[cfg(unix)]
    #[test]
    fn process_state_tells_a_live_process_from_a_gone_one() {
        assert_eq!(process_state(std::process::id()), ProcessState::Alive);
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(process_state(pid), ProcessState::Gone);
        assert_eq!(process_state(0), ProcessState::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn inspect_lock_reports_a_free_lock_and_a_held_one() {
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            internal_dir: dir.path().join("internal"),
            lock_path: dir.path().join("daemon.lock"),
            trace_socket_path: dir.path().join("t.sock"),
            control_socket_path: dir.path().join("c.sock"),
        };
        let free = inspect_lock(&config);
        assert!(free.free && free.verdict.is_none(), "{free:?}");

        let held = LockFile::try_acquire(&config.lock_path).unwrap();
        let busy = inspect_lock(&config);
        assert!(!busy.free, "{busy:?}");
        assert!(busy.open_error.is_some(), "{busy:?}");
        // Nothing answers on the pipe and this is not Windows: busy, never
        // taken over.
        assert_eq!(busy.verdict, Some(HeldLock::Busy));
        let message = resolve_held_lock(&config).unwrap_err();
        assert!(message.contains("lock held at"), "{message}");
        assert!(LockFile::try_acquire(&config.lock_path).is_none());
        drop(held);
    }

    /// A stand-in service: both sockets listen; the control socket either
    /// answers each request with `reply` or, with `None`, drops it unread.
    #[cfg(unix)]
    fn fake_service(reply: Option<&'static str>) -> (tempfile::TempDir, DaemonConfig) {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            internal_dir: dir.path().join("internal"),
            lock_path: dir.path().join("daemon.lock"),
            trace_socket_path: dir.path().join("t.sock"),
            control_socket_path: dir.path().join("c.sock"),
        };
        let control = UnixListener::bind(&config.control_socket_path).unwrap();
        let trace = UnixListener::bind(&config.trace_socket_path).unwrap();
        std::thread::spawn(move || {
            for stream in control.incoming().flatten() {
                if let Some(reply) = reply {
                    let mut reader = std::io::BufReader::new(&stream);
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    let _ = (&stream).write_all(reply.as_bytes());
                }
            }
        });
        std::thread::spawn(move || for _ in trace.incoming() {});
        (dir, config)
    }

    #[cfg(unix)]
    #[test]
    fn health_probe_needs_a_reply_on_the_control_socket() {
        let (_dir, config) = fake_service(None);
        let health = probe_daemon_health_within(
            &config,
            Duration::from_millis(500),
            Duration::from_millis(500),
        );
        assert!(health.control_socket && health.trace_socket, "{health:?}");
        assert!(!health.control_answers, "{health:?}");
        assert_eq!(health.state, DaemonHealthState::Unresponsive);
        assert!(
            health.summary.contains("does not answer requests"),
            "{health:?}"
        );
        assert!(
            health.details(&config)[0].contains("did not answer a request"),
            "{:?}",
            health.details(&config)
        );

        let (_dir, config) = fake_service(Some("{\"ok\":true}\n"));
        let health = probe_daemon_health(&config);
        assert_eq!(health.state, DaemonHealthState::Healthy, "{health:?}");
        assert_eq!(health.summary, "background service is running");

        // An older service that does not know `ping` answers with an error:
        // that still proves it serves requests.
        let (_dir, config) = fake_service(Some(
            "{\"ok\":false,\"error\":\"unknown variant `ping`\"}\n",
        ));
        assert_eq!(
            probe_daemon_health(&config).state,
            DaemonHealthState::Healthy
        );

        // A refusal (the service is turning requests away) is not serving.
        let (_dir, config) = fake_service(Some(
            "{\"ok\":false,\"error\":\"busy\",\"data\":{\"refused\":true}}\n",
        ));
        assert_eq!(
            probe_daemon_health(&config).state,
            DaemonHealthState::Unresponsive
        );
    }

    /// Every user-facing restart suggestion must use the canonical command.
    /// `bg`/`daemon`/`d` are aliases, but mixing them (and `bg start` for a
    /// service that may be wedged) confused users comparing doctor output
    /// with `bg status`.
    #[test]
    fn user_facing_text_uses_only_the_canonical_restart_command() {
        fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path
                    .extension()
                    .is_some_and(|e| e == "rs" || e == "snap" || e == "md")
                {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        collect(&root.join("src"), &mut files);
        files.push(root.join("README.md"));
        files.push(root.join("INSTALL.md"));

        let forbidden = [
            concat!("`autter daemon ", "restart`"),
            concat!("`autter daemon ", "status`"),
            concat!("`autter bg ", "start`"),
            concat!("autter daemon ", "restart "),
            concat!("autter bg ", "start\\x1b"),
        ];
        let mut offenders = Vec::new();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for needle in forbidden {
                if text.contains(needle) {
                    offenders.push(format!("{}: {needle}", file.display()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use `{RESTART_COMMAND}` for restart suggestions: {offenders:#?}"
        );
    }

    #[test]
    fn canonical_restart_command_is_bg_restart() {
        assert_eq!(RESTART_COMMAND, "autter bg restart");
        assert_eq!(
            concat!("run `", restart_command!(), "`"),
            "run `autter bg restart`"
        );
    }
}
