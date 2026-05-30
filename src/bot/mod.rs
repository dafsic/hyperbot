//! Bot orchestration: wires the exchange, persistence layer, grid strategy and
//! risk manager together into a long-running event loop.

use std::sync::Arc;

use tracing::{error, info, warn};

use crate::exchange::{Exchange, ExchangeEvent, FillEvent, PlaceOrder};
use crate::grid::{DesiredOrder, GridStrategy};
use crate::risk::{RiskManager, RiskVerdict};
use crate::store::{NewGridOrder, OrderStatus, Store};

/// The grid bot.
pub struct Bot {
    exchange: Arc<dyn Exchange>,
    store: Store,
    strategy: GridStrategy,
    risk: RiskManager,
    leverage: u32,
    cross_margin: bool,
    /// Whether to cancel all resting orders on graceful shutdown.
    cancel_on_exit: bool,
}

impl Bot {
    /// Creates a new bot.
    pub fn new(
        exchange: Arc<dyn Exchange>,
        store: Store,
        strategy: GridStrategy,
        risk: RiskManager,
        leverage: u32,
        cross_margin: bool,
    ) -> Self {
        Self {
            exchange,
            store,
            strategy,
            risk,
            leverage,
            cross_margin,
            cancel_on_exit: true,
        }
    }

    /// Overrides whether resting orders are cancelled on shutdown.
    pub fn with_cancel_on_exit(mut self, value: bool) -> Self {
        self.cancel_on_exit = value;
        self
    }

    fn coin(&self) -> &str {
        &self.strategy.params().coin
    }

    /// Sets leverage, reconciles with the exchange and seeds the initial grid.
    pub async fn bootstrap(&self) -> anyhow::Result<()> {
        let coin = self.coin().to_string();
        info!(coin = %coin, leverage = self.leverage, "configuring leverage");
        if let Err(e) = self
            .exchange
            .update_leverage(&coin, self.leverage, self.cross_margin)
            .await
        {
            // Leverage may already be set; treat as non-fatal but visible.
            warn!("could not set leverage: {e}");
        }

        let active_levels = self.reconcile().await?;
        let mid = self.exchange.mid_price(&coin).await?;
        info!(mid, active = active_levels.len(), "seeding grid");

        let initial = self.strategy.initial_orders(mid, &active_levels);
        for order in initial {
            if let Err(e) = self.place_and_persist(&order).await {
                error!(
                    "failed to place initial order at level {}: {e}",
                    order.level
                );
            }
        }
        Ok(())
    }

    /// Reconciles persisted state with the live exchange.
    ///
    /// Returns the set of grid levels that already have a live order so the
    /// caller can avoid double-placing them. Exchange orders that the bot does
    /// not track are cancelled.
    pub async fn reconcile(&self) -> anyhow::Result<Vec<usize>> {
        let coin = self.coin().to_string();
        let db_orders = self.store.open_orders(&coin).await?;
        let exchange_orders = self.exchange.open_orders(&coin).await?;

        let tracked: std::collections::HashSet<i64> =
            db_orders.iter().filter_map(|o| o.exchange_oid).collect();

        // Cancel orphan exchange orders we no longer track.
        for o in &exchange_orders {
            if !tracked.contains(&(o.oid as i64)) {
                warn!(oid = o.oid, "cancelling orphan exchange order");
                if let Err(e) = self.exchange.cancel_order(&coin, o.oid).await {
                    error!("failed to cancel orphan order {}: {e}", o.oid);
                }
            }
        }

        // Detect tracked orders that have vanished from the book (filled while
        // we were offline) and mark them so.
        let live: std::collections::HashSet<i64> =
            exchange_orders.iter().map(|o| o.oid as i64).collect();
        let mut active_levels = Vec::new();
        for o in &db_orders {
            match o.exchange_oid {
                Some(oid) if live.contains(&oid) => active_levels.push(o.level as usize),
                Some(_) => {
                    info!(level = o.level, "order no longer on book; marking filled");
                    self.store.set_status(o.id, OrderStatus::Filled).await?;
                }
                None => {}
            }
        }
        Ok(active_levels)
    }

    /// Places an order on the exchange and persists it.
    pub async fn place_and_persist(&self, order: &DesiredOrder) -> anyhow::Result<()> {
        let coin = self.coin().to_string();
        let new_order = NewGridOrder {
            coin: coin.clone(),
            level: order.level as i32,
            side: order.side,
            price: order.price,
            size: order.size,
            reduce_only: order.reduce_only,
        };
        let id = self.store.insert_order(&new_order).await?;

        let req = PlaceOrder {
            coin,
            side: order.side,
            price: order.price,
            size: order.size,
            reduce_only: order.reduce_only,
        };
        match self.exchange.place_order(&req).await {
            Ok(placed) => {
                self.store.mark_open(id, placed.oid).await?;
                info!(
                    level = order.level,
                    side = ?order.side,
                    price = order.price,
                    oid = placed.oid,
                    "order placed"
                );
                Ok(())
            }
            Err(e) => {
                self.store.set_status(id, OrderStatus::Cancelled).await?;
                Err(e)
            }
        }
    }

    /// Handles a fill: records it, marks the order filled and places the
    /// counter order(s) dictated by the strategy.
    pub async fn handle_fill(&self, fill: &FillEvent) -> anyhow::Result<()> {
        self.store
            .record_fill(fill.oid, &fill.coin, fill.side, fill.price, fill.size)
            .await?;

        let order = match self.store.order_by_exchange_oid(fill.oid).await? {
            Some(o) => o,
            None => {
                warn!(oid = fill.oid, "fill for untracked order; ignoring");
                return Ok(());
            }
        };

        // Ignore duplicate fills for orders we already closed.
        if order.status == OrderStatus::Filled.as_str() {
            return Ok(());
        }
        self.store.set_status(order.id, OrderStatus::Filled).await?;
        info!(level = order.level, side = ?order.side, "order filled");

        let counters = self.strategy.on_fill(order.level as usize, order.side);
        for c in counters {
            if self.has_open_at_level(c.level).await? {
                continue; // avoid duplicating an already-live level
            }
            if let Err(e) = self.place_and_persist(&c).await {
                error!("failed to place counter order at level {}: {e}", c.level);
            }
        }
        Ok(())
    }

    async fn has_open_at_level(&self, level: usize) -> anyhow::Result<bool> {
        let open = self.store.open_orders(self.coin()).await?;
        Ok(open.iter().any(|o| o.level as usize == level))
    }

    /// Snapshots the current position and evaluates risk limits, returning the
    /// verdict.
    pub async fn check_risk(&self) -> anyhow::Result<RiskVerdict> {
        let coin = self.coin().to_string();
        let position = self.exchange.position(&coin).await?;
        let (size, pnl) = match &position {
            Some(p) => (p.size, p.unrealized_pnl),
            None => (0.0, 0.0),
        };
        if let Some(p) = &position {
            let _ = self.store.snapshot_position(p).await;
        }
        let verdict = self.risk.evaluate(size, pnl);
        if let RiskVerdict::Breached(reason) = &verdict {
            warn!("risk limit breached: {reason}");
        }
        Ok(verdict)
    }

    /// Cancels every order the bot currently tracks as open.
    pub async fn cancel_all(&self) -> anyhow::Result<()> {
        let coin = self.coin().to_string();
        for o in self.store.open_orders(&coin).await? {
            if let Some(oid) = o.exchange_oid {
                if let Err(e) = self.exchange.cancel_order(&coin, oid as u64).await {
                    error!("failed to cancel order {oid}: {e}");
                } else {
                    self.store.set_status(o.id, OrderStatus::Cancelled).await?;
                }
            }
        }
        Ok(())
    }

    /// Runs the bot until `shutdown` resolves.
    pub async fn run<F>(&self, shutdown: F) -> anyhow::Result<()>
    where
        F: std::future::Future<Output = ()> + Send,
    {
        self.bootstrap().await?;
        let coin = self.coin().to_string();
        let mut events = self.exchange.subscribe(&coin).await?;

        let mut risk_tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    info!("shutdown requested");
                    break;
                }
                _ = risk_tick.tick() => {
                    match self.check_risk().await {
                        Ok(RiskVerdict::Breached(reason)) => {
                            error!("tripping circuit breaker: {reason}");
                            let _ = self.cancel_all().await;
                            break;
                        }
                        Ok(RiskVerdict::Ok) => {}
                        Err(e) => error!("risk check failed: {e}"),
                    }
                }
                maybe_event = events.recv() => {
                    match maybe_event {
                        Some(ExchangeEvent::Fill(fill)) => {
                            if let Err(e) = self.handle_fill(&fill).await {
                                error!("error handling fill: {e}");
                            }
                        }
                        Some(ExchangeEvent::Mid(_mid)) => {}
                        None => {
                            warn!("event stream closed; stopping");
                            break;
                        }
                    }
                }
            }
        }

        if self.cancel_on_exit {
            info!("cancelling resting orders before exit");
            let _ = self.cancel_all().await;
        }
        Ok(())
    }

    /// Convenience accessor for the strategy.
    pub fn strategy(&self) -> &GridStrategy {
        &self.strategy
    }
}
