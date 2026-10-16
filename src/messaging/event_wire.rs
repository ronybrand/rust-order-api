//! The JSON form of [`OrderStatusChanged`]: the wire contract of the notification queue, shared by the API (producer)
//! and the worker (consumer). The amount travels as text so no consumer ever parses it as a float.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::domain::{OrderStatus, OrderStatusChanged};

const MONEY_SCALE: u32 = 2;

#[derive(Serialize, Deserialize)]
struct Payload {
    order_id: String,
    customer_email: String,
    customer_name: String,
    old_status: OrderStatus,
    new_status: OrderStatus,
    total_amount: String,
    changed_at: String,
}

pub fn encode(event: &OrderStatusChanged) -> Vec<u8> {
    let mut total = event.total_amount;
    total.rescale(MONEY_SCALE);
    let payload = Payload {
        order_id: event.order_id.to_string(),
        customer_email: event.customer_email.clone(),
        customer_name: event.customer_name.clone(),
        old_status: event.old_status,
        new_status: event.new_status,
        total_amount: total.to_string(),
        // Formatting a valid instant as RFC 3339 cannot fail; the epoch is only the answer to an impossible case.
        changed_at: event.changed_at.format(&Rfc3339).unwrap_or_else(|_| "1970-01-01T00:00:00Z".into()),
    };
    serde_json::to_vec(&payload).unwrap_or_default()
}

/// `None` for anything that is not a valid event: the caller dead-letters it, retrying cannot fix it. Malformed JSON,
/// a missing field, an unknown status, a bad UUID or number or timestamp are all the same for the caller.
pub fn decode(body: &[u8]) -> Option<OrderStatusChanged> {
    let payload: Payload = serde_json::from_slice(body).ok()?;
    Some(OrderStatusChanged {
        order_id: Uuid::parse_str(&payload.order_id).ok()?,
        customer_email: payload.customer_email,
        customer_name: payload.customer_name,
        old_status: payload.old_status,
        new_status: payload.new_status,
        total_amount: Decimal::from_str_exact(&payload.total_amount).ok()?,
        changed_at: OffsetDateTime::parse(&payload.changed_at, &Rfc3339).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn event() -> OrderStatusChanged {
        OrderStatusChanged {
            order_id: Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
            customer_email: "ada@example.com".into(),
            customer_name: "Ada \"The\" Lovelace".into(),
            old_status: OrderStatus::Open,
            new_status: OrderStatus::Confirmed,
            total_amount: Decimal::from_str_exact("10.3").unwrap(),
            changed_at: OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        }
    }

    fn valid_json() -> Value {
        json!({
            "order_id": "123e4567-e89b-42d3-a456-426614174000",
            "customer_email": "ada@example.com",
            "customer_name": "Ada",
            "old_status": "OPEN",
            "new_status": "CONFIRMED",
            "total_amount": "10.30",
            "changed_at": "2027-01-15T08:00:00Z",
        })
    }

    #[test]
    fn the_wire_form_has_snake_case_names_and_the_amount_as_text() {
        let wire: Value = serde_json::from_slice(&encode(&event())).unwrap();
        assert_eq!(wire["order_id"], "123e4567-e89b-42d3-a456-426614174000");
        assert_eq!(wire["total_amount"], "10.30");
        assert_eq!((wire["old_status"].clone(), wire["new_status"].clone()), (json!("OPEN"), json!("CONFIRMED")));
        assert_eq!(wire["changed_at"], "2027-01-15T08:00:00Z");
        assert_eq!(wire.as_object().unwrap().len(), 7);
    }

    #[test]
    fn an_event_survives_the_round_trip() {
        let mut original = event();
        original.total_amount.rescale(2);
        assert_eq!(decode(&encode(&original)), Some(original));
    }

    #[test]
    fn unknown_fields_are_ignored_so_the_producer_can_add_one() {
        let mut body = valid_json();
        body["added_later"] = json!(true);
        assert!(decode(body.to_string().as_bytes()).is_some());
    }

    #[test]
    fn anything_that_is_not_a_valid_event_decodes_to_none() {
        assert!(decode(valid_json().to_string().as_bytes()).is_some());
        assert!(decode(b"").is_none());
        assert!(decode(b"not json").is_none());
        assert!(decode(b"[]").is_none());
        for (key, bad) in [
            ("order_id", json!("nope")),
            ("order_id", json!(7)),
            ("old_status", json!("SHIPPED")),
            ("new_status", json!("open")),
            ("total_amount", json!("ten")),
            ("total_amount", json!(10.3)),
            ("changed_at", json!("yesterday")),
            ("customer_email", Value::Null),
        ] {
            let mut body = valid_json();
            body[key] = bad;
            assert!(decode(body.to_string().as_bytes()).is_none(), "{key}");
        }
        for key in
            ["order_id", "customer_email", "customer_name", "old_status", "new_status", "total_amount", "changed_at"]
        {
            let mut body = valid_json();
            body.as_object_mut().unwrap().remove(key);
            assert!(decode(body.to_string().as_bytes()).is_none(), "missing {key}");
        }
    }
}
