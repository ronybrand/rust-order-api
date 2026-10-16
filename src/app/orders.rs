use std::sync::Arc;

use tracing::{error, info};
use uuid::Uuid;

use super::search::{Page, SearchRequest};
use super::{Actor, Clock, EventPublisher, Store};
use crate::domain::{ItemInput, Order, OrderStatus, OrderStatusChanged, StatusChange};
use crate::error::{AppError, Error, Result, codes};

/// The order use cases. The publisher may be absent when no notification is wanted.
pub struct OrderService {
    store: Arc<dyn Store>,
    publisher: Option<Arc<dyn EventPublisher>>,
    clock: Clock,
}

opaque_debug!(OrderService);

impl OrderService {
    pub fn new(store: Arc<dyn Store>, publisher: Option<Arc<dyn EventPublisher>>, clock: Clock) -> Self {
        Self { store, publisher, clock }
    }

    /// Opens an order for an existing customer (VALIDATION-07 if the customer does not exist).
    pub async fn create(&self, actor: &Actor, customer_id: Uuid, items: Vec<ItemInput>) -> Result<Order> {
        let mut tx = self.store.begin().await?;
        // FOR SHARE on the customer, in the same transaction that saves the order: a concurrent delete of this
        // customer waits for us (or we wait for it and then find nothing), so an order is never created under a
        // customer that was deleted in between.
        if let Err(error) = tx.customer_lock_for_share(customer_id).await {
            return Err(if error.code() == Some(codes::CUSTOMER_NOT_FOUND) {
                AppError::validation(codes::CUSTOMER_NOT_FOUND_IN_BODY, "customer does not exist")
                    .with("customer_id", customer_id)
                    .into()
            } else {
                error
            });
        }
        let order = Order::create(customer_id, items, &actor.id, (self.clock)())?;
        tx.order_create(&order).await?;
        tx.commit().await?;
        info!(order_id = %order.id, actor = %actor.id, "order created");
        Ok(order)
    }

    pub async fn get(&self, id: Uuid) -> Result<Order> {
        self.store.session().await?.order_get(id).await
    }

    pub async fn delete(&self, actor: &Actor, id: Uuid) -> Result<()> {
        let mut db = self.store.session().await?;
        db.order_get(id).await?;
        db.order_soft_delete(id, &actor.id, (self.clock)()).await?;
        info!(order_id = %id, actor = %actor.id, "order deleted");
        Ok(())
    }

    pub async fn add_item(&self, actor: &Actor, order_id: Uuid, input: ItemInput) -> Result<Order> {
        let now = (self.clock)();
        self.mutate(order_id, |order| order.add_item(input, &actor.id, now)).await
    }

    pub async fn update_item_quantity(
        &self,
        actor: &Actor,
        order_id: Uuid,
        item_id: Uuid,
        quantity: i32,
    ) -> Result<Order> {
        let now = (self.clock)();
        self.mutate(order_id, |order| order.update_item_quantity(item_id, quantity, &actor.id, now)).await
    }

    pub async fn remove_item(&self, actor: &Actor, order_id: Uuid, item_id: Uuid) -> Result<Order> {
        let now = (self.clock)();
        self.mutate(order_id, |order| order.remove_item(item_id, &actor.id, now)).await
    }

    /// Moves an OPEN order to CONFIRMED and announces the change.
    pub async fn confirm(&self, actor: &Actor, order_id: Uuid) -> Result<Order> {
        let now = (self.clock)();
        self.transition(order_id, |order| order.confirm(&actor.id, now)).await
    }

    /// Moves an OPEN or CONFIRMED order to CANCELED and announces the change.
    pub async fn cancel(&self, actor: &Actor, order_id: Uuid) -> Result<Order> {
        let now = (self.clock)();
        self.transition(order_id, |order| order.cancel(&actor.id, now)).await
    }

    /// Filters, sorts and pages the active orders.
    pub async fn search(&self, request: SearchRequest) -> Result<Page<Order>> {
        let request = request.with_defaults();
        request.validate()?;
        self.store.session().await?.order_search(&request).await
    }

    /// Loads the order, applies the change and saves it with the version it read.
    async fn mutate(&self, order_id: Uuid, change: impl FnOnce(Order) -> Result<Order, AppError>) -> Result<Order> {
        let mut tx = self.store.begin().await?;
        let current = tx.order_get(order_id).await?;
        let saved = tx.order_save(&change(current)?).await?;
        tx.commit().await?;
        Ok(saved)
    }

    async fn transition(
        &self,
        order_id: Uuid,
        change: impl FnOnce(Order) -> Result<StatusChange, AppError>,
    ) -> Result<Order> {
        let mut previous = None;
        let order = self
            .mutate(order_id, |current| {
                let result = change(current)?;
                previous = Some(result.previous);
                Ok(result.order)
            })
            .await?;
        let previous = previous.unwrap_or(order.status);
        info!(order_id = %order.id, from = %previous, to = %order.status, "order status changed");
        self.announce(&order, previous).await;
        Ok(order)
    }

    /// Publishes the status change after the commit. A broker failure is logged and never reaches the caller: the
    /// transition already happened and must not fail because a notification could not be queued.
    async fn announce(&self, order: &Order, previous: OrderStatus) {
        let Some(publisher) = &self.publisher else { return };
        let Some(customer) = self.customer_to_notify(order).await else { return };
        if customer.email.is_empty() {
            return;
        }
        let event = OrderStatusChanged {
            order_id: order.id,
            customer_email: customer.email,
            customer_name: customer.name,
            old_status: previous,
            new_status: order.status,
            total_amount: order.total(),
            changed_at: (self.clock)(),
        };
        // The error is logged through its Display, which the publisher keeps free of the payload.
        if let Err(failure) = publisher.publish_status_changed(&event).await {
            error!(event = ?event, reason = %failure, "failed to publish");
        }
    }

    async fn customer_to_notify(&self, order: &Order) -> Option<crate::domain::Customer> {
        let lookup = async { self.store.session().await?.customer_get(order.customer_id).await }.await;
        match lookup {
            Ok(customer) => Some(customer),
            Err(failure) => {
                let reason = match &failure {
                    Error::App(app) => app.code,
                    Error::Unexpected(_) => "unexpected",
                };
                error!(order_id = %order.id, reason, "could not load the customer to announce the status change");
                None
            }
        }
    }
}
