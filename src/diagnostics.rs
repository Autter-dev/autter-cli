use crate::authorship::working_log::CheckpointKind;
use crate::commands::blame::{AutterBlameOptions, BlameAnalysisResult};
use crate::config::Config;
use crate::daemon::control_api::{ControlRequest, FamilyStatus};
use crate::diagnostic_sentinels::{
    DEBUG_SELF_CHECK_REMOTE_URL, debug_self_check_root, path_is_in_debug_self_check_root,
};
use crate::git::repository::discover_repository_in_path_no_git_exec;
use crate::process_timeout::run_command_with_timeout;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SELF_CHECK_FILE: &str = "autter-debug-self-check.txt";
const SELF_CHECK_CONTENT_UNTRACKED: &str = "Untracked line\n";
const SELF_CHECK_CONTENT_KNOWN_HUMAN: &str = "Untracked line\nKnown human line\n";
const SELF_CHECK_CONTENT_AI: &str = "Untracked line\nKnown human line\nAI line\n";
const TRACE2_EVENT_TARGET_KEY: &str = "trace2.eventTarget";
const TRACE2_EVENT_NESTING_KEY: &str = "trace2.eventNesting";
const TRACE2_EVENT_NESTING_VALUE: &str = "10";
const TRACE2_ENV_VARS_KEY: &str = "trace2.envVars";
const SELF_CHECK_TRACE_ENV_REMOVE: &[&str] = &[
    "GIT_TRACE2_PARENT_SID",
    "GIT_TRACE2_PARENT_NAME",
    "AUTTER_WRAPPER_INVOCATION_ID",
    "GIT_TRACE2_ENV_VARS",
];
const DEBUG_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// End-to-end attribution self-check budget. Windows process spawn is slower,
/// so give it a longer wall clock — the old shared 3s budget often expired
/// before waits started ("timed out after 0.0s").
const ATTRIBUTION_SELF_CHECK_TIMEOUT: Duration = if cfg!(windows) {
    Duration::from_secs(20)
} else {
    Duration::from_secs(12)
};
const DAEMON_CONTROL_TIMEOUT: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `git --version` slower than this means the machine runs git slowly (a
/// loaded laptop, antivirus scanning every process start, a network home
/// directory), and fixed check budgets start failing on a healthy setup.
const TIMEOUT_SCALE_BASELINE: Duration = Duration::from_millis(100);
const TIMEOUT_SCALE_MAX: f64 = 10.0;
/// Overrides the measured scale, e.g. `AUTTER_DOCTOR_TIMEOUT_SCALE=4`.
const TIMEOUT_SCALE_ENV: &str = "AUTTER_DOCTOR_TIMEOUT_SCALE";

/// A check's time budget on this machine: `base` on a machine that runs git
/// at normal speed, proportionally longer (up to 10x) on a slower one. The
/// budgets were fixed at 3s and 12s, so `autter doctor` reported timeouts on
/// machines where a plain `git commit` takes a few seconds.
pub fn check_timeout(base: Duration) -> Duration {
    scale_timeout(base, machine_timeout_scale())
}

/// How much longer than their base budget checks may take here. Measured once
/// per process from one `git --version`, unless the environment sets it.
pub fn machine_timeout_scale() -> f64 {
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        if let Some(scale) =
            timeout_scale_from_env(std::env::var(TIMEOUT_SCALE_ENV).ok().as_deref())
        {
            return scale;
        }
        let started = Instant::now();
        let ran = std::process::Command::new(Config::get().git_cmd())
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        if ran {
            timeout_scale_for(started.elapsed())
        } else {
            1.0
        }
    })
}

fn timeout_scale_from_env(value: Option<&str>) -> Option<f64> {
    let scale: f64 = value?.trim().parse().ok()?;
    (scale.is_finite() && scale >= 1.0).then(|| scale.min(TIMEOUT_SCALE_MAX))
}

fn timeout_scale_for(git_version_elapsed: Duration) -> f64 {
    (git_version_elapsed.as_secs_f64() / TIMEOUT_SCALE_BASELINE.as_secs_f64())
        .clamp(1.0, TIMEOUT_SCALE_MAX)
}

fn scale_timeout(base: Duration, scale: f64) -> Duration {
    base.mul_f64(scale.clamp(1.0, TIMEOUT_SCALE_MAX))
}

// Remediation strings surfaced as `fix:` lines in the doctor and debug
// reports. Keep them to concrete next commands a stuck user can run without
// support.
const REMEDIATION_DAEMON_PATHS: &str = "check that your home directory is set and writable (HOME on macOS/Linux, USERPROFILE on Windows), then re-run `autter doctor`";
const REMEDIATION_DAEMON_FAILED: &str = concat!(
    "run `",
    crate::commands::daemon::restart_command!(),
    "`; if it keeps failing, re-run `autter install --system` and then `autter doctor`"
);
const REMEDIATION_TRACE2_INGESTION: &str = concat!(
    "if a trace2 config check below failed, run `autter install --system`; otherwise run `",
    crate::commands::daemon::restart_command!(),
    "`, then `autter doctor`"
);
const REMEDIATION_TRACE2_CONFIG_INSPECT: &str = "check that git runs at all (`git --version`) and that your global git config is readable, then re-run `autter doctor`";
// Not `autter onboard`: once onboarding is complete it only prints a status
// summary, and without a terminal it skips integrations, so it never writes
// trace2 for someone who already ran it.
const REMEDIATION_TRACE2_CONFIG_MISSING: &str = "run `autter install --system` to write the required trace2 settings to your global git config, then re-run `autter doctor`";
const REMEDIATION_TRACE2_FILE: &str = "check that your global git config is writable and that no GIT_TRACE2* environment variables are set, then re-run `autter doctor`; if the trace2 config check also failed, run `autter install --system`";

/// Fallback when the attribution self-check fails for an unrecognized reason.
const REMEDIATION_ATTRIBUTION_FALLBACK: &str = "likely cause: the checkpoint → daemon → commit → notes pipeline is broken end-to-end. next: run `autter bg restart`, then `autter doctor`; if any trace2 check also failed, run `autter install --system` first";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticStatus {
    Passed,
    Failed,
    Skipped,
}

impl DiagnosticStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DiagnosticStatus::Passed => "passed",
            DiagnosticStatus::Failed => "failed",
            DiagnosticStatus::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandRecord {
    pub command: String,
    pub cwd: Option<String>,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl CommandRecord {
    fn success(&self) -> bool {
        !self.timed_out && self.status == Some(0)
    }
}

#[derive(Debug, Clone)]
pub struct DiagnosticCheckResult {
    pub status: DiagnosticStatus,
    pub summary: String,
    pub details: Vec<String>,
    pub commands: Vec<CommandRecord>,
    pub trace2_json: Option<String>,
    /// Plain-language fix for a failed check: the exact command(s) to run or
    /// setting to change so the user can resolve it without support.
    pub remediation: Option<String>,
}

impl DiagnosticCheckResult {
    fn passed(
        summary: impl Into<String>,
        details: Vec<String>,
        commands: Vec<CommandRecord>,
    ) -> Self {
        Self {
            status: DiagnosticStatus::Passed,
            summary: summary.into(),
            details,
            commands,
            trace2_json: None,
            remediation: None,
        }
    }

    pub(crate) fn failed(
        summary: impl Into<String>,
        details: Vec<String>,
        commands: Vec<CommandRecord>,
    ) -> Self {
        Self {
            status: DiagnosticStatus::Failed,
            summary: summary.into(),
            details,
            commands,
            trace2_json: None,
            remediation: None,
        }
    }

    pub(crate) fn skipped(summary: impl Into<String>, details: Vec<String>) -> Self {
        Self {
            status: DiagnosticStatus::Skipped,
            summary: summary.into(),
            details,
            commands: Vec::new(),
            trace2_json: None,
            remediation: None,
        }
    }

    fn with_trace2_json(mut self, trace2_json: Option<String>) -> Self {
        self.trace2_json = trace2_json;
        self
    }

    pub(crate) fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct GitDiagnosticTarget {
    pub label: String,
    pub program: String,
}

impl GitDiagnosticTarget {
    pub fn new(label: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            program: program.into(),
        }
    }
}

/// Prefix of the detail line that says doctor started or restarted the
/// service. `autter doctor` reads it to tell "fixed by doctor" from "healthy".
pub const DOCTOR_FIX_DETAIL_PREFIX: &str = "doctor fix: ";

/// Before restarting a service that did not answer, doctor keeps probing it
/// for this long (scaled to the machine, up to [`SERVICE_CONFIRM_WINDOW_MAX`]).
/// A restart drops the work in flight, and on a loaded machine one failed
/// probe usually means slow, not stuck.
const SERVICE_CONFIRM_WINDOW: Duration = if cfg!(windows) {
    Duration::from_secs(10)
} else {
    Duration::from_secs(5)
};
const SERVICE_CONFIRM_WINDOW_MAX: Duration = Duration::from_secs(30);
/// Connect and reply budget of each confirmation probe: longer than the
/// first probe's, scaled to the machine, at most [`SERVICE_CONFIRM_PROBE_MAX`].
const SERVICE_CONFIRM_PROBE: Duration = if cfg!(windows) {
    Duration::from_secs(4)
} else {
    Duration::from_secs(2)
};
const SERVICE_CONFIRM_PROBE_MAX: Duration = Duration::from_secs(10);
const SERVICE_CONFIRM_INTERVAL: Duration = Duration::from_millis(500);
/// Unresponsive probes in a row, the first one included, before a restart.
const SERVICE_CONFIRM_MIN_PROBES: usize = 3;

/// What doctor does about the background service, given what its probes saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceVerdict {
    /// Healthy now (possibly after being slow): leave it alone.
    Healthy,
    /// Unresponsive, but not for long enough yet: probe again.
    KeepProbing,
    /// Not running: start it (nothing in flight to lose).
    Start,
    /// Runs an outdated binary, or stayed unresponsive for the whole window.
    Restart,
}

/// Decide from the probe results so far (oldest first, the first probe
/// included) and the time since the first probe. Only the latest result and
/// how long the service has been unresponsive matter: a service that answers
/// once is left alone, one that never answers for `window` (and at least
/// [`SERVICE_CONFIRM_MIN_PROBES`] probes) is restarted.
pub(crate) fn service_verdict(
    probes: &[crate::commands::daemon::DaemonHealthState],
    waited: Duration,
    window: Duration,
) -> ServiceVerdict {
    use crate::commands::daemon::DaemonHealthState as State;
    match probes.last() {
        None => ServiceVerdict::KeepProbing,
        Some(State::Healthy) => ServiceVerdict::Healthy,
        Some(State::Outdated) => ServiceVerdict::Restart,
        Some(State::NotRunning) => ServiceVerdict::Start,
        Some(State::Unresponsive) => {
            let unresponsive = probes
                .iter()
                .rev()
                .take_while(|state| **state == State::Unresponsive)
                .count();
            if unresponsive >= SERVICE_CONFIRM_MIN_PROBES && waited >= window {
                ServiceVerdict::Restart
            } else {
                ServiceVerdict::KeepProbing
            }
        }
    }
}

/// The outcome of probing an unhealthy-looking service until
/// [`service_verdict`] decides.
struct ServiceConfirmation {
    health: crate::commands::daemon::DaemonHealth,
    initial: crate::commands::daemon::DaemonHealthState,
    verdict: ServiceVerdict,
    waited: Duration,
    probes: usize,
    probe_timeout: Duration,
}

impl ServiceConfirmation {
    /// What doctor saw and why it acts (or not), for the check's details.
    fn explanation(&self) -> Option<String> {
        use crate::commands::daemon::DaemonHealthState as State;
        let waited = self.waited.as_secs_f64();
        match (self.initial, self.verdict) {
            (State::Unresponsive, ServiceVerdict::Healthy) => Some(format!(
                "the service did not answer the first probe but answered after doctor waited {waited:.1}s ({} probes): it is slow, not stuck, so doctor did not restart it",
                self.probes
            )),
            (State::Unresponsive, ServiceVerdict::Restart) => Some(format!(
                "the service did not answer for {waited:.1}s ({} probes of up to {:.1}s each), so doctor restarted it",
                self.probes,
                self.probe_timeout.as_secs_f64()
            )),
            (_, ServiceVerdict::Restart) => Some(
                "doctor restarted the service because it runs an older autter binary than the one installed"
                    .to_string(),
            ),
            (State::Unresponsive, ServiceVerdict::Start) => Some(format!(
                "the service stopped while doctor waited {waited:.1}s for it to answer, so doctor started it"
            )),
            _ => None,
        }
    }
}

fn service_confirm_budgets() -> (Duration, Duration) {
    let scale = machine_timeout_scale();
    (
        scale_timeout(SERVICE_CONFIRM_WINDOW, scale).min(SERVICE_CONFIRM_WINDOW_MAX),
        scale_timeout(SERVICE_CONFIRM_PROBE, scale).min(SERVICE_CONFIRM_PROBE_MAX),
    )
}

/// Probe the service again, with a longer budget, until [`service_verdict`]
/// decides. Returns at once for a healthy, stopped or outdated service.
fn confirm_service_state(
    config: &crate::daemon::DaemonConfig,
    initial: crate::commands::daemon::DaemonHealth,
) -> ServiceConfirmation {
    let (window, probe_timeout) = service_confirm_budgets();
    let started = Instant::now();
    let initial_state = initial.state;
    let mut states = vec![initial.state];
    let mut health = initial;
    loop {
        let verdict = service_verdict(&states, started.elapsed(), window);
        if verdict != ServiceVerdict::KeepProbing {
            let confirmation = ServiceConfirmation {
                health,
                initial: initial_state,
                verdict,
                waited: started.elapsed(),
                probes: states.len(),
                probe_timeout,
            };
            if states.len() > 1 {
                record_timing(format!(
                    "service confirmation: {:?} -> {:?} after {:.1}s, {} probes",
                    initial_state,
                    verdict,
                    confirmation.waited.as_secs_f64(),
                    confirmation.probes
                ));
            }
            return confirmation;
        }
        std::thread::sleep(SERVICE_CONFIRM_INTERVAL);
        health = crate::commands::daemon::probe_daemon_health_within(
            config,
            probe_timeout,
            probe_timeout,
        );
        states.push(health.state);
    }
}

/// Timings measured during this run (service start, probes), for
/// `autter doctor --report`.
static TIMINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn record_timing(line: String) {
    if let Ok(mut timings) = TIMINGS.lock() {
        timings.push(line);
    }
}

pub(crate) fn recorded_timings() -> Vec<String> {
    TIMINGS.lock().map(|t| t.clone()).unwrap_or_default()
}

/// Doctor/debug readiness for the background service.
///
/// The verdict comes from [`crate::commands::daemon::probe_daemon_health`],
/// the same probe `autter bg status` reports, so the two commands agree. Like
/// before, this also fixes what it can: a stopped service is started and an
/// outdated one restarted. An unresponsive one is probed again with a longer
/// budget first and only restarted when it stays unresponsive (see
/// [`service_verdict`]); the result is re-probed.
pub fn ensure_daemon_ready_for_self_checks() -> DiagnosticCheckResult {
    use crate::commands::daemon::{DaemonHealthState, probe_daemon_health};

    let commands = Vec::new();
    let config = match crate::daemon::DaemonConfig::from_env_or_default_paths() {
        Ok(config) => config,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "daemon readiness could not be inspected",
                vec![format!("failed to determine daemon paths: {}", err)],
                commands,
            )
            .with_remediation(REMEDIATION_DAEMON_PATHS);
        }
    };

    let probed_at = Instant::now();
    let initial = probe_daemon_health(&config);
    record_timing(format!(
        "service health probe: {:?} in {:.2}s",
        initial.state,
        probed_at.elapsed().as_secs_f64()
    ));
    let mut details = initial.details(&config);
    details.push(format!("initial state: {}", initial.summary));

    // Before onboarding, a stopped service is expected: starting it is a
    // system integration that needs consent, so doctor must not do it.
    if initial.state == DaemonHealthState::NotRunning && !crate::config::collection_consented_now()
    {
        details.push("not started: run `autter onboard` to set up this machine".to_string());
        return DiagnosticCheckResult::skipped(
            "not running; starts after setup (`autter onboard`)",
            details,
        );
    }

    let confirmation = confirm_service_state(&config, initial);
    details.extend(confirmation.explanation());
    let fix = match confirmation.verdict {
        ServiceVerdict::Healthy | ServiceVerdict::KeepProbing => None,
        ServiceVerdict::Start => Some(("starting the background service", {
            let started = Instant::now();
            let result =
                crate::commands::daemon::ensure_daemon_running(service_start_timeout()).map(|_| ());
            record_timing(format!(
                "service start: {} after {:.1}s",
                if result.is_ok() { "ready" } else { "failed" },
                started.elapsed().as_secs_f64()
            ));
            result
        })),
        ServiceVerdict::Restart => Some(("restarting the background service", {
            let started = Instant::now();
            let result = crate::commands::daemon::restart_daemon(&config);
            record_timing(format!(
                "service restart: {} after {:.1}s",
                if result.is_ok() { "ready" } else { "failed" },
                started.elapsed().as_secs_f64()
            ));
            result
        })),
    };
    let fixed = fix.is_some();
    if let Some((action, result)) = fix {
        details.push(format!("{DOCTOR_FIX_DETAIL_PREFIX}{action}"));
        if let Err(err) = result {
            details.push(format!("fix failed: {err}"));
        }
    }

    let health = if fixed {
        let after = probe_daemon_health(&config);
        details.push(format!("state after fix: {}", after.summary));
        after
    } else {
        confirmation.health
    };

    if health.is_healthy() {
        let summary = if confirmation.initial == DaemonHealthState::Unresponsive && !fixed {
            format!(
                "{} (slow: it answered after {:.1}s, so doctor did not restart it)",
                health.summary,
                confirmation.waited.as_secs_f64()
            )
        } else {
            health.summary.clone()
        };
        DiagnosticCheckResult::passed(summary, details, commands)
    } else {
        let remediation = daemon_failure_remediation(cfg!(windows), &details);
        DiagnosticCheckResult::failed(&health.summary, details, commands)
            .with_remediation(remediation)
    }
}

/// How long doctor lets a service it starts take to get ready: the machine's
/// check budget, but never less than a fresh start needs on this platform
/// (much longer on Windows).
fn service_start_timeout() -> Duration {
    check_timeout(DEBUG_CHECK_TIMEOUT).max(crate::commands::daemon::first_readiness_timeout(
        cfg!(windows),
        std::env::var_os("CI").is_some(),
    ))
}

/// What to do about a background service that stayed unhealthy after
/// doctor's own start or restart. On Windows two causes show up in the
/// errors those attempts leave in `details`, and for both a restart is the
/// wrong advice: an antivirus block and a service running as Administrator.
fn daemon_failure_remediation(windows: bool, details: &[String]) -> String {
    windows
        .then(|| {
            details
                .iter()
                .find_map(|line| crate::commands::daemon::windows_service_failure_hint(line))
        })
        .flatten()
        .map(|hint| format!("{hint}; then re-run `autter doctor`"))
        .unwrap_or_else(|| REMEDIATION_DAEMON_FAILED.to_string())
}

/// A service that started less than this long ago is still on its first
/// readiness: its first trace event can lag (UNVERIFIED ON WINDOWS).
const FIRST_READINESS_WINDOW: Duration = Duration::from_secs(120);
/// No single ingestion probe waits longer than this, however slow the machine.
const INGESTION_BUDGET_MAX: Duration = Duration::from_secs(60);

/// How long one trace2 ingestion probe may take: `git init` plus the service
/// recording its event.
///
/// UNVERIFIED ON WINDOWS: users there saw this probe time out at the old
/// fixed 3s. Starting git.exe (and autter.exe as its trace target's peer)
/// with antivirus scanning each start can take seconds, and a service that
/// has just started can take longer still for its first event. So Windows
/// gets 10s, 30s right after a start, on top of the machine-speed scaling.
pub(crate) fn trace2_ingestion_budget(
    windows: bool,
    first_readiness: bool,
    scale: f64,
) -> Duration {
    let base = match (windows, first_readiness) {
        (true, true) => Duration::from_secs(30),
        (true, false) => Duration::from_secs(10),
        (false, _) => DEBUG_CHECK_TIMEOUT,
    };
    scale_timeout(base, scale).min(INGESTION_BUDGET_MAX)
}

/// The second, longer try after an ingestion probe missed its budget.
fn trace2_ingestion_retry_budget(first: Duration) -> Duration {
    first.mul_f64(1.5).min(INGESTION_BUDGET_MAX)
}

/// What one failed trace2 ingestion probe measured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IngestionProbeFailure {
    pub error: String,
    /// How long `git init` ran; `None` when it never started.
    pub git_elapsed: Option<Duration>,
    /// `git init` ran out of time.
    pub git_timed_out: bool,
    /// `git init` ran but failed.
    pub git_failed: bool,
    /// How long doctor then waited for the service to record the event.
    pub waited_for_service: Duration,
    /// Events the service had recorded for the probe repository, when its
    /// status request answered.
    pub latest_seq: Option<u64>,
}

impl IngestionProbeFailure {
    fn measured(&self) -> String {
        let git = match self.git_elapsed {
            None => "git init did not start".to_string(),
            Some(elapsed) if self.git_timed_out => {
                format!("git init ran {:.1}s and was stopped", elapsed.as_secs_f64())
            }
            Some(elapsed) if self.git_failed => {
                format!("git init failed after {:.1}s", elapsed.as_secs_f64())
            }
            Some(elapsed) => format!("git init took {:.1}s", elapsed.as_secs_f64()),
        };
        match (
            self.git_elapsed.is_some() && !self.git_timed_out && !self.git_failed,
            self.latest_seq,
        ) {
            (false, _) => git,
            (true, Some(seq)) => format!(
                "{git}, then the service had recorded {seq} event(s) for it after {:.1}s",
                self.waited_for_service.as_secs_f64()
            ),
            (true, None) => format!(
                "{git}, then the service did not answer status requests for {:.1}s",
                self.waited_for_service.as_secs_f64()
            ),
        }
    }
}

/// Why trace2 ingestion failed, for the summary and the fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngestionFailure {
    /// Doctor could not set up the probe, or `git init` failed.
    Setup,
    /// `git init` itself did not finish in time.
    GitSlow,
    /// The service is up and answering, started recently, and has not
    /// recorded the event yet: it is still getting ready, not broken.
    NotProcessedYet,
    /// The service answers, but the event never arrived.
    NotReceived,
    /// The service did not answer status requests.
    ServiceSilent,
}

pub(crate) fn classify_ingestion_failure(
    failure: &IngestionProbeFailure,
    service_answering: bool,
    recently_started: bool,
) -> IngestionFailure {
    if failure.git_timed_out {
        return IngestionFailure::GitSlow;
    }
    if failure.git_elapsed.is_none() || failure.git_failed {
        return IngestionFailure::Setup;
    }
    match failure.latest_seq {
        Some(0) if service_answering && recently_started => IngestionFailure::NotProcessedYet,
        Some(_) => IngestionFailure::NotReceived,
        None => IngestionFailure::ServiceSilent,
    }
}

fn ingestion_failure_summary(kind: IngestionFailure) -> &'static str {
    match kind {
        IngestionFailure::NotProcessedYet => {
            "the background service is running but has not processed its first git event yet"
        }
        IngestionFailure::GitSlow => "git took too long to run the trace2 ingestion probe",
        IngestionFailure::ServiceSilent => {
            "the background service did not answer while git trace2 events were checked"
        }
        IngestionFailure::Setup | IngestionFailure::NotReceived => {
            "git trace2 events are not reaching the background service"
        }
    }
}

/// UNVERIFIED ON WINDOWS. The Windows text says what was measured and names
/// the two causes seen there that a restart does not fix.
pub(crate) fn trace2_ingestion_remediation(
    windows: bool,
    kind: IngestionFailure,
    failure: &IngestionProbeFailure,
    budget: Duration,
) -> String {
    let measured = format!(
        "measured: {} (budget {:.1}s)",
        failure.measured(),
        budget.as_secs_f64()
    );
    match (windows, kind) {
        (_, IngestionFailure::NotProcessedYet) => format!(
            "the background service started moments ago and is still getting ready ({measured}). Run `autter doctor` again in a minute; if this check still fails, run `{}`",
            crate::commands::daemon::RESTART_COMMAND
        ),
        (true, _) => format!(
            "{measured}. On Windows the usual causes are antivirus scanning git.exe or autter.exe every time they start (in Windows Security, add exclusions for your Git folder and %USERPROFILE%\\.autter\\bin), or git running as a different user or elevation than the background service (git in an Administrator window cannot report to a service started from a normal window, or the reverse: use normal windows for both). If a trace2 config check failed, run `autter install --system`; otherwise run `{}`, then `autter doctor`",
            crate::commands::daemon::RESTART_COMMAND
        ),
        (false, IngestionFailure::GitSlow) => format!(
            "{measured}. git is slow on this machine: re-run with a longer budget, `AUTTER_DOCTOR_TIMEOUT_SCALE=5 autter doctor`"
        ),
        (false, _) => format!("{REMEDIATION_TRACE2_INGESTION} ({measured})"),
    }
}

/// How long ago the running service started, from its pid file.
fn service_age(config: &crate::daemon::DaemonConfig) -> Option<Duration> {
    let started_ns = read_daemon_started_at_ns(config).ok()??;
    let now_ns = system_time_to_unix_nanos(SystemTime::now())?;
    let age_ns = u64::try_from(now_ns.checked_sub(started_ns)?).ok()?;
    Some(Duration::from_nanos(age_ns))
}

/// End-to-end trace2 ingestion: a real `git init` must reach the background
/// service through the global trace2 config. Separate from service health on
/// purpose: a healthy service with missing trace2 config used to fail the
/// "background service" check and suggest a restart that could not help.
pub fn check_daemon_trace2_ingestion(git_program: &str) -> DiagnosticCheckResult {
    trace2_ingestion_check(git_program).0
}

/// [`check_daemon_trace2_ingestion`], with why it failed.
///
/// A probe that misses its budget is tried again with a longer one; only a
/// service that then also stays unresponsive (see [`service_verdict`]) is
/// restarted. It used to be restarted after one missed budget, dropping the
/// work in flight of a service that was only slow.
fn trace2_ingestion_check(git_program: &str) -> (DiagnosticCheckResult, Option<IngestionFailure>) {
    use crate::commands::daemon::probe_daemon_health;

    let mut commands = Vec::new();
    let mut details = Vec::new();
    let config = match crate::daemon::DaemonConfig::from_env_or_default_paths() {
        Ok(config) => config,
        Err(err) => {
            return (
                DiagnosticCheckResult::failed(
                    "trace2 ingestion could not be inspected",
                    vec![format!("failed to determine daemon paths: {}", err)],
                    commands,
                )
                .with_remediation(REMEDIATION_DAEMON_PATHS),
                Some(IngestionFailure::Setup),
            );
        }
    };

    let age = service_age(&config);
    let recently_started = age.is_some_and(|age| age < FIRST_READINESS_WINDOW);
    let budget = trace2_ingestion_budget(cfg!(windows), recently_started, machine_timeout_scale());
    let retry_budget = trace2_ingestion_retry_budget(budget);
    if let Some(age) = age {
        details.push(format!("service started {:.0}s ago", age.as_secs_f64()));
    }

    let passed = |details: Vec<String>, commands: Vec<CommandRecord>, summary: &str| {
        (
            DiagnosticCheckResult::passed(summary, details, commands),
            None,
        )
    };
    const PASSED: &str = "background service is ingesting git trace2 events";

    let mut last_failure = IngestionProbeFailure::default();
    for (attempt, budget) in [(1, budget), (2, retry_budget)] {
        match run_daemon_trace2_ingestion_probe(&mut commands, git_program, &config, budget) {
            Ok(mut probe_details) => {
                details.append(&mut probe_details);
                if attempt == 1 {
                    return passed(details, commands, PASSED);
                }
                details.push(
                    "passed on the second try with a longer budget: the service was slow, not stuck, so doctor did not restart it"
                        .to_string(),
                );
                return passed(
                    details,
                    commands,
                    "background service is ingesting git trace2 events (slowly: it passed on the second try)",
                );
            }
            Err(failure) => {
                details.push(format!(
                    "trace2 ingestion probe {attempt} (budget {:.1}s) failed: {}; {}",
                    budget.as_secs_f64(),
                    failure.error,
                    failure.measured()
                ));
                last_failure = failure;
            }
        }
    }

    // Restart only a service that stays unresponsive. One that answers
    // requests gains nothing from a restart but losing its work in flight.
    let health = probe_daemon_health(&config);
    let mut service_answering = health.is_healthy();
    if !service_answering {
        let confirmation = confirm_service_state(&config, health);
        details.extend(confirmation.explanation());
        service_answering = confirmation.verdict == ServiceVerdict::Healthy;
        if matches!(
            confirmation.verdict,
            ServiceVerdict::Restart | ServiceVerdict::Start
        ) {
            details.push(format!(
                "{DOCTOR_FIX_DETAIL_PREFIX}restarting the background service and retrying"
            ));
            if let Err(err) = crate::commands::daemon::restart_daemon(&config) {
                details.push(format!("restart failed: {err}"));
            }
            match run_daemon_trace2_ingestion_probe(
                &mut commands,
                git_program,
                &config,
                retry_budget,
            ) {
                Ok(mut probe_details) => {
                    details.append(&mut probe_details);
                    details.push("passed after restart".to_string());
                    return passed(details, commands, PASSED);
                }
                Err(failure) => {
                    details.push(format!(
                        "probe failed after restart: {}; {}",
                        failure.error,
                        failure.measured()
                    ));
                    service_answering = probe_daemon_health(&config).is_healthy();
                    last_failure = failure;
                }
            }
        }
    } else {
        details.push(
            "the background service answers requests, so doctor did not restart it".to_string(),
        );
    }

    let kind = classify_ingestion_failure(&last_failure, service_answering, recently_started);
    record_timing(format!(
        "trace2 ingestion probe failed ({kind:?}): {}",
        last_failure.measured()
    ));
    let remediation =
        trace2_ingestion_remediation(cfg!(windows), kind, &last_failure, retry_budget);
    (
        DiagnosticCheckResult::failed(ingestion_failure_summary(kind), details, commands)
            .with_remediation(remediation),
        Some(kind),
    )
}

/// Service readiness plus trace2 ingestion, combined for `autter debug`.
pub fn prepare_daemon_for_debug_self_checks(git_program: &str) -> DiagnosticCheckResult {
    let mut ready = ensure_daemon_ready_for_self_checks();
    if ready.status != DiagnosticStatus::Passed {
        return ready;
    }
    let mut ingestion = check_daemon_trace2_ingestion(git_program);
    ready.details.append(&mut ingestion.details);
    ready.commands.append(&mut ingestion.commands);
    if ingestion.status == DiagnosticStatus::Passed {
        ready.summary = "daemon is ready for debug self-checks".to_string();
        ready
    } else {
        ingestion.details = ready.details;
        ingestion.commands = ready.commands;
        ingestion
    }
}

pub fn check_trace2_global_config(target: &GitDiagnosticTarget) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let expected_target = match crate::daemon::DaemonConfig::from_env_or_default_paths() {
        Ok(config) => config.trace2_event_target(),
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(
                    &format!("failed to determine expected trace2 target: {}", err),
                    None,
                    None,
                    None,
                ),
                commands,
            )
            .with_remediation(REMEDIATION_DAEMON_PATHS);
        }
    };

    let event_targets =
        read_global_git_config_values(&mut commands, &target.program, TRACE2_EVENT_TARGET_KEY);
    let event_nesting =
        read_global_git_config_values(&mut commands, &target.program, TRACE2_EVENT_NESTING_KEY);

    let event_targets = match event_targets {
        Ok(values) => values,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(&err, Some(&expected_target), None, None),
                commands,
            )
            .with_remediation(REMEDIATION_TRACE2_CONFIG_INSPECT);
        }
    };
    let event_nesting = match event_nesting {
        Ok(values) => values,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(
                    &err,
                    Some(&expected_target),
                    Some(&event_targets),
                    None,
                ),
                commands,
            )
            .with_remediation(REMEDIATION_TRACE2_CONFIG_INSPECT);
        }
    };

    let target_matches = event_targets.iter().any(|value| value == &expected_target);
    let nesting_matches = event_nesting
        .iter()
        .any(|value| value == TRACE2_EVENT_NESTING_VALUE);

    if target_matches && nesting_matches {
        return DiagnosticCheckResult::passed(
            "trace2 global config is configured",
            vec![
                format!("{}: {}", TRACE2_EVENT_TARGET_KEY, expected_target),
                format!(
                    "{}: {}",
                    TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_NESTING_VALUE
                ),
            ],
            commands,
        );
    }

    DiagnosticCheckResult::failed(
        "trace2 global config is not configured",
        trace2_config_failure_details(
            "trace2 is not configured for autter daemon mode",
            Some(&expected_target),
            Some(&event_targets),
            Some(&event_nesting),
        ),
        commands,
    )
    .with_remediation(REMEDIATION_TRACE2_CONFIG_MISSING)
}

pub fn run_attribution_self_check(target: &GitDiagnosticTarget) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let deadline = Instant::now() + check_timeout(ATTRIBUTION_SELF_CHECK_TIMEOUT);
    let repo_path = debug_self_check_root().join(format!(
        "{}-{}",
        sanitize_label(&target.label),
        crate::uuid::generate_v4()
    ));
    let file_path = repo_path.join(SELF_CHECK_FILE);

    let result = (|| -> Result<Vec<String>, String> {
        fs::create_dir_all(&repo_path)
            .map_err(|e| format!("failed to create {}: {}", repo_path.display(), e))?;

        run_required_until(
            &mut commands,
            &target.program,
            &["init", "."],
            Some(&repo_path),
            deadline,
        )?;
        run_required_until(
            &mut commands,
            &target.program,
            &["config", "user.name", "Autter Debug"],
            Some(&repo_path),
            deadline,
        )?;
        run_required_until(
            &mut commands,
            &target.program,
            &["config", "user.email", "debug-self-check@autter.invalid"],
            Some(&repo_path),
            deadline,
        )?;
        run_required_until(
            &mut commands,
            &target.program,
            &["remote", "add", "origin", DEBUG_SELF_CHECK_REMOTE_URL],
            Some(&repo_path),
            deadline,
        )?;

        fs::write(&file_path, SELF_CHECK_CONTENT_UNTRACKED)
            .map_err(|e| format!("failed to write {}: {}", file_path.display(), e))?;
        run_autter_checkpoint(&mut commands, &repo_path, "human", deadline)?;
        wait_for_checkpoint_count(&repo_path, 1, deadline)?;

        fs::write(&file_path, SELF_CHECK_CONTENT_KNOWN_HUMAN)
            .map_err(|e| format!("failed to write {}: {}", file_path.display(), e))?;
        run_autter_checkpoint(&mut commands, &repo_path, "mock_known_human", deadline)?;
        wait_for_checkpoint_count(&repo_path, 2, deadline)?;

        fs::write(&file_path, SELF_CHECK_CONTENT_AI)
            .map_err(|e| format!("failed to write {}: {}", file_path.display(), e))?;
        run_autter_checkpoint(&mut commands, &repo_path, "mock_ai", deadline)?;
        wait_for_checkpoint_count(&repo_path, 3, deadline)?;

        run_required_until(
            &mut commands,
            &target.program,
            &["add", SELF_CHECK_FILE],
            Some(&repo_path),
            deadline,
        )?;
        run_required_until(
            &mut commands,
            &target.program,
            &["commit", "-m", "autter debug self check"],
            Some(&repo_path),
            deadline,
        )?;

        let commit_sha = run_required_until(
            &mut commands,
            &target.program,
            &["rev-parse", "HEAD"],
            Some(&repo_path),
            deadline,
        )?
        .stdout
        .trim()
        .to_string();

        let mut details = poll_self_check_attribution(&repo_path, &commit_sha, deadline)?;
        details.insert(0, format!("repo: {}", repo_path.display()));
        details.insert(1, format!("commit: {}", commit_sha));
        details.insert(
            2,
            format!("notes backend: {}", Config::get().notes_backend_kind()),
        );
        Ok(details)
    })();

    match result {
        Ok(details) => {
            wait_for_daemon_family_idle(&repo_path, deadline);
            let _ = fs::remove_dir_all(&repo_path);
            DiagnosticCheckResult::passed("attribution self-check completed", details, commands)
        }
        Err(err) => {
            let mut details = vec![format!("repo: {}", repo_path.display()), err.clone()];
            let daemon_detail = daemon_family_status_detail(&repo_path);
            details.push(daemon_detail.clone());
            if path_is_in_debug_self_check_root(&repo_path) {
                details.push(
                    "failed self-check repository was left in place for inspection".to_string(),
                );
                details.push(format!(
                    "inspect leftover repo with: cd {} && autter blame {}",
                    repo_path.display(),
                    SELF_CHECK_FILE
                ));
            }
            let remediation =
                attribution_self_check_remediation(&err, Some(daemon_detail.as_str()));
            DiagnosticCheckResult::failed("attribution self-check failed", details, commands)
                .with_remediation(remediation)
        }
    }
}

/// Map attribution self-check failure text to a cause-specific `fix:` line.
///
/// Doctor and debug print this as the actionable next step; the leading
/// `likely cause:` clause tells the user *why* before the commands.
pub(crate) fn attribution_self_check_remediation(err: &str, daemon_detail: Option<&str>) -> String {
    let err_l = err.to_ascii_lowercase();
    let daemon_error = daemon_detail
        .filter(|d| d.contains("last_error=") && !d.contains("last_error=<none>"))
        .and_then(|d| {
            d.split("last_error=")
                .nth(1)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && *s != "<none>")
        });

    if err_l.contains("waiting for checkpoint persistence")
        || err_l.contains("checkpoint(s) visible")
    {
        let mut msg = String::from(
            "likely cause: checkpoints are not persisting to the working log (daemon not draining or checkpoint write failing). next: run `autter bg status`, then `autter bg restart`, then re-run `autter doctor`",
        );
        if let Some(daemon_err) = daemon_error {
            msg.push_str(&format!(" (daemon last_error: {})", daemon_err));
        }
        return msg;
    }

    if err_l.contains("checkpoint")
        && (err_l.contains("command failed") || err_l.contains("command timed out"))
    {
        return String::from(
            "likely cause: the `autter checkpoint` command itself failed or hung before writing. next: run `autter bg restart`, then `autter doctor`; if it keeps failing, re-run `autter install --system` and check `autter debug` for the checkpoint command log",
        );
    }

    if err_l.contains("unexpected attribution")
        || err_l.contains("waiting for expected attribution")
    {
        // Checkpoints ran far enough to commit; blame/notes did not match.
        if err_l.contains("got untracked") || err_l.contains("got unknown") {
            return String::from(
                "likely cause: the commit completed but authorship notes were not written (post-commit / git-proxy / notes path). next: run `autter install --system` so git is proxied and trace2 is configured, then `autter doctor`; if trace2 checks already failed above, fix those first",
            );
        }
        return String::from(
            "likely cause: blame saw the wrong line classes after commit (notes present but mismatched). next: inspect the leftover self-check repo if listed above, run `autter install --system`, then `autter doctor`; share `autter debug` output if it still fails",
        );
    }

    if err_l.contains("blame analysis failed") {
        return String::from(
            "likely cause: blame/notes could not be read after the self-check commit. next: run `autter install --system`, then `autter doctor`; if notes backend errors appear in the details, re-run `autter bg restart` and try again",
        );
    }

    if err_l.contains("failed to create")
        || err_l.contains("failed to write")
        || err_l.contains("permission denied")
    {
        return String::from(
            "likely cause: the self-check could not create or write its temp repository (permissions or full disk). next: check that your home directory is writable, free some disk space, then re-run `autter doctor`",
        );
    }

    if err_l.contains("timed out") && err_l.contains("before this command could start") {
        return String::from(
            "likely cause: earlier self-check steps exhausted the time budget (often a stuck daemon). next: run `autter bg restart`, then `autter doctor`",
        );
    }

    if let Some(daemon_err) = daemon_error {
        return format!(
            "likely cause: daemon reported an error while the self-check ran ({daemon_err}). next: run `autter bg status`, then `autter bg restart`, then `autter doctor`; if a trace2 check also failed, run `autter install --system` first"
        );
    }

    REMEDIATION_ATTRIBUTION_FALLBACK.to_string()
}

pub fn run_trace2_file_self_check(target: &GitDiagnosticTarget) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let deadline = Instant::now() + check_timeout(DEBUG_CHECK_TIMEOUT);
    let trace_dir = crate::mdm::utils::home_dir()
        .join(".autter")
        .join("internal")
        .join("daemon");
    let trace_path = trace_dir.join(format!(
        "trace2-debug-check-{}-{}.json",
        sanitize_label(&target.label),
        crate::uuid::generate_v4()
    ));
    let trace_command_dir = debug_self_check_root().join(format!(
        "trace2-{}-{}",
        sanitize_label(&target.label),
        crate::uuid::generate_v4()
    ));

    let snapshot = match snapshot_global_trace2_event_target(&mut commands, &target.program) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 file self-check failed",
                vec![err],
                commands,
            )
            .with_remediation(REMEDIATION_TRACE2_FILE);
        }
    };

    let mut changed_global_event_target = false;
    let result = (|| -> Result<(Vec<String>, String), String> {
        fs::create_dir_all(&trace_dir)
            .map_err(|e| format!("failed to create {}: {}", trace_dir.display(), e))?;
        fs::create_dir_all(&trace_command_dir)
            .map_err(|e| format!("failed to create {}: {}", trace_command_dir.display(), e))?;
        let _ = fs::remove_file(&trace_path);
        let trace_path_string = trace_path.to_string_lossy().to_string();

        // This intentionally uses global git config rather than a process-local
        // GIT_TRACE2_EVENT override so the diagnostic exercises the install path.
        run_required_until(
            &mut commands,
            &target.program,
            &[
                "config",
                "--global",
                "--replace-all",
                TRACE2_EVENT_TARGET_KEY,
                trace_path_string.as_str(),
            ],
            None,
            deadline,
        )?;
        changed_global_event_target = true;

        // Use init rather than version: when terminal git is the autter proxy,
        // read-only commands intentionally suppress trace2 before invoking real git.
        run_required_until(
            &mut commands,
            &target.program,
            &["init", "."],
            Some(&trace_command_dir),
            deadline,
        )?;

        let trace2_json = fs::read_to_string(&trace_path)
            .map_err(|e| format!("failed to read {}: {}", trace_path.display(), e))?;
        let details = validate_trace2_command_events(&trace2_json, "init")?;
        Ok((details, trace2_json))
    })();

    let restore_result = if changed_global_event_target {
        restore_global_trace2_event_target(&mut commands, &target.program, &snapshot)
    } else {
        Ok(())
    };
    let _ = fs::remove_file(&trace_path);
    let _ = fs::remove_dir_all(&trace_command_dir);

    match (result, restore_result) {
        (Ok((mut details, trace2_json)), Ok(())) => {
            details.insert(0, format!("trace2 file: {}", trace_path.display()));
            details.insert(1, format!("command dir: {}", trace_command_dir.display()));
            DiagnosticCheckResult::passed("trace2 file self-check completed", details, commands)
                .with_trace2_json(Some(trace2_json))
        }
        (Ok((mut details, trace2_json)), Err(restore_err)) => {
            details.insert(0, format!("trace2 file: {}", trace_path.display()));
            details.insert(1, format!("command dir: {}", trace_command_dir.display()));
            details.push(format!("restore failed: {}", restore_err));
            DiagnosticCheckResult::failed("trace2 file self-check failed", details, commands)
                .with_trace2_json(Some(trace2_json))
                .with_remediation(REMEDIATION_TRACE2_FILE)
        }
        (Err(err), Ok(())) => DiagnosticCheckResult::failed(
            "trace2 file self-check failed",
            vec![
                format!("trace2 file: {}", trace_path.display()),
                format!("command dir: {}", trace_command_dir.display()),
                err,
            ],
            commands,
        )
        .with_remediation(REMEDIATION_TRACE2_FILE),
        (Err(err), Err(restore_err)) => DiagnosticCheckResult::failed(
            "trace2 file self-check failed",
            vec![
                format!("trace2 file: {}", trace_path.display()),
                format!("command dir: {}", trace_command_dir.display()),
                err,
                format!("restore failed: {}", restore_err),
            ],
            commands,
        )
        .with_remediation(REMEDIATION_TRACE2_FILE),
    }
}

fn run_daemon_trace2_ingestion_probe(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    config: &crate::daemon::DaemonConfig,
    budget: Duration,
) -> Result<Vec<String>, IngestionProbeFailure> {
    let deadline = Instant::now() + budget;
    let probe_path =
        debug_self_check_root().join(format!("daemon-probe-{}", crate::uuid::generate_v4()));
    let mut failure = IngestionProbeFailure::default();

    let result = (|| -> Result<Vec<String>, String> {
        fs::create_dir_all(&probe_path)
            .map_err(|e| format!("failed to create {}: {}", probe_path.display(), e))?;
        let git_started = Instant::now();
        let git = run_required_until(
            commands,
            git_program,
            &["init", "."],
            Some(&probe_path),
            deadline,
        );
        let git_elapsed = git_started.elapsed();
        failure.git_elapsed = Some(git_elapsed);
        if let Err(err) = git {
            let timed_out = commands.last().is_some_and(|record| record.timed_out);
            failure.git_timed_out = timed_out;
            failure.git_failed = !timed_out;
            if err.starts_with("command timed out before start") {
                failure.git_elapsed = None;
            }
            return Err(err);
        }
        record_timing(format!(
            "trace2 probe: git init took {:.2}s",
            git_elapsed.as_secs_f64()
        ));

        let waited = Instant::now();
        let status = wait_for_daemon_family_status(config, &probe_path, 1, deadline);
        failure.waited_for_service = waited.elapsed();
        let status = status.map_err(|(err, latest_seq)| {
            failure.latest_seq = latest_seq;
            err
        })?;
        record_timing(format!(
            "trace2 probe: service recorded the event after {:.2}s",
            failure.waited_for_service.as_secs_f64()
        ));
        Ok(vec![
            format!("daemon trace2 probe repo: {}", probe_path.display()),
            format!("daemon trace2 probe latest_seq: {}", status.latest_seq),
            format!(
                "daemon trace2 probe git init: {:.2}s, event recorded after {:.2}s",
                git_elapsed.as_secs_f64(),
                failure.waited_for_service.as_secs_f64()
            ),
            format!(
                "daemon trace2 probe last_error: {}",
                status.last_error.unwrap_or_else(|| "<none>".to_string())
            ),
        ])
    })();

    match result {
        Ok(details) => {
            let _ = fs::remove_dir_all(&probe_path);
            Ok(details)
        }
        Err(error) => {
            failure.error = error;
            Err(failure)
        }
    }
}

fn run_autter_checkpoint(
    commands: &mut Vec<CommandRecord>,
    repo_path: &Path,
    preset: &str,
    deadline: Instant,
) -> Result<CommandRecord, String> {
    let autter = std::env::current_exe()
        .map_err(|e| format!("failed to resolve autter binary path: {}", e))?;
    let autter = autter.to_string_lossy().to_string();
    run_required_until(
        commands,
        &autter,
        &["checkpoint", preset, SELF_CHECK_FILE],
        Some(repo_path),
        deadline,
    )
}

fn run_required_until(
    commands: &mut Vec<CommandRecord>,
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    deadline: Instant,
) -> Result<CommandRecord, String> {
    let timeout = remaining_timeout(deadline);
    if timeout.is_zero() {
        let record = CommandRecord {
            command: format_command(program, args),
            cwd: cwd.map(|p| p.display().to_string()),
            status: None,
            stdout: String::new(),
            stderr: format!(
                "self-check timed out after {:.1}s before this command could start",
                check_timeout(ATTRIBUTION_SELF_CHECK_TIMEOUT).as_secs_f64()
            ),
            timed_out: true,
        };
        let error = format!("command timed out before start: {}", record.command);
        commands.push(record);
        return Err(error);
    }

    run_required_with_timeout(commands, program, args, cwd, timeout)
}

fn run_required_with_timeout(
    commands: &mut Vec<CommandRecord>,
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<CommandRecord, String> {
    let record = run_logged_command_with_timeout(program, args, cwd, timeout);
    let success = record.success();
    let error = if success {
        None
    } else if record.timed_out {
        let mut error = format!(
            "command timed out: {} (timeout={:.1}s, status={})",
            record.command,
            timeout.as_secs_f64(),
            format_status(record.status)
        );
        if let Some(cwd) = &record.cwd {
            error.push_str(&format!(", cwd={}", cwd));
        }
        if !record.stdout.trim().is_empty() {
            error.push_str(&format!(", stdout={}", record.stdout.trim()));
        }
        if !record.stderr.trim().is_empty() {
            error.push_str(&format!(", stderr={}", record.stderr.trim()));
        }
        Some(error)
    } else {
        Some(format!(
            "command failed: {} (status={})",
            record.command,
            format_status(record.status)
        ))
    };
    commands.push(record.clone());
    match error {
        Some(error) => Err(error),
        None => Ok(record),
    }
}

fn run_logged_command(program: &str, args: &[&str], cwd: Option<&Path>) -> CommandRecord {
    run_logged_command_with_timeout(program, args, cwd, check_timeout(DEBUG_CHECK_TIMEOUT))
}

fn run_logged_command_with_timeout(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> CommandRecord {
    let command = format_command(program, args);
    let cwd_display = cwd.map(|p| p.display().to_string());
    match run_command_with_timeout(
        program,
        args,
        cwd,
        timeout,
        POLL_INTERVAL,
        SELF_CHECK_TRACE_ENV_REMOVE,
    ) {
        Ok(output) => {
            let stderr = format_logged_stderr(
                output.timed_out,
                timeout,
                output.stderr,
                output.diagnostics,
                output.wait_error,
            );
            CommandRecord {
                command,
                cwd: cwd_display,
                status: output.status,
                stdout: output.stdout,
                stderr,
                timed_out: output.timed_out,
            }
        }
        Err(e) => CommandRecord {
            command,
            cwd: cwd_display,
            status: None,
            stdout: String::new(),
            stderr: e,
            timed_out: false,
        },
    }
}

fn format_logged_stderr(
    timed_out: bool,
    timeout: Duration,
    process_stderr: String,
    diagnostics: Vec<String>,
    wait_error: Option<String>,
) -> String {
    let mut stderr = String::new();
    if timed_out {
        stderr.push_str(&format!("timed out after {:.1}s", timeout.as_secs_f64()));
        if !process_stderr.trim().is_empty() {
            stderr.push_str("\nstderr before timeout:\n");
            stderr.push_str(process_stderr.trim());
        }
    } else {
        stderr.push_str(process_stderr.trim());
    }

    if let Some(wait_error) = wait_error {
        append_stderr_line(
            &mut stderr,
            &format!("failed while waiting for command: {}", wait_error),
        );
    }
    for diagnostic in diagnostics {
        append_stderr_line(&mut stderr, &diagnostic);
    }
    stderr
}

fn append_stderr_line(stderr: &mut String, line: &str) {
    if !stderr.is_empty() {
        stderr.push('\n');
    }
    stderr.push_str(line);
}

fn remaining_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn wait_for_checkpoint_count(
    repo_path: &Path,
    expected_min_count: usize,
    deadline: Instant,
) -> Result<(), String> {
    let start = Instant::now();
    let mut last_error = None;

    while Instant::now() < deadline {
        match read_checkpoint_count(repo_path) {
            Ok(count) if count >= expected_min_count => return Ok(()),
            Ok(count) => {
                last_error = Some(format!(
                    "only {} checkpoint(s) visible, expected at least {}",
                    count, expected_min_count
                ));
            }
            Err(e) => last_error = Some(e),
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    Err(format!(
        "timed out after {:.1}s waiting for checkpoint persistence: {}",
        start.elapsed().as_secs_f64(),
        last_error.unwrap_or_else(|| {
            format!(
                "no checkpoint status available for repo {}",
                repo_path.display()
            )
        })
    ))
}

fn read_checkpoint_count(repo_path: &Path) -> Result<usize, String> {
    let repo = discover_repository_in_path_no_git_exec(repo_path).map_err(|e| e.to_string())?;
    let working_log = repo
        .storage
        .working_log_for_base_commit("initial")
        .map_err(|e| e.to_string())?;
    working_log
        .read_all_checkpoints()
        .map(|checkpoints| checkpoints.len())
        .map_err(|e| e.to_string())
}

fn poll_self_check_attribution(
    repo_path: &Path,
    commit_sha: &str,
    deadline: Instant,
) -> Result<Vec<String>, String> {
    let start = Instant::now();
    let repo = discover_repository_in_path_no_git_exec(repo_path).map_err(|e| e.to_string())?;
    let notes_backend = Config::get().notes_backend_kind();
    let mut last_error = None;

    while Instant::now() < deadline {
        match validate_self_check_blame_analysis(
            repo.blame_analysis(SELF_CHECK_FILE, &self_check_blame_options(commit_sha))
                .map_err(|e| e.to_string()),
        ) {
            Ok(details) => return Ok(details),
            Err(err) => last_error = Some(err),
        }

        if remaining_timeout(deadline).is_zero() {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    Err(format!(
        "timed out after {:.1}s waiting for expected attribution via {} backend for {} in {}: {}",
        start.elapsed().as_secs_f64(),
        notes_backend,
        commit_sha,
        repo_path.display(),
        last_error.unwrap_or_else(|| "no blame analysis result available".to_string())
    ))
}

fn self_check_blame_options(commit_sha: &str) -> AutterBlameOptions {
    AutterBlameOptions {
        line_ranges: vec![(1, 3)],
        newest_commit: Some(commit_sha.to_string()),
        use_prompt_hashes_as_names: true,
        return_human_authors_as_human: true,
        ..AutterBlameOptions::default()
    }
}

/// Poll the service until it has recorded `expected_min_seq` events for the
/// repository. On timeout, the error and the last event count it reported
/// (`None` when it never answered).
fn wait_for_daemon_family_status(
    config: &crate::daemon::DaemonConfig,
    repo_path: &Path,
    expected_min_seq: u64,
    deadline: Instant,
) -> Result<FamilyStatus, (String, Option<u64>)> {
    let mut last_error = None;
    let mut last_seq = None;

    while Instant::now() < deadline {
        match read_daemon_family_status(config, repo_path) {
            Ok(status) if status.latest_seq >= expected_min_seq => return Ok(status),
            Ok(status) => {
                last_seq = Some(status.latest_seq);
                last_error = Some(format!(
                    "latest_seq={}, expected at least {}, last_error={}",
                    status.latest_seq,
                    expected_min_seq,
                    status.last_error.as_deref().unwrap_or("<none>")
                ));
            }
            Err(err) => last_error = Some(err),
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    Err((
        format!(
            "timed out waiting for daemon family status: {}",
            last_error.unwrap_or_else(|| format!("no status for {}", repo_path.display()))
        ),
        last_seq,
    ))
}

/// Waits until the daemon has no side effects in flight for the repo, so that
/// deleting the repo does not race them. Returns at once when the daemon is not
/// reachable or is too old to report in-flight side effects.
pub(crate) fn wait_for_daemon_family_idle(repo_path: &Path, deadline: Instant) {
    let Ok(config) = crate::daemon::DaemonConfig::from_env_or_default_paths() else {
        return;
    };
    while Instant::now() < deadline {
        match read_daemon_family_status(&config, repo_path) {
            Ok(status) if status.inflight_effects > 0 => std::thread::sleep(POLL_INTERVAL),
            _ => return,
        }
    }
}

fn read_daemon_family_status(
    config: &crate::daemon::DaemonConfig,
    repo_path: &Path,
) -> Result<FamilyStatus, String> {
    let request = ControlRequest::StatusFamily {
        repo_working_dir: repo_path.display().to_string(),
    };
    let response = crate::daemon::send_control_request_with_timeout(
        &config.control_socket_path,
        &request,
        DAEMON_CONTROL_TIMEOUT,
    )
    .map_err(|e| e.to_string())?;

    if !response.ok {
        return Err(response
            .error
            .unwrap_or_else(|| "daemon status request failed".to_string()));
    }

    let data = response
        .data
        .ok_or_else(|| "daemon status response had no data".to_string())?;
    serde_json::from_value::<FamilyStatus>(data).map_err(|e| e.to_string())
}

fn daemon_family_status_detail(repo_path: &Path) -> String {
    let config = match crate::daemon::DaemonConfig::from_env_or_default_paths() {
        Ok(config) => config,
        Err(err) => {
            return format!("daemon status for repo: <error: {}>", err);
        }
    };

    match read_daemon_family_status(&config, repo_path) {
        Ok(status) => format!(
            "daemon status for repo: latest_seq={}, last_error={}",
            status.latest_seq,
            status.last_error.as_deref().unwrap_or("<none>")
        ),
        Err(err) => format!("daemon status for repo: <error: {}>", err),
    }
}

pub(crate) fn daemon_binary_is_stale(config: &crate::daemon::DaemonConfig) -> Result<bool, String> {
    let Some(started_at_ns) = read_daemon_started_at_ns(config)? else {
        return Ok(false);
    };
    let binary_modified_ns = current_binary_modified_ns()?;
    Ok(binary_modified_ns > started_at_ns)
}

/// Where the service writes its pid and start time: next to its lock, the
/// way `daemon.rs` does. With long paths on macOS and Linux that is the short
/// temp folder, not `~/.autter/internal/daemon`, which is where this used to
/// look (so the start time, and an outdated binary, went unseen there).
pub(crate) fn daemon_pid_metadata_path(config: &crate::daemon::DaemonConfig) -> std::path::PathBuf {
    config
        .lock_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("daemon.pid.json")
}

fn read_daemon_started_at_ns(config: &crate::daemon::DaemonConfig) -> Result<Option<u128>, String> {
    let pid_path = daemon_pid_metadata_path(config);
    let contents = match fs::read_to_string(&pid_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "failed to read daemon pid metadata at {}: {}",
                pid_path.display(),
                err
            ));
        }
    };
    let value: Value = serde_json::from_str(&contents).map_err(|e| e.to_string())?;
    Ok(value
        .get("started_at_ns")
        .and_then(Value::as_u64)
        .map(u128::from))
}

fn current_binary_modified_ns() -> Result<u128, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let modified = fs::metadata(&exe)
        .and_then(|metadata| metadata.modified())
        .map_err(|e| format!("failed to read mtime for {}: {}", exe.display(), e))?;
    system_time_to_unix_nanos(modified).ok_or_else(|| {
        format!(
            "failed to convert mtime for {} to unix timestamp",
            exe.display()
        )
    })
}

fn system_time_to_unix_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineClassification {
    Untracked,
    KnownHuman,
    Ai,
    Unknown,
}

impl LineClassification {
    fn as_str(self) -> &'static str {
        match self {
            LineClassification::Untracked => "untracked",
            LineClassification::KnownHuman => "known_human",
            LineClassification::Ai => "ai",
            LineClassification::Unknown => "unknown",
        }
    }
}

fn validate_self_check_blame_analysis(
    analysis: Result<BlameAnalysisResult, String>,
) -> Result<Vec<String>, String> {
    let analysis = analysis.map_err(|err| format!("blame analysis failed: {}", err))?;
    let expected = [
        (1, LineClassification::Untracked),
        (2, LineClassification::KnownHuman),
        (3, LineClassification::Ai),
    ];
    let mut details = Vec::new();

    for (line, expected_class) in expected {
        let actual = classify_line(&analysis, line);
        let raw_author = analysis
            .line_authors
            .get(&line)
            .cloned()
            .unwrap_or_else(|| "<missing>".to_string());
        details.push(format!(
            "line {}: {} (expected {}, raw={})",
            line,
            actual.as_str(),
            expected_class.as_str(),
            raw_author
        ));
        if actual != expected_class {
            return Err(format!(
                "unexpected attribution for line {}: got {}, expected {}\n{}",
                line,
                actual.as_str(),
                expected_class.as_str(),
                format_blame_analysis_debug(&analysis)
            ));
        }
    }

    details.push(format_blame_analysis_debug(&analysis));
    Ok(details)
}

fn classify_line(analysis: &BlameAnalysisResult, line: u32) -> LineClassification {
    let Some(author) = analysis.line_authors.get(&line) else {
        return LineClassification::Unknown;
    };

    if author == &CheckpointKind::Human.to_str() {
        return LineClassification::Untracked;
    }

    if author.starts_with("h_") && analysis.humans.contains_key(author) {
        return LineClassification::KnownHuman;
    }

    if analysis
        .prompt_records
        .get(author)
        .is_some_and(|prompt| prompt.agent_id.tool == "mock_ai")
    {
        return LineClassification::Ai;
    }

    LineClassification::Unknown
}

fn format_blame_analysis_debug(analysis: &BlameAnalysisResult) -> String {
    let mut prompt_keys = analysis.prompt_records.keys().cloned().collect::<Vec<_>>();
    prompt_keys.sort();
    let mut session_keys = analysis.session_records.keys().cloned().collect::<Vec<_>>();
    session_keys.sort();
    let mut human_keys = analysis.humans.keys().cloned().collect::<Vec<_>>();
    human_keys.sort();

    format!(
        "blame analysis: line_authors={:?}, prompt_keys={:?}, session_keys={:?}, human_keys={:?}",
        analysis.line_authors, prompt_keys, session_keys, human_keys
    )
}

#[derive(Debug, Clone, Default)]
struct Trace2EventTargetSnapshot {
    values: Vec<String>,
}

fn snapshot_global_trace2_event_target(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
) -> Result<Trace2EventTargetSnapshot, String> {
    let record = run_logged_command(
        git_program,
        &[
            "config",
            "--global",
            "--no-includes",
            "--get-all",
            TRACE2_EVENT_TARGET_KEY,
        ],
        None,
    );
    let snapshot = if record.success() {
        record
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else if record.status == Some(1) {
        Vec::new()
    } else {
        let err = format!(
            "failed to snapshot global {}: status={}, stderr={}",
            TRACE2_EVENT_TARGET_KEY,
            format_status(record.status),
            record.stderr
        );
        commands.push(record);
        return Err(err);
    };
    commands.push(record);
    Ok(Trace2EventTargetSnapshot { values: snapshot })
}

fn restore_global_trace2_event_target(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    snapshot: &Trace2EventTargetSnapshot,
) -> Result<(), String> {
    let remove = run_logged_command(
        git_program,
        &["config", "--global", "--unset-all", TRACE2_EVENT_TARGET_KEY],
        None,
    );
    let remove_ok = remove.success() || remove.status == Some(5);
    let remove_error = if remove_ok {
        None
    } else {
        Some(format!(
            "failed to remove temporary {}: status={}, stderr={}",
            TRACE2_EVENT_TARGET_KEY,
            format_status(remove.status),
            remove.stderr
        ))
    };
    commands.push(remove);
    if let Some(error) = remove_error {
        return Err(error);
    }

    for value in &snapshot.values {
        let record = run_logged_command(
            git_program,
            &[
                "config",
                "--global",
                "--add",
                TRACE2_EVENT_TARGET_KEY,
                value,
            ],
            None,
        );
        let error = if record.success() {
            None
        } else {
            Some(format!(
                "failed to restore {}: status={}, stderr={}",
                TRACE2_EVENT_TARGET_KEY,
                format_status(record.status),
                record.stderr
            ))
        };
        commands.push(record);
        if let Some(error) = error {
            return Err(error);
        }
    }

    Ok(())
}

fn validate_trace2_command_events(
    trace2_json: &str,
    expected_command: &str,
) -> Result<Vec<String>, String> {
    let mut events = Vec::new();
    for (idx, line) in trace2_json.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid trace2 JSON on line {}: {}", idx + 1, e))?;
        events.push(value);
    }

    if events.is_empty() {
        return Err("trace2 file was empty".to_string());
    }

    let has_version = events
        .iter()
        .any(|event| event.get("event").and_then(Value::as_str) == Some("version"));
    let has_start = events
        .iter()
        .any(|event| event.get("event").and_then(Value::as_str) == Some("start"));
    let has_cmd_name_expected = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("cmd_name")
            && event.get("name").and_then(Value::as_str) == Some(expected_command)
    });
    let has_exit_zero = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("exit")
            && event.get("code").and_then(Value::as_i64) == Some(0)
    });
    let has_atexit_zero = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("atexit")
            && event.get("code").and_then(Value::as_i64) == Some(0)
    });

    let failures = [
        (has_version, "missing version event"),
        (has_start, "missing start event"),
        (
            has_cmd_name_expected,
            "missing cmd_name event for expected command",
        ),
        (has_exit_zero, "missing exit event with code 0"),
        (has_atexit_zero, "missing atexit event with code 0"),
    ]
    .into_iter()
    .filter_map(|(ok, msg)| (!ok).then_some(msg))
    .collect::<Vec<_>>();

    if !failures.is_empty() {
        return Err(format!("unexpected trace2 events: {}", failures.join(", ")));
    }

    Ok(vec![
        format!("events: {}", events.len()),
        format!(
            "validated: version/start/cmd_name({})/exit(0)/atexit(0)",
            expected_command
        ),
    ])
}

fn read_global_git_config_values(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    key: &str,
) -> Result<Vec<String>, String> {
    let record = run_logged_command(git_program, &["config", "--global", "--get-all", key], None);
    let values = if record.success() {
        record
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else if record.status == Some(1) {
        Vec::new()
    } else {
        let err = format!(
            "failed to read global {}: status={}, stderr={}",
            key,
            format_status(record.status),
            record.stderr
        );
        commands.push(record);
        return Err(err);
    };
    commands.push(record);
    Ok(values)
}

fn trace2_config_failure_details(
    reason: &str,
    expected_target: Option<&str>,
    actual_targets: Option<&[String]>,
    actual_nesting: Option<&[String]>,
) -> Vec<String> {
    let mut details = vec![
        format!("ERROR: {}", reason),
        "Why this matters: autter daemon mode relies on Git trace2 events to match real Git commands to checkpoint and authorship state; without this config, commit/rebase/merge attribution can be missed or delayed.".to_string(),
    ];

    if let Some(expected_target) = expected_target {
        details.push(format!(
            "Expected {}: {}",
            TRACE2_EVENT_TARGET_KEY, expected_target
        ));
    }
    if let Some(actual_targets) = actual_targets {
        details.push(format!(
            "Actual {}: {}",
            TRACE2_EVENT_TARGET_KEY,
            format_config_values(actual_targets)
        ));
    }
    details.push(format!(
        "Expected {}: {}",
        TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_NESTING_VALUE
    ));
    if let Some(actual_nesting) = actual_nesting {
        details.push(format!(
            "Actual {}: {}",
            TRACE2_EVENT_NESTING_KEY,
            format_config_values(actual_nesting)
        ));
    }

    details.push("Common causes: `autter install-hooks` has not run, was run with `--dry-run`, or failed while writing global Git config.".to_string());
    details.push("Common causes: autter cannot edit the same global Git config Git reads because HOME/USERPROFILE/XDG_CONFIG_HOME/GIT_CONFIG_GLOBAL points somewhere different, the global config file or parent directory is read-only or locked, permissions or ownership are wrong, or the configured git and terminal git use different config locations.".to_string());
    details
}

fn format_config_values(values: &[String]) -> String {
    if values.is_empty() {
        "<missing>".to_string()
    } else {
        values.join(", ")
    }
}

fn sanitize_label(label: &str) -> String {
    let sanitized = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "git".to_string()
    } else {
        trimmed.to_lowercase()
    }
}

fn format_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .map(shell_quote_for_display)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote_for_display(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn format_status(status: Option<i32>) -> String {
    status
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

/// One reason capture is not working, with the one command that fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureIssue {
    pub what: String,
    pub fix: String,
}

impl CaptureIssue {
    pub fn new(what: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            fix: fix.into(),
        }
    }
}

/// The fix to show for a failed check: its remediation, or `fallback`.
fn issue_from_check(check: DiagnosticCheckResult, fallback: &str) -> CaptureIssue {
    CaptureIssue::new(
        check.summary,
        check.remediation.unwrap_or_else(|| fallback.to_string()),
    )
}

/// Is commit capture actually working right now? Onboarding and
/// `autter install --system` run this at the end and only report success when
/// it returns no issues, so "You're all set!" can no longer follow a skipped
/// trace2 setup or a background service that timed out.
///
/// Covers what every commit depends on: the background service is healthy,
/// git's global trace2 config points at it (for the git autter runs and the
/// git on PATH), and a real git event reaches it. The slower end-to-end
/// checkpoint round-trip stays in `autter doctor`.
pub fn verify_capture() -> Vec<CaptureIssue> {
    let mut issues = Vec::new();
    let git_cmd = Config::get().git_cmd().to_string();

    let service = ensure_daemon_ready_for_self_checks();
    let service_ok = service.status == DiagnosticStatus::Passed;
    if !service_ok {
        issues.push(issue_from_check(service, REMEDIATION_DAEMON_FAILED));
    }

    for target in [
        GitDiagnosticTarget::new("configured git", &git_cmd),
        GitDiagnosticTarget::new("terminal git", "git"),
    ] {
        // Same binary twice (the usual case): check it once.
        if target.label == "terminal git" && git_cmd == "git" {
            continue;
        }
        let check = check_trace2_global_config(&target);
        if check.status == DiagnosticStatus::Failed {
            let mut issue = issue_from_check(check, REMEDIATION_TRACE2_CONFIG_MISSING);
            issue.what = format!("{} ({})", issue.what, target.label);
            issues.push(issue);
        }
    }

    if service_ok {
        let (ingestion, failure) = trace2_ingestion_check(&git_cmd);
        if failure == Some(IngestionFailure::NotProcessedYet) {
            // A service that has just started and answers requests but has
            // not recorded its first event is still starting, not a failed
            // start (UNVERIFIED ON WINDOWS, where this was reported).
            eprintln!(
                "Note: {}. This is common right after it starts on Windows; run `autter doctor` in a minute to confirm commits reach it.",
                ingestion.summary
            );
        } else if ingestion.status == DiagnosticStatus::Failed {
            issues.push(issue_from_check(ingestion, REMEDIATION_TRACE2_INGESTION));
        }
    }
    issues
}

/// Repair a trace2 config that autter itself wrote but that no longer points
/// at this machine's background service (an older install location, a moved
/// home folder). The user already consented to that config when it was
/// written, so doctor fixes it in place instead of telling them to.
///
/// Never writes trace2 for someone who has none, and never replaces a target
/// that isn't autter's: those still need `autter install --system`. Returns a
/// line describing the repair, or `None` when nothing was changed.
pub fn repair_stale_trace2_config(git_program: &str) -> Option<String> {
    let expected = crate::daemon::DaemonConfig::from_env_or_default_paths()
        .ok()?
        .trace2_event_target();
    let mut commands = Vec::new();
    let targets =
        read_global_git_config_values(&mut commands, git_program, TRACE2_EVENT_TARGET_KEY).ok()?;
    if !trace2_targets_are_stale_autter(&targets, &expected) {
        return None;
    }
    let set = |key: &str, value: &str| {
        std::process::Command::new(git_program)
            .args(["config", "--global", "--replace-all", key, value])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if set(TRACE2_EVENT_TARGET_KEY, &expected)
        && set(TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_NESTING_VALUE)
    {
        Some(format!(
            "repaired: {TRACE2_EVENT_TARGET_KEY} pointed at an old autter location ({}); now {expected}",
            targets.join(", ")
        ))
    } else {
        None
    }
}

/// When git's trace2 events already go to this machine's autter service, make sure
/// `trace2.envVars` also lists the background-agent detection variables, so the
/// service judges each commit by the shell that made it. Installs from before
/// per-commit detection lack them; this repairs them in place. Returns a note
/// when something changed.
pub fn repair_trace2_agent_env_vars(git_program: &str) -> Option<String> {
    let expected = crate::daemon::DaemonConfig::from_env_or_default_paths()
        .ok()?
        .trace2_event_target();
    let mut commands = Vec::new();
    let targets =
        read_global_git_config_values(&mut commands, git_program, TRACE2_EVENT_TARGET_KEY).ok()?;
    if !targets.iter().any(|t| t == &expected) {
        return None;
    }
    let existing =
        read_global_git_config_values(&mut commands, git_program, TRACE2_ENV_VARS_KEY).ok()?;
    let merged = crate::authorship::background_agent::merge_trace2_env_vars(&existing);
    if existing == [merged.clone()] {
        return None;
    }
    let ok = std::process::Command::new(git_program)
        .args([
            "config",
            "--global",
            "--replace-all",
            TRACE2_ENV_VARS_KEY,
            &merged,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    ok.then(|| {
        format!(
            "repaired: {TRACE2_ENV_VARS_KEY} now reports cloud-agent variables for each command, so a commit is only credited to an agent when it was made inside one"
        )
    })
}

/// True when every configured trace2 event target is an autter target, but
/// none is this machine's current one.
fn trace2_targets_are_stale_autter(targets: &[String], expected: &str) -> bool {
    !targets.is_empty()
        && !targets.iter().any(|t| t == expected)
        && targets
            .iter()
            .all(|t| t.to_ascii_lowercase().contains("autter"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_restarts_only_a_service_that_stays_unresponsive() {
        use crate::commands::daemon::DaemonHealthState::*;
        let window = Duration::from_secs(5);
        let secs = Duration::from_secs;

        // One failed probe on a loaded machine: wait and probe again.
        assert_eq!(
            service_verdict(&[Unresponsive], secs(0), window),
            ServiceVerdict::KeepProbing
        );
        // It answers on a later probe: slow, not stuck. No restart.
        assert_eq!(
            service_verdict(&[Unresponsive, Unresponsive, Healthy], secs(2), window),
            ServiceVerdict::Healthy
        );
        // Unresponsive for the whole window, but too few probes yet (each
        // probe waited its long budget): keep probing.
        assert_eq!(
            service_verdict(&[Unresponsive, Unresponsive], secs(6), window),
            ServiceVerdict::KeepProbing
        );
        // Enough probes, but not for long enough.
        assert_eq!(
            service_verdict(&[Unresponsive; 4], secs(3), window),
            ServiceVerdict::KeepProbing
        );
        // Stayed unresponsive: restart.
        assert_eq!(
            service_verdict(&[Unresponsive; 3], secs(5), window),
            ServiceVerdict::Restart
        );
        // Answered once and then went silent again: the silent streak starts
        // over, it does not count the earlier failures.
        assert_eq!(
            service_verdict(
                &[Unresponsive, Unresponsive, Healthy, Unresponsive],
                secs(9),
                window
            ),
            ServiceVerdict::KeepProbing
        );
        // A service that stopped is started; an outdated one is restarted at
        // once (waiting cannot make it current); a healthy one is left alone.
        assert_eq!(
            service_verdict(&[Unresponsive, NotRunning], secs(1), window),
            ServiceVerdict::Start
        );
        assert_eq!(
            service_verdict(&[NotRunning], secs(0), window),
            ServiceVerdict::Start
        );
        assert_eq!(
            service_verdict(&[Outdated], secs(0), window),
            ServiceVerdict::Restart
        );
        assert_eq!(
            service_verdict(&[Healthy], secs(0), window),
            ServiceVerdict::Healthy
        );
    }

    #[test]
    fn the_output_says_doctor_waited_and_why_it_restarted() {
        use crate::commands::daemon::DaemonHealthState::*;
        let confirmation = |initial, verdict| ServiceConfirmation {
            health: crate::commands::daemon::DaemonHealth {
                state: initial,
                control_socket: false,
                control_answers: false,
                trace_socket: false,
                lock_held: true,
                pid: None,
                binary_outdated: false,
                summary: String::new(),
                remediation: None,
            },
            initial,
            verdict,
            waited: Duration::from_millis(6200),
            probes: 5,
            probe_timeout: Duration::from_secs(2),
        };
        let restarted = confirmation(Unresponsive, ServiceVerdict::Restart)
            .explanation()
            .unwrap();
        assert!(restarted.contains("did not answer for 6.2s"), "{restarted}");
        assert!(restarted.contains("5 probes"), "{restarted}");
        assert!(restarted.contains("so doctor restarted it"), "{restarted}");
        let slow = confirmation(Unresponsive, ServiceVerdict::Healthy)
            .explanation()
            .unwrap();
        assert!(slow.contains("waited 6.2s"), "{slow}");
        assert!(slow.contains("did not restart it"), "{slow}");
        assert!(
            confirmation(Healthy, ServiceVerdict::Healthy)
                .explanation()
                .is_none()
        );
        assert!(
            confirmation(Outdated, ServiceVerdict::Restart)
                .explanation()
                .unwrap()
                .contains("older autter binary")
        );
    }

    // Windows: best guesses for reports that cannot be reproduced here; the
    // tests pin the decisions, not Windows behaviour.

    #[test]
    fn windows_gets_a_longer_trace2_probe_budget_on_top_of_machine_scaling() {
        let secs = Duration::from_secs;
        assert_eq!(trace2_ingestion_budget(false, false, 1.0), secs(3));
        assert_eq!(trace2_ingestion_budget(false, true, 1.0), secs(3));
        assert_eq!(trace2_ingestion_budget(true, false, 1.0), secs(10));
        assert_eq!(trace2_ingestion_budget(true, true, 1.0), secs(30));
        // Scaled for a slow machine, but capped.
        assert_eq!(trace2_ingestion_budget(true, false, 2.0), secs(20));
        assert_eq!(
            trace2_ingestion_budget(true, true, 10.0),
            INGESTION_BUDGET_MAX
        );
        // The retry is longer than the first try.
        assert_eq!(trace2_ingestion_retry_budget(secs(10)), secs(15));
        assert_eq!(
            trace2_ingestion_retry_budget(secs(50)),
            INGESTION_BUDGET_MAX
        );
    }

    fn probe_failure(git_secs: Option<f64>, seq: Option<u64>) -> IngestionProbeFailure {
        IngestionProbeFailure {
            error: "timed out".to_string(),
            git_elapsed: git_secs.map(Duration::from_secs_f64),
            waited_for_service: Duration::from_millis(2500),
            latest_seq: seq,
            ..IngestionProbeFailure::default()
        }
    }

    #[test]
    fn a_service_that_has_not_processed_its_first_event_is_not_a_failed_start() {
        use IngestionFailure::*;
        let no_event = probe_failure(Some(0.4), Some(0));
        // Up, answering, started moments ago, nothing recorded yet.
        assert_eq!(
            classify_ingestion_failure(&no_event, true, true),
            NotProcessedYet
        );
        // Same counts on a service that has run for a while: the event is
        // not arriving.
        assert_eq!(
            classify_ingestion_failure(&no_event, true, false),
            NotReceived
        );
        // Not answering: neither.
        assert_eq!(
            classify_ingestion_failure(&no_event, false, true),
            NotReceived
        );
        assert_eq!(
            classify_ingestion_failure(&probe_failure(Some(0.4), None), true, true),
            ServiceSilent
        );
        let mut slow_git = probe_failure(Some(10.0), None);
        slow_git.git_timed_out = true;
        assert_eq!(classify_ingestion_failure(&slow_git, true, true), GitSlow);
        assert_eq!(
            classify_ingestion_failure(&probe_failure(None, None), true, true),
            Setup
        );

        assert!(ingestion_failure_summary(NotProcessedYet).contains("is running"));
        let fix = trace2_ingestion_remediation(
            false,
            NotProcessedYet,
            &no_event,
            Duration::from_secs(30),
        );
        assert!(fix.contains("again in a minute"), "{fix}");
    }

    #[test]
    fn windows_trace2_remediation_says_what_was_measured_and_the_likely_causes() {
        let mut failure = probe_failure(Some(9.96), None);
        failure.git_timed_out = true;
        let fix = trace2_ingestion_remediation(
            true,
            IngestionFailure::GitSlow,
            &failure,
            Duration::from_secs(10),
        );
        assert!(fix.contains("git init ran 10.0s and was stopped"), "{fix}");
        assert!(fix.contains("budget 10.0s"), "{fix}");
        assert!(fix.contains("antivirus"), "{fix}");
        assert!(fix.contains("git.exe or autter.exe"), "{fix}");
        assert!(fix.contains("different user or elevation"), "{fix}");

        let fix = trace2_ingestion_remediation(
            true,
            IngestionFailure::NotReceived,
            &probe_failure(Some(1.3), Some(0)),
            Duration::from_secs(10),
        );
        assert!(fix.contains("git init took 1.3s"), "{fix}");
        assert!(
            fix.contains("recorded 0 event(s) for it after 2.5s"),
            "{fix}"
        );

        // Elsewhere the old advice stays, with the measurement added.
        let fix = trace2_ingestion_remediation(
            false,
            IngestionFailure::NotReceived,
            &probe_failure(Some(0.1), Some(0)),
            Duration::from_secs(3),
        );
        assert!(fix.starts_with(REMEDIATION_TRACE2_INGESTION), "{fix}");
        assert!(!fix.contains("antivirus"), "{fix}");
    }

    #[test]
    fn daemon_remediation_names_the_windows_cause_when_the_fix_attempt_shows_one() {
        let restart = REMEDIATION_DAEMON_FAILED;
        let antivirus = vec![
            "initial state: not running".to_string(),
            "doctor fix: starting the background service".to_string(),
            "fix failed: failed to spawn detached daemon with flags 0x1000208: (os error 225)"
                .to_string(),
            "state after fix: not running".to_string(),
        ];
        let fix = daemon_failure_remediation(true, &antivirus);
        assert!(fix.starts_with("Windows Security"), "{fix}");
        assert!(fix.contains("Protection history"), "{fix}");
        assert!(fix.ends_with("then re-run `autter doctor`"), "{fix}");
        assert_ne!(fix, restart);

        let elevated = vec![
            "doctor fix: restarting the background service".to_string(),
            "fix failed: taskkill /F /T /PID 4312 failed: ERROR: The process could not be terminated.\nReason: Access is denied.".to_string(),
        ];
        let fix = daemon_failure_remediation(true, &elevated);
        assert!(fix.starts_with("Windows denied access"), "{fix}");
        assert!(fix.contains("Run as administrator"), "{fix}");
        assert_ne!(fix, restart);

        // No recognised cause, or not Windows: the restart advice stands.
        let timeout = vec!["fix failed: timed out after 5s waiting for daemon sockets".to_string()];
        assert_eq!(daemon_failure_remediation(true, &timeout), restart);
        assert_eq!(daemon_failure_remediation(false, &antivirus), restart);
        assert_eq!(daemon_failure_remediation(true, &[]), restart);
    }

    #[test]
    fn check_budgets_grow_with_how_slowly_the_machine_runs_git() {
        // A machine that starts git in a few milliseconds keeps the base budgets.
        assert_eq!(timeout_scale_for(Duration::from_millis(8)), 1.0);
        assert_eq!(timeout_scale_for(Duration::from_millis(100)), 1.0);
        // One where `git --version` takes 400 ms gets four times as long: the
        // 3 s checks get 12 s, enough for a commit that takes 4.8 s.
        let slow = timeout_scale_for(Duration::from_millis(400));
        assert_eq!(slow, 4.0);
        assert_eq!(
            scale_timeout(Duration::from_secs(3), slow),
            Duration::from_secs(12)
        );
        // Capped, so a hung git cannot make doctor wait for minutes.
        assert_eq!(
            timeout_scale_for(Duration::from_secs(30)),
            TIMEOUT_SCALE_MAX
        );
        assert_eq!(
            scale_timeout(Duration::from_secs(3), 500.0),
            Duration::from_secs(30)
        );
        // A budget is never shortened.
        assert_eq!(
            scale_timeout(Duration::from_secs(3), 0.2),
            Duration::from_secs(3)
        );

        // The environment can set the scale; nonsense is ignored.
        assert_eq!(timeout_scale_from_env(Some("4")), Some(4.0));
        assert_eq!(timeout_scale_from_env(Some(" 2.5 ")), Some(2.5));
        assert_eq!(timeout_scale_from_env(Some("50")), Some(TIMEOUT_SCALE_MAX));
        for bad in ["0", "0.5", "-3", "fast", "", "NaN", "inf"] {
            assert_eq!(timeout_scale_from_env(Some(bad)), None, "{bad}");
        }
        assert_eq!(timeout_scale_from_env(None), None);
    }

    /// How long the partial-output tests let `stdout_stderr_sleep_command`
    /// run before timing it out. It must outlast the shell's startup so
    /// "out" and "err" are written first: `sh` starts in milliseconds, but a
    /// cold `powershell.exe` on a busy Windows CI runner can take over a
    /// second, and with 300 ms the test failed whenever it had not printed
    /// yet. The timeout ends the 60 s sleep either way.
    #[cfg(not(windows))]
    const PARTIAL_OUTPUT_TIMEOUT: Duration = Duration::from_millis(300);
    #[cfg(windows)]
    const PARTIAL_OUTPUT_TIMEOUT: Duration = Duration::from_secs(10);

    #[cfg(not(windows))]
    fn stdout_stderr_sleep_command() -> (&'static str, Vec<&'static str>) {
        (
            "sh",
            vec!["-c", "printf out; printf err >&2; exec sleep 60"],
        )
    }

    #[cfg(windows)]
    fn stdout_stderr_sleep_command() -> (&'static str, Vec<&'static str>) {
        (
            "powershell.exe",
            vec![
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write('out'); [Console]::Error.Write('err'); Start-Sleep -Seconds 60",
            ],
        )
    }

    #[test]
    fn test_validate_trace2_command_events_accepts_expected_events() {
        let trace = r#"{"event":"version"}
{"event":"start","argv":["git","init","."]}
{"event":"cmd_name","name":"init"}
{"event":"exit","code":0}
{"event":"atexit","code":0}
"#;

        let details = validate_trace2_command_events(trace, "init").unwrap();
        assert!(details.iter().any(|detail| detail == "events: 5"));
    }

    #[test]
    fn test_validate_trace2_command_events_rejects_missing_cmd_name() {
        let trace = r#"{"event":"version"}
{"event":"start","argv":["git","init","."]}
{"event":"exit","code":0}
{"event":"atexit","code":0}
"#;

        let err = validate_trace2_command_events(trace, "init").unwrap_err();
        assert!(err.contains("missing cmd_name event for expected command"));
    }

    #[test]
    fn test_trace2_config_failure_details_explains_missing_config() {
        let empty = Vec::new();
        let details = trace2_config_failure_details(
            "trace2 is not configured for autter daemon mode",
            Some("af_unix:stream:/tmp/autter-trace2.sock"),
            Some(&empty),
            Some(&empty),
        );

        assert!(details[0].contains("ERROR: trace2 is not configured"));
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("Why this matters"))
        );
        assert!(
            details
                .iter()
                .any(|detail| detail == "Actual trace2.eventTarget: <missing>")
        );
        assert!(
            details
                .iter()
                .any(|detail| detail == "Actual trace2.eventNesting: <missing>")
        );
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("Common causes"))
        );
    }

    #[test]
    fn test_run_logged_command_with_timeout_reports_partial_output() {
        let (program, args) = stdout_stderr_sleep_command();
        let record = run_logged_command_with_timeout(program, &args, None, PARTIAL_OUTPUT_TIMEOUT);

        assert!(record.timed_out, "{record:?}");
        assert_eq!(record.stdout, "out");
        assert!(record.stderr.contains("timed out after"), "{record:?}");
        assert!(
            record.stderr.contains("sent kill to child process")
                || record.stderr.contains("failed to kill child process"),
            "{record:?}"
        );
        assert!(
            record.stderr.contains("stderr before timeout"),
            "{record:?}"
        );
        assert!(record.stderr.contains("err"), "{record:?}");
    }

    #[test]
    fn attribution_remediation_maps_checkpoint_persistence() {
        let msg = attribution_self_check_remediation(
            "timed out after 3.0s waiting for checkpoint persistence: only 0 checkpoint(s) visible, expected at least 1",
            Some("daemon status for repo: latest_seq=0, last_error=<none>"),
        );
        assert!(
            msg.contains("likely cause: checkpoints are not persisting"),
            "{msg}"
        );
        assert!(msg.contains("autter bg restart"), "{msg}");
    }

    #[test]
    fn attribution_remediation_maps_untracked_after_commit() {
        let msg = attribution_self_check_remediation(
            "timed out after 1.2s waiting for expected attribution via notes backend for abc in /tmp/x: unexpected attribution for line 2: got untracked, expected known_human",
            None,
        );
        assert!(msg.contains("authorship notes were not written"), "{msg}");
        assert!(msg.contains("autter install"), "{msg}");
    }

    #[test]
    fn attribution_remediation_includes_daemon_last_error() {
        let msg = attribution_self_check_remediation(
            "timed out after 3.0s waiting for checkpoint persistence: only 0 checkpoint(s) visible, expected at least 1",
            Some("daemon status for repo: latest_seq=0, last_error=socket closed"),
        );
        assert!(msg.contains("daemon last_error: socket closed"), "{msg}");
    }

    #[test]
    fn attribution_remediation_falls_back_for_unknown_errors() {
        let msg = attribution_self_check_remediation("something unexpected went wrong", None);
        assert_eq!(msg, REMEDIATION_ATTRIBUTION_FALLBACK);
    }

    #[test]
    fn stale_trace2_repair_only_touches_old_autter_targets() {
        let expected = "af_unix:stream:/home/me/.autter/internal/daemon/trace2.sock";
        let old = "af_unix:stream:/Users/old/.autter/internal/daemon/trace2.sock".to_string();
        assert!(trace2_targets_are_stale_autter(
            std::slice::from_ref(&old),
            expected
        ));
        // Nothing configured: needs consent, not a silent write.
        assert!(!trace2_targets_are_stale_autter(&[], expected));
        // Already right.
        assert!(!trace2_targets_are_stale_autter(
            &[expected.to_string()],
            expected
        ));
        // Someone else's target is never replaced.
        assert!(!trace2_targets_are_stale_autter(
            &["/tmp/my-own-trace.json".to_string()],
            expected
        ));
        assert!(!trace2_targets_are_stale_autter(
            &[old, "/tmp/my-own-trace.json".to_string()],
            expected
        ));
    }

    #[test]
    fn trace2_remediation_never_sends_people_back_to_onboard() {
        assert!(!REMEDIATION_TRACE2_CONFIG_MISSING.contains("onboard"));
        assert!(REMEDIATION_TRACE2_CONFIG_MISSING.contains("autter install --system"));
    }
}
