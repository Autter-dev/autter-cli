//! The machine-readable contract of the reporting commands (`stats`, `status`,
//! `log`, `show`, `blame`, `diff`, `whoami`, `doctor`, `sync status`).
//!
//! * With `--json`, stdout carries exactly one JSON document and nothing else;
//!   diagnostics and warnings go to stderr.
//! * Every document has a top-level `schema_version` ([`SCHEMA_VERSION`]).
//!   Fields are only ever added; an incompatible change bumps the version.
//! * A failure under `--json` still prints one document, with an `error`
//!   object (`code`, `message`, `exit_code`), and exits non-zero. When the
//!   command got far enough to produce its report (for example
//!   `stats --strict` on a commit with nothing captured), the report fields
//!   are kept next to `error`.
//! * Exit codes: [`EXIT_SUCCESS`](super::EXIT_SUCCESS) `0`,
//!   [`EXIT_RUNTIME_ERROR`](super::EXIT_RUNTIME_ERROR) `1`,
//!   [`EXIT_USAGE_ERROR`](super::EXIT_USAGE_ERROR) `2` (bad flags, outside a
//!   repository), [`EXIT_NOTHING_CAPTURED`] `3`, [`EXIT_CAPTURE_BROKEN`] `4`
//!   and [`EXIT_INTEGRITY_FAILED`] `5`. `3` and `4` only happen with
//!   `--strict` (or `AUTTER_STRICT=1`); `5` only from `verify`.
//!
//! The JSON Schemas for these documents live in `docs/schemas/` and the
//! integration test `json_contract.rs` checks every command against them.

use crate::commands::arg_parser;
use crate::error::AutterError;
use serde::Serialize;

/// Version of the JSON documents printed by `--json`. Bump only for a change
/// that breaks a reader (a removed or retyped field); added fields keep it.
pub const SCHEMA_VERSION: u32 = 1;

/// `--strict`: the command ran, and Autter has no record of the lines added
/// in what it looked at (`stats`, `status`): capture missed them. A commit
/// with a record and no AI lines is not this; it exits 0.
pub const EXIT_NOTHING_CAPTURED: i32 = 3;

/// `--strict`: capture itself is broken (`doctor`: a background service,
/// trace2, end-to-end checkpoint or agent hook check failed).
pub const EXIT_CAPTURE_BROKEN: i32 = 4;

/// `verify`: a note does not belong to its commit or was changed after
/// autter wrote it (with or without `--strict`).
pub const EXIT_INTEGRITY_FAILED: i32 = 5;

/// Why a reporting command failed. Each kind has a stable `code` string for
/// the JSON error object and a fixed process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Bad flags or arguments.
    Usage,
    /// Run outside a git repository.
    NotARepository,
    /// A commit, revision or range that does not resolve.
    RevisionNotFound,
    /// The background service could not be reached, so the command did not
    /// run.
    ServiceUnreachable,
    /// `stats --wait` gave up before the authorship note was written.
    NotePending,
    /// Not signed in (`whoami`).
    NotSignedIn,
    /// Cloud upload needs attention (`sync status`).
    SyncUnhealthy,
    /// A setup check failed that does not break capture (`doctor`).
    CheckFailed,
    /// `--strict`: nothing was captured (no Autter record of the added lines).
    NothingCaptured,
    /// `--strict`: capture is broken.
    CaptureBroken,
    /// `verify`: a note failed the integrity check.
    IntegrityFailed,
    /// Anything else that went wrong at runtime.
    Runtime,
}

impl ErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            ErrorKind::Usage => "usage_error",
            ErrorKind::NotARepository => "not_a_repository",
            ErrorKind::RevisionNotFound => "revision_not_found",
            ErrorKind::ServiceUnreachable => "service_unreachable",
            ErrorKind::NotePending => "note_pending",
            ErrorKind::NotSignedIn => "not_signed_in",
            ErrorKind::SyncUnhealthy => "sync_unhealthy",
            ErrorKind::CheckFailed => "check_failed",
            ErrorKind::NothingCaptured => "nothing_captured",
            ErrorKind::CaptureBroken => "capture_broken",
            ErrorKind::IntegrityFailed => "integrity_failed",
            ErrorKind::Runtime => "runtime_error",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::Usage | ErrorKind::NotARepository => super::EXIT_USAGE_ERROR,
            ErrorKind::NothingCaptured => EXIT_NOTHING_CAPTURED,
            ErrorKind::CaptureBroken => EXIT_CAPTURE_BROKEN,
            ErrorKind::IntegrityFailed => EXIT_INTEGRITY_FAILED,
            ErrorKind::RevisionNotFound
            | ErrorKind::ServiceUnreachable
            | ErrorKind::NotePending
            | ErrorKind::NotSignedIn
            | ErrorKind::SyncUnhealthy
            | ErrorKind::CheckFailed
            | ErrorKind::Runtime => super::EXIT_RUNTIME_ERROR,
        }
    }
}

/// The `error` object of a failed `--json` document.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
    pub exit_code: i32,
}

impl ErrorBody {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            code: kind.code(),
            message: message.into(),
            exit_code: kind.exit_code(),
        }
    }
}

/// One `--json` document: `schema_version`, the command's own fields, and an
/// `error` object when it failed.
#[derive(Serialize)]
struct Document<'a, T: Serialize + ?Sized> {
    schema_version: u32,
    #[serde(flatten)]
    payload: Option<&'a T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a ErrorBody>,
}

/// Serialize `payload` (a struct or map) as one `--json` line.
pub fn document<T: Serialize + ?Sized>(
    payload: Option<&T>,
    error: Option<&ErrorBody>,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&Document {
        schema_version: SCHEMA_VERSION,
        payload,
        error,
    })
}

/// Same document, indented (for `blame --json`, which has always been
/// pretty-printed).
pub fn document_pretty<T: Serialize + ?Sized>(payload: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&Document {
        schema_version: SCHEMA_VERSION,
        payload: Some(payload),
        error: None,
    })
}

/// Print `payload` as the command's `--json` document.
pub fn print_json<T: Serialize + ?Sized>(payload: &T) {
    match document(Some(payload), None) {
        Ok(line) => println!("{line}"),
        Err(e) => fail(
            ErrorKind::Runtime,
            format!("failed to serialize JSON output: {e}"),
        ),
    }
}

/// Exit with `kind`'s code. Under `--json`, print `{schema_version, error}` on
/// stdout; the message also goes to stderr so it shows in CI logs.
pub fn fail(kind: ErrorKind, message: impl AsRef<str>) -> ! {
    fail_with::<()>(kind, message, None)
}

/// [`fail`], keeping the report the command already built next to `error` in
/// the JSON document. Without `--json` the report is not printed here (the
/// caller has printed its human output already, if any).
pub fn fail_with<T: Serialize + ?Sized>(
    kind: ErrorKind,
    message: impl AsRef<str>,
    payload: Option<&T>,
) -> ! {
    let message = message.as_ref();
    eprintln!("{message}");
    print_error_document(kind, message, payload);
    std::process::exit(kind.exit_code())
}

/// [`fail`] for a caller that has already written its message (and any hint)
/// to stderr: under `--json` print the error document, then exit.
pub fn fail_reported(kind: ErrorKind, message: impl AsRef<str>) -> ! {
    print_error_document::<()>(kind, message.as_ref(), None);
    std::process::exit(kind.exit_code())
}

/// Under `--json`, print the one document of a failed command on stdout.
fn print_error_document<T: Serialize + ?Sized>(
    kind: ErrorKind,
    message: &str,
    payload: Option<&T>,
) {
    if !arg_parser::json() {
        return;
    }
    let body = ErrorBody::new(kind, first_line(message));
    match document(payload, Some(&body)) {
        Ok(line) => println!("{line}"),
        // The payload is what failed to serialize: the error alone still can.
        Err(_) => {
            if let Ok(line) = document::<()>(None, Some(&body)) {
                println!("{line}");
            }
        }
    }
}

/// Classify a failed git invocation by what git said: an option git does not
/// know is a usage error, a revision or range that does not resolve is
/// `revision_not_found`, anything else is a runtime error.
pub fn git_error_kind(stderr: &str) -> ErrorKind {
    let text = stderr.to_ascii_lowercase();
    if text.contains("unrecognized argument")
        || text.contains("unknown option")
        || text.contains("unknown switch")
        || text.contains("invalid option")
        || text.contains("not an integer")
        || text.contains("requires a value")
        || text.contains("requires an argument")
        || text.contains("usage: git")
    {
        ErrorKind::Usage
    } else if text.contains("unknown revision")
        || text.contains("bad revision")
        || text.contains("ambiguous argument")
        || text.contains("not a valid object name")
        || text.contains("needed a single revision")
        || text.contains("invalid revision range")
        || text.contains("cannot find revision")
        || text.contains("no commit found")
    {
        ErrorKind::RevisionNotFound
    } else {
        ErrorKind::Runtime
    }
}

/// Whether an error from a git helper says a revision or range does not
/// resolve; anything else it reports is a runtime error. (Bad options are
/// rejected by each command's own parser before git runs.)
pub fn error_kind(err: &AutterError) -> ErrorKind {
    let text = match err {
        AutterError::GitCliError { stderr, .. } => stderr.clone(),
        other => other.to_string(),
    };
    if git_error_kind(&text) == ErrorKind::RevisionNotFound {
        ErrorKind::RevisionNotFound
    } else {
        ErrorKind::Runtime
    }
}

/// The first line of an error message, without the `error: ` prefix human
/// output uses. Help text that follows a usage error stays on stderr.
fn first_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or(message).trim();
    line.strip_prefix("error: ")
        .or_else(|| line.strip_prefix("Error: "))
        .unwrap_or(line)
        .to_string()
}

/// Classify a repository-lookup error.
pub fn repository_error_kind(err: &AutterError) -> ErrorKind {
    let text = err.to_string().to_ascii_lowercase();
    if text.contains("not a git repository") || text.contains("not in a git repository") {
        ErrorKind::NotARepository
    } else {
        ErrorKind::Runtime
    }
}

/// `--strict` was given, or `AUTTER_STRICT` is set to a true value.
pub fn strict() -> bool {
    arg_parser::strict_flag() || env_flag_on("AUTTER_STRICT")
}

/// Running in CI: the `CI` environment variable is set to anything but an
/// explicit false (`CI=true` on GitHub Actions, GitLab, CircleCI, Buildkite…).
/// CI mode turns off color, prompts, spinners, full-screen views and the
/// update prompt, as if no terminal were attached.
pub fn ci_mode() -> bool {
    ci_value_means_ci(std::env::var_os("CI").as_deref())
}

fn ci_value_means_ci(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| {
        let value = value.to_string_lossy().trim().to_ascii_lowercase();
        !matches!(value.as_str(), "0" | "false" | "no" | "off")
    })
}

fn env_flag_on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Percentages with one decimal, so `ai_percent > 50` reads naturally in jq.
pub fn percent(part: u32, total: u32) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (f64::from(part) / f64::from(total) * 1000.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Report {
        commit: &'static str,
        lines: u32,
    }

    #[test]
    fn document_puts_schema_version_first_and_flattens_the_payload() {
        let line = document(
            Some(&Report {
                commit: "abc",
                lines: 3,
            }),
            None,
        )
        .unwrap();
        assert_eq!(line, r#"{"schema_version":1,"commit":"abc","lines":3}"#);
    }

    #[test]
    fn error_document_keeps_the_report_next_to_error() {
        let body = ErrorBody::new(ErrorKind::NothingCaptured, "nothing captured");
        let line = document(
            Some(&Report {
                commit: "abc",
                lines: 3,
            }),
            Some(&body),
        )
        .unwrap();
        assert_eq!(
            line,
            r#"{"schema_version":1,"commit":"abc","lines":3,"error":{"code":"nothing_captured","message":"nothing captured","exit_code":3}}"#
        );
        let bare = document::<()>(None, Some(&body)).unwrap();
        assert_eq!(
            bare,
            r#"{"schema_version":1,"error":{"code":"nothing_captured","message":"nothing captured","exit_code":3}}"#
        );
    }

    #[test]
    fn exit_codes_match_the_documented_table() {
        assert_eq!(ErrorKind::Usage.exit_code(), 2);
        assert_eq!(ErrorKind::NotARepository.exit_code(), 2);
        assert_eq!(ErrorKind::Runtime.exit_code(), 1);
        assert_eq!(ErrorKind::RevisionNotFound.exit_code(), 1);
        assert_eq!(ErrorKind::ServiceUnreachable.exit_code(), 1);
        assert_eq!(ErrorKind::NothingCaptured.exit_code(), 3);
        assert_eq!(ErrorKind::CaptureBroken.exit_code(), 4);
    }

    #[test]
    fn git_errors_are_classified_by_what_git_said() {
        for (stderr, kind) in [
            ("fatal: unrecognized argument: --bogus", ErrorKind::Usage),
            (
                "error: unknown option `bogus'\nusage: git blame",
                ErrorKind::Usage,
            ),
            (
                "fatal: ambiguous argument 'nosuchrev': unknown revision or path not in the working tree.",
                ErrorKind::RevisionNotFound,
            ),
            (
                "fatal: bad revision 'nosuchrev..HEAD'",
                ErrorKind::RevisionNotFound,
            ),
            (
                "fatal: Not a valid object name nosuchrev",
                ErrorKind::RevisionNotFound,
            ),
            ("fatal: unable to read tree", ErrorKind::Runtime),
        ] {
            assert_eq!(git_error_kind(stderr), kind, "{stderr}");
        }
    }

    #[test]
    fn first_line_drops_prefix_and_help_text() {
        assert_eq!(
            first_line("error: unknown option '--x' for 'autter show'\nRun 'autter show --help'"),
            "unknown option '--x' for 'autter show'"
        );
    }

    #[test]
    fn ci_variable_values() {
        use std::ffi::OsStr;
        assert!(!ci_value_means_ci(None));
        for on in ["true", "1", "yes", "TRUE", ""] {
            assert!(ci_value_means_ci(Some(OsStr::new(on))), "{on:?}");
        }
        for off in ["false", "0", "no", "off", " False "] {
            assert!(!ci_value_means_ci(Some(OsStr::new(off))), "{off:?}");
        }
    }

    #[test]
    fn percent_rounds_to_one_decimal() {
        assert_eq!(percent(1, 3), 33.3);
        assert_eq!(percent(2, 3), 66.7);
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(5, 5), 100.0);
    }
}
