//! Hardcoded configuration.
//!
//! All configurable values are defined as constants at the top of this file.
//! Edit them before compiling, then rebuild.

use crate::grid::{GridMode, GridParams, Spacing};

// =============================================================================
// ▼▼▼  All user-configurable values — edit here  ▼▼▼
// =============================================================================

// --- Exchange / Wallet -------------------------------------------------------

/// Hyperliquid network to trade on.
const NETWORK: Network = Network::Testnet;

/// API wallet private key (hex, with or without `0x`).
const PRIVATE_KEY: &str = "REPLACE_WITH_YOUR_PRIVATE_KEY_HEX";

/// Master account address (`0x`-prefixed).
/// Set when `PRIVATE_KEY` is an agent/API wallet whose derived address differs
/// from the funded master account (signing uses the agent key but all info
/// queries target the master account). Leave empty to use the key's own address.
const ACCOUNT_ADDRESS: &str = "0xf581803C5998FAb668Ee4E0826eCf8e2Ca3f469b";

/// Leverage to set for the traded coin before the grid starts.
const LEVERAGE: u32 = 3;

/// `true` = cross margin, `false` = isolated margin.
const CROSS_MARGIN: bool = false;

/// Cancel all resting orders on shutdown.
/// `false` preserves grid orders across restarts; they are picked up on the
/// next startup via reconcile.
const CANCEL_ON_EXIT: bool = false;

// --- Grid --------------------------------------------------------------------

/// Perp coin symbol, e.g. `"XMR"`.
const COIN: &str = "XMR";

/// Lower price bound of the grid.
const LOWER_PRICE: f64 = 350.0;

/// Upper price bound of the grid.
const UPPER_PRICE: f64 = 370.0;

/// Number of grid intervals. The grid has `GRID_COUNT + 1` price lines.
const GRID_COUNT: usize = 10;

/// Grid line spacing.
/// `Spacing::Arithmetic` — equal absolute distance (等差网格).
/// `Spacing::Geometric`  — equal ratio between lines (等比网格).
const SPACING: Spacing = Spacing::Arithmetic;

/// Order size (contracts) placed at every grid line.
const ORDER_SIZE: f64 = 0.2;

/// Trade direction.
/// `GridMode::ShortOnly` — only ever hold a short position.
/// `GridMode::LongOnly`  — only ever hold a long position.
/// `GridMode::Neutral`   — allow both long and short legs around the mid.
const MODE: GridMode = GridMode::ShortOnly;

// --- Database ----------------------------------------------------------------

/// PostgreSQL connection string.
const DATABASE_URL: &str =
    "postgres://hyperbot:k32F3k4v4oE5b0qDoh7a@localhost:5432/hyperbot_db";

/// Maximum number of pooled connections.
const MAX_CONNECTIONS: u32 = 5;

// =============================================================================
// ▲▲▲  End of user configuration  ▲▲▲
// =============================================================================

/// Which Hyperliquid network to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
}

/// Top-level configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub exchange: ExchangeConfig,
    pub grid: GridConfig,
    pub database: DatabaseConfig,
}

/// Exchange and wallet configuration.
#[derive(Debug, Clone)]
pub struct ExchangeConfig {
    pub network: Network,
    pub private_key: String,
    pub account_address: String,
    pub leverage: u32,
    pub cross_margin: bool,
    pub cancel_on_exit: bool,
}

/// Grid strategy configuration.
#[derive(Debug, Clone)]
pub struct GridConfig {
    pub coin: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub grid_count: usize,
    pub spacing: Spacing,
    pub order_size: f64,
    pub mode: GridMode,
}

impl GridConfig {
    /// Converts to the strategy parameter type.
    pub fn to_params(&self) -> GridParams {
        GridParams {
            coin: self.coin.clone(),
            lower_price: self.lower_price,
            upper_price: self.upper_price,
            grid_count: self.grid_count,
            spacing: self.spacing,
            order_size: self.order_size,
            mode: self.mode,
        }
    }
}

/// Database configuration.
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
}

impl Config {
    /// Builds configuration from the hardcoded constants above.
    pub fn load() -> anyhow::Result<Self> {
        let cfg = Self {
            exchange: ExchangeConfig {
                network: NETWORK,
                private_key: PRIVATE_KEY.to_string(),
                account_address: ACCOUNT_ADDRESS.to_string(),
                leverage: LEVERAGE,
                cross_margin: CROSS_MARGIN,
                cancel_on_exit: CANCEL_ON_EXIT,
            },
            grid: GridConfig {
                coin: COIN.to_string(),
                lower_price: LOWER_PRICE,
                upper_price: UPPER_PRICE,
                grid_count: GRID_COUNT,
                spacing: SPACING,
                order_size: ORDER_SIZE,
                mode: MODE,
            },
            database: DatabaseConfig {
                url: DATABASE_URL.to_string(),
                max_connections: MAX_CONNECTIONS,
            },
        };
        cfg.grid
            .to_params()
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid grid config: {e}"))?;
        Ok(cfg)
    }
}
