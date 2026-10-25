use crate::commands::arg_parser::paint;

/// The spinner now lives with the rest of the terminal UI.
pub use crate::ui::progress::Spinner;

/// Print a formatted diff using colors
pub fn print_diff(diff_text: &str) {
    // Print a formatted diff using colors
    for line in diff_text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            // File headers in bold
            println!("{}", paint("1", line));
        } else if line.starts_with('+') {
            // Additions in green
            println!("{}", paint("32", line));
        } else if line.starts_with('-') {
            // Deletions in red
            println!("{}", paint("31", line));
        } else if line.starts_with("@@") {
            // Hunk headers in cyan
            println!("{}", paint("36", line));
        } else {
            // Context lines normal
            println!("{}", line);
        }
    }
    println!(); // Blank line after diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_creation() {
        let spinner = Spinner::new("Testing spinner");
        // Just verify it doesn't panic
        spinner.start();
    }

    #[test]
    fn test_spinner_success_output() {
        let spinner = Spinner::new("Processing");
        // Verify success message doesn't panic
        spinner.success("Operation completed successfully");
    }

    #[test]
    fn test_spinner_pending_output() {
        let spinner = Spinner::new("Processing");
        spinner.pending("Pending action required");
    }

    #[test]
    fn test_spinner_error_output() {
        let spinner = Spinner::new("Processing");
        spinner.error("An error occurred");
    }

    #[test]
    fn test_spinner_skipped_output() {
        let spinner = Spinner::new("Processing");
        spinner.skipped("Operation skipped");
    }

    #[test]
    fn test_spinner_update_message() {
        let spinner = Spinner::new("Initial message");
        spinner.update_message("Updated message");
        spinner.success("Done");
    }

    #[test]
    fn test_print_diff_additions() {
        let diff = "+new line\n+another new line";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_deletions() {
        let diff = "-removed line\n-another removed line";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_file_headers() {
        let diff = "--- a/file.txt\n+++ b/file.txt";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_hunk_headers() {
        let diff = "@@ -1,3 +1,4 @@";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_context_lines() {
        let diff = " context line 1\n context line 2";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_complete() {
        let diff = "--- a/test.txt\n+++ b/test.txt\n@@ -1,3 +1,4 @@\n context\n-old line\n+new line\n context";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_empty() {
        let diff = "";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_multiline() {
        let diff = "--- a/file.rs\n+++ b/file.rs\n@@ -10,5 +10,6 @@\n fn main() {\n-    println!(\"old\");\n+    println!(\"new\");\n+    println!(\"extra\");\n }";
        print_diff(diff);
    }
}
