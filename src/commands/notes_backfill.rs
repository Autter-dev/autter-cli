//! `autter notes backfill` — copy notes that exist only in the local notes
//! database into `refs/notes/ai` for the current repository.
//!
//! Commits made while the machine used the `http` notes backend (connected mode
//! before it moved to `both`) have their authorship note only in the notes
//! database. This command writes those notes into git so `autter blame`,
//! `autter log --plain` and other git-ref readers see them. Existing local
//! notes are never overwritten, and running it again writes nothing new.
//!
//! The same backfill runs once per repository automatically on the first note
//! write under a backend that writes git notes; this command runs it on demand.

use crate::git::find_repository;

/// Entry point for `autter notes backfill`.
pub fn handle_notes_backfill(args: &[String]) {
    // The command takes no options: the first argument is either help or an
    // unknown option.
    if let Some(arg) = args.first() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                return;
            }
            other => {
                eprintln!("error: unknown option '{}'", other);
                eprintln!("Run 'autter notes backfill --help' for usage");
                std::process::exit(crate::commands::EXIT_USAGE_ERROR);
            }
        }
    }

    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: not a git repository ({})", e);
            std::process::exit(1);
        }
    };

    match crate::git::notes_backfill::backfill_local_notes(&repo) {
        Ok(outcome) => {
            let written = outcome.written;
            // Mark the automatic run as done too, so it doesn't repeat the scan.
            let marker = crate::git::notes_backfill::auto_backfill_marker_path(&repo);
            if let Some(parent) = marker.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&marker, format!("{}\n", written));
            if written == 0 {
                println!(
                    "No notes to backfill: refs/notes/ai already has every note in the local notes database for this repository."
                );
            } else {
                println!(
                    "Backfilled {} authorship note{} from the local notes database into refs/notes/ai.",
                    written,
                    if written == 1 { "" } else { "s" }
                );
            }
            if !outcome.failed.is_empty() {
                let failed = outcome.failed.len();
                eprintln!(
                    "warning: {} of them {} the integrity check and {} written unchanged; stats and blame count {} lines as unknown. Run `autter verify <commit>` for details:",
                    failed,
                    if failed == 1 { "fails" } else { "fail" },
                    if failed == 1 { "was" } else { "were" },
                    if failed == 1 { "its" } else { "their" },
                );
                for r in &outcome.failed {
                    eprintln!("  {} {}", r.commit, r.status.as_str());
                }
            }
        }
        Err(e) => {
            eprintln!("error: notes backfill failed: {}", e);
            std::process::exit(1);
        }
    }
}

fn print_help() {
    eprintln!(
        "autter notes backfill - Write notes stored only in the local notes database into refs/notes/ai"
    );
    eprintln!();
    eprintln!("Usage: autter notes backfill");
    eprintln!();
    eprintln!("Commits made while notes_backend.kind was 'http' have their authorship");
    eprintln!("note only in the local notes database. This copies those notes into");
    eprintln!("refs/notes/ai for commits in the current repository. Existing local");
    eprintln!("notes are never overwritten; running it again writes nothing new.");
    eprintln!();
    eprintln!("Notes are copied as stored, without a new integrity stamp: a note that");
    eprintln!("fails `autter verify` still fails after the copy (it is listed), and a");
    eprintln!("note written before the integrity check stays legacy.");
}
