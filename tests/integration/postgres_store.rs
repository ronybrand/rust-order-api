//! The PostgreSQL store against a real server: every statement, the constraints, the locks and the search.

use std::sync::Arc;

use orderapi::app::search::Condition;
use orderapi::app::{Actor, CustomerService, OrderService, SearchRequest, Session, Store, system_clock};
use orderapi::domain::{Customer, CustomerInput, ItemInput, Order, OrderStatus};
use orderapi::error::Error;
use orderapi::persistence::plan::{CUSTOMERS, ORDERS};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::common::TestDb;

/// `PostgreSQL` keeps microseconds; a timestamp with more would not compare equal after a round trip.
fn now() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_nanosecond(now.nanosecond() / 1000 * 1000).unwrap()
}

fn customer(name: &str, tax_id: &str, passport: Option<&str>) -> Customer {
    Customer::create(
        CustomerInput {
            name: name.into(),
            tax_id: tax_id.into(),
            passport_number: passport.map(str::to_owned),
            email: format!(
                "{}@example.com",
                name.to_lowercase().chars().filter(char::is_ascii_alphanumeric).collect::<String>()
            ),
        },
        "admin-1",
        now(),
    )
    .unwrap()
}

fn item(description: &str, price: &str, quantity: i32) -> ItemInput {
    ItemInput { description: description.into(), unit_price: Decimal::from_str_exact(price).unwrap(), quantity }
}

fn order_for(customer: &Customer, items: Vec<ItemInput>) -> Order {
    Order::create(customer.id, items, "user-1", now()).unwrap()
}

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> &'static str {
    result.unwrap_err().code().expect("an application error")
}

async fn session(db: &TestDb) -> Box<dyn Session> {
    db.store.session().await.unwrap()
}

async fn saved_customer(db: &TestDb, name: &str, tax_id: &str) -> Customer {
    let customer = customer(name, tax_id, None);
    session(db).await.customer_create(&customer).await.unwrap();
    customer
}

fn search(filters: Value, sort: &str) -> SearchRequest {
    let filters = filters
        .as_object()
        .unwrap()
        .iter()
        .map(|(field, conditions)| {
            let conditions = conditions
                .as_array()
                .unwrap()
                .iter()
                .map(|c| Condition { op: c["op"].as_str().unwrap_or("").into(), value: c.get("value").cloned() })
                .collect();
            (field.clone(), conditions)
        })
        .collect();
    SearchRequest { page: 0, size: 100, sort: sort.into(), filters }
}

macro_rules! database {
    ($name:ident) => {
        let Some($name) = TestDb::new(module_path!()).await else { return };
    };
}

// --- customers --------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_customer_round_trips_with_its_audit_fields() {
    database!(db);
    let stored = customer("Ada Lovelace", "123.456-78", Some("AB123456"));
    let mut conn = session(&db).await;
    conn.customer_create(&stored).await.unwrap();

    let read = conn.customer_get(stored.id).await.unwrap();
    assert_eq!(
        (read.name.as_str(), read.tax_id.as_str(), read.passport_number.as_deref(), read.email.as_str()),
        ("Ada Lovelace", "12345678", Some("AB123456"), "adalovelace@example.com")
    );
    assert_eq!((read.version, read.created_by.as_str(), read.updated_by.as_str()), (1, "admin-1", "admin-1"));
    assert_eq!((read.created_at, read.updated_at), (stored.created_at, stored.updated_at));
    assert!(read.deleted_at.is_none() && read.deleted_by.is_none());
    db.finish().await;
}

#[tokio::test]
async fn an_unknown_customer_is_resource_not_found_01() {
    database!(db);
    assert_eq!(code(session(&db).await.customer_get(Uuid::new_v4()).await), "RESOURCE-NOT-FOUND-01");
    db.finish().await;
}

#[tokio::test]
async fn uniqueness_is_enforced_by_the_database_even_when_the_application_check_is_bypassed() {
    database!(db);
    let mut conn = session(&db).await;
    conn.customer_create(&customer("Ada", "11111111", Some("AB123456"))).await.unwrap();

    let same_tax_id = conn.customer_create(&customer("Bob", "11111111", None)).await.unwrap_err();
    assert_eq!(same_tax_id.code(), Some("CONFLICT-01"));
    assert!(!same_tax_id.to_string().contains("11111111"), "the value is not echoed");
    assert_eq!(code(conn.customer_create(&customer("Cy", "22222222", Some("AB123456"))).await), "CONFLICT-02");
    db.finish().await;
}

#[tokio::test]
async fn a_soft_deleted_customer_is_hidden_and_releases_its_tax_id_and_passport() {
    database!(db);
    let mut conn = session(&db).await;
    let ada = customer("Ada", "11111111", Some("AB123456"));
    conn.customer_create(&ada).await.unwrap();

    conn.customer_soft_delete(ada.id, "admin-2", now()).await.unwrap();

    assert_eq!(code(conn.customer_get(ada.id).await), "RESOURCE-NOT-FOUND-01");
    assert_eq!(code(conn.customer_soft_delete(ada.id, "admin-2", now()).await), "RESOURCE-NOT-FOUND-01");
    assert!(!conn.tax_id_taken("11111111", None).await.unwrap());
    conn.customer_create(&customer("Ada again", "11111111", Some("AB123456"))).await.unwrap();
    let row = sqlx::query("SELECT deleted_by, version FROM customers WHERE id = $1")
        .bind(ada.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!((row.get::<String, _>("deleted_by"), row.get::<i64, _>("version")), ("admin-2".to_owned(), 2));
    db.finish().await;
}

#[tokio::test]
async fn the_uniqueness_checks_can_ignore_the_customer_itself() {
    database!(db);
    let mut conn = session(&db).await;
    let ada = customer("Ada", "11111111", Some("AB123456"));
    conn.customer_create(&ada).await.unwrap();
    assert!(conn.tax_id_taken("11111111", None).await.unwrap());
    assert!(!conn.tax_id_taken("11111111", Some(ada.id)).await.unwrap());
    assert!(conn.tax_id_taken("11111111", Some(Uuid::new_v4())).await.unwrap());
    assert!(conn.passport_taken("AB123456", None).await.unwrap());
    assert!(!conn.passport_taken("AB123456", Some(ada.id)).await.unwrap());
    assert!(!conn.passport_taken("ZZ999999", None).await.unwrap());
    db.finish().await;
}

#[tokio::test]
async fn an_update_checks_the_version_and_returns_the_next_one() {
    database!(db);
    let mut conn = session(&db).await;
    let ada = customer("Ada", "11111111", None);
    conn.customer_create(&ada).await.unwrap();

    let renamed = ada
        .clone()
        .replace(
            CustomerInput {
                name: "Ada King".into(),
                tax_id: "11111111".into(),
                passport_number: None,
                email: ada.email.clone(),
            },
            "admin-2",
            now(),
        )
        .unwrap();
    let saved = conn.customer_update(&renamed).await.unwrap();
    assert_eq!(saved.version, 2);
    let read = conn.customer_get(ada.id).await.unwrap();
    assert_eq!(
        (read.name.as_str(), read.version, read.updated_by.as_str(), read.created_by.as_str()),
        ("Ada King", 2, "admin-2", "admin-1")
    );

    // The same write again carries the old version: somebody else got there first.
    assert_eq!(code(conn.customer_update(&renamed).await), "CONFLICT-00");
    db.finish().await;
}

// --- orders -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_order_round_trips_with_its_items_in_insertion_order_and_an_exact_total() {
    database!(db);
    let ada = saved_customer(&db, "Ada", "11111111").await;
    let order = order_for(&ada, vec![item("Pen", "0.10", 3), item("Book", "19.99", 2), item("Cap", "1.00", 1)]);
    let mut conn = session(&db).await;
    conn.order_create(&order).await.unwrap();

    let read = conn.order_get(order.id).await.unwrap();
    assert_eq!(read, order);
    assert_eq!(read.items.iter().map(|i| i.description.as_str()).collect::<Vec<_>>(), ["Pen", "Book", "Cap"]);
    assert_eq!(read.total().to_string(), "41.28");
    db.finish().await;
}

#[tokio::test]
async fn an_unknown_order_is_resource_not_found_02() {
    database!(db);
    assert_eq!(code(session(&db).await.order_get(Uuid::new_v4()).await), "RESOURCE-NOT-FOUND-02");
    db.finish().await;
}

#[tokio::test]
async fn save_applies_quantity_changes_removals_and_additions_in_one_go() {
    database!(db);
    let ada = saved_customer(&db, "Ada", "11111111").await;
    let order = order_for(&ada, vec![item("Pen", "1.00", 1), item("Book", "10.00", 1), item("Cap", "2.00", 1)]);
    let mut conn = session(&db).await;
    conn.order_create(&order).await.unwrap();
    let (pen, book, cap) = (order.items[0].id, order.items[1].id, order.items[2].id);

    let changed = order
        .update_item_quantity(pen, 5, "user-2", now())
        .unwrap()
        .remove_item(book, "user-2", now())
        .unwrap()
        .add_item(item("Mug", "7.50", 2), "user-2", now())
        .unwrap();
    let saved = conn.order_save(&changed).await.unwrap();
    assert_eq!(saved.version, 2);

    let read = conn.order_get(changed.id).await.unwrap();
    let lines: Vec<_> = read.items.iter().map(|i| (i.description.as_str(), i.quantity)).collect();
    assert_eq!(lines, [("Pen", 5), ("Cap", 1), ("Mug", 2)]);
    assert_eq!(read.items[1].id, cap, "an untouched item keeps its row");
    assert_eq!((read.version, read.updated_by.as_str()), (2, "user-2"));
    db.finish().await;
}

#[tokio::test]
async fn save_rejects_a_stale_version_and_applies_nothing() {
    database!(db);
    let ada = saved_customer(&db, "Ada", "11111111").await;
    let order = order_for(&ada, vec![item("Pen", "1.00", 1)]);
    let mut conn = session(&db).await;
    conn.order_create(&order).await.unwrap();
    conn.order_save(&order.clone().add_item(item("First", "1.00", 1), "a", now()).unwrap()).await.unwrap();

    let stale = order.add_item(item("Second", "1.00", 1), "b", now()).unwrap();
    assert_eq!(code(conn.order_save(&stale).await), "CONFLICT-00");
    let read = conn.order_get(stale.id).await.unwrap();
    assert_eq!(read.items.len(), 2, "the stale write changed no item");
    assert!(read.items.iter().all(|i| i.description != "Second"));
    db.finish().await;
}

#[tokio::test]
async fn an_order_for_a_customer_that_does_not_exist_is_a_data_integrity_conflict() {
    database!(db);
    let ghost = customer("Ghost", "99999999", None);
    let order = order_for(&ghost, vec![item("Pen", "1.00", 1)]);
    let mut tx = db.store.begin().await.unwrap();
    assert_eq!(code(tx.order_create(&order).await), "CONFLICT-04");
    db.finish().await;
}

#[tokio::test]
async fn check_constraints_back_up_the_domain_rules() {
    database!(db);
    let ada = saved_customer(&db, "Ada", "11111111").await;
    let order = order_for(&ada, vec![item("Pen", "1.00", 1)]);
    session(&db).await.order_create(&order).await.unwrap();
    for statement in [
        "UPDATE order_items SET quantity = 0",
        "UPDATE order_items SET quantity = 10001",
        "UPDATE order_items SET unit_price = 0",
        "UPDATE order_items SET description = '   '",
        "UPDATE orders SET status = 'SHIPPED'",
        "UPDATE customers SET name = '  '",
        "UPDATE customers SET deleted_at = now()",
        "UPDATE customers SET tax_id = repeat('1', 21)",
    ] {
        let refused = sqlx::query(statement).execute(&db.pool).await.is_err();
        assert!(refused, "the database accepted: {statement}");
    }
    db.finish().await;
}

#[tokio::test]
async fn has_active_orders_ignores_deleted_orders() {
    database!(db);
    let ada = saved_customer(&db, "Ada", "11111111").await;
    let mut conn = session(&db).await;
    assert!(!conn.has_active_orders(ada.id).await.unwrap());
    let order = order_for(&ada, vec![]);
    conn.order_create(&order).await.unwrap();
    assert!(conn.has_active_orders(ada.id).await.unwrap());
    conn.order_soft_delete(order.id, "user-1", now()).await.unwrap();
    assert!(!conn.has_active_orders(ada.id).await.unwrap());
    assert_eq!(code(conn.order_get(order.id).await), "RESOURCE-NOT-FOUND-02");
    assert_eq!(code(conn.order_soft_delete(order.id, "user-1", now()).await), "RESOURCE-NOT-FOUND-02");
    db.finish().await;
}

// --- search -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_order_search_filters_by_status_sorts_and_pages_loading_the_items() {
    database!(db);
    let ada = saved_customer(&db, "Ada", "11111111").await;
    let mut conn = session(&db).await;
    let mut ids = Vec::new();
    for index in 0..5 {
        let order = order_for(&ada, vec![item(&format!("Item {index}"), "1.00", index + 1)]);
        conn.order_create(&order).await.unwrap();
        ids.push(order.id);
    }
    let confirmed = conn.order_get(ids[1]).await.unwrap().confirm("admin", now()).unwrap().order;
    conn.order_save(&confirmed).await.unwrap();

    let only_confirmed =
        conn.order_search(&search(json!({"status": [{"op": "eq", "value": "CONFIRMED"}]}), "")).await.unwrap();
    assert_eq!((only_confirmed.total, only_confirmed.items.len()), (1, 1));
    assert_eq!(only_confirmed.items[0].items[0].quantity, 2, "the items come with the order");

    let by_version = conn
        .order_search(&SearchRequest { page: 1, size: 2, sort: "-version".into(), ..SearchRequest::default() })
        .await
        .unwrap();
    assert_eq!((by_version.total, by_version.items.len(), by_version.page, by_version.size), (5, 2, 1, 2));
    assert!(by_version.items.iter().all(|o| o.status == OrderStatus::Open && !o.items.is_empty()));

    let beyond = conn.order_search(&SearchRequest { page: 9, size: 2, ..SearchRequest::default() }).await.unwrap();
    assert!(beyond.items.is_empty() && beyond.total == 5);
    db.finish().await;
}

#[tokio::test]
async fn every_operator_runs_against_the_real_database() {
    database!(db);
    let mut conn = session(&db).await;
    for (name, tax_id) in [("Ada", "11111111"), ("Bob", "22222222"), ("Cy", "33333333")] {
        conn.customer_create(&customer(name, tax_id, None)).await.unwrap();
    }
    let names = |page: orderapi::app::Page<Customer>| {
        let mut names: Vec<_> = page.items.into_iter().map(|c| c.name).collect();
        names.sort();
        names
    };
    let run = |filters: Value| search(filters, "name");

    let cases: Vec<(Value, Vec<&str>)> = vec![
        (json!({"name": [{"op": "eq", "value": "Bob"}]}), vec!["Bob"]),
        (json!({"name": [{"value": "Bob"}]}), vec!["Bob"]),
        (json!({"name": [{"op": "neq", "value": "Bob"}]}), vec!["Ada", "Cy"]),
        (json!({"name": [{"op": "gt", "value": "Ada"}]}), vec!["Bob", "Cy"]),
        (json!({"name": [{"op": "gte", "value": "Bob"}]}), vec!["Bob", "Cy"]),
        (json!({"name": [{"op": "lt", "value": "Bob"}]}), vec!["Ada"]),
        (json!({"name": [{"op": "lte", "value": "Bob"}]}), vec!["Ada", "Bob"]),
        (json!({"name": [{"op": "in", "value": ["Ada", "Cy", "Nobody"]}]}), vec!["Ada", "Cy"]),
        (json!({"name": [{"op": "between", "value": ["Ada", "Bob"]}]}), vec!["Ada", "Bob"]),
        (json!({"tax_id": [{"op": "lk", "value": "222"}]}), vec!["Bob"]),
        (json!({"version": [{"op": "eq", "value": 1}]}), vec!["Ada", "Bob", "Cy"]),
        (json!({"version": [{"op": "gt", "value": 1}]}), vec![]),
        (json!({"name": [{"op": "gte", "value": "Ada"}, {"op": "lt", "value": "Cy"}]}), vec!["Ada", "Bob"]),
        (json!({"created_at": [{"op": "lte", "value": "2999-01-01"}]}), vec!["Ada", "Bob", "Cy"]),
        (json!({"created_at": [{"op": "gt", "value": "2999-01-01T00:00:00Z"}]}), vec![]),
        (json!({"unknown_field": [{"op": "eq", "value": "x"}]}), vec!["Ada", "Bob", "Cy"]),
    ];
    for (filters, expected) in cases {
        let found = names(conn.customer_search(&run(filters.clone())).await.unwrap());
        assert_eq!(found, expected, "{filters}");
    }
    db.finish().await;
}

#[tokio::test]
async fn lk_is_a_contains_match_that_ignores_case_and_escapes_wildcards() {
    database!(db);
    let mut conn = session(&db).await;
    for (name, tax_id) in [
        ("50% off", "11111111"),
        ("500 off", "22222222"),
        ("a_b", "33333333"),
        ("axb", "44444444"),
        ("Back\\slash", "55555555"),
    ] {
        conn.customer_create(&customer(name, tax_id, None)).await.unwrap();
    }
    let find = |needle: &'static str| search(json!({"name": [{"op": "lk", "value": needle}]}), "name");
    let found = |page: orderapi::app::Page<Customer>| page.items.into_iter().map(|c| c.name).collect::<Vec<_>>();

    assert_eq!(found(conn.customer_search(&find("50%")).await.unwrap()), ["50% off"]);
    assert_eq!(found(conn.customer_search(&find("a_b")).await.unwrap()), ["a_b"]);
    assert_eq!(found(conn.customer_search(&find("K\\S")).await.unwrap()), ["Back\\slash"]);
    assert_eq!(found(conn.customer_search(&find("OFF")).await.unwrap()).len(), 2, "case-insensitive contains");
    db.finish().await;
}

#[tokio::test]
async fn sorting_by_every_searchable_field_works() {
    database!(db);
    let ada = saved_customer(&db, "Ada", "11111111").await;
    let mut conn = session(&db).await;
    conn.order_create(&order_for(&ada, vec![item("Pen", "1.00", 1)])).await.unwrap();
    for field in CUSTOMERS.field_names() {
        for sort in [field.to_owned(), format!("-{field}")] {
            let request = SearchRequest { sort: sort.clone(), size: 10, ..SearchRequest::default() };
            assert!(conn.customer_search(&request).await.is_ok(), "customers sorted by {sort}");
        }
    }
    for field in ORDERS.field_names() {
        for sort in [field.to_owned(), format!("-{field}")] {
            let request = SearchRequest { sort: sort.clone(), size: 10, ..SearchRequest::default() };
            assert!(conn.order_search(&request).await.is_ok(), "orders sorted by {sort}");
        }
    }
    db.finish().await;
}

#[tokio::test]
async fn an_unknown_sort_field_and_a_bad_filter_are_client_errors_not_driver_errors() {
    database!(db);
    let mut conn = session(&db).await;
    assert_eq!(
        code(
            conn.customer_search(&SearchRequest { sort: "deleted_at".into(), size: 5, ..SearchRequest::default() })
                .await
        ),
        "VALIDATION-06"
    );
    assert_eq!(
        code(conn.customer_search(&search(json!({"version": [{"op": "eq", "value": "many"}]}), "")).await),
        "VALIDATION-05"
    );
    db.finish().await;
}

#[tokio::test]
async fn the_search_whitelist_covers_every_column_of_the_real_schema_except_the_soft_delete_markers() {
    database!(db);
    for (table, entity) in [("customers", &CUSTOMERS), ("orders", &ORDERS)] {
        let rows = sqlx::query("SELECT column_name FROM information_schema.columns WHERE table_name = $1")
            .bind(table)
            .fetch_all(&db.pool)
            .await
            .unwrap();
        let mut columns: Vec<String> = rows.iter().map(|r| r.get::<String, _>("column_name")).collect();
        columns.retain(|c| c != "deleted_at" && c != "deleted_by");
        columns.sort();
        assert_eq!(columns, entity.field_names(), "{table}");
    }
    db.finish().await;
}

// --- transactions and locks -------------------------------------------------------------------------------

#[tokio::test]
async fn a_transaction_rolls_back_when_it_is_dropped_and_persists_when_it_is_committed() {
    database!(db);
    let (dropped, committed) = (customer("Dropped", "11111111", None), customer("Committed", "22222222", None));

    let mut tx = db.store.begin().await.unwrap();
    tx.customer_create(&dropped).await.unwrap();
    assert!(tx.customer_get(dropped.id).await.is_ok(), "visible inside the transaction");
    drop(tx);
    assert_eq!(code(session(&db).await.customer_get(dropped.id).await), "RESOURCE-NOT-FOUND-01");

    let mut tx = db.store.begin().await.unwrap();
    tx.customer_create(&committed).await.unwrap();
    tx.commit().await.unwrap();
    assert!(session(&db).await.customer_get(committed.id).await.is_ok());
    db.finish().await;
}

#[tokio::test]
async fn deleting_a_customer_while_an_order_is_created_never_leaves_an_orphan() {
    database!(db);
    let store: Arc<dyn Store> = Arc::new(db.store.clone());
    let customers = Arc::new(CustomerService::new(Arc::clone(&store), system_clock()));
    let orders = Arc::new(OrderService::new(Arc::clone(&store), None, system_clock()));
    let (admin, user) = (Actor::new("admin-1", ["user", "admin"]), Actor::new("user-1", ["user"]));

    for round in 0..20 {
        let ada = customers
            .create(
                &admin,
                CustomerInput {
                    name: "Ada".into(),
                    tax_id: format!("RACE-{round:04}"),
                    passport_number: None,
                    email: "ada@example.com".into(),
                },
            )
            .await
            .unwrap();
        let delete = tokio::spawn({
            let (customers, admin) = (Arc::clone(&customers), admin.clone());
            async move { customers.delete(&admin, ada.id).await }
        });
        let create = tokio::spawn({
            let (orders, user) = (Arc::clone(&orders), user.clone());
            async move { orders.create(&user, ada.id, vec![item("Pen", "1.00", 1)]).await }
        });
        let (deleted, created) = (delete.await.unwrap(), create.await.unwrap());

        match (deleted, created) {
            // The order won the lock: the delete saw it and refused.
            (Err(refused), Ok(_)) => assert_eq!(refused.code(), Some("CONFLICT-03"), "round {round}"),
            // The delete won: the order found no customer.
            (Ok(()), Err(refused)) => assert_eq!(refused.code(), Some("VALIDATION-07"), "round {round}"),
            (deleted, created) => panic!("round {round}: delete {deleted:?}, create {created:?}"),
        }
        let orphans: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM orders o JOIN customers c ON c.id = o.customer_id \
             WHERE o.deleted_at IS NULL AND c.deleted_at IS NOT NULL",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(orphans, 0, "round {round}");
    }
    db.finish().await;
}

#[tokio::test]
async fn migrating_twice_is_harmless_and_leaves_one_applied_migration() {
    database!(db);
    orderapi::persistence::migrate(&db.pool).await.unwrap();
    orderapi::persistence::migrate(&db.pool).await.unwrap();
    let applied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success").fetch_one(&db.pool).await.unwrap();
    assert_eq!(applied, 1);
    db.finish().await;
}

#[tokio::test]
async fn the_store_answers_a_ping() {
    database!(db);
    assert!(db.store.ping().await.is_ok());
    db.finish().await;
}
