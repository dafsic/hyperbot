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
use tokio::sync::mpsc::UnboundedReceiver;

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

/// A fill event received from the exchange stream.
#[derive(Debug, Clone, PartialEq)]
pub struct FillEvent {
    /// Exchange order id that was filled.
    pub oid: u64,
    /// Coin symbol.
    pub coin: String,
    /// Side of the fill.
    pub side: Side,
    /// Fill price.
    pub price: f64,
    /// Fill size.
    pub size: f64,
}

/// Events emitted by [`Exchange::subscribe`].
#[derive(Debug, Clone, PartialEq)]
pub enum ExchangeEvent {
    /// New mid price for the traded coin.
    Mid(f64),
    /// One of our orders was (partially) filled.
    Fill(FillEvent),
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

    /// Returns the current position for `coin`, if any.
    async fn position(&self, coin: &str) -> anyhow::Result<Option<Position>>;

    /// Subscribes to mid-price and fill events for `coin`.
    async fn subscribe(&self, coin: &str) -> anyhow::Result<UnboundedReceiver<ExchangeEvent>>;
}
