//! Replacing the installed autter binary while a process may be running it.
//!
//! Windows does not replace or delete an `.exe` that a running process has
//! open: the background service runs `~/.autter/bin/autter.exe`, and so does
//! `autter upgrade` itself. It does let such a file be renamed. So before the
//! installer runs, the background service is stopped (as `autter bg shutdown`
//! does) and the installed binary is renamed aside to `autter.exe.old`; the
//! installer then writes a fresh file. If the installer leaves no binary
//! behind, the old one is renamed back, so a failed upgrade never leaves a
//! half-replaced install. The aside file is deleted as soon as nothing runs
//! it: by the next upgrade, or when the background service next starts. The
//! service is started again, on whatever binary is in place, if it was
//! running before.
//!
//! The file system and service calls go through [`SwapHost`], so the logic is
//! tested on every platform; only Windows uses it.

#![cfg_attr(not(windows), allow(dead_code))]

use std::io;
use std::path::{Path, PathBuf};

/// Suffix of a binary renamed aside: `autter.exe.old`, or
/// `autter.exe.<pid>.old` when an older one is still running.
pub(crate) const ASIDE_SUFFIX: &str = ".old";

/// The binaries the installers put in `~/.autter/bin`.
pub(crate) const INSTALLED_BINARY_NAMES: &[&str] = &["autter.exe", "git.exe"];

/// The file system and background-service calls a swap makes.
pub(crate) trait SwapHost {
    fn exists(&self, path: &Path) -> bool;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    fn list_dir(&self, dir: &Path) -> Vec<PathBuf>;
    /// Whether the background service is running or starting.
    fn service_running(&self) -> bool;
    /// Stop the background service and wait for it to exit.
    fn stop_service(&self) -> Result<(), String>;
    /// Start the background service from `exe`.
    fn start_service(&self, exe: &Path) -> Result<(), String>;
}

/// Leftovers of earlier swaps in `dir`: `<name>.old` and `<name>.<n>.old`
/// for each of `names`.
pub(crate) fn leftover_binaries(host: &impl SwapHost, dir: &Path, names: &[&str]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = host
        .list_dir(dir)
        .into_iter()
        .filter(|path| {
            let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
                return false;
            };
            names.iter().any(|name| {
                file.len() > name.len() + 1
                    && file
                        .get(..name.len() + 1)
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&format!("{name}.")))
                    && file.to_ascii_lowercase().ends_with(ASIDE_SUFFIX)
            })
        })
        .collect();
    found.sort();
    found
}

/// Delete the leftovers no process runs any more. One still running stays
/// until a later call.
pub(crate) fn remove_leftover_binaries(host: &impl SwapHost, dir: &Path, names: &[&str]) {
    for path in leftover_binaries(host, dir, names) {
        let _ = host.remove(&path);
    }
}

fn aside_path(host: &impl SwapHost, exe: &Path, pid: u32) -> PathBuf {
    let plain = PathBuf::from(format!("{}{ASIDE_SUFFIX}", exe.display()));
    if !host.exists(&plain) {
        return plain;
    }
    PathBuf::from(format!("{}.{pid}{ASIDE_SUFFIX}", exe.display()))
}

/// Whether `err` is Windows refusing a file another process has open.
pub(crate) fn is_in_use(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION.
    cfg!(windows) && matches!(err.raw_os_error(), Some(32 | 33))
}

/// What `autter upgrade` says when the installed binary cannot be moved.
pub(crate) fn swap_failure_message(exe: &Path, err: &io::Error) -> String {
    if is_in_use(err) {
        format!(
            "Could not replace {}: the file is in use ({err}).\n\
             The autter background service or another program still has it open, and Windows does not replace a running program.\n\
             The current autter is still installed and works.\n\
             Fix: run `autter bg shutdown`, then `autter upgrade` again.",
            exe.display()
        )
    } else {
        format!(
            "Could not move {} aside to replace it ({err}).\nThe current autter is still installed and works.",
            exe.display()
        )
    }
}

/// Run `run_installer`, which writes a new binary at `exe`, with the
/// background service stopped and the binary there now renamed aside.
///
/// Returns the installer's error, or an error when `exe` could not be moved;
/// in both cases the binary that was at `exe` is there again. The service is
/// started again, on whatever binary is at `exe`, if it was running.
pub(crate) fn run_installer_with_binary_aside(
    host: &impl SwapHost,
    exe: &Path,
    pid: u32,
    run_installer: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if let Some(dir) = exe.parent() {
        remove_leftover_binaries(host, dir, INSTALLED_BINARY_NAMES);
    }

    let service_was_running = host.service_running();
    if service_was_running && let Err(err) = host.stop_service() {
        // Not fatal: renaming aside works on a running file.
        eprintln!("Could not stop the autter background service ({err}); continuing.");
    }
    let restart = || {
        if service_was_running
            && host.exists(exe)
            && let Err(err) = host.start_service(exe)
        {
            eprintln!(
                "Could not start the autter background service again ({err}). Run `autter bg restart`."
            );
        }
    };

    let aside = if host.exists(exe) {
        let aside = aside_path(host, exe, pid);
        if let Err(err) = host.rename(exe, &aside) {
            restart();
            return Err(swap_failure_message(exe, &err));
        }
        Some(aside)
    } else {
        None
    };

    let mut result = run_installer();

    if let Some(aside) = aside {
        if host.exists(exe) {
            // Fails while this process (or the old service) still runs it.
            let _ = host.remove(&aside);
        } else {
            match host.rename(&aside, exe) {
                Ok(()) => {
                    let reason = result.err().unwrap_or_else(|| {
                        "The installer left no autter binary behind.".to_string()
                    });
                    result = Err(format!(
                        "{reason}\nThe previous autter is still installed ({}).",
                        exe.display()
                    ));
                }
                Err(err) => {
                    let reason = result.err().unwrap_or_default();
                    return Err(format!(
                        "{reason}\nThe previous autter could not be put back ({err}): rename {} to {}.",
                        aside.display(),
                        exe.display()
                    ));
                }
            }
        }
    }

    restart();
    result
}

/// The real file system and background service.
#[cfg(windows)]
pub(crate) struct SystemHost {
    daemon: Option<crate::daemon::DaemonConfig>,
}

#[cfg(windows)]
impl SystemHost {
    pub(crate) fn new() -> Self {
        Self {
            daemon: crate::daemon::DaemonConfig::from_env_or_default_paths().ok(),
        }
    }
}

#[cfg(windows)]
impl SwapHost for SystemHost {
    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
    }

    fn service_running(&self) -> bool {
        self.daemon
            .as_ref()
            .is_some_and(crate::commands::daemon::daemon_is_running_or_starting)
    }

    fn stop_service(&self) -> Result<(), String> {
        match &self.daemon {
            Some(config) => {
                crate::commands::daemon::stop_daemon(config, std::time::Duration::from_secs(10))
            }
            None => Ok(()),
        }
    }

    fn start_service(&self, exe: &Path) -> Result<(), String> {
        let status = std::process::Command::new(exe)
            .args(["bg", "start"])
            // Debug builds read AUTTER=git as "run as git".
            .env_remove("AUTTER")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("starting it exited with {status}"))
        }
    }
}

/// Delete the aside files of earlier upgrades in `~/.autter/bin` that nothing
/// runs any more. Called when the background service starts.
#[cfg(windows)]
pub(crate) fn remove_leftover_installed_binaries() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let host = SystemHost { daemon: None };
    remove_leftover_binaries(
        &host,
        &home.join(".autter").join("bin"),
        INSTALLED_BINARY_NAMES,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    /// A stand-in for the Windows file system: a file in `running` is the
    /// image of a live process (it can be renamed, not replaced or deleted);
    /// a file in `locked` is open without delete sharing (cannot be renamed).
    #[derive(Default)]
    struct FakeHost {
        files: RefCell<BTreeMap<PathBuf, &'static str>>,
        running: RefCell<BTreeSet<PathBuf>>,
        locked: BTreeSet<PathBuf>,
        service_up: bool,
        events: RefCell<Vec<String>>,
    }

    fn name(path: &Path) -> String {
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    impl FakeHost {
        fn with(files: &[(&Path, &'static str)]) -> Self {
            let host = FakeHost::default();
            for (path, content) in files {
                host.files.borrow_mut().insert(path.to_path_buf(), content);
            }
            host
        }

        fn log(&self, event: String) {
            self.events.borrow_mut().push(event);
        }

        fn events(&self) -> Vec<String> {
            self.events.borrow().clone()
        }

        fn listing(&self) -> Vec<(String, &'static str)> {
            self.files
                .borrow()
                .iter()
                .map(|(p, c)| (name(p), *c))
                .collect()
        }
    }

    fn denied(what: &str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, format!("{what} is in use"))
    }

    impl SwapHost for FakeHost {
        fn exists(&self, path: &Path) -> bool {
            self.files.borrow().contains_key(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.log(format!("rename {} -> {}", name(from), name(to)));
            if self.locked.contains(from) {
                return Err(denied(&name(from)));
            }
            if self.exists(to) && self.running.borrow().contains(to) {
                return Err(denied(&name(to)));
            }
            let content = self
                .files
                .borrow_mut()
                .remove(from)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            self.files.borrow_mut().insert(to.to_path_buf(), content);
            if self.running.borrow_mut().remove(from) {
                self.running.borrow_mut().insert(to.to_path_buf());
            }
            Ok(())
        }

        fn remove(&self, path: &Path) -> io::Result<()> {
            self.log(format!("remove {}", name(path)));
            if self.running.borrow().contains(path) || self.locked.contains(path) {
                return Err(denied(&name(path)));
            }
            self.files.borrow_mut().remove(path);
            Ok(())
        }

        fn list_dir(&self, dir: &Path) -> Vec<PathBuf> {
            self.files
                .borrow()
                .keys()
                .filter(|p| p.parent() == Some(dir))
                .cloned()
                .collect()
        }

        fn service_running(&self) -> bool {
            self.service_up
        }

        fn stop_service(&self) -> Result<(), String> {
            self.log("stop service".to_string());
            Ok(())
        }

        fn start_service(&self, exe: &Path) -> Result<(), String> {
            let content = self.files.borrow().get(exe).copied().unwrap_or("none");
            self.log(format!("start service on {} ({content})", name(exe)));
            Ok(())
        }
    }

    fn dir() -> PathBuf {
        PathBuf::from("home").join(".autter").join("bin")
    }

    fn exe() -> PathBuf {
        dir().join("autter.exe")
    }

    fn aside() -> PathBuf {
        dir().join("autter.exe.old")
    }

    /// What install.ps1 does once the old binary is out of the way.
    fn installer_writes_new<'a>(host: &'a FakeHost) -> impl FnOnce() -> Result<(), String> + 'a {
        move || {
            host.log("installer".to_string());
            if host.exists(&exe()) {
                return Err("the installer found autter.exe still in place".to_string());
            }
            host.files.borrow_mut().insert(exe(), "new");
            Ok(())
        }
    }

    #[test]
    fn running_binary_is_renamed_aside_and_the_service_restarts_on_the_new_one() {
        let mut host = FakeHost::with(&[(&exe(), "old")]);
        host.service_up = true;
        // `autter upgrade` itself runs the installed binary.
        host.running.borrow_mut().insert(exe());

        let result = run_installer_with_binary_aside(&host, &exe(), 7, installer_writes_new(&host));
        assert_eq!(result, Ok(()));
        assert_eq!(
            host.events(),
            vec![
                "stop service",
                "rename autter.exe -> autter.exe.old",
                "installer",
                // Still running this process: deleted by a later run.
                "remove autter.exe.old",
                "start service on autter.exe (new)",
            ]
        );
        assert_eq!(
            host.listing(),
            vec![
                ("autter.exe".to_string(), "new"),
                ("autter.exe.old".to_string(), "old")
            ]
        );

        // The next upgrade (or service start), with the old process gone,
        // deletes the leftover.
        host.running.borrow_mut().clear();
        remove_leftover_binaries(&host, &dir(), INSTALLED_BINARY_NAMES);
        assert_eq!(host.listing(), vec![("autter.exe".to_string(), "new")]);
    }

    #[test]
    fn service_that_was_not_running_is_not_started() {
        let host = FakeHost::with(&[(&exe(), "old")]);
        let result = run_installer_with_binary_aside(&host, &exe(), 7, installer_writes_new(&host));
        assert_eq!(result, Ok(()));
        let events = host.events();
        assert!(!events.iter().any(|e| e.contains("service")), "{events:?}");
        // Nothing ran the aside file, so it is gone already.
        assert_eq!(host.listing(), vec![("autter.exe".to_string(), "new")]);
    }

    #[test]
    fn file_in_use_fails_loudly_and_leaves_the_old_binary() {
        let mut host = FakeHost::with(&[(&exe(), "old")]);
        host.service_up = true;
        host.locked.insert(exe());

        let mut installer_ran = false;
        let result = run_installer_with_binary_aside(&host, &exe(), 7, || {
            installer_ran = true;
            Ok(())
        });
        let message = result.unwrap_err();
        assert!(!installer_ran);
        assert!(message.contains("the file is in use"), "{message}");
        assert!(message.contains("still installed and works"), "{message}");
        assert!(
            message.contains("Fix: run `autter bg shutdown`, then `autter upgrade` again."),
            "{message}"
        );
        assert_eq!(host.listing(), vec![("autter.exe".to_string(), "old")]);
        // The service that was stopped runs again, on the old binary.
        assert_eq!(
            host.events(),
            vec![
                "stop service",
                "rename autter.exe -> autter.exe.old",
                "start service on autter.exe (old)",
            ]
        );
    }

    #[test]
    fn failed_installer_puts_the_old_binary_back() {
        let mut host = FakeHost::with(&[(&exe(), "old")]);
        host.service_up = true;
        host.running.borrow_mut().insert(exe());

        let result = run_installer_with_binary_aside(&host, &exe(), 7, || {
            Err("Installation script failed with exit code: Some(1)".to_string())
        });
        let message = result.unwrap_err();
        assert!(
            message.starts_with("Installation script failed"),
            "{message}"
        );
        assert!(
            message.contains("previous autter is still installed"),
            "{message}"
        );
        assert_eq!(host.listing(), vec![("autter.exe".to_string(), "old")]);
        assert_eq!(
            host.events().last().map(String::as_str),
            Some("start service on autter.exe (old)")
        );
    }

    #[test]
    fn installer_that_succeeds_without_a_binary_is_a_failure() {
        let host = FakeHost::with(&[(&exe(), "old")]);
        let result = run_installer_with_binary_aside(&host, &exe(), 7, || Ok(()));
        let message = result.unwrap_err();
        assert!(message.contains("left no autter binary"), "{message}");
        assert_eq!(host.listing(), vec![("autter.exe".to_string(), "old")]);
    }

    #[test]
    fn installer_failing_after_the_new_binary_is_in_place_keeps_the_new_one() {
        // e.g. git.exe could not be refreshed: autter.exe is already new.
        let host = FakeHost::with(&[(&exe(), "old")]);
        let result = run_installer_with_binary_aside(&host, &exe(), 7, || {
            host.files.borrow_mut().insert(exe(), "new");
            Err("Installation script failed with exit code: Some(1)".to_string())
        });
        assert_eq!(
            result,
            Err("Installation script failed with exit code: Some(1)".to_string())
        );
        assert_eq!(host.listing(), vec![("autter.exe".to_string(), "new")]);
    }

    #[test]
    fn an_old_aside_file_still_running_gets_a_numbered_neighbour() {
        let host = FakeHost::with(&[(&exe(), "old"), (&aside(), "older")]);
        host.running.borrow_mut().insert(aside());
        let result = run_installer_with_binary_aside(&host, &exe(), 7, installer_writes_new(&host));
        assert_eq!(result, Ok(()));
        assert_eq!(
            host.listing(),
            vec![
                ("autter.exe".to_string(), "new"),
                ("autter.exe.old".to_string(), "older"),
            ]
        );
        assert!(
            host.events()
                .contains(&"rename autter.exe -> autter.exe.7.old".to_string()),
            "{:?}",
            host.events()
        );
    }

    #[test]
    fn no_installed_binary_runs_the_installer_as_is() {
        let host = FakeHost::default();
        let result = run_installer_with_binary_aside(&host, &exe(), 7, installer_writes_new(&host));
        assert_eq!(result, Ok(()));
        assert_eq!(host.events(), vec!["installer"]);
    }

    #[test]
    fn leftovers_are_recognised_by_name() {
        let host = FakeHost::with(&[
            (&exe(), "a"),
            (&dir().join("autter.exe.old"), "b"),
            (&dir().join("autter.exe.123.old"), "c"),
            (&dir().join("GIT.EXE.old"), "d"),
            (&dir().join("git.exe"), "e"),
            (&dir().join("autter.exe.bak"), "f"),
            (&dir().join("other.exe.old"), "g"),
        ]);
        let names: Vec<String> = leftover_binaries(&host, &dir(), INSTALLED_BINARY_NAMES)
            .iter()
            .map(|p| name(p))
            .collect();
        assert_eq!(
            names,
            vec!["GIT.EXE.old", "autter.exe.123.old", "autter.exe.old"]
        );
    }

    #[test]
    fn in_use_errors_name_the_cause_and_the_fix() {
        let message = swap_failure_message(&exe(), &denied("autter.exe"));
        assert!(message.contains("in use"), "{message}");
        assert!(message.contains("autter bg shutdown"), "{message}");
        let other = swap_failure_message(&exe(), &io::Error::from(io::ErrorKind::NotFound));
        assert!(!other.contains("in use"), "{other}");
        assert!(other.contains("still installed"), "{other}");
    }

    /// The real Windows file system: a running .exe can be renamed aside and
    /// replaced, and one held open without delete sharing fails with the
    /// in-use message and stays in place.
    #[cfg(windows)]
    #[test]
    fn system_host_swaps_a_running_exe_and_reports_a_held_one() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("autter.exe");
        let windir = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let ping = PathBuf::from(windir).join("System32").join("PING.EXE");
        std::fs::copy(&ping, &exe).unwrap();
        let mut child = std::process::Command::new(&exe)
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let host = SystemHost { daemon: None };

        // Renaming the running file aside works, and the new one goes in.
        let new = dir.path().join("new.bin");
        std::fs::write(&new, b"new").unwrap();
        let result = run_installer_with_binary_aside(&host, &exe, 7, || {
            std::fs::rename(&new, &exe).map_err(|e| e.to_string())
        });
        assert_eq!(result, Ok(()));
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let aside = dir.path().join("autter.exe.old");
        assert!(aside.exists(), "still running, so kept for later");

        let _ = child.kill();
        let _ = child.wait();
        remove_leftover_binaries(&host, dir.path(), INSTALLED_BINARY_NAMES);
        assert!(!aside.exists());

        // Held open without FILE_SHARE_DELETE: cannot even be renamed.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ
            .open(&exe)
            .unwrap();
        let result = run_installer_with_binary_aside(&host, &exe, 7, || {
            panic!("the installer must not run when the binary cannot be moved")
        });
        drop(held);
        let message = result.unwrap_err();
        assert!(message.contains("the file is in use"), "{message}");
        assert!(message.contains("autter bg shutdown"), "{message}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
    }
}
