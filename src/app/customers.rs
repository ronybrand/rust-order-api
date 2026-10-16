use std::sync::Arc;

use tracing::info;
use uuid::Uuid;

use super::search::{Page, SearchRequest};
use super::{Actor, Clock, Session, Store};
use crate::domain::{Customer, CustomerInput};
use crate::error::{AppError, Result, codes};

/// The customer use cases.
pub struct CustomerService {
    store: Arc<dyn Store>,
    clock: Clock,
}

opaque_debug!(CustomerService);

impl CustomerService {
    pub fn new(store: Arc<dyn Store>, clock: Clock) -> Self {
        Self { store, clock }
    }

    /// Registers a customer, rejecting a duplicate tax id (CONFLICT-01) or passport (CONFLICT-02).
    pub async fn create(&self, actor: &Actor, input: CustomerInput) -> Result<Customer> {
        let customer = Customer::create(input, &actor.id, (self.clock)())?;
        let mut db = self.store.session().await?;
        // The unique indexes are the real guarantee under concurrency: the check below only gives the common case a
        // readable error without a failed write, and the repository maps a violation to the same codes.
        ensure_unique(db.as_mut(), &customer, None).await?;
        db.customer_create(&customer).await?;
        info!(customer = ?customer, actor = %actor.id, "customer created");
        Ok(customer)
    }

    pub async fn get(&self, id: Uuid) -> Result<Customer> {
        self.store.session().await?.customer_get(id).await
    }

    /// Replaces a customer's data, validating uniqueness against the other customers.
    pub async fn update(&self, actor: &Actor, id: Uuid, input: CustomerInput) -> Result<Customer> {
        let mut db = self.store.session().await?;
        let replaced = db.customer_get(id).await?.replace(input, &actor.id, (self.clock)())?;
        ensure_unique(db.as_mut(), &replaced, Some(id)).await?;
        let saved = db.customer_update(&replaced).await?;
        info!(customer = ?saved, actor = %actor.id, "customer updated");
        Ok(saved)
    }

    /// Soft-deletes a customer that has no active orders (CONFLICT-03 otherwise).
    pub async fn delete(&self, actor: &Actor, id: Uuid) -> Result<()> {
        let mut tx = self.store.begin().await?;
        // FOR UPDATE first: an order being created for this customer holds the shared lock, so the check below sees
        // it, and one that starts afterwards waits for this transaction and then finds the customer gone.
        tx.customer_lock_for_update(id).await?;
        if tx.has_active_orders(id).await? {
            return Err(AppError::conflict(codes::CUSTOMER_HAS_ORDERS, "customer has orders and cannot be deleted")
                .with("id", id)
                .into());
        }
        tx.customer_soft_delete(id, &actor.id, (self.clock)()).await?;
        tx.commit().await?;
        info!(customer_id = %id, actor = %actor.id, "customer deleted");
        Ok(())
    }

    /// Filters, sorts and pages the active customers.
    pub async fn search(&self, request: SearchRequest) -> Result<Page<Customer>> {
        let request = request.with_defaults();
        request.validate()?;
        self.store.session().await?.customer_search(&request).await
    }
}

/// CONFLICT-01/02 without echoing the value: tax id and passport are personal data.
async fn ensure_unique(db: &mut dyn Session, customer: &Customer, except: Option<Uuid>) -> Result<()> {
    if db.tax_id_taken(&customer.tax_id, except).await? {
        return Err(AppError::conflict(codes::DUPLICATE_TAX_ID, "a customer with this tax_id already exists").into());
    }
    if let Some(passport) = &customer.passport_number
        && db.passport_taken(passport, except).await?
    {
        return Err(AppError::conflict(
            codes::DUPLICATE_PASSPORT,
            "a customer with this passport_number already exists",
        )
        .into());
    }
    Ok(())
}
