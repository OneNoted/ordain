use std::io;

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{message}")]
pub struct OrdainError {
    pub code: ErrorCode,
    pub message: String,
}

impl OrdainError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NoApiKey,
    KeyFileUnwritable,
    NoInstructionFiles,
    RubricInvalid,
    RubricMissing,
    SettingsInvalid,
    HostUnknown,
    HostNotFound,
    GitUnavailable,
    ClaudeUnavailable,
    CheckTimeout,
    CheckFailed,
    ContextIncomplete,
    Superseded,
    UnsupportedDelivery,
    InvalidArguments,
    Io,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoApiKey => "NO_API_KEY",
            Self::KeyFileUnwritable => "KEY_FILE_UNWRITABLE",
            Self::NoInstructionFiles => "NO_INSTRUCTION_FILES",
            Self::RubricInvalid => "RUBRIC_INVALID",
            Self::RubricMissing => "RUBRIC_MISSING",
            Self::SettingsInvalid => "SETTINGS_INVALID",
            Self::HostUnknown => "HOST_UNKNOWN",
            Self::HostNotFound => "HOST_NOT_FOUND",
            Self::GitUnavailable => "GIT_UNAVAILABLE",
            Self::ClaudeUnavailable => "CLAUDE_UNAVAILABLE",
            Self::CheckTimeout => "CHECK_TIMEOUT",
            Self::CheckFailed => "CHECK_FAILED",
            Self::ContextIncomplete => "CONTEXT_INCOMPLETE",
            Self::Superseded => "SUPERSEDED",
            Self::UnsupportedDelivery => "UNSUPPORTED_DELIVERY",
            Self::InvalidArguments => "INVALID_ARGUMENTS",
            Self::Io => "IO_ERROR",
        }
    }
}

impl From<io::Error> for OrdainError {
    fn from(value: io::Error) -> Self {
        Self::new(ErrorCode::Io, value.to_string())
    }
}

pub type Result<T> = std::result::Result<T, OrdainError>;
