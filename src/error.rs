use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use progenitor_client::Error as GeneratedError;

use crate::types::ErrorResponse;

/// The result of a call.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What an error answered by the API is, by its HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// 400: the request is malformed.
    BadRequest,
    /// 401: no credential, or an unknown, revoked or expired one.
    Authentication,
    /// 402: the seller's plan does not include this.
    PaymentRequired,
    /// 403: the API key does not grant the scope the route requires.
    PermissionDenied,
    /// 404: no such resource for this seller.
    NotFound,
    /// 409: an `Idempotency-Key` used with another request, or still in flight.
    Conflict,
    /// 422: the request is well formed but refused.
    UnprocessableEntity,
    /// 429: the rate limit of the API key is reached.
    RateLimit,
    /// 5xx: an error of the API; retry later.
    Server,
    /// Any other status.
    Other,
}

/// An error answered by the API, read from its envelope `{"error": {code, message, params, requestId}}`.
///
/// `code` is an open string (`item_not_found`, `rate_limited`...): match the codes you handle and treat any other one
/// by its [`kind`](Self::kind).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    /// The HTTP status.
    pub status: u16,
    /// The error's code.
    pub code: String,
    /// What went wrong, for a human.
    pub message: String,
    /// The error's parameters.
    pub params: HashMap<String, String>,
    /// The identifier to give the support, when the API sent one.
    pub request_id: Option<String>,
    /// The `Retry-After` of a rate limit, when the API sent one.
    pub retry_after: Option<Duration>,
}

impl ApiError {
    /// The kind of error, from the status.
    pub fn kind(&self) -> ErrorKind {
        match self.status {
            400 => ErrorKind::BadRequest,
            401 => ErrorKind::Authentication,
            402 => ErrorKind::PaymentRequired,
            403 => ErrorKind::PermissionDenied,
            404 => ErrorKind::NotFound,
            409 => ErrorKind::Conflict,
            422 => ErrorKind::UnprocessableEntity,
            429 => ErrorKind::RateLimit,
            500..=599 => ErrorKind::Server,
            _ => ErrorKind::Other,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "controlresell: {} (HTTP {}, {})", self.message, self.status, self.code)
    }
}

impl std::error::Error for ApiError {}

/// An error of a call.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The API answered with an error status.
    Api(Box<ApiError>),
    /// The API could not be reached (network, DNS, TLS), after the retries.
    Connection(reqwest::Error),
    /// The answer could not be read.
    InvalidResponse(String),
    /// The call was refused before leaving: a missing or invalid parameter, or no API key.
    InvalidRequest(String),
}

impl Error {
    /// The API's error, when the API answered one.
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Error::Api(error) => Some(error),
            _ => None,
        }
    }

    /// The kind of the API's error, when the API answered one.
    pub fn kind(&self) -> Option<ErrorKind> {
        self.api().map(ApiError::kind)
    }

    /// Whether the API answered 404.
    pub fn is_not_found(&self) -> bool {
        self.kind() == Some(ErrorKind::NotFound)
    }

    /// Whether the API answered 429.
    pub fn is_rate_limited(&self) -> bool {
        self.kind() == Some(ErrorKind::RateLimit)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api(error) => error.fmt(f),
            Error::Connection(error) => write!(f, "controlresell: could not reach the API: {error}"),
            Error::InvalidResponse(message) => write!(f, "controlresell: invalid response: {message}"),
            Error::InvalidRequest(message) => write!(f, "controlresell: invalid request: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Api(error) => Some(error.as_ref()),
            Error::Connection(error) => Some(error),
            _ => None,
        }
    }
}

impl From<GeneratedError<ErrorResponse>> for Error {
    fn from(error: GeneratedError<ErrorResponse>) -> Self {
        match error {
            GeneratedError::ErrorResponse(response) => {
                let status = response.status().as_u16();
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .map(Duration::from_secs_f64);
                let body = response.into_inner().error;
                Error::Api(Box::new(ApiError {
                    status,
                    code: body.code,
                    message: body.message,
                    params: body.params,
                    request_id: Some(body.request_id).filter(|id| !id.is_empty()),
                    retry_after,
                }))
            }
            GeneratedError::UnexpectedResponse(response) => {
                let status = response.status().as_u16();
                Error::Api(Box::new(ApiError {
                    status,
                    code: format!("http_{status}"),
                    message: format!("HTTP {status}"),
                    params: HashMap::new(),
                    request_id: None,
                    retry_after: None,
                }))
            }
            GeneratedError::CommunicationError(e) | GeneratedError::InvalidUpgrade(e) => Error::Connection(e),
            GeneratedError::ResponseBodyError(e) => Error::Connection(e),
            GeneratedError::InvalidResponsePayload(_, e) => Error::InvalidResponse(e.to_string()),
            GeneratedError::InvalidRequest(message) | GeneratedError::Custom(message) => Error::InvalidRequest(message),
        }
    }
}
