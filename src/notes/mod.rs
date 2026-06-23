//! Notes backend module.
//!
//! `notes::db` provides the dedicated `~/.autter/internal/notes-db` SQLite store
//! used as both the write queue and the local read cache for authorship notes
//! that sync to the org's own database (see `api::org_db`).

pub mod db;
