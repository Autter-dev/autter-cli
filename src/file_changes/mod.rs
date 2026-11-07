//! Track which repository files are changed most often.
//!
//! Counts are aggregated locally in SQLite and synced to the org database when
//! the machine is in connected mode and the user is logged in to Autter Cloud.
//! Local mode keeps the counts on the machine only.

pub mod db;

use crate::error::AutterError;
use crate::git::repository::Repository;
use crate::utils::normalize_to_posix;

pub use db::{FileChangeRow, FileChangesDatabase};

/// Stable key for a repository: canonical remote URL, or local workdir path.
pub fn resolve_repo_key(repo: &Repository) -> String {
    if let Some(url) = crate::repo_url::resolve_repo_url_from_repo(repo) {
        return url;
    }
    normalize_to_posix(&repo.workdir().unwrap_or_default().to_string_lossy())
}

/// Record a file touch from a checkpoint.
pub fn record_checkpoint_file(
    repo: &Repository,
    file_path: &str,
    lines_added: u32,
    lines_deleted: u32,
    changed_at: u64,
) {
    let repo_key = resolve_repo_key(repo);
    let normalized_path = normalize_to_posix(file_path);

    let Ok(db) = FileChangesDatabase::global() else {
        return;
    };
    let Ok(mut lock) = db.lock() else {
        return;
    };
    // Local mode keeps the counts (they power `autter file-changes`) but does
    // not mark them for upload.
    let queue_for_upload = crate::config::platform_sync_enabled_now();
    let _ = lock.record_change(
        &repo_key,
        &normalized_path,
        lines_added,
        lines_deleted,
        changed_at,
        queue_for_upload,
    );
}

/// Top changed files for a repository key.
pub fn top_changed_files(repo_key: &str, limit: usize) -> Result<Vec<FileChangeRow>, AutterError> {
    let db = FileChangesDatabase::global()?;
    let lock = db
        .lock()
        .map_err(|_| AutterError::Generic("file-changes database lock poisoned".to_string()))?;
    lock.top_files(repo_key, limit)
}

/// Upload pending rows to the org database when in connected mode and authenticated.
pub fn flush_pending_to_cloud() {
    use crate::api::client::{
        ApiClient, REPO_NOT_CONNECTED, RepoRoute, UNCONNECTED_RECHECK_SECS, route_for_repo_at,
    };
    use crate::api::types::{FileChangeCount, FileChangeUploadResponse};
    use crate::config;

    let cfg = config::Config::fresh();
    // Local mode never uploads, including rows queued by an earlier connected
    // session.
    if !cfg.platform_sync_enabled() {
        return;
    }
    let backend_url = match cfg.notes_backend_url() {
        Some(url) => url.to_string(),
        None => return,
    };

    // Cheap local check first: with nothing queued, skip building the API
    // client entirely (constructing it can trigger a token refresh).
    let db = match FileChangesDatabase::global() {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(%e, "file-changes: failed to get DB");
            return;
        }
    };
    let pending_count = {
        let Ok(lock) = db.lock() else {
            tracing::warn!("file-changes: DB lock poisoned");
            return;
        };
        lock.count_pending().unwrap_or(0)
    };
    if pending_count == 0 {
        return;
    }

    let default_client = ApiClient::new(crate::api::client::ApiContext::new(Some(
        backend_url.clone(),
    )));
    if !default_client.is_logged_in() && !default_client.has_api_key() {
        crate::daemon::telemetry_worker::note_durable_sync_unauthenticated(
            "file_changes",
            pending_count,
        );
        if let Some(issue) = default_client.auth_issue_to_log() {
            tracing::warn!(reason = %issue, pending = pending_count, "file-changes: sync authentication unavailable");
        }
        return;
    }

    let pending = {
        let Ok(mut lock) = db.lock() else {
            tracing::warn!("file-changes: DB lock poisoned");
            return;
        };
        match lock.dequeue_pending(100) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(%e, "file-changes: failed to dequeue pending rows");
                return;
            }
        }
    };

    if pending.is_empty() {
        return;
    }

    let distinct_id = config::get_or_create_distinct_id();

    // Each repo's counts go only to the org that connected that repo. A repo no
    // org of the user's has connected keeps its counts on this machine.
    let mut groups: std::collections::HashMap<String, Vec<db::PendingFileChangeRow>> =
        std::collections::HashMap::new();

    for row in pending {
        groups.entry(row.repo_url.clone()).or_default().push(row);
    }

    for (repo_url, batch) in groups {
        let client = match route_for_repo_at(Some(&repo_url), Some(backend_url.clone())) {
            RepoRoute::Upload(client) => client,
            RepoRoute::NotConnected => {
                mark_batch_failed(&batch, REPO_NOT_CONNECTED, UNCONNECTED_RECHECK_SECS as u64);
                continue;
            }
            RepoRoute::Unavailable(reason) => {
                mark_batch_failed(&batch, &reason, 300);
                continue;
            }
        };

        let rows: Vec<FileChangeCount> = batch
            .iter()
            .map(|row| FileChangeCount {
                repo_url: row.repo_url.clone(),
                file_path: row.file_path.clone(),
                change_count: row.change_count,
                lines_added: row.lines_added,
                lines_deleted: row.lines_deleted,
                last_changed_at: row.last_changed_at,
            })
            .collect();

        match client.upload_file_change_counts(&rows, &distinct_id) {
            Ok(FileChangeUploadResponse { failed, rejected }) => {
                let failed_keys: std::collections::HashSet<(String, String)> = failed
                    .into_iter()
                    .map(|row| (row.repo_url, row.file_path))
                    .collect();
                let rejected_keys: std::collections::HashSet<(String, String)> = rejected
                    .into_iter()
                    .map(|row| (row.repo_url, row.file_path))
                    .collect();

                let mut by_repo: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                for row in &batch {
                    let key = (row.repo_url.clone(), row.file_path.clone());
                    if failed_keys.contains(&key) || rejected_keys.contains(&key) {
                        continue;
                    }
                    by_repo
                        .entry(row.repo_url.clone())
                        .or_default()
                        .push(row.file_path.clone());
                }

                if !by_repo.is_empty() {
                    crate::daemon::telemetry_worker::note_durable_sync_authenticated();
                    if failed_keys.is_empty() {
                        crate::auth::notice::clear_upload_failure_for("file_changes");
                    }
                }

                if let Ok(db) = FileChangesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    for (repo_url, paths) in by_repo {
                        let _ = lock.mark_synced(&repo_url, &paths);
                    }
                }

                // Refused by the server because the repo isn't connected: park.
                if !rejected_keys.is_empty()
                    && let Ok(db) = FileChangesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    for (repo_url, file_path) in rejected_keys {
                        let _ = lock.mark_failed(
                            &repo_url,
                            &[file_path],
                            REPO_NOT_CONNECTED,
                            UNCONNECTED_RECHECK_SECS as u64,
                        );
                    }
                }

                if !failed_keys.is_empty() {
                    crate::auth::notice::record_upload_failure(
                        "file_changes",
                        "org database upsert failed",
                    );
                }
                if !failed_keys.is_empty()
                    && let Ok(db) = FileChangesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    for (repo_url, file_path) in failed_keys {
                        let _ = lock.mark_failed(
                            &repo_url,
                            &[file_path],
                            "org database upsert failed",
                            300,
                        );
                    }
                }
            }
            Err(e) => {
                crate::auth::notice::record_upload_failure("file_changes", &e.to_string());
                mark_batch_failed(&batch, &e.to_string(), 300);
            }
        }
    }
}

fn mark_batch_failed(batch: &[db::PendingFileChangeRow], error: &str, retry_delay_secs: u64) {
    if let Ok(db) = FileChangesDatabase::global()
        && let Ok(mut lock) = db.lock()
    {
        for row in batch {
            let _ = lock.mark_failed(
                &row.repo_url,
                std::slice::from_ref(&row.file_path),
                error,
                retry_delay_secs,
            );
        }
    }
}
