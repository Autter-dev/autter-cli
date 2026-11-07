use crate::authorship::authorship_log::{LineRange, SessionRecord};
use crate::authorship::authorship_log_serialization::{
    AttestationEntry, AuthorshipLog, generate_session_id, generate_trace_id,
};
use crate::authorship::working_log::AgentId;
use std::collections::{BTreeMap, HashMap, HashSet};

const DEVIN_ID_PATH: &str = "/opt/.devin/devin_id";
const DEVIN_DIR_PATH: &str = "/opt/.devin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundAgent {
    WithHooks { tool: String },
    NoHooks { tool: String, id: String },
    None,
}

/// Environment variables that background-agent detection looks at.
///
/// Detection must use the environment of the git command that made the commit, not the
/// environment of the long-lived background service: the service is started once and
/// keeps whatever environment it was launched with, so a single stray variable there
/// would otherwise mark every commit on the machine as written by an agent. The service
/// asks git to report these exact names for each command (trace2 `envVars`), which is
/// why this is an explicit list and not a prefix match.
pub const DETECTION_ENV_VARS: &[&str] = &[
    "CLAUDE_CODE_REMOTE",
    "HOSTNAME",
    "CURSOR_AGENT",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "CODEX_THREAD_ID",
    "AUTTER_CLOUD_AGENT",
];

/// The trace2 `envVars` value: every detection variable plus the wrapper invocation id
/// that the wrapper already forwards.
pub fn trace2_env_vars_value() -> String {
    std::iter::once("AUTTER_WRAPPER_INVOCATION_ID")
        .chain(DETECTION_ENV_VARS.iter().copied())
        .collect::<Vec<_>>()
        .join(",")
}

/// A snapshot of the detection variables seen by one git command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentEnv(BTreeMap<String, String>);

impl AgentEnv {
    /// Detection variables from the current process. Only correct for code that runs in
    /// the same process as the command being attributed (never inside the daemon).
    pub fn from_process() -> Self {
        Self::from_pairs(
            DETECTION_ENV_VARS
                .iter()
                .filter_map(|name| std::env::var(name).ok().map(|v| (name.to_string(), v))),
        )
    }

    /// Keeps only known detection variables with non-empty values.
    pub fn from_pairs<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .filter(|(k, v)| !v.is_empty() && DETECTION_ENV_VARS.contains(&k.as_str()))
                .collect(),
        )
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    fn is(&self, name: &str, expected: &str) -> bool {
        self.get(name) == Some(expected)
    }
}

/// Detects a background agent from the current process environment. Use
/// [`detect_in`] with the committing command's environment when attributing commits.
pub fn detect() -> BackgroundAgent {
    detect_in(&AgentEnv::from_process())
}

pub fn detect_in(env: &AgentEnv) -> BackgroundAgent {
    if env.is("CLAUDE_CODE_REMOTE", "true") {
        return BackgroundAgent::WithHooks {
            tool: "claude-web".to_string(),
        };
    }

    if env.is("HOSTNAME", "cursor") && env.is("CURSOR_AGENT", "1") {
        return BackgroundAgent::WithHooks {
            tool: "cursor-agent".to_string(),
        };
    }

    // Devin runs everything (including the background service) inside its own VM, so a
    // machine-level marker is reliable here.
    if std::path::Path::new(DEVIN_DIR_PATH).is_dir() {
        let id = std::fs::read_to_string(DEVIN_ID_PATH)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| placeholder_id("DEVIN"));
        return BackgroundAgent::NoHooks {
            tool: "devin".to_string(),
            id,
        };
    }

    if env.is("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "codex_web_agent") {
        let id = env
            .get("CODEX_THREAD_ID")
            .map(str::to_string)
            .unwrap_or_else(|| placeholder_id("CODEX_CLOUD"));
        return BackgroundAgent::NoHooks {
            tool: "codex-cloud".to_string(),
            id,
        };
    }

    if env.is("AUTTER_CLOUD_AGENT", "1") {
        return BackgroundAgent::NoHooks {
            tool: "autter-cloud-agent".to_string(),
            id: placeholder_id("AUTTER_CLOUD_AGENT"),
        };
    }

    BackgroundAgent::None
}

/// If the commit was made by a no-hooks background agent, attribute any committed
/// lines that have no existing attestation ("holes") to that agent.
/// Existing attributions (human, other AI) are preserved.
/// Returns true if any attribution was applied.
pub fn fill_unattributed_lines(
    agent: &BackgroundAgent,
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    human_author: &str,
) -> bool {
    let BackgroundAgent::NoHooks { tool, id } = agent else {
        return false;
    };

    if committed_hunks.is_empty() {
        return false;
    }

    // Collect already-attributed lines per file
    let mut attributed_lines: HashMap<&str, HashSet<u32>> = HashMap::new();
    for file_attestation in &authorship_log.attestations {
        let lines = attributed_lines
            .entry(&file_attestation.file_path)
            .or_default();
        for entry in &file_attestation.entries {
            for range in &entry.line_ranges {
                for line in range.expand() {
                    lines.insert(line);
                }
            }
        }
    }

    // Find unattributed lines per file
    let mut unattributed_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();
    for (file_path, line_ranges) in committed_hunks {
        let existing = attributed_lines.get(file_path.as_str());
        let mut unattributed: Vec<u32> = Vec::new();
        for range in line_ranges {
            for line in range.expand() {
                if existing.is_none_or(|set| !set.contains(&line)) {
                    unattributed.push(line);
                }
            }
        }
        if !unattributed.is_empty() {
            unattributed.sort();
            unattributed_hunks.insert(file_path.clone(), LineRange::compress_lines(&unattributed));
        }
    }

    if unattributed_hunks.is_empty() {
        return false;
    }

    // Background agents rarely expose a model; normalize so we still emit a concrete
    // tool-default model rather than the "unknown" bucket.
    let agent_id = AgentId {
        tool: tool.clone(),
        id: id.clone(),
        model: String::new(),
    }
    .normalized();

    let session_key = generate_session_id(id, tool);
    let trace_id = generate_trace_id();
    let attestation_hash = format!("{}::{}", session_key, trace_id);

    // Every line we attribute here is both an addition and an accepted line for this
    // (hole-filling) session, so record matching per-session and per-file stats.
    let mut stats = crate::authorship::authorship_log::SessionStats::default();
    for (file_path, line_ranges) in &unattributed_hunks {
        let count: u32 = line_ranges.iter().map(|r| r.line_count()).sum();
        stats.total_additions += count;
        stats.accepted_lines += count;
        stats.files.insert(
            file_path.clone(),
            crate::authorship::authorship_log::FileStats {
                total_additions: count,
                total_deletions: 0,
                accepted_lines: count,
                overriden_lines: 0,
            },
        );
    }

    authorship_log.metadata.sessions.insert(
        session_key,
        SessionRecord {
            agent_id,
            human_author: Some(human_author.to_string()),
            messages_url: None,
            stats: Some(stats),
            custom_attributes: None,
        },
    );

    for (file_path, line_ranges) in unattributed_hunks {
        let file_attestation = authorship_log.get_or_create_file(&file_path);
        file_attestation.add_entry(AttestationEntry::new(attestation_hash.clone(), line_ranges));
    }

    true
}

/// Runs [`fill_unattributed_lines`] over every line the commit added relative to
/// `parent_sha` (`"initial"` for a root commit). Does nothing unless `agent` is a
/// no-hooks background agent.
pub fn fill_unattributed_commit_lines(
    repo: &crate::git::repository::Repository,
    agent: &BackgroundAgent,
    authorship_log: &mut AuthorshipLog,
    parent_sha: &str,
    commit_sha: &str,
    human_author: &str,
) -> bool {
    if !matches!(agent, BackgroundAgent::NoHooks { .. }) {
        return false;
    }
    tracing::debug!(
        ?agent,
        commit = commit_sha,
        "commit made by a background agent"
    );

    let diff_base = if parent_sha == "initial" {
        EMPTY_TREE_SHA
    } else {
        parent_sha
    };
    let Ok(added_lines) = repo.diff_added_lines(diff_base, commit_sha, None) else {
        return false;
    };
    let committed_hunks: HashMap<String, Vec<LineRange>> = added_lines
        .into_iter()
        .filter(|(_, lines)| !lines.is_empty())
        .map(|(path, lines)| (path, LineRange::compress_lines(&lines)))
        .collect();
    fill_unattributed_lines(agent, authorship_log, &committed_hunks, human_author)
}

const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn placeholder_id(name: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{name}_SESSION{ts}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> AgentEnv {
        AgentEnv::from_pairs(pairs.iter().copied())
    }

    fn tool(agent: &BackgroundAgent) -> Option<&str> {
        match agent {
            BackgroundAgent::WithHooks { tool } | BackgroundAgent::NoHooks { tool, .. } => {
                Some(tool)
            }
            BackgroundAgent::None => None,
        }
    }

    #[test]
    fn empty_env_is_not_an_agent() {
        if std::path::Path::new(DEVIN_DIR_PATH).is_dir() {
            return; // machine-level Devin marker present
        }
        assert_eq!(detect_in(&AgentEnv::default()), BackgroundAgent::None);
    }

    #[test]
    fn explicit_variables_are_detected() {
        assert_eq!(
            tool(&detect_in(&env(&[("AUTTER_CLOUD_AGENT", "1")]))),
            Some("autter-cloud-agent")
        );
        let codex = detect_in(&env(&[
            ("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "codex_web_agent"),
            ("CODEX_THREAD_ID", "thread-1"),
        ]));
        if !std::path::Path::new(DEVIN_DIR_PATH).is_dir() {
            assert_eq!(
                codex,
                BackgroundAgent::NoHooks {
                    tool: "codex-cloud".to_string(),
                    id: "thread-1".to_string()
                }
            );
        }
        assert_eq!(
            detect_in(&env(&[("CLAUDE_CODE_REMOTE", "true")])),
            BackgroundAgent::WithHooks {
                tool: "claude-web".to_string()
            }
        );
    }

    #[test]
    fn unknown_or_empty_variables_are_ignored() {
        if std::path::Path::new(DEVIN_DIR_PATH).is_dir() {
            return;
        }
        assert_eq!(
            detect_in(&env(&[
                ("CLOUD_AGENT_ID", "abc"),
                ("AUTTER_CLOUD_AGENT", ""),
                ("AUTTER_CLOUD_AGENT_X", "1"),
            ])),
            BackgroundAgent::None
        );
        assert_eq!(
            detect_in(&env(&[("AUTTER_CLOUD_AGENT", "true")])),
            BackgroundAgent::None
        );
    }

    #[test]
    fn trace2_env_vars_value_lists_every_detection_variable() {
        let value = trace2_env_vars_value();
        let names: Vec<&str> = value.split(',').collect();
        assert!(names.contains(&"AUTTER_WRAPPER_INVOCATION_ID"));
        for name in DETECTION_ENV_VARS {
            assert!(names.contains(name), "{name} missing from {value}");
        }
    }
}
