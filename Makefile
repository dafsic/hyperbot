.PHONY: build test fmt fmt-check clippy run docker up down

build:
cargo build

test:
cargo test

# Run the integration test against a database:
#   make test-integration TEST_DATABASE_URL=postgres://postgres@localhost:5432/hyperbot
test-integration:
cargo test -- --include-ignored

fmt:
cargo fmt

fmt-check:
cargo fmt --check

clippy:
cargo clippy --all-targets -- -D warnings

run:
cargo run --bin hyperbot

docker:
docker build -t hyperbot:latest .

up:
docker compose up -d --build

down:
docker compose down
