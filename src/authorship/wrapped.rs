//! `autter wrapped`: a year of AI-assisted coding in this repository, in the
//! numbers a share card needs. Walks the year's commits, reads each
//! authorship note, and folds the per-commit stats into one
//! [`WrappedSummary`]: AI share overall and by month, the agents and models
//! behind the AI lines, the file that drew the most prompts, the longest
//! prompt, and the streak and weekday habits.
//!
//! Nothing here leaves the machine. Prompt text is read only from the local
//! transcript cache, and even then only its length is reported unless the
//! caller opts into the 120-character snippet.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{Datelike, NaiveDate, Weekday};
use serde::Serialize;

use crate::authorship::authorship_log_serialization::{AUTTER_VERSION, AuthorshipLog};
use crate::authorship::stats::stats_for_commit_stats_with_authorship;
use crate::authorship::transcript::Message;
use crate::authorship::working_log::AgentId;
use crate::error::AutterError;
use crate::git::notes_api::read_authorship;
use crate::git::repository::{Repository, exec_git_allow_nonzero};

/// Schema tag on the JSON output. Bump when the shape changes; the share
/// image endpoint validates against it.
pub const WRAPPED_SCHEMA_VERSION: &str = "wrapped/1.0.0";

/// The most commits one summary walks. A year of a busy repository fits;
/// beyond this the summary says it was capped rather than running for minutes.
pub const COMMIT_CAP: usize = 5000;

/// Commits with an authorship note needed before a card is worth showing.
pub const MIN_COMMITS_FOR_CARD: u32 = 3;

/// Characters of prompt text included in the snippet when opted in.
pub const SNIPPET_CHARS: usize = 120;

#[derive(Debug, Clone)]
pub struct WrappedOptions {
    pub year: i32,
    /// Commits by this author email only; `None` keeps every author.
    pub author: Option<String>,
    /// Include the first 120 characters of the longest prompt.
    pub include_prompt_text: bool,
    pub ignore_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MonthStats {
    /// 1 = January … 12 = December.
    pub month: u32,
    pub ai_accepted: u64,
    pub added_lines: u64,
    pub ai_percent: f64,
    pub commits: u32,
}

/// One agent or model with the AI lines it accounts for.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RankedName {
    pub name: String,
    pub lines: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RankedFile {
    pub path: String,
    /// Distinct prompts or sessions attested on this file across the year.
    pub prompts: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LongestPrompt {
    pub chars: u64,
    pub words: u64,
    pub agent: String,
    pub model: String,
    pub commit: String,
    /// First 120 characters of the prompt; `null` unless prompt text was
    /// opted in.
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WrappedSummary {
    pub schema_version: &'static str,
    pub autter_version: &'static str,
    pub year: i32,
    /// The author email the summary is scoped to; `null` with all authors.
    pub author: Option<String>,
    pub all_authors: bool,
    pub commits: u32,
    pub commits_with_authorship: u32,
    /// True when the walk stopped at [`COMMIT_CAP`] commits.
    pub capped: bool,
    pub commit_cap: usize,
    pub added_lines: u64,
    pub ai_lines: u64,
    pub human_lines: u64,
    pub untracked_lines: u64,
    /// `ai_lines / added_lines`, as a percentage with two decimals.
    pub ai_percent: f64,
    pub by_month: Vec<MonthStats>,
    pub top_agent: Option<RankedName>,
    pub top_model: Option<RankedName>,
    pub agents: Vec<RankedName>,
    pub models: Vec<RankedName>,
    pub most_prompted_file: Option<RankedFile>,
    pub files: Vec<RankedFile>,
    pub prompts_total: u32,
    /// Prompts whose transcript is in the local cache, so their length is known.
    pub prompts_with_transcript: u32,
    pub longest_prompt: Option<LongestPrompt>,
    /// Weekday with the most AI lines, e.g. `"Tuesday"`.
    pub busiest_day: Option<String>,
    /// Longest run of consecutive days with an AI-attributed commit.
    pub streak_days: u32,
}

impl WrappedSummary {
    /// Whether there is enough data for a card.
    pub fn has_enough_data(&self) -> bool {
        self.commits_with_authorship >= MIN_COMMITS_FOR_CARD
    }
}

struct CommitRow {
    sha: String,
    date: NaiveDate,
}

/// Build the summary for the year.
pub fn wrapped_summary(
    repo: &Repository,
    options: &WrappedOptions,
) -> Result<WrappedSummary, AutterError> {
    let (rows, capped) = year_commits(repo, options.year, options.author.as_deref())?;

    let mut months: Vec<MonthStats> = (1..=12)
        .map(|month| MonthStats {
            month,
            ai_accepted: 0,
            added_lines: 0,
            ai_percent: 0.0,
            commits: 0,
        })
        .collect();
    let mut commits_with_authorship = 0u32;
    let mut added_lines = 0u64;
    let mut ai_lines = 0u64;
    let mut human_lines = 0u64;
    let mut untracked_lines = 0u64;
    let mut agents: BTreeMap<String, u64> = BTreeMap::new();
    let mut models: BTreeMap<String, u64> = BTreeMap::new();
    let mut prompts_by_file: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut prompt_ids: HashSet<String> = HashSet::new();
    let mut ai_by_weekday: [u64; 7] = [0; 7];
    let mut ai_days: BTreeSet<NaiveDate> = BTreeSet::new();
    let mut prompts_with_transcript = 0u32;
    let mut longest: Option<LongestPrompt> = None;
    // A transcript shared by several commits is read once.
    let mut transcript_lengths: HashMap<String, Option<(u64, u64, String)>> = HashMap::new();

    for row in &rows {
        let log = read_authorship(repo, &row.sha);
        if log.is_some() {
            commits_with_authorship += 1;
        }
        let stats = stats_for_commit_stats_with_authorship(
            repo,
            &row.sha,
            &options.ignore_patterns,
            log.as_ref(),
        )?;

        let month = &mut months[(row.date.month0()) as usize];
        month.commits += 1;
        month.added_lines += u64::from(stats.git_diff_added_lines);
        month.ai_accepted += u64::from(stats.ai_accepted);

        added_lines += u64::from(stats.git_diff_added_lines);
        ai_lines += u64::from(stats.ai_accepted);
        human_lines += u64::from(stats.human_additions);
        untracked_lines += u64::from(stats.unknown_additions);

        if stats.ai_accepted > 0 {
            ai_by_weekday[row.date.weekday().num_days_from_monday() as usize] +=
                u64::from(stats.ai_accepted);
            ai_days.insert(row.date);
        }

        for (key, headline) in &stats.tool_model_breakdown {
            let (tool, model) = split_tool_model(key);
            *agents.entry(tool.to_string()).or_default() += u64::from(headline.ai_accepted);
            *models.entry(model.to_string()).or_default() += u64::from(headline.ai_accepted);
        }

        let Some(log) = log else {
            continue;
        };

        for file in &log.attestations {
            for entry in &file.entries {
                if let Some(id) = prompt_id_from_hash(&entry.hash) {
                    prompts_by_file
                        .entry(file.file_path.clone())
                        .or_default()
                        .insert(id.to_string());
                }
            }
        }

        for (id, agent_id, messages_url) in prompt_records(&log) {
            if !prompt_ids.insert(id) {
                continue;
            }
            let Some(url) = messages_url else {
                continue;
            };
            let measured = transcript_lengths
                .entry(url.clone())
                .or_insert_with(|| first_user_prompt_length(&url))
                .clone();
            let Some((chars, words, text)) = measured else {
                continue;
            };
            prompts_with_transcript += 1;
            if longest.as_ref().is_none_or(|best| chars > best.chars) {
                longest = Some(LongestPrompt {
                    chars,
                    words,
                    agent: agent_id.tool.clone(),
                    model: agent_id.model.clone(),
                    commit: row.sha.clone(),
                    snippet: options
                        .include_prompt_text
                        .then(|| text.chars().take(SNIPPET_CHARS).collect()),
                });
            }
        }
    }

    for month in &mut months {
        month.ai_percent = percent(month.ai_accepted, month.added_lines);
    }

    let agents = ranked(agents);
    let models = ranked(models);
    let mut files: Vec<RankedFile> = prompts_by_file
        .into_iter()
        .map(|(path, ids)| RankedFile {
            path,
            prompts: ids.len() as u32,
        })
        .collect();
    files.sort_by(|a, b| b.prompts.cmp(&a.prompts).then_with(|| a.path.cmp(&b.path)));
    files.truncate(5);

    Ok(WrappedSummary {
        schema_version: WRAPPED_SCHEMA_VERSION,
        autter_version: AUTTER_VERSION,
        year: options.year,
        author: options.author.clone(),
        all_authors: options.author.is_none(),
        commits: rows.len() as u32,
        commits_with_authorship,
        capped,
        commit_cap: COMMIT_CAP,
        added_lines,
        ai_lines,
        human_lines,
        untracked_lines,
        ai_percent: percent(ai_lines, added_lines),
        by_month: months,
        top_agent: agents.first().cloned(),
        top_model: models.first().cloned(),
        agents,
        models,
        most_prompted_file: files.first().cloned(),
        files,
        prompts_total: prompt_ids.len() as u32,
        prompts_with_transcript,
        longest_prompt: longest,
        busiest_day: busiest_weekday(&ai_by_weekday),
        streak_days: longest_streak(&ai_days),
    })
}

/// The year's commits, newest first, by the author when one is given.
/// Merge commits are left out: they add no lines of their own.
fn year_commits(
    repo: &Repository,
    year: i32,
    author: Option<&str>,
) -> Result<(Vec<CommitRow>, bool), AutterError> {
    let mut args = repo.global_args_for_exec();
    // The window is a day wider on each side than the year because git
    // reads `--since`/`--until` in this machine's timezone while the
    // commit's own date (what `%ad` prints) decides the bucket below.
    args.extend(
        [
            "log",
            "--no-color",
            "--no-merges",
            "-n",
            &COMMIT_CAP.to_string(),
            &format!("--since={}-12-31T00:00:00", year - 1),
            &format!("--until={}-01-01T23:59:59", year + 1),
            "--date=format:%Y-%m-%d",
            "--format=%H%x00%ad%x00%ae",
            "HEAD",
            "--",
        ]
        .map(String::from),
    );
    let output = exec_git_allow_nonzero(&args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // A repository with no commits yet has nothing to wrap up.
        if stderr.contains("does not have any commits")
            || stderr.contains("unknown revision")
            || stderr.contains("bad revision")
        {
            return Ok((Vec::new(), false));
        }
        return Err(AutterError::Generic(format!(
            "git log failed: {}",
            stderr.trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut listed = 0usize;
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let mut fields = line.split('\0');
        let (Some(sha), Some(date), Some(email)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        listed += 1;
        let Ok(date) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
            continue;
        };
        if date.year() != year {
            continue;
        }
        if let Some(author) = author
            && !email.eq_ignore_ascii_case(author)
        {
            continue;
        }
        rows.push(CommitRow {
            sha: sha.to_string(),
            date,
        });
    }
    Ok((rows, listed >= COMMIT_CAP))
}

/// `tool::model` keys from `CommitStats::tool_model_breakdown`.
pub fn split_tool_model(key: &str) -> (&str, &str) {
    match key.split_once("::") {
        Some((tool, model)) => (tool, model),
        None => (key, key),
    }
}

/// The prompt or session id behind an attestation hash, or `None` for a
/// known-human entry. Session entries carry a trace suffix
/// (`s_xxx::t_yyy`) that is not part of the id.
fn prompt_id_from_hash(hash: &str) -> Option<&str> {
    if hash.starts_with("h_") {
        return None;
    }
    if hash.starts_with("s_") {
        return hash.split("::").next();
    }
    Some(hash)
}

/// Every prompt and session record in a note as `(id, agent, messages_url)`.
/// The prompts map mirrors the sessions map, so ids repeat; the caller
/// deduplicates.
fn prompt_records(log: &AuthorshipLog) -> Vec<(String, AgentId, Option<String>)> {
    let mut records = Vec::new();
    for (id, record) in &log.metadata.prompts {
        records.push((
            id.clone(),
            record.agent_id.clone(),
            record.messages_url.clone(),
        ));
    }
    for (id, record) in &log.metadata.sessions {
        records.push((
            id.clone(),
            record.agent_id.clone(),
            record.messages_url.clone(),
        ));
    }
    records
}

/// Length of the first user message of a transcript held in the local cache:
/// `(chars, words, text)`. Transcripts that are not on this machine are
/// skipped; nothing is fetched.
fn first_user_prompt_length(messages_url: &str) -> Option<(u64, u64, String)> {
    let messages = crate::authorship::cas_bridge::resolve_cas_messages_local(messages_url)
        .ok()
        .flatten()?;
    let text = messages.into_iter().find_map(|message| match message {
        Message::User { text, .. } => Some(text),
        _ => None,
    })?;
    let chars = text.chars().count() as u64;
    let words = text.split_whitespace().count() as u64;
    Some((chars, words, text))
}

fn ranked(counts: BTreeMap<String, u64>) -> Vec<RankedName> {
    let mut ranked: Vec<RankedName> = counts
        .into_iter()
        .filter(|(_, lines)| *lines > 0)
        .map(|(name, lines)| RankedName { name, lines })
        .collect();
    ranked.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.name.cmp(&b.name)));
    ranked
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        ((part as f64 / total as f64) * 10000.0).round() / 100.0
    }
}

const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// The weekday (Monday first) with the most AI lines; ties go to the
/// earlier weekday.
fn busiest_weekday(ai_by_weekday: &[u64; 7]) -> Option<String> {
    let (index, lines) =
        ai_by_weekday
            .iter()
            .enumerate()
            .fold((0usize, 0u64), |best, (i, &lines)| {
                if lines > best.1 { (i, lines) } else { best }
            });
    (lines > 0).then(|| WEEKDAYS[index].to_string())
}

/// Longest run of consecutive dates.
fn longest_streak(days: &BTreeSet<NaiveDate>) -> u32 {
    let mut best = 0u32;
    let mut run = 0u32;
    let mut previous: Option<NaiveDate> = None;
    for &day in days {
        run = match previous {
            Some(prev) if prev.succ_opt() == Some(day) => run + 1,
            _ => 1,
        };
        best = best.max(run);
        previous = Some(day);
    }
    best
}

/// Full weekday name for a date, as `busiest_day` reports it.
pub fn weekday_name(weekday: Weekday) -> &'static str {
    WEEKDAYS[weekday.num_days_from_monday() as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn splits_tool_and_model_keys() {
        assert_eq!(split_tool_model("claude::opus"), ("claude", "opus"));
        assert_eq!(
            split_tool_model("cursor::anthropic/claude-4::fast"),
            ("cursor", "anthropic/claude-4::fast")
        );
        assert_eq!(split_tool_model("legacy"), ("legacy", "legacy"));
    }

    #[test]
    fn prompt_ids_skip_humans_and_trace_suffixes() {
        assert_eq!(prompt_id_from_hash("h_abc"), None);
        assert_eq!(prompt_id_from_hash("s_abc::t_def"), Some("s_abc"));
        assert_eq!(prompt_id_from_hash("s_abc"), Some("s_abc"));
        assert_eq!(prompt_id_from_hash("1a2b3c4d"), Some("1a2b3c4d"));
    }

    #[test]
    fn streak_counts_consecutive_days_only() {
        let days: BTreeSet<NaiveDate> = [
            date(2026, 3, 1),
            date(2026, 3, 2),
            date(2026, 3, 3),
            date(2026, 3, 5),
            date(2026, 3, 6),
        ]
        .into_iter()
        .collect();
        assert_eq!(longest_streak(&days), 3);
        assert_eq!(longest_streak(&BTreeSet::new()), 0);
        assert_eq!(longest_streak(&[date(2026, 1, 1)].into_iter().collect()), 1);
    }

    #[test]
    fn streak_crosses_month_boundaries() {
        let days: BTreeSet<NaiveDate> = [date(2026, 1, 31), date(2026, 2, 1)].into_iter().collect();
        assert_eq!(longest_streak(&days), 2);
    }

    #[test]
    fn busiest_weekday_prefers_earlier_day_on_ties() {
        assert_eq!(busiest_weekday(&[0; 7]), None);
        assert_eq!(
            busiest_weekday(&[1, 5, 5, 0, 0, 0, 0]),
            Some("Tuesday".to_string())
        );
        assert_eq!(
            busiest_weekday(&[0, 0, 0, 0, 0, 0, 9]),
            Some("Sunday".to_string())
        );
    }

    #[test]
    fn ranking_drops_zero_and_breaks_ties_by_name() {
        let counts: BTreeMap<String, u64> = [
            ("zed".to_string(), 4),
            ("claude".to_string(), 4),
            ("cursor".to_string(), 0),
            ("codex".to_string(), 9),
        ]
        .into_iter()
        .collect();
        let names: Vec<String> = ranked(counts).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["codex", "claude", "zed"]);
    }

    #[test]
    fn percent_rounds_to_two_decimals() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(9, 14), 64.29);
        assert_eq!(percent(2, 3), 66.67);
        assert_eq!(percent(3, 3), 100.0);
    }

    #[test]
    fn weekday_names_are_full() {
        assert_eq!(weekday_name(Weekday::Mon), "Monday");
        assert_eq!(weekday_name(Weekday::Sun), "Sunday");
    }
}
