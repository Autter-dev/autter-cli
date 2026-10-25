//! Cursor-style JSONL transcripts (one JSON object per line, append-only),
//! with sweep discovery. One reader serves three agents:
//!
//! - Cursor: `~/.cursor/projects/**/*.jsonl` (written when Cursor's
//!   transcripts setting is on), plus the transcripts autter copies out of
//!   Cursor's own database when it is off (`cursor_state`).
//! - Firebender: Cursor-compatible hooks and transcripts,
//!   `~/.firebender/projects/**/agent-<conversationId>.jsonl`.
//! - Antigravity: `~/.gemini/antigravity*/brain/<conversationId>/.system_generated/logs/transcript.jsonl`.
//!   Its record format is undocumented, so lines are uploaded as written and the
//!   backend reads the shapes it recognises.

use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
use crate::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::streams::types::{StreamBatch, StreamError};
use crate::streams::watermark::{ByteOffsetWatermark, WatermarkStrategy};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Which agent's JSONL transcripts a [`CursorAgent`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlFlavor {
    Cursor,
    Firebender,
    Antigravity,
}

/// Antigravity hooks may not run in every build (the IDE), so its transcripts
/// are also found by a one-minute sweep.
pub const ANTIGRAVITY_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

impl JsonlFlavor {
    pub fn tool(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::Firebender => "firebender",
            Self::Antigravity => "antigravity",
        }
    }

    fn format(self) -> StreamFormat {
        match self {
            Self::Cursor => StreamFormat::CursorJsonl,
            Self::Firebender => StreamFormat::FirebenderJsonl,
            Self::Antigravity => StreamFormat::AntigravityJsonl,
        }
    }

    /// The agent's conversation id for a transcript file: the file stem for
    /// Cursor, the stem without `agent-` for Firebender, and the `brain/<id>`
    /// directory for Antigravity.
    pub fn external_session_id(self, path: &Path) -> Option<String> {
        match self {
            Self::Cursor => path.file_stem()?.to_str().map(str::to_string),
            Self::Firebender => {
                let stem = path.file_stem()?.to_str()?;
                Some(stem.strip_prefix("agent-").unwrap_or(stem).to_string())
            }
            Self::Antigravity => path
                .parent()? // logs
                .parent()? // .system_generated
                .parent()? // <conversationId>
                .file_name()?
                .to_str()
                .map(str::to_string),
        }
    }
}

/// Reads Cursor-style JSONL transcripts for one agent ([`JsonlFlavor`]).
pub struct CursorAgent {
    batch_size: usize,
    flavor: JsonlFlavor,
}

impl CursorAgent {
    pub fn new() -> Self {
        Self::for_flavor(JsonlFlavor::Cursor)
    }

    pub fn firebender() -> Self {
        Self::for_flavor(JsonlFlavor::Firebender)
    }

    pub fn antigravity() -> Self {
        Self::for_flavor(JsonlFlavor::Antigravity)
    }

    fn for_flavor(flavor: JsonlFlavor) -> Self {
        Self {
            batch_size: 1000,
            flavor,
        }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self {
            batch_size,
            flavor: JsonlFlavor::Cursor,
        }
    }

    /// Scan for this agent's transcript files in their standard locations.
    fn scan_conversation_files(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        match self.flavor {
            JsonlFlavor::Cursor => {
                let base_dir = if let Ok(config_dir) = std::env::var("CURSOR_CONFIG_DIR") {
                    Some(PathBuf::from(config_dir))
                } else {
                    dirs::home_dir().map(|p| p.join(".cursor"))
                };
                let search_dirs = vec![
                    base_dir.as_ref().map(|p| p.join("projects")),
                    crate::streams::cursor_state::exported_transcripts_dir(),
                ];
                for dir in search_dirs.into_iter().flatten() {
                    if dir.exists() {
                        Self::scan_jsonl_recursive(&dir, &mut paths);
                    }
                }
            }
            JsonlFlavor::Firebender => {
                let base_dir = if let Ok(config_dir) = std::env::var("FIREBENDER_CONFIG_DIR") {
                    Some(PathBuf::from(config_dir))
                } else {
                    dirs::home_dir().map(|p| p.join(".firebender"))
                };
                if let Some(dir) = base_dir.map(|p| p.join("projects"))
                    && dir.exists()
                {
                    Self::scan_jsonl_recursive(&dir, &mut paths);
                }
            }
            JsonlFlavor::Antigravity => {
                let base = if let Ok(dir) = std::env::var("ANTIGRAVITY_GEMINI_DIR") {
                    Some(PathBuf::from(dir))
                } else {
                    dirs::home_dir().map(|p| p.join(".gemini"))
                };
                if let Some(base) = base {
                    let pattern = base
                        .join("antigravity*/brain/*/.system_generated/logs/transcript.jsonl")
                        .to_string_lossy()
                        .to_string();
                    if let Ok(entries) = glob::glob(&pattern) {
                        paths.extend(entries.flatten().filter(|p| p.is_file()));
                    }
                }
            }
        }
        paths
    }

    fn scan_jsonl_recursive(dir: &Path, paths: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::scan_jsonl_recursive(&path, paths);
            } else if path.is_file() && path.extension().map(|ext| ext == "jsonl").unwrap_or(false)
            {
                paths.push(path);
            }
        }
    }
}

impl Default for CursorAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for CursorAgent {
    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        match self.flavor {
            JsonlFlavor::Antigravity => SweepStrategy::Periodic(ANTIGRAVITY_SWEEP_INTERVAL),
            // Hooks wake the reader; the full sweep catches anything they missed.
            _ => SweepStrategy::Periodic(Duration::from_secs(30 * 60)),
        }
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        let paths = self.scan_conversation_files();
        let mut sessions = Vec::new();
        let tool = self.flavor.tool();

        for path in paths {
            let Some(external_session_id) = self.flavor.external_session_id(&path) else {
                continue;
            };
            let session_id = generate_session_id(&external_session_id, tool);

            let session = DiscoveredSession {
                session_id,
                tool: tool.to_string(),
                stream_path: path,
                external_session_id,
                external_parent_session_id: None,
            };

            sessions.push(session);
        }

        Ok(sessions)
    }

    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError> {
        use std::fs::File;
        use std::io::{BufReader, Seek, SeekFrom};

        let byte_watermark = watermark
            .as_any()
            .downcast_ref::<ByteOffsetWatermark>()
            .ok_or_else(|| StreamError::Fatal {
                message: format!(
                    "Cursor reader requires ByteOffsetWatermark, got incompatible type for session {}",
                    session_id
                ),
            })?;

        let start_offset = byte_watermark.0;

        let file = File::open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StreamError::Fatal {
                    message: format!("Transcript file not found: {}", path.display()),
                }
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                StreamError::Fatal {
                    message: format!("Permission denied reading transcript: {}", path.display()),
                }
            } else {
                StreamError::Transient {
                    message: format!("Failed to open transcript file: {}", e),
                    retry_after: std::time::Duration::from_secs(5),
                }
            }
        })?;

        let mut reader = BufReader::new(file);

        reader
            .seek(SeekFrom::Start(start_offset))
            .map_err(|e| StreamError::Transient {
                message: format!("Failed to seek to offset {}: {}", start_offset, e),
                retry_after: std::time::Duration::from_secs(5),
            })?;

        let batch_limit = self.batch_size_hint();
        let mut events = Vec::with_capacity(batch_limit);
        let mut current_offset = start_offset;
        let mut line_number = 0;

        let mut line = String::new();
        loop {
            match crate::streams::types::read_jsonl_line(&mut reader, &mut line).map_err(|e| {
                StreamError::Transient {
                    message: format!("I/O error reading line: {}", e),
                    retry_after: std::time::Duration::from_secs(5),
                }
            })? {
                crate::streams::types::JsonlLineState::Eof => break,
                crate::streams::types::JsonlLineState::Partial => break,
                crate::streams::types::JsonlLineState::Complete(bytes_read) => {
                    line_number += 1;
                    current_offset += bytes_read as u64;
                }
            }

            if line.trim().is_empty() {
                continue;
            }

            let entry: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        line = line_number,
                        path = %path.display(),
                        error = %e,
                        "skipping malformed JSON line"
                    );
                    continue;
                }
            };

            events.push(entry);
            if events.len() >= batch_limit {
                break;
            }
        }

        let new_watermark = Box::new(ByteOffsetWatermark::new(current_offset));

        Ok(StreamBatch {
            events,
            new_watermark,
        })
    }

    fn extract_event_timestamp(
        &self,
        _event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        crate::streams::agent::file_time_fallback(file_meta, is_first_event)
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = self.flavor.format();
        vec![StreamDescriptor {
            stream_kind: "transcript",
            format,
            watermark_type: format.watermark_type(),
            path_resolver: PathResolverKind::Identity,
            shared: false,
            watermark_type_resolver: None,
            format_resolver: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sweep_strategy() {
        let agent = CursorAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_jsonl_line(i: usize) -> String {
        format!(
            r#"{{"role":"user","id":{},"message":{{"content":[{{"type":"text","text":"msg-{}"}}]}}}}"#,
            i, i
        )
    }

    fn drain_all(
        agent: &CursorAgent,
        path: &Path,
    ) -> (Vec<serde_json::Value>, Box<dyn WatermarkStrategy>) {
        let mut all = Vec::new();
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(ByteOffsetWatermark::new(0));
        loop {
            let batch = agent.read_incremental(path, wm, "test").unwrap();
            if batch.events.is_empty() {
                wm = batch.new_watermark;
                break;
            }
            all.extend(batch.events);
            wm = batch.new_watermark;
        }
        (all, wm)
    }

    #[test]
    fn test_batch_resume_no_loss_or_repeat() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..5 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CursorAgent::with_batch_size(2);
        let (events, _) = drain_all(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<u64> = events.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_append_one_record_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CursorAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, file.path());
        assert_eq!(all.len(), 3);

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        writeln!(f, "{}", make_jsonl_line(3)).unwrap();
        f.flush().unwrap();

        let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["id"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_append_several_records_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CursorAgent::with_batch_size(2);
        let (_, mut wm) = drain_all(&agent, file.path());

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        for i in 3..6 {
            writeln!(f, "{}", make_jsonl_line(i)).unwrap();
        }
        f.flush().unwrap();

        let mut new_events = Vec::new();
        loop {
            let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
            wm = batch.new_watermark;
            if batch.events.is_empty() {
                break;
            }
            new_events.extend(batch.events);
        }
        assert_eq!(new_events.len(), 3);
        let ids: Vec<u64> = new_events
            .iter()
            .map(|e| e["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    #[test]
    fn test_read_incremental_basic() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"role":"user","message":{{"content":[{{"type":"text","text":"Hello"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"role":"assistant","message":{{"content":[{{"type":"text","text":"Hi there"}}]}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = CursorAgent::new();
        let watermark = Box::new(ByteOffsetWatermark::new(0));
        let result = agent
            .read_incremental(file.path(), watermark, "test-session")
            .unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["role"].as_str(), Some("user"));
        assert_eq!(result.events[1]["role"].as_str(), Some("assistant"));
    }
}

#[cfg(test)]
mod flavor_tests {
    use super::*;

    #[test]
    fn names_each_agents_conversation() {
        assert_eq!(
            JsonlFlavor::Cursor
                .external_session_id(Path::new(
                    "/h/.cursor/projects/p/agent-transcripts/k1.jsonl"
                ))
                .as_deref(),
            Some("k1")
        );
        assert_eq!(
            JsonlFlavor::Firebender
                .external_session_id(Path::new(
                    "/h/.firebender/projects/p/transcripts/agent-fb-1.jsonl"
                ))
                .as_deref(),
            Some("fb-1")
        );
        assert_eq!(
            JsonlFlavor::Antigravity
                .external_session_id(Path::new(
                    "/h/.gemini/antigravity-ide/brain/ec33/.system_generated/logs/transcript.jsonl"
                ))
                .as_deref(),
            Some("ec33")
        );
    }

    #[test]
    fn firebender_and_antigravity_have_readers() {
        for (tool, format) in [
            ("firebender", StreamFormat::FirebenderJsonl),
            ("antigravity", StreamFormat::AntigravityJsonl),
        ] {
            let agent = crate::streams::agent::get_agent(tool).expect(tool);
            assert_eq!(agent.streams()[0].format, format, "{tool}");
            assert!(
                crate::streams::agent::get_all_agents()
                    .iter()
                    .any(|(name, _)| name == tool),
                "{tool} is swept"
            );
        }
        // Antigravity hooks may not run (the IDE), so it is swept every minute.
        assert_eq!(
            CursorAgent::antigravity().sweep_strategy().fast_interval(),
            Some(ANTIGRAVITY_SWEEP_INTERVAL)
        );
        assert_eq!(
            CursorAgent::firebender().sweep_strategy().fast_interval(),
            None
        );
    }

    #[test]
    #[serial_test::serial]
    fn discovers_antigravity_transcripts_in_every_app_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        for app in ["antigravity", "antigravity-cli", "antigravity-ide"] {
            let logs = tmp
                .path()
                .join(app)
                .join("brain")
                .join(format!("{app}-conv"))
                .join(".system_generated/logs");
            std::fs::create_dir_all(&logs).unwrap();
            std::fs::write(logs.join("transcript.jsonl"), "{\"role\":\"user\"}\n").unwrap();
        }
        // The Gemini CLI's own files are not Antigravity's.
        std::fs::create_dir_all(tmp.path().join("tmp")).unwrap();
        std::fs::write(tmp.path().join("tmp/session.jsonl"), "{}\n").unwrap();

        unsafe { std::env::set_var("ANTIGRAVITY_GEMINI_DIR", tmp.path()) };
        let sessions = CursorAgent::antigravity().discover_sessions().unwrap();
        unsafe { std::env::remove_var("ANTIGRAVITY_GEMINI_DIR") };

        let mut ids: Vec<_> = sessions
            .iter()
            .map(|s| (s.tool.as_str(), s.external_session_id.as_str()))
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                ("antigravity", "antigravity-cli-conv"),
                ("antigravity", "antigravity-conv"),
                ("antigravity", "antigravity-ide-conv"),
            ]
        );
    }
}
