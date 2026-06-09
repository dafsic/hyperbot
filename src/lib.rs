//! Hyperbot — a perpetual (contract) grid trading bot for Hyperliquid.
//!
//! The crate is split into independent modules so each piece can be tested in
//! isolation:
//!
//! * [`config`] — strongly typed configuration loaded from a TOML file with
//!   secrets injected from the environment.
//! * [`telemetry`] — `tracing` based structured logging.
//! * [`exchange`] — an [`exchange::Exchange`] trait abstracting the venue, with
//!   a Hyperliquid implementation and an in-memory mock used for tests.
//! * [`grid`] — the pure grid strategy logic (no I/O), fully unit tested.
//! * [`store`] — a `sqlx` + PostgreSQL backed persistence layer.
//! * [`risk`] — risk controls (max position / loss, leverage caps).
//! * [`bot`] — the orchestration layer wiring everything together.

pub mod bot;
pub mod config;
pub mod exchange;
pub mod grid;
pub mod store;
pub mod telemetry;

/// Convenient crate-wide `Result` alias.
pub type Result<T> = anyhow::Result<T>;
