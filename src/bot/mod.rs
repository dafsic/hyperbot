//! Bot orchestration: wires the exchange, persistence layer, grid strategy and
//! risk manager together into a long-running event loop.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::{error, info, warn};

use crate::exchange::{Exchange, FillEvent, PlaceOrder};
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
    /// Timestamp (ms) of the most recent fill processed, used as a watermark so
    /// periodic reconciliation only fetches fills newer than the last one seen
    /// instead of re-scanning the entire (capped but large) fill history each
    /// tick. Starts at `0` so the first reconcile pulls full history (catching
    /// fills that happened while the bot was offline).
    fill_watermark: AtomicU64,
}

/// Safety cap on how many follow-up orders a single submission may cascade
/// into, guarding against an unexpected configuration looping indefinitely.
/// A grid only ever cascades a couple of hops per fill, so 64 is far above any
/// legitimate chain while still bounding a runaway loop.
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
            fill_watermark: AtomicU64::new(0),
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
    /// caller can avoid double-placing them.
    ///
    /// Two things happen:
    ///
    /// * **Orphan cancellation** — exchange orders the bot does not track are
    ///   cancelled. This only ever cancels orders that ARE on the book, so it is
    ///   safe even if the book read is partial.
    /// * **Fill detection** — fills are detected from the exchange's own
    ///   authoritative fill history ([`Exchange::recent_fills`]), NOT by
    ///   inferring them from the absence of an order on the book. Inferring from
    ///   book absence is unsafe: a single incomplete `open_orders` response would
    ///   make every tracked order look filled and trigger a flood of spurious
    ///   counter orders. [`Bot::apply_fill`] deduplicates by order status, so a
    ///   fill already handled (live, immediately, or on a previous tick) is a
    ///   no-op; only genuinely missed fills get their counter leg seeded.
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

        // Authoritatively detect fills from the exchange's fill history and
        // replay any we missed through the strategy. `apply_fill` dedups by
        // status, so fills already processed are ignored; counters that cross
        // the current mid fill immediately and cascade inside `submit_order`.
        // The query is windowed by `fill_watermark` so each tick only pulls
        // fills newer than the last one processed, keeping the volume bounded no
        // matter how long the bot runs. The window is inclusive, so the boundary
        // fill may reappear; `apply_fill` makes that a no-op.
        let since = self.fill_watermark.load(Ordering::Relaxed);
        let fills = self.exchange.recent_fills(&coin, since).await?;
        if !fills.is_empty() {
            let mid = self.exchange.mid_price(&coin).await?;
            let mut max_time = since;
            for fill in &fills {
                if let Err(e) = self.apply_fill(fill, mid).await {
                    error!("failed to apply reconciled fill oid {}: {e}", fill.oid);
                }
                max_time = max_time.max(fill.time);
            }
            // Advance the watermark past everything we just saw so the next tick
            // only asks for newer fills.
            self.fill_watermark.fetch_max(max_time, Ordering::Relaxed);
        }

        // Levels still resting in the DB, so the caller can avoid double-seeding.
        let active_levels = self
            .store
            .open_orders(&coin)
            .await?
            .iter()
            .map(|o| o.level as usize)
            .collect();
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
    /// Skips an order whose (level, side) already rests. It also avoids
    /// re-opening a position the bot already holds across a restart:
    ///
    /// * An opening **sell** that crosses (short-only) is skipped when its
    ///   matching take-profit reduce-only buy one line below is already resting.
    /// * An opening **buy** that crosses (long-only) is skipped when its matching
    ///   take-profit reduce-only sell one line above is already resting.
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
        // An opening buy that crosses would open a long. If the matching
        // take-profit sell one line above is already resting, the long is
        // already open; don't open another.
        let last = self.strategy.levels().len().saturating_sub(1);
        if order.side == Side::Buy && !order.reduce_only && order.price >= mid && order.level < last
        {
            let above = order.level + 1;
            if open
                .iter()
                .any(|o| o.level as usize == above && o.side == Side::Sell && o.reduce_only)
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

    /// Applies a single fill: records it, marks the order filled and seeds the
    /// strategy's counter order(s), using `reference_price` to decide whether
    /// each counter rests or crosses.
    ///
    /// Idempotent: a fill whose order is already `filled` (handled immediately
    /// on placement or on a previous reconcile tick) is a no-op, so this is safe
    /// to call repeatedly from periodic reconciliation with overlapping fills. A
    /// fill for an order the bot does not track (e.g. unrelated history) is
    /// ignored.
    async fn apply_fill(&self, fill: &FillEvent, reference_price: f64) -> anyhow::Result<()> {
        let order = match self.store.order_by_exchange_oid(fill.oid).await? {
            Some(o) => o,
            None => {
                warn!(
                    oid = fill.oid,
                    coin = %fill.coin,
                    "fill has no matching tracked order; ignoring"
                );
                return Ok(());
            }
        };

        // Dedup: ignore fills for orders we already closed.
        if order.status == OrderStatus::Filled.as_str() {
            return Ok(());
        }

        self.store
            .record_fill(fill.oid, &fill.coin, order.side, fill.price, fill.size)
            .await?;
        self.store.set_status(order.id, OrderStatus::Filled).await?;
        info!(level = order.level, side = ?order.side, "order filled");

        for counter in self.strategy.on_fill(order.level as usize, order.side) {
            if let Err(e) = self.submit_order(counter.clone(), reference_price).await {
                error!(
                    "failed to submit counter order at level {}: {e}",
                    counter.level
                );
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

        let mut risk_tick = tokio::time::interval(std::time::Duration::from_secs(30));
        // The bot is poll-only: there is no websocket. Every 20s it reconciles
        // against the exchange's authoritative fill history (`recent_fills`),
        // detecting any resting order that filled and seeding its take-profit
        // counter leg. This is the sole fill-detection path (alongside orders
        // that fill immediately on placement). The first tick is delayed so it
        // does not duplicate the reconcile already done in `bootstrap`.
        let reconcile_period = std::time::Duration::from_secs(20);
        let mut reconcile_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + reconcile_period,
            reconcile_period,
        );
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
                _ = reconcile_tick.tick() => {
                    if let Err(e) = self.reconcile().await {
                        error!("periodic reconcile failed: {e}");
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
