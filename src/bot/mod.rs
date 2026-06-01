//! Bot orchestration: wires the exchange, persistence layer, grid strategy and
//! risk manager together into a long-running event loop.

use std::collections::VecDeque;
use std::sync::Arc;

use tracing::{error, info, warn};

use crate::exchange::{Exchange, ExchangeEvent, FillEvent, PlaceOrder};
use crate::grid::{DesiredOrder, GridStrategy, Side};
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

/// Safety cap on how many follow-up orders a single fill may cascade into,
/// guarding against an unexpected configuration looping indefinitely.
const MAX_FOLLOWUPS: usize = 64;

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
            cancel_on_exit: false,
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
            if let Err(e) = self.submit_order(order.clone(), mid).await {
                error!("failed to seed initial order at level {}: {e}", order.level);
            }
        }
        Ok(())
    }

    /// Reconciles persisted state with the live exchange.
    ///
    /// Returns the set of grid levels that already have a live order so the
    /// caller can avoid double-placing them. Exchange orders that the bot does
    /// not track are cancelled. Tracked orders that vanished from the book while
    /// the bot was offline are marked filled, and the counter order(s) the
    /// strategy dictates for those fills are seeded (this is how a short opened
    /// while offline gets its take-profit buy, or a take-profit that filled
    /// offline gets its sell re-placed).
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
        // we were offline) and mark them so, remembering the fills we need to
        // replay through the strategy once everything is reconciled.
        let live: std::collections::HashSet<i64> =
            exchange_orders.iter().map(|o| o.oid as i64).collect();
        let mut active_levels = Vec::new();
        let mut offline_fills: Vec<(usize, Side)> = Vec::new();
        for o in &db_orders {
            match o.exchange_oid {
                Some(oid) if live.contains(&oid) => active_levels.push(o.level as usize),
                Some(oid) => {
                    info!(level = o.level, "order no longer on book; marking filled");
                    self.store.set_status(o.id, OrderStatus::Filled).await?;
                    self.store
                        .record_fill(oid as u64, &coin, o.side, o.price, o.size)
                        .await?;
                    offline_fills.push((o.level as usize, o.side));
                }
                None => {}
            }
        }

        // Replay the offline fills: seed each counter order, letting any that
        // cross the current mid fill immediately and cascade.
        if !offline_fills.is_empty() {
            let mid = self.exchange.mid_price(&coin).await?;
            for (level, side) in offline_fills {
                for counter in self.strategy.on_fill(level, side) {
                    if let Err(e) = self.submit_order(counter.clone(), mid).await {
                        error!(
                            "failed to seed reconcile counter at level {}: {e}",
                            counter.level
                        );
                    }
                }
            }
            // Recompute the live levels after seeding follow-ups.
            active_levels = self
                .store
                .open_orders(&coin)
                .await?
                .iter()
                .map(|o| o.level as usize)
                .collect();
        }
        Ok(active_levels)
    }

    /// Submits a desired order, deciding from `mid` whether it rests on the book
    /// or crosses and fills immediately, and cascading any follow-up counter
    /// orders the strategy dictates for an immediate fill.
    ///
    /// A crossing order (a sell at/below the mid, or a buy at/above it) is sent
    /// as a marketable limit priced at the mid so it fills right away; the fill
    /// is recorded and the counter order(s) seeded, which may themselves cross.
    /// Resting orders are placed at their grid price. Orders that would duplicate
    /// an already-live leg are skipped (see [`Bot::should_skip`]).
    pub async fn submit_order(&self, order: DesiredOrder, mid: f64) -> anyhow::Result<()> {
        let mut queue: VecDeque<DesiredOrder> = VecDeque::new();
        queue.push_back(order);
        let mut processed = 0usize;
        while let Some(o) = queue.pop_front() {
            processed += 1;
            if processed > MAX_FOLLOWUPS {
                warn!("follow-up cascade exceeded {MAX_FOLLOWUPS}; stopping");
                break;
            }
            if self.should_skip(&o, mid).await? {
                continue;
            }
            // Cross the spread (fill now) for a sell at/below or a buy at/above
            // the mid; otherwise rest at the grid price.
            let crosses = match o.side {
                Side::Sell => o.price <= mid,
                Side::Buy => o.price >= mid,
            };
            let place_price = if crosses { mid } else { o.price };
            let placed = match self.place_and_persist(&o, place_price).await {
                Ok(p) => p,
                Err(e) => {
                    error!("failed to place order at level {}: {e}", o.level);
                    continue;
                }
            };
            // The exchange tells us whether it rested or filled immediately.
            if !placed.resting {
                self.store
                    .record_fill(placed.oid, self.coin(), o.side, place_price, o.size)
                    .await?;
                if let Some(row) = self.store.order_by_exchange_oid(placed.oid).await? {
                    self.store.set_status(row.id, OrderStatus::Filled).await?;
                }
                info!(
                    level = o.level,
                    side = ?o.side,
                    price = place_price,
                    "order filled immediately"
                );
                for counter in self.strategy.on_fill(o.level, o.side) {
                    queue.push_back(counter);
                }
            }
        }
        Ok(())
    }

    /// Decides whether a desired order would duplicate work already on the book.
    ///
    /// Skips an order whose (level, side) already rests, and skips an
    /// open-short sell whose short is already open — represented by a resting
    /// reduce-only buy one line below — so a restart does not re-open a position
    /// the bot is already holding.
    async fn should_skip(&self, order: &DesiredOrder, mid: f64) -> anyhow::Result<bool> {
        let open = self.store.open_orders(self.coin()).await?;
        if open
            .iter()
            .any(|o| o.level as usize == order.level && o.side == order.side)
        {
            return Ok(true);
        }
        // An opening sell that crosses would open a short. If the matching
        // take-profit buy one line below is already resting, the short is
        // already open; don't open another.
        if order.side == Side::Sell && !order.reduce_only && order.price <= mid && order.level >= 1
        {
            let below = order.level - 1;
            if open
                .iter()
                .any(|o| o.level as usize == below && o.side == Side::Buy && o.reduce_only)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Places an order on the exchange at `price` and persists it, returning the
    /// exchange acknowledgement (which reports whether it rested or filled).
    pub async fn place_and_persist(
        &self,
        order: &DesiredOrder,
        price: f64,
    ) -> anyhow::Result<crate::exchange::PlacedOrder> {
        let coin = self.coin().to_string();
        let new_order = NewGridOrder {
            coin: coin.clone(),
            level: order.level as i32,
            side: order.side,
            price,
            size: order.size,
            reduce_only: order.reduce_only,
        };
        let id = self.store.insert_order(&new_order).await?;

        let req = PlaceOrder {
            coin,
            side: order.side,
            price,
            size: order.size,
            reduce_only: order.reduce_only,
        };
        match self.exchange.place_order(&req).await {
            Ok(placed) => {
                self.store.mark_open(id, placed.oid).await?;
                info!(
                    level = order.level,
                    side = ?order.side,
                    price,
                    oid = placed.oid,
                    resting = placed.resting,
                    "order placed"
                );
                Ok(placed)
            }
            Err(e) => {
                self.store.set_status(id, OrderStatus::Cancelled).await?;
                Err(e)
            }
        }
    }

    /// Handles a fill: records it, marks the order filled and submits the
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

        // Use the fill price as the market reference for the counter legs.
        let counters = self.strategy.on_fill(order.level as usize, order.side);
        for c in counters {
            if let Err(e) = self.submit_order(c.clone(), fill.price).await {
                error!("failed to submit counter order at level {}: {e}", c.level);
            }
        }
        Ok(())
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
