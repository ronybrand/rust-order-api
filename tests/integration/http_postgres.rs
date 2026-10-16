//! The whole application, from an HTTP request to a real PostgreSQL and back.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use orderapi::app::{CustomerService, OrderService, Store, system_clock};
use orderapi::auth::JwtVerifier;
use orderapi::http::{ApiDependencies, HttpConfig, order_api, system_millis};
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

use crate::common::{AUDIENCE, ISSUER, TestDb, keys, token};

fn app(db: &TestDb) -> Router {
    let store: Arc<dyn Store> = Arc::new(db.store.clone());
    order_api(ApiDependencies {
        customers: Arc::new(CustomerService::new(Arc::clone(&store), system_clock())),
        orders: Arc::new(OrderService::new(Arc::clone(&store), None, system_clock())),
        verifier: Arc::new(JwtVerifier::from_pem(keys().public_pem.as_bytes(), AUDIENCE, ISSUER).unwrap()),
        store,
        config: HttpConfig {
            rate_limit_writes: 10_000,
            rate_limit_per_user: 10_000,
            rate_limit_per_ip: 10_000,
            ..HttpConfig::default()
        },
        now_millis: system_millis(),
    })
}

async fn call(
    app: &Router,
    method: Method,
    path: &str,
    bearer: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let mut request = request.body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned()))).unwrap();
    request.extensions_mut().insert(ConnectInfo::<SocketAddr>("203.0.113.9:4000".parse().unwrap()));
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

macro_rules! database {
    ($name:ident) => {
        let Some($name) = TestDb::new(module_path!()).await else { return };
    };
}

const ADA: &str = r#"{"name":"Ada Lovelace","tax_id":"123.456-78","email":"ada@example.com"}"#;

#[tokio::test]
async fn health_reports_the_real_database() {
    database!(db);
    let (status, body) = call(&app(&db), Method::GET, "/health", None, None).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"status": "ok"})));
    db.finish().await;
}

#[tokio::test]
async fn health_is_unavailable_when_the_database_is_gone() {
    database!(db);
    let app = app(&db);
    db.pool.close().await;
    let (status, body) = call(&app, Method::GET, "/health", None, None).await;
    assert_eq!((status, body), (StatusCode::SERVICE_UNAVAILABLE, json!({"status": "unavailable"})));
    db.finish().await;
}

#[tokio::test]
async fn a_customer_and_an_order_go_through_the_whole_lifecycle_on_a_real_database() {
    database!(db);
    let app = app(&db);
    let (admin, user) = (token("admin-1", &["user", "admin"]), token("user-1", &["user"]));

    let (status, customer) = call(&app, Method::POST, "/customers", Some(&admin), Some(ADA)).await;
    assert_eq!(status, StatusCode::CREATED);
    let customer_id = customer["id"].as_str().unwrap().to_owned();

    let body = format!(
        r#"{{"customer_id":"{customer_id}","items":[{{"description":"Pen","unit_price":0.10,"quantity":3}},{{"description":"Book","unit_price":19.99,"quantity":2}}]}}"#
    );
    let (status, order) = call(&app, Method::POST, "/orders", Some(&user), Some(&body)).await;
    assert_eq!((status, order["total"].to_string()), (StatusCode::CREATED, "40.28".to_owned()));
    let order_id = order["id"].as_str().unwrap();

    let (_, read) = call(&app, Method::GET, &format!("/orders/{order_id}"), Some(&user), None).await;
    assert_eq!(read, order);

    let item = order["items"][0]["id"].as_str().unwrap();
    let (_, changed) =
        call(&app, Method::PATCH, &format!("/orders/{order_id}/items/{item}"), Some(&user), Some(r#"{"quantity":10}"#))
            .await;
    assert_eq!((changed["total"].to_string(), changed["version"].clone()), ("40.98".to_owned(), json!(2)));

    let (_, confirmed) = call(&app, Method::POST, &format!("/orders/{order_id}/confirm"), Some(&user), None).await;
    assert_eq!(confirmed["status"], "CONFIRMED");

    // The customer has an order, so it cannot be deleted; once the order is, it can.
    let (status, refused) = call(&app, Method::DELETE, &format!("/customers/{customer_id}"), Some(&admin), None).await;
    assert_eq!((status, refused["code"].clone()), (StatusCode::CONFLICT, json!("CONFLICT-03")));
    let (status, _) = call(&app, Method::DELETE, &format!("/orders/{order_id}"), Some(&user), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, Method::DELETE, &format!("/customers/{customer_id}"), Some(&admin), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, gone) = call(&app, Method::GET, &format!("/customers/{customer_id}"), Some(&user), None).await;
    assert_eq!((status, gone["code"].clone()), (StatusCode::NOT_FOUND, json!("RESOURCE-NOT-FOUND-01")));
    db.finish().await;
}

#[tokio::test]
async fn a_duplicate_tax_id_is_conflict_01_from_the_unique_index_and_the_value_is_not_echoed() {
    database!(db);
    let app = app(&db);
    let admin = token("admin-1", &["user", "admin"]);
    call(&app, Method::POST, "/customers", Some(&admin), Some(ADA)).await;

    // The other spelling of the same document: only the canonical form makes the two collide.
    let again = r#"{"name":"Other","tax_id":"12345678","email":"other@example.com"}"#;
    let (status, body) = call(&app, Method::POST, "/customers", Some(&admin), Some(again)).await;
    assert_eq!((status, body["code"].clone()), (StatusCode::CONFLICT, json!("CONFLICT-01")));
    assert!(!body.to_string().contains("12345678"));
    db.finish().await;
}

#[tokio::test]
async fn the_audit_columns_record_who_did_what() {
    database!(db);
    let app = app(&db);
    let (admin, user) = (token("admin-7", &["user", "admin"]), token("user-9", &["user"]));
    let (_, customer) = call(&app, Method::POST, "/customers", Some(&admin), Some(ADA)).await;
    let id = customer["id"].as_str().unwrap();
    let order =
        call(&app, Method::POST, "/orders", Some(&user), Some(&format!(r#"{{"customer_id":"{id}","items":[]}}"#)))
            .await
            .1;
    let order_id = order["id"].as_str().unwrap();
    call(&app, Method::POST, &format!("/orders/{order_id}/cancel"), Some(&token("user-10", &["user"])), None).await;

    let row = sqlx::query("SELECT created_by, updated_by FROM customers WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!((row.get::<String, _>(0), row.get::<String, _>(1)), ("admin-7".to_owned(), "admin-7".to_owned()));
    let row = sqlx::query("SELECT created_by, updated_by FROM orders WHERE id = $1::uuid")
        .bind(order_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!((row.get::<String, _>(0), row.get::<String, _>(1)), ("user-9".to_owned(), "user-10".to_owned()));
    db.finish().await;
}

#[tokio::test]
async fn concurrent_writes_to_one_order_never_overwrite_each_other() {
    database!(db);
    let app = app(&db);
    let (admin, user) = (token("admin-1", &["user", "admin"]), token("user-1", &["user"]));
    let (_, customer) = call(&app, Method::POST, "/customers", Some(&admin), Some(ADA)).await;
    let body = format!(r#"{{"customer_id":"{}","items":[]}}"#, customer["id"].as_str().unwrap());
    let (_, order) = call(&app, Method::POST, "/orders", Some(&user), Some(&body)).await;
    let order_id = order["id"].as_str().unwrap().to_owned();

    let attempts = 12;
    let mut tasks = Vec::new();
    for index in 0..attempts {
        let (app, user, order_id) = (app.clone(), user.clone(), order_id.clone());
        tasks.push(tokio::spawn(async move {
            let item = format!(r#"{{"description":"Item {index}","unit_price":1.00,"quantity":1}}"#);
            call(&app, Method::POST, &format!("/orders/{order_id}/items"), Some(&user), Some(&item)).await
        }));
    }
    let mut accepted = 0;
    for task in tasks {
        let (status, body) = task.await.unwrap();
        match status {
            StatusCode::OK => accepted += 1,
            StatusCode::CONFLICT => assert_eq!(body["code"], "CONFLICT-00"),
            other => panic!("unexpected {other}: {body}"),
        }
    }
    assert!(accepted >= 1);

    // Each accepted write is in the order, once, and the version counts them: nothing was lost or overwritten.
    let (_, read) = call(&app, Method::GET, &format!("/orders/{order_id}"), Some(&user), None).await;
    assert_eq!(read["items"].as_array().unwrap().len(), accepted);
    assert_eq!(read["version"], 1 + accepted);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM order_items WHERE order_id = $1::uuid")
        .bind(&order_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(stored, i64::try_from(accepted).unwrap());
    Uuid::parse_str(&order_id).unwrap();
    db.finish().await;
}

#[tokio::test]
async fn the_post_search_filters_on_the_real_database_and_a_bad_filter_is_a_400() {
    database!(db);
    let app = app(&db);
    let (admin, user) = (token("admin-1", &["user", "admin"]), token("user-1", &["user"]));
    for (name, tax_id) in [("Ada Lovelace", "11111111"), ("Bob Stone", "22222222"), ("Cy Young", "33333333")] {
        let body = format!(r#"{{"name":"{name}","tax_id":"{tax_id}","email":"x@example.com"}}"#);
        call(&app, Method::POST, "/customers", Some(&admin), Some(&body)).await;
    }
    let (status, page) = call(
        &app,
        Method::POST,
        "/customers/search",
        Some(&user),
        Some(r#"{"size":10,"sort":"name","filters":{"name":[{"op":"lk","value":"o"}]}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = page["items"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!((names, page["total"].clone()), (vec!["Ada Lovelace", "Bob Stone", "Cy Young"], json!(3)));

    let (status, error) = call(
        &app,
        Method::POST,
        "/customers/search",
        Some(&user),
        Some(r#"{"filters":{"version":[{"op":"gte","value":"a lot"}]}}"#),
    )
    .await;
    assert_eq!((status, error["code"].clone()), (StatusCode::BAD_REQUEST, json!("VALIDATION-05")));
    let (status, error) = call(&app, Method::GET, "/customers/search?sort=password", Some(&user), None).await;
    assert_eq!((status, error["code"].clone()), (StatusCode::BAD_REQUEST, json!("VALIDATION-06")));
    db.finish().await;
}
