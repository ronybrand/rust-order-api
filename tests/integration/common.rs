use std::str::FromStr;
use std::sync::OnceLock;

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor, PgPool};
use uuid::Uuid;

use orderapi::persistence::{PostgresStore, migrate};

pub const AUDIENCE: &str = "order-api";
pub const ISSUER: &str = "https://idp.example";

/// Whether a missing service fails the test (CI) or skips it (a bare machine).
fn required() -> bool {
    std::env::var("REQUIRE_INTEGRATION").is_ok_and(|value| !value.is_empty() && value != "0")
}

/// The value of an environment variable, or `None` after saying why the test is being skipped.
pub fn service(variable: &str, test: &str) -> Option<String> {
    match std::env::var(variable) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            assert!(!required(), "{variable} is not set, but REQUIRE_INTEGRATION is: {test} cannot be skipped here");
            eprintln!("skipped {test}: {variable} is not set");
            None
        }
    }
}

/// A database of its own, migrated, dropped by [`TestDb::finish`].
pub struct TestDb {
    admin: PgPool,
    name: String,
    pub pool: PgPool,
    pub store: PostgresStore,
}

impl TestDb {
    pub async fn new(test: &str) -> Option<Self> {
        let url = service("TEST_DATABASE_URL", test)?;
        let options = PgConnectOptions::from_str(&url).expect("TEST_DATABASE_URL is a postgres URL");
        let admin =
            PgPoolOptions::new().max_connections(2).connect_with(options.clone()).await.expect("the admin connection");
        let name = format!("orders_test_{}", Uuid::new_v4().simple());
        admin.execute(AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#))).await.expect("create the test database");
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(options.database(&name))
            .await
            .expect("a connection to the test database");
        migrate(&pool).await.expect("the migrations");
        Some(Self { store: PostgresStore::new(pool.clone()), admin, name, pool })
    }

    /// Drops the database. `WITH (FORCE)` ends the connections a test still holds (a session it did not drop), which
    /// closing the pool would wait for forever.
    pub async fn finish(self) {
        self.admin
            .execute(AssertSqlSafe(format!(r#"DROP DATABASE IF EXISTS "{}" WITH (FORCE)"#, self.name)))
            .await
            .expect("drop the test database");
        self.admin.close().await;
    }
}

pub struct KeyPair {
    pub private_pem: String,
    pub public_pem: String,
}

/// One RSA key pair per test process: the issuer the API trusts.
pub fn keys() -> &'static KeyPair {
    static KEYS: OnceLock<KeyPair> = OnceLock::new();
    KEYS.get_or_init(|| {
        let private = RsaPrivateKey::new(&mut rand_core::OsRng, 2048).expect("a key pair");
        KeyPair {
            private_pem: private.to_pkcs8_pem(LineEnding::LF).expect("pem").to_string(),
            public_pem: RsaPublicKey::from(&private).to_public_key_pem(LineEnding::LF).expect("pem"),
        }
    })
}

pub fn token(subject: &str, roles: &[&str]) -> String {
    let exp = jsonwebtoken::get_current_timestamp() + 3600;
    let claims: Value = json!({ "sub": subject, "aud": AUDIENCE, "iss": ISSUER, "exp": exp, "roles": roles });
    let key = EncodingKey::from_rsa_pem(keys().private_pem.as_bytes()).expect("the private key");
    encode(&Header::new(Algorithm::RS256), &claims, &key).expect("a signed token")
}
