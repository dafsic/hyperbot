//! In-memory mock exchange used by unit and integration tests.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;

use super::{Exchange, OpenOrder, OrderState, PlaceOrder, PlacedOrder, Position};
use crate::grid::Side;

/// Deterministic, in-memory implementation of [`Exchange`].
///
/// Orders are assigned monotonically increasing ids and rest on the book until
/// explicitly cancelled or filled via [`MockExchange::fill`].
pub struct MockExchange {
    inner: Mutex<Inner>,
}

struct Inner {
    next_oid: u64,
    mid: f64,
    orders: HashMap<u64, OpenOrder>,
    position: f64,
    leverage: u32,
    /// Oids that filled completely.
    filled_oids: HashSet<u64>,
    /// Oids that are still resting but have had at least one partial fill.
    partial_oids: HashSet<u64>,
}

impl MockExchange {
    /// Creates a mock with an initial `mid` price.
    pub fn new(mid: f64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_oid: 1,
                mid,
                orders: HashMap::new(),
                position: 0.0,
                leverage: 1,
                filled_oids: HashSet::new(),
                partial_oids: HashSet::new(),
            }),
        }
    }

    /// Returns the ids of all resting orders.
    pub fn resting_oids(&self) -> Vec<u64> {
        let inner = self.inner.lock().unwrap();
        let mut v: Vec<u64> = inner.orders.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Returns the number of resting orders.
    pub fn open_count(&self) -> usize {
        self.inner.lock().unwrap().orders.len()
    }

    /// Returns the current net position.
    pub fn net_position(&self) -> f64 {
        self.inner.lock().unwrap().position
    }

    /// Returns the configured leverage.
    pub fn leverage(&self) -> u32 {
        self.inner.lock().unwrap().leverage
    }

    /// Simulates an external fill of a resting order, updating the position.
    /// Returns `None` if `oid` is not currently resting.
    pub fn fill(&self, oid: u64) -> Option<()> {
        let mut inner = self.inner.lock().unwrap();
        let order = inner.orders.remove(&oid)?;
        let signed = match order.side {
            Side::Buy => order.size,
            Side::Sell => -order.size,
        };
        inner.position += signed;
        inner.partial_oids.remove(&oid);
        inner.filled_oids.insert(oid);
        Some(())
    }

    /// Simulates a partial fill of a resting order, reducing its remaining
    /// size by `qty` and updating the position. Returns `None` if `oid` is
    /// not currently resting.
    pub fn partial_fill(&self, oid: u64, qty: f64) -> Option<()> {
        let mut inner = self.inner.lock().unwrap();
        // Destructure to avoid overlapping mutable borrows.
        let order = inner.orders.get(&oid)?;
        let side = order.side;
        let remaining = order.size - qty;
        let signed = match side {
            Side::Buy => qty,
            Side::Sell => -qty,
        };
        inner.position += signed;
        inner.orders.get_mut(&oid).unwrap().size = remaining;
        inner.partial_oids.insert(oid);
        Some(())
    }

    /// Sets a new mid price.
    pub fn push_mid(&self, mid: f64) {
        self.inner.lock().unwrap().mid = mid;
    }
}

#[async_trait]
impl Exchange for MockExchange {
    async fn mid_price(&self, _coin: &str) -> anyhow::Result<f64> {
        Ok(self.inner.lock().unwrap().mid)
    }

    async fn update_leverage(
        &self,
        _coin: &str,
        leverage: u32,
        _cross: bool,
    ) -> anyhow::Result<()> {
        self.inner.lock().unwrap().leverage = leverage;
        Ok(())
    }

    async fn place_order(&self, req: &PlaceOrder) -> anyhow::Result<PlacedOrder> {
        let mut inner = self.inner.lock().unwrap();
        let oid = inner.next_oid;
        inner.next_oid += 1;
        // A crossing limit order fills immediately against the current mid: a
        // sell priced at or below the mid, or a buy priced at or above it. Such
        // an order never rests; it updates the position right away. This mirrors
        // a live venue and lets the bot open shorts (or close them) on startup.
        let crosses = match req.side {
            Side::Sell => req.price <= inner.mid,
            Side::Buy => req.price >= inner.mid,
        };
        if crosses {
            let signed = match req.side {
                Side::Buy => req.size,
                Side::Sell => -req.size,
            };
            inner.position += signed;
            inner.filled_oids.insert(oid);
            return Ok(PlacedOrder {
                oid,
                resting: false,
            });
        }
        inner.orders.insert(
            oid,
            OpenOrder {
                oid,
                coin: req.coin.clone(),
                side: req.side,
                price: req.price,
                size: req.size,
            },
        );
        Ok(PlacedOrder { oid, resting: true })
    }

    async fn cancel_order(&self, _coin: &str, oid: u64) -> anyhow::Result<()> {
        self.inner.lock().unwrap().orders.remove(&oid);
        Ok(())
    }

    async fn open_orders(&self, coin: &str) -> anyhow::Result<Vec<OpenOrder>> {
        let inner = self.inner.lock().unwrap();
        let mut v: Vec<OpenOrder> = inner
            .orders
            .values()
            .filter(|o| o.coin == coin)
            .cloned()
            .collect();
        v.sort_by_key(|o| o.oid);
        Ok(v)
    }

    async fn order_status(&self, _coin: &str, oid: u64) -> anyhow::Result<OrderState> {
        let inner = self.inner.lock().unwrap();
        if inner.orders.contains_key(&oid) {
            if inner.partial_oids.contains(&oid) {
                Ok(OrderState::PartialFill)
            } else {
                Ok(OrderState::Open)
            }
        } else if inner.filled_oids.contains(&oid) {
            Ok(OrderState::Filled)
        } else {
            Ok(OrderState::Cancelled)
        }
    }

    async fn position(&self, coin: &str) -> anyhow::Result<Option<Position>> {
        let inner = self.inner.lock().unwrap();
        if inner.position == 0.0 {
            return Ok(None);
        }
        Ok(Some(Position {
            coin: coin.to_string(),
            size: inner.position,
            entry_price: Some(inner.mid),
            unrealized_pnl: 0.0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn places_and_fills_orders() {
        let ex = MockExchange::new(150.0);
        let placed = ex
            .place_order(&PlaceOrder {
                coin: "XMR".into(),
                side: Side::Sell,
                price: 160.0,
                size: 1.0,
                reduce_only: false,
            })
            .await
            .unwrap();
        assert_eq!(ex.open_count(), 1);
        ex.fill(placed.oid).unwrap();
        assert_eq!(ex.open_count(), 0);
        assert_eq!(ex.net_position(), -1.0);
    }
}
