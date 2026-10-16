//! The error catalog of the specification (DOMAIN.md section 6) and the one error type of the crate.
//!
//! A failure is either an [`AppError`], which the client is allowed to see (a stable code, a readable message and
//! parameters that never carry personal data), or an unexpected one, which the client only ever sees as `INTERNAL-00`.

use std::fmt;

use serde::Serialize;
use serde_json::{Map, Value};

/// Groups errors by the HTTP status family they map to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Validation,
    NotFound,
    /// An unmatched method on a known path: 405, in the same envelope as everything else.
    MethodNotAllowed,
    Conflict,
    Unauthenticated,
    Forbidden,
    TooLarge,
    RateLimited,
}

/// Error codes. A code is never reused for another meaning.
pub mod codes {
    pub const INVALID_REQUEST: &str = "VALIDATION-01";
    pub const ORDER_NOT_EDITABLE: &str = "VALIDATION-02";
    pub const ORDER_EMPTY: &str = "VALIDATION-03";
    pub const INVALID_TRANSITION: &str = "VALIDATION-04";
    pub const INVALID_FILTER: &str = "VALIDATION-05";
    pub const UNKNOWN_SORT: &str = "VALIDATION-06";
    pub const CUSTOMER_NOT_FOUND_IN_BODY: &str = "VALIDATION-07";
    pub const ALREADY_CANCELED: &str = "VALIDATION-08";
    pub const ITEM_LIMIT: &str = "VALIDATION-09";
    pub const BODY_TOO_LARGE: &str = "VALIDATION-12";

    // Additions of this implementation: unmatched routes and methods are answered in the same envelope.
    pub const METHOD_NOT_ALLOWED: &str = "VALIDATION-13";
    pub const ROUTE_NOT_FOUND: &str = "RESOURCE-NOT-FOUND-00";

    pub const CUSTOMER_NOT_FOUND: &str = "RESOURCE-NOT-FOUND-01";
    pub const ORDER_NOT_FOUND: &str = "RESOURCE-NOT-FOUND-02";
    pub const ITEM_NOT_FOUND: &str = "RESOURCE-NOT-FOUND-03";

    pub const CONCURRENT_MODIFICATION: &str = "CONFLICT-00";
    pub const DUPLICATE_TAX_ID: &str = "CONFLICT-01";
    pub const DUPLICATE_PASSPORT: &str = "CONFLICT-02";
    pub const CUSTOMER_HAS_ORDERS: &str = "CONFLICT-03";
    pub const DATA_INTEGRITY: &str = "CONFLICT-04";

    pub const UNAUTHENTICATED: &str = "AUTH-01";
    pub const FORBIDDEN: &str = "AUTH-02";

    pub const RATE_LIMIT: &str = "RATE-00";
    pub const INTERNAL: &str = "INTERNAL-00";

    /// Every code of the catalog, for the test that compares it with the `OpenAPI` document.
    pub const ALL: &[&str] = &[
        INVALID_REQUEST,
        ORDER_NOT_EDITABLE,
        ORDER_EMPTY,
        INVALID_TRANSITION,
        INVALID_FILTER,
        UNKNOWN_SORT,
        CUSTOMER_NOT_FOUND_IN_BODY,
        ALREADY_CANCELED,
        ITEM_LIMIT,
        BODY_TOO_LARGE,
        METHOD_NOT_ALLOWED,
        ROUTE_NOT_FOUND,
        CUSTOMER_NOT_FOUND,
        ORDER_NOT_FOUND,
        ITEM_NOT_FOUND,
        CONCURRENT_MODIFICATION,
        DUPLICATE_TAX_ID,
        DUPLICATE_PASSPORT,
        CUSTOMER_HAS_ORDERS,
        DATA_INTEGRITY,
        UNAUTHENTICATED,
        FORBIDDEN,
        RATE_LIMIT,
        INTERNAL,
    ];
}

/// A failure the client is allowed to see.
///
/// Params exist for interpolation, not for debugging: they never carry personal data (tax id, e-mail...).
#[derive(Debug, Clone, PartialEq)]
pub struct AppError {
    pub kind: ErrorKind,
    pub code: &'static str,
    pub message: String,
    pub params: Map<String, Value>,
}

impl AppError {
    fn new(kind: ErrorKind, code: &'static str, message: impl Into<String>) -> Self {
        Self { kind, code, message: message.into(), params: Map::new() }
    }

    pub fn validation(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, code, message)
    }

    /// The common VALIDATION-01 for a single offending field.
    pub fn invalid_field(field: &str, message: impl Into<String>) -> Self {
        Self::validation(codes::INVALID_REQUEST, message).with("field", field)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, code, message)
    }

    pub fn route_not_found() -> Self {
        Self::not_found(codes::ROUTE_NOT_FOUND, "Route not found")
    }

    pub fn method_not_allowed() -> Self {
        Self::new(ErrorKind::MethodNotAllowed, codes::METHOD_NOT_ALLOWED, "Method not allowed")
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, code, message)
    }

    pub fn unauthenticated() -> Self {
        Self::new(ErrorKind::Unauthenticated, codes::UNAUTHENTICATED, "Authentication required")
    }

    pub fn forbidden() -> Self {
        Self::new(ErrorKind::Forbidden, codes::FORBIDDEN, "Access denied")
    }

    pub fn too_large(max_bytes: u64) -> Self {
        Self::new(ErrorKind::TooLarge, codes::BODY_TOO_LARGE, "Request body too large").with("max_bytes", max_bytes)
    }

    pub fn rate_limited() -> Self {
        Self::new(ErrorKind::RateLimited, codes::RATE_LIMIT, "Rate limit exceeded")
    }

    /// Adds a parameter. A value that cannot be represented as JSON is dropped: a parameter is never worth a failure.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Serialize) -> Self {
        if let Ok(value) = serde_json::to_value(value) {
            self.params.insert(key.to_owned(), value);
        }
        self
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

/// Anything that can go wrong in a use case.
#[derive(Debug)]
pub enum Error {
    /// A failure the client may see, with its code.
    App(AppError),
    /// Anything else (a driver error, a broken connection). Reported as `INTERNAL-00` without its text.
    Unexpected(Box<dyn std::error::Error + Send + Sync>),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn unexpected(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Unexpected(Box::new(error))
    }

    /// The code of an application error, for the callers that translate one into another.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::App(error) => Some(error.code),
            Self::Unexpected(_) => None,
        }
    }
}

impl From<AppError> for Error {
    fn from(error: AppError) -> Self {
        Self::App(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::App(error) => error.fmt(f),
            Self::Unexpected(error) => write!(f, "unexpected error: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::App(error) => Some(error),
            Self::Unexpected(error) => Some(error.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_lists_every_code_of_this_file() {
        let source = include_str!("error.rs");
        let defined: Vec<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub const ")?.split_once(": &str = \"")?.1.strip_suffix("\";"))
            .collect();
        assert!(defined.len() >= 24, "{defined:?}");
        for code in &defined {
            assert!(codes::ALL.contains(code), "{code} is missing from codes::ALL");
        }
        assert_eq!(codes::ALL.len(), defined.len());
    }

    #[test]
    fn the_unmatched_route_and_method_helpers_use_their_own_codes() {
        assert_eq!(AppError::route_not_found().code, "RESOURCE-NOT-FOUND-00");
        assert_eq!(AppError::method_not_allowed().kind, ErrorKind::MethodNotAllowed);
        assert_eq!(AppError::method_not_allowed().code, "VALIDATION-13");
    }

    #[test]
    fn invalid_field_names_the_field() {
        let error = AppError::invalid_field("name", "name must not be blank");
        assert_eq!(error.code, "VALIDATION-01");
        assert_eq!(error.kind, ErrorKind::Validation);
        assert_eq!(error.params["field"], "name");
    }

    #[test]
    fn params_keep_their_json_type() {
        let error = AppError::validation(codes::ITEM_LIMIT, "too many").with("max", 200).with("ok", true);
        assert_eq!(error.params["max"], 200);
        assert_eq!(error.params["ok"], true);
    }

    #[test]
    fn the_catalog_helpers_use_the_right_kind_and_code() {
        assert_eq!(AppError::unauthenticated().code, "AUTH-01");
        assert_eq!(AppError::forbidden().code, "AUTH-02");
        assert_eq!(AppError::rate_limited().code, "RATE-00");
        let large = AppError::too_large(1024);
        assert_eq!((large.kind, large.code), (ErrorKind::TooLarge, "VALIDATION-12"));
        assert_eq!(large.params["max_bytes"], 1024);
        assert_eq!(AppError::not_found(codes::ORDER_NOT_FOUND, "x").kind, ErrorKind::NotFound);
        assert_eq!(AppError::conflict(codes::DUPLICATE_TAX_ID, "x").kind, ErrorKind::Conflict);
    }

    #[test]
    fn an_error_exposes_the_code_of_an_application_error_only() {
        let app: Error = AppError::forbidden().into();
        assert_eq!(app.code(), Some("AUTH-02"));
        let other = Error::unexpected(std::io::Error::other("disk"));
        assert_eq!(other.code(), None);
        assert!(other.to_string().contains("disk"));
    }
}
