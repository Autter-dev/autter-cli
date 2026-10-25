// src/streams/sweep.rs

use std::path::PathBuf;
use std::time::Duration;

/// How often the daemon runs a full sweep across every agent.
pub const FULL_SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Strategy for discovering new/updated sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepStrategy {
    /// Periodic polling at the given interval. Agents asking for less than
    /// [`FULL_SWEEP_INTERVAL`] are also swept on their own, faster ticker
    /// (agents with no hook, whose sessions only a sweep can find).
    Periodic(Duration),
    /// File system watcher (not implemented yet)
    FsWatcher,
    /// HTTP API polling (not implemented yet)
    HttpApi,
    /// No sweep support for this agent
    None,
}

impl SweepStrategy {
    /// The interval of a periodic agent that wants sweeping more often than the full sweep.
    pub fn fast_interval(&self) -> Option<Duration> {
        match self {
            Self::Periodic(d) if *d < FULL_SWEEP_INTERVAL => Some(*d),
            _ => None,
        }
    }
}

/// A session discovered during a sweep.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    pub session_id: String,
    pub tool: String,
    pub stream_path: PathBuf,
    pub external_session_id: String,
    pub external_parent_session_id: Option<String>,
}

/// Transcript file format enum
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    ClaudeJsonl,
    CursorJsonl,
    DroidJsonl,
    CopilotSessionJson,
    CopilotEventStreamJsonl,
    GeminiJsonl,
    ContinueJson,
    WindsurfJsonl,
    CodexJsonl,
    AmpThreadJson,
    OpenCodeSqlite,
    PiJsonl,
    CopilotOtelSqlite,
    /// Firebender's agent transcripts (Cursor-compatible JSONL).
    FirebenderJsonl,
    /// Antigravity's `brain/<conversationId>/.system_generated/logs/transcript.jsonl`.
    AntigravityJsonl,
}

impl std::fmt::Display for StreamFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaudeJsonl => write!(f, "ClaudeJsonl"),
            Self::CursorJsonl => write!(f, "CursorJsonl"),
            Self::DroidJsonl => write!(f, "DroidJsonl"),
            Self::CopilotSessionJson => write!(f, "CopilotSessionJson"),
            Self::CopilotEventStreamJsonl => write!(f, "CopilotEventStreamJsonl"),
            Self::GeminiJsonl => write!(f, "GeminiJsonl"),
            Self::ContinueJson => write!(f, "ContinueJson"),
            Self::WindsurfJsonl => write!(f, "WindsurfJsonl"),
            Self::CodexJsonl => write!(f, "CodexJsonl"),
            Self::AmpThreadJson => write!(f, "AmpThreadJson"),
            Self::OpenCodeSqlite => write!(f, "OpenCodeSqlite"),
            Self::PiJsonl => write!(f, "PiJsonl"),
            Self::CopilotOtelSqlite => write!(f, "CopilotOtelSqlite"),
            Self::FirebenderJsonl => write!(f, "FirebenderJsonl"),
            Self::AntigravityJsonl => write!(f, "AntigravityJsonl"),
        }
    }
}

impl StreamFormat {
    pub fn watermark_type(self) -> super::watermark::WatermarkType {
        use super::watermark::WatermarkType;
        match self {
            Self::ClaudeJsonl
            | Self::CursorJsonl
            | Self::GeminiJsonl
            | Self::WindsurfJsonl
            | Self::CodexJsonl
            | Self::PiJsonl
            | Self::FirebenderJsonl
            | Self::AntigravityJsonl
            | Self::CopilotEventStreamJsonl => WatermarkType::ByteOffset,
            Self::DroidJsonl => WatermarkType::Hybrid,
            Self::CopilotSessionJson | Self::ContinueJson | Self::AmpThreadJson => {
                WatermarkType::RecordIndex
            }
            Self::OpenCodeSqlite => WatermarkType::Timestamp,
            Self::CopilotOtelSqlite => WatermarkType::TimestampCursor,
        }
    }
}

#[cfg(test)]
mod sweep_strategy_tests {
    use super::*;

    #[test]
    fn only_shorter_periodic_intervals_are_fast() {
        assert_eq!(
            SweepStrategy::Periodic(Duration::from_secs(60)).fast_interval(),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            SweepStrategy::Periodic(FULL_SWEEP_INTERVAL).fast_interval(),
            None
        );
        assert_eq!(SweepStrategy::None.fast_interval(), None);
        assert_eq!(SweepStrategy::FsWatcher.fast_interval(), None);
    }
}
