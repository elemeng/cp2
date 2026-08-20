//! Security module errors

/// Security-related errors (path sanitization, etc.)
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    #[error("Absolute paths are not allowed")]
    AbsolutePathNotAllowed,

    #[error("Path traversal attempt detected")]
    TraversalAttempt,
}

pub type SecurityResult<T> = std::result::Result<T, SecurityError>;
