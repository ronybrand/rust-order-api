use std::fmt;

use time::OffsetDateTime;
use uuid::Uuid;

use super::email::is_valid_email;
use crate::error::AppError;

// Field limits shared by validation and the database schema (migrations/0001_init.sql): the database refuses what
// the domain would refuse, and the numbers cannot drift apart silently.
pub const MAX_NAME_LENGTH: usize = 200;
pub const MAX_TAX_ID_LENGTH: usize = 20;
pub const MAX_PASSPORT_LENGTH: usize = 9;
pub const MAX_ACTOR_LENGTH: usize = 200;
const MIN_TAX_ID_LENGTH: usize = 5;
const MIN_PASSPORT_LENGTH: usize = 6;

/// The data a client supplies to create or replace a customer.
#[derive(Debug, Clone)]
pub struct CustomerInput {
    pub name: String,
    pub tax_id: String,
    pub passport_number: Option<String>,
    pub email: String,
}

/// A registered customer.
///
/// The tax id, passport number and e-mail are personal data: they are never logged (`Debug` shows only the id) and
/// never returned by the API.
#[derive(Clone)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub tax_id: String,
    pub passport_number: Option<String>,
    pub email: String,
    pub version: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub created_by: String,
    pub updated_by: String,
    pub deleted_at: Option<OffsetDateTime>,
    pub deleted_by: Option<String>,
}

impl fmt::Debug for Customer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Customer(id={})", self.id)
    }
}

impl Customer {
    pub fn create(input: CustomerInput, actor: &str, now: OffsetDateTime) -> Result<Self, AppError> {
        Self::create_with_id(Uuid::new_v4(), input, actor, now)
    }

    pub fn create_with_id(id: Uuid, input: CustomerInput, actor: &str, now: OffsetDateTime) -> Result<Self, AppError> {
        let valid = validated(input)?;
        Ok(Self {
            id,
            name: valid.name,
            tax_id: valid.tax_id,
            passport_number: valid.passport_number,
            email: valid.email,
            version: 1,
            created_at: now,
            updated_at: now,
            created_by: actor.to_owned(),
            updated_by: actor.to_owned(),
            deleted_at: None,
            deleted_by: None,
        })
    }

    /// Validates the input and overwrites the customer's data (PUT semantics); the creation audit is kept.
    pub fn replace(self, input: CustomerInput, actor: &str, now: OffsetDateTime) -> Result<Self, AppError> {
        let valid = validated(input)?;
        Ok(Self {
            name: valid.name,
            tax_id: valid.tax_id,
            passport_number: valid.passport_number,
            email: valid.email,
            updated_at: now,
            updated_by: actor.to_owned(),
            ..self
        })
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

fn validated(input: CustomerInput) -> Result<CustomerInput, AppError> {
    let CustomerInput { name, tax_id, passport_number, email } = input;
    if name.trim().is_empty() {
        return Err(AppError::invalid_field("name", "name must not be blank"));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        return Err(AppError::invalid_field("name", "name is too long"));
    }

    if !is_tax_id_shape(&tax_id) {
        return Err(AppError::invalid_field("tax_id", "tax_id has an invalid format"));
    }
    let canonical = canonical_tax_id(&tax_id);
    if canonical.len() < MIN_TAX_ID_LENGTH {
        return Err(AppError::invalid_field("tax_id", "tax_id must contain at least 5 letters or digits"));
    }

    let passport = passport_number.filter(|p| !p.is_empty());
    if let Some(passport) = &passport
        && !is_passport_shape(passport)
    {
        return Err(AppError::invalid_field("passport_number", "passport_number has an invalid format"));
    }

    if !is_valid_email(&email) {
        return Err(AppError::invalid_field("email", "email is invalid"));
    }
    Ok(CustomerInput { name, tax_id: canonical, passport_number: passport, email })
}

/// `[A-Za-z0-9./-]{5,20}`, the whole string.
fn is_tax_id_shape(tax_id: &str) -> bool {
    (MIN_TAX_ID_LENGTH..=MAX_TAX_ID_LENGTH).contains(&tax_id.len())
        && tax_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '-'))
}

/// `[A-Z0-9]{6,9}`, the whole string.
fn is_passport_shape(passport: &str) -> bool {
    (MIN_PASSPORT_LENGTH..=MAX_PASSPORT_LENGTH).contains(&passport.len())
        && passport.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Keeps only ASCII letters and digits, upper-cased: "123.456-78" and "12345678" are the same document, so the stored
/// value (and the uniqueness check) always uses this form.
pub fn canonical_tax_id(raw: &str) -> String {
    raw.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> CustomerInput {
        CustomerInput {
            name: "Ada Lovelace".into(),
            tax_id: "123.456-78".into(),
            passport_number: Some("AB123456".into()),
            email: "ada@example.com".into(),
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn invalid_field(input: CustomerInput) -> String {
        let error = Customer::create(input, "admin", now()).unwrap_err();
        assert_eq!(error.code, "VALIDATION-01");
        error.params["field"].as_str().unwrap().to_owned()
    }

    #[test]
    fn creates_a_customer_with_a_canonical_tax_id_and_the_audit_columns() {
        let customer = Customer::create(input(), "admin-1", now()).unwrap();
        assert_eq!(customer.tax_id, "12345678");
        assert_eq!(customer.version, 1);
        assert_eq!((customer.created_by.as_str(), customer.updated_by.as_str()), ("admin-1", "admin-1"));
        assert_eq!(customer.created_at, now());
        assert!(!customer.is_deleted());
    }

    #[test]
    fn the_tax_id_is_upper_cased_and_stripped_of_punctuation() {
        assert_eq!(canonical_tax_id("ab.12/3-c"), "AB123C");
        assert_eq!(canonical_tax_id("1234-5"), "12345");
    }

    #[test]
    fn a_blank_or_too_long_name_is_refused() {
        assert_eq!(invalid_field(CustomerInput { name: "   ".into(), ..input() }), "name");
        assert_eq!(invalid_field(CustomerInput { name: "a".repeat(201), ..input() }), "name");
        let two_hundred_emoji = "\u{1F600}".repeat(200);
        assert!(Customer::create(CustomerInput { name: two_hundred_emoji, ..input() }, "a", now()).is_ok());
    }

    #[test]
    fn a_tax_id_of_the_wrong_shape_is_refused() {
        for tax_id in ["1234", &"1".repeat(21), "123 456 78", "12345\u{e9}", "12345;--"] {
            assert_eq!(invalid_field(CustomerInput { tax_id: tax_id.into(), ..input() }), "tax_id", "{tax_id}");
        }
    }

    #[test]
    fn a_tax_id_with_too_few_significant_characters_is_refused() {
        assert_eq!(invalid_field(CustomerInput { tax_id: "1.2.3.4".into(), ..input() }), "tax_id");
        assert_eq!(invalid_field(CustomerInput { tax_id: "-----".into(), ..input() }), "tax_id");
    }

    #[test]
    fn a_passport_must_be_six_to_nine_upper_case_letters_or_digits() {
        for passport in ["AB12", "AB12345678", "ab123456", "AB 12345", "AB-12345"] {
            let bad = CustomerInput { passport_number: Some(passport.into()), ..input() };
            assert_eq!(invalid_field(bad), "passport_number", "{passport}");
        }
    }

    #[test]
    fn an_empty_or_missing_passport_means_none() {
        for passport in [None, Some(String::new())] {
            let customer =
                Customer::create(CustomerInput { passport_number: passport, ..input() }, "a", now()).unwrap();
            assert_eq!(customer.passport_number, None);
        }
    }

    #[test]
    fn an_invalid_email_is_refused() {
        assert_eq!(invalid_field(CustomerInput { email: "not an email".into(), ..input() }), "email");
    }

    #[test]
    fn replace_overwrites_the_data_and_keeps_the_creation_audit() {
        let created = Customer::create(input(), "admin-1", now()).unwrap();
        let later = now() + time::Duration::hours(1);
        let replaced = created
            .replace(
                CustomerInput { name: "Ada King".into(), tax_id: "999.888-77".into(), ..input() },
                "admin-2",
                later,
            )
            .unwrap();
        assert_eq!(replaced.name, "Ada King");
        assert_eq!(replaced.tax_id, "99988877");
        assert_eq!((replaced.created_by.as_str(), replaced.updated_by.as_str()), ("admin-1", "admin-2"));
        assert_eq!((replaced.created_at, replaced.updated_at), (now(), later));
    }

    #[test]
    fn replace_validates_like_create() {
        let created = Customer::create(input(), "a", now()).unwrap();
        let error = created.replace(CustomerInput { email: "nope".into(), ..input() }, "a", now()).unwrap_err();
        assert_eq!(error.params["field"], "email");
    }

    #[test]
    fn debug_never_shows_personal_data() {
        let customer = Customer::create(input(), "a", now()).unwrap();
        let text = format!("{customer:?}");
        assert_eq!(text, format!("Customer(id={})", customer.id));
        for secret in ["Ada", "12345678", "AB123456", "ada@example.com"] {
            assert!(!text.contains(secret), "{secret}");
        }
    }
}
