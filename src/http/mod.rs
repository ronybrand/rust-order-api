//! The HTTP application: routes plus the middleware stack, with everything it needs passed in.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderName, HeaderValue, Method, header};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, patch, post, put};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::error;

use crate::app::{CustomerService, OrderService, Store};
use crate::auth::JwtVerifier;
use crate::error::{AppError, Error};

mod envelope;
mod handlers;
mod limiter;
mod middleware;
pub mod wire;

#[cfg(test)]
mod tests;

pub use envelope::{REQUEST_ID_HEADER, RequestId, json_response};
pub use limiter::{KeyedLimiter, NowMillis, system_millis};

pub const HEALTH_PATH: &str = "/health";

const DEFAULT_MAX_BODY_BYTES: u64 = 1 << 20;
const DEFAULT_RATE_PER_USER: u32 = 120;
const DEFAULT_RATE_WRITES: u32 = 30;
const DEFAULT_RATE_PER_IP: u32 = 600;
const PREFLIGHT_MAX_AGE: Duration = Duration::from_secs(600);

/// The HTTP-layer settings; the defaults are the ones of the other implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    /// Exactly these origins get CORS headers; empty means CORS is off.
    pub cors_origins: Vec<String>,
    /// Counts the bytes actually read, so a chunked body cannot skip it.
    pub max_body_bytes: u64,
    /// Requests per minute per authenticated user, any method.
    pub rate_limit_per_user: u32,
    /// Requests per minute per authenticated user, POST/PUT/PATCH/DELETE only.
    pub rate_limit_writes: u32,
    /// Requests per minute per client address, applied before authentication.
    pub rate_limit_per_ip: u32,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            cors_origins: Vec::new(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            rate_limit_per_user: DEFAULT_RATE_PER_USER,
            rate_limit_writes: DEFAULT_RATE_WRITES,
            rate_limit_per_ip: DEFAULT_RATE_PER_IP,
        }
    }
}

/// Everything the HTTP layer needs, passed in: nothing is read from globals.
pub struct ApiDependencies {
    pub customers: Arc<CustomerService>,
    pub orders: Arc<OrderService>,
    pub verifier: Arc<JwtVerifier>,
    /// Only used to check the database for /health.
    pub store: Arc<dyn Store>,
    pub config: HttpConfig,
    pub now_millis: NowMillis,
}

opaque_debug!(ApiDependencies);

/// What the handlers and middleware share. Cheap to clone: everything in it is behind an `Arc`.
#[derive(Clone)]
pub struct AppState {
    customers: Arc<CustomerService>,
    orders: Arc<OrderService>,
    verifier: Arc<JwtVerifier>,
    store: Arc<dyn Store>,
    config: Arc<HttpConfig>,
    ip_limiter: Arc<KeyedLimiter>,
    user_limiter: Arc<KeyedLimiter>,
    write_limiter: Arc<KeyedLimiter>,
}

opaque_debug!(AppState);

impl AppState {
    fn new(dependencies: ApiDependencies) -> Self {
        let ApiDependencies { customers, orders, verifier, store, config, now_millis } = dependencies;
        let limiter = |per_minute| Arc::new(KeyedLimiter::new(per_minute, Arc::clone(&now_millis)));
        Self {
            customers,
            orders,
            verifier,
            store,
            ip_limiter: limiter(config.rate_limit_per_ip),
            user_limiter: limiter(config.rate_limit_per_user),
            write_limiter: limiter(config.rate_limit_writes),
            config: Arc::new(config),
        }
    }
}

/// Who may call an endpoint. Declared for every one of them: there is no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Public,
    User,
    Admin,
}

struct Route {
    method: Method,
    path: &'static str,
    access: Access,
    handler: MethodRouter<AppState>,
}

fn route(method: Method, path: &'static str, access: Access, handler: MethodRouter<AppState>) -> Route {
    Route { method, path, access, handler }
}

/// Every route of the API, with the access it requires.
fn routes() -> Vec<Route> {
    use handlers as h;
    vec![
        route(Method::GET, HEALTH_PATH, Access::Public, get(h::health)),
        route(Method::POST, "/customers", Access::Admin, post(h::create_customer)),
        route(Method::GET, "/customers/search", Access::User, get(h::search_customers_query)),
        route(Method::POST, "/customers/search", Access::User, post(h::search_customers_body)),
        route(Method::GET, "/customers/{id}", Access::User, get(h::get_customer)),
        route(Method::PUT, "/customers/{id}", Access::Admin, put(h::update_customer)),
        route(Method::DELETE, "/customers/{id}", Access::Admin, delete(h::delete_customer)),
        route(Method::POST, "/orders", Access::User, post(h::create_order)),
        route(Method::GET, "/orders/search", Access::User, get(h::search_orders_query)),
        route(Method::POST, "/orders/search", Access::User, post(h::search_orders_body)),
        route(Method::GET, "/orders/{id}", Access::User, get(h::get_order)),
        route(Method::DELETE, "/orders/{id}", Access::User, delete(h::delete_order)),
        route(Method::POST, "/orders/{id}/items", Access::User, post(h::add_item)),
        route(Method::PATCH, "/orders/{id}/items/{item_id}", Access::User, patch(h::update_item)),
        route(Method::DELETE, "/orders/{id}/items/{item_id}", Access::User, delete(h::remove_item)),
        route(Method::POST, "/orders/{id}/confirm", Access::User, post(h::confirm_order)),
        route(Method::POST, "/orders/{id}/cancel", Access::User, post(h::cancel_order)),
    ]
}

/// The routes of the API as method, path and access, for the test that compares them with the `OpenAPI` document.
pub fn route_table() -> Vec<(Method, &'static str, Access)> {
    routes().into_iter().map(|r| (r.method, r.path, r.access)).collect()
}

/// A protected route is installed behind authentication (and, for the admin ones, the role check): the layers are
/// added here, per route, so that a handler cannot be registered without saying who may call it.
fn protected(handler: MethodRouter<AppState>, access: Access, state: &AppState) -> MethodRouter<AppState> {
    match access {
        Access::Public => handler,
        Access::User => handler.route_layer(from_fn_with_state(state.clone(), middleware::authenticate)),
        // The last layer added runs first: authentication, then the role check.
        Access::Admin => handler
            .route_layer(from_fn(middleware::require_admin))
            .route_layer(from_fn_with_state(state.clone(), middleware::authenticate)),
    }
}

/// The whole HTTP application.
pub fn order_api(dependencies: ApiDependencies) -> Router {
    let state = AppState::new(dependencies);

    let mut by_path: BTreeMap<&'static str, MethodRouter<AppState>> = BTreeMap::new();
    for route in routes() {
        let handler = protected(route.handler, route.access, &state);
        let merged = match by_path.remove(route.path) {
            Some(existing) => existing.merge(handler),
            None => handler,
        };
        by_path.insert(route.path, merged);
    }
    let mut router = Router::new();
    for (path, handler) in by_path {
        router = router.route(path, handler);
    }

    let router = router
        .fallback(|| async { Error::from(AppError::route_not_found()) })
        .method_not_allowed_fallback(|| async { Error::from(AppError::method_not_allowed()) });

    // The layer added last is the outermost: the request id and the envelope wrap everything, so they cover every
    // failure below them, including a panic.
    let router = router.layer(from_fn_with_state(state.clone(), middleware::ip_rate_limit));
    let router = match cors(&state.config.cors_origins) {
        Some(layer) => router.layer(layer),
        None => router,
    };
    router
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(from_fn(middleware::access_log))
        .layer(from_fn(middleware::security_headers))
        .layer(from_fn(envelope::request_context))
        .with_state(state)
}

// The signature is the one `CatchPanicLayer::custom` calls with.
#[allow(clippy::needless_pass_by_value)]
fn panic_response(payload: Box<dyn Any + Send + 'static>) -> Response {
    let reason = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a handler panicked".to_owned());
    error!(reason, "handler panicked");
    Error::unexpected(std::io::Error::other("handler panicked")).into_response()
}

fn cors(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let allowed: Vec<HeaderValue> = origins.iter().filter_map(|origin| HeaderValue::from_str(origin).ok()).collect();
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(allowed))
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, HeaderName::from_static(REQUEST_ID_HEADER)])
            .expose_headers([HeaderName::from_static(REQUEST_ID_HEADER), header::RETRY_AFTER])
            .max_age(PREFLIGHT_MAX_AGE),
    )
}
