//! The HTTP application against the in-memory store: the status codes, the envelope, the headers, the limits and the
//! access rules, with no network and no database.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use futures_util::stream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::{Access, ApiDependencies, HttpConfig, order_api, route_table};
use crate::app::fake::{MemoryStore, RecordingPublisher};
use crate::app::{CustomerService, OrderService, system_clock};
use crate::auth::JwtVerifier;
use crate::auth::testing::{AUDIENCE, ISSUER, claims, keys, now, sign, token};
use crate::error::{Error, codes};

struct Api {
    app: Router,
    store: MemoryStore,
    publisher: RecordingPublisher,
    millis: Arc<AtomicU64>,
    admin: String,
    user: String,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|e| panic!("not json ({e}): {}", self.text))
    }

    fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or("").to_owned()
    }
}

fn api() -> Api {
    api_with(HttpConfig::default())
}

fn api_with(config: HttpConfig) -> Api {
    let store = MemoryStore::new();
    let publisher = RecordingPublisher::default();
    let millis = Arc::new(AtomicU64::new(1_000_000));
    let reader = Arc::clone(&millis);
    let shared: Arc<dyn crate::app::Store> = Arc::new(store.clone());
    let app = order_api(ApiDependencies {
        customers: Arc::new(CustomerService::new(Arc::clone(&shared), system_clock())),
        orders: Arc::new(OrderService::new(Arc::clone(&shared), Some(Arc::new(publisher.clone())), system_clock())),
        verifier: Arc::new(JwtVerifier::from_pem(keys().public_pem.as_bytes(), AUDIENCE, ISSUER).unwrap()),
        store: shared,
        config,
        now_millis: Arc::new(move || reader.load(Ordering::SeqCst)),
    });
    Api { app, store, publisher, millis, admin: token("admin-1", &["user", "admin"]), user: token("user-1", &["user"]) }
}

impl Api {
    async fn send(&self, mut request: Request<Body>) -> Reply {
        if request.extensions().get::<ConnectInfo<SocketAddr>>().is_none() {
            request.extensions_mut().insert(ConnectInfo::<SocketAddr>("203.0.113.9:4000".parse().unwrap()));
        }
        let response = self.app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        Reply { status: parts.status, headers: parts.headers, text: String::from_utf8_lossy(&bytes).into_owned() }
    }

    async fn call(&self, method: Method, path: &str, bearer: Option<&str>, body: Option<&str>) -> Reply {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        self.send(builder.body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned()))).unwrap()).await
    }

    async fn as_admin(&self, method: Method, path: &str, body: Option<&str>) -> Reply {
        self.call(method, path, Some(&self.admin), body).await
    }

    async fn as_user(&self, method: Method, path: &str, body: Option<&str>) -> Reply {
        self.call(method, path, Some(&self.user), body).await
    }

    async fn customer(&self) -> String {
        let reply = self
            .as_admin(
                Method::POST,
                "/customers",
                Some(r#"{"name":"Ada Lovelace","tax_id":"123.456-78","email":"ada@example.com"}"#),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
        reply.json()["id"].as_str().unwrap().to_owned()
    }

    async fn order(&self, items: &str) -> Value {
        let customer = self.customer().await;
        let body = format!(r#"{{"customer_id":"{customer}","items":{items}}}"#);
        let reply = self.as_user(Method::POST, "/orders", Some(&body)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
        reply.json()
    }
}

const PEN: &str =
    r#"[{"description":"Pen","unit_price":0.10,"quantity":3},{"description":"Book","unit_price":19.99,"quantity":2}]"#;

// --- health and the envelope -----------------------------------------------------------------------------------

#[tokio::test]
async fn health_is_public_and_reports_the_database() {
    let api = api();
    let reply = api.call(Method::GET, "/health", None, None).await;
    assert_eq!((reply.status, reply.json()), (StatusCode::OK, json!({"status": "ok"})));

    api.store.set_ping_failure(true);
    let down = api.call(Method::GET, "/health", None, None).await;
    assert_eq!((down.status, down.json()), (StatusCode::SERVICE_UNAVAILABLE, json!({"status": "unavailable"})));
    assert!(!down.text.contains("database unavailable"), "the reason stays out of the response");
}

#[tokio::test]
async fn every_error_uses_the_same_envelope_with_the_request_id() {
    let api = api();
    let reply = api.call(Method::GET, "/orders/search", None, None).await;
    let body = reply.json();
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "AUTH-01");
    assert_eq!(body["message"], "Authentication required");
    assert_eq!(body["params"], json!({}));
    assert_eq!(body["request_id"], reply.header("x-request-id"));
    assert!(Uuid::parse_str(body["request_id"].as_str().unwrap()).is_ok());
    assert_eq!(reply.header("content-type"), "application/json");
}

#[tokio::test]
async fn a_valid_incoming_request_id_is_kept_and_anything_else_is_replaced() {
    let api = api();
    let id = Uuid::new_v4().to_string();
    let kept = api.send(Request::get("/health").header("x-request-id", &id).body(Body::empty()).unwrap()).await;
    assert_eq!(kept.header("x-request-id"), id);

    for bad in ["not-a-uuid", "../../etc/passwd", "123e4567e89b42d3a456426614174000", &"a".repeat(500)] {
        let reply = api.send(Request::get("/health").header("x-request-id", bad).body(Body::empty()).unwrap()).await;
        assert_ne!(reply.header("x-request-id"), bad);
        assert!(Uuid::parse_str(reply.header("x-request-id")).is_ok());
    }
    let generated = api.call(Method::GET, "/health", None, None).await;
    assert!(Uuid::parse_str(generated.header("x-request-id")).is_ok());
}

#[tokio::test]
async fn the_security_headers_are_on_every_response_including_errors() {
    let api = api();
    for reply in
        [api.call(Method::GET, "/health", None, None).await, api.call(Method::GET, "/nowhere", None, None).await]
    {
        assert_eq!(reply.header("x-content-type-options"), "nosniff");
        assert_eq!(reply.header("x-frame-options"), "DENY");
        assert_eq!(reply.header("referrer-policy"), "no-referrer");
        assert_eq!(reply.header("cache-control"), "no-store");
        assert_eq!(reply.header("content-security-policy"), "default-src 'none'; frame-ancestors 'none'");
        assert!(reply.header("strict-transport-security").starts_with("max-age="));
        assert!(!reply.header("permissions-policy").is_empty());
    }
}

#[tokio::test]
async fn an_unknown_route_is_a_404_and_a_wrong_method_a_405_with_allow() {
    let api = api();
    let missing = api.as_admin(Method::GET, "/nowhere", None).await;
    assert_eq!((missing.status, missing.code()), (StatusCode::NOT_FOUND, "RESOURCE-NOT-FOUND-00".to_owned()));

    let wrong = api.as_admin(Method::PUT, "/orders", Some("{}")).await;
    assert_eq!((wrong.status, wrong.code()), (StatusCode::METHOD_NOT_ALLOWED, "VALIDATION-13".to_owned()));
    assert!(wrong.header("allow").contains("POST"), "allow: {}", wrong.header("allow"));
    assert_eq!(wrong.json()["request_id"], wrong.header("x-request-id"));
}

#[tokio::test]
async fn an_unexpected_failure_is_internal_00_without_its_message() {
    let api = api();
    let order = api.order(PEN).await;
    api.store
        .fail_order_save_with(|| Error::unexpected(std::io::Error::other("connection to 10.0.0.5 refused: secret")));
    let reply = api.as_user(Method::POST, &format!("/orders/{}/confirm", order["id"].as_str().unwrap()), None).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL-00".to_owned()));
    assert_eq!(reply.json()["message"], "Internal error");
    assert!(!reply.text.contains("10.0.0.5") && !reply.text.contains("secret"));
}

#[tokio::test]
async fn a_panicking_handler_is_an_internal_error_and_not_a_dropped_connection() {
    let response = super::panic_response(Box::new("boom: secret"));
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

// --- authentication and authorization ------------------------------------------------------------------------

#[tokio::test]
async fn a_protected_route_needs_a_bearer_token() {
    let api = api();
    for authorization in [None, Some(""), Some("Bearer"), Some("Basic abc"), Some("Bearer a b"), Some("Token abc")] {
        let mut request = Request::get("/orders/search");
        if let Some(value) = authorization {
            request = request.header("authorization", value);
        }
        let reply = api.send(request.body(Body::empty()).unwrap()).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::UNAUTHORIZED, "AUTH-01".to_owned()), "{authorization:?}");
        assert_eq!(reply.header("www-authenticate"), "Bearer");
    }
}

#[tokio::test]
async fn the_scheme_is_case_insensitive() {
    let api = api();
    let request =
        Request::get("/orders/search").header("authorization", format!("bEaReR {}", api.user)).body(Body::empty());
    assert_eq!(api.send(request.unwrap()).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_bad_token_is_a_401_that_says_invalid_token() {
    let api = api();
    let expired = sign(&{
        let mut c = claims("user-1", &["user"]);
        c["exp"] = json!(now() - 3600);
        c
    });
    for bad in ["garbage", expired.as_str()] {
        let reply = api.call(Method::GET, "/orders/search", Some(bad), None).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::UNAUTHORIZED, "AUTH-01".to_owned()));
        assert_eq!(reply.header("www-authenticate"), r#"Bearer error="invalid_token""#);
    }
}

#[tokio::test]
async fn customer_writes_need_the_admin_role() {
    let api = api();
    let body = r#"{"name":"Ada","tax_id":"123.456-78","email":"ada@example.com"}"#;
    let denied = api.as_user(Method::POST, "/customers", Some(body)).await;
    assert_eq!((denied.status, denied.code()), (StatusCode::FORBIDDEN, "AUTH-02".to_owned()));
    assert!(api.store.data().customers.is_empty());

    let id = api.customer().await;
    for (method, body) in [(Method::PUT, Some(body)), (Method::DELETE, None)] {
        let reply = api.as_user(method, &format!("/customers/{id}"), body).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }
    assert_eq!(api.as_user(Method::GET, &format!("/customers/{id}"), None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_token_without_roles_is_a_plain_user_and_an_empty_role_list_is_nobody() {
    let api = api();
    let mut no_roles = claims("user-2", &[]);
    no_roles.as_object_mut().unwrap().remove("roles");
    let plain = sign(&no_roles);
    assert_eq!(api.call(Method::GET, "/orders/search", Some(&plain), None).await.status, StatusCode::OK);
    let forbidden = api.call(Method::POST, "/customers", Some(&plain), Some("{}")).await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_audit_columns_come_from_the_token_never_from_the_body() {
    let api = api();
    let reply = api
        .as_admin(
            Method::POST,
            "/customers",
            Some(r#"{"name":"Ada","tax_id":"123.456-78","email":"ada@example.com","created_by":"someone-else"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "an extra field is refused, not obeyed");
    let id = api.customer().await;
    let stored = api.store.data().customers[&id.parse::<Uuid>().unwrap()].clone();
    assert_eq!(stored.created_by, "admin-1");
}

// --- customers --------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_customer_is_created_read_updated_and_deleted() {
    let api = api();
    let id = api.customer().await;

    let read = api.as_user(Method::GET, &format!("/customers/{id}"), None).await;
    assert_eq!(read.json(), json!({"id": id, "name": "Ada Lovelace"}));

    let body = r#"{"name":"Ada King","tax_id":"123.456-78","email":"ada@example.com"}"#;
    let updated = api.as_admin(Method::PUT, &format!("/customers/{id}"), Some(body)).await;
    assert_eq!((updated.status, updated.json()["name"].clone()), (StatusCode::OK, json!("Ada King")));

    let deleted = api.as_admin(Method::DELETE, &format!("/customers/{id}"), None).await;
    assert_eq!((deleted.status, deleted.text.as_str()), (StatusCode::NO_CONTENT, ""));
    let gone = api.as_user(Method::GET, &format!("/customers/{id}"), None).await;
    assert_eq!((gone.status, gone.code()), (StatusCode::NOT_FOUND, "RESOURCE-NOT-FOUND-01".to_owned()));
}

#[tokio::test]
async fn a_customer_response_carries_no_personal_data() {
    let api = api();
    let body = r#"{"name":"Ada","tax_id":"123.456-78","passport_number":"AB123456","email":"ada@example.com"}"#;
    let created = api.as_admin(Method::POST, "/customers", Some(body)).await;
    let keys: Vec<_> = created.json().as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["id", "name"]);
    for secret in ["12345678", "AB123456", "ada@example.com"] {
        assert!(!created.text.contains(secret));
    }
}

#[tokio::test]
async fn a_conflict_does_not_echo_the_document() {
    let api = api();
    api.customer().await;
    let again = api
        .as_admin(
            Method::POST,
            "/customers",
            Some(r#"{"name":"Other","tax_id":"12345678","email":"other@example.com"}"#),
        )
        .await;
    assert_eq!((again.status, again.code()), (StatusCode::CONFLICT, "CONFLICT-01".to_owned()));
    assert!(!again.text.contains("12345678"));
}

#[tokio::test]
async fn an_invalid_customer_names_the_field() {
    let api = api();
    let reply = api
        .as_admin(Method::POST, "/customers", Some(r#"{"name":"Ada","tax_id":"1","email":"ada@example.com"}"#))
        .await;
    assert_eq!((reply.status, reply.code()), (StatusCode::BAD_REQUEST, "VALIDATION-01".to_owned()));
    assert_eq!(reply.json()["params"], json!({"field": "tax_id"}));
}

#[tokio::test]
async fn a_path_id_must_be_a_canonical_uuid() {
    let api = api();
    for bad in ["nope", "123", "123e4567e89b42d3a456426614174000"] {
        let reply = api.as_user(Method::GET, &format!("/customers/{bad}"), None).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::BAD_REQUEST, "VALIDATION-01".to_owned()), "{bad}");
        assert_eq!(reply.json()["params"], json!({"field": "id"}));
    }
    let item = api.as_user(Method::DELETE, &format!("/orders/{}/items/nope", Uuid::new_v4()), None).await;
    assert_eq!(item.json()["params"], json!({"field": "item_id"}));
}

// --- bodies -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_malformed_body_is_validation_01_on_the_body_without_echoing_it() {
    let api = api();
    for bad in ["", "not json", "[]", r#"{"name":"A""#, r#"{"name":1,"tax_id":"x","email":"y"}"#, "\u{0}"] {
        let reply = api.as_admin(Method::POST, "/customers", Some(bad)).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::BAD_REQUEST, "VALIDATION-01".to_owned()), "{bad:?}");
        assert_eq!(reply.json()["params"], json!({"field": "body"}));
    }
}

#[tokio::test]
async fn a_body_over_the_limit_is_a_413_in_the_envelope() {
    let api = api_with(HttpConfig { max_body_bytes: 64, ..HttpConfig::default() });
    let big = format!(r#"{{"name":"{}","tax_id":"123.456-78","email":"a@b.co"}}"#, "x".repeat(200));
    let reply = api.as_admin(Method::POST, "/customers", Some(&big)).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::PAYLOAD_TOO_LARGE, "VALIDATION-12".to_owned()));
    assert_eq!(reply.json()["params"], json!({"max_bytes": 64}));
}

#[tokio::test]
async fn the_limit_counts_the_bytes_of_a_chunked_body_that_declares_no_length() {
    let api = api_with(HttpConfig { max_body_bytes: 64, ..HttpConfig::default() });
    let chunks = (0..10).map(|_| Ok::<_, std::io::Error>(bytes_of(&[b'x'; 20])));
    let request = Request::post("/customers")
        .header("authorization", format!("Bearer {}", api.admin))
        .header("content-type", "application/json")
        .body(Body::from_stream(stream::iter(chunks)))
        .unwrap();
    assert!(request.headers().get("content-length").is_none());
    let reply = api.send(request).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::PAYLOAD_TOO_LARGE, "VALIDATION-12".to_owned()));
}

fn bytes_of(content: &[u8]) -> axum::body::Bytes {
    axum::body::Bytes::copy_from_slice(content)
}

#[tokio::test]
async fn authentication_comes_before_the_body_is_read() {
    let api = api_with(HttpConfig { max_body_bytes: 8, ..HttpConfig::default() });
    let reply = api.call(Method::POST, "/customers", None, Some(&"x".repeat(100))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

// --- orders -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_order_is_created_with_an_exact_total_written_as_a_number() {
    let api = api();
    let order = api.order(PEN).await;
    assert_eq!(order["status"], "OPEN");
    assert_eq!(order["version"], 1);
    assert_eq!(order["items"].as_array().unwrap().len(), 2);

    let customer = order["customer_id"].as_str().unwrap();
    let again =
        api.as_user(Method::POST, "/orders", Some(&format!(r#"{{"customer_id":"{customer}","items":{PEN}}}"#))).await;
    assert!(again.text.contains(r#""total":40.28"#), "{}", again.text);
    assert!(again.text.contains(r#""unit_price":0.10"#) && again.text.contains(r#""subtotal":0.30"#), "{}", again.text);
}

#[tokio::test]
async fn an_order_for_an_unknown_customer_is_validation_07() {
    let api = api();
    let body = format!(r#"{{"customer_id":"{}","items":[]}}"#, Uuid::new_v4());
    let reply = api.as_user(Method::POST, "/orders", Some(&body)).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::BAD_REQUEST, "VALIDATION-07".to_owned()));
}

#[tokio::test]
async fn a_price_must_be_a_json_number_with_at_most_two_places() {
    let api = api();
    let customer = api.customer().await;
    for price in [r#""10.00""#, "10.001", "0", "-1", "1e400", "true", "null"] {
        let body = format!(
            r#"{{"customer_id":"{customer}","items":[{{"description":"x","unit_price":{price},"quantity":1}}]}}"#
        );
        let reply = api.as_user(Method::POST, "/orders", Some(&body)).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::BAD_REQUEST, "VALIDATION-01".to_owned()), "{price}");
    }
}

#[tokio::test]
async fn items_are_added_changed_and_removed() {
    let api = api();
    let order = api.order("[]").await;
    let id = order["id"].as_str().unwrap();

    let added = api
        .as_user(
            Method::POST,
            &format!("/orders/{id}/items"),
            Some(r#"{"description":"Pen","unit_price":2.50,"quantity":2}"#),
        )
        .await;
    assert_eq!(added.status, StatusCode::OK);
    let item = added.json()["items"][0]["id"].as_str().unwrap().to_owned();
    assert!(added.text.contains(r#""total":5.00"#));

    let changed = api.as_user(Method::PATCH, &format!("/orders/{id}/items/{item}"), Some(r#"{"quantity":4}"#)).await;
    assert!(changed.text.contains(r#""total":10.00"#), "{}", changed.text);
    assert_eq!(changed.json()["version"], 3);

    let removed = api.as_user(Method::DELETE, &format!("/orders/{id}/items/{item}"), None).await;
    assert_eq!(removed.json()["items"], json!([]));
    let missing = api.as_user(Method::DELETE, &format!("/orders/{id}/items/{item}"), None).await;
    assert_eq!((missing.status, missing.code()), (StatusCode::NOT_FOUND, "RESOURCE-NOT-FOUND-03".to_owned()));
}

#[tokio::test]
async fn confirming_and_canceling_follow_the_state_machine_and_announce() {
    let api = api();
    let id = api.order(PEN).await["id"].as_str().unwrap().to_owned();

    let confirmed = api.as_user(Method::POST, &format!("/orders/{id}/confirm"), None).await;
    assert_eq!(confirmed.json()["status"], "CONFIRMED");
    let twice = api.as_user(Method::POST, &format!("/orders/{id}/confirm"), None).await;
    assert_eq!((twice.status, twice.code()), (StatusCode::BAD_REQUEST, "VALIDATION-04".to_owned()));
    let frozen = api
        .as_user(
            Method::POST,
            &format!("/orders/{id}/items"),
            Some(r#"{"description":"x","unit_price":1,"quantity":1}"#),
        )
        .await;
    assert_eq!(frozen.code(), "VALIDATION-02");

    let canceled = api.as_user(Method::POST, &format!("/orders/{id}/cancel"), None).await;
    assert_eq!(canceled.json()["status"], "CANCELED");
    let again = api.as_user(Method::POST, &format!("/orders/{id}/cancel"), None).await;
    assert_eq!(again.code(), "VALIDATION-08");

    assert_eq!(api.publisher.events().len(), 2);
}

#[tokio::test]
async fn an_order_can_be_read_and_deleted_by_any_user() {
    let api = api();
    let id = api.order(PEN).await["id"].as_str().unwrap().to_owned();
    let other = token("user-9", &["user"]);
    assert_eq!(api.call(Method::GET, &format!("/orders/{id}"), Some(&other), None).await.status, StatusCode::OK);
    assert_eq!(
        api.call(Method::DELETE, &format!("/orders/{id}"), Some(&other), None).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(api.as_user(Method::GET, &format!("/orders/{id}"), None).await.code(), "RESOURCE-NOT-FOUND-02");
}

#[tokio::test]
async fn a_customer_with_an_order_cannot_be_deleted() {
    let api = api();
    let order = api.order(PEN).await;
    let customer = order["customer_id"].as_str().unwrap();
    let reply = api.as_admin(Method::DELETE, &format!("/customers/{customer}"), None).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::CONFLICT, "CONFLICT-03".to_owned()));
}

// --- search -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_get_search_takes_page_size_and_sort() {
    let api = api();
    api.customer().await;
    let page = api.as_user(Method::GET, "/customers/search?page=0&size=5&sort=-name", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        (page.json()["total"].clone(), page.json()["size"].clone(), page.json()["page"].clone()),
        (json!(1), json!(5), json!(0))
    );
    let defaults = api.as_user(Method::GET, "/orders/search", None).await;
    assert_eq!(defaults.json()["size"], 20);
    let zero = api.as_user(Method::GET, "/orders/search?size=0", None).await;
    assert_eq!(zero.json()["size"], 20, "a size of zero means \"not given\"");
}

#[tokio::test]
async fn a_search_with_bad_numbers_is_validation_01() {
    let api = api();
    for (query, field) in [("size=abc", "size"), ("page=1.5", "page"), ("size=101", "size"), ("page=-1&size=5", "page")]
    {
        let reply = api.as_user(Method::GET, &format!("/customers/search?{query}"), None).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::BAD_REQUEST, "VALIDATION-01".to_owned()), "{query}");
        assert_eq!(reply.json()["params"], json!({"field": field}), "{query}");
    }
}

#[tokio::test]
async fn the_post_search_takes_filters_and_refuses_a_bad_shape() {
    let api = api();
    api.customer().await;
    let ok = api
        .as_user(
            Method::POST,
            "/customers/search",
            Some(r#"{"page":0,"size":10,"filters":{"name":[{"op":"lk","value":"ada"}]}}"#),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK);
    for bad in ["not json", r#"{"filters":[]}"#, r#"{"surprise":1}"#, r#"{"size":"10"}"#] {
        let reply = api.as_user(Method::POST, "/orders/search", Some(bad)).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::BAD_REQUEST, "VALIDATION-01".to_owned()), "{bad}");
        assert_eq!(reply.json()["params"], json!({"field": "body"}));
    }
}

// --- rate limiting ----------------------------------------------------------------------------------------

#[tokio::test]
async fn a_user_over_the_limit_gets_429_with_retry_after_and_recovers() {
    let api = api_with(HttpConfig { rate_limit_per_user: 3, ..HttpConfig::default() });
    for _ in 0..3 {
        assert_eq!(api.as_user(Method::GET, "/orders/search", None).await.status, StatusCode::OK);
    }
    let limited = api.as_user(Method::GET, "/orders/search", None).await;
    assert_eq!((limited.status, limited.code()), (StatusCode::TOO_MANY_REQUESTS, "RATE-00".to_owned()));
    assert_eq!(limited.header("retry-after"), "20");

    let other = api.call(Method::GET, "/orders/search", Some(&api.admin), None).await;
    assert_eq!(other.status, StatusCode::OK, "another user has its own bucket");

    api.millis.fetch_add(20_000, Ordering::SeqCst);
    assert_eq!(api.as_user(Method::GET, "/orders/search", None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn writes_have_their_own_smaller_budget() {
    let api = api_with(HttpConfig { rate_limit_writes: 2, ..HttpConfig::default() });
    let customer = api.customer().await;
    let order = format!(r#"{{"customer_id":"{customer}","items":[]}}"#);
    assert_eq!(api.as_user(Method::POST, "/orders", Some(&order)).await.status, StatusCode::CREATED);
    assert_eq!(api.as_user(Method::POST, "/orders", Some(&order)).await.status, StatusCode::CREATED);
    assert_eq!(api.as_user(Method::POST, "/orders", Some(&order)).await.code(), "RATE-00");
    assert_eq!(api.as_user(Method::GET, "/orders/search", None).await.status, StatusCode::OK, "reads are not writes");
}

#[tokio::test]
async fn the_address_limit_applies_before_authentication_and_spares_health() {
    let api = api_with(HttpConfig { rate_limit_per_ip: 2, ..HttpConfig::default() });
    for _ in 0..2 {
        assert_eq!(api.call(Method::GET, "/orders/search", None, None).await.status, StatusCode::UNAUTHORIZED);
    }
    let limited = api.call(Method::GET, "/orders/search", None, None).await;
    assert_eq!((limited.status, limited.code()), (StatusCode::TOO_MANY_REQUESTS, "RATE-00".to_owned()));
    for _ in 0..10 {
        assert_eq!(api.call(Method::GET, "/health", None, None).await.status, StatusCode::OK);
    }
    let mut elsewhere = Request::get("/orders/search").body(Body::empty()).unwrap();
    elsewhere.extensions_mut().insert(ConnectInfo("198.51.100.7:1".parse::<SocketAddr>().unwrap()));
    assert_eq!(api.send(elsewhere).await.status, StatusCode::UNAUTHORIZED, "another address has its own bucket");
}

// --- CORS -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn cors_answers_only_the_configured_origins() {
    let api = api_with(HttpConfig { cors_origins: vec!["https://app.example".into()], ..HttpConfig::default() });
    let allowed =
        api.send(Request::get("/health").header("origin", "https://app.example").body(Body::empty()).unwrap()).await;
    assert_eq!(allowed.header("access-control-allow-origin"), "https://app.example");
    let denied =
        api.send(Request::get("/health").header("origin", "https://evil.example").body(Body::empty()).unwrap()).await;
    assert_eq!(denied.header("access-control-allow-origin"), "");

    let preflight = api
        .send(
            Request::options("/customers")
                .header("origin", "https://app.example")
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "authorization,content-type")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(preflight.status.is_success());
    assert!(preflight.header("access-control-allow-headers").contains("authorization"));
    assert_eq!(preflight.header("access-control-max-age"), "600");
}

#[tokio::test]
async fn without_configured_origins_cors_is_off() {
    let api = api();
    let reply =
        api.send(Request::get("/health").header("origin", "https://app.example").body(Body::empty()).unwrap()).await;
    assert_eq!(reply.header("access-control-allow-origin"), "");
}

// --- the contract -----------------------------------------------------------------------------------------

fn openapi() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/api/openapi.yaml")).expect("api/openapi.yaml")
}

/// The `METHOD path` pairs and the public ones of the document, read line by line (two-space indented paths, four-space
/// indented methods, `security: []` marking a public operation): it is a fixed layout kept in sync by this test.
fn documented() -> (Vec<String>, Vec<String>) {
    let (mut all, mut public) = (Vec::new(), Vec::new());
    let (mut in_paths, mut path, mut method) = (false, String::new(), None::<String>);
    for line in openapi().lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            in_paths = line.trim_end() == "paths:";
            continue;
        }
        if !in_paths {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let text = line.trim();
        match indent {
            2 if text.starts_with('/') => text.trim_end_matches(':').clone_into(&mut path),
            4 if ["get:", "post:", "put:", "patch:", "delete:"].contains(&text) => {
                let name = text.trim_end_matches(':').to_uppercase();
                all.push(format!("{name} {path}"));
                method = Some(name);
            }
            6 if text == "security: []" => {
                if let Some(name) = &method {
                    public.push(format!("{name} {path}"));
                }
            }
            _ => {}
        }
    }
    (all, public)
}

#[test]
fn the_document_describes_exactly_the_registered_routes() {
    let mut registered: Vec<String> = route_table().iter().map(|(m, p, _)| format!("{m} {p}")).collect();
    let (mut documented, _) = documented();
    registered.sort();
    documented.sort();
    assert_eq!(registered, documented);
}

#[test]
fn the_routes_documented_as_public_are_the_public_ones_and_no_other() {
    let mut registered: Vec<String> =
        route_table().iter().filter(|(_, _, a)| *a == Access::Public).map(|(m, p, _)| format!("{m} {p}")).collect();
    let (_, mut public) = documented();
    registered.sort();
    public.sort();
    assert_eq!(registered, public);
    assert_eq!(registered, ["GET /health"]);
}

#[test]
fn the_document_lists_every_error_code_of_the_catalog() {
    let text = openapi();
    let missing: Vec<_> = codes::ALL.iter().filter(|code| !text.contains(*code)).collect();
    assert!(missing.is_empty(), "codes missing from api/openapi.yaml: {missing:?}");
}

#[test]
fn every_route_has_an_explicit_access() {
    let table = route_table();
    assert_eq!(table.len(), 17);
    let admin: Vec<_> =
        table.iter().filter(|(_, _, a)| *a == Access::Admin).map(|(m, p, _)| format!("{m} {p}")).collect();
    assert_eq!(admin, ["POST /customers", "PUT /customers/{id}", "DELETE /customers/{id}"]);
}
