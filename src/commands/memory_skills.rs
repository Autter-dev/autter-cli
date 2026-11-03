//! `autter memory skills install|preview`: the repository's memory as skill
//! files in the checkout.
//!
//! Autter serves a skill generated from the repository's memory facts
//! (`GET /api/cli/orgs/<org>/repo-memory/skill`): a `SKILL.md` for agents that
//! read `.agents/skills/` and a Cursor rule, each with frontmatter carrying
//! `autter_generated: true`. Writing them into the checkout lets the team
//! commit them, so every teammate's agent has the memory even without the CLI
//! or the MCP server. A file that exists without that marker belongs to
//! someone else and is only replaced with `--force`.
use super::memory::clean;
use crate::error::AutterError;
use crate::mdm::utils::write_atomic;
use serde_json::{Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) const MISSING_ROUTE: &str = "This Autter deployment does not serve repository memory skills yet (GET /repo-memory/skill). Read the memory with `autter context \"<task>\"` or `autter memory for` until it is updated.";

/// The frontmatter line that marks a file as written by Autter.
const MARKER: &str = "autter_generated: true";

/// One file of the skill, as the server sent it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SkillFile {
    pub path: String,
    pub content: String,
}

/// What happened to one file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    Created,
    Updated,
    /// Replaced a file Autter did not write (`--force`).
    Replaced,
    Unchanged,
    /// Exists without the marker and `--force` was not given.
    Kept,
    /// Not written: the reason.
    Failed(String),
}

/// Whether `content` starts with frontmatter that has `autter_generated: true`.
pub(crate) fn is_autter_generated(content: &str) -> bool {
    let mut lines = content.trim_start_matches('\u{feff}').lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            return false;
        }
        if line == MARKER {
            return true;
        }
    }
    false
}

/// `path` as a relative path inside the checkout, or None when it is absolute,
/// climbs out with `..`, or points into `.git`.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let path = path.replace('\\', "/");
    if path.trim().is_empty() || path.starts_with('/') || path.contains(':') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in Path::new(&path).components() {
        match part {
            Component::Normal(name) if name != ".git" => out.push(name),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (out.components().count() > 0).then_some(out)
}

/// The files in a skill response, checked: each needs a safe path and content.
pub(crate) fn skill_files(result: &Value) -> Result<Vec<SkillFile>, AutterError> {
    let files = result["files"].as_array().ok_or_else(|| {
        AutterError::Generic("Autter's answer had no skill files. Try again later.".into())
    })?;
    let mut out = Vec::new();
    for file in files {
        let (Some(path), Some(content)) = (file["path"].as_str(), file["content"].as_str()) else {
            return Err(AutterError::Generic(
                "Autter's answer had a skill file without a path or content.".into(),
            ));
        };
        if safe_relative(path).is_none() {
            return Err(AutterError::Generic(format!(
                "Autter's answer named a file outside the repository ({}); nothing was written.",
                clean(path)
            )));
        }
        out.push(SkillFile {
            path: path.replace('\\', "/"),
            content: content.to_owned(),
        });
    }
    if out.is_empty() {
        return Err(AutterError::Generic(
            "This repository's memory has no facts to put in a skill yet. Autter adds them as reviews, sessions and the team teach it.".into(),
        ));
    }
    Ok(out)
}

/// Whether `dir`, or the deepest part of it that exists, resolves inside `root`.
fn inside_root(dir: &Path, root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let mut existing = dir;
    while !existing.exists() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return false,
        }
    }
    existing
        .canonicalize()
        .is_ok_and(|resolved| resolved.starts_with(root))
}

fn install_one(root: &Path, file: &SkillFile, force: bool) -> Outcome {
    let Some(relative) = safe_relative(&file.path) else {
        return Outcome::Failed("not a path inside the repository".into());
    };
    if !is_autter_generated(&file.content) {
        return Outcome::Failed(format!(
            "Autter's copy has no `{MARKER}` frontmatter, so a later run could not recognise it"
        ));
    }
    let target = root.join(&relative);
    if target.is_symlink() {
        return Outcome::Failed("is a symbolic link".into());
    }
    if target.is_dir() {
        return Outcome::Failed("is a directory".into());
    }
    let existing = if target.exists() {
        match fs::read(&target) {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(error) => return Outcome::Failed(format!("could not be read: {error}")),
        }
    } else {
        None
    };
    let outcome = match &existing {
        Some(text) if *text == file.content => return Outcome::Unchanged,
        Some(text) if is_autter_generated(text) => Outcome::Updated,
        Some(_) if force => Outcome::Replaced,
        Some(_) => return Outcome::Kept,
        None => Outcome::Created,
    };
    let Some(parent) = target.parent() else {
        return Outcome::Failed("has no parent directory".into());
    };
    // A symlinked directory on the way must not lead out of the checkout:
    // check the deepest directory that exists before creating anything.
    if !inside_root(parent, root) {
        return Outcome::Failed("its directory leads outside the repository".into());
    }
    if let Err(error) = fs::create_dir_all(parent) {
        return Outcome::Failed(format!("could not create its directory: {error}"));
    }
    if !inside_root(parent, root) {
        return Outcome::Failed("its directory leads outside the repository".into());
    }
    match write_atomic(&target, file.content.as_bytes()) {
        Ok(()) => outcome,
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

/// Write each file under `root`. Autter's own files (with the marker) are
/// replaced freely; anyone else's only with `force`.
pub(crate) fn install_files(
    root: &Path,
    files: &[SkillFile],
    force: bool,
) -> Vec<(String, Outcome)> {
    files
        .iter()
        .map(|file| (file.path.clone(), install_one(root, file, force)))
        .collect()
}

fn heading(result: &Value) -> String {
    let repo = clean(result["repo"].as_str().unwrap_or("this repository"));
    match result["facts"].as_u64() {
        Some(1) => format!("Autter memory skill for {repo} (1 fact)"),
        Some(count) => format!("Autter memory skill for {repo} ({count} facts)"),
        None => format!("Autter memory skill for {repo}"),
    }
}

/// The files as they would be written, for `preview`.
pub(crate) fn render_preview(result: &Value, files: &[SkillFile]) -> String {
    let mut out = heading(result);
    if let Some(at) = result["generatedAt"].as_str() {
        out.push_str(&format!(", generated {}", clean(at)));
    }
    out.push_str(&format!(
        "\nPreview: `autter memory skills install` writes {} file{} into this checkout.\n",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    ));
    for file in files {
        out.push_str(&format!("\n==> {} <==\n", clean(&file.path)));
        let content = clean(&file.content);
        out.push_str(&content);
        if !content.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn outcome_label(outcome: &Outcome) -> (&'static str, Option<String>) {
    match outcome {
        Outcome::Created => ("wrote", None),
        Outcome::Updated => ("updated", None),
        Outcome::Replaced => (
            "replaced",
            Some("was not written by Autter; --force".into()),
        ),
        Outcome::Unchanged => ("unchanged", None),
        Outcome::Kept => (
            "kept",
            Some("exists and was not written by Autter; pass --force to replace it".into()),
        ),
        Outcome::Failed(reason) => ("not written", Some(reason.clone())),
    }
}

/// What `install` did, file by file, and how to share it.
pub(crate) fn render_install(result: &Value, outcomes: &[(String, Outcome)]) -> String {
    let mut out = format!("{}\n\n", heading(result));
    for (path, outcome) in outcomes {
        let (label, note) = outcome_label(outcome);
        out.push_str(&format!("  {label:<11} {}", clean(path)));
        if let Some(note) = note {
            out.push_str(&format!("  ({})", clean(&note)));
        }
        out.push('\n');
    }
    let written: Vec<&str> = outcomes
        .iter()
        .filter(|(_, o)| {
            matches!(
                o,
                Outcome::Created | Outcome::Updated | Outcome::Replaced | Outcome::Unchanged
            )
        })
        .map(|(path, _)| path.as_str())
        .collect();
    if !written.is_empty() {
        out.push_str(&format!(
            "\nCommit these files so your teammates' agents get the same memory:\n  git add {}\nRun `autter memory skills install` again to refresh them as the memory grows.\n",
            written.iter().map(|p| clean(p)).collect::<Vec<_>>().join(" ")
        ));
    }
    out
}

/// The result as JSON, for `--json`.
pub(crate) fn outcomes_json(result: &Value, outcomes: &[(String, Outcome)]) -> Value {
    json!({
        "repo": result["repo"],
        "facts": result["facts"],
        "generatedAt": result["generatedAt"],
        "files": outcomes.iter().map(|(path, outcome)| {
            let (label, note) = outcome_label(outcome);
            json!({ "path": path, "result": label, "note": note })
        }).collect::<Vec<_>>(),
    })
}

/// The error to exit with when a file was kept or failed, else None.
pub(crate) fn refusal(outcomes: &[(String, Outcome)]) -> Option<String> {
    let kept = outcomes
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Kept))
        .count();
    let failed = outcomes
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Failed(_)))
        .count();
    match (kept, failed) {
        (0, 0) => None,
        (kept, 0) => Some(format!(
            "{kept} file{} already exist{} and {} not written by Autter. Pass --force to replace {}.",
            if kept == 1 { "" } else { "s" },
            if kept == 1 { "s" } else { "" },
            if kept == 1 { "was" } else { "were" },
            if kept == 1 { "it" } else { "them" },
        )),
        (_, failed) => Some(format!(
            "{failed} skill file{} could not be written.",
            if failed == 1 { "" } else { "s" }
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const OURS: &str = "---\nname: autter-repo-memory\nautter_generated: true\n---\n# Memory\n- Dedupe webhooks (rmf_ab12cd34ef)\n";

    fn file(path: &str, content: &str) -> SkillFile {
        SkillFile {
            path: path.into(),
            content: content.into(),
        }
    }

    #[test]
    fn the_marker_must_be_in_the_frontmatter() {
        assert!(is_autter_generated(OURS));
        assert!(is_autter_generated(
            "\u{feff}---\r\nautter_generated: true\r\n---\r\nbody"
        ));
        assert!(!is_autter_generated("# Notes\nautter_generated: true\n"));
        assert!(!is_autter_generated(
            "---\nname: mine\n---\nautter_generated: true\n"
        ));
        assert!(!is_autter_generated("---\nautter_generated: false\n---\n"));
        assert!(!is_autter_generated(""));
    }

    #[test]
    fn server_paths_must_stay_inside_the_checkout() {
        assert_eq!(
            safe_relative(".agents/skills/autter-repo-memory/SKILL.md"),
            Some(PathBuf::from(".agents/skills/autter-repo-memory/SKILL.md"))
        );
        assert_eq!(
            safe_relative("./.cursor/rules/autter-repo-memory.mdc"),
            Some(PathBuf::from(".cursor/rules/autter-repo-memory.mdc"))
        );
        for bad in [
            "",
            "/etc/passwd",
            "../outside.md",
            "a/../../b.md",
            ".git/hooks/pre-commit",
            "C:/Windows/x.md",
            "\\\\server\\share\\x.md",
        ] {
            assert_eq!(safe_relative(bad), None, "{bad}");
        }
        let response = json!({ "files": [{ "path": "../x.md", "content": OURS }] });
        assert!(skill_files(&response).is_err());
        let response = json!({ "files": [] });
        assert!(skill_files(&response).is_err());
        let response = json!({ "files": [{ "path": "a.md", "content": OURS }] });
        assert_eq!(skill_files(&response).unwrap(), vec![file("a.md", OURS)]);
    }

    #[test]
    fn install_writes_new_files_and_replaces_only_its_own() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let skill = ".agents/skills/autter-repo-memory/SKILL.md";
        let rule = ".cursor/rules/autter-repo-memory.mdc";
        let files = vec![file(skill, OURS), file(rule, OURS)];

        // Fresh checkout: both written, directories created.
        let outcomes = install_files(root, &files, false);
        assert_eq!(
            outcomes,
            vec![
                (skill.to_string(), Outcome::Created),
                (rule.to_string(), Outcome::Created)
            ]
        );
        assert_eq!(fs::read_to_string(root.join(skill)).unwrap(), OURS);
        assert_eq!(refusal(&outcomes), None);

        // Same content again: unchanged.
        let outcomes = install_files(root, &files, false);
        assert!(outcomes.iter().all(|(_, o)| *o == Outcome::Unchanged));

        // Autter's own older file is replaced freely.
        let newer = OURS.replace("Dedupe webhooks", "Dedupe webhooks on the event id");
        let outcomes = install_files(root, &[file(skill, &newer)], false);
        assert_eq!(outcomes[0].1, Outcome::Updated);
        assert_eq!(fs::read_to_string(root.join(skill)).unwrap(), newer);

        // Someone's own file is kept without --force...
        let theirs = "---\nname: our-rules\n---\nHand written.\n";
        fs::write(root.join(rule), theirs).unwrap();
        let outcomes = install_files(root, &[file(rule, OURS)], false);
        assert_eq!(outcomes[0].1, Outcome::Kept);
        assert_eq!(fs::read_to_string(root.join(rule)).unwrap(), theirs);
        assert!(refusal(&outcomes).unwrap().contains("--force"));
        let text = render_install(&json!({ "repo": "acme/api", "facts": 12 }), &outcomes);
        assert!(text.contains("kept"), "{text}");
        assert!(!text.contains("git add"), "{text}");

        // ...and replaced with it.
        let outcomes = install_files(root, &[file(rule, OURS)], true);
        assert_eq!(outcomes[0].1, Outcome::Replaced);
        assert_eq!(fs::read_to_string(root.join(rule)).unwrap(), OURS);
    }

    #[test]
    fn install_refuses_content_without_the_marker() {
        let dir = TempDir::new().unwrap();
        let outcomes = install_files(dir.path(), &[file("a.md", "# no frontmatter\n")], true);
        assert!(matches!(outcomes[0].1, Outcome::Failed(_)));
        assert!(!dir.path().join("a.md").exists());
        assert!(refusal(&outcomes).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn install_does_not_follow_a_symlink_out_of_the_checkout() {
        let outside = TempDir::new().unwrap();
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".cursor")).unwrap();
        let outcomes = install_files(
            dir.path(),
            &[file(".cursor/rules/autter-repo-memory.mdc", OURS)],
            true,
        );
        assert!(matches!(outcomes[0].1, Outcome::Failed(_)), "{outcomes:?}");
        assert!(!outside.path().join("rules").exists());
    }

    #[test]
    fn install_and_preview_say_what_to_commit() {
        let result =
            json!({ "repo": "acme/api", "facts": 12, "generatedAt": "2026-11-03T09:00:00Z" });
        let outcomes = vec![
            ("a/SKILL.md".to_string(), Outcome::Created),
            ("b.mdc".to_string(), Outcome::Unchanged),
        ];
        assert_eq!(
            render_install(&result, &outcomes),
            "Autter memory skill for acme/api (12 facts)\n\n  wrote       a/SKILL.md\n  unchanged   b.mdc\n\
             \nCommit these files so your teammates' agents get the same memory:\n  git add a/SKILL.md b.mdc\
             \nRun `autter memory skills install` again to refresh them as the memory grows.\n"
        );
        let preview = render_preview(&result, &[file("a/SKILL.md", OURS)]);
        assert!(preview.starts_with(
            "Autter memory skill for acme/api (12 facts), generated 2026-11-03T09:00:00Z\n"
        ));
        assert!(preview.contains("\n==> a/SKILL.md <==\n---\nname: autter-repo-memory"));
    }
}
