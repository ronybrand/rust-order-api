//! An in-memory [`Store`] and [`EventPublisher`] for the tests of the use cases and of the HTTP layer.
//!
//! It keeps the rules the database enforces and the use cases rely on: only active rows are visible, the version is
//! checked on writes (CONFLICT-00), the unique indexes (CONFLICT-01/02) refuse a duplicate even when the use case's own
//! check was skipped, and a transaction works on a copy that only a commit makes permanent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use super::ports::{EventPublisher, PublishError, Session, Store};
use super::search::{Page, SearchRequest};
use crate::domain::{Customer, Order, OrderStatusChanged};
use crate::error::{AppError, Error, Result, codes};

#[derive(Clone, Default)]
pub struct Data {
    pub customers: HashMap<Uuid, Customer>,
    pub orders: HashMap<Uuid, Order>,
}

#[derive(Default)]
struct Shared {
    data: Data,
    /// The names of the locks taken, in order, so a test can assert the locking discipline.
    locks: Vec<String>,
    commits: usize,
    fail_ping: bool,
    fail_save: Option<Arc<dyn Fn() -> Error + Send + Sync>>,
}

#[derive(Clone, Default)]
pub struct MemoryStore {
    shared: Arc<Mutex<Shared>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    pub fn data(&self) -> Data {
        self.lock().data.clone()
    }

    pub fn insert_customer(&self, customer: Customer) {
        self.lock().data.customers.insert(customer.id, customer);
    }

    pub fn locks(&self) -> Vec<String> {
        self.lock().locks.clone()
    }

    pub fn commits(&self) -> usize {
        self.lock().commits
    }

    pub fn set_ping_failure(&self, fail: bool) {
        self.lock().fail_ping = fail;
    }

    /// Makes every `order_save` fail with the given error, to test what happens around a failing write.
    pub fn fail_order_save_with(&self, error: impl Fn() -> Error + Send + Sync + 'static) {
        self.lock().fail_save = Some(Arc::new(error));
    }

    /// Bumps the stored version of an order behind the use case's back: a concurrent writer.
    pub fn touch_order(&self, id: Uuid) {
        if let Some(order) = self.lock().data.orders.get_mut(&id) {
            order.version += 1;
        }
    }
}

pub struct MemorySession {
    store: MemoryStore,
    /// The working copy of a transaction; `None` for a session that writes straight to the store.
    copy: Option<Data>,
}

impl MemorySession {
    fn read<T>(&self, f: impl FnOnce(&Data) -> T) -> T {
        match &self.copy {
            Some(copy) => f(copy),
            None => f(&self.store.lock().data),
        }
    }

    fn write<T>(&mut self, f: impl FnOnce(&mut Data) -> T) -> T {
        match &mut self.copy {
            Some(copy) => f(copy),
            None => f(&mut self.store.lock().data),
        }
    }

    fn lock_event(&self, name: &str, id: Uuid) {
        self.store.lock().locks.push(format!("{name} {id}"));
    }
}

fn customer_not_found(id: Uuid) -> Error {
    AppError::not_found(codes::CUSTOMER_NOT_FOUND, "customer not found").with("id", id).into()
}

fn order_not_found(id: Uuid) -> Error {
    AppError::not_found(codes::ORDER_NOT_FOUND, "order not found").with("id", id).into()
}

fn concurrent_modification() -> Error {
    AppError::conflict(
        codes::CONCURRENT_MODIFICATION,
        "the resource was modified by someone else, reload it and try again",
    )
    .into()
}

#[async_trait]
impl Session for MemorySession {
    async fn customer_get(&mut self, id: Uuid) -> Result<Customer> {
        self.read(|d| d.customers.get(&id).filter(|c| !c.is_deleted()).cloned()).ok_or_else(|| customer_not_found(id))
    }

    async fn customer_lock_for_update(&mut self, id: Uuid) -> Result<Customer> {
        let customer = self.customer_get(id).await?;
        self.lock_event("update customer", id);
        Ok(customer)
    }

    async fn customer_lock_for_share(&mut self, id: Uuid) -> Result<Customer> {
        let customer = self.customer_get(id).await?;
        self.lock_event("share customer", id);
        Ok(customer)
    }

    async fn customer_create(&mut self, customer: &Customer) -> Result<()> {
        self.write(|d| {
            let clash = |other: &Customer| !other.is_deleted() && other.id != customer.id;
            if d.customers.values().any(|o| clash(o) && o.tax_id == customer.tax_id) {
                return Err(
                    AppError::conflict(codes::DUPLICATE_TAX_ID, "a customer with this tax_id already exists").into()
                );
            }
            if customer.passport_number.is_some()
                && d.customers.values().any(|o| clash(o) && o.passport_number == customer.passport_number)
            {
                return Err(AppError::conflict(
                    codes::DUPLICATE_PASSPORT,
                    "a customer with this passport_number already exists",
                )
                .into());
            }
            d.customers.insert(customer.id, customer.clone());
            Ok(())
        })
    }

    async fn customer_update(&mut self, customer: &Customer) -> Result<Customer> {
        self.write(|d| {
            let stored = d.customers.get_mut(&customer.id).filter(|c| !c.is_deleted());
            match stored {
                Some(stored) if stored.version == customer.version => {
                    let mut saved = customer.clone();
                    saved.version += 1;
                    *stored = saved.clone();
                    Ok(saved)
                }
                _ => Err(concurrent_modification()),
            }
        })
    }

    async fn customer_soft_delete(&mut self, id: Uuid, actor: &str, at: OffsetDateTime) -> Result<()> {
        self.write(|d| match d.customers.get_mut(&id).filter(|c| !c.is_deleted()) {
            Some(customer) => {
                customer.deleted_at = Some(at);
                customer.deleted_by = Some(actor.to_owned());
                customer.version += 1;
                Ok(())
            }
            None => Err(customer_not_found(id)),
        })
    }

    async fn tax_id_taken(&mut self, tax_id: &str, except: Option<Uuid>) -> Result<bool> {
        Ok(self.read(|d| d.customers.values().any(|c| !c.is_deleted() && c.tax_id == tax_id && Some(c.id) != except)))
    }

    async fn passport_taken(&mut self, passport: &str, except: Option<Uuid>) -> Result<bool> {
        Ok(self.read(|d| {
            d.customers
                .values()
                .any(|c| !c.is_deleted() && c.passport_number.as_deref() == Some(passport) && Some(c.id) != except)
        }))
    }

    async fn customer_search(&mut self, request: &SearchRequest) -> Result<Page<Customer>> {
        // The planner and the SQL are tested against a real database; here a search only needs to page and sort.
        let mut all: Vec<Customer> = self.read(|d| d.customers.values().filter(|c| !c.is_deleted()).cloned().collect());
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        Ok(page_of(all, request))
    }

    async fn order_get(&mut self, id: Uuid) -> Result<Order> {
        self.read(|d| d.orders.get(&id).filter(|o| !o.is_deleted()).cloned()).ok_or_else(|| order_not_found(id))
    }

    async fn order_create(&mut self, order: &Order) -> Result<()> {
        self.write(|d| {
            if d.customers.get(&order.customer_id).is_none_or(Customer::is_deleted) {
                // The foreign key: what the database would refuse.
                return Err(
                    AppError::conflict(codes::DATA_INTEGRITY, "the operation violates a data integrity rule").into()
                );
            }
            d.orders.insert(order.id, order.clone());
            Ok(())
        })
    }

    async fn order_save(&mut self, order: &Order) -> Result<Order> {
        let failure = self.store.lock().fail_save.clone();
        if let Some(failure) = failure {
            return Err(failure());
        }
        self.write(|d| match d.orders.get_mut(&order.id).filter(|o| !o.is_deleted()) {
            Some(stored) if stored.version == order.version => {
                let mut saved = order.clone();
                saved.version += 1;
                *stored = saved.clone();
                Ok(saved)
            }
            _ => Err(concurrent_modification()),
        })
    }

    async fn order_soft_delete(&mut self, id: Uuid, actor: &str, at: OffsetDateTime) -> Result<()> {
        self.write(|d| match d.orders.get_mut(&id).filter(|o| !o.is_deleted()) {
            Some(order) => {
                order.deleted_at = Some(at);
                order.deleted_by = Some(actor.to_owned());
                order.version += 1;
                Ok(())
            }
            None => Err(order_not_found(id)),
        })
    }

    async fn has_active_orders(&mut self, customer_id: Uuid) -> Result<bool> {
        Ok(self.read(|d| d.orders.values().any(|o| o.customer_id == customer_id && !o.is_deleted())))
    }

    async fn order_search(&mut self, request: &SearchRequest) -> Result<Page<Order>> {
        let mut all: Vec<Order> = self.read(|d| d.orders.values().filter(|o| !o.is_deleted()).cloned().collect());
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        Ok(page_of(all, request))
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        let Self { store, copy } = *self;
        if let Some(copy) = copy {
            let mut shared = store.lock();
            shared.data = copy;
            shared.commits += 1;
        }
        Ok(())
    }
}

fn page_of<T>(all: Vec<T>, request: &SearchRequest) -> Page<T> {
    let total = i64::try_from(all.len()).unwrap_or(i64::MAX);
    let offset = usize::try_from(request.offset()).unwrap_or(usize::MAX);
    let items = all.into_iter().skip(offset).take(usize::try_from(request.size).unwrap_or(0)).collect();
    Page { items, total, page: request.page, size: request.size }
}

#[async_trait]
impl Store for MemoryStore {
    async fn session(&self) -> Result<Box<dyn Session>> {
        Ok(Box::new(MemorySession { store: self.clone(), copy: None }))
    }

    async fn begin(&self) -> Result<Box<dyn Session>> {
        let copy = self.lock().data.clone();
        Ok(Box::new(MemorySession { store: self.clone(), copy: Some(copy) }))
    }

    async fn ping(&self) -> Result<()> {
        if self.lock().fail_ping {
            return Err(Error::unexpected(std::io::Error::other("database unavailable")));
        }
        Ok(())
    }
}

/// Records what is published, and can be told to fail.
#[derive(Clone, Default)]
pub struct RecordingPublisher {
    events: Arc<Mutex<Vec<OrderStatusChanged>>>,
    fail: Arc<Mutex<bool>>,
}

impl RecordingPublisher {
    pub fn events(&self) -> Vec<OrderStatusChanged> {
        self.events.lock().unwrap().clone()
    }

    pub fn set_failing(&self, fail: bool) {
        *self.fail.lock().unwrap() = fail;
    }
}

#[async_trait]
impl EventPublisher for RecordingPublisher {
    async fn publish_status_changed(&self, event: &OrderStatusChanged) -> Result<(), PublishError> {
        if *self.fail.lock().unwrap() {
            return Err(PublishError("broker unavailable".into()));
        }
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}
