//! The [`Store`] backed by PostgreSQL through sqlx.
//!
//! Queries are written by hand and checked at run time. A compile-time check (`sqlx::query!`) needs a database or a
//! saved query cache at build time, and the search builds its SQL dynamically anyway; the integration tests, which run
//! every statement against a real server, are what catch a typo here.

use std::time::Duration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, FromRow, PgConnection, PgPool, Postgres, QueryBuilder, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use super::plan::{CUSTOMERS, ORDERS};
use super::sql::{push_order, push_where};
use super::translate::{concurrent_modification, customer_not_found, order_not_found};
use crate::app::search::{Page, SearchRequest};
use crate::app::{Session, Store};
use crate::domain::{Customer, Item, Order, OrderStatus};
use crate::error::{Error, Result};

const POOL_SIZE: u32 = 10;
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const PING_TIMEOUT: Duration = Duration::from_secs(2);

// The statements below that are built with `format!` interpolate only these constants, the lock clause and two column
// names that are literals in this file: never a value that came from a client. Those go through `AssertSqlSafe`.
const CUSTOMER_COLUMNS: &str = "id, name, tax_id, passport_number, email, version, created_at, updated_at, \
     created_by, updated_by, deleted_at, deleted_by";
const ORDER_COLUMNS: &str =
    "id, customer_id, status, version, created_at, updated_at, created_by, updated_by, deleted_at, deleted_by";
const ITEM_COLUMNS: &str = "id, order_id, description, unit_price, quantity";

/// A pool of connections to the database at `url` (`postgres://user:pass@host:5432/db?sslmode=disable`).
pub async fn connect(url: &str) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(POOL_SIZE)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .test_before_acquire(true)
        .connect(url)
        .await
        .map_err(Error::from)
}

/// Applies the migrations under `migrations/`, which are embedded in the binary. sqlx takes an advisory lock, so
/// several replicas starting together are safe, and a database already at the latest version is left as it is.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await.map_err(Error::unexpected)
}

#[derive(Clone, Debug)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Store for PostgresStore {
    async fn session(&self) -> Result<Box<dyn Session>> {
        Ok(Box::new(PgSession { conn: Conn::Pool(self.pool.acquire().await?) }))
    }

    async fn begin(&self) -> Result<Box<dyn Session>> {
        Ok(Box::new(PgSession { conn: Conn::Tx(self.pool.begin().await?) }))
    }

    async fn ping(&self) -> Result<()> {
        let probe = sqlx::query("SELECT 1").execute(&self.pool);
        match tokio::time::timeout(PING_TIMEOUT, probe).await {
            Ok(result) => result.map(|_| ()).map_err(Error::from),
            Err(elapsed) => Err(Error::unexpected(elapsed)),
        }
    }
}

enum Conn {
    /// Each statement commits on its own.
    Pool(PoolConnection<Postgres>),
    /// Everything commits together, or nothing does: dropping it without `commit` rolls back.
    Tx(Transaction<'static, Postgres>),
}

pub struct PgSession {
    conn: Conn,
}

opaque_debug!(PgSession);

impl PgSession {
    fn conn(&mut self) -> &mut PgConnection {
        match &mut self.conn {
            Conn::Pool(connection) => connection,
            Conn::Tx(transaction) => transaction,
        }
    }
}

#[derive(FromRow)]
struct CustomerRow {
    id: Uuid,
    name: String,
    tax_id: String,
    passport_number: Option<String>,
    email: String,
    version: i64,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    created_by: String,
    updated_by: String,
    deleted_at: Option<OffsetDateTime>,
    deleted_by: Option<String>,
}

impl From<CustomerRow> for Customer {
    fn from(row: CustomerRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            tax_id: row.tax_id,
            passport_number: row.passport_number,
            email: row.email,
            version: row.version,
            created_at: row.created_at,
            updated_at: row.updated_at,
            created_by: row.created_by,
            updated_by: row.updated_by,
            deleted_at: row.deleted_at,
            deleted_by: row.deleted_by,
        }
    }
}

#[derive(FromRow)]
struct OrderRow {
    id: Uuid,
    customer_id: Uuid,
    status: String,
    version: i64,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    created_by: String,
    updated_by: String,
    deleted_at: Option<OffsetDateTime>,
    deleted_by: Option<String>,
}

impl OrderRow {
    fn into_order(self, items: Vec<Item>) -> Result<Order> {
        // The CHECK constraint keeps anything else out; a value outside the enum would mean the schema was changed.
        let status = self.status.parse::<OrderStatus>().map_err(|()| {
            Error::unexpected(std::io::Error::other(format!("order {} has an unknown status", self.id)))
        })?;
        Ok(Order {
            id: self.id,
            customer_id: self.customer_id,
            status,
            items,
            version: self.version,
            created_at: self.created_at,
            updated_at: self.updated_at,
            created_by: self.created_by,
            updated_by: self.updated_by,
            deleted_at: self.deleted_at,
            deleted_by: self.deleted_by,
        })
    }
}

#[derive(FromRow)]
struct ItemRow {
    id: Uuid,
    order_id: Uuid,
    description: String,
    unit_price: Decimal,
    quantity: i32,
}

impl From<ItemRow> for Item {
    fn from(row: ItemRow) -> Self {
        Self {
            id: row.id,
            order_id: row.order_id,
            description: row.description,
            unit_price: row.unit_price,
            quantity: row.quantity,
        }
    }
}

enum Lock {
    None,
    Update,
    Share,
}

impl Lock {
    fn clause(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Update => " FOR UPDATE",
            Self::Share => " FOR SHARE",
        }
    }
}

impl PgSession {
    async fn find_customer(&mut self, id: Uuid, lock: Lock) -> Result<Customer> {
        let sql =
            format!("SELECT {CUSTOMER_COLUMNS} FROM customers WHERE id = $1 AND deleted_at IS NULL{}", lock.clause());
        let row = sqlx::query_as::<_, CustomerRow>(AssertSqlSafe(sql)).bind(id).fetch_optional(self.conn()).await?;
        row.map(Customer::from).ok_or_else(|| customer_not_found(id))
    }

    /// The items of the given orders in insertion order (`seq`).
    async fn items_of(&mut self, order_ids: &[Uuid]) -> Result<Vec<Item>> {
        let sql = format!("SELECT {ITEM_COLUMNS} FROM order_items WHERE order_id = ANY($1) ORDER BY seq");
        let rows = sqlx::query_as::<_, ItemRow>(AssertSqlSafe(sql)).bind(order_ids).fetch_all(self.conn()).await?;
        Ok(rows.into_iter().map(Item::from).collect())
    }

    async fn insert_item(&mut self, item: &Item) -> Result<()> {
        sqlx::query(
            "INSERT INTO order_items (id, order_id, description, unit_price, quantity) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(item.id)
        .bind(item.order_id)
        .bind(&item.description)
        .bind(item.unit_price)
        .bind(item.quantity)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    /// Makes the stored items equal to the aggregate's: rows no longer in it are deleted, new ones inserted, and the
    /// ones whose data changed are updated. The order row was just updated, so it is locked for this transaction.
    async fn sync_items(&mut self, order: &Order) -> Result<()> {
        let stored = self.items_of(&[order.id]).await?;
        let kept: Vec<Uuid> = order.items.iter().map(|item| item.id).collect();
        sqlx::query("DELETE FROM order_items WHERE order_id = $1 AND id <> ALL($2)")
            .bind(order.id)
            .bind(&kept)
            .execute(self.conn())
            .await?;
        for item in &order.items {
            match stored.iter().find(|before| before.id == item.id) {
                None => self.insert_item(item).await?,
                Some(before) if before != item => {
                    sqlx::query(
                        "UPDATE order_items SET description = $2, unit_price = $3, quantity = $4 WHERE id = $1",
                    )
                    .bind(item.id)
                    .bind(&item.description)
                    .bind(item.unit_price)
                    .bind(item.quantity)
                    .execute(self.conn())
                    .await?;
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    async fn customer_taken(&mut self, column: &'static str, value: &str, except: Option<Uuid>) -> Result<bool> {
        // `column` is one of two literals below, never client input.
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM customers WHERE {column} = $1 AND deleted_at IS NULL AND ($2::uuid IS NULL OR id <> $2))"
        );
        Ok(sqlx::query_scalar::<_, bool>(AssertSqlSafe(sql)).bind(value).bind(except).fetch_one(self.conn()).await?)
    }
}

#[async_trait]
impl Session for PgSession {
    async fn customer_get(&mut self, id: Uuid) -> Result<Customer> {
        self.find_customer(id, Lock::None).await
    }

    async fn customer_lock_for_update(&mut self, id: Uuid) -> Result<Customer> {
        self.find_customer(id, Lock::Update).await
    }

    async fn customer_lock_for_share(&mut self, id: Uuid) -> Result<Customer> {
        self.find_customer(id, Lock::Share).await
    }

    async fn customer_create(&mut self, customer: &Customer) -> Result<()> {
        sqlx::query(
            "INSERT INTO customers (id, name, tax_id, passport_number, email, version, created_at, updated_at, \
             created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(customer.id)
        .bind(&customer.name)
        .bind(&customer.tax_id)
        .bind(&customer.passport_number)
        .bind(&customer.email)
        .bind(customer.version)
        .bind(customer.created_at)
        .bind(customer.updated_at)
        .bind(&customer.created_by)
        .bind(&customer.updated_by)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    async fn customer_update(&mut self, customer: &Customer) -> Result<Customer> {
        let changed = sqlx::query(
            "UPDATE customers SET name = $3, tax_id = $4, passport_number = $5, email = $6, updated_at = $7, \
             updated_by = $8, version = version + 1 WHERE id = $1 AND deleted_at IS NULL AND version = $2",
        )
        .bind(customer.id)
        .bind(customer.version)
        .bind(&customer.name)
        .bind(&customer.tax_id)
        .bind(&customer.passport_number)
        .bind(&customer.email)
        .bind(customer.updated_at)
        .bind(&customer.updated_by)
        .execute(self.conn())
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(concurrent_modification());
        }
        Ok(Customer { version: customer.version + 1, ..customer.clone() })
    }

    async fn customer_soft_delete(&mut self, id: Uuid, actor: &str, at: OffsetDateTime) -> Result<()> {
        let changed = sqlx::query(
            "UPDATE customers SET deleted_at = $2, deleted_by = $3, updated_at = $2, updated_by = $3, \
             version = version + 1 WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(at)
        .bind(actor)
        .execute(self.conn())
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(customer_not_found(id));
        }
        Ok(())
    }

    async fn tax_id_taken(&mut self, tax_id: &str, except: Option<Uuid>) -> Result<bool> {
        self.customer_taken("tax_id", tax_id, except).await
    }

    async fn passport_taken(&mut self, passport: &str, except: Option<Uuid>) -> Result<bool> {
        self.customer_taken("passport_number", passport, except).await
    }

    async fn customer_search(&mut self, request: &SearchRequest) -> Result<Page<Customer>> {
        let plan = CUSTOMERS.plan(request)?;
        let mut count = QueryBuilder::<Postgres>::new("SELECT count(*) FROM customers");
        push_where(&mut count, &plan);
        let total: i64 = count.build_query_scalar().fetch_one(self.conn()).await?;

        let mut select = QueryBuilder::<Postgres>::new(format!("SELECT {CUSTOMER_COLUMNS} FROM customers"));
        push_where(&mut select, &plan);
        push_order(&mut select, plan.sort);
        select.push(" LIMIT ").push_bind(i64::from(request.size)).push(" OFFSET ").push_bind(request.offset());
        let rows: Vec<CustomerRow> = select.build_query_as().fetch_all(self.conn()).await?;
        Ok(Page {
            items: rows.into_iter().map(Customer::from).collect(),
            total,
            page: request.page,
            size: request.size,
        })
    }

    async fn order_get(&mut self, id: Uuid) -> Result<Order> {
        let sql = format!("SELECT {ORDER_COLUMNS} FROM orders WHERE id = $1 AND deleted_at IS NULL");
        let row = sqlx::query_as::<_, OrderRow>(AssertSqlSafe(sql)).bind(id).fetch_optional(self.conn()).await?;
        let row = row.ok_or_else(|| order_not_found(id))?;
        let items = self.items_of(&[id]).await?;
        row.into_order(items)
    }

    async fn order_create(&mut self, order: &Order) -> Result<()> {
        sqlx::query(
            "INSERT INTO orders (id, customer_id, status, version, created_at, updated_at, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(order.id)
        .bind(order.customer_id)
        .bind(order.status.as_str())
        .bind(order.version)
        .bind(order.created_at)
        .bind(order.updated_at)
        .bind(&order.created_by)
        .bind(&order.updated_by)
        .execute(self.conn())
        .await?;
        for item in &order.items {
            self.insert_item(item).await?;
        }
        Ok(())
    }

    async fn order_save(&mut self, order: &Order) -> Result<Order> {
        let changed = sqlx::query(
            "UPDATE orders SET status = $3, updated_at = $4, updated_by = $5, version = version + 1 \
             WHERE id = $1 AND deleted_at IS NULL AND version = $2",
        )
        .bind(order.id)
        .bind(order.version)
        .bind(order.status.as_str())
        .bind(order.updated_at)
        .bind(&order.updated_by)
        .execute(self.conn())
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(concurrent_modification());
        }
        self.sync_items(order).await?;
        Ok(Order { version: order.version + 1, ..order.clone() })
    }

    async fn order_soft_delete(&mut self, id: Uuid, actor: &str, at: OffsetDateTime) -> Result<()> {
        let changed = sqlx::query(
            "UPDATE orders SET deleted_at = $2, deleted_by = $3, updated_at = $2, updated_by = $3, \
             version = version + 1 WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(at)
        .bind(actor)
        .execute(self.conn())
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(order_not_found(id));
        }
        Ok(())
    }

    async fn has_active_orders(&mut self, customer_id: Uuid) -> Result<bool> {
        let found = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM orders WHERE customer_id = $1 AND deleted_at IS NULL)",
        )
        .bind(customer_id)
        .fetch_one(self.conn())
        .await?;
        Ok(found)
    }

    async fn order_search(&mut self, request: &SearchRequest) -> Result<Page<Order>> {
        let plan = ORDERS.plan(request)?;
        let mut count = QueryBuilder::<Postgres>::new("SELECT count(*) FROM orders");
        push_where(&mut count, &plan);
        let total: i64 = count.build_query_scalar().fetch_one(self.conn()).await?;

        let mut select = QueryBuilder::<Postgres>::new(format!("SELECT {ORDER_COLUMNS} FROM orders"));
        push_where(&mut select, &plan);
        push_order(&mut select, plan.sort);
        select.push(" LIMIT ").push_bind(i64::from(request.size)).push(" OFFSET ").push_bind(request.offset());
        let rows: Vec<OrderRow> = select.build_query_as().fetch_all(self.conn()).await?;

        let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
        let mut items = if ids.is_empty() { Vec::new() } else { self.items_of(&ids).await? };
        let mut orders = Vec::with_capacity(rows.len());
        for row in rows {
            let (mine, rest): (Vec<Item>, Vec<Item>) = items.into_iter().partition(|item| item.order_id == row.id);
            items = rest;
            orders.push(row.into_order(mine)?);
        }
        Ok(Page { items: orders, total, page: request.page, size: request.size })
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        if let Conn::Tx(transaction) = self.conn {
            transaction.commit().await?;
        }
        Ok(())
    }
}
