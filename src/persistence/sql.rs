//! The WHERE and ORDER BY of a search plan. Column names come only from the whitelist of the plan; every value is a
//! bound parameter.

use sqlx::{Postgres, QueryBuilder};

use super::plan::{Operator, Predicate, SearchPlan, SortSpec, Typed};

/// Appends ` WHERE deleted_at IS NULL AND ...`: the soft-delete filter plus every predicate of the plan.
pub fn push_where(builder: &mut QueryBuilder<Postgres>, plan: &SearchPlan) {
    builder.push(" WHERE deleted_at IS NULL");
    for predicate in &plan.predicates {
        builder.push(" AND ");
        push_predicate(builder, predicate);
    }
}

/// Appends ` ORDER BY <column> ASC|DESC, id ASC`: the id makes the order total, so pages never overlap.
pub fn push_order(builder: &mut QueryBuilder<Postgres>, sort: SortSpec) {
    builder.push(" ORDER BY ").push(sort.column).push(if sort.descending { " DESC" } else { " ASC" }).push(", id ASC");
}

fn push_predicate(builder: &mut QueryBuilder<Postgres>, predicate: &Predicate) {
    let column = predicate.column;
    let values = &predicate.values;
    builder.push(column);
    match predicate.operator {
        Operator::Eq => push_one(builder, " = ", &values[0]),
        Operator::Neq => push_one(builder, " <> ", &values[0]),
        Operator::Gt => push_one(builder, " > ", &values[0]),
        Operator::Gte => push_one(builder, " >= ", &values[0]),
        Operator::Lt => push_one(builder, " < ", &values[0]),
        Operator::Lte => push_one(builder, " <= ", &values[0]),
        Operator::In => {
            builder.push(" IN (");
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    builder.push(", ");
                }
                push_value(builder, value);
            }
            builder.push(")");
        }
        Operator::Between => {
            builder.push(" BETWEEN ");
            push_value(builder, &values[0]);
            builder.push(" AND ");
            push_value(builder, &values[1]);
        }
        // The plan already escaped the wildcards of the text with a backslash.
        Operator::Like => {
            builder.push(" ILIKE ");
            push_value(builder, &values[0]);
            builder.push(" ESCAPE '\\'");
        }
    }
}

fn push_one(builder: &mut QueryBuilder<Postgres>, operator: &str, value: &Typed) {
    builder.push(operator);
    push_value(builder, value);
}

fn push_value(builder: &mut QueryBuilder<Postgres>, value: &Typed) {
    match value {
        Typed::Text(text) => builder.push_bind(text.clone()),
        Typed::Uuid(id) => builder.push_bind(*id),
        Typed::Int(number) => builder.push_bind(*number),
        Typed::Time(instant) => builder.push_bind(*instant),
    };
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)] // the helpers take a request the way the tests write it
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::app::search::{Condition, SearchRequest};
    use crate::persistence::plan::{CUSTOMERS, ORDERS, filters_of};

    fn sql(entity: &crate::persistence::plan::SearchEntity, request: SearchRequest) -> String {
        let plan = entity.plan(&request).unwrap();
        let mut builder = QueryBuilder::<Postgres>::new("SELECT * FROM t");
        push_where(&mut builder, &plan);
        push_order(&mut builder, plan.sort);
        builder.sql().as_str().to_string()
    }

    fn filter(field: &str, op: &str, value: serde_json::Value) -> SearchRequest {
        SearchRequest {
            filters: filters_of(&[(field, vec![Condition { op: op.into(), value: Some(value) }])]),
            ..SearchRequest::default()
        }
    }

    #[test]
    fn the_default_search_filters_only_the_deleted_rows_and_sorts_newest_first() {
        assert_eq!(
            sql(&CUSTOMERS, SearchRequest::default()),
            "SELECT * FROM t WHERE deleted_at IS NULL ORDER BY created_at DESC, id ASC"
        );
    }

    #[test]
    fn comparison_operators_bind_one_parameter_each() {
        for (op, symbol) in [("eq", "="), ("neq", "<>"), ("gt", ">"), ("gte", ">="), ("lt", "<"), ("lte", "<=")] {
            assert_eq!(
                sql(&CUSTOMERS, filter("version", op, json!(3))),
                format!(
                    "SELECT * FROM t WHERE deleted_at IS NULL AND version {symbol} $1 ORDER BY created_at DESC, id ASC"
                )
            );
        }
    }

    #[test]
    fn in_binds_one_parameter_per_value_and_between_two() {
        assert_eq!(
            sql(&ORDERS, filter("status", "in", json!(["OPEN", "CANCELED"]))),
            "SELECT * FROM t WHERE deleted_at IS NULL AND status IN ($1, $2) ORDER BY created_at DESC, id ASC"
        );
        assert_eq!(
            sql(&CUSTOMERS, filter("version", "between", json!([1, 5]))),
            "SELECT * FROM t WHERE deleted_at IS NULL AND version BETWEEN $1 AND $2 ORDER BY created_at DESC, id ASC"
        );
    }

    #[test]
    fn like_is_case_insensitive_and_declares_its_escape_character() {
        assert_eq!(
            sql(&CUSTOMERS, filter("name", "lk", json!("ada"))),
            "SELECT * FROM t WHERE deleted_at IS NULL AND name ILIKE $1 ESCAPE '\\' ORDER BY created_at DESC, id ASC"
        );
    }

    #[test]
    fn predicates_are_anded_and_the_sort_follows_the_request() {
        let request = SearchRequest {
            sort: "-version".into(),
            filters: filters_of(&[
                ("customer_id", vec![Condition { op: "eq".into(), value: Some(json!(Uuid::nil().to_string())) }]),
                ("status", vec![Condition { op: "neq".into(), value: Some(json!("OPEN")) }]),
            ]),
            ..SearchRequest::default()
        };
        assert_eq!(
            sql(&ORDERS, request),
            "SELECT * FROM t WHERE deleted_at IS NULL AND customer_id = $1 AND status <> $2 ORDER BY version DESC, id ASC"
        );
    }

    #[test]
    fn a_hostile_value_never_reaches_the_statement_text() {
        let text = sql(&CUSTOMERS, filter("name", "eq", json!("x'; DROP TABLE customers; --")));
        assert!(!text.contains("DROP") && text.contains("name = $1"), "{text}");
    }
}
