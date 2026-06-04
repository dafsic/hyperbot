//! Exchange abstraction.
//!
//! The [`Exchange`] trait isolates the rest of the bot from the Hyperliquid
//! SDK so the orchestration and strategy code can be exercised against the
//! in-memory [`mock::MockExchange`].

mod hyperliquid;
pub mod mock;

pub use hyperliquid::HyperliquidExchange;
pub use mock::MockExchange;

use async_trait::async_trait;

use crate::grid::Side;

/// A request to place a single limit order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaceOrder {
    /// Perp coin symbol, e.g. `"XMR"`.
    pub coin: String,
    /// Order side.
    pub side: Side,
    /// Limit price.
    pub price: f64,
    /// Size in contracts.
    pub size: f64,
    /// Whether the order may only reduce an existing position.
    pub reduce_only: bool,
}

/// Result of placing an order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedOrder {
    /// Exchange order id.
    pub oid: u64,
    /// Whether the order rested on the book (`false` means it filled
    /// immediately).
    pub resting: bool,
}

/// An open (resting) order returned by the exchange.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrder {
    /// Exchange order id.
    pub oid: u64,
    /// Coin symbol.
    pub coin: String,
    /// Side.
    pub side: Side,
    /// Limit price.
    pub price: f64,
    /// Remaining size.
    pub size: f64,
}

/// A position snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    /// Coin symbol.
    pub coin: String,
    /// Signed size (negative = short).
    pub size: f64,
    /// Average entry price, if any.
    pub entry_price: Option<f64>,
    /// Unrealised PnL in USDC.
    pub unrealized_pnl: f64,
}

/// The lifecycle state of an order as reported by the exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    /// Still resting on the order book with no fills yet.
    Open,
    /// Resting on the order book but part of the quantity has already been
    /// filled (remaining size < original size).
    PartialFill,
    /// Fully filled.
    Filled,
    /// Cancelled or unknown oid.
    Cancelled,
}

/// Abstraction over a trading venue.
#[async_trait]
pub trait Exchange: Send + Sync {
    /// Returns the current mid price for `coin`.
    async fn mid_price(&self, coin: &str) -> anyhow::Result<f64>;

    /// Sets leverage for `coin`.
    async fn update_leverage(&self, coin: &str, leverage: u32, cross: bool) -> anyhow::Result<()>;

    /// Places a single limit order.
    async fn place_order(&self, req: &PlaceOrder) -> anyhow::Result<PlacedOrder>;

    /// Cancels an order by id.
    async fn cancel_order(&self, coin: &str, oid: u64) -> anyhow::Result<()>;

    /// Lists currently open orders for `coin`.
    async fn open_orders(&self, coin: &str) -> anyhow::Result<Vec<OpenOrder>>;

    /// Returns the lifecycle state of a specific order by its exchange-assigned oid.
    ///
    /// Used by periodic reconciliation to check whether frontier resting orders
    /// have filled or been cancelled externally. Returns [`OrderState::Cancelled`]
    /// for an unknown oid so the caller can stop tracking it safely.
    async fn order_status(&self, coin: &str, oid: u64) -> anyhow::Result<OrderState>;

    /// Returns the current position for `coin`, if any.
    async fn position(&self, coin: &str) -> anyhow::Result<Option<Position>>;
}
