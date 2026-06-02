//! Persistence layer backed by `sqlx` + PostgreSQL.
//!
//! Runtime (not compile-time) queries are used so the crate builds without a
//! live database. Migrations live in `./migrations` and are embedded into the
//! binary and applied on [`Store::connect`].

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::exchange::Position;
use crate::grid::Side;

/// Lifecycle status of a grid order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    /// Created locally, not yet acknowledged by the exchange.
    Pending,
    /// Resting on the exchange order book.
    Open,
    /// Fully filled.
    Filled,
    /// Cancelled.
    Cancelled,
}

impl OrderStatus {
    /// String representation stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::Pending => "pending",
            OrderStatus::Open => "open",
            OrderStatus::Filled => "filled",
            OrderStatus::Cancelled => "cancelled",
        }
    }
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

/// Parses a side stored in the database.
pub fn parse_side(s: &str) -> Side {
    if s.eq_ignore_ascii_case("buy") {
        Side::Buy
    } else {
        Side::Sell
    }
}

/// A grid order row.
#[derive(Debug, Clone)]
pub struct GridOrderRow {
    pub id: i64,
    pub coin: String,
    pub level: i32,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub reduce_only: bool,
    pub status: String,
    pub exchange_oid: Option<i64>,
}

impl GridOrderRow {
    fn from_row(r: GridOrderRaw) -> Self {
        Self {
            id: r.id,
            coin: r.coin,
            level: r.level,
            side: parse_side(&r.side),
            price: r.price,
            size: r.size,
            reduce_only: r.reduce_only,
            status: r.status,
            exchange_oid: r.exchange_oid,
        }
    }
}

#[derive(sqlx::FromRow)]
struct GridOrderRaw {
    id: i64,
    coin: String,
    level: i32,
    side: String,
    price: f64,
    size: f64,
    reduce_only: bool,
    status: String,
    exchange_oid: Option<i64>,
}

/// A new order to insert.
#[derive(Debug, Clone)]
pub struct NewGridOrder {
    pub coin: String,
    pub level: i32,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub reduce_only: bool,
}

/// Repository over the bot's persistent state.
#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connects to PostgreSQL and runs migrations.
    pub async fn connect(url: &str, max_connections: u32) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .connect(url)
            .await
            .context("connecting to PostgreSQL")?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("running migrations")?;
        Ok(Self { pool })
    }

    /// Builds a store from an existing pool (used in tests).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Runs the embedded migrations against the pool.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("running migrations")?;
        Ok(())
    }

    /// Inserts a new order with status `pending`, returning its row id.
    pub async fn insert_order(&self, order: &NewGridOrder) -> anyhow::Result<i64> {
        let rec = sqlx::query_scalar::<_, i64>(
            "INSERT INTO grid_orders (coin, level, side, price, size, reduce_only, status) \
             VALUES ($1, $2, $3, $4, $5, $6, 'pending') RETURNING id",
        )
        .bind(&order.coin)
        .bind(order.level)
        .bind(side_str(order.side))
        .bind(order.price)
        .bind(order.size)
        .bind(order.reduce_only)
        .fetch_one(&self.pool)
        .await
        .context("inserting order")?;
        Ok(rec)
    }

    /// Marks an order as resting on the exchange and records its exchange oid.
    pub async fn mark_open(&self, id: i64, exchange_oid: u64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE grid_orders SET status = 'open', exchange_oid = $1, updated_at = now() \
             WHERE id = $2",
        )
        .bind(exchange_oid as i64)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("marking order open")?;
        Ok(())
    }

    /// Updates an order's status by row id.
    pub async fn set_status(&self, id: i64, status: OrderStatus) -> anyhow::Result<()> {
        sqlx::query("UPDATE grid_orders SET status = $1, updated_at = now() WHERE id = $2")
            .bind(status.as_str())
            .bind(id)
            .execute(&self.pool)
            .await
            .context("updating order status")?;
        Ok(())
    }

    /// Looks up an order by its exchange oid.
    pub async fn order_by_exchange_oid(
        &self,
        exchange_oid: u64,
    ) -> anyhow::Result<Option<GridOrderRow>> {
        let row = sqlx::query_as::<_, GridOrderRaw>(
            "SELECT id, coin, level, side, price, size, reduce_only, status, exchange_oid \
             FROM grid_orders WHERE exchange_oid = $1",
        )
        .bind(exchange_oid as i64)
        .fetch_optional(&self.pool)
        .await
        .context("querying order by oid")?;
        Ok(row.map(GridOrderRow::from_row))
    }

    /// Lists all orders for `coin` that are currently `open`.
    pub async fn open_orders(&self, coin: &str) -> anyhow::Result<Vec<GridOrderRow>> {
        let rows = sqlx::query_as::<_, GridOrderRaw>(
            "SELECT id, coin, level, side, price, size, reduce_only, status, exchange_oid \
             FROM grid_orders WHERE coin = $1 AND status = 'open' ORDER BY level",
        )
        .bind(coin)
        .fetch_all(&self.pool)
        .await
        .context("listing open orders")?;
        Ok(rows.into_iter().map(GridOrderRow::from_row).collect())
    }

    /// Records a fill.
    pub async fn record_fill(
        &self,
        exchange_oid: u64,
        coin: &str,
        side: Side,
        price: f64,
        size: f64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO fills (exchange_oid, coin, side, price, size) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(exchange_oid as i64)
        .bind(coin)
        .bind(side_str(side))
        .bind(price)
        .bind(size)
        .execute(&self.pool)
        .await
        .context("recording fill")?;
        Ok(())
    }

    /// Stores a position snapshot.
    pub async fn snapshot_position(&self, pos: &Position) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO position_snapshots (coin, size, entry_price, unrealized_pnl) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&pos.coin)
        .bind(pos.size)
        .bind(pos.entry_price)
        .bind(pos.unrealized_pnl)
        .execute(&self.pool)
        .await
        .context("snapshotting position")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_side_round_trip() {
        assert_eq!(OrderStatus::Open.as_str(), "open");
        assert_eq!(parse_side("buy"), Side::Buy);
        assert_eq!(parse_side("SELL"), Side::Sell);
        assert_eq!(side_str(Side::Sell), "sell");
    }
}
