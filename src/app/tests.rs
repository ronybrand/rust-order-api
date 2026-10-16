use std::sync::Arc;

use rust_decimal::Decimal;
use time::OffsetDateTime;
use uuid::Uuid;

use super::fake::{MemoryStore, RecordingPublisher};
use super::search::SearchRequest;
use super::{Actor, Clock, CustomerService, OrderService};
use crate::domain::{CustomerInput, ItemInput, OrderStatus};
use crate::error::{AppError, Error, codes};

fn instant() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn clock() -> Clock {
    Arc::new(instant)
}

fn admin() -> Actor {
    Actor::new("admin-1", ["user", "admin"])
}

fn user() -> Actor {
    Actor::new("user-1", ["user"])
}

fn customer_input(name: &str, tax_id: &str) -> CustomerInput {
    CustomerInput { name: name.into(), tax_id: tax_id.into(), passport_number: None, email: "ada@example.com".into() }
}

fn item(description: &str, price: &str, quantity: i32) -> ItemInput {
    ItemInput { description: description.into(), unit_price: Decimal::from_str_exact(price).unwrap(), quantity }
}

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> &'static str {
    result.unwrap_err().code().expect("an application error")
}

struct World {
    store: MemoryStore,
    publisher: RecordingPublisher,
    customers: CustomerService,
    orders: OrderService,
}

fn world() -> World {
    let store = MemoryStore::new();
    let publisher = RecordingPublisher::default();
    World {
        customers: CustomerService::new(Arc::new(store.clone()), clock()),
        orders: OrderService::new(Arc::new(store.clone()), Some(Arc::new(publisher.clone())), clock()),
        store,
        publisher,
    }
}

// --- customers -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_customer_stores_it_with_the_actor_in_the_audit() {
    let w = world();
    let customer = w.customers.create(&admin(), customer_input("Ada", "123.456-78")).await.unwrap();
    assert_eq!(customer.tax_id, "12345678");
    assert_eq!((customer.created_by.as_str(), customer.created_at), ("admin-1", instant()));
    assert_eq!(w.customers.get(customer.id).await.unwrap().name, "Ada");
}

#[tokio::test]
async fn an_invalid_customer_is_never_stored() {
    let w = world();
    assert_eq!(code(w.customers.create(&admin(), customer_input("  ", "12345678")).await), "VALIDATION-01");
    assert!(w.store.data().customers.is_empty());
}

#[tokio::test]
async fn a_tax_id_is_unique_whatever_its_punctuation() {
    let w = world();
    w.customers.create(&admin(), customer_input("Ada", "123.456-78")).await.unwrap();
    let error = w.customers.create(&admin(), customer_input("Other", "12345678")).await.unwrap_err();
    assert_eq!(error.code(), Some("CONFLICT-01"));
    assert!(!error.to_string().contains("12345678"), "the conflict must not echo the document");
}

#[tokio::test]
async fn a_passport_is_unique_among_active_customers() {
    let w = world();
    let with_passport =
        |tax_id: &str| CustomerInput { passport_number: Some("AB123456".into()), ..customer_input("C", tax_id) };
    w.customers.create(&admin(), with_passport("11111111")).await.unwrap();
    assert_eq!(code(w.customers.create(&admin(), with_passport("22222222")).await), "CONFLICT-02");
}

#[tokio::test]
async fn updating_replaces_the_data_and_moves_the_version() {
    let w = world();
    let created = w.customers.create(&admin(), customer_input("Ada", "11111111")).await.unwrap();
    let updated = w.customers.update(&admin(), created.id, customer_input("Ada King", "11111111")).await.unwrap();
    assert_eq!((updated.name.as_str(), updated.version), ("Ada King", 2));
    assert_eq!(w.customers.get(created.id).await.unwrap().version, 2);
}

#[tokio::test]
async fn a_customer_keeps_its_own_tax_id_on_update_but_cannot_take_anothers() {
    let w = world();
    let first = w.customers.create(&admin(), customer_input("A", "11111111")).await.unwrap();
    let second = w.customers.create(&admin(), customer_input("B", "22222222")).await.unwrap();
    assert!(w.customers.update(&admin(), first.id, customer_input("A2", "11111111")).await.is_ok());
    assert_eq!(code(w.customers.update(&admin(), second.id, customer_input("B", "11111111")).await), "CONFLICT-01");
}

#[tokio::test]
async fn updating_or_reading_a_missing_customer_is_a_404() {
    let w = world();
    let missing = Uuid::new_v4();
    assert_eq!(code(w.customers.get(missing).await), "RESOURCE-NOT-FOUND-01");
    assert_eq!(
        code(w.customers.update(&admin(), missing, customer_input("A", "11111111")).await),
        "RESOURCE-NOT-FOUND-01"
    );
    assert_eq!(code(w.customers.delete(&admin(), missing).await), "RESOURCE-NOT-FOUND-01");
}

#[tokio::test]
async fn a_stale_update_is_a_concurrent_modification() {
    let w = world();
    let created = w.customers.create(&admin(), customer_input("Ada", "11111111")).await.unwrap();
    // Somebody else wrote between the read and the write of the use case: the stored version moved on.
    let mut stored = w.store.data().customers[&created.id].clone();
    stored.version = 5;
    w.store.insert_customer(stored);
    let mut db = super::ports::Store::session(&w.store).await.unwrap();
    assert_eq!(code(db.customer_update(&created).await), "CONFLICT-00");
}

#[tokio::test]
async fn deleting_a_customer_takes_the_exclusive_lock_and_releases_the_tax_id() {
    let w = world();
    let created = w.customers.create(&admin(), customer_input("Ada", "11111111")).await.unwrap();
    w.customers.delete(&admin(), created.id).await.unwrap();

    assert_eq!(w.store.locks(), [format!("update customer {}", created.id)]);
    assert_eq!(w.store.commits(), 1);
    assert_eq!(code(w.customers.get(created.id).await), "RESOURCE-NOT-FOUND-01");
    let again = w.customers.create(&admin(), customer_input("Ada again", "11111111")).await;
    assert!(again.is_ok(), "a soft-deleted customer releases its tax id");
}

#[tokio::test]
async fn a_customer_with_orders_cannot_be_deleted() {
    let w = world();
    let customer = w.customers.create(&admin(), customer_input("Ada", "11111111")).await.unwrap();
    let order = w.orders.create(&user(), customer.id, vec![item("Pen", "1.00", 1)]).await.unwrap();

    let error = w.customers.delete(&admin(), customer.id).await.unwrap_err();
    assert_eq!(error.code(), Some("CONFLICT-03"));
    assert_eq!(w.store.commits(), 1, "only the order creation committed");
    assert!(w.customers.get(customer.id).await.is_ok());

    w.orders.delete(&user(), order.id).await.unwrap();
    assert!(w.customers.delete(&admin(), customer.id).await.is_ok(), "no active order left");
}

#[tokio::test]
async fn a_search_is_validated_before_it_reaches_the_store() {
    let w = world();
    let too_big = SearchRequest { size: 101, ..SearchRequest::default() };
    assert_eq!(code(w.customers.search(too_big.clone()).await), "VALIDATION-01");
    assert_eq!(code(w.orders.search(too_big).await), "VALIDATION-01");
    w.customers.create(&admin(), customer_input("Ada", "11111111")).await.unwrap();
    let page = w.customers.search(SearchRequest::default()).await.unwrap();
    assert_eq!((page.total, page.size, page.items.len()), (1, 20, 1));
}

// --- orders ----------------------------------------------------------------------------------------------------

async fn world_with_customer() -> (World, Uuid) {
    let w = world();
    let customer = w.customers.create(&admin(), customer_input("Ada Lovelace", "11111111")).await.unwrap();
    (w, customer.id)
}

#[tokio::test]
async fn creating_an_order_locks_the_customer_for_share_in_the_same_transaction() {
    let (w, customer_id) = world_with_customer().await;
    let order =
        w.orders.create(&user(), customer_id, vec![item("Pen", "0.10", 3), item("Book", "19.99", 2)]).await.unwrap();

    assert_eq!(order.total().to_string(), "40.28");
    assert_eq!(w.store.locks(), [format!("share customer {customer_id}")]);
    assert_eq!(w.orders.get(order.id).await.unwrap().items.len(), 2);
    assert_eq!(w.store.commits(), 1);
}

#[tokio::test]
async fn an_order_for_a_missing_customer_is_validation_07_with_the_id() {
    let w = world();
    let missing = Uuid::new_v4();
    let error = w.orders.create(&user(), missing, vec![]).await.unwrap_err();
    let Error::App(AppError { code, params, .. }) = error else { panic!("expected an application error") };
    assert_eq!(code, "VALIDATION-07");
    assert_eq!(params["customer_id"], missing.to_string());
}

#[tokio::test]
async fn an_order_for_a_deleted_customer_is_refused_too() {
    let (w, customer_id) = world_with_customer().await;
    w.customers.delete(&admin(), customer_id).await.unwrap();
    assert_eq!(code(w.orders.create(&user(), customer_id, vec![]).await), "VALIDATION-07");
}

#[tokio::test]
async fn an_invalid_item_rolls_the_whole_order_back() {
    let (w, customer_id) = world_with_customer().await;
    let commits = w.store.commits();
    assert_eq!(code(w.orders.create(&user(), customer_id, vec![item("Pen", "0", 1)]).await), "VALIDATION-01");
    assert_eq!(w.store.commits(), commits);
    assert!(w.store.data().orders.is_empty());
}

#[tokio::test]
async fn items_are_added_changed_and_removed_with_the_version_moving_each_time() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![item("Pen", "1.00", 1)]).await.unwrap();

    let order = w.orders.add_item(&user(), order.id, item("Book", "10.00", 1)).await.unwrap();
    assert_eq!((order.items.len(), order.version), (2, 2));
    let book = order.items[1].id;
    let order = w.orders.update_item_quantity(&user(), order.id, book, 4).await.unwrap();
    assert_eq!((order.total().to_string(), order.version), ("41.00".to_string(), 3));
    let order = w.orders.remove_item(&user(), order.id, book).await.unwrap();
    assert_eq!((order.items.len(), order.version), (1, 4));
    assert_eq!(w.orders.get(order.id).await.unwrap().version, 4);
}

#[tokio::test]
async fn a_rule_violation_changes_nothing() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![item("Pen", "1.00", 1)]).await.unwrap();
    assert_eq!(
        code(w.orders.update_item_quantity(&user(), order.id, Uuid::new_v4(), 2).await),
        "RESOURCE-NOT-FOUND-03"
    );
    assert_eq!(w.orders.get(order.id).await.unwrap().version, 1);
}

#[tokio::test]
async fn a_missing_order_is_a_404_everywhere() {
    let w = world();
    let missing = Uuid::new_v4();
    assert_eq!(code(w.orders.get(missing).await), "RESOURCE-NOT-FOUND-02");
    assert_eq!(code(w.orders.delete(&user(), missing).await), "RESOURCE-NOT-FOUND-02");
    assert_eq!(code(w.orders.add_item(&user(), missing, item("x", "1.00", 1)).await), "RESOURCE-NOT-FOUND-02");
    assert_eq!(code(w.orders.confirm(&user(), missing).await), "RESOURCE-NOT-FOUND-02");
    assert_eq!(code(w.orders.cancel(&user(), missing).await), "RESOURCE-NOT-FOUND-02");
}

#[tokio::test]
async fn a_deleted_order_disappears() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![]).await.unwrap();
    w.orders.delete(&user(), order.id).await.unwrap();
    assert_eq!(code(w.orders.get(order.id).await), "RESOURCE-NOT-FOUND-02");
    assert_eq!(code(w.orders.delete(&user(), order.id).await), "RESOURCE-NOT-FOUND-02");
}

#[tokio::test]
async fn confirming_announces_the_change_with_the_customer_and_the_exact_total() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![item("Pen", "0.10", 3)]).await.unwrap();

    let confirmed = w.orders.confirm(&user(), order.id).await.unwrap();

    assert_eq!(confirmed.status, OrderStatus::Confirmed);
    let events = w.publisher.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(
        (event.order_id, event.old_status, event.new_status),
        (order.id, OrderStatus::Open, OrderStatus::Confirmed)
    );
    assert_eq!((event.customer_email.as_str(), event.customer_name.as_str()), ("ada@example.com", "Ada Lovelace"));
    assert_eq!((event.total_amount.to_string(), event.changed_at), ("0.30".to_string(), instant()));
}

#[tokio::test]
async fn canceling_a_confirmed_order_reports_the_previous_status() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![item("Pen", "1.00", 1)]).await.unwrap();
    w.orders.confirm(&user(), order.id).await.unwrap();
    w.orders.cancel(&user(), order.id).await.unwrap();
    let last = w.publisher.events().pop().unwrap();
    assert_eq!((last.old_status, last.new_status), (OrderStatus::Confirmed, OrderStatus::Canceled));
}

#[tokio::test]
async fn a_failed_transition_announces_nothing() {
    let (w, customer_id) = world_with_customer().await;
    let empty = w.orders.create(&user(), customer_id, vec![]).await.unwrap();
    assert_eq!(code(w.orders.confirm(&user(), empty.id).await), "VALIDATION-03");
    w.orders.cancel(&user(), empty.id).await.unwrap();
    assert_eq!(code(w.orders.cancel(&user(), empty.id).await), "VALIDATION-08");
    assert_eq!(w.publisher.events().len(), 1, "only the successful cancel");
}

#[tokio::test]
async fn a_broker_failure_never_fails_the_transition() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![item("Pen", "1.00", 1)]).await.unwrap();
    w.publisher.set_failing(true);
    let confirmed = w.orders.confirm(&user(), order.id).await.unwrap();
    assert_eq!(confirmed.status, OrderStatus::Confirmed);
    assert_eq!(w.orders.get(order.id).await.unwrap().status, OrderStatus::Confirmed);
    assert!(w.publisher.events().is_empty());
}

#[tokio::test]
async fn without_a_publisher_the_transition_still_works() {
    let (w, customer_id) = world_with_customer().await;
    let silent = OrderService::new(Arc::new(w.store.clone()), None, clock());
    let order = silent.create(&user(), customer_id, vec![item("Pen", "1.00", 1)]).await.unwrap();
    assert!(silent.confirm(&user(), order.id).await.is_ok());
}

#[tokio::test]
async fn a_customer_that_vanished_before_the_announcement_is_not_an_error() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![item("Pen", "1.00", 1)]).await.unwrap();
    // Soft-delete behind the use case's back: the order stays, the customer to notify is gone.
    let mut customer = w.store.data().customers[&customer_id].clone();
    customer.deleted_at = Some(instant());
    customer.deleted_by = Some("x".into());
    w.store.insert_customer(customer);

    assert!(w.orders.confirm(&user(), order.id).await.is_ok());
    assert!(w.publisher.events().is_empty());
}

#[tokio::test]
async fn a_concurrent_modification_is_reported_and_announces_nothing() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![item("Pen", "1.00", 1)]).await.unwrap();
    w.store.fail_order_save_with(|| {
        AppError::conflict(codes::CONCURRENT_MODIFICATION, "the resource was modified by someone else").into()
    });
    assert_eq!(code(w.orders.confirm(&user(), order.id).await), "CONFLICT-00");
    assert!(w.publisher.events().is_empty());
}

#[tokio::test]
async fn a_stale_order_write_is_refused_by_the_version_check() {
    let (w, customer_id) = world_with_customer().await;
    let order = w.orders.create(&user(), customer_id, vec![item("Pen", "1.00", 1)]).await.unwrap();
    w.store.touch_order(order.id);
    let mut db = super::ports::Store::session(&w.store).await.unwrap();
    assert_eq!(code(db.order_save(&order).await), "CONFLICT-00");
}
