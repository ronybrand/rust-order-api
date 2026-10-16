use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::{AppError, codes};

// Limits of a search request: one request must not carry hundreds of predicates.
pub const DEFAULT_PAGE_SIZE: i32 = 20;
pub const MAX_PAGE_SIZE: i32 = 100;
pub const MAX_FILTER_FIELDS: usize = 10;
pub const MAX_CONDITIONS_PER_FIELD: usize = 10;
pub const MAX_IN_VALUES: usize = 100;
pub const MAX_FILTER_VALUE_LENGTH: usize = 200;

/// One predicate on a field, e.g. `gte` with the value 10. An empty `op` means `eq`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Condition {
    pub op: String,
    pub value: Option<Value>,
}

/// The filter, sort and page of a search. `page` is 0-based; `sort` is a field name with a `-` prefix for descending
/// order. Filter values are plain JSON values; numbers keep their digits exactly as sent.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchRequest {
    pub page: i32,
    pub size: i32,
    pub sort: String,
    pub filters: BTreeMap<String, Vec<Condition>>,
}

impl SearchRequest {
    /// The number of rows to skip.
    pub fn offset(&self) -> i64 {
        i64::from(self.page) * i64::from(self.size)
    }

    /// Fills in the page size when the client left it out.
    #[must_use]
    pub fn with_defaults(mut self) -> Self {
        if self.size == 0 {
            self.size = DEFAULT_PAGE_SIZE;
        }
        self
    }

    /// Checks the structural limits of the request. Field names and value types are checked later, against the
    /// columns of the entity being searched (they are known only to the repository).
    pub fn validate(&self) -> Result<(), AppError> {
        if self.page < 0 {
            return Err(AppError::invalid_field("page", "page must not be negative"));
        }
        if !(1..=MAX_PAGE_SIZE).contains(&self.size) {
            return Err(AppError::invalid_field("size", "size must be between 1 and 100"));
        }
        if self.offset() > i64::from(i32::MAX) {
            return Err(AppError::invalid_field("page", "page is too large"));
        }
        if self.filters.len() > MAX_FILTER_FIELDS {
            return Err(invalid_filter("too many filter fields", None));
        }
        for (field, conditions) in &self.filters {
            if conditions.len() > MAX_CONDITIONS_PER_FIELD {
                return Err(invalid_filter("too many conditions on one field", Some(field)));
            }
            if conditions.iter().any(|c| c.value.as_ref().is_some_and(|v| !within_value_limits(v))) {
                return Err(invalid_filter("filter value exceeds the allowed limits", Some(field)));
            }
        }
        Ok(())
    }
}

fn invalid_filter(message: &str, field: Option<&str>) -> AppError {
    let error = AppError::validation(codes::INVALID_FILTER, message);
    match field {
        Some(field) => error.with("field", field),
        None => error,
    }
}

fn within_value_limits(value: &Value) -> bool {
    match value {
        Value::String(text) => text.chars().count() <= MAX_FILTER_VALUE_LENGTH,
        Value::Array(values) => values.len() <= MAX_IN_VALUES && values.iter().all(within_value_limits),
        _ => true,
    }
}

/// One page of results plus the total number of matches.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: i32,
    pub size: i32,
}

impl<T> Page<T> {
    pub fn map<U>(self, convert: impl FnMut(T) -> U) -> Page<U> {
        Page {
            items: self.items.into_iter().map(convert).collect(),
            total: self.total,
            page: self.page,
            size: self.size,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn request() -> SearchRequest {
        SearchRequest { page: 0, size: 20, ..SearchRequest::default() }
    }

    fn with_filter(field: &str, conditions: Vec<Condition>) -> SearchRequest {
        SearchRequest { filters: BTreeMap::from([(field.to_owned(), conditions)]), ..request() }
    }

    fn eq(value: Value) -> Condition {
        Condition { op: String::new(), value: Some(value) }
    }

    #[test]
    fn a_missing_size_becomes_the_default_and_the_offset_follows() {
        let request = SearchRequest { page: 3, ..SearchRequest::default() }.with_defaults();
        assert_eq!((request.size, request.offset()), (20, 60));
        let explicit = SearchRequest { size: 5, ..SearchRequest::default() }.with_defaults();
        assert_eq!(explicit.size, 5);
    }

    #[test]
    fn the_page_and_size_are_bounded() {
        assert!(request().validate().is_ok());
        for (page, size, field) in [(-1, 20, "page"), (0, 0, "size"), (0, 101, "size"), (0, -1, "size")] {
            let error = SearchRequest { page, size, ..request() }.validate().unwrap_err();
            assert_eq!((error.code, error.params["field"].as_str()), ("VALIDATION-01", Some(field)));
        }
        let far = SearchRequest { page: i32::MAX, size: 100, ..request() }.validate().unwrap_err();
        assert_eq!(far.params["field"], "page");
        assert!(SearchRequest { page: 21_474_836, size: 100, ..request() }.validate().is_ok());
    }

    #[test]
    fn too_many_fields_or_conditions_are_refused() {
        let many_fields =
            SearchRequest { filters: (0..11).map(|i| (format!("f{i}"), vec![eq(json!(1))])).collect(), ..request() };
        assert_eq!(many_fields.validate().unwrap_err().code, "VALIDATION-05");
        let many_conditions = with_filter("name", (0..11).map(|_| eq(json!("a"))).collect());
        let error = many_conditions.validate().unwrap_err();
        assert_eq!((error.code, error.params["field"].as_str()), ("VALIDATION-05", Some("name")));
        assert!(with_filter("name", (0..10).map(|_| eq(json!("a"))).collect()).validate().is_ok());
    }

    #[test]
    fn values_are_bounded_in_length_and_in_count() {
        let long = with_filter("name", vec![eq(json!("a".repeat(201)))]);
        assert_eq!(long.validate().unwrap_err().code, "VALIDATION-05");
        assert!(with_filter("name", vec![eq(json!("a".repeat(200)))]).validate().is_ok());
        let many = with_filter("id", vec![Condition { op: "in".into(), value: Some(json!(vec!["x"; 101])) }]);
        assert_eq!(many.validate().unwrap_err().code, "VALIDATION-05");
        let nested = with_filter("id", vec![Condition { op: "in".into(), value: Some(json!([["a".repeat(201)]])) }]);
        assert_eq!(nested.validate().unwrap_err().code, "VALIDATION-05");
        assert!(with_filter("id", vec![Condition { op: "in".into(), value: None }]).validate().is_ok());
    }

    #[test]
    fn a_page_maps_its_items_and_keeps_the_counts() {
        let page = Page { items: vec![1, 2], total: 7, page: 1, size: 2 }.map(|n| n * 10);
        assert_eq!(page, Page { items: vec![10, 20], total: 7, page: 1, size: 2 });
    }
}
