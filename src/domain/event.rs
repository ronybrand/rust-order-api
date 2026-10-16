use std::fmt;

use rust_decimal::Decimal;
use time::OffsetDateTime;
use uuid::Uuid;

use super::order::OrderStatus;

/// Fired at the end of confirm and cancel, only when the customer has an e-mail (DOMAIN.md section 5). The JSON form
/// (see `messaging::event_wire`) is the wire contract of the notification queue, shared by the API (producer) and the
/// worker (consumer).
///
/// `Debug` keeps the customer's e-mail and name out of the logs: the order id is enough to trace the event.
#[derive(Clone, PartialEq)]
pub struct OrderStatusChanged {
    pub order_id: Uuid,
    pub customer_email: String,
    pub customer_name: String,
    pub old_status: OrderStatus,
    pub new_status: OrderStatus,
    pub total_amount: Decimal,
    pub changed_at: OffsetDateTime,
}

impl fmt::Debug for OrderStatusChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OrderStatusChanged(order_id={}, old_status={}, new_status={})",
            self.order_id, self.old_status, self.new_status
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_keeps_the_customer_out_of_the_logs() {
        let event = OrderStatusChanged {
            order_id: Uuid::nil(),
            customer_email: "ada@example.com".into(),
            customer_name: "Ada".into(),
            old_status: OrderStatus::Open,
            new_status: OrderStatus::Confirmed,
            total_amount: Decimal::new(1030, 2),
            changed_at: OffsetDateTime::UNIX_EPOCH,
        };
        let text = format!("{event:?}");
        assert!(text.contains("OPEN") && text.contains("CONFIRMED"));
        assert!(!text.contains("ada@example.com") && !text.contains("Ada"));
    }
}
