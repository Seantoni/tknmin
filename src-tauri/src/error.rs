//! The error shape React receives.
//!
//! Internal error enums stay internal; every command failure is flattened into
//! a stable code plus a message the interface can show without interpreting.

use serde::{Deserialize, Serialize};

use crate::adapters::AdapterError;
use crate::repository::RepositoryError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The usage store could not be read or written.
    Storage,
    /// A source adapter failed or is not implemented yet.
    Adapter,
    /// The interface sent something the command could not accept.
    InvalidRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }
}

impl From<RepositoryError> for AppError {
    fn from(error: RepositoryError) -> Self {
        Self::new(ErrorCode::Storage, error.to_string())
    }
}

impl From<AdapterError> for AppError {
    fn from(error: AdapterError) -> Self {
        Self::new(ErrorCode::Adapter, error.to_string())
    }
}
