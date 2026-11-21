//! Recognises the four Autter key types from their prefix, so a wrong key can
//! be refused locally with a message naming the right one.
//!
//! The copy mirrors the other Autter components (the runtime SDKs and the
//! ingester), so a user sees the same wording wherever they paste a key.

/// Prefix marking a Personal Access Token (mirrors the backend).
pub const PAT_PREFIX: &str = "autter_pat_";
const SERVER_PREFIX: &str = "autter_rt_";
const CLIENT_PREFIX: &str = "autter_rtc_";
const UPLOAD_PREFIX: &str = "autter_rtu_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// Personal access token (`autter_pat_`): CLI and MCP sign-in.
    Pat,
    /// Runtime server key (`autter_rt_`): backend telemetry.
    Server,
    /// Runtime client key (`autter_rtc_`): browser errors.
    Client,
    /// Source map upload key (`autter_rtu_`): CI source map uploads.
    Upload,
    /// No known prefix (self-hosted keys may have any shape).
    Unknown,
}

impl KeyKind {
    /// True for the three runtime key kinds.
    pub fn is_runtime(self) -> bool {
        matches!(self, Self::Server | Self::Client | Self::Upload)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProblem {
    Quoted,
    Bearer,
    Assignment,
    Masked,
    Whitespace,
}

impl KeyProblem {
    /// Problems that make the value unusable even after cleaning.
    pub fn is_fatal(self) -> bool {
        matches!(self, Self::Masked | Self::Whitespace)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedKey {
    pub kind: KeyKind,
    /// The value with quotes, a `Bearer ` prefix and a `NAME=` prefix removed.
    pub key: String,
    pub problem: Option<KeyProblem>,
}

fn kind_of(key: &str) -> KeyKind {
    // `autter_rtc_` / `autter_rtu_` are checked before `autter_rt_` so the
    // prefixes stay unambiguous even if one ever becomes a prefix of another.
    if key.starts_with(CLIENT_PREFIX) {
        KeyKind::Client
    } else if key.starts_with(UPLOAD_PREFIX) {
        KeyKind::Upload
    } else if key.starts_with(SERVER_PREFIX) {
        KeyKind::Server
    } else if key.starts_with(PAT_PREFIX) {
        KeyKind::Pat
    } else {
        KeyKind::Unknown
    }
}

fn strip_bearer(s: &str) -> Option<&str> {
    let head = s.get(..7)?;
    head.eq_ignore_ascii_case("bearer ").then(|| &s[7..])
}

fn strip_assignment(s: &str) -> Option<&str> {
    let (name, rest) = s.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    let valid = first.is_ascii_uppercase()
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    valid.then_some(rest)
}

fn strip_quotes(s: &str) -> Option<&str> {
    let first = s.chars().next()?;
    if s.len() >= 2 && matches!(first, '"' | '\'' | '`') && s.ends_with(first) {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

fn is_masked(s: &str) -> bool {
    s.contains('\u{2026}') || s.contains("...") || s.contains('*') || s.contains('\u{2022}')
}

/// Clean a pasted key and say what kind it is and what was wrong with it.
pub fn classify_key(raw: &str) -> ClassifiedKey {
    let mut s = raw.trim();
    let (mut quoted, mut bearer, mut assignment) = (false, false, false);
    // A few passes cover wrappers in any order, e.g. `NAME="Bearer x"`.
    for _ in 0..3 {
        let before = s;
        if let Some(rest) = strip_quotes(s) {
            quoted = true;
            s = rest.trim();
        }
        if let Some(rest) = strip_bearer(s) {
            bearer = true;
            s = rest.trim();
        }
        if let Some(rest) = strip_assignment(s) {
            assignment = true;
            s = rest.trim();
        }
        if s == before {
            break;
        }
    }
    let problem = if quoted {
        Some(KeyProblem::Quoted)
    } else if bearer {
        Some(KeyProblem::Bearer)
    } else if assignment {
        Some(KeyProblem::Assignment)
    } else if is_masked(s) {
        Some(KeyProblem::Masked)
    } else if s.chars().any(char::is_whitespace) {
        Some(KeyProblem::Whitespace)
    } else {
        None
    };
    // A masked key is still a masked key when it also had quotes; the fatal
    // problem wins so it is never silently accepted.
    let problem = match problem {
        Some(p) if !p.is_fatal() && is_masked(s) => Some(KeyProblem::Masked),
        Some(p) if !p.is_fatal() && s.chars().any(char::is_whitespace) => {
            Some(KeyProblem::Whitespace)
        }
        other => other,
    };
    ClassifiedKey {
        kind: kind_of(s),
        key: s.to_string(),
        problem,
    }
}

fn got_sentence(got: KeyKind) -> &'static str {
    match got {
        KeyKind::Pat => {
            "This is a personal access token (autter_pat_\u{2026}), for signing in the Autter CLI and MCP."
        }
        KeyKind::Server => {
            "This is a runtime server key (autter_rt_\u{2026}), for backend telemetry."
        }
        KeyKind::Client => {
            "This is a runtime client key (autter_rtc_\u{2026}), for browser errors."
        }
        KeyKind::Upload => {
            "This is a source map upload key (autter_rtu_\u{2026}), for CI source map uploads."
        }
        KeyKind::Unknown => "",
    }
}

fn use_sentence(expected: KeyKind) -> &'static str {
    match expected {
        KeyKind::Pat => {
            "To sign in the CLI, use a personal access token (autter_pat_\u{2026}) with `autter login --token <token>`, or run `autter login` to sign in in the browser."
        }
        KeyKind::Server => {
            "Backend telemetry needs a runtime server key (autter_rt_\u{2026}) in AUTTER_RUNTIME_KEY."
        }
        KeyKind::Client => {
            "Browser errors need a runtime client key (autter_rtc_\u{2026}) in initAutterBrowser({ clientKey })."
        }
        KeyKind::Upload => {
            "Source map uploads need a source map upload key (autter_rtu_\u{2026}) as the AUTTER_RUNTIME_KEY CI secret (a runtime server key also works)."
        }
        KeyKind::Unknown => "",
    }
}

fn where_sentence(web: &str) -> String {
    format!("Create one in the Autter dashboard ({web}) at Settings > Keys and tokens.")
}

/// What the user passed, what they need instead, and where to get it.
pub fn wrong_key_message(expected: KeyKind, got: KeyKind, web: &str) -> String {
    let mut parts = vec![
        got_sentence(got).to_string(),
        use_sentence(expected).to_string(),
    ];
    if expected == KeyKind::Client && got == KeyKind::Server {
        parts.push(
            "A server key is secret: never put it in browser code, and rotate this one."
                .to_string(),
        );
    }
    parts.push(where_sentence(web));
    parts.retain(|p| !p.is_empty());
    parts.join(" ")
}

/// Shown when a personal access token only carries MCP scopes.
pub fn mcp_only_pat_message(web: &str) -> String {
    format!(
        "This personal access token only has MCP scopes, so the CLI cannot use it. Create one without scope restrictions in the Autter dashboard ({web}) at Settings > Keys and tokens, then run `autter login --token <token>`."
    )
}

pub fn key_problem_message(problem: KeyProblem) -> &'static str {
    match problem {
        KeyProblem::Quoted => {
            "The key is wrapped in quotes. Remove them: env files and CI secrets keep quotes as part of the value."
        }
        KeyProblem::Bearer => {
            "The key starts with \"Bearer \". Set only the key; the SDK adds the Bearer prefix."
        }
        KeyProblem::Assignment => "The value includes a variable name (NAME=). Set only the key.",
        KeyProblem::Masked => {
            "This is the shortened key shown in the dashboard list, not the key. A key is shown in full only once, when it is created: create a new one at Settings > Keys and tokens."
        }
        KeyProblem::Whitespace => {
            "The key contains spaces or line breaks. Copy it again as one line."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(raw: &str) -> KeyKind {
        classify_key(raw).kind
    }

    #[test]
    fn kinds_by_prefix() {
        assert_eq!(kind("autter_pat_abc"), KeyKind::Pat);
        assert_eq!(kind("autter_rt_abc"), KeyKind::Server);
        assert_eq!(kind("autter_rtc_abc"), KeyKind::Client);
        assert_eq!(kind("autter_rtu_abc"), KeyKind::Upload);
        assert_eq!(kind("something_else"), KeyKind::Unknown);
        assert_eq!(kind(""), KeyKind::Unknown);
    }

    #[test]
    fn rtc_is_not_mistaken_for_rt() {
        assert_eq!(kind("autter_rtc_x"), KeyKind::Client);
        assert_eq!(kind("autter_rt_cx"), KeyKind::Server);
    }

    #[test]
    fn cleaning_table() {
        let cases: &[(&str, &str, Option<KeyProblem>)] = &[
            ("autter_pat_abc", "autter_pat_abc", None),
            ("  autter_pat_abc\n", "autter_pat_abc", None),
            (
                "\"autter_pat_abc\"",
                "autter_pat_abc",
                Some(KeyProblem::Quoted),
            ),
            (
                "'autter_pat_abc'",
                "autter_pat_abc",
                Some(KeyProblem::Quoted),
            ),
            (
                "`autter_pat_abc`",
                "autter_pat_abc",
                Some(KeyProblem::Quoted),
            ),
            (
                "Bearer autter_pat_abc",
                "autter_pat_abc",
                Some(KeyProblem::Bearer),
            ),
            (
                "bearer autter_pat_abc",
                "autter_pat_abc",
                Some(KeyProblem::Bearer),
            ),
            (
                "AUTTER_API_KEY=autter_pat_abc",
                "autter_pat_abc",
                Some(KeyProblem::Assignment),
            ),
            (
                "AUTTER_API_KEY=\"autter_pat_abc\"",
                "autter_pat_abc",
                Some(KeyProblem::Quoted),
            ),
            (
                "autter_pat_AbCd\u{2026}",
                "autter_pat_AbCd\u{2026}",
                Some(KeyProblem::Masked),
            ),
            (
                "autter_pat_AbCd...",
                "autter_pat_AbCd...",
                Some(KeyProblem::Masked),
            ),
            (
                "autter_pat_****",
                "autter_pat_****",
                Some(KeyProblem::Masked),
            ),
            (
                "autter_pat_ab cd",
                "autter_pat_ab cd",
                Some(KeyProblem::Whitespace),
            ),
            (
                "autter_pat_ab\ncd",
                "autter_pat_ab\ncd",
                Some(KeyProblem::Whitespace),
            ),
        ];
        for (raw, key, problem) in cases {
            let got = classify_key(raw);
            assert_eq!(&got.key, key, "{raw:?}");
            assert_eq!(&got.problem, problem, "{raw:?}");
        }
    }

    #[test]
    fn a_masked_key_stays_fatal_when_quoted() {
        let got = classify_key("\"autter_pat_AbCd\u{2026}\"");
        assert_eq!(got.problem, Some(KeyProblem::Masked));
    }

    #[test]
    fn wrong_key_message_names_the_right_key_and_where() {
        let web = "https://app.example.test";
        assert_eq!(
            wrong_key_message(KeyKind::Pat, KeyKind::Server, web),
            "This is a runtime server key (autter_rt_\u{2026}), for backend telemetry. To sign in the CLI, use a personal access token (autter_pat_\u{2026}) with `autter login --token <token>`, or run `autter login` to sign in in the browser. Create one in the Autter dashboard (https://app.example.test) at Settings > Keys and tokens."
        );
    }

    #[test]
    fn server_key_where_client_expected_says_rotate() {
        let text = wrong_key_message(KeyKind::Client, KeyKind::Server, "https://w");
        assert!(text.contains("never put it in browser code, and rotate this one."));
    }
}
