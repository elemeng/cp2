//! Protocol module errors

use thiserror::Error;

/// Protocol errors
#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("Protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;
