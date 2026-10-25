//! `autter stats -i`: recent commits with their AI share, and the selected
//! commit's breakdown by tool and model. Stats are computed one commit at a
//! time on a background thread, newest first, so the list opens at once.

use std::sync::mpsc::{self, Receiver};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, TableState};

use super::{Palette, View, scroll_to_keep_visible, step};
use crate::authorship::stats::{CommitStats, stats_for_commit_stats};
use crate::commands::home::bar_columns;
use crate::error::AutterError;
use crate::git::repository::{Repository, exec_git, find_repository_in_path};

/// How many commits the view lists.
const COMMIT_LIMIT: usize = 200;
const BAR_WIDTH: usize = 12;

struct CommitRow {
    sha: String,
    short: String,
    author: String,
    when: String,
    subject: String,
}

enum Load {
    Pending,
    Ready(CommitStats),
    Failed,
}

struct StatsView {
    rows: Vec<CommitRow>,
    stats: Vec<Load>,
    results: Receiver<(usize, Load)>,
    table: TableState,
    palette: Palette,
}

/// Open the view on the commits reachable from `start` (default `HEAD`).
pub fn run(repo: &Repository, start: Option<&str>) -> Result<(), AutterError> {
    let rows = recent_commits(repo, start.unwrap_or("HEAD"))?;
    if rows.is_empty() {
        return Err(AutterError::Generic("No commits to show yet.".to_string()));
    }

    let (tx, results) = mpsc::channel();
    let workdir = repo.workdir()?;
    let shas: Vec<String> = rows.iter().map(|row| row.sha.clone()).collect();
    std::thread::spawn(move || {
        let Ok(repo) = find_repository_in_path(&workdir.to_string_lossy()) else {
            return;
        };
        let patterns = crate::authorship::ignore::effective_ignore_patterns(&repo, &[], &[]);
        for (i, sha) in shas.iter().enumerate() {
            let load = match stats_for_commit_stats(&repo, sha, &patterns) {
                Ok(stats) => Load::Ready(stats),
                Err(_) => Load::Failed,
            };
            // The view closed: stop computing.
            if tx.send((i, load)).is_err() {
                return;
            }
        }
    });

    let mut view = StatsView {
        stats: rows.iter().map(|_| Load::Pending).collect(),
        rows,
        results,
        table: TableState::default().with_selected(Some(0)),
        palette: Palette::new(),
    };
    super::run(&mut view)?;
    Ok(())
}

fn recent_commits(repo: &Repository, start: &str) -> Result<Vec<CommitRow>, AutterError> {
    let mut args = repo.global_args_for_exec();
    args.extend(
        [
            "log",
            "--no-color",
            "-n",
            &COMMIT_LIMIT.to_string(),
            "--format=%H%x1f%h%x1f%an%x1f%ar%x1f%s",
            start,
            "--",
        ]
        .map(String::from),
    );
    let output = exec_git(&args)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\u{1f}');
            Some(CommitRow {
                sha: fields.next()?.to_string(),
                short: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                when: fields.next()?.to_string(),
                subject: fields.next().unwrap_or("").to_string(),
            })
        })
        .collect())
}

/// AI, human and untracked shares of a commit's added lines.
fn shares(stats: &CommitStats) -> Option<(u32, u32, u32)> {
    let added = stats.git_diff_added_lines;
    if added == 0 {
        return None;
    }
    let ai = stats.ai_additions.min(added);
    let human = stats.human_additions.min(added - ai);
    Some((human, added - ai - human, ai))
}

fn percent(part: u32, total: u32) -> u32 {
    if total == 0 {
        0
    } else {
        (f64::from(part) * 100.0 / f64::from(total)).round() as u32
    }
}

impl StatsView {
    fn selected(&self) -> usize {
        self.table.selected().unwrap_or(0)
    }

    fn bar(&self, load: &Load) -> Line<'static> {
        let p = &self.palette;
        match load {
            Load::Pending => Line::from(Span::styled("\u{2026}", p.dim)),
            Load::Failed => Line::from(Span::styled("unavailable", p.dim)),
            Load::Ready(stats) => match shares(stats) {
                None => Line::from(Span::styled("no additions", p.dim)),
                Some((human, untracked, ai)) => {
                    let [h, u, a] = bar_columns(human, untracked, ai, BAR_WIDTH);
                    Line::from(vec![
                        Span::styled(p.bar_full.repeat(h), p.human),
                        Span::styled(p.bar_untracked.repeat(u), p.untracked),
                        Span::styled(p.bar_ai.repeat(a), p.ai),
                    ])
                }
            },
        }
    }

    fn percent_cell(&self, load: &Load) -> Span<'static> {
        match load {
            Load::Ready(stats) if stats.git_diff_added_lines > 0 => Span::styled(
                format!(
                    "{:>3}%",
                    percent(stats.ai_additions, stats.git_diff_added_lines)
                ),
                self.palette.ai,
            ),
            _ => Span::raw(""),
        }
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let p = &self.palette;
        let rows: Vec<Row> = self
            .rows
            .iter()
            .zip(&self.stats)
            .map(|(row, load)| {
                Row::new(vec![
                    Line::from(Span::styled(row.short.clone(), p.ai)),
                    Line::from(Span::styled(row.when.clone(), p.dim)),
                    Line::from(row.author.clone()),
                    self.bar(load),
                    Line::from(self.percent_cell(load)),
                    Line::from(row.subject.clone()),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(9),
                Constraint::Length(15),
                Constraint::Length(16),
                Constraint::Length(BAR_WIDTH as u16),
                Constraint::Length(4),
                Constraint::Min(10),
            ],
        )
        .column_spacing(1)
        .row_highlight_style(p.selected)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(p.dim)
                .title(Span::styled(" Recent commits ", p.title)),
        );
        // Keep the selection on screen as it moves.
        let visible = area.height.saturating_sub(2) as usize;
        let offset = scroll_to_keep_visible(self.table.offset(), self.selected(), visible);
        *self.table.offset_mut() = offset;
        frame.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect) {
        let p = &self.palette;
        let row = &self.rows[self.selected()];
        let mut lines = vec![
            Line::from(Span::styled(row.subject.clone(), p.title)),
            Line::from(Span::styled(
                format!("{} \u{00B7} {}", row.author, row.when),
                p.dim,
            )),
            Line::from(""),
        ];
        match &self.stats[self.selected()] {
            Load::Pending => lines.push(Line::from(Span::styled("Counting lines\u{2026}", p.dim))),
            Load::Failed => lines.push(Line::from(Span::styled(
                "Couldn't read this commit's authorship.",
                p.dim,
            ))),
            Load::Ready(stats) => lines.extend(self.breakdown(stats)),
        }
        let detail = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(p.dim)
                .title(Span::styled(format!(" {} ", row.short), p.title)),
        );
        frame.render_widget(detail, area);
    }

    fn breakdown(&self, stats: &CommitStats) -> Vec<Line<'static>> {
        let p = &self.palette;
        let Some((human, untracked, ai)) = shares(stats) else {
            return vec![Line::from(Span::styled(
                format!(
                    "No added lines \u{00B7} {} deleted",
                    stats.git_diff_deleted_lines
                ),
                p.dim,
            ))];
        };
        let added = stats.git_diff_added_lines;
        let row = |label: &str, count: u32, style| {
            Line::from(vec![
                Span::styled(format!("{label:<11}"), style),
                Span::raw(format!("{count:>6} lines  {:>3}%", percent(count, added))),
            ])
        };
        let mut lines = vec![
            row("AI", ai, p.ai),
            row("Human", human, p.human),
            row("Untracked", untracked, p.untracked),
            Line::from(Span::styled(
                format!(
                    "{added} added \u{00B7} {} deleted",
                    stats.git_diff_deleted_lines
                ),
                p.dim,
            )),
        ];
        if !stats.tool_model_breakdown.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("By tool", p.title)));
            let mut tools: Vec<_> = stats.tool_model_breakdown.iter().collect();
            tools.sort_by_key(|(_, headline)| std::cmp::Reverse(headline.ai_additions));
            for (key, headline) in tools {
                let label = key.replace("::", " \u{00B7} ");
                lines.push(Line::from(vec![
                    Span::styled(format!("{label:<24}"), p.ai),
                    Span::raw(format!("{:>6}", headline.ai_additions)),
                ]));
            }
        }
        lines
    }
}

impl View for StatsView {
    fn tick(&mut self) {
        while let Ok((i, load)) = self.results.try_recv() {
            if let Some(slot) = self.stats.get_mut(i) {
                *slot = load;
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [body, help] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .areas(frame.area());
        let wide = body.width >= 110;
        let [list, detail] = Layout::default()
            .direction(if wide {
                Direction::Horizontal
            } else {
                Direction::Vertical
            })
            .constraints(if wide {
                [Constraint::Min(70), Constraint::Length(40)]
            } else {
                [Constraint::Min(6), Constraint::Length(14)]
            })
            .areas(body);
        self.draw_list(frame, list);
        self.draw_detail(frame, detail);
        frame.render_widget(
            Paragraph::new(self.palette.help(&[
                ("\u{2191}\u{2193}", "move"),
                ("g/G", "first/last"),
                ("q", "quit"),
            ])),
            help,
        );
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        let len = self.rows.len();
        let selected = self.selected();
        let next = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Down | KeyCode::Char('j') => step(selected, 1, len),
            KeyCode::Up | KeyCode::Char('k') => step(selected, -1, len),
            KeyCode::PageDown => step(selected, 10, len),
            KeyCode::PageUp => step(selected, -10, len),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => len.saturating_sub(1),
            _ => selected,
        };
        self.table.select(Some(next));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_split_added_lines() {
        let stats = CommitStats {
            human_additions: 30,
            ai_additions: 62,
            git_diff_added_lines: 100,
            ..CommitStats::default()
        };
        assert_eq!(shares(&stats), Some((30, 8, 62)));
        assert_eq!(shares(&CommitStats::default()), None);
        assert_eq!(percent(62, 100), 62);
        assert_eq!(percent(1, 0), 0);
    }
}
