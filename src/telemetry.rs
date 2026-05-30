//! Structured logging via `tracing`.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialises the global tracing subscriber.
///
/// The log level is taken from the `RUST_LOG` environment variable, defaulting
/// to `info` for the bot and `warn` for noisy dependencies. Safe to call once
/// at start-up; subsequent calls are ignored.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,hyper=warn,sqlx=warn,tungstenite=warn,tokio_tungstenite=warn")
    });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .try_init();
}
