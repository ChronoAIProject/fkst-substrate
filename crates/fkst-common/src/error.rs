//! fkst error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FkstError {
    #[error("schema validation: {0}")]
    Schema(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
