//! `autter blame -i <file>`: the file with a colored gutter per line (AI,
//! human, untracked), details for the line under the cursor, and the prompt
//! transcript behind an AI line on Enter.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::{Palette, View, ago, scroll_to_keep_visible, step};
use crate::authorship::authorship_log::PromptRecord;
use crate::authorship::transcript::Message;
use crate::commands::blame::{AutterBlameOptions, BlameAnalysisResult, BlameHunk};
use crate::error::AutterError;
use crate::git::repository::Repository;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// Keyed by prompt hash.
    Ai(String),
    Human,
    Untracked,
}

/// What the prompt overlay is showing.
enum Overlay {
    None,
    /// Enter was pressed; the transcript loads on the next tick, after a
    /// frame with "Loading…" has been drawn.
    Loading(String),
    Prompt {
        lines: Vec<Line<'static>>,
        scroll: u16,
    },
}

struct BlameView {
    path: String,
    lines: Vec<String>,
    kinds: Vec<Kind>,
    prompts: HashMap<String, PromptRecord>,
    hunks: Vec<BlameHunk>,
    cursor: usize,
    offset: usize,
    overlay: Overlay,
    now: i64,
    palette: Palette,
}

pub fn run(
    repo: &Repository,
    file_path: &str,
    options: &AutterBlameOptions,
) -> Result<(), AutterError> {
    let (path, lines, analysis) = repo.blame_for_browser(file_path, options)?;
    if lines.is_empty() {
        return Err(AutterError::Generic(format!("{path} is empty.")));
    }
    let kinds = classify(lines.len(), &analysis);
    let mut view = BlameView {
        path,
        lines,
        kinds,
        prompts: analysis.prompt_records,
        hunks: analysis.blame_hunks,
        cursor: 0,
        offset: 0,
        overlay: Overlay::None,
        now: chrono::Utc::now().timestamp(),
        palette: Palette::new(),
    };
    super::run(&mut view)?;
    Ok(())
}

/// One [`Kind`] per line (index 0 is line 1). AI lines carry their prompt
/// hash; `h_` hashes are known human edits; everything else is untracked.
fn classify(line_count: usize, analysis: &BlameAnalysisResult) -> Vec<Kind> {
    (1..=line_count as u32)
        .map(|line| match analysis.line_authors.get(&line) {
            Some(author) if analysis.prompt_records.contains_key(author) => {
                Kind::Ai(author.clone())
            }
            Some(author) if author.starts_with("h_") => Kind::Human,
            _ => Kind::Untracked,
        })
        .collect()
}

impl BlameView {
    fn hunk_for(&self, line: usize) -> Option<&BlameHunk> {
        let line = line as u32 + 1;
        self.hunks
            .iter()
            .find(|hunk| hunk.range.0 <= line && line <= hunk.range.1)
    }

    fn ai_share(&self) -> u32 {
        let ai = self
            .kinds
            .iter()
            .filter(|kind| matches!(kind, Kind::Ai(_)))
            .count();
        (ai as f64 * 100.0 / self.kinds.len() as f64).round() as u32
    }

    /// The next (or previous) AI line after the cursor that starts a new run.
    fn next_ai(&self, forward: bool) -> Option<usize> {
        let is_ai = |i: usize| matches!(self.kinds[i], Kind::Ai(_));
        let starts_run = |i: usize| is_ai(i) && (i == 0 || !is_ai(i - 1));
        if forward {
            (self.cursor + 1..self.lines.len()).find(|&i| starts_run(i))
        } else {
            (0..self.cursor).rev().find(|&i| starts_run(i))
        }
    }

    fn style_for(&self, kind: &Kind) -> ratatui::style::Style {
        match kind {
            Kind::Ai(_) => self.palette.ai,
            Kind::Human => self.palette.human,
            Kind::Untracked => self.palette.untracked,
        }
    }

    fn draw_file(&mut self, frame: &mut Frame, area: Rect) {
        let p = &self.palette;
        let height = area.height.saturating_sub(2) as usize;
        self.offset = scroll_to_keep_visible(self.offset, self.cursor, height);
        let number_width = self.lines.len().to_string().len();

        let rows: Vec<Line> = (self.offset..self.lines.len().min(self.offset + height))
            .map(|i| {
                let kind = &self.kinds[i];
                let text = self.lines[i].replace('\t', "    ");
                let mut line = Line::from(vec![
                    Span::styled(p.gutter, self.style_for(kind)),
                    Span::styled(format!(" {:>number_width$} ", i + 1), p.dim),
                    Span::raw(text),
                ]);
                if i == self.cursor {
                    line = line.style(p.selected);
                }
                line
            })
            .collect();

        let title = Line::from(vec![
            Span::styled(format!(" {} ", self.path), p.title),
            Span::styled(
                format!(
                    "\u{00B7} {} lines \u{00B7} {}% AI ",
                    self.lines.len(),
                    self.ai_share()
                ),
                p.dim,
            ),
        ]);
        frame.render_widget(
            Paragraph::new(rows).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(p.dim)
                    .title(title),
            ),
            area,
        );
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect) {
        let p = &self.palette;
        let kind = &self.kinds[self.cursor];
        let mut first = match kind {
            Kind::Ai(hash) => {
                let mut spans = vec![Span::styled("AI", p.ai)];
                if let Some(prompt) = self.prompts.get(hash) {
                    spans.push(Span::raw(format!(
                        " \u{00B7} {} \u{00B7} {}",
                        prompt.agent_id.tool, prompt.agent_id.model
                    )));
                }
                spans
            }
            Kind::Human => vec![Span::raw("Human \u{00B7} typed in an editor")],
            Kind::Untracked => vec![Span::styled(
                "Untracked \u{00B7} no attribution recorded",
                p.untracked,
            )],
        };
        if let Some(hunk) = self.hunk_for(self.cursor) {
            first.push(Span::styled(
                format!(
                    "   commit {} \u{00B7} {} \u{00B7} {}",
                    hunk.abbrev_sha,
                    hunk.original_author,
                    ago(self.now - hunk.author_time)
                ),
                p.dim,
            ));
        }
        let mut lines = vec![Line::from(first)];
        if let Kind::Ai(hash) = kind
            && let Some(prompt) = self.prompts.get(hash)
        {
            let short: String = hash.chars().take(12).collect();
            let mut second = vec![Span::styled(
                format!(
                    "prompt {short} \u{00B7} {} lines accepted",
                    prompt.accepted_lines
                ),
                p.dim,
            )];
            if prompt.messages_url.is_some() {
                second.push(Span::styled("   enter", p.title));
                second.push(Span::styled(" show the prompt", p.dim));
            }
            lines.push(Line::from(second));
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(p.dim)
                    .title(Span::styled(format!(" Line {} ", self.cursor + 1), p.title)),
            ),
            area,
        );
    }

    fn draw_overlay(&self, frame: &mut Frame) {
        let p = &self.palette;
        let (lines, scroll) = match &self.overlay {
            Overlay::None => return,
            Overlay::Loading(_) => (
                vec![Line::from(Span::styled(
                    "Loading the prompt\u{2026}",
                    p.dim,
                ))],
                0,
            ),
            Overlay::Prompt { lines, scroll } => (lines.clone(), *scroll),
        };
        let area = frame.area();
        let popup = Rect {
            x: area.width / 10,
            y: area.height / 10,
            width: area.width - area.width / 5,
            height: area.height - area.height / 5,
        };
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(p.ai)
                        .title(Span::styled(" Prompt ", p.title))
                        .title_bottom(p.help(&[("\u{2191}\u{2193}", "scroll"), ("esc", "close")])),
                ),
            popup,
        );
    }

    fn open_prompt(&mut self) {
        if let Kind::Ai(hash) = &self.kinds[self.cursor]
            && self
                .prompts
                .get(hash)
                .is_some_and(|prompt| prompt.messages_url.is_some())
        {
            self.overlay = Overlay::Loading(hash.clone());
        }
    }

    fn load_prompt(&self, hash: &str) -> Vec<Line<'static>> {
        let p = &self.palette;
        let Some(url) = self
            .prompts
            .get(hash)
            .and_then(|prompt| prompt.messages_url.clone())
        else {
            return vec![Line::from("No prompt transcript is linked to this line.")];
        };
        let messages = match crate::authorship::cas_bridge::resolve_cas_messages(&url) {
            Ok(Some(messages)) if !messages.is_empty() => messages,
            Ok(_) => {
                return vec![Line::from(format!(
                    "The transcript isn't available on this machine ({url})."
                ))];
            }
            Err(e) => return vec![Line::from(format!("Couldn't load the transcript: {e}"))],
        };
        let mut lines = Vec::new();
        for message in &messages {
            let (who, text, style) = match message {
                Message::User { text, .. } => ("You", text, p.title),
                Message::Assistant { text, .. } => ("Agent", text, p.ai),
                Message::Plan { text, .. } => ("Plan", text, p.ai),
                // Thinking and tool calls are noise for "why is this line here".
                Message::Thinking { .. } | Message::ToolUse { .. } => continue,
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            lines.push(Line::from(Span::styled(who, style)));
            lines.extend(text.lines().map(|line| Line::from(line.to_string())));
            lines.push(Line::from(""));
        }
        if lines.is_empty() {
            lines.push(Line::from("The transcript has no messages to show."));
        }
        lines
    }
}

impl View for BlameView {
    fn tick(&mut self) {
        if let Overlay::Loading(hash) = &self.overlay {
            let hash = hash.clone();
            self.overlay = Overlay::Prompt {
                lines: self.load_prompt(&hash),
                scroll: 0,
            };
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [file, detail, help] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(4),
                Constraint::Length(1),
            ])
            .areas(frame.area());
        self.draw_file(frame, file);
        self.draw_detail(frame, detail);
        frame.render_widget(
            Paragraph::new(self.palette.help(&[
                ("\u{2191}\u{2193}", "move"),
                ("n/N", "next/prev AI change"),
                ("enter", "prompt"),
                ("q", "quit"),
            ])),
            help,
        );
        self.draw_overlay(frame);
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        if let Overlay::Prompt { scroll, .. } = &mut self.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.overlay = Overlay::None,
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                _ => {}
            }
            return true;
        }

        let len = self.lines.len();
        self.cursor = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Down | KeyCode::Char('j') => step(self.cursor, 1, len),
            KeyCode::Up | KeyCode::Char('k') => step(self.cursor, -1, len),
            KeyCode::PageDown => step(self.cursor, 20, len),
            KeyCode::PageUp => step(self.cursor, -20, len),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => len.saturating_sub(1),
            KeyCode::Char('n') => self.next_ai(true).unwrap_or(self.cursor),
            KeyCode::Char('N') => self.next_ai(false).unwrap_or(self.cursor),
            KeyCode::Enter => {
                self.open_prompt();
                self.cursor
            }
            _ => self.cursor,
        };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::working_log::AgentId;
    use std::collections::BTreeMap;

    fn prompt() -> PromptRecord {
        PromptRecord {
            agent_id: AgentId {
                tool: "claude".to_string(),
                id: "s1".to_string(),
                model: "opus".to_string(),
            },
            human_author: None,
            messages_url: None,
            total_additions: 2,
            total_deletions: 0,
            accepted_lines: 2,
            overriden_lines: 0,
            custom_attributes: None,
        }
    }

    #[test]
    fn classifies_ai_human_and_untracked_lines() {
        let analysis = BlameAnalysisResult {
            line_authors: HashMap::from([
                (1, "p_abc".to_string()),
                (2, "h_def".to_string()),
                (3, "Unknown".to_string()),
            ]),
            prompt_records: HashMap::from([("p_abc".to_string(), prompt())]),
            session_records: HashMap::new(),
            blame_hunks: Vec::new(),
            humans: BTreeMap::new(),
        };
        assert_eq!(
            classify(4, &analysis),
            [
                Kind::Ai("p_abc".to_string()),
                Kind::Human,
                Kind::Untracked,
                Kind::Untracked,
            ]
        );
    }
}
