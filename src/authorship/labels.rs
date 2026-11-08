//! The one set of authorship labels every view uses (`stats`, `blame`, `show`,
//! `log`, `status`). Each added line falls into exactly one category.
//!
//! JSON output keeps its existing field names: `ai_additions` is [`LineCategory::Ai`],
//! `human_additions` is [`LineCategory::Human`], and `unknown_additions` is
//! [`LineCategory::Untracked`] plus [`LineCategory::Unknown`]. The record status
//! added to `stats --json` tells those last two apart.

/// Who put a line into the file, as far as Autter recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineCategory {
    /// An AI agent that Autter captures wrote the line.
    Ai,
    /// A person typed the line in an editor with an Autter extension.
    Human,
    /// Autter recorded the commit, but no agent or editor extension recorded
    /// this line. Usually typed by hand in an editor Autter doesn't watch.
    Untracked,
    /// The commit has no Autter record at all, so nothing is known about the line.
    Unknown,
}

impl LineCategory {
    pub const ALL: [LineCategory; 4] = [
        LineCategory::Ai,
        LineCategory::Human,
        LineCategory::Untracked,
        LineCategory::Unknown,
    ];

    /// Short label, used in stats, log, status and the blame marker column.
    pub fn label(self) -> &'static str {
        match self {
            LineCategory::Ai => "AI",
            LineCategory::Human => "human",
            LineCategory::Untracked => "untracked",
            LineCategory::Unknown => "unknown",
        }
    }

    /// One-clause meaning, for legends.
    pub fn meaning(self) -> &'static str {
        match self {
            LineCategory::Ai => "written by an AI agent",
            LineCategory::Human => "typed in an editor with the Autter extension",
            LineCategory::Untracked => {
                "no agent or Autter editor extension recorded it, usually typed by hand"
            }
            LineCategory::Unknown => "the commit has no Autter record",
        }
    }

    /// Width of the widest label, for aligned columns.
    pub fn max_label_width() -> usize {
        Self::ALL
            .iter()
            .map(|category| category.label().len())
            .max()
            .unwrap_or(0)
    }
}

/// What the AI percentage measures, in one line.
pub const AI_SHARE_MEANING: &str = "AI % is the share of added lines an agent wrote into the file, not who directed or reviewed the work.";

/// `label = meaning` for each category present, joined for a one-line legend.
pub fn legend(categories: &[LineCategory]) -> String {
    LineCategory::ALL
        .iter()
        .filter(|category| categories.contains(category))
        .map(|category| format!("{} = {}", category.label(), category.meaning()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Where to read which agents and editors Autter supports.
pub const SUPPORTED_TOOLS_URL: &str =
    "https://github.com/Autter-dev/autter-cli#whats-supported-and-what-isnt";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_distinct_and_lowercase_except_ai() {
        let labels: Vec<&str> = LineCategory::ALL.iter().map(|c| c.label()).collect();
        assert_eq!(labels, vec!["AI", "human", "untracked", "unknown"]);
        assert_eq!(LineCategory::max_label_width(), "untracked".len());
    }

    #[test]
    fn legend_lists_only_present_categories_in_order() {
        let legend = legend(&[LineCategory::Unknown, LineCategory::Ai]);
        assert_eq!(
            legend,
            "AI = written by an AI agent; unknown = the commit has no Autter record"
        );
    }
}
