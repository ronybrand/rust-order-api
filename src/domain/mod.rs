//! The domain: plain types and the rules that hold for every caller. No I/O, no framework.

pub mod customer;
pub mod email;
pub mod event;
pub mod order;

pub use customer::{Customer, CustomerInput, MAX_ACTOR_LENGTH, canonical_tax_id};
pub use email::is_valid_email;
pub use event::OrderStatusChanged;
pub use order::{Item, ItemInput, Order, OrderStatus, StatusChange};
