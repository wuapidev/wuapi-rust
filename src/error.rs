// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/src/error.rs. Do not edit here.

//! Errors.

use std::fmt;
use std::time::Duration;

/// Why a request failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The API answered with a non-2xx status.
    Api {
        /// The HTTP status.
        status: u16,
        /// Machine-readable code, e.g. `invalid_request`, `account_not_ready`.
        /// `http_<status>` when the body carries none.
        code: String,
        /// Human-readable explanation.
        message: String,
        /// Extra context from the error body.
        details: Option<serde_json::Map<String, serde_json::Value>>,
        /// Value of the `x-request-id` response header, when present.
        request_id: Option<String>,
        /// The `Retry-After` response header, when present.
        retry_after: Option<Duration>,
    },
    /// An attempt took longer than the timeout, and no retry was left or
    /// allowed.
    Timeout {
        /// The per-attempt timeout that ran out.
        timeout: Duration,
    },
    /// The request could not be sent or its response could not be read.
    Network(reqwest::Error),
    /// The API answered 2xx with a body that does not match the expected type.
    Decode {
        /// The HTTP status.
        status: u16,
        /// Value of the `x-request-id` response header, when present.
        request_id: Option<String>,
        /// What did not parse.
        source: serde_json::Error,
    },
    /// The request body could not be serialized. Nothing was sent.
    Encode(String),
    /// The client is misconfigured (no API key, a bad project). Nothing was sent.
    Config(String),
}

impl Error {
    /// The HTTP status, when the API answered.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } | Self::Decode { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// A machine-readable code: the API's own for [`Error::Api`], and
    /// `timeout`, `network_error`, `invalid_response`, `invalid_request` or
    /// `configuration` for the rest.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::Api { code, .. } => code,
            Self::Timeout { .. } => "timeout",
            Self::Network(_) => "network_error",
            Self::Decode { .. } => "invalid_response",
            Self::Encode(_) => "invalid_request",
            Self::Config(_) => "configuration",
        }
    }

    /// The request id, when the API sent one. Quote it to support.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Api { request_id, .. } | Self::Decode { request_id, .. } => request_id.as_deref(),
            _ => None,
        }
    }

    /// How long the API asked to wait before trying again.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Extra context from the error body (`field` names the offending field).
    #[must_use]
    pub fn details(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        match self {
            Self::Api { details, .. } => details.as_ref(),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api {
                status,
                code,
                message,
                request_id,
                ..
            } => {
                write!(f, "{message} ({status} {code}")?;
                if let Some(id) = request_id {
                    write!(f, ", request {id}")?;
                }
                f.write_str(")")
            }
            Self::Timeout { timeout } => {
                write!(f, "request timed out after {} ms", timeout.as_millis())
            }
            Self::Network(error) => write!(f, "network error: {error}"),
            Self::Decode { status, source, .. } => {
                write!(f, "the {status} response did not parse: {source}")
            }
            Self::Encode(message) => write!(f, "the request body did not serialize: {message}"),
            Self::Config(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Network(error) => Some(error),
            Self::Decode { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Why a webhook request was rejected.
#[derive(Debug)]
#[non_exhaustive]
pub enum WebhookVerificationError {
    /// The signature header is empty.
    MissingHeader,
    /// The signing secret is empty.
    MissingSecret,
    /// The signature header has no timestamp or no `v1` signature.
    MalformedHeader,
    /// The timestamp is further from now than the tolerance.
    TimestampOutOfTolerance,
    /// No signature in the header matches the body.
    SignatureMismatch,
    /// The signature is valid but the body is not the expected JSON.
    InvalidBody(serde_json::Error),
}

impl fmt::Display for WebhookVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => f.write_str("missing webhook signature header"),
            Self::MissingSecret => f.write_str("missing webhook secret"),
            Self::MalformedHeader => f.write_str("malformed webhook signature header"),
            Self::TimestampOutOfTolerance => {
                f.write_str("webhook timestamp is outside the tolerance window")
            }
            Self::SignatureMismatch => f.write_str("webhook signature does not match"),
            Self::InvalidBody(error) => write!(f, "webhook body is not valid: {error}"),
        }
    }
}

impl std::error::Error for WebhookVerificationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidBody(error) => Some(error),
            _ => None,
        }
    }
}
