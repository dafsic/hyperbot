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

/// A fill reported by the exchange's own fill history.
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
    /// Fill timestamp in milliseconds since the Unix epoch. Used as a watermark
    /// so periodic polling only fetches fills newer than the last one seen.
    pub time: u64,
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

    /// Returns fills for `coin` from the exchange's own fill history, limited to
    /// those at or after `since_ms` (milliseconds since the Unix epoch).
    ///
    /// This is the **authoritative** source of fills. The bot detects filled
    /// orders by periodically polling this history and matching fills back to
    /// tracked orders by `oid`. Unlike inferring fills from the absence of an
    /// order on the book (which is unsafe when a book snapshot is incomplete), a
    /// fill reported here definitely happened.
    ///
    /// `since_ms` bounds the volume so a long-running bot does not re-fetch and
    /// re-scan its entire (capped, but still large) fill history every tick:
    /// callers pass the timestamp of the last fill they processed. The window is
    /// **inclusive** of `since_ms`, so the boundary fill may be returned again;
    /// callers must deduplicate (which the bot already does by order status).
    /// Pass `0` to fetch the full available history (e.g. on startup, to catch
    /// fills that happened while the bot was offline).
    async fn recent_fills(&self, coin: &str, since_ms: u64) -> anyhow::Result<Vec<FillEvent>>;

    /// Returns the current position for `coin`, if any.
    async fn position(&self, coin: &str) -> anyhow::Result<Option<Position>>;
}
