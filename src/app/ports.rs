use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use super::search::{Page, SearchRequest};
use crate::domain::{Customer, Order, OrderStatusChanged};
use crate::error::Result;

/// One connection, optionally inside a transaction: every operation the use cases need from the database.
///
/// The traits live here, where they are consumed; `persistence` implements them over PostgreSQL and the tests
/// implement them in memory. A session is one trait (not a customers one and an orders one) because a transaction
/// owns one connection, and both repositories must run on it.
///
/// Every read only sees active (non soft-deleted) rows, and a missing row is reported as RESOURCE-NOT-FOUND-01 (a
/// customer) or -02 (an order).
#[async_trait]
pub trait Session: Send {
    async fn customer_get(&mut self, id: Uuid) -> Result<Customer>;

    /// Reads the customer and takes a row lock that excludes every other lock on it. Deleting a customer uses it so
    /// the "has orders?" check and the soft delete cannot interleave with an order creation.
    async fn customer_lock_for_update(&mut self, id: Uuid) -> Result<Customer>;

    /// Reads the customer and takes a shared row lock: many orders can be created for one customer at once, but none
    /// while that customer is being deleted (and vice versa).
    async fn customer_lock_for_share(&mut self, id: Uuid) -> Result<Customer>;

    async fn customer_create(&mut self, customer: &Customer) -> Result<()>;

    /// Writes the customer if its version still matches the row (CONFLICT-00 otherwise) and returns it with the next
    /// version.
    async fn customer_update(&mut self, customer: &Customer) -> Result<Customer>;

    async fn customer_soft_delete(&mut self, id: Uuid, actor: &str, at: OffsetDateTime) -> Result<()>;

    /// True when another active customer has this tax id; `except` is the customer to ignore (itself on update).
    async fn tax_id_taken(&mut self, tax_id: &str, except: Option<Uuid>) -> Result<bool>;

    async fn passport_taken(&mut self, passport: &str, except: Option<Uuid>) -> Result<bool>;

    async fn customer_search(&mut self, request: &SearchRequest) -> Result<Page<Customer>>;

    async fn order_get(&mut self, id: Uuid) -> Result<Order>;

    async fn order_create(&mut self, order: &Order) -> Result<()>;

    /// Writes the status and the items of an order that was read earlier. It checks and increments the version: a
    /// concurrent change fails with CONFLICT-00 instead of being overwritten silently. Returns the order with the next
    /// version.
    async fn order_save(&mut self, order: &Order) -> Result<Order>;

    async fn order_soft_delete(&mut self, id: Uuid, actor: &str, at: OffsetDateTime) -> Result<()>;

    /// True when the customer has any non soft-deleted order.
    async fn has_active_orders(&mut self, customer_id: Uuid) -> Result<bool>;

    async fn order_search(&mut self, request: &SearchRequest) -> Result<Page<Order>>;

    /// Makes the changes of a transaction permanent. For a session that is not in one this does nothing, and dropping
    /// a transaction without committing rolls it back.
    async fn commit(self: Box<Self>) -> Result<()>;
}

/// Hands out sessions: one statement at a time ([`Store::session`]) or inside a transaction ([`Store::begin`]).
#[async_trait]
pub trait Store: Send + Sync {
    /// A connection that runs each statement on its own.
    async fn session(&self) -> Result<Box<dyn Session>>;

    /// A transaction: nothing is permanent until [`Session::commit`], and dropping it rolls everything back.
    async fn begin(&self) -> Result<Box<dyn Session>>;

    /// Checks that the database answers, for /health.
    async fn ping(&self) -> Result<()>;
}

/// Announces order events. Publishing happens after the database commit and is best effort: see docs/adr/0003 for why
/// this is not a transactional outbox.
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish_status_changed(&self, event: &OrderStatusChanged) -> Result<(), PublishError>;
}

/// A failure to hand an event to the broker. The text is for the log and never quotes the payload.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PublishError(pub String);
