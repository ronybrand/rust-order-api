//! The JSON contract: `snake_case`, money as numbers, no personal data on customers.
//!
//! The wire types are separate from the domain types on purpose, so the contract does not change because a domain type
//! did. Decoding is strict: unknown keys, trailing data and wrong types are all errors, never silently accepted.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::app::search::{Condition, Page, SearchRequest};
use crate::domain::{Customer, CustomerInput, Item, ItemInput, Order, OrderStatus};
use crate::error::AppError;
use crate::persistence::plan::parse_canonical_uuid;

const MONEY_SCALE: u32 = 2;

fn invalid_body(message: &str) -> AppError {
    AppError::invalid_field("body", message)
}

/// Money is read and written as a JSON number with the digits exactly as sent: no float is ever involved. Quoted
/// numbers are refused, and a number that does not fit a decimal is refused by the parse itself.
pub mod money {
    use rust_decimal::{Decimal, RoundingStrategy};
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser};
    use serde_json::Number;

    use super::MONEY_SCALE;

    pub fn parse(digits: &str) -> Option<Decimal> {
        Decimal::from_str_exact(digits).or_else(|_| Decimal::from_scientific(digits)).ok()
    }

    pub fn serialize<S: Serializer>(value: &Decimal, serializer: S) -> Result<S::Ok, S::Error> {
        let mut scaled = value.round_dp_with_strategy(MONEY_SCALE, RoundingStrategy::MidpointAwayFromZero);
        scaled.rescale(MONEY_SCALE);
        // serde_json keeps the digits of a number it parsed from text (the `arbitrary_precision` feature).
        let number: Number = serde_json::from_str(&scaled.to_string()).map_err(ser::Error::custom)?;
        number.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
        let number = Number::deserialize(deserializer)?;
        parse(&number.to_string()).ok_or_else(|| de::Error::custom("expected a number"))
    }
}

/// A UUID in its canonical 8-4-4-4-12 spelling and no other.
mod canonical_uuid {
    use serde::{Deserialize, Deserializer, de};
    use uuid::Uuid;

    use super::parse_canonical_uuid;

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_canonical_uuid(&text).ok_or_else(|| de::Error::custom("expected a UUID"))
    }
}

// --- requests -----------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomerRequest {
    pub name: String,
    pub tax_id: String,
    #[serde(default)]
    pub passport_number: Option<String>,
    pub email: String,
}

impl From<CustomerRequest> for CustomerInput {
    fn from(request: CustomerRequest) -> Self {
        Self {
            name: request.name,
            tax_id: request.tax_id,
            passport_number: request.passport_number,
            email: request.email,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRequest {
    pub description: String,
    #[serde(with = "money")]
    pub unit_price: Decimal,
    pub quantity: i32,
}

impl From<ItemRequest> for ItemInput {
    fn from(request: ItemRequest) -> Self {
        Self { description: request.description, unit_price: request.unit_price, quantity: request.quantity }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderRequest {
    #[serde(deserialize_with = "canonical_uuid::deserialize")]
    pub customer_id: Uuid,
    #[serde(default)]
    pub items: Vec<ItemRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuantityRequest {
    pub quantity: i32,
}

// --- responses ----------------------------------------------------------------------------------------------

/// Only id and name: tax id, passport and e-mail are personal data, and no consumer needs them back.
#[derive(Debug, Serialize)]
pub struct CustomerResponse {
    pub id: Uuid,
    pub name: String,
}

impl From<Customer> for CustomerResponse {
    fn from(customer: Customer) -> Self {
        Self { id: customer.id, name: customer.name }
    }
}

#[derive(Debug, Serialize)]
pub struct ItemResponse {
    pub id: Uuid,
    pub order_id: Uuid,
    pub description: String,
    #[serde(with = "money")]
    pub unit_price: Decimal,
    pub quantity: i32,
    #[serde(with = "money")]
    pub subtotal: Decimal,
}

impl From<Item> for ItemResponse {
    fn from(item: Item) -> Self {
        Self {
            subtotal: item.subtotal(),
            id: item.id,
            order_id: item.order_id,
            description: item.description,
            unit_price: item.unit_price,
            quantity: item.quantity,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OrderResponse {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub status: OrderStatus,
    pub version: i64,
    pub items: Vec<ItemResponse>,
    #[serde(with = "money")]
    pub total: Decimal,
}

impl From<Order> for OrderResponse {
    fn from(order: Order) -> Self {
        Self {
            total: order.total(),
            id: order.id,
            customer_id: order.customer_id,
            status: order.status,
            version: order.version,
            items: order.items.into_iter().map(ItemResponse::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PageResponse<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: i32,
    pub size: i32,
}

impl<T> PageResponse<T> {
    pub fn from_page<D>(page: Page<D>) -> Self
    where
        T: From<D>,
    {
        let page = page.map(T::from);
        Self { items: page.items, total: page.total, page: page.page, size: page.size }
    }
}

// --- search body --------------------------------------------------------------------------------------------

const SEARCH_KEYS: [&str; 4] = ["page", "size", "sort", "filters"];
const CONDITION_KEYS: [&str; 2] = ["op", "value"];

/// Reads the POST form of a search from raw JSON: unknown keys and wrong shapes are VALIDATION-01.
pub fn parse_search_body(body: &[u8]) -> Result<SearchRequest, AppError> {
    let Value::Object(root) =
        serde_json::from_slice::<Value>(body).map_err(|_| invalid_body("request body is not valid JSON"))?
    else {
        return Err(invalid_body("request body must be a JSON object"));
    };
    if root.keys().any(|key| !SEARCH_KEYS.contains(&key.as_str())) {
        return Err(invalid_body("request body has unknown fields"));
    }
    Ok(SearchRequest {
        page: root.get("page").map(|v| int_of(v, "page")).transpose()?.unwrap_or(0),
        size: root.get("size").map(|v| int_of(v, "size")).transpose()?.unwrap_or(0),
        sort: root.get("sort").map(|v| string_of(v, "sort")).transpose()?.unwrap_or_default(),
        filters: root.get("filters").map(filters_of).transpose()?.unwrap_or_default(),
    })
}

fn int_of(value: &Value, field: &str) -> Result<i32, AppError> {
    match value {
        Value::Number(number) => {
            number.to_string().parse().map_err(|_| invalid_body(&format!("{field} must be an integer")))
        }
        _ => Err(invalid_body(&format!("{field} must be an integer"))),
    }
}

fn string_of(value: &Value, field: &str) -> Result<String, AppError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        _ => Err(invalid_body(&format!("{field} must be a string"))),
    }
}

fn filters_of(value: &Value) -> Result<BTreeMap<String, Vec<Condition>>, AppError> {
    let Value::Object(filters) = value else { return Err(invalid_body("filters must be an object")) };
    filters
        .iter()
        .map(|(field, conditions)| {
            let Value::Array(list) = conditions else {
                return Err(invalid_body("each filter must be a list of conditions"));
            };
            Ok((field.clone(), list.iter().map(condition_of).collect::<Result<Vec<_>, _>>()?))
        })
        .collect()
}

fn condition_of(value: &Value) -> Result<Condition, AppError> {
    let Value::Object(condition) = value else { return Err(invalid_body("each condition must be an object")) };
    if condition.keys().any(|key| !CONDITION_KEYS.contains(&key.as_str())) {
        return Err(invalid_body("a condition has unknown fields"));
    }
    Ok(Condition {
        op: condition.get("op").map(|v| string_of(v, "op")).transpose()?.unwrap_or_default(),
        // A JSON null is "no value", as if the key were absent.
        value: condition.get("value").filter(|v| !v.is_null()).cloned(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn search(body: &str) -> Result<SearchRequest, AppError> {
        parse_search_body(body.as_bytes())
    }

    fn rejected(body: &str) -> String {
        let error = search(body).unwrap_err();
        assert_eq!((error.code, error.params["field"].as_str()), ("VALIDATION-01", Some("body")), "{body}");
        error.message
    }

    fn render<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn money_is_written_as_a_number_with_two_places() {
        #[derive(Serialize)]
        struct Priced {
            #[serde(with = "money")]
            price: Decimal,
        }
        for (input, expected) in
            [("10.3", "10.30"), ("10", "10.00"), ("0.1", "0.10"), ("10.30", "10.30"), ("99999999.99", "99999999.99")]
        {
            let price = Decimal::from_str_exact(input).unwrap();
            assert_eq!(render(&Priced { price }), format!(r#"{{"price":{expected}}}"#));
        }
    }

    #[test]
    fn money_is_read_from_numbers_only_with_their_digits_intact() {
        #[derive(Deserialize)]
        struct Priced {
            #[serde(with = "money")]
            price: Decimal,
        }
        let read = |text: &str| serde_json::from_str::<Priced>(text).map(|p| p.price.to_string());
        assert_eq!(read(r#"{"price": 0.10}"#).unwrap(), "0.10");
        assert_eq!(read(r#"{"price": 19.99}"#).unwrap(), "19.99");
        assert_eq!(read(r#"{"price": 5}"#).unwrap(), "5");
        assert_eq!(read(r#"{"price": 1e2}"#).unwrap(), "100");
        assert!(read(r#"{"price": "10.00"}"#).is_err(), "a quoted number is not a number");
        assert!(read(r#"{"price": null}"#).is_err());
        assert!(read(r#"{"price": true}"#).is_err());
        assert!(read(r#"{"price": 1e400}"#).is_err());
        assert!(read(r#"{"price": 1e-400}"#).is_err() || read(r#"{"price": 1e-400}"#).unwrap() == "0");
    }

    #[test]
    fn a_customer_request_is_strict() {
        let ok = r#"{"name":"Ada","tax_id":"123.456","email":"a@b.co"}"#;
        assert!(serde_json::from_str::<CustomerRequest>(ok).is_ok());
        for bad in [
            r#"{"name":"Ada","tax_id":"123.456","email":"a@b.co","extra":1}"#,
            r#"{"name":"Ada","tax_id":"123.456"}"#,
            r#"{"name":1,"tax_id":"123.456","email":"a@b.co"}"#,
            r#"{"name":null,"tax_id":"123.456","email":"a@b.co"}"#,
            "[]",
            r#"{"name":"Ada","tax_id":"123.456","email":"a@b.co"} trailing"#,
            r#"{"name":"Ada","name":"Bob","tax_id":"123.456","email":"a@b.co"}"#,
        ] {
            assert!(serde_json::from_str::<CustomerRequest>(bad).is_err(), "{bad}");
        }
        let with_passport: CustomerRequest =
            serde_json::from_str(r#"{"name":"A","tax_id":"123.456","email":"a@b.co","passport_number":null}"#).unwrap();
        assert_eq!(with_passport.passport_number, None);
    }

    #[test]
    fn an_order_request_needs_a_canonical_customer_id() {
        let id = Uuid::new_v4();
        let ok = format!(r#"{{"customer_id":"{id}"}}"#);
        let request: OrderRequest = serde_json::from_str(&ok).unwrap();
        assert_eq!((request.customer_id, request.items.len()), (id, 0));
        for bad in [
            format!(r#"{{"customer_id":"{}"}}"#, id.simple()),
            format!(r#"{{"customer_id":"{{{id}}}"}}"#),
            r#"{"customer_id":"nope"}"#.to_owned(),
            r#"{"customer_id":5}"#.to_owned(),
            "{}".to_owned(),
        ] {
            assert!(serde_json::from_str::<OrderRequest>(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_item_request_reads_the_price_exactly_and_refuses_a_float_quantity() {
        let item: ItemRequest =
            serde_json::from_str(r#"{"description":"Pen","unit_price":0.10,"quantity":3}"#).unwrap();
        assert_eq!((item.unit_price.to_string().as_str(), item.quantity), ("0.10", 3));
        assert!(serde_json::from_str::<ItemRequest>(r#"{"description":"Pen","unit_price":1,"quantity":3.5}"#).is_err());
        assert!(serde_json::from_str::<ItemRequest>(r#"{"description":"Pen","unit_price":1,"quantity":"3"}"#).is_err());
        assert!(serde_json::from_str::<QuantityRequest>(r#"{"quantity":2,"extra":true}"#).is_err());
    }

    #[test]
    fn a_customer_response_has_only_id_and_name() {
        let response = CustomerResponse { id: Uuid::nil(), name: "Ada".into() };
        assert_eq!(render(&response), r#"{"id":"00000000-0000-0000-0000-000000000000","name":"Ada"}"#);
    }

    #[test]
    fn a_page_response_keeps_the_counts() {
        let page = Page { items: vec![1, 2], total: 9, page: 1, size: 2 };
        let response = PageResponse::<i64>::from_page(page);
        assert_eq!(render(&response), r#"{"items":[1,2],"total":9,"page":1,"size":2}"#);
    }

    #[test]
    fn an_empty_search_body_is_an_empty_request() {
        assert_eq!(search("{}").unwrap(), SearchRequest::default());
    }

    #[test]
    fn a_search_body_carries_page_size_sort_and_filters() {
        let request = search(
            r#"{"page":2,"size":10,"sort":"-name","filters":{"name":[{"op":"lk","value":"ada"}],"version":[{"value":1},{"op":"gte","value":2.50}]}}"#,
        )
        .unwrap();
        assert_eq!((request.page, request.size, request.sort.as_str()), (2, 10, "-name"));
        assert_eq!(request.filters["name"], [Condition { op: "lk".into(), value: Some(json!("ada")) }]);
        assert_eq!(request.filters["version"][0], Condition { op: String::new(), value: Some(json!(1)) });
        assert_eq!(request.filters["version"][1].value.as_ref().unwrap().to_string(), "2.50");
    }

    #[test]
    fn a_null_value_means_no_value() {
        let request = search(r#"{"filters":{"name":[{"op":"eq","value":null}]}}"#).unwrap();
        assert_eq!(request.filters["name"][0].value, None);
    }

    #[test]
    fn a_malformed_search_body_is_refused_with_a_reason() {
        assert_eq!(rejected("not json"), "request body is not valid JSON");
        assert_eq!(rejected(""), "request body is not valid JSON");
        assert_eq!(rejected("[]"), "request body must be a JSON object");
        assert_eq!(rejected(r#"{"extra":1}"#), "request body has unknown fields");
        assert_eq!(rejected(r#"{"page":"1"}"#), "page must be an integer");
        assert_eq!(rejected(r#"{"size":1.5}"#), "size must be an integer");
        assert_eq!(rejected(r#"{"page":99999999999}"#), "page must be an integer");
        assert_eq!(rejected(r#"{"sort":5}"#), "sort must be a string");
        assert_eq!(rejected(r#"{"filters":[]}"#), "filters must be an object");
        assert_eq!(rejected(r#"{"filters":{"name":{}}}"#), "each filter must be a list of conditions");
        assert_eq!(rejected(r#"{"filters":{"name":[1]}}"#), "each condition must be an object");
        assert_eq!(rejected(r#"{"filters":{"name":[{"op":"eq","oops":1}]}}"#), "a condition has unknown fields");
        assert_eq!(rejected(r#"{"filters":{"name":[{"op":5}]}}"#), "op must be a string");
    }

    #[test]
    fn invalid_utf8_is_not_valid_json() {
        let error = parse_search_body(&[b'{', 0xff, 0xfe, b'}']).unwrap_err();
        assert_eq!(error.message, "request body is not valid JSON");
    }
}
