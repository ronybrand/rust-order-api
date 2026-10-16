//! Converts the database errors the client should understand into application errors and leaves everything else
//! untouched (the HTTP layer reports those as INTERNAL-00 without their text).

use uuid::Uuid;

use crate::error::{AppError, Error, codes};

// Constraint names from migrations/0001_init.sql that map to a client-facing code.
const TAX_ID_INDEX: &str = "customers_tax_id_active_uq";
const PASSPORT_INDEX: &str = "customers_passport_active_uq";

// PostgreSQL error codes (SQLSTATE) the repositories translate.
const UNIQUE_VIOLATION: &str = "23505";
const FOREIGN_KEY_VIOLATION: &str = "23503";
const CHECK_VIOLATION: &str = "23514";
const STRING_TOO_LONG: &str = "22001";
const SERIALIZATION_FAILURE: &str = "40001";
const DEADLOCK_DETECTED: &str = "40P01";

pub fn customer_not_found(id: Uuid) -> Error {
    AppError::not_found(codes::CUSTOMER_NOT_FOUND, "customer not found").with("id", id).into()
}

pub fn order_not_found(id: Uuid) -> Error {
    AppError::not_found(codes::ORDER_NOT_FOUND, "order not found").with("id", id).into()
}

pub fn concurrent_modification() -> Error {
    AppError::conflict(
        codes::CONCURRENT_MODIFICATION,
        "the resource was modified by someone else, reload it and try again",
    )
    .into()
}

fn integrity_error() -> Error {
    AppError::conflict(codes::DATA_INTEGRITY, "the operation violates a data integrity rule").into()
}

/// The translation by SQLSTATE and constraint name, apart from the driver type so it can be tested without one.
pub fn translate(sqlstate: Option<&str>, constraint: Option<&str>) -> Option<Error> {
    Some(match sqlstate? {
        UNIQUE_VIOLATION => match constraint {
            Some(TAX_ID_INDEX) => {
                AppError::conflict(codes::DUPLICATE_TAX_ID, "a customer with this tax_id already exists").into()
            }
            Some(PASSPORT_INDEX) => {
                AppError::conflict(codes::DUPLICATE_PASSPORT, "a customer with this passport_number already exists")
                    .into()
            }
            _ => integrity_error(),
        },
        FOREIGN_KEY_VIOLATION | CHECK_VIOLATION | STRING_TOO_LONG => integrity_error(),
        SERIALIZATION_FAILURE | DEADLOCK_DETECTED => concurrent_modification(),
        _ => return None,
    })
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        if let sqlx::Error::Database(database) = &error
            && let Some(translated) = translate(database.code().as_deref(), database.constraint())
        {
            return translated;
        }
        Self::unexpected(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(sqlstate: &str, constraint: Option<&str>) -> Option<&'static str> {
        translate(Some(sqlstate), constraint).and_then(|e| e.code())
    }

    #[test]
    fn the_two_unique_indexes_have_their_own_codes() {
        assert_eq!(code_of("23505", Some("customers_tax_id_active_uq")), Some("CONFLICT-01"));
        assert_eq!(code_of("23505", Some("customers_passport_active_uq")), Some("CONFLICT-02"));
    }

    #[test]
    fn any_other_unique_violation_is_a_data_integrity_conflict() {
        assert_eq!(code_of("23505", Some("something_else")), Some("CONFLICT-04"));
        assert_eq!(code_of("23505", None), Some("CONFLICT-04"));
    }

    #[test]
    fn foreign_key_check_and_length_violations_are_data_integrity_conflicts() {
        for state in ["23503", "23514", "22001"] {
            assert_eq!(code_of(state, None), Some("CONFLICT-04"), "{state}");
        }
    }

    #[test]
    fn serialization_failures_and_deadlocks_are_concurrent_modifications() {
        assert_eq!(code_of("40001", None), Some("CONFLICT-00"));
        assert_eq!(code_of("40P01", None), Some("CONFLICT-00"));
    }

    #[test]
    fn anything_else_is_left_alone() {
        assert!(translate(Some("42P01"), None).is_none());
        assert!(translate(None, None).is_none());
        let other: Error = sqlx::Error::PoolTimedOut.into();
        assert!(matches!(other, Error::Unexpected(_)));
    }
}
