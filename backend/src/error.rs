// src/error.rs
//
// Centralized error type for the DroidKer backend.
// Every fallible operation funnels through `DroidkerError`, which converts
// cleanly into an HTTP response so handlers stay terse.

use actix_web::{http::StatusCode, HttpResponse, ResponseError};
use serde_json::json;
use thiserror::Error;

/// Top-level error type used by every backend module.
#[derive(Error, Debug)]
pub enum DroidkerError {
    #[error("container not found: {0}")]
    NotFound(String),

    #[error("container already exists: {0}")]
    AlreadyExists(String),

    #[error("invalid input: {0}")]
    BadRequest(String),

    #[error("container is in wrong state: {0}")]
    InvalidState(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("linux system call failed: {0}")]
    Syscall(String),

    #[error("apk file rejected: {0}")]
    InvalidApk(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl From<nix::Error> for DroidkerError {
    fn from(e: nix::Error) -> Self {
        DroidkerError::Syscall(format!("{:?}", e))
    }
}

impl From<anyhow::Error> for DroidkerError {
    fn from(e: anyhow::Error) -> Self {
        DroidkerError::Internal(e.to_string())
    }
}

impl From<actix_multipart::MultipartError> for DroidkerError {
    fn from(e: actix_multipart::MultipartError) -> Self {
        DroidkerError::BadRequest(format!("multipart error: {e}"))
    }
}

/// HTTP status code mapping for each error variant.
impl ResponseError for DroidkerError {
    fn status_code(&self) -> StatusCode {
        match self {
            DroidkerError::NotFound(_) => StatusCode::NOT_FOUND,
            DroidkerError::AlreadyExists(_) => StatusCode::CONFLICT,
            DroidkerError::BadRequest(_) | DroidkerError::InvalidApk(_) => {
                StatusCode::BAD_REQUEST
            }
            DroidkerError::InvalidState(_) => StatusCode::CONFLICT,
            DroidkerError::Io(_)
            | DroidkerError::Serde(_)
            | DroidkerError::Syscall(_)
            | DroidkerError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(json!({
            "error": self.to_string(),
            "kind": format!("{:?}", self).split('(').next().unwrap_or("Unknown"),
        }))
    }
}

/// Convenience alias used by every handler.
pub type Result<T> = std::result::Result<T, DroidkerError>;
