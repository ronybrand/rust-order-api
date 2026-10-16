//! Translates between HTTP and the use cases. It holds no business rule.

use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tracing::error;
use uuid::Uuid;

use super::wire::{
    CustomerRequest, CustomerResponse, ItemRequest, OrderRequest, OrderResponse, PageResponse, QuantityRequest,
    parse_search_body,
};
use super::{AppState, envelope::json_response};
use crate::app::{Actor, SearchRequest};
use crate::domain::ItemInput;
use crate::error::{AppError, Error};
use crate::persistence::plan::parse_canonical_uuid;

type Reply = Result<Response, Error>;

/// The single `{id}` of a path. A path that cannot be decoded is VALIDATION-01 in the usual envelope, not a plain-text
/// rejection.
pub struct One(String);

impl<S: Send + Sync> FromRequestParts<S> for One {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Error> {
        let Path(id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| AppError::invalid_field("id", "id must be a valid UUID"))?;
        Ok(Self(id))
    }
}

/// The `{id}` and `{item_id}` of an item path.
pub struct Two(String, String);

impl<S: Send + Sync> FromRequestParts<S> for Two {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Error> {
        let Path((order, item)) = Path::<(String, String)>::from_request_parts(parts, state)
            .await
            .map_err(|_| AppError::invalid_field("id", "id must be a valid UUID"))?;
        Ok(Self(order, item))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for QueryParams {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Error> {
        let Query(params) = Query::<RawQuery>::from_request_parts(parts, state)
            .await
            .map_err(|_| AppError::invalid_field("query", "the query string is not valid"))?;
        Ok(params.into())
    }
}

fn path_id(name: &str, raw: &str) -> Result<Uuid, AppError> {
    parse_canonical_uuid(raw).ok_or_else(|| AppError::invalid_field(name, format!("{name} must be a valid UUID")))
}

/// Reads the body, counting the bytes actually read so a chunked body cannot skip the limit.
async fn read_body(state: &AppState, body: Body) -> Result<Vec<u8>, Error> {
    let max = state.config.max_body_bytes;
    let limit = usize::try_from(max).unwrap_or(usize::MAX);
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes().to_vec()),
        Err(failure) if failure.downcast_ref::<LengthLimitError>().is_some() => Err(AppError::too_large(max).into()),
        Err(_) => Err(AppError::invalid_field("body", "request body could not be read").into()),
    }
}

async fn read_json<T: DeserializeOwned>(state: &AppState, request: Request) -> Result<T, Error> {
    let bytes = read_body(state, request.into_body()).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| AppError::invalid_field("body", "request body is not valid for this endpoint").into())
}

// Handlers return `Reply`, so the helpers that build a successful one return it too.
#[allow(clippy::unnecessary_wraps)]
fn ok<T: Serialize>(body: T) -> Reply {
    Ok(json_response(StatusCode::OK, &body))
}

#[allow(clippy::unnecessary_wraps)]
fn created<T: Serialize>(body: T) -> Reply {
    Ok(json_response(StatusCode::CREATED, &body))
}

// /health answers 503 for any failure of the probe, whatever its type: the caller only needs to know it is down.
pub async fn health(State(state): State<AppState>) -> Response {
    match state.store.ping().await {
        Ok(()) => json_response(StatusCode::OK, &json!({ "status": "ok" })),
        Err(failure) => {
            // The reason (a connection string, a host name) stays in the log and out of the response.
            error!(reason = %failure, "health check failed");
            json_response(StatusCode::SERVICE_UNAVAILABLE, &json!({ "status": "unavailable" }))
        }
    }
}

// --- customers ------------------------------------------------------------------------------------------------

pub async fn create_customer(State(state): State<AppState>, actor: Actor, request: Request) -> Reply {
    let body: CustomerRequest = read_json(&state, request).await?;
    created(CustomerResponse::from(state.customers.create(&actor, body.into()).await?))
}

pub async fn get_customer(State(state): State<AppState>, One(id): One) -> Reply {
    ok(CustomerResponse::from(state.customers.get(path_id("id", &id)?).await?))
}

pub async fn update_customer(State(state): State<AppState>, actor: Actor, One(id): One, request: Request) -> Reply {
    let id = path_id("id", &id)?;
    let body: CustomerRequest = read_json(&state, request).await?;
    ok(CustomerResponse::from(state.customers.update(&actor, id, body.into()).await?))
}

pub async fn delete_customer(State(state): State<AppState>, actor: Actor, One(id): One) -> Reply {
    state.customers.delete(&actor, path_id("id", &id)?).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn search_customers_query(State(state): State<AppState>, query: QueryParams) -> Reply {
    let page = state.customers.search(query.into_request()?).await?;
    ok(PageResponse::<CustomerResponse>::from_page(page))
}

pub async fn search_customers_body(State(state): State<AppState>, request: Request) -> Reply {
    let bytes = read_body(&state, request.into_body()).await?;
    let page = state.customers.search(parse_search_body(&bytes)?).await?;
    ok(PageResponse::<CustomerResponse>::from_page(page))
}

// --- orders ---------------------------------------------------------------------------------------------------

pub async fn create_order(State(state): State<AppState>, actor: Actor, request: Request) -> Reply {
    let body: OrderRequest = read_json(&state, request).await?;
    let items = body.items.into_iter().map(ItemInput::from).collect();
    created(OrderResponse::from(state.orders.create(&actor, body.customer_id, items).await?))
}

pub async fn get_order(State(state): State<AppState>, One(id): One) -> Reply {
    ok(OrderResponse::from(state.orders.get(path_id("id", &id)?).await?))
}

pub async fn delete_order(State(state): State<AppState>, actor: Actor, One(id): One) -> Reply {
    state.orders.delete(&actor, path_id("id", &id)?).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn add_item(State(state): State<AppState>, actor: Actor, One(id): One, request: Request) -> Reply {
    let id = path_id("id", &id)?;
    let body: ItemRequest = read_json(&state, request).await?;
    ok(OrderResponse::from(state.orders.add_item(&actor, id, body.into()).await?))
}

pub async fn update_item(
    State(state): State<AppState>,
    actor: Actor,
    Two(order_id, item_id): Two,
    request: Request,
) -> Reply {
    let order_id = path_id("id", &order_id)?;
    let item_id = path_id("item_id", &item_id)?;
    let body: QuantityRequest = read_json(&state, request).await?;
    ok(OrderResponse::from(state.orders.update_item_quantity(&actor, order_id, item_id, body.quantity).await?))
}

pub async fn remove_item(State(state): State<AppState>, actor: Actor, Two(order_id, item_id): Two) -> Reply {
    let order_id = path_id("id", &order_id)?;
    let item_id = path_id("item_id", &item_id)?;
    ok(OrderResponse::from(state.orders.remove_item(&actor, order_id, item_id).await?))
}

pub async fn confirm_order(State(state): State<AppState>, actor: Actor, One(id): One) -> Reply {
    ok(OrderResponse::from(state.orders.confirm(&actor, path_id("id", &id)?).await?))
}

pub async fn cancel_order(State(state): State<AppState>, actor: Actor, One(id): One) -> Reply {
    ok(OrderResponse::from(state.orders.cancel(&actor, path_id("id", &id)?).await?))
}

pub async fn search_orders_query(State(state): State<AppState>, query: QueryParams) -> Reply {
    let page = state.orders.search(query.into_request()?).await?;
    ok(PageResponse::<OrderResponse>::from_page(page))
}

pub async fn search_orders_body(State(state): State<AppState>, request: Request) -> Reply {
    let bytes = read_body(&state, request.into_body()).await?;
    let page = state.orders.search(parse_search_body(&bytes)?).await?;
    ok(PageResponse::<OrderResponse>::from_page(page))
}

/// The GET form of a search: page, size and sort only. Filters need the POST form.
#[derive(Debug, serde::Deserialize, Default)]
struct RawQuery {
    page: Option<String>,
    size: Option<String>,
    sort: Option<String>,
}

#[derive(Debug, Default)]
pub struct QueryParams(RawQuery);

impl From<RawQuery> for QueryParams {
    fn from(raw: RawQuery) -> Self {
        Self(raw)
    }
}

impl QueryParams {
    fn into_request(self) -> Result<SearchRequest, AppError> {
        fn number(name: &str, raw: Option<&str>) -> Result<i32, AppError> {
            match raw {
                None | Some("") => Ok(0),
                Some(text) => {
                    text.parse().map_err(|_| AppError::invalid_field(name, format!("{name} must be an integer")))
                }
            }
        }
        let RawQuery { page, size, sort } = self.0;
        Ok(SearchRequest {
            page: number("page", page.as_deref())?,
            size: number("size", size.as_deref())?,
            sort: sort.unwrap_or_default(),
            ..SearchRequest::default()
        })
    }
}
