use std::fmt;

#[derive(Debug)]
pub enum AutterError {
    IoError(std::io::Error),
    /// Errors from invoking the git CLI that exited with a non-zero status
    GitCliError {
        code: Option<i32>,
        stderr: String,
        args: Vec<String>,
    },
    /// Errors from  Gix
    GixError(String),
    JsonError(serde_json::Error),
    Utf8Error(std::str::Utf8Error),
    FromUtf8Error(std::string::FromUtf8Error),
    PresetError(String),
    SqliteError(rusqlite::Error),
    Generic(String),
}

impl fmt::Display for AutterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AutterError::IoError(e) => write!(f, "IO error: {}", e),
            AutterError::GitCliError { code, stderr, args } => match code {
                Some(c) => write!(
                    f,
                    "Git CLI ({}) failed with exit code {}: {}",
                    args.join(" "),
                    c,
                    stderr
                ),
                None => write!(f, "Git CLI ({}) failed: {}", args.join(" "), stderr),
            },
            AutterError::JsonError(e) => write!(f, "JSON error: {}", e),
            AutterError::Utf8Error(e) => write!(f, "UTF-8 error: {}", e),
            AutterError::FromUtf8Error(e) => write!(f, "From UTF-8 error: {}", e),
            AutterError::PresetError(e) => write!(f, "{}", e),
            AutterError::SqliteError(e) => write!(f, "SQLite error: {}", e),
            AutterError::Generic(e) => write!(f, "Generic error: {}", e),
            AutterError::GixError(e) => write!(f, "Gix error: {}", e),
        }
    }
}

impl std::error::Error for AutterError {}

impl From<std::io::Error> for AutterError {
    fn from(err: std::io::Error) -> Self {
        AutterError::IoError(err)
    }
}

impl From<serde_json::Error> for AutterError {
    fn from(err: serde_json::Error) -> Self {
        AutterError::JsonError(err)
    }
}

impl From<std::str::Utf8Error> for AutterError {
    fn from(err: std::str::Utf8Error) -> Self {
        AutterError::Utf8Error(err)
    }
}

impl From<std::string::FromUtf8Error> for AutterError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        AutterError::FromUtf8Error(err)
    }
}

impl From<rusqlite::Error> for AutterError {
    fn from(err: rusqlite::Error) -> Self {
        AutterError::SqliteError(err)
    }
}

impl Clone for AutterError {
    fn clone(&self) -> Self {
        match self {
            AutterError::IoError(e) => {
                AutterError::IoError(std::io::Error::new(e.kind(), e.to_string()))
            }
            AutterError::GitCliError { code, stderr, args } => AutterError::GitCliError {
                code: *code,
                stderr: stderr.clone(),
                args: args.clone(),
            },
            AutterError::JsonError(e) => AutterError::Generic(format!("JSON error: {}", e)),
            AutterError::Utf8Error(e) => AutterError::Utf8Error(*e),
            AutterError::FromUtf8Error(e) => AutterError::FromUtf8Error(e.clone()),
            AutterError::PresetError(s) => AutterError::PresetError(s.clone()),
            AutterError::SqliteError(e) => AutterError::Generic(format!("SQLite error: {}", e)),
            AutterError::Generic(s) => AutterError::Generic(s.clone()),
            AutterError::GixError(e) => AutterError::Generic(format!("Gix error: {}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err = AutterError::from(io_err);
        let display = format!("{}", err);
        assert!(display.contains("IO error"));
        assert!(display.contains("file not found"));
    }

    #[test]
    fn test_error_display_git_cli_error_with_code() {
        let err = AutterError::GitCliError {
            code: Some(128),
            stderr: "fatal: not a git repository".to_string(),
            args: vec!["git".to_string(), "status".to_string()],
        };
        let display = format!("{}", err);
        assert!(display.contains("128"));
        assert!(display.contains("fatal: not a git repository"));
        assert!(display.contains("git status"));
    }

    #[test]
    fn test_error_display_git_cli_error_without_code() {
        let err = AutterError::GitCliError {
            code: None,
            stderr: "command terminated".to_string(),
            args: vec!["git".to_string(), "push".to_string()],
        };
        let display = format!("{}", err);
        assert!(display.contains("Git CLI"));
        assert!(display.contains("command terminated"));
        assert!(display.contains("git push"));
    }

    #[test]
    fn test_error_display_json_error() {
        let json_str = "{invalid json";
        let json_err = serde_json::from_str::<serde_json::Value>(json_str).unwrap_err();
        let err = AutterError::from(json_err);
        let display = format!("{}", err);
        assert!(display.contains("JSON error"));
    }

    #[test]
    fn test_error_display_utf8_error() {
        let invalid_utf8 = vec![0xFF, 0xFE, 0xFD];
        let utf8_err = std::str::from_utf8(&invalid_utf8).unwrap_err();
        let err = AutterError::from(utf8_err);
        let display = format!("{}", err);
        assert!(display.contains("UTF-8 error"));
    }

    #[test]
    fn test_error_display_from_utf8_error() {
        let invalid_utf8 = vec![0xFF, 0xFE, 0xFD];
        let from_utf8_err = String::from_utf8(invalid_utf8).unwrap_err();
        let err = AutterError::from(from_utf8_err);
        let display = format!("{}", err);
        assert!(display.contains("From UTF-8 error"));
    }

    #[test]
    fn test_error_display_preset_error() {
        let err = AutterError::PresetError("invalid preset configuration".to_string());
        let display = format!("{}", err);
        assert_eq!(display, "invalid preset configuration");
    }

    #[test]
    fn test_error_display_sqlite_error() {
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        let sql_err = conn.execute("INVALID SQL", []).unwrap_err();
        let err = AutterError::from(sql_err);
        let display = format!("{}", err);
        assert!(display.contains("SQLite error"));
    }

    #[test]
    fn test_error_display_generic() {
        let err = AutterError::Generic("custom error message".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Generic error"));
        assert!(display.contains("custom error message"));
    }

    #[test]
    fn test_error_display_gix_error() {
        let err = AutterError::GixError("gix operation failed".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Gix error"));
        assert!(display.contains("gix operation failed"));
    }

    #[test]
    fn test_error_clone_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err = AutterError::from(io_err);
        let cloned = err.clone();
        assert!(matches!(cloned, AutterError::IoError(_)));
        let display = format!("{}", cloned);
        assert!(display.contains("access denied"));
    }

    #[test]
    fn test_error_clone_git_cli_error() {
        let err = AutterError::GitCliError {
            code: Some(1),
            stderr: "error message".to_string(),
            args: vec!["git".to_string(), "commit".to_string()],
        };
        let cloned = err.clone();
        match cloned {
            AutterError::GitCliError { code, stderr, args } => {
                assert_eq!(code, Some(1));
                assert_eq!(stderr, "error message");
                assert_eq!(args, vec!["git".to_string(), "commit".to_string()]);
            }
            _ => panic!("Expected GitCliError"),
        }
    }

    #[test]
    fn test_error_clone_utf8_error() {
        let invalid_utf8 = vec![0xFF];
        let utf8_err = std::str::from_utf8(&invalid_utf8).unwrap_err();
        let err = AutterError::from(utf8_err);
        let cloned = err.clone();
        assert!(matches!(cloned, AutterError::Utf8Error(_)));
    }

    #[test]
    fn test_error_clone_from_utf8_error() {
        let invalid_utf8 = vec![0xFF];
        let from_utf8_err = String::from_utf8(invalid_utf8).unwrap_err();
        let err = AutterError::from(from_utf8_err);
        let cloned = err.clone();
        assert!(matches!(cloned, AutterError::FromUtf8Error(_)));
    }

    #[test]
    fn test_error_clone_preset_error() {
        let err = AutterError::PresetError("preset error".to_string());
        let cloned = err.clone();
        match cloned {
            AutterError::PresetError(msg) => assert_eq!(msg, "preset error"),
            _ => panic!("Expected PresetError"),
        }
    }

    #[test]
    fn test_error_clone_generic() {
        let err = AutterError::Generic("generic".to_string());
        let cloned = err.clone();
        match cloned {
            AutterError::Generic(msg) => assert_eq!(msg, "generic"),
            _ => panic!("Expected Generic"),
        }
    }

    #[test]
    fn test_error_clone_json_converts_to_generic() {
        let json_err = serde_json::from_str::<serde_json::Value>("{bad}").unwrap_err();
        let err = AutterError::from(json_err);
        let cloned = err.clone();
        assert!(matches!(cloned, AutterError::Generic(_)));
        let display = format!("{}", cloned);
        assert!(display.contains("JSON error"));
    }

    #[test]
    fn test_error_clone_sqlite_converts_to_generic() {
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        let sql_err = conn.execute("BAD SQL", []).unwrap_err();
        let err = AutterError::from(sql_err);
        let cloned = err.clone();
        assert!(matches!(cloned, AutterError::Generic(_)));
        let display = format!("{}", cloned);
        assert!(display.contains("SQLite error"));
    }

    #[test]
    fn test_error_clone_gix_converts_to_generic() {
        let err = AutterError::GixError("gix error".to_string());
        let cloned = err.clone();
        assert!(matches!(cloned, AutterError::Generic(_)));
        let display = format!("{}", cloned);
        assert!(display.contains("Gix error"));
    }

    #[test]
    fn test_error_is_std_error() {
        let err = AutterError::Generic("test".to_string());
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn test_error_debug_trait() {
        let err = AutterError::Generic("debug test".to_string());
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("Generic"));
        assert!(debug_str.contains("debug test"));
    }
}
