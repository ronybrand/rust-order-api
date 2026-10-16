.PHONY: build test integration lint verify run-api run-worker

build:
	cargo build --release --locked

# Format, lints and the unit tests: fast, no database, no broker, no network.
test: lint
	cargo test --lib --locked

lint:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings

# A real PostgreSQL and RabbitMQ. Without them these tests say so and pass; with REQUIRE_INTEGRATION=1 they fail.
#   docker compose up -d postgres rabbitmq
#   TEST_DATABASE_URL=postgres://orders:orders@127.0.0.1:5432/postgres TEST_AMQP_URL=amqp://guest:guest@127.0.0.1:5672/
integration:
	cargo test --test integration --locked

# The single local gate, same as CI: format, lints, every test and the coverage floor (needs cargo-llvm-cov).
verify: lint
	cargo llvm-cov --locked --fail-under-lines 85

# Needs a PostgreSQL: docker compose up -d postgres
run-api:
	cargo run --bin api

run-worker:
	cargo run --bin worker
