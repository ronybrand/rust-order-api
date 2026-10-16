//! The error body of the whole API: `{message, code, params, request_id}`.
//!
//! Anything that fails (a handler, an extractor, a middleware, a panic, an unmatched route) just returns an [`Error`].
//! Turning it into a response happens in one place, [`request_context`], which is the only code that knows the request
//! id, so no failure path can forget it.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Map, Value};
use tracing::error;
use uuid::Uuid;

use crate::error::{AppError, Error, ErrorKind, codes};
use crate::persistence::plan::parse_canonical_uuid;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The id of the request being served, for the handlers and the access log.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// Carried on a response from the code that failed to the layer that renders the envelope.
#[derive(Clone)]
struct Failure(Arc<Error>);

pub fn status_of(kind: ErrorKind) -> StatusCode {
    match kind {
        ErrorKind::Validation => StatusCode::BAD_REQUEST,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        ErrorKind::Conflict => StatusCode::CONFLICT,
        ErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorKind::Forbidden => StatusCode::FORBIDDEN,
        ErrorKind::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorKind::RateLimited => StatusCode::TOO_MANY_REQUESTS,
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::App(app) => status_of(app.kind),
            Self::Unexpected(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut response = Response::new(Body::empty());
        *response.status_mut() = status;
        response.extensions_mut().insert(Failure(Arc::new(self)));
        response
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        Error::App(self).into_response()
    }
}

#[derive(Serialize)]
struct Envelope<'a> {
    message: &'a str,
    code: &'a str,
    params: &'a Map<String, Value>,
    request_id: &'a str,
}

/// A JSON response with the given status.
pub fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = status;
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            response
        }
        Err(failure) => Error::unexpected(failure).into_response(),
    }
}

/// Keeps an incoming `X-Request-Id` only when it is a canonical UUID: anything else (log injection, absurdly long
/// values) is replaced by a fresh one. Renders the envelope of a failed response and echoes the id.
pub async fn request_context(mut request: Request, next: Next) -> Response {
    let incoming = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| parse_canonical_uuid(value).is_some())
        .map(str::to_owned);
    let id = incoming.unwrap_or_else(|| Uuid::new_v4().to_string());
    request.extensions_mut().insert(RequestId(id.clone()));
    let (method, path) = (request.method().clone(), request.uri().path().to_owned());

    let mut response = next.run(request).await;
    if let Some(Failure(failure)) = response.extensions_mut().remove::<Failure>() {
        render(&mut response, &failure, &id, &format!("{method} {path}"));
    }
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

fn render(response: &mut Response, failure: &Error, request_id: &str, route: &str) {
    let empty = Map::new();
    let (status, code, message, params) = match failure {
        Error::App(app) => (status_of(app.kind), app.code, app.message.as_str(), &app.params),
        Error::Unexpected(cause) => {
            // The client only ever sees INTERNAL-00; the cause stays in the log.
            error!(route, request_id, cause = %cause, "unexpected error");
            (StatusCode::INTERNAL_SERVER_ERROR, codes::INTERNAL, "Internal error", &empty)
        }
    };
    let body = serde_json::to_vec(&Envelope { message, code, params, request_id }).unwrap_or_default();
    *response.status_mut() = status;
    *response.body_mut() = Body::from(body);
    let headers = response.headers_mut();
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
}
