use std::collections::HashSet;

use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::error::AutterError;
use crate::git::notes_api::{
    commits_with_notes as commits_with_authorship_notes,
    read_note_blob_oids as note_blob_oids_for_commits,
};
#[cfg(test)]
use crate::git::repository::exec_git;
use crate::git::repository::{Repository, exec_git_stdin};

pub async fn load_ai_touched_files_for_commits(
    repo: &Repository,
    commit_shas: Vec<String>,
) -> Result<HashSet<String>, AutterError> {
    let repo = repo.clone();

    smol::unblock(move || {
        if commit_shas.is_empty() {
            return Ok(HashSet::new());
        }

        let note_blob_map = note_blob_oids_for_commits(&repo, &commit_shas)?;
        if note_blob_map.is_empty() {
            return Ok(HashSet::new());
        }

        let mut unique_blob_oids = HashSet::new();
        for blob_oid in note_blob_map.values() {
            unique_blob_oids.insert(blob_oid.clone());
        }
        let mut blob_oids: Vec<String> = unique_blob_oids.into_iter().collect();
        blob_oids.sort();

        let blob_contents = batch_read_blobs_with_oids(&repo.global_args_for_exec(), &blob_oids)?;

        let mut all_files = HashSet::new();
        for blob_oid in note_blob_map.into_values() {
            if let Some(content) = blob_contents.get(&blob_oid) {
                extract_file_paths_from_note(content, &mut all_files);
            }
        }

        Ok(all_files)
    })
    .await
}

/// Return true if any of the provided commits has an authorship note attached.
pub fn commits_have_authorship_notes(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<bool, AutterError> {
    if commit_shas.is_empty() {
        return Ok(false);
    }

    Ok(!commits_with_authorship_notes(repo, commit_shas)?.is_empty())
}

fn batch_read_blobs_with_oids(
    global_args: &[String],
    blob_oids: &[String],
) -> Result<std::collections::HashMap<String, String>, AutterError> {
    if blob_oids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let mut args = global_args.to_vec();
    args.push("cat-file".to_string());
    args.push("--batch".to_string());

    let stdin_data = blob_oids.join("\n") + "\n";
    let output = exec_git_stdin(&args, stdin_data.as_bytes())?;

    parse_cat_file_batch_output_with_oids(&output.stdout)
}

fn parse_cat_file_batch_output_with_oids(
    data: &[u8],
) -> Result<std::collections::HashMap<String, String>, AutterError> {
    let mut results = std::collections::HashMap::new();
    let mut pos = 0usize;

    while pos < data.len() {
        let header_end = match data[pos..].iter().position(|&b| b == b'\n') {
            Some(idx) => pos + idx,
            None => break,
        };

        let header = std::str::from_utf8(&data[pos..header_end])?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() < 2 {
            pos = header_end + 1;
            continue;
        }

        let oid = parts[0].to_string();
        if parts[1] == "missing" {
            pos = header_end + 1;
            continue;
        }

        if parts.len() < 3 {
            pos = header_end + 1;
            continue;
        }

        let size: usize = parts[2]
            .parse()
            .map_err(|e| AutterError::Generic(format!("Invalid size in cat-file output: {}", e)))?;

        let content_start = header_end + 1;
        let content_end = content_start + size;
        if content_end > data.len() {
            return Err(AutterError::Generic(
                "Malformed cat-file --batch output: truncated content".to_string(),
            ));
        }

        let content = String::from_utf8_lossy(&data[content_start..content_end]).to_string();
        results.insert(oid, content);

        pos = content_end;
        if pos < data.len() && data[pos] == b'\n' {
            pos += 1;
        }
    }

    Ok(results)
}

/// Extract file paths from a note blob content
/// Public wrapper for extracting file paths from a note's attestation section.
pub fn extract_file_paths_from_note_public(content: &str, files: &mut HashSet<String>) {
    extract_file_paths_from_note(content, files);
}

fn extract_file_paths_from_note(content: &str, files: &mut HashSet<String>) {
    // Find the divider and slice before it, then add minimal metadata to make it parseable
    if let Some(divider_pos) = content.find("\n---\n") {
        let attestation_section = &content[..divider_pos];
        // Create a complete parseable format with empty metadata
        let parseable = format!(
            "{}\n---\n{{\"schema_version\":\"authorship/3.0.0\",\"base_commit_sha\":\"\",\"prompts\":{{}}}}",
            attestation_section
        );

        if let Ok(log) = AuthorshipLog::deserialize_from_string(&parseable) {
            for attestation in log.attestations {
                files.insert(attestation.file_path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::find_repository_in_path;

    /// Build a throwaway repo with one commit and an AI authorship note on
    /// `refs/notes/ai`, returning `(repo, commit_sha)`.
    fn repo_with_ai_note(paths: &[&str]) -> (Repository, String) {
        use crate::authorship::authorship_log::LineRange;
        use crate::authorship::authorship_log_serialization::{AttestationEntry, AuthorshipLog};

        // Keep the TempDir alive for the whole test: `Repository` only holds
        // the path, and the caller reads from it after this helper returns.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.keep();

        let git = |args: &[&str]| -> String {
            let mut argv = vec!["-C".to_string(), dir.to_string_lossy().to_string()];
            argv.extend(args.iter().map(|a| a.to_string()));
            let out = exec_git(&argv).unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("file.txt"), "hello\n").unwrap();
        git(&["add", "file.txt"]);
        git(&["commit", "-qm", "add file"]);
        let commit_sha = git(&["rev-parse", "HEAD"]);

        let mut log = AuthorshipLog::new();
        for path in paths {
            log.get_or_create_file(path)
                .entries
                .push(AttestationEntry::new(
                    "abc123hash".to_string(),
                    vec![LineRange::Range(1, 5)],
                ));
        }
        let note = log.serialize_to_string().unwrap();

        let mut argv = vec!["-C".to_string(), dir.to_string_lossy().to_string()];
        argv.extend([
            "notes".to_string(),
            "--ref=ai".to_string(),
            "add".to_string(),
            "-f".to_string(),
            "-F".to_string(),
            "-".to_string(),
            commit_sha.clone(),
        ]);
        let out = exec_git_stdin(&argv, note.as_bytes()).unwrap();
        assert!(
            out.status.success(),
            "failed to attach note: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let repo = find_repository_in_path(&dir.to_string_lossy()).unwrap();
        (repo, commit_sha)
    }

    // Pins the notes backend to git notes for the duration of `f`. Without
    // this the developer's own `notes_backend: { kind: "http" }` config makes
    // `read_note_blob_oids` short-circuit to an empty map and the test asserts
    // nothing. Env vars are process-global, so callers must hold the
    // `serial_test` lock (see the `#[serial]` on the tests below).
    fn with_git_notes_backend(f: impl FnOnce()) {
        const ENV: &str = "AUTTER_NOTES_BACKEND_KIND";
        let old = std::env::var(ENV).ok();
        unsafe { std::env::set_var(ENV, "git_notes") };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        match old {
            Some(v) => unsafe { std::env::set_var(ENV, v) },
            None => unsafe { std::env::remove_var(ENV) },
        }

        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    #[serial_test::serial(notes_db_env)]
    fn test_load_ai_touched_files_reads_paths_from_ai_notes() {
        with_git_notes_backend(|| {
            smol::block_on(async {
                let (repo, commit_sha) = repo_with_ai_note(&["src/ai.rs", "docs/notes.md"]);

                let files = load_ai_touched_files_for_commits(&repo, vec![commit_sha])
                    .await
                    .unwrap();

                assert_eq!(
                    files,
                    HashSet::from(["src/ai.rs".to_string(), "docs/notes.md".to_string()])
                );
            });
        });
    }

    #[test]
    fn test_load_ai_touched_files_for_nonexistent_commit() {
        smol::block_on(async {
            let repo = find_repository_in_path(".").unwrap();

            // Use a fake SHA that doesn't exist
            let fake_commits = vec![
                "0000000000000000000000000000000000000000".to_string(),
                "1111111111111111111111111111111111111111".to_string(),
            ];

            let files = load_ai_touched_files_for_commits(&repo, fake_commits)
                .await
                .unwrap();

            // Should return empty set, not crash
            assert!(
                files.is_empty(),
                "Should return empty set for non-existent commits"
            );
        });
    }

    #[test]
    fn test_load_ai_touched_files_empty_commits() {
        smol::block_on(async {
            let repo = find_repository_in_path(".").unwrap();

            let files = load_ai_touched_files_for_commits(&repo, vec![])
                .await
                .unwrap();

            assert!(files.is_empty(), "Should return empty set for empty input");
        });
    }

    #[test]
    fn test_commits_have_authorship_notes_empty() {
        let repo = find_repository_in_path(".").unwrap();

        let result = commits_have_authorship_notes(&repo, &[]).unwrap();

        assert!(!result, "Empty list should return false");
    }

    #[test]
    fn test_commits_have_authorship_notes_nonexistent() {
        let repo = find_repository_in_path(".").unwrap();

        let fake_commits = vec![
            "0000000000000000000000000000000000000000".to_string(),
            "1111111111111111111111111111111111111111".to_string(),
        ];

        let result = commits_have_authorship_notes(&repo, &fake_commits).unwrap();

        // Non-existent commits don't have notes
        assert!(!result);
    }

    #[test]
    fn test_parse_cat_file_batch_output_empty() {
        let result = parse_cat_file_batch_output_with_oids(b"").unwrap();
        assert!(result.is_empty(), "Empty input should return empty map");
    }

    #[test]
    fn test_parse_cat_file_batch_output_missing() {
        let data = b"abc123 missing\n";
        let result = parse_cat_file_batch_output_with_oids(data).unwrap();
        assert!(
            result.is_empty(),
            "Missing blobs should not be included in result"
        );
    }

    #[test]
    fn test_parse_cat_file_batch_output_single_blob() {
        let data = b"abc123 blob 11\nhello world\n";
        let result = parse_cat_file_batch_output_with_oids(data).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("abc123"), Some(&"hello world".to_string()));
    }

    #[test]
    fn test_parse_cat_file_batch_output_multiple_blobs() {
        let data = b"abc123 blob 5\nhello\ndef456 blob 5\nworld\n";
        let result = parse_cat_file_batch_output_with_oids(data).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result.get("abc123"), Some(&"hello".to_string()));
        assert_eq!(result.get("def456"), Some(&"world".to_string()));
    }

    #[test]
    fn test_parse_cat_file_batch_output_truncated() {
        // Size says 20 bytes but only 5 provided
        let data = b"abc123 blob 20\nhello";
        let result = parse_cat_file_batch_output_with_oids(data);
        assert!(result.is_err(), "Truncated content should return error");
    }

    #[test]
    fn test_parse_cat_file_batch_output_invalid_size() {
        let data = b"abc123 blob notanumber\n";
        let result = parse_cat_file_batch_output_with_oids(data);
        assert!(result.is_err(), "Invalid size should return error");
    }

    #[test]
    fn test_parse_cat_file_batch_output_malformed_header() {
        let data = b"abc123\n";
        let result = parse_cat_file_batch_output_with_oids(data).unwrap();
        assert!(result.is_empty(), "Malformed header should skip that entry");
    }

    #[test]
    fn test_batch_read_blobs_with_oids_empty() {
        let repo = find_repository_in_path(".").unwrap();
        let result = batch_read_blobs_with_oids(&repo.global_args_for_exec(), &[]).unwrap();
        assert!(result.is_empty(), "Empty OID list should return empty map");
    }

    #[test]
    fn test_extract_file_paths_from_note_empty() {
        let mut files = HashSet::new();
        extract_file_paths_from_note("", &mut files);
        assert!(files.is_empty(), "Empty note should extract no files");
    }

    #[test]
    fn test_extract_file_paths_from_note_no_divider() {
        let mut files = HashSet::new();
        extract_file_paths_from_note("some content without divider", &mut files);
        assert!(
            files.is_empty(),
            "Note without divider should extract no files"
        );
    }

    #[test]
    fn test_extract_file_paths_from_note_invalid_format() {
        let mut files = HashSet::new();
        let content = "invalid attestation\n---\n{\"metadata\":\"test\"}";
        extract_file_paths_from_note(content, &mut files);
        // Should not crash, might extract nothing or handle gracefully
        // This tests error handling path
    }
}
