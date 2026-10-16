use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::info;

use super::envelope::RequestId;
use super::limiter::KeyedLimiter;
use super::{AppState, HEALTH_PATH};
use crate::app::Actor;
use crate::auth::ROLE_ADMIN;
use crate::error::{AppError, Error};

const NO_ADDRESS: &str = "unknown";

/// The baseline response headers of an API that serves JSON only.
pub async fn security_headers(request: Request, next: Next) -> Response {
    const HEADERS: [(&str, &str); 7] = [
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
        ("content-security-policy", "default-src 'none'; frame-ancestors 'none'"),
        ("strict-transport-security", "max-age=63072000; includeSubDomains"),
        ("permissions-policy", "geolocation=(), microphone=(), camera=()"),
    ];
    let mut response = next.run(request).await;
    for (name, value) in HEADERS {
        response.headers_mut().insert(HeaderName::from_static(name), HeaderValue::from_static(value));
    }
    response
}

/// One line per request. The query string is left out on purpose: search filters can carry personal data (an e-mail, a
/// tax id).
pub async fn access_log(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let (method, path) = (request.method().clone(), request.uri().path().to_owned());
    let request_id = request.extensions().get::<RequestId>().map(|id| id.0.clone()).unwrap_or_default();
    let response = next.run(request).await;
    info!(
        target: "access",
        method = %method,
        path,
        status = response.status().as_u16(),
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        request_id,
        "request"
    );
    response
}

fn rate_limited(limiter: &KeyedLimiter) -> Response {
    let mut response = AppError::rate_limited().into_response();
    if let Ok(value) = HeaderValue::from_str(&limiter.retry_after_seconds()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// A coarse limit per client address, before authentication; /health is exempt so probes never get throttled.
pub async fn ip_rate_limit(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.uri().path() != HEALTH_PATH {
        let address = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map_or_else(|| NO_ADDRESS.to_owned(), |info| info.0.ip().to_string());
        if !state.ip_limiter.allow(&address) {
            return rate_limited(&state.ip_limiter);
        }
    }
    next.run(request).await
}

fn unauthenticated(invalid_token: bool) -> Response {
    let mut response = AppError::unauthenticated().into_response();
    let challenge = if invalid_token { r#"Bearer error="invalid_token""# } else { "Bearer" };
    response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(challenge));
    response
}

/// Requires a valid bearer token, puts the caller on the request, and applies the per-user limits (a general one, and
/// a separate one for writes). Every route is installed with this or declared public: a route is never open by
/// omission.
pub async fn authenticate(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let header = request.headers().get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).unwrap_or("");
    let fields: Vec<&str> = header.split_whitespace().collect();
    let [scheme, token] = fields[..] else { return unauthenticated(false) };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return unauthenticated(false);
    }
    let Ok(claims) = state.verifier.verify(token) else { return unauthenticated(true) };
    let actor = Actor::new(claims.subject, claims.roles);

    if !state.user_limiter.allow(&actor.id) {
        return rate_limited(&state.user_limiter);
    }
    let safe = matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && !state.write_limiter.allow(&actor.id) {
        return rate_limited(&state.write_limiter);
    }
    request.extensions_mut().insert(actor);
    next.run(request).await
}

/// Must run after [`authenticate`]: with no actor on the request it denies.
pub async fn require_admin(request: Request, next: Next) -> Response {
    match request.extensions().get::<Actor>() {
        Some(actor) if actor.has_role(ROLE_ADMIN) => next.run(request).await,
        _ => AppError::forbidden().into_response(),
    }
}

/// The caller of a protected route, as put on the request by [`authenticate`]. A handler that asks for it without the
/// route being behind authentication gets a 401 instead of a panic.
impl<S: Send + Sync> FromRequestParts<S> for Actor {
    type Rejection = Error;

    fn from_request_parts(parts: &mut Parts, _state: &S) -> impl Future<Output = Result<Self, Error>> + Send {
        std::future::ready(parts.extensions.get::<Actor>().cloned().ok_or_else(|| AppError::unauthenticated().into()))
    }
}
