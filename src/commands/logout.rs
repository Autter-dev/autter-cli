use crate::auth::CredentialStore;

/// Handle the `autter logout` command
pub fn handle_logout(args: &[String]) {
    crate::commands::arg_parser::reject_any_args("logout", args);
    let store = CredentialStore::new();

    // Check if currently logged in
    match store.load() {
        Ok(Some(_)) => {
            // Clear credentials
            if let Err(e) = store.clear() {
                eprintln!("{}", clear_failure_message(&e.to_string()));
                std::process::exit(1);
            }
            eprintln!("Successfully logged out.");
        }
        Ok(None) => {
            eprintln!("Not currently logged in.");
        }
        Err(e) => {
            eprintln!("{}", read_failure_message(&e.to_string()));
            std::process::exit(1);
        }
    }
}

/// Removing the saved login failed: usually files the user's account cannot
/// change (autter once run as root or Administrator).
fn clear_failure_message(error: &str) -> String {
    format!(
        "Failed to clear credentials: {error}\nMake sure your user owns ~/.autter (it can be left owned by root after `sudo autter ...`), then run `autter logout` again."
    )
}

/// The saved login could not be read, so there is nothing logout can clear.
fn read_failure_message(error: &str) -> String {
    format!(
        "Error checking credentials: {error}\nRun `autter login` to replace the saved login, or `autter doctor` to check your sign-in."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_failures_say_what_to_do_next() {
        let text = clear_failure_message("permission denied");
        assert!(text.contains("permission denied"), "{text}");
        assert!(text.contains("autter logout"), "{text}");
        let text = read_failure_message("bad json");
        assert!(text.contains("autter login"), "{text}");
    }
}
