//! Track which repository files are changed most often.
//!
//! Counts are aggregated locally in SQLite and synced to the org database when
//! the user is logged in to Autter Cloud.

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
    let _ = lock.record_change(
        &repo_key,
        &normalized_path,
        lines_added,
        lines_deleted,
        changed_at,
    );
}

/// Top changed files for a repository key.
pub fn top_changed_files(repo_key: &str, limit: usize) -> Result<Vec<FileChangeRow>, AutterError> {
    let db = FileChangesDatabase::global()?;
    let lock = db.lock().map_err(|_| {
        AutterError::Generic("file-changes database lock poisoned".to_string())
    })?;
    lock.top_files(repo_key, limit)
}

/// Upload pending rows to the org database when authenticated.
pub fn flush_pending_to_cloud() {
    use crate::api::client::{ApiClient, access_token_for_org, resolve_org_for_repo_cached};
    use crate::api::org_db;
    use crate::config;

    let cfg = config::Config::fresh();
    let backend_url = match cfg.notes_backend_url() {
        Some(url) => url.to_string(),
        None => return,
    };

    let default_client = ApiClient::new(crate::api::client::ApiContext::new(Some(
        backend_url.clone(),
    )));
    if !default_client.is_logged_in() && !default_client.has_api_key() {
        return;
    }

    let pending = match FileChangesDatabase::global() {
        Ok(db) => match db.lock() {
            Ok(mut lock) => match lock.dequeue_pending(100) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(%e, "file-changes: failed to dequeue pending rows");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!("file-changes: DB lock poisoned: {}", e);
                return;
            }
        },
        Err(e) => {
            tracing::warn!(%e, "file-changes: failed to get DB");
            return;
        }
    };

    if pending.is_empty() {
        return;
    }

    let distinct_id = config::get_or_create_distinct_id();

    let mut groups: std::collections::HashMap<Option<String>, Vec<db::PendingFileChangeRow>> =
        std::collections::HashMap::new();

    for row in pending {
        let org = resolve_org_for_repo_cached(&row.repo_url);
        groups.entry(org).or_default().push(row);
    }

    for (org_opt, batch) in groups {
        let client = match &org_opt {
            Some(org) => match access_token_for_org(org) {
                Some(token) => ApiClient::new(crate::api::client::ApiContext::with_auth(
                    Some(backend_url.clone()),
                    token,
                )),
                None => {
                    mark_batch_failed(&batch, "could not mint org-scoped token", 300);
                    continue;
                }
            },
            None => ApiClient::new(crate::api::client::ApiContext::new(Some(
                backend_url.clone(),
            ))),
        };

        let identity = match client.org_identity() {
            Ok(id) => id,
            Err(e) => {
                mark_batch_failed(&batch, &e.to_string(), 300);
                continue;
            }
        };

        let rows: Vec<org_db::FileChangeCountRow> = batch
            .iter()
            .map(|row| org_db::FileChangeCountRow {
                repo_url: row.repo_url.clone(),
                file_path: row.file_path.clone(),
                change_count: row.change_count,
                lines_added: row.lines_added,
                lines_deleted: row.lines_deleted,
                last_changed_at: row.last_changed_at,
            })
            .collect();

        match org_db::upsert_file_change_counts(&identity, &rows, &distinct_id) {
            Ok(failed) => {
                let failed_keys: std::collections::HashSet<(String, String)> = failed
                    .into_iter()
                    .map(|(repo_url, file_path)| (repo_url, file_path))
                    .collect();

                let mut by_repo: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                for row in &batch {
                    if failed_keys.contains(&(row.repo_url.clone(), row.file_path.clone())) {
                        continue;
                    }
                    by_repo
                        .entry(row.repo_url.clone())
                        .or_default()
                        .push(row.file_path.clone());
                }

                if let Ok(db) = FileChangesDatabase::global()
                    && let Ok(mut lock) = db.lock()
                {
                    for (repo_url, paths) in by_repo {
                        let _ = lock.mark_synced(&repo_url, &paths);
                    }
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
                &[row.file_path.clone()],
                error,
                retry_delay_secs,
            );
        }
    }
}
