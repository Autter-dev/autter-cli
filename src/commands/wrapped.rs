//! `autter wrapped`: the year's AI-assisted coding as a share card. The
//! numbers come from `authorship::wrapped`; this module parses the flags,
//! prints the JSON contract for the share-image pipeline, or draws the
//! fixed-width card. The card is 60 columns, deterministic for a given
//! summary, and goes to stdout so it can be piped into a screenshot tool.
//! `--share` publishes the JSON as a page with a share image through
//! `api::wrapped_share`; `--unshare` takes one down.

use crate::api::client::{
    ApiClient, ApiContext, access_token_for_org, resolve_org_for_repo_cached,
};
use crate::api::wrapped_share::{
    ShareError, WrappedShareResponse, build_share_request, share_token_from,
};
use crate::authorship::ignore::effective_ignore_patterns;
use crate::authorship::post_commit::current_repo_url;
use crate::authorship::wrapped::{
    MIN_COMMITS_FOR_CARD, WrappedOptions, WrappedSummary, wrapped_summary,
};
use crate::commands::{self, arg_parser};
use crate::git::find_repository;
use crate::git::repository::Repository;
use crate::ui;
use crate::ui::rail;
use crate::ui::theme::{Charset, TermCaps};

/// Outer width of the card, borders included.
const CARD_WIDTH: usize = 60;
/// Columns between the two borders.
const INNER_WIDTH: usize = CARD_WIDTH - 2;
/// Margin inside the card, on both sides.
const INDENT: usize = 3;
/// Columns available to content between the margins.
const CONTENT_WIDTH: usize = INNER_WIDTH - 2 * INDENT;
/// Width of the label column in the stats rows.
const LABEL_WIDTH: usize = 15;
/// Rows of the monthly bar chart.
const CHART_ROWS: usize = 4;
const FOOTER: &str = "autter.dev/wrapped";
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn handle_wrapped(args: &[String]) {
    let pp = match arg_parser::pre_parse(args, arg_parser::ScanMode::Full, false) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(commands::EXIT_USAGE_ERROR);
        }
    };
    if pp.flags.help {
        arg_parser::print_command_help("wrapped");
        return;
    }
    arg_parser::merge_global_flags(&pp.flags);

    let mut year: Option<i32> = None;
    let mut author: Option<String> = None;
    let mut all_authors = false;
    let mut include_prompt_text = false;
    let mut ignore_patterns: Vec<String> = Vec::new();
    let mut share = false;
    let mut hide_repo = false;
    let mut unshare: Option<String> = None;

    let usage = |message: &str| -> ! {
        eprintln!("{message}");
        eprintln!("Run 'autter wrapped --help' for usage.");
        std::process::exit(commands::EXIT_USAGE_ERROR);
    };

    let rest = &pp.rest;
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].as_str();
        let (flag, inline_value) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
            _ => (arg, None),
        };
        let mut take_value = |what: &str| -> String {
            if let Some(value) = inline_value.clone() {
                return value;
            }
            i += 1;
            match rest.get(i) {
                Some(value) if !value.starts_with("--") => value.clone(),
                _ => usage(&format!("{flag} requires {what}")),
            }
        };
        match flag {
            "--year" => {
                let raw = take_value("a year, e.g. --year 2026");
                match raw.parse::<i32>() {
                    Ok(parsed) if (1970..=9999).contains(&parsed) => year = Some(parsed),
                    _ => usage(&format!("--year expects a four-digit year, got '{raw}'")),
                }
            }
            "--author" => {
                let email = take_value("an email address");
                if email.trim().is_empty() {
                    usage("--author expects an email address");
                }
                author = Some(email.trim().to_string());
            }
            "--all-authors" => all_authors = true,
            "--include-prompt-text" => include_prompt_text = true,
            "--ignore" => ignore_patterns.push(take_value("a pattern")),
            "--share" => share = true,
            "--hide-repo" => hide_repo = true,
            "--unshare" => {
                let raw = take_value("a share link or token");
                match share_token_from(&raw) {
                    Some(token) => unshare = Some(token),
                    None => usage(&format!(
                        "--unshare expects a share link (app.autter.dev/wrapped/<token>) or a token, got '{raw}'"
                    )),
                }
            }
            other => usage(&format!("Unknown wrapped argument: {other}")),
        }
        i += 1;
    }
    if all_authors && author.is_some() {
        usage("--author and --all-authors cannot be combined");
    }
    if hide_repo && !share {
        usage("--hide-repo only applies with --share");
    }
    if unshare.is_some() && share {
        usage("--share and --unshare cannot be combined");
    }

    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(commands::EXIT_RUNTIME_ERROR);
        }
    };

    if let Some(token) = unshare {
        let client = match share_client(&repo) {
            Ok(client) => client,
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(commands::EXIT_RUNTIME_ERROR);
            }
        };
        match client.unshare_wrapped(&token) {
            Ok(()) => println!("Removed the share page for {token}."),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(commands::EXIT_RUNTIME_ERROR);
            }
        }
        return;
    }

    let author = if all_authors {
        None
    } else if let Some(author) = author {
        Some(author)
    } else {
        match repo.git_commit_author_identity().email {
            Some(email) if !email.trim().is_empty() => Some(email.trim().to_string()),
            _ => usage(
                "No git author email is configured. Set user.email, or pass --author <email> or --all-authors.",
            ),
        }
    };

    let options = WrappedOptions {
        year: year.unwrap_or_else(current_year),
        author,
        include_prompt_text,
        ignore_patterns: effective_ignore_patterns(&repo, &ignore_patterns, &[]),
    };

    let summary = match wrapped_summary(&repo, &options) {
        Ok(summary) => summary,
        Err(e) => {
            eprintln!("Wrapped failed: {}", e);
            std::process::exit(commands::EXIT_RUNTIME_ERROR);
        }
    };

    // Sharing needs a connected account; check before printing anything so
    // the reason is the last line on screen.
    let client = if share {
        if !summary.has_enough_data() {
            print!("{}", render(&summary, &card_caps()));
            eprintln!("{}", ShareError::NothingToShare);
            std::process::exit(commands::EXIT_RUNTIME_ERROR);
        }
        match share_client(&repo) {
            Ok(client) => Some(client),
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(commands::EXIT_RUNTIME_ERROR);
            }
        }
    } else {
        None
    };

    if arg_parser::json() {
        match serde_json::to_string_pretty(&summary) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("Wrapped failed: {}", e);
                std::process::exit(commands::EXIT_RUNTIME_ERROR);
            }
        }
    } else {
        print!("{}", render(&summary, &card_caps()));
    }

    let Some(client) = client else {
        return;
    };
    let body = build_share_request(
        &summary,
        current_repo_url(&repo).as_deref(),
        include_prompt_text,
        hide_repo,
    );
    match client.share_wrapped(&body) {
        Ok(response) => announce_share(&response, arg_parser::json()),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(commands::EXIT_RUNTIME_ERROR);
        }
    }
}

/// The signed-in client for sharing: an org-scoped token for the repository's
/// org when the account has one, else the stored sign-in. Local mode and a
/// missing sign-in stop here, before anything is sent.
fn share_client(repo: &Repository) -> Result<ApiClient, String> {
    if !crate::config::platform_sync_enabled_now() {
        return Err(
            "Sharing is off in local mode: nothing leaves this machine. Run `autter onboard` to connect an account."
                .to_string(),
        );
    }
    let client = if let Some(repo_url) = current_repo_url(repo)
        && let Some(org) = resolve_org_for_repo_cached(&repo_url)
        && let Some(token) = access_token_for_org(&org)
    {
        ApiClient::new(ApiContext::with_auth(None, token))
    } else {
        ApiClient::new(ApiContext::new(None))
    };
    if !client.is_logged_in() && !client.has_api_key() {
        return Err(ShareError::LoginRequired.to_string());
    }
    Ok(client)
}

/// The share link, after the card (or on stderr after `--json`, keeping
/// stdout the exact contract), opened in the browser at a terminal.
fn announce_share(response: &WrappedShareResponse, json: bool) {
    let caps = card_caps();
    let line = format!("Shared at {}", caps.accent(&response.url));
    if json {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
    if crate::utils::is_interactive_terminal()
        && std::io::IsTerminal::is_terminal(&std::io::stdout())
        && let Err(e) = crate::commands::personal_dashboard::open_browser(&response.url)
    {
        eprintln!("Could not open the browser: {e}");
    }
}

fn current_year() -> i32 {
    use chrono::Datelike;
    chrono::Local::now().year()
}

/// Capabilities for the card on stdout. The card is decorative and meant
/// to be piped into screenshot tools, so it always draws as if on a wide
/// terminal; only the color decision follows stdout (`--no-color`,
/// `NO_COLOR`, not a TTY).
fn card_caps() -> TermCaps {
    TermCaps::from_env(cfg!(windows), true, arg_parser::use_color(), 80, |key| {
        std::env::var(key).ok()
    })
}

/// Box, bar and big-digit glyphs for one charset.
struct CardGlyphs {
    top_left: &'static str,
    top_right: &'static str,
    bottom_left: &'static str,
    bottom_right: &'static str,
    horizontal: &'static str,
    vertical: &'static str,
    full: &'static str,
    /// One to seven eighths of a block, bottom-up.
    eighths: [&'static str; 7],
    /// A month without commits.
    empty_month: &'static str,
    /// A month with commits but no AI lines.
    zero_month: &'static str,
}

const UNICODE_CARD: CardGlyphs = CardGlyphs {
    top_left: "┌",
    top_right: "┐",
    bottom_left: "└",
    bottom_right: "┘",
    horizontal: "─",
    vertical: "│",
    full: "█",
    eighths: ["▁", "▂", "▃", "▄", "▅", "▆", "▇"],
    empty_month: "·",
    zero_month: "▁",
};

const ASCII_CARD: CardGlyphs = CardGlyphs {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    horizontal: "-",
    vertical: "|",
    full: "#",
    eighths: [" ", " ", " ", "#", "#", "#", "#"],
    empty_month: ".",
    zero_month: "_",
};

/// Five-row block font for the headline percentage: digits then `%`.
/// `#` is a block, space is empty.
const BIG_FONT: [[&str; 5]; 11] = [
    [" ### ", "#   #", "#   #", "#   #", " ### "],
    ["  #  ", " ##  ", "  #  ", "  #  ", " ### "],
    [" ### ", "#   #", "   # ", "  #  ", "#####"],
    ["#### ", "    #", " ### ", "    #", "#### "],
    ["#  # ", "#  # ", "#####", "   # ", "   # "],
    ["#####", "#    ", "#### ", "    #", "#### "],
    [" ### ", "#    ", "#### ", "#   #", " ### "],
    ["#####", "    #", "   # ", "  #  ", "  #  "],
    [" ### ", "#   #", " ### ", "#   #", " ### "],
    [" ### ", "#   #", " ####", "    #", " ### "],
    ["##  #", "   # ", "  #  ", " #   ", "#  ##"],
];

/// A styled piece of a card row. `style` is an SGR code for `caps.paint`,
/// `"accent"` for the brand color, or empty for plain text.
struct Segment {
    style: &'static str,
    text: String,
}

fn seg(style: &'static str, text: impl Into<String>) -> Segment {
    Segment {
        style,
        text: text.into(),
    }
}

fn dim(text: impl Into<String>) -> Segment {
    seg("2", text)
}

fn bold(text: impl Into<String>) -> Segment {
    seg("1", text)
}

fn accent(text: impl Into<String>) -> Segment {
    seg("accent", text)
}

struct Card<'a> {
    caps: &'a TermCaps,
    glyphs: &'static CardGlyphs,
    out: String,
}

impl Card<'_> {
    fn paint(&self, segment: &Segment) -> String {
        match segment.style {
            "" => segment.text.clone(),
            "accent" => self.caps.accent(&segment.text),
            sgr => self.caps.paint(sgr, &segment.text),
        }
    }

    fn border(&mut self, left: &str, right: &str) {
        let line = format!(
            "{left}{}{right}",
            self.glyphs.horizontal.repeat(INNER_WIDTH)
        );
        self.out.push_str(&self.caps.paint("2", &line));
        self.out.push('\n');
    }

    /// One row: the segments left-aligned inside the margin, `right`
    /// right-aligned against the other margin, padded to the inner width.
    fn row(&mut self, left: &[Segment], right: &[Segment]) {
        let left_width: usize = left.iter().map(|s| s.text.chars().count()).sum();
        let right_width: usize = right.iter().map(|s| s.text.chars().count()).sum();
        let used = left_width + right_width;
        let gap = CONTENT_WIDTH
            .saturating_sub(used)
            .max(if right.is_empty() { 0 } else { 1 });
        let mut body = String::new();
        body.push_str(&" ".repeat(INDENT));
        for segment in left {
            body.push_str(&self.paint(segment));
        }
        body.push_str(&" ".repeat(gap));
        for segment in right {
            body.push_str(&self.paint(segment));
        }
        // Over-long content gets no padding rather than a broken border.
        let trailing = INNER_WIDTH.saturating_sub(INDENT + left_width + gap + right_width);
        body.push_str(&" ".repeat(trailing));
        let bar = self.caps.paint("2", self.glyphs.vertical);
        self.out.push_str(&format!("{bar}{body}{bar}\n"));
    }

    fn blank(&mut self) {
        self.row(&[], &[]);
    }

    /// A stats row: dimmed label column, then the value segments.
    fn labeled(&mut self, label: &str, value: Vec<Segment>) {
        let mut segments = vec![dim(format!("{label:<LABEL_WIDTH$}"))];
        segments.extend(value);
        self.row(&segments, &[]);
    }
}

/// The card for a summary with enough data, or the empty state otherwise.
pub fn render(summary: &WrappedSummary, caps: &TermCaps) -> String {
    if !summary.has_enough_data() {
        return render_empty_state(summary, caps);
    }
    let glyphs = if caps.charset == Charset::Unicode {
        &UNICODE_CARD
    } else {
        &ASCII_CARD
    };
    let mut card = Card {
        caps,
        glyphs,
        out: String::new(),
    };

    card.out.push_str(&header(summary).render(caps));
    card.out.push('\n');

    card.border(glyphs.top_left, glyphs.top_right);
    card.blank();

    // Headline percentage in the block font.
    let headline = format!("{}%", summary.ai_percent.round() as u64);
    for row in big_text(&headline, glyphs) {
        card.row(&[accent(row)], &[]);
    }
    card.blank();
    card.row(
        &[dim(format!(
            "of the lines added in {} were written with AI",
            summary.year
        ))],
        &[],
    );
    card.blank();

    // Monthly chart.
    let peak = summary
        .by_month
        .iter()
        .filter(|month| month.commits > 0)
        .max_by(|a, b| {
            a.ai_percent
                .partial_cmp(&b.ai_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.month.cmp(&a.month))
        });
    let peak_label = peak
        .map(|month| {
            format!(
                "peak {} {}%",
                MONTHS[(month.month - 1) as usize],
                month.ai_percent.round() as u64
            )
        })
        .unwrap_or_default();
    card.row(&[dim("AI share by month")], &[dim(peak_label)]);
    for row in chart_rows(summary, glyphs) {
        card.row(&row, &[]);
    }
    card.row(&[dim(MONTHS.join(" "))], &[]);
    card.blank();

    // Who and what.
    card.labeled(
        "Top agent",
        match &summary.top_agent {
            Some(agent) => vec![
                bold(agent.name.clone()),
                dim(format!(" · {} lines", thousands(agent.lines))),
            ],
            None => vec![dim("no AI lines yet")],
        },
    );
    card.labeled(
        "Top model",
        match &summary.top_model {
            Some(model) => vec![
                bold(fit(&model.name, 24)),
                dim(format!(" · {} lines", thousands(model.lines))),
            ],
            None => vec![dim("no AI lines yet")],
        },
    );
    card.labeled(
        "Most prompted",
        match &summary.most_prompted_file {
            Some(file) => {
                let suffix = format!(" · {} prompts", file.prompts);
                let room = CONTENT_WIDTH - LABEL_WIDTH - suffix.chars().count();
                vec![bold(fit_path(&file.path, room)), dim(suffix)]
            }
            None => vec![dim("no prompted files yet")],
        },
    );
    match &summary.longest_prompt {
        Some(prompt) => {
            card.labeled(
                "Longest prompt",
                vec![
                    bold(format!("{} chars", thousands(prompt.chars))),
                    dim(format!(
                        " · {} words · {}",
                        thousands(prompt.words),
                        prompt.agent
                    )),
                ],
            );
            if let Some(snippet) = &prompt.snippet {
                let room = CONTENT_WIDTH - LABEL_WIDTH - 2;
                let one_line: String = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
                card.labeled(
                    "",
                    vec![dim(format!("\u{201C}{}\u{201D}", fit(&one_line, room)))],
                );
            }
        }
        None => card.labeled("Longest prompt", vec![dim("no transcripts cached locally")]),
    }
    card.labeled(
        "Streak",
        match summary.streak_days {
            0 => vec![dim("no AI commits yet")],
            1 => vec![bold("1 day"), dim(" with AI commits")],
            days => vec![
                bold(format!("{days} days")),
                dim(" in a row with AI commits"),
            ],
        },
    );
    card.labeled(
        "Busiest day",
        match &summary.busiest_day {
            Some(day) => vec![bold(day.clone()), dim(" · most AI lines")],
            None => vec![dim("no AI commits yet")],
        },
    );
    card.blank();

    // Totals.
    card.row(
        &[dim(format!(
            "{} commits · {} AI lines",
            thousands(u64::from(summary.commits)),
            thousands(summary.ai_lines)
        ))],
        &[],
    );
    card.row(
        &[dim(format!(
            "{} human · {} untracked lines",
            thousands(summary.human_lines),
            thousands(summary.untracked_lines)
        ))],
        &[],
    );
    if summary.capped {
        card.row(
            &[dim(format!(
                "first {} commits of the year only",
                thousands(summary.commit_cap as u64)
            ))],
            &[],
        );
    }
    card.blank();
    card.row(&[], &[accent(FOOTER)]);
    card.blank();
    card.border(glyphs.bottom_left, glyphs.bottom_right);
    card.out
}

fn header(summary: &WrappedSummary) -> ui::Header {
    let who = summary
        .author
        .clone()
        .unwrap_or_else(|| "all authors".to_string());
    ui::Header::new(
        "Autter Wrapped",
        summary.year.to_string(),
        who,
        format!(
            "{} commits · {} with authorship notes",
            thousands(u64::from(summary.commits)),
            thousands(u64::from(summary.commits_with_authorship))
        ),
    )
}

/// What `autter wrapped` prints when the year has too few noted commits.
fn render_empty_state(summary: &WrappedSummary, caps: &TermCaps) -> String {
    let noted = summary.commits_with_authorship;
    let found = match (summary.commits, noted) {
        (0, _) => format!("No commits in {} yet.", summary.year),
        (commits, 0) => format!(
            "{} in {}, none with an authorship note.",
            plural(commits, "commit"),
            summary.year
        ),
        (_, noted) => format!(
            "{} with an authorship note in {}.",
            plural(noted, "commit"),
            summary.year
        ),
    };
    [
        rail::step(caps, &format!("Autter Wrapped {}", summary.year), None),
        rail::note(
            caps,
            &format!(
                "{found} A card needs at least {MIN_COMMITS_FOR_CARD}.\n\
                 Commit with an autter-tracked agent, then run autter wrapped again."
            ),
        ),
        rail::end(caps, FOOTER),
    ]
    .concat()
}

fn plural(count: u32, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// `text` in the five-row block font, one string per row.
fn big_text(text: &str, glyphs: &CardGlyphs) -> Vec<String> {
    let mut rows = vec![String::new(); 5];
    for (i, ch) in text.chars().enumerate() {
        let glyph = match ch {
            '0'..='9' => &BIG_FONT[ch as usize - '0' as usize],
            _ => &BIG_FONT[10],
        };
        for (row, line) in rows.iter_mut().zip(glyph.iter()) {
            if i > 0 {
                row.push(' ');
            }
            for cell in line.chars() {
                row.push_str(if cell == '#' { glyphs.full } else { " " });
            }
        }
    }
    rows
}

/// The bar chart, top row first. Each month is a two-column bar scaled to
/// its AI share, in eighths of a row; months without commits show a dot.
fn chart_rows(summary: &WrappedSummary, glyphs: &CardGlyphs) -> Vec<Vec<Segment>> {
    let heights: Vec<Option<usize>> = summary
        .by_month
        .iter()
        .map(|month| {
            if month.commits == 0 {
                return None;
            }
            let eighths = (month.ai_percent / 100.0 * (CHART_ROWS * 8) as f64).round() as usize;
            let eighths = if month.ai_percent > 0.0 {
                eighths.clamp(1, CHART_ROWS * 8)
            } else {
                0
            };
            Some(eighths)
        })
        .collect();

    (0..CHART_ROWS)
        .rev()
        .map(|from_bottom| {
            let mut segments = Vec::new();
            for (i, height) in heights.iter().enumerate() {
                let pad = if i + 1 < MONTHS.len() { "  " } else { "" };
                let cell: String = match height {
                    None if from_bottom == 0 => {
                        segments.push(dim(format!("{} {pad}", glyphs.empty_month)));
                        continue;
                    }
                    Some(0) if from_bottom == 0 => {
                        segments.push(dim(format!("{}{pad}", glyphs.zero_month.repeat(2))));
                        continue;
                    }
                    None => "  ".to_string(),
                    Some(eighths) => {
                        let full = eighths / 8;
                        let part = eighths % 8;
                        if from_bottom < full {
                            glyphs.full.repeat(2)
                        } else if from_bottom == full && part > 0 {
                            glyphs.eighths[part - 1].repeat(2)
                        } else {
                            "  ".to_string()
                        }
                    }
                };
                segments.push(accent(format!("{cell}{pad}")));
            }
            segments
        })
        .collect()
}

/// `n` with thousands separators.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `text` cut to `width` characters with a trailing ellipsis.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('\u{2026}');
    out
}

/// A path cut from the left so its file name stays visible.
fn fit_path(path: &str, width: usize) -> String {
    if path.chars().count() <= width {
        return path.to_string();
    }
    let keep = width.saturating_sub(1);
    let tail: String = path
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("\u{2026}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::wrapped::{
        COMMIT_CAP, LongestPrompt, MonthStats, RankedFile, RankedName, WRAPPED_SCHEMA_VERSION,
    };
    use crate::ui::theme::ColorLevel;

    fn caps(color: ColorLevel, charset: Charset) -> TermCaps {
        TermCaps {
            tty: true,
            color,
            charset,
            width: 100,
        }
    }

    fn month(month: u32, ai: u64, added: u64, commits: u32) -> MonthStats {
        MonthStats {
            month,
            ai_accepted: ai,
            added_lines: added,
            ai_percent: if added == 0 {
                0.0
            } else {
                (ai as f64 / added as f64 * 10000.0).round() / 100.0
            },
            commits,
        }
    }

    fn summary() -> WrappedSummary {
        let by_month = vec![
            month(1, 120, 400, 9),
            month(2, 300, 500, 12),
            month(3, 0, 80, 2),
            month(4, 0, 0, 0),
            month(5, 900, 1000, 20),
            month(6, 450, 600, 8),
            month(7, 700, 700, 15),
            month(8, 0, 0, 0),
            month(9, 20, 400, 3),
            month(10, 650, 900, 11),
            month(11, 800, 850, 14),
            month(12, 0, 0, 0),
        ];
        WrappedSummary {
            schema_version: WRAPPED_SCHEMA_VERSION,
            autter_version: "test",
            year: 2026,
            author: Some("you@company.com".to_string()),
            all_authors: false,
            commits: 94,
            commits_with_authorship: 90,
            capped: false,
            commit_cap: COMMIT_CAP,
            added_lines: 5430,
            ai_lines: 3940,
            human_lines: 1200,
            untracked_lines: 290,
            ai_percent: 72.56,
            by_month,
            top_agent: Some(RankedName {
                name: "claude".to_string(),
                lines: 3100,
            }),
            top_model: Some(RankedName {
                name: "claude-opus-5-5".to_string(),
                lines: 2400,
            }),
            agents: vec![],
            models: vec![],
            most_prompted_file: Some(RankedFile {
                path: "src/commands/autter_handlers.rs".to_string(),
                prompts: 42,
            }),
            files: vec![],
            prompts_total: 310,
            prompts_with_transcript: 280,
            longest_prompt: Some(LongestPrompt {
                chars: 1834,
                words: 312,
                agent: "claude".to_string(),
                model: "claude-opus-5-5".to_string(),
                commit: "abc123".to_string(),
                snippet: None,
            }),
            busiest_day: Some("Tuesday".to_string()),
            streak_days: 14,
        }
    }

    #[test]
    fn card_without_color() {
        insta::assert_snapshot!(render(
            &summary(),
            &caps(ColorLevel::None, Charset::Unicode)
        ));
    }

    #[test]
    fn card_in_truecolor() {
        insta::assert_snapshot!(render(
            &summary(),
            &caps(ColorLevel::TrueColor, Charset::Unicode)
        ));
    }

    #[test]
    fn card_in_ascii() {
        insta::assert_snapshot!(render(&summary(), &caps(ColorLevel::None, Charset::Ascii)));
    }

    #[test]
    fn every_card_line_is_sixty_columns() {
        let text = render(&summary(), &caps(ColorLevel::None, Charset::Unicode));
        let card_lines: Vec<&str> = text
            .lines()
            .skip_while(|line| !line.starts_with('┌'))
            .collect();
        assert!(card_lines.len() > 20, "{text}");
        for line in card_lines {
            assert_eq!(line.chars().count(), CARD_WIDTH, "{line:?}");
        }
    }

    #[test]
    fn snippet_is_shown_only_when_present() {
        let mut with_snippet = summary();
        with_snippet.longest_prompt.as_mut().unwrap().snippet =
            Some("Please refactor the\nconfig loader so it validates every field before use and reports all errors at once, not just the first".to_string());
        let text = render(&with_snippet, &caps(ColorLevel::None, Charset::Unicode));
        assert!(
            text.contains("\u{201C}Please refactor the config loader"),
            "{text}"
        );
        assert!(text.contains('\u{2026}'), "long snippets are cut: {text}");
        let without = render(&summary(), &caps(ColorLevel::None, Charset::Unicode));
        assert!(!without.contains('\u{201C}'), "{without}");
    }

    #[test]
    fn empty_state_when_too_few_commits() {
        let mut sparse = summary();
        sparse.commits = 2;
        sparse.commits_with_authorship = 1;
        let text = render(&sparse, &caps(ColorLevel::None, Charset::Unicode));
        assert_eq!(
            text,
            "◇  Autter Wrapped 2026\n\
             │  1 commit with an authorship note in 2026. A card needs at least 3.\n\
             │  Commit with an autter-tracked agent, then run autter wrapped again.\n\
             └  autter.dev/wrapped\n"
        );
        sparse.commits = 0;
        sparse.commits_with_authorship = 0;
        let text = render(&sparse, &caps(ColorLevel::None, Charset::Unicode));
        assert!(text.contains("No commits in 2026 yet."), "{text}");
    }

    #[test]
    fn big_font_covers_every_digit() {
        let rows = big_text("0123456789%", &UNICODE_CARD);
        assert_eq!(rows.len(), 5);
        for row in &rows {
            assert_eq!(row.chars().count(), 11 * 5 + 10);
        }
    }

    #[test]
    fn numbers_get_thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn paths_are_cut_from_the_left() {
        assert_eq!(fit_path("src/main.rs", 20), "src/main.rs");
        assert_eq!(
            fit_path("src/commands/autter_handlers.rs", 12),
            "…handlers.rs"
        );
        assert_eq!(fit("abcdef", 4), "abc…");
    }
}
