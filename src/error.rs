//! Application errors and stable process-exit classifications.

use crate::config::ConfigError;
use crate::init::InitError;
use crate::send::SendError;
use crate::sync::SyncError;
use thiserror::Error;

/// Sendmail-compatible process exit codes used by Nochange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// Successful completion.
    Success = 0,
    /// Invalid command-line usage.
    Usage = 64,
    /// Invalid message or other input data.
    DataError = 65,
    /// A required service is unavailable.
    Unavailable = 69,
    /// An internal software error occurred.
    Software = 70,
    /// A temporary failure allows a later retry.
    TemporaryFailure = 75,
    /// Configuration is invalid or unavailable.
    Configuration = 78,
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(value: ExitCode) -> Self {
        Self::from(value as u8)
    }
}

/// Top-level failures classified for command-line callers.
#[derive(Debug, Error)]
pub enum AppError {
    /// The configuration could not be loaded or validated.
    #[error(transparent)]
    Configuration(#[from] ConfigError),

    /// Account initialization or Microsoft 365 authentication failed.
    #[error(transparent)]
    Initialization(#[from] InitError),

    /// Mail synchronization failed.
    #[error(transparent)]
    Synchronization(#[from] SyncError),

    /// Outbound message validation or submission failed.
    #[error(transparent)]
    Sending(#[from] SendError),

    /// One or more accounts failed while later accounts continued.
    #[error("{0}")]
    Temporary(String),

    /// A command received invalid input.
    #[error("{0}")]
    Usage(String),

    /// An unexpected internal failure occurred.
    #[error("{0}")]
    Software(String),
}

impl AppError {
    /// Return the stable process classification for this error.
    pub fn get_exit_code(&self) -> ExitCode {
        match self {
            Self::Configuration(_) => ExitCode::Configuration,
            Self::Initialization(_) => ExitCode::Unavailable,
            Self::Synchronization(_) | Self::Temporary(_) => ExitCode::TemporaryFailure,
            Self::Sending(error) => error.get_exit_code(),
            Self::Usage(_) => ExitCode::Usage,
            Self::Software(_) => ExitCode::Software,
        }
    }
}
