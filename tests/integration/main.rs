//! Integration tests: a real `PostgreSQL`, a real `RabbitMQ` and an SMTP server speaking the protocol.
//!
//! They need those services and are driven by environment variables (see the README):
//!
//! - `TEST_DATABASE_URL`: an admin connection, e.g. `postgres://orders:orders@127.0.0.1:5432/postgres`. Every test makes
//!   its own database and drops it.
//! - `TEST_AMQP_URL`: a broker, e.g. `amqp://guest:guest@127.0.0.1:5672/`.
//!
//! Without a variable the tests that need it say so and pass, so `cargo test` works on a bare machine. CI sets
//! `REQUIRE_INTEGRATION=1`, and then a missing service is a failure: a skipped test must never look like a green one
//! there.

#![allow(clippy::needless_pass_by_value)] // the helpers take their arguments the way the tests write them

mod common;
mod http_postgres;
mod postgres_store;
mod rabbit;
mod smtp;
