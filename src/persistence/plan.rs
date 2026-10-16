//! What a search request means for one entity, validated and typed, with no SQL in it.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, Time};
use uuid::Uuid;

use crate::app::search::{Condition, MAX_IN_VALUES, SearchRequest};
use crate::domain::OrderStatus;
use crate::error::{AppError, codes};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Text,
    Uuid,
    Int,
    Time,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    Between,
    Like,
}

impl Operator {
    fn parse(raw: &str, field: &str) -> Result<Self, AppError> {
        Ok(match raw.to_lowercase().as_str() {
            "eq" => Self::Eq,
            "neq" => Self::Neq,
            "gt" => Self::Gt,
            "gte" => Self::Gte,
            "lt" => Self::Lt,
            "lte" => Self::Lte,
            "in" => Self::In,
            "between" => Self::Between,
            "lk" => Self::Like,
            other => {
                return Err(AppError::validation(codes::INVALID_FILTER, "unknown operator")
                    .with("field", field)
                    .with("op", other));
            }
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Neq => "neq",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::In => "in",
            Self::Between => "between",
            Self::Like => "lk",
        }
    }
}

/// A value already converted to the type of its column.
#[derive(Debug, Clone, PartialEq)]
pub enum Typed {
    Text(String),
    Uuid(Uuid),
    Int(i64),
    Time(OffsetDateTime),
}

/// A validated predicate: the column, the operator and the values converted to the column's type.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub column: &'static str,
    pub operator: Operator,
    pub values: Vec<Typed>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub column: &'static str,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchPlan {
    pub predicates: Vec<Predicate>,
    pub sort: SortSpec,
}

/// The whitelist of what a search can filter and sort on. Only columns are listed (never the soft-delete markers): the
/// query only ever uses columns from here, and values are always bound parameters.
#[derive(Debug)]
pub struct SearchEntity {
    pub name: &'static str,
    /// Field name and type; the column has the same name.
    pub fields: &'static [(&'static str, FieldType)],
}

const STATUSES: &[&str] = &["OPEN", "CONFIRMED", "CANCELED"];

pub static CUSTOMERS: SearchEntity = SearchEntity {
    name: "customers",
    fields: &[
        ("id", FieldType::Uuid),
        ("name", FieldType::Text),
        ("tax_id", FieldType::Text),
        ("passport_number", FieldType::Text),
        ("email", FieldType::Text),
        ("version", FieldType::Int),
        ("created_at", FieldType::Time),
        ("updated_at", FieldType::Time),
        ("created_by", FieldType::Text),
        ("updated_by", FieldType::Text),
    ],
};

pub static ORDERS: SearchEntity = SearchEntity {
    name: "orders",
    fields: &[
        ("id", FieldType::Uuid),
        ("customer_id", FieldType::Uuid),
        ("status", FieldType::Enum(STATUSES)),
        ("version", FieldType::Int),
        ("created_at", FieldType::Time),
        ("updated_at", FieldType::Time),
        ("created_by", FieldType::Text),
        ("updated_by", FieldType::Text),
    ],
};

// The enum of the domain and the closed set above must agree; a test compares them.
const _: () = assert!(OrderStatus::ALL.len() == STATUSES.len());

impl SearchEntity {
    fn field(&self, name: &str) -> Option<(&'static str, FieldType)> {
        self.fields.iter().find(|(field, _)| *field == name).copied()
    }

    pub fn field_names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.fields.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names
    }

    /// Turns a request into a plan. Unknown filter fields are ignored (a client may send a superset of what an entity
    /// knows); an unknown sort field is VALIDATION-06.
    pub fn plan(&self, request: &SearchRequest) -> Result<SearchPlan, AppError> {
        let mut predicates = Vec::new();
        for (name, conditions) in &request.filters {
            let Some((column, kind)) = self.field(name) else { continue };
            for condition in conditions {
                predicates.push(predicate_of(name, column, kind, condition)?);
            }
        }
        Ok(SearchPlan { predicates, sort: self.sort_of(&request.sort)? })
    }

    fn sort_of(&self, spec: &str) -> Result<SortSpec, AppError> {
        if spec.is_empty() {
            return Ok(SortSpec { column: "created_at", descending: true });
        }
        let name = spec.strip_prefix('-').unwrap_or(spec);
        match self.field(name) {
            Some((column, _)) => Ok(SortSpec { column, descending: spec.starts_with('-') }),
            None => Err(AppError::validation(codes::UNKNOWN_SORT, "cannot sort by this field")
                .with("field", name)
                .with("allowed", self.field_names())),
        }
    }
}

fn predicate_of(
    name: &str,
    column: &'static str,
    kind: FieldType,
    condition: &Condition,
) -> Result<Predicate, AppError> {
    let op = if condition.op.is_empty() { "eq" } else { &condition.op };
    let operator = Operator::parse(op, name)?;
    PredicateBuilder { name, column, kind, operator, value: condition.value.as_ref() }.build()
}

const BETWEEN_VALUES: usize = 2;

/// Turns one condition into a [`Predicate`], rejecting what does not fit the field's type as VALIDATION-05.
struct PredicateBuilder<'a> {
    name: &'a str,
    column: &'static str,
    kind: FieldType,
    operator: Operator,
    value: Option<&'a Value>,
}

impl PredicateBuilder<'_> {
    fn build(&self) -> Result<Predicate, AppError> {
        match self.operator {
            Operator::Eq | Operator::Neq => self.single(),
            Operator::Gt | Operator::Gte | Operator::Lt | Operator::Lte => self.ordered()?.single(),
            Operator::In => self.in_list(),
            Operator::Between => {
                self.ordered()?;
                Ok(self.predicate(self.coerced_list(Some(BETWEEN_VALUES))?))
            }
            Operator::Like => self.like(),
        }
    }

    fn predicate(&self, values: Vec<Typed>) -> Predicate {
        Predicate { column: self.column, operator: self.operator, values }
    }

    fn single(&self) -> Result<Predicate, AppError> {
        Ok(self.predicate(vec![self.coerced(self.value)?]))
    }

    fn ordered(&self) -> Result<&Self, AppError> {
        if matches!(self.kind, FieldType::Enum(_)) {
            return Err(self.bad("ordering operators are not supported on this field"));
        }
        Ok(self)
    }

    fn in_list(&self) -> Result<Predicate, AppError> {
        let values = self.coerced_list(None)?;
        if values.is_empty() || values.len() > MAX_IN_VALUES {
            return Err(self.bad("in expects between 1 and 100 values"));
        }
        Ok(self.predicate(values))
    }

    fn like(&self) -> Result<Predicate, AppError> {
        if self.kind != FieldType::Text {
            return Err(self.bad("lk is only supported on text fields"));
        }
        let Some(Value::String(text)) = self.value else { return Err(self.bad("lk expects a string")) };
        Ok(self.predicate(vec![Typed::Text(format!("%{}%", escape_like(text)))]))
    }

    fn coerced(&self, raw: Option<&Value>) -> Result<Typed, AppError> {
        raw.and_then(|value| coerce(self.kind, value)).ok_or_else(|| self.bad(&expectation(self.kind)))
    }

    fn coerced_list(&self, exactly: Option<usize>) -> Result<Vec<Typed>, AppError> {
        let Some(Value::Array(list)) = self.value else { return Err(self.bad("expected a list of values")) };
        if let Some(count) = exactly
            && list.len() != count
        {
            return Err(self.bad(&format!("expected exactly {count} values")));
        }
        list.iter().map(|value| self.coerced(Some(value))).collect()
    }

    fn bad(&self, reason: &str) -> AppError {
        AppError::validation(codes::INVALID_FILTER, reason).with("field", self.name).with("op", self.operator.name())
    }
}

fn expectation(kind: FieldType) -> String {
    match kind {
        FieldType::Text => "expected a string".into(),
        FieldType::Uuid => "expected a UUID".into(),
        FieldType::Int => "expected an integer".into(),
        FieldType::Time => "expected an RFC 3339 timestamp or a YYYY-MM-DD date".into(),
        FieldType::Enum(allowed) => {
            let mut allowed = allowed.to_vec();
            allowed.sort_unstable();
            format!("expected one of {}", allowed.join(", "))
        }
    }
}

/// Converts a decoded JSON value to the type of the column, or `None` when it does not fit.
fn coerce(kind: FieldType, value: &Value) -> Option<Typed> {
    match (kind, value) {
        (FieldType::Text, Value::String(text)) => Some(Typed::Text(text.clone())),
        (FieldType::Uuid, Value::String(text)) => parse_canonical_uuid(text).map(Typed::Uuid),
        (FieldType::Int, Value::Number(number)) => integer(&number.to_string()).map(Typed::Int),
        (FieldType::Int, Value::String(text)) => text.parse().ok().map(Typed::Int),
        (FieldType::Time, Value::String(text)) => parse_time(text).map(Typed::Time),
        (FieldType::Enum(allowed), Value::String(text)) => {
            allowed.contains(&text.as_str()).then(|| Typed::Text(text.clone()))
        }
        _ => None,
    }
}

/// Only the canonical 8-4-4-4-12 form: the other spellings the `uuid` crate accepts (no hyphens, braces, URN) are not
/// what any client of this API sends, and accepting several spellings of one id invites inconsistent caching.
pub fn parse_canonical_uuid(text: &str) -> Option<Uuid> {
    if text.len() != 36 {
        return None;
    }
    Uuid::try_parse(text).ok()
}

/// A whole number written as `5`, `5.0` or `5e0`, but not `5.5`; `None` when it does not fit in 64 bits.
fn integer(digits: &str) -> Option<i64> {
    if let Ok(exact) = digits.parse::<i64>() {
        return Some(exact);
    }
    let decimal = Decimal::from_str_exact(digits).or_else(|_| Decimal::from_scientific(digits)).ok()?;
    if decimal.fract().is_zero() { decimal.to_i64() } else { None }
}

fn parse_time(text: &str) -> Option<OffsetDateTime> {
    if let Ok(instant) = OffsetDateTime::parse(text, &Rfc3339) {
        return Some(instant);
    }
    let format = time::macros::format_description!("[year]-[month]-[day]");
    Date::parse(text, &format).ok().map(|date| date.with_time(Time::MIDNIGHT).assume_utc())
}

/// Neutralizes the LIKE wildcards: "50%" must match the text "50%", not everything starting with 50.
pub fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Builds the filters of a request, for the tests of this module and of the SQL builder.
#[cfg(test)]
pub(crate) fn filters_of(pairs: &[(&str, Vec<Condition>)]) -> std::collections::BTreeMap<String, Vec<Condition>> {
    pairs.iter().map(|(name, conditions)| ((*name).to_owned(), conditions.clone())).collect()
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)] // the helpers take a condition the way the tests write it
mod tests {
    use serde_json::json;

    use super::*;

    fn cond(op: &str, value: Value) -> Condition {
        Condition { op: op.into(), value: Some(value) }
    }

    fn plan_of(entity: &SearchEntity, field: &str, condition: Condition) -> Result<SearchPlan, AppError> {
        entity.plan(&SearchRequest { filters: filters_of(&[(field, vec![condition])]), ..SearchRequest::default() })
    }

    fn bad(entity: &SearchEntity, field: &str, condition: Condition) -> AppError {
        let error = plan_of(entity, field, condition).unwrap_err();
        assert_eq!(error.code, "VALIDATION-05", "{error:?}");
        error
    }

    fn only(plan: SearchPlan) -> Predicate {
        assert_eq!(plan.predicates.len(), 1);
        plan.predicates.into_iter().next().unwrap()
    }

    #[test]
    fn an_empty_request_sorts_by_creation_time_newest_first() {
        let plan = CUSTOMERS.plan(&SearchRequest::default()).unwrap();
        assert!(plan.predicates.is_empty());
        assert_eq!(plan.sort, SortSpec { column: "created_at", descending: true });
    }

    #[test]
    fn a_sort_is_ascending_or_descending_by_a_known_field() {
        let sort = |spec: &str| CUSTOMERS.plan(&SearchRequest { sort: spec.into(), ..SearchRequest::default() });
        assert_eq!(sort("name").unwrap().sort, SortSpec { column: "name", descending: false });
        assert_eq!(sort("-name").unwrap().sort, SortSpec { column: "name", descending: true });
        let error = sort("deleted_at").unwrap_err();
        assert_eq!((error.code, error.params["field"].as_str()), ("VALIDATION-06", Some("deleted_at")));
        assert!(error.params["allowed"].as_array().unwrap().contains(&json!("tax_id")));
        assert!(!error.params["allowed"].as_array().unwrap().contains(&json!("deleted_at")));
        assert_eq!(sort("--name").unwrap_err().code, "VALIDATION-06");
    }

    #[test]
    fn a_field_the_entity_does_not_know_is_ignored() {
        let plan = plan_of(&CUSTOMERS, "customer_id", cond("eq", json!("whatever"))).unwrap();
        assert!(plan.predicates.is_empty());
        assert!(plan_of(&ORDERS, "deleted_at", cond("eq", json!("x"))).unwrap().predicates.is_empty());
    }

    #[test]
    fn the_operator_defaults_to_eq_and_is_case_insensitive() {
        assert_eq!(only(plan_of(&CUSTOMERS, "name", cond("", json!("Ada"))).unwrap()).operator, Operator::Eq);
        assert_eq!(only(plan_of(&CUSTOMERS, "name", cond("NEQ", json!("Ada"))).unwrap()).operator, Operator::Neq);
        let error = bad(&CUSTOMERS, "name", cond("regex", json!("a")));
        assert_eq!((error.params["field"].as_str(), error.params["op"].as_str()), (Some("name"), Some("regex")));
    }

    #[test]
    fn values_are_converted_to_the_type_of_the_column() {
        let id = Uuid::new_v4();
        let uuid = only(plan_of(&CUSTOMERS, "id", cond("eq", json!(id.to_string()))).unwrap());
        assert_eq!(uuid.values, [Typed::Uuid(id)]);
        let int = only(plan_of(&CUSTOMERS, "version", cond("gte", json!(3))).unwrap());
        assert_eq!(int.values, [Typed::Int(3)]);
        let time = only(plan_of(&CUSTOMERS, "created_at", cond("lt", json!("2026-01-02T03:04:05Z"))).unwrap());
        assert_eq!(time.values, [Typed::Time(OffsetDateTime::from_unix_timestamp(1_767_323_045).unwrap())]);
        let status = only(plan_of(&ORDERS, "status", cond("eq", json!("OPEN"))).unwrap());
        assert_eq!(status.values, [Typed::Text("OPEN".into())]);
    }

    #[test]
    fn a_value_of_the_wrong_type_is_validation_05_with_the_expectation() {
        for (entity, field, value, expected) in [
            (&CUSTOMERS, "name", json!(5), "expected a string"),
            (&CUSTOMERS, "id", json!("not-a-uuid"), "expected a UUID"),
            (&CUSTOMERS, "id", json!("123e4567e89b42d3a456426614174000"), "expected a UUID"),
            (&CUSTOMERS, "version", json!("x"), "expected an integer"),
            (&CUSTOMERS, "version", json!(1.5), "expected an integer"),
            (&CUSTOMERS, "version", json!(true), "expected an integer"),
            (&CUSTOMERS, "created_at", json!("yesterday"), "expected an RFC 3339 timestamp or a YYYY-MM-DD date"),
            (&ORDERS, "status", json!("SHIPPED"), "expected one of CANCELED, CONFIRMED, OPEN"),
            (&ORDERS, "status", json!("open"), "expected one of CANCELED, CONFIRMED, OPEN"),
        ] {
            let error = bad(entity, field, cond("eq", value));
            assert_eq!(error.message, expected);
        }
    }

    #[test]
    fn a_missing_value_is_refused() {
        bad(&CUSTOMERS, "name", Condition { op: "eq".into(), value: None });
        bad(&CUSTOMERS, "name", cond("eq", Value::Null));
    }

    #[test]
    fn integers_may_be_written_with_a_zero_fraction_or_as_text() {
        for value in [json!(7), json!(7.0), json!("7"), json!(7e0)] {
            let predicate = only(plan_of(&CUSTOMERS, "version", cond("eq", value)).unwrap());
            assert_eq!(predicate.values, [Typed::Int(7)]);
        }
        bad(&CUSTOMERS, "version", cond("eq", json!(1e30)));
    }

    #[test]
    fn dates_without_a_time_mean_midnight_utc() {
        let predicate = only(plan_of(&ORDERS, "created_at", cond("gte", json!("2026-01-02"))).unwrap());
        assert_eq!(predicate.values, [Typed::Time(OffsetDateTime::from_unix_timestamp(1_767_312_000).unwrap())]);
        bad(&ORDERS, "created_at", cond("gte", json!("2026-13-45")));
    }

    #[test]
    fn ordering_operators_are_refused_on_enums_but_not_elsewhere() {
        for op in ["gt", "gte", "lt", "lte"] {
            assert_eq!(
                bad(&ORDERS, "status", cond(op, json!("OPEN"))).message,
                "ordering operators are not supported on this field"
            );
        }
        bad(&ORDERS, "status", cond("between", json!(["OPEN", "CANCELED"])));
        assert!(plan_of(&ORDERS, "id", cond("gt", json!(Uuid::nil().to_string()))).is_ok());
        assert!(plan_of(&CUSTOMERS, "name", cond("lte", json!("M"))).is_ok());
    }

    #[test]
    fn in_takes_between_one_and_a_hundred_values_of_the_column_type() {
        let ok = only(plan_of(&ORDERS, "status", cond("in", json!(["OPEN", "CANCELED"]))).unwrap());
        assert_eq!(ok.values.len(), 2);
        assert_eq!(bad(&ORDERS, "status", cond("in", json!([]))).message, "in expects between 1 and 100 values");
        assert_eq!(
            bad(&CUSTOMERS, "name", cond("in", json!(vec!["a"; 101]))).message,
            "in expects between 1 and 100 values"
        );
        assert_eq!(bad(&CUSTOMERS, "name", cond("in", json!("a"))).message, "expected a list of values");
        bad(&CUSTOMERS, "version", cond("in", json!([1, "x"])));
    }

    #[test]
    fn between_takes_exactly_two_values() {
        let ok = only(plan_of(&CUSTOMERS, "version", cond("between", json!([1, 5]))).unwrap());
        assert_eq!(ok.values, [Typed::Int(1), Typed::Int(5)]);
        assert_eq!(bad(&CUSTOMERS, "version", cond("between", json!([1]))).message, "expected exactly 2 values");
        assert_eq!(bad(&CUSTOMERS, "version", cond("between", json!([1, 2, 3]))).message, "expected exactly 2 values");
        assert_eq!(bad(&CUSTOMERS, "version", cond("between", json!(1))).message, "expected a list of values");
    }

    #[test]
    fn like_escapes_the_wildcards_and_wraps_the_text() {
        let predicate = only(plan_of(&CUSTOMERS, "name", cond("lk", json!("50%_off\\"))).unwrap());
        assert_eq!(predicate.values, [Typed::Text("%50\\%\\_off\\\\%".into())]);
        assert_eq!(bad(&CUSTOMERS, "version", cond("lk", json!("1"))).message, "lk is only supported on text fields");
        assert_eq!(bad(&CUSTOMERS, "name", cond("lk", json!(5))).message, "lk expects a string");
    }

    #[test]
    fn predicates_come_out_in_field_order() {
        let request = SearchRequest {
            filters: filters_of(&[("version", vec![cond("eq", json!(1))]), ("name", vec![cond("eq", json!("a"))])]),
            ..SearchRequest::default()
        };
        let columns: Vec<_> = CUSTOMERS.plan(&request).unwrap().predicates.iter().map(|p| p.column).collect();
        assert_eq!(columns, ["name", "version"]);
    }

    #[test]
    fn the_whitelist_never_exposes_the_soft_delete_columns() {
        for entity in [&CUSTOMERS, &ORDERS] {
            let names = entity.field_names();
            assert!(!names.contains(&"deleted_at") && !names.contains(&"deleted_by"), "{}", entity.name);
        }
    }

    #[test]
    fn the_status_set_matches_the_domain() {
        let FieldType::Enum(allowed) = ORDERS.field("status").unwrap().1 else { panic!("status is an enum") };
        let domain: Vec<_> = OrderStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(allowed, domain.as_slice());
    }
}
