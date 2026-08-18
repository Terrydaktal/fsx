use std::io;

pub type Result<T> = std::result::Result<T, Error>;

/// Stable machine-readable categories for errors crossing a tool boundary.
/// The numeric values are intentionally fixed; human-facing messages remain
/// unchanged and may continue to carry the original source error.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    Io = 1000,
    PermissionDenied = 1001,
    NotFound = 1002,
    InvalidInput = 1003,
    Busy = 1004,
    Timeout = 1005,
    Incomplete = 1006,
    Unsupported = 1007,
    Internal = 1099,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "FSX_IO",
            Self::PermissionDenied => "FSX_PERMISSION_DENIED",
            Self::NotFound => "FSX_NOT_FOUND",
            Self::InvalidInput => "FSX_INVALID_INPUT",
            Self::Busy => "FSX_BUSY",
            Self::Timeout => "FSX_TIMEOUT",
            Self::Incomplete => "FSX_INCOMPLETE",
            Self::Unsupported => "FSX_UNSUPPORTED",
            Self::Internal => "FSX_INTERNAL",
        }
    }

    pub const fn numeric(self) -> u16 {
        self as u16
    }
}

/// Classify a preserved contextual error without requiring callers to throw
/// away the original message. This is deliberately conservative: unknown
/// failures remain `FSX_INTERNAL` instead of being mislabelled.
pub fn code_for_message(message: &str) -> ErrorCode {
    let lower = message.to_ascii_lowercase();
    if lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("access denied")
    {
        ErrorCode::PermissionDenied
    } else if lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("does not exist")
    {
        ErrorCode::NotFound
    } else if lower.contains("timed out") || lower.contains("timeout") {
        ErrorCode::Timeout
    } else if lower.contains("busy") || lower.contains("already running") {
        ErrorCode::Busy
    } else if lower.contains("incomplete") || lower.contains("could not read every") {
        ErrorCode::Incomplete
    } else if lower.contains("invalid") || lower.contains("requires") {
        ErrorCode::InvalidInput
    } else if lower.contains("unsupported") || lower.contains("not supported") {
        ErrorCode::Unsupported
    } else if lower.contains("io error") || lower.contains("i/o") {
        ErrorCode::Io
    } else {
        ErrorCode::Internal
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidPath(String),
    Unsupported(String),
}

impl Error {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Io(error) => match error.kind() {
                io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
                io::ErrorKind::NotFound => ErrorCode::NotFound,
                io::ErrorKind::TimedOut => ErrorCode::Timeout,
                io::ErrorKind::WouldBlock => ErrorCode::Busy,
                _ => ErrorCode::Io,
            },
            Self::InvalidPath(_) => ErrorCode::InvalidInput,
            Self::Unsupported(_) => ErrorCode::Unsupported,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::InvalidPath(path) => write!(f, "invalid path: {path}"),
            Self::Unsupported(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidPath(_) | Self::Unsupported(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorCode, code_for_message};
    use std::io;

    #[test]
    fn stable_codes_classify_boundary_messages() {
        assert_eq!(
            code_for_message("permission denied"),
            ErrorCode::PermissionDenied
        );
        assert_eq!(code_for_message("search timed out"), ErrorCode::Timeout);
        assert_eq!(
            code_for_message("watcher is not running"),
            ErrorCode::Internal
        );
        assert_eq!(ErrorCode::Busy.as_str(), "FSX_BUSY");
        assert_eq!(ErrorCode::Busy.numeric(), 1004);
    }

    #[test]
    fn io_errors_preserve_their_source_chain() {
        let source = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let error = super::Error::from(source);
        assert!(std::error::Error::source(&error).is_some());
    }
}
