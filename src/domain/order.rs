use std::fmt;
use std::str::FromStr;

use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AppError, codes};

// Order rules (DOMAIN.md section 4).
pub const MAX_ITEMS_PER_ORDER: usize = 200;
pub const MAX_ITEM_DESCRIPTION: usize = 255;
pub const MAX_QUANTITY: i32 = 10_000;
const MONEY_SCALE: u32 = 2;

/// The largest value the `numeric(10,2)` column holds.
pub fn max_unit_price() -> Decimal {
    Decimal::new(9_999_999_999, MONEY_SCALE)
}

/// The lifecycle of an order: OPEN to CONFIRMED to CANCELED, or OPEN to CANCELED.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderStatus {
    Open,
    Confirmed,
    Canceled,
}

impl OrderStatus {
    pub const ALL: [Self; 3] = [Self::Open, Self::Confirmed, Self::Canceled];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Confirmed => "CONFIRMED",
            Self::Canceled => "CANCELED",
        }
    }
}

impl fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OrderStatus {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        Self::ALL.into_iter().find(|status| status.as_str() == text).ok_or(())
    }
}

/// An order line. Its subtotal is derived, never stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: Uuid,
    pub order_id: Uuid,
    pub description: String,
    pub unit_price: Decimal,
    pub quantity: i32,
}

impl Item {
    pub fn subtotal(&self) -> Decimal {
        self.unit_price * Decimal::from(self.quantity)
    }
}

/// The data a client supplies for a new item.
#[derive(Debug, Clone)]
pub struct ItemInput {
    pub description: String,
    pub unit_price: Decimal,
    pub quantity: i32,
}

impl ItemInput {
    fn validate(&self) -> Result<(), AppError> {
        if self.description.trim().is_empty() {
            return Err(AppError::invalid_field("description", "description must not be blank"));
        }
        if self.description.chars().count() > MAX_ITEM_DESCRIPTION {
            return Err(AppError::invalid_field("description", "description is too long"));
        }
        if self.unit_price <= Decimal::ZERO {
            return Err(AppError::invalid_field("unit_price", "unit_price must be greater than zero"));
        }
        if self.unit_price > max_unit_price() {
            return Err(AppError::invalid_field("unit_price", "unit_price is too large"));
        }
        if self.unit_price.normalize().scale() > MONEY_SCALE {
            return Err(AppError::invalid_field("unit_price", "unit_price must have at most 2 decimal places"));
        }
        validate_quantity(self.quantity)
    }

    fn into_item(self, order_id: Uuid) -> Item {
        let mut unit_price = self.unit_price;
        unit_price.rescale(MONEY_SCALE);
        Item { id: Uuid::new_v4(), order_id, description: self.description, unit_price, quantity: self.quantity }
    }
}

fn validate_quantity(quantity: i32) -> Result<(), AppError> {
    if !(1..=MAX_QUANTITY).contains(&quantity) {
        return Err(AppError::invalid_field("quantity", "quantity must be between 1 and 10000"));
    }
    Ok(())
}

/// The result of a status transition: the order after it and the status it had before.
#[derive(Debug)]
pub struct StatusChange {
    pub order: Order,
    pub previous: OrderStatus,
}

/// The aggregate root: it owns its items and keeps the total consistent with them.
///
/// Every change consumes the order and returns the new state, so the old one cannot be used by mistake, and a failed
/// rule never leaves a half-changed order.
///
/// `version` is the optimistic-concurrency token: a write that changes status or items must carry the version it
/// read, and the repository fails with CONFLICT-00 if the row moved on.
#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub status: OrderStatus,
    pub items: Vec<Item>,
    pub version: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub created_by: String,
    pub updated_by: String,
    pub deleted_at: Option<OffsetDateTime>,
    pub deleted_by: Option<String>,
}

impl Order {
    /// Builds an OPEN order from the requested items (rules 5 and 6).
    pub fn create(
        customer_id: Uuid,
        items: Vec<ItemInput>,
        actor: &str,
        now: OffsetDateTime,
    ) -> Result<Self, AppError> {
        if items.len() > MAX_ITEMS_PER_ORDER {
            return Err(item_limit_error());
        }
        for item in &items {
            item.validate()?;
        }
        let id = Uuid::new_v4();
        Ok(Self {
            id,
            customer_id,
            status: OrderStatus::Open,
            items: items.into_iter().map(|item| item.into_item(id)).collect(),
            version: 1,
            created_at: now,
            updated_at: now,
            created_by: actor.to_owned(),
            updated_by: actor.to_owned(),
            deleted_at: None,
            deleted_by: None,
        })
    }

    /// Derived from the current items on every call (rule 1): there is no stored total to go stale.
    pub fn total(&self) -> Decimal {
        let sum: Decimal = self.items.iter().map(Item::subtotal).sum();
        let mut total = sum.round_dp_with_strategy(MONEY_SCALE, RoundingStrategy::MidpointAwayFromZero);
        total.rescale(MONEY_SCALE);
        total
    }

    /// Items may still change (rule 2).
    pub fn is_editable(&self) -> bool {
        self.status == OrderStatus::Open
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    pub fn add_item(mut self, input: ItemInput, actor: &str, now: OffsetDateTime) -> Result<Self, AppError> {
        self.require_editable()?;
        if self.items.len() >= MAX_ITEMS_PER_ORDER {
            return Err(item_limit_error());
        }
        input.validate()?;
        self.items.push(input.into_item(self.id));
        Ok(self.touched(actor, now))
    }

    pub fn update_item_quantity(
        mut self,
        item_id: Uuid,
        quantity: i32,
        actor: &str,
        now: OffsetDateTime,
    ) -> Result<Self, AppError> {
        self.require_editable()?;
        let index = self.index_of(item_id)?;
        validate_quantity(quantity)?;
        self.items[index].quantity = quantity;
        Ok(self.touched(actor, now))
    }

    pub fn remove_item(mut self, item_id: Uuid, actor: &str, now: OffsetDateTime) -> Result<Self, AppError> {
        self.require_editable()?;
        let index = self.index_of(item_id)?;
        self.items.remove(index);
        Ok(self.touched(actor, now))
    }

    /// OPEN to CONFIRMED (rule 3).
    pub fn confirm(self, actor: &str, now: OffsetDateTime) -> Result<StatusChange, AppError> {
        if self.status != OrderStatus::Open {
            return Err(AppError::validation(codes::INVALID_TRANSITION, "cannot confirm an order that is not OPEN")
                .with("id", self.id)
                .with("status", self.status));
        }
        if self.items.is_empty() {
            return Err(AppError::validation(codes::ORDER_EMPTY, "an order without items cannot be confirmed")
                .with("id", self.id));
        }
        Ok(self.transition(OrderStatus::Confirmed, actor, now))
    }

    /// OPEN or CONFIRMED to CANCELED (rule 4).
    pub fn cancel(self, actor: &str, now: OffsetDateTime) -> Result<StatusChange, AppError> {
        if self.status == OrderStatus::Canceled {
            return Err(AppError::validation(codes::ALREADY_CANCELED, "order is already canceled")
                .with("id", self.id)
                .with("status", self.status));
        }
        Ok(self.transition(OrderStatus::Canceled, actor, now))
    }

    fn transition(mut self, to: OrderStatus, actor: &str, now: OffsetDateTime) -> StatusChange {
        let previous = self.status;
        self.status = to;
        StatusChange { order: self.touched(actor, now), previous }
    }

    fn touched(mut self, actor: &str, now: OffsetDateTime) -> Self {
        actor.clone_into(&mut self.updated_by);
        self.updated_at = now;
        self
    }

    fn require_editable(&self) -> Result<(), AppError> {
        if self.is_editable() {
            return Ok(());
        }
        Err(AppError::validation(codes::ORDER_NOT_EDITABLE, "order is not editable in its current status")
            .with("id", self.id)
            .with("status", self.status))
    }

    fn index_of(&self, item_id: Uuid) -> Result<usize, AppError> {
        self.items.iter().position(|item| item.id == item_id).ok_or_else(|| {
            AppError::not_found(codes::ITEM_NOT_FOUND, "item not found in this order")
                .with("order_id", self.id)
                .with("item_id", item_id)
        })
    }
}

fn item_limit_error() -> AppError {
    AppError::validation(codes::ITEM_LIMIT, "an order can have at most 200 items").with("max", MAX_ITEMS_PER_ORDER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn money(text: &str) -> Decimal {
        Decimal::from_str_exact(text).unwrap()
    }

    fn item(description: &str, price: &str, quantity: i32) -> ItemInput {
        ItemInput { description: description.into(), unit_price: money(price), quantity }
    }

    fn open_order(items: Vec<ItemInput>) -> Order {
        Order::create(Uuid::new_v4(), items, "user-1", now()).unwrap()
    }

    fn code(result: Result<impl std::fmt::Debug, AppError>) -> &'static str {
        result.unwrap_err().code
    }

    #[test]
    fn a_new_order_is_open_at_version_one_with_the_audit() {
        let order = open_order(vec![item("Pen", "1.50", 2)]);
        assert_eq!(order.status, OrderStatus::Open);
        assert_eq!(order.version, 1);
        assert_eq!((order.created_by.as_str(), order.updated_at), ("user-1", now()));
        assert_eq!(order.items[0].order_id, order.id);
        assert!(order.is_editable() && !order.is_deleted());
    }

    #[test]
    fn the_total_is_the_exact_sum_with_two_places() {
        let order = open_order(vec![item("Pen", "0.10", 3), item("Book", "19.99", 2)]);
        assert_eq!(order.total().to_string(), "40.28");
        assert_eq!(order.items[0].subtotal().to_string(), "0.30");
        assert_eq!(open_order(vec![]).total().to_string(), "0.00");
    }

    #[test]
    fn money_is_never_a_float() {
        let order = open_order(vec![item("Cent", "0.01", 10_000), item("Tenth", "0.10", 3)]);
        assert_eq!(order.total().to_string(), "100.30");
    }

    #[test]
    fn a_price_with_fewer_places_is_stored_with_two() {
        let order = open_order(vec![item("Pen", "2.5", 1), item("Cup", "3", 1), item("Mug", "4.500", 1)]);
        let prices: Vec<String> = order.items.iter().map(|i| i.unit_price.to_string()).collect();
        assert_eq!(prices, ["2.50", "3.00", "4.50"]);
    }

    #[test]
    fn item_validation_names_the_field() {
        let cases = [
            (item("  ", "1.00", 1), "description"),
            (item(&"a".repeat(256), "1.00", 1), "description"),
            (item("x", "0", 1), "unit_price"),
            (item("x", "-1", 1), "unit_price"),
            (item("x", "100000000.00", 1), "unit_price"),
            (item("x", "1.001", 1), "unit_price"),
            (item("x", "1.00", 0), "quantity"),
            (item("x", "1.00", 10_001), "quantity"),
            (item("x", "1.00", -5), "quantity"),
        ];
        for (input, field) in cases {
            let error = Order::create(Uuid::new_v4(), vec![input], "u", now()).unwrap_err();
            assert_eq!((error.code, error.params["field"].as_str()), ("VALIDATION-01", Some(field)));
        }
    }

    #[test]
    fn the_limits_are_inclusive() {
        assert!(Order::create(Uuid::new_v4(), vec![item("x", "99999999.99", 10_000)], "u", now()).is_ok());
        assert!(Order::create(Uuid::new_v4(), vec![item(&"a".repeat(255), "0.01", 1)], "u", now()).is_ok());
        assert!(Order::create(Uuid::new_v4(), vec![item("x", "1.10", 1)], "u", now()).is_ok());
        assert!(Order::create(Uuid::new_v4(), vec![item("x", "1.100", 1)], "u", now()).is_ok());
    }

    #[test]
    fn at_most_two_hundred_items() {
        let many = |n| (0..n).map(|i| item(&format!("i{i}"), "1.00", 1)).collect::<Vec<_>>();
        assert!(Order::create(Uuid::new_v4(), many(200), "u", now()).is_ok());
        assert_eq!(code(Order::create(Uuid::new_v4(), many(201), "u", now())), "VALIDATION-09");
        let full = Order::create(Uuid::new_v4(), many(200), "u", now()).unwrap();
        assert_eq!(code(full.add_item(item("one more", "1.00", 1), "u", now())), "VALIDATION-09");
    }

    #[test]
    fn items_can_be_added_changed_and_removed_while_open() {
        let order = open_order(vec![item("Pen", "1.00", 1)]);
        let later = now() + time::Duration::minutes(5);
        let order = order.add_item(item("Book", "10.00", 2), "user-2", later).unwrap();
        assert_eq!(order.items.len(), 2);
        assert_eq!((order.updated_by.as_str(), order.updated_at), ("user-2", later));
        assert_eq!(order.created_by, "user-1");

        let book = order.items[1].id;
        let order = order.update_item_quantity(book, 3, "u", later).unwrap();
        assert_eq!(order.total().to_string(), "31.00");

        let order = order.remove_item(book, "u", later).unwrap();
        assert_eq!(order.items.len(), 1);
        assert_eq!(order.total().to_string(), "1.00");
    }

    #[test]
    fn an_unknown_item_is_a_404_that_names_both_ids() {
        let order = open_order(vec![item("Pen", "1.00", 1)]);
        let missing = Uuid::new_v4();
        let error = order.clone().remove_item(missing, "u", now()).unwrap_err();
        assert_eq!(error.code, "RESOURCE-NOT-FOUND-03");
        assert_eq!(error.params["item_id"], missing.to_string());
        assert_eq!(error.params["order_id"], order.id.to_string());
        assert_eq!(code(order.update_item_quantity(missing, 2, "u", now())), "RESOURCE-NOT-FOUND-03");
    }

    #[test]
    fn a_bad_quantity_change_is_refused_without_touching_the_order() {
        let order = open_order(vec![item("Pen", "1.00", 1)]);
        let id = order.items[0].id;
        assert_eq!(code(order.update_item_quantity(id, 0, "u", now())), "VALIDATION-01");
    }

    #[test]
    fn confirming_moves_open_to_confirmed_and_reports_the_previous_status() {
        let change = open_order(vec![item("Pen", "1.00", 1)]).confirm("admin", now()).unwrap();
        assert_eq!((change.previous, change.order.status), (OrderStatus::Open, OrderStatus::Confirmed));
        assert_eq!(change.order.updated_by, "admin");
    }

    #[test]
    fn an_empty_order_cannot_be_confirmed() {
        assert_eq!(code(open_order(vec![]).confirm("a", now())), "VALIDATION-03");
    }

    #[test]
    fn only_an_open_order_can_be_confirmed() {
        let confirmed = open_order(vec![item("Pen", "1.00", 1)]).confirm("a", now()).unwrap().order;
        let error = confirmed.clone().confirm("a", now()).unwrap_err();
        assert_eq!((error.code, error.params["status"].as_str()), ("VALIDATION-04", Some("CONFIRMED")));
        let canceled = confirmed.cancel("a", now()).unwrap().order;
        assert_eq!(code(canceled.confirm("a", now())), "VALIDATION-04");
    }

    #[test]
    fn open_and_confirmed_orders_can_be_canceled_once() {
        let open = open_order(vec![]);
        let change = open.cancel("a", now()).unwrap();
        assert_eq!((change.previous, change.order.status), (OrderStatus::Open, OrderStatus::Canceled));
        assert_eq!(code(change.order.cancel("a", now())), "VALIDATION-08");

        let confirmed = open_order(vec![item("Pen", "1.00", 1)]).confirm("a", now()).unwrap().order;
        let change = confirmed.cancel("a", now()).unwrap();
        assert_eq!((change.previous, change.order.status), (OrderStatus::Confirmed, OrderStatus::Canceled));
    }

    #[test]
    fn items_are_frozen_outside_open() {
        let confirmed = open_order(vec![item("Pen", "1.00", 1)]).confirm("a", now()).unwrap().order;
        let id = confirmed.items[0].id;
        assert_eq!(code(confirmed.clone().add_item(item("x", "1.00", 1), "a", now())), "VALIDATION-02");
        assert_eq!(code(confirmed.clone().update_item_quantity(id, 2, "a", now())), "VALIDATION-02");
        assert_eq!(code(confirmed.remove_item(id, "a", now())), "VALIDATION-02");
    }

    #[test]
    fn the_status_round_trips_through_its_wire_name() {
        for status in OrderStatus::ALL {
            assert_eq!(status.as_str().parse::<OrderStatus>(), Ok(status));
            assert_eq!(serde_json::to_string(&status).unwrap(), format!("\"{status}\""));
        }
        assert!("open".parse::<OrderStatus>().is_err());
    }
}
