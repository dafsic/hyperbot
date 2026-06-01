//! In-memory mock exchange used by unit and integration tests.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::{Exchange, ExchangeEvent, FillEvent, OpenOrder, PlaceOrder, PlacedOrder, Position};
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
    event_tx: Option<UnboundedSender<ExchangeEvent>>,
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
                event_tx: None,
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

    /// Simulates a fill of a resting order, updating the position and emitting a
    /// [`ExchangeEvent::Fill`] to any subscriber. Returns the fill event.
    pub fn fill(&self, oid: u64) -> Option<FillEvent> {
        let mut inner = self.inner.lock().unwrap();
        let order = inner.orders.remove(&oid)?;
        let signed = match order.side {
            Side::Buy => order.size,
            Side::Sell => -order.size,
        };
        inner.position += signed;
        let event = FillEvent {
            oid,
            coin: order.coin.clone(),
            side: order.side,
            price: order.price,
            size: order.size,
        };
        if let Some(tx) = &inner.event_tx {
            let _ = tx.send(ExchangeEvent::Fill(event.clone()));
        }
        Some(event)
    }

    /// Pushes a new mid price to subscribers.
    pub fn push_mid(&self, mid: f64) {
        let mut inner = self.inner.lock().unwrap();
        inner.mid = mid;
        if let Some(tx) = &inner.event_tx {
            let _ = tx.send(ExchangeEvent::Mid(mid));
        }
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

    async fn subscribe(&self, _coin: &str) -> anyhow::Result<UnboundedReceiver<ExchangeEvent>> {
        let (tx, rx) = unbounded_channel();
        self.inner.lock().unwrap().event_tx = Some(tx);
        Ok(rx)
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
        let fill = ex.fill(placed.oid).unwrap();
        assert_eq!(fill.side, Side::Sell);
        assert_eq!(ex.open_count(), 0);
        assert_eq!(ex.net_position(), -1.0);
    }
}
