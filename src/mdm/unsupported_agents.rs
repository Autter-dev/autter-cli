//! Detection of AI coding agents and AI IDEs that autter cannot capture yet.
//!
//! Supported agents are detected by their installers in [`crate::mdm::agents`]
//! (editor CLI on PATH, app bundle, home dot-directory). Agents without a
//! preset or installer are invisible to `autter doctor`, so their edits
//! silently show up as untracked and `autter blame` credits the human who
//! committed them. This module looks for those agents with the same kind of
//! cheap, read-only signals so doctor and stats can warn about it.
//!
//! Detection never runs the tool, lists processes, or touches the network:
//! only PATH lookups and `exists()`/`read_dir()` on well-known locations.

use std::path::{Path, PathBuf};

/// An installed AI agent/IDE whose edits autter cannot capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedAgent {
    pub id: &'static str,
    pub name: &'static str,
    /// What was found, e.g. "app: /Applications/Antigravity.app".
    pub evidence: String,
}

/// Where to look for one agent.
struct AgentSignature {
    id: &'static str,
    name: &'static str,
    /// Executables on PATH.
    cli_names: &'static [&'static str],
    /// macOS app bundle names under /Applications and ~/Applications.
    mac_apps: &'static [&'static str],
    /// Install folders under %LOCALAPPDATA%\Programs on Windows.
    windows_programs: &'static [&'static str],
    /// VS Code-style product names: `<config dir>/<product>/User` exists.
    vscode_products: &'static [&'static str],
    /// Paths relative to the home directory.
    home_paths: &'static [&'static str],
    /// VS Code extension ID prefixes (lowercase) looked up in the extension
    /// folders of VS Code and its forks.
    extension_prefixes: &'static [&'static str],
}

/// Agents with no autter preset. Kept to tools that write code on their own;
/// JetBrains Junie is not listed because the autter JetBrains plugin captures
/// it.
const SIGNATURES: &[AgentSignature] = &[
    AgentSignature {
        id: "antigravity",
        name: "Google Antigravity",
        cli_names: &["antigravity"],
        mac_apps: &["Antigravity.app"],
        windows_programs: &["Antigravity"],
        vscode_products: &["Antigravity"],
        home_paths: &[".antigravity", ".gemini/antigravity"],
        extension_prefixes: &[],
    },
    AgentSignature {
        id: "kiro",
        name: "Kiro",
        cli_names: &["kiro"],
        mac_apps: &["Kiro.app"],
        windows_programs: &["Kiro"],
        vscode_products: &["Kiro"],
        home_paths: &[".kiro"],
        extension_prefixes: &[],
    },
    AgentSignature {
        id: "trae",
        name: "Trae",
        cli_names: &["trae"],
        mac_apps: &["Trae.app"],
        windows_programs: &["Trae"],
        vscode_products: &["Trae"],
        home_paths: &[".trae"],
        extension_prefixes: &[],
    },
    AgentSignature {
        id: "zed",
        name: "Zed (agent panel)",
        cli_names: &["zed"],
        mac_apps: &["Zed.app"],
        windows_programs: &["Zed"],
        vscode_products: &[],
        home_paths: &[".config/zed"],
        extension_prefixes: &[],
    },
    AgentSignature {
        id: "cline",
        name: "Cline",
        cli_names: &[],
        mac_apps: &[],
        windows_programs: &[],
        vscode_products: &[],
        home_paths: &[],
        extension_prefixes: &["saoudrizwan.claude-dev-"],
    },
    AgentSignature {
        id: "roo-code",
        name: "Roo Code",
        cli_names: &[],
        mac_apps: &[],
        windows_programs: &[],
        vscode_products: &[],
        home_paths: &[],
        extension_prefixes: &["rooveterinaryinc.roo-cline-"],
    },
    AgentSignature {
        id: "kilo-code",
        name: "Kilo Code",
        cli_names: &[],
        mac_apps: &[],
        windows_programs: &[],
        vscode_products: &[],
        home_paths: &[],
        extension_prefixes: &["kilocode.kilo-code-"],
    },
    AgentSignature {
        id: "aider",
        name: "Aider",
        cli_names: &["aider"],
        mac_apps: &[],
        windows_programs: &[],
        vscode_products: &[],
        home_paths: &[".aider.conf.yml"],
        extension_prefixes: &[],
    },
];

/// Extension folders of VS Code and the forks autter knows about, relative to
/// the home directory.
const EXTENSION_DIRS: &[&str] = &[
    ".vscode/extensions",
    ".vscode-insiders/extensions",
    ".vscode-oss/extensions",
    ".cursor/extensions",
    ".windsurf/extensions",
    ".antigravity/extensions",
    ".kiro/extensions",
    ".trae/extensions",
];

/// Filesystem view used by detection, so tests can run against a temp home.
struct DetectionEnv<'a> {
    home: &'a Path,
    on_path: &'a dyn Fn(&str) -> bool,
    /// Directories that hold `<product>/User` for VS Code-style editors.
    vscode_config_dirs: Vec<PathBuf>,
    mac_app_dirs: Vec<PathBuf>,
    windows_programs_dir: Option<PathBuf>,
}

/// Detect installed agents that autter cannot capture.
pub fn detect_unsupported_agents() -> Vec<UnsupportedAgent> {
    let home = crate::mdm::utils::home_dir();
    let on_path = |name: &str| crate::mdm::utils::binary_exists(name);

    let vscode_config_dirs = vscode_config_dirs(&home);
    let mac_app_dirs = mac_app_dirs(&home);

    #[cfg(windows)]
    let windows_programs_dir = std::env::var("LOCALAPPDATA")
        .ok()
        .map(|dir| PathBuf::from(dir).join("Programs"));
    #[cfg(not(windows))]
    let windows_programs_dir = None;

    detect_in(&DetectionEnv {
        home: &home,
        on_path: &on_path,
        vscode_config_dirs,
        mac_app_dirs,
        windows_programs_dir,
    })
}

/// Directories that hold `<product>/User` for VS Code-style editors.
fn vscode_config_dirs(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![home.join("Library").join("Application Support")]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![home.join(".config")]
    }
    #[cfg(windows)]
    {
        let _ = home;
        std::env::var("APPDATA")
            .map(|dir| vec![PathBuf::from(dir)])
            .unwrap_or_default()
    }
}

/// macOS application folders; empty elsewhere.
fn mac_app_dirs(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![PathBuf::from("/Applications"), home.join("Applications")]
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home;
        Vec::new()
    }
}

fn detect_in(env: &DetectionEnv<'_>) -> Vec<UnsupportedAgent> {
    let extension_names = installed_extension_names(env.home);
    SIGNATURES
        .iter()
        .filter_map(|sig| {
            evidence_for(sig, env, &extension_names).map(|evidence| UnsupportedAgent {
                id: sig.id,
                name: sig.name,
                evidence,
            })
        })
        .collect()
}

fn evidence_for(
    sig: &AgentSignature,
    env: &DetectionEnv<'_>,
    extension_names: &[String],
) -> Option<String> {
    for app in sig.mac_apps {
        for dir in &env.mac_app_dirs {
            let path = dir.join(app);
            if path.exists() {
                return Some(format!("app: {}", path.display()));
            }
        }
    }
    if let Some(programs) = &env.windows_programs_dir {
        for folder in sig.windows_programs {
            let path = programs.join(folder);
            if path.exists() {
                return Some(format!("app: {}", path.display()));
            }
        }
    }
    for cli in sig.cli_names {
        if (env.on_path)(cli) {
            return Some(format!("`{cli}` on PATH"));
        }
    }
    for product in sig.vscode_products {
        for dir in &env.vscode_config_dirs {
            let path = dir.join(product).join("User");
            if path.exists() {
                return Some(format!("settings: {}", path.display()));
            }
        }
    }
    for rel in sig.home_paths {
        let path = env.home.join(rel);
        if path.exists() {
            return Some(format!("config: {}", path.display()));
        }
    }
    for prefix in sig.extension_prefixes {
        if let Some(name) = extension_names.iter().find(|n| n.starts_with(prefix)) {
            return Some(format!("editor extension: {name}"));
        }
    }
    None
}

fn installed_extension_names(home: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for rel in EXTENSION_DIRS {
        let Ok(entries) = std::fs::read_dir(home.join(rel)) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_ascii_lowercase());
            }
        }
    }
    names
}

/// One-line explanation used by doctor and stats.
pub fn not_captured_message(agent: &UnsupportedAgent) -> String {
    format!(
        "{} detected — its edits are not captured; they will be attributed to you",
        agent.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn env_for<'a>(home: &'a Path, on_path: &'a dyn Fn(&str) -> bool) -> DetectionEnv<'a> {
        DetectionEnv {
            home,
            on_path,
            vscode_config_dirs: vec![home.join("config")],
            mac_app_dirs: vec![home.join("Applications")],
            windows_programs_dir: Some(home.join("Programs")),
        }
    }

    fn ids(found: &[UnsupportedAgent]) -> Vec<&'static str> {
        found.iter().map(|a| a.id).collect()
    }

    #[test]
    fn clean_home_detects_nothing() {
        let home = tempfile::tempdir().unwrap();
        let none = |_: &str| false;
        assert!(detect_in(&env_for(home.path(), &none)).is_empty());
    }

    #[test]
    fn antigravity_is_detected_from_each_signal() {
        let none = |_: &str| false;

        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("Applications/Antigravity.app")).unwrap();
        let found = detect_in(&env_for(home.path(), &none));
        assert_eq!(ids(&found), vec!["antigravity"]);
        assert!(found[0].evidence.starts_with("app: "), "{:?}", found[0]);

        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join("config/Antigravity/User")).unwrap();
        assert_eq!(
            ids(&detect_in(&env_for(home.path(), &none))),
            vec!["antigravity"]
        );

        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".gemini/antigravity")).unwrap();
        assert_eq!(
            ids(&detect_in(&env_for(home.path(), &none))),
            vec!["antigravity"]
        );

        let home = tempfile::tempdir().unwrap();
        let antigravity_cli = |name: &str| name == "antigravity";
        assert_eq!(
            ids(&detect_in(&env_for(home.path(), &antigravity_cli))),
            vec!["antigravity"]
        );
    }

    #[test]
    fn gemini_cli_alone_is_not_antigravity() {
        // ~/.gemini belongs to the (supported) Gemini CLI.
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".gemini")).unwrap();
        let none = |_: &str| false;
        assert!(detect_in(&env_for(home.path(), &none)).is_empty());
    }

    #[test]
    fn extension_based_agents_are_found_in_any_editor() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(
            home.path()
                .join(".vscode/extensions/saoudrizwan.claude-dev-3.20.0"),
        )
        .unwrap();
        fs::create_dir_all(
            home.path()
                .join(".cursor/extensions/RooVeterinaryInc.roo-cline-3.1.0"),
        )
        .unwrap();
        let none = |_: &str| false;
        let found = detect_in(&env_for(home.path(), &none));
        assert_eq!(ids(&found), vec!["cline", "roo-code"]);
    }

    #[test]
    fn message_says_edits_are_attributed_to_the_user() {
        let agent = UnsupportedAgent {
            id: "antigravity",
            name: "Google Antigravity",
            evidence: "app: /Applications/Antigravity.app".to_string(),
        };
        assert_eq!(
            not_captured_message(&agent),
            "Google Antigravity detected — its edits are not captured; they will be attributed to you"
        );
    }
}
