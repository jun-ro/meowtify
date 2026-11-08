use std::fmt;
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors shared by every layer. Cheap to clone so they can travel in events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NotFound,
    /// A song file could not be opened or decoded.
    Decode(Arc<str>),
    /// The engine does not support this operation.
    Unsupported,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => write!(f, "not found"),
            Error::Decode(m) => write!(f, "decode error: {m}"),
            Error::Unsupported => write!(f, "operation not supported"),
        }
    }
}

impl std::error::Error for Error {}
