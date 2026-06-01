//! Pure grid strategy logic.
//!
//! This module contains **no I/O**: given the grid parameters and market /
//! fill events it computes the orders that should be placed. That makes the
//! core trading logic deterministic and unit testable.

use serde::{Deserialize, Serialize};

/// Spacing between grid lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Spacing {
    /// Equal absolute distance between levels (等差网格).
    #[default]
    Arithmetic,
    /// Equal ratio between levels (等比网格).
    Geometric,
}

/// Direction the bot is allowed to trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GridMode {
    /// Only ever hold a long (net positive) position.
    LongOnly,
    /// Only ever hold a short (net negative) position (只做空，单边持仓).
    #[default]
    ShortOnly,
    /// Allow both long and short legs around the mid price.
    Neutral,
}

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Returns `true` for [`Side::Buy`].
    pub fn is_buy(self) -> bool {
        matches!(self, Side::Buy)
    }
}

/// Parameters describing a grid.
#[derive(Debug, Clone, PartialEq)]
pub struct GridParams {
    /// Perp coin symbol, e.g. `"XMR"`.
    pub coin: String,
    /// Lower bound of the grid (price).
    pub lower_price: f64,
    /// Upper bound of the grid (price).
    pub upper_price: f64,
    /// Number of intervals. The grid therefore has `grid_count + 1` lines.
    pub grid_count: usize,
    /// Spacing strategy.
    pub spacing: Spacing,
    /// Order size (contracts) placed at every grid line.
    pub order_size: f64,
    /// Trade direction restriction.
    pub mode: GridMode,
}

impl GridParams {
    /// Validates the parameters, returning a descriptive error if invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.coin.trim().is_empty() {
            return Err("coin must not be empty".into());
        }
        if self.lower_price <= 0.0 || self.upper_price <= 0.0 {
            return Err("prices must be positive".into());
        }
        if self.upper_price <= self.lower_price {
            return Err("upper_price must be greater than lower_price".into());
        }
        if self.grid_count == 0 {
            return Err("grid_count must be at least 1".into());
        }
        if self.order_size <= 0.0 {
            return Err("order_size must be positive".into());
        }
        Ok(())
    }
}

/// An order the strategy wants to place.
#[derive(Debug, Clone, PartialEq)]
pub struct DesiredOrder {
    /// Index of the grid line this order sits on (`0..=grid_count`).
    pub level: usize,
    /// Limit price (equal to the grid line price).
    pub price: f64,
    /// Side of the order.
    pub side: Side,
    /// Order size in contracts.
    pub size: f64,
    /// Whether the order only reduces an existing position (take-profit leg).
    pub reduce_only: bool,
}

/// The grid strategy: holds the immutable parameters and the pre-computed
/// price of every grid line.
#[derive(Debug, Clone)]
pub struct GridStrategy {
    params: GridParams,
    levels: Vec<f64>,
}

impl GridStrategy {
    /// Builds a strategy, validating parameters and pre-computing levels.
    pub fn new(params: GridParams) -> Result<Self, String> {
        params.validate()?;
        let levels = build_levels(
            params.lower_price,
            params.upper_price,
            params.grid_count,
            params.spacing,
        );
        Ok(Self { params, levels })
    }

    /// Grid parameters.
    pub fn params(&self) -> &GridParams {
        &self.params
    }

    /// Prices of every grid line, ascending (`grid_count + 1` entries).
    pub fn levels(&self) -> &[f64] {
        &self.levels
    }

    /// Index of the highest grid line.
    fn last_level(&self) -> usize {
        self.levels.len() - 1
    }

    /// Computes the set of orders to place when (re)starting the grid given the
    /// current `mid` price and the set of levels that already have a live order.
    ///
    /// All three modes share the same idea (思路): seed the opening leg on every
    /// relevant grid line and let the caller decide, from the mid, whether each
    /// order rests on the book or crosses and fills immediately. Immediate fills
    /// open the position right away; the caller then seeds the matching
    /// take-profit counter leg via [`GridStrategy::on_fill`].
    ///
    /// * **Short-only**: seeds a sell (open-short) on every line but the lowest
    ///   (which only ever hosts a take-profit buy). Lines at or below the mid
    ///   cross and open shorts immediately; lines above rest. Buy legs are only
    ///   ever created in response to a sell fill, so the net position can never
    ///   become positive.
    /// * **Long-only** (mirror of short-only): seeds a buy (open-long) on every
    ///   line but the highest (which only ever hosts a take-profit sell). Lines
    ///   at or above the mid cross and open longs immediately; lines below rest.
    ///   Sell legs are only ever created in response to a buy fill, so the net
    ///   position can never become negative.
    /// * **Neutral**: seeds buys below the mid (open long as price drops) and
    ///   sells above the mid (open short as price rises). Neither side crosses at
    ///   startup, so the grid opens no position until the market moves into a
    ///   resting order; the position may then swing either way around the mid.
    pub fn initial_orders(&self, mid: f64, active_levels: &[usize]) -> Vec<DesiredOrder> {
        let size = self.params.order_size;
        let last = self.last_level();
        let mut out = Vec::new();
        for (level, &price) in self.levels.iter().enumerate() {
            if active_levels.contains(&level) {
                continue;
            }
            match self.params.mode {
                GridMode::ShortOnly => {
                    // Seed a sell on every line but the lowest. Whether it rests
                    // or fills immediately is decided by the caller from the mid.
                    if level >= 1 {
                        out.push(DesiredOrder {
                            level,
                            price,
                            side: Side::Sell,
                            size,
                            reduce_only: false,
                        });
                    }
                }
                GridMode::LongOnly => {
                    // Seed a buy on every line but the highest. Whether it rests
                    // or fills immediately (opening a long) is decided by the
                    // caller from the mid: buys at or above the mid cross, buys
                    // below rest.
                    if level < last {
                        out.push(DesiredOrder {
                            level,
                            price,
                            side: Side::Buy,
                            size,
                            reduce_only: false,
                        });
                    }
                }
                GridMode::Neutral => {
                    if price < mid {
                        out.push(DesiredOrder {
                            level,
                            price,
                            side: Side::Buy,
                            size,
                            reduce_only: false,
                        });
                    } else if price > mid {
                        out.push(DesiredOrder {
                            level,
                            price,
                            side: Side::Sell,
                            size,
                            reduce_only: false,
                        });
                    }
                }
            }
        }
        out
    }

    /// Given a fill on `level` of `side`, returns the counter order(s) to place.
    ///
    /// Short-only example: a sell filled at level `i` (short opened) seeds a
    /// reduce-only buy at level `i-1` to take profit; once that buy fills (short
    /// closed) a fresh sell is placed back at level `i`.
    pub fn on_fill(&self, level: usize, side: Side) -> Vec<DesiredOrder> {
        let size = self.params.order_size;
        let last = self.last_level();
        let mut out = Vec::new();
        match (self.params.mode, side) {
            // ----- short-only -----
            (GridMode::ShortOnly, Side::Sell) => {
                if level >= 1 {
                    let l = level - 1;
                    out.push(DesiredOrder {
                        level: l,
                        price: self.levels[l],
                        side: Side::Buy,
                        size,
                        reduce_only: true,
                    });
                }
            }
            (GridMode::ShortOnly, Side::Buy) => {
                if level < last {
                    let l = level + 1;
                    out.push(DesiredOrder {
                        level: l,
                        price: self.levels[l],
                        side: Side::Sell,
                        size,
                        reduce_only: false,
                    });
                }
            }
            // ----- long-only -----
            (GridMode::LongOnly, Side::Buy) => {
                if level < last {
                    let l = level + 1;
                    out.push(DesiredOrder {
                        level: l,
                        price: self.levels[l],
                        side: Side::Sell,
                        size,
                        reduce_only: true,
                    });
                }
            }
            (GridMode::LongOnly, Side::Sell) => {
                if level >= 1 {
                    let l = level - 1;
                    out.push(DesiredOrder {
                        level: l,
                        price: self.levels[l],
                        side: Side::Buy,
                        size,
                        reduce_only: false,
                    });
                }
            }
            // ----- neutral -----
            (GridMode::Neutral, Side::Buy) => {
                if level < last {
                    let l = level + 1;
                    out.push(DesiredOrder {
                        level: l,
                        price: self.levels[l],
                        side: Side::Sell,
                        size,
                        reduce_only: false,
                    });
                }
            }
            (GridMode::Neutral, Side::Sell) => {
                if level >= 1 {
                    let l = level - 1;
                    out.push(DesiredOrder {
                        level: l,
                        price: self.levels[l],
                        side: Side::Buy,
                        size,
                        reduce_only: false,
                    });
                }
            }
        }
        out
    }
}

/// Computes the prices of every grid line.
///
/// Returns `count + 1` ascending prices, with the first equal to `lower` and
/// the last equal to `upper`.
pub fn build_levels(lower: f64, upper: f64, count: usize, spacing: Spacing) -> Vec<f64> {
    let n = count.max(1);
    let mut levels = Vec::with_capacity(n + 1);
    match spacing {
        Spacing::Arithmetic => {
            let step = (upper - lower) / n as f64;
            for i in 0..=n {
                levels.push(lower + step * i as f64);
            }
        }
        Spacing::Geometric => {
            let ratio = (upper / lower).powf(1.0 / n as f64);
            for i in 0..=n {
                levels.push(lower * ratio.powi(i as i32));
            }
        }
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    fn short_params() -> GridParams {
        GridParams {
            coin: "XMR".into(),
            lower_price: 100.0,
            upper_price: 200.0,
            grid_count: 10,
            spacing: Spacing::Arithmetic,
            order_size: 1.0,
            mode: GridMode::ShortOnly,
        }
    }

    #[test]
    fn arithmetic_levels_are_evenly_spaced() {
        let levels = build_levels(100.0, 200.0, 10, Spacing::Arithmetic);
        assert_eq!(levels.len(), 11);
        assert_relative_eq!(levels[0], 100.0);
        assert_relative_eq!(levels[10], 200.0);
        assert_relative_eq!(levels[1], 110.0);
        for w in levels.windows(2) {
            assert_relative_eq!(w[1] - w[0], 10.0, epsilon = 1e-9);
        }
    }

    #[test]
    fn geometric_levels_have_constant_ratio() {
        let levels = build_levels(100.0, 200.0, 10, Spacing::Geometric);
        assert_eq!(levels.len(), 11);
        assert_relative_eq!(levels[0], 100.0);
        assert_relative_eq!(levels[10], 200.0, epsilon = 1e-9);
        let ratio = levels[1] / levels[0];
        for w in levels.windows(2) {
            assert_relative_eq!(w[1] / w[0], ratio, epsilon = 1e-9);
        }
    }

    #[test]
    fn validate_rejects_bad_params() {
        let mut p = short_params();
        p.upper_price = 50.0;
        assert!(p.validate().is_err());
        p = short_params();
        p.grid_count = 0;
        assert!(p.validate().is_err());
        p = short_params();
        p.order_size = 0.0;
        assert!(p.validate().is_err());
        assert!(short_params().validate().is_ok());
    }

    #[test]
    fn short_only_initial_orders_seed_a_sell_on_every_line_but_the_lowest() {
        let s = GridStrategy::new(short_params()).unwrap();
        // mid in the middle of the grid (150). Levels are 100,110,...,200.
        let orders = s.initial_orders(150.0, &[]);
        assert!(!orders.is_empty());
        for o in &orders {
            assert_eq!(o.side, Side::Sell);
            assert!(!o.reduce_only);
        }
        // A sell is seeded on every line except the lowest (level 0 == price 100):
        // levels 1..=10 => 10 orders.
        assert_eq!(orders.len(), 10);
        assert!(orders.iter().all(|o| o.level != 0));
        // Lines at or below the mid (110..=150) are seeded too; the bot fills
        // those immediately. Here level 1 (price 110) up to level 5 (price 150).
        assert!(orders.iter().any(|o| o.level == 1 && o.price <= 150.0));
        assert!(orders.iter().any(|o| o.level == 5 && o.price <= 150.0));
    }

    #[test]
    fn short_only_skips_already_active_levels() {
        let s = GridStrategy::new(short_params()).unwrap();
        let all = s.initial_orders(150.0, &[]);
        let with_active = s.initial_orders(150.0, &[10]); // level 10 == price 200
        assert_eq!(with_active.len(), all.len() - 1);
        assert!(with_active.iter().all(|o| o.level != 10));
    }

    #[test]
    fn short_only_seeds_a_mid_range_grid() {
        // Requirement scenario: mid = 100, region 80..120 step 10.
        let s = GridStrategy::new(GridParams {
            coin: "XMR".into(),
            lower_price: 80.0,
            upper_price: 120.0,
            grid_count: 4,
            spacing: Spacing::Arithmetic,
            order_size: 1.0,
            mode: GridMode::ShortOnly,
        })
        .unwrap();
        // Levels: 0=80, 1=90, 2=100, 3=110, 4=120.
        let orders = s.initial_orders(100.0, &[]);
        // Sells on every line but the lowest (80): 90, 100, 110, 120 => 4 orders.
        assert_eq!(orders.len(), 4);
        assert!(orders
            .iter()
            .all(|o| o.side == Side::Sell && !o.reduce_only));
        // Lines at or below mid (90, 100) are crossing orders -> immediate short.
        assert!(orders.iter().any(|o| o.level == 1 && o.price <= 100.0));
        assert!(orders.iter().any(|o| o.level == 2 && o.price <= 100.0));
        // Lines above mid (110, 120) rest on the book.
        assert!(orders.iter().any(|o| o.level == 3 && o.price > 100.0));
        assert!(orders.iter().any(|o| o.level == 4 && o.price > 100.0));
        // The lowest line (80) only ever hosts a take-profit buy.
        assert!(orders.iter().all(|o| o.level != 0));
    }

    #[test]
    fn short_only_sell_fill_seeds_reduce_only_buy_below() {
        let s = GridStrategy::new(short_params()).unwrap();
        let counter = s.on_fill(5, Side::Sell);
        assert_eq!(counter.len(), 1);
        assert_eq!(counter[0].side, Side::Buy);
        assert!(counter[0].reduce_only);
        assert_eq!(counter[0].level, 4);
        assert_relative_eq!(counter[0].price, 140.0);
    }

    #[test]
    fn short_only_buy_fill_seeds_sell_above() {
        let s = GridStrategy::new(short_params()).unwrap();
        let counter = s.on_fill(4, Side::Buy);
        assert_eq!(counter.len(), 1);
        assert_eq!(counter[0].side, Side::Sell);
        assert!(!counter[0].reduce_only);
        assert_eq!(counter[0].level, 5);
    }

    #[test]
    fn short_only_never_buys_below_bottom_or_sells_above_top() {
        let s = GridStrategy::new(short_params()).unwrap();
        // sell filled at bottom level cannot seed a buy below it.
        assert!(s.on_fill(0, Side::Sell).is_empty());
        // buy filled at top level cannot seed a sell above it.
        assert!(s.on_fill(s.last_level(), Side::Buy).is_empty());
    }

    fn long_params() -> GridParams {
        GridParams {
            mode: GridMode::LongOnly,
            ..short_params()
        }
    }

    #[test]
    fn long_only_initial_orders_seed_a_buy_on_every_line_but_the_highest() {
        let s = GridStrategy::new(long_params()).unwrap();
        // mid in the middle of the grid (150). Levels are 100,110,...,200.
        let orders = s.initial_orders(150.0, &[]);
        assert!(!orders.is_empty());
        for o in &orders {
            assert_eq!(o.side, Side::Buy);
            assert!(!o.reduce_only);
        }
        // A buy is seeded on every line except the highest (level 10 == price
        // 200): levels 0..=9 => 10 orders.
        assert_eq!(orders.len(), 10);
        assert!(orders.iter().all(|o| o.level != 10));
        // Lines at or above the mid (150..=190) are crossing orders that fill
        // immediately, opening the long. Here level 5 (price 150) up to level 9.
        assert!(orders.iter().any(|o| o.level == 5 && o.price >= 150.0));
        assert!(orders.iter().any(|o| o.level == 9 && o.price >= 150.0));
    }

    #[test]
    fn long_only_skips_already_active_levels() {
        let s = GridStrategy::new(long_params()).unwrap();
        let all = s.initial_orders(150.0, &[]);
        let with_active = s.initial_orders(150.0, &[0]); // level 0 == price 100
        assert_eq!(with_active.len(), all.len() - 1);
        assert!(with_active.iter().all(|o| o.level != 0));
    }

    #[test]
    fn long_only_seeds_a_mid_range_grid() {
        // Mirror of the short-only mid-range scenario: mid = 100, region 80..120.
        let s = GridStrategy::new(GridParams {
            lower_price: 80.0,
            upper_price: 120.0,
            grid_count: 4,
            mode: GridMode::LongOnly,
            ..short_params()
        })
        .unwrap();
        // Levels: 0=80, 1=90, 2=100, 3=110, 4=120.
        let orders = s.initial_orders(100.0, &[]);
        // Buys on every line but the highest (120): 80, 90, 100, 110 => 4 orders.
        assert_eq!(orders.len(), 4);
        assert!(orders.iter().all(|o| o.side == Side::Buy && !o.reduce_only));
        // Lines at or above mid (100, 110) are crossing orders -> immediate long.
        assert!(orders.iter().any(|o| o.level == 2 && o.price >= 100.0));
        assert!(orders.iter().any(|o| o.level == 3 && o.price >= 100.0));
        // Lines below mid (80, 90) rest on the book.
        assert!(orders.iter().any(|o| o.level == 0 && o.price < 100.0));
        assert!(orders.iter().any(|o| o.level == 1 && o.price < 100.0));
        // The highest line (120) only ever hosts a take-profit sell.
        assert!(orders.iter().all(|o| o.level != 4));
    }

    #[test]
    fn long_only_buy_fill_seeds_reduce_only_sell_above() {
        let s = GridStrategy::new(long_params()).unwrap();
        let counter = s.on_fill(5, Side::Buy);
        assert_eq!(counter.len(), 1);
        assert_eq!(counter[0].side, Side::Sell);
        assert!(counter[0].reduce_only);
        assert_eq!(counter[0].level, 6);
        assert_relative_eq!(counter[0].price, 160.0);
    }

    #[test]
    fn long_only_sell_fill_seeds_buy_below() {
        let s = GridStrategy::new(long_params()).unwrap();
        let counter = s.on_fill(6, Side::Sell);
        assert_eq!(counter.len(), 1);
        assert_eq!(counter[0].side, Side::Buy);
        assert!(!counter[0].reduce_only);
        assert_eq!(counter[0].level, 5);
    }

    #[test]
    fn long_only_never_sells_above_top_or_buys_below_bottom() {
        let s = GridStrategy::new(long_params()).unwrap();
        // buy filled at top level cannot seed a take-profit sell above it.
        assert!(s.on_fill(s.last_level(), Side::Buy).is_empty());
        // sell filled at bottom level cannot seed a buy below it.
        assert!(s.on_fill(0, Side::Sell).is_empty());
    }

    fn neutral_params() -> GridParams {
        GridParams {
            mode: GridMode::Neutral,
            ..short_params()
        }
    }

    #[test]
    fn neutral_initial_orders_buy_below_and_sell_above_mid() {
        let s = GridStrategy::new(neutral_params()).unwrap();
        // Levels 100,110,...,200; mid = 150 sits exactly on level 5.
        let orders = s.initial_orders(150.0, &[]);
        // Buys below mid: 100..140 (levels 0..=4). Sells above mid: 160..200
        // (levels 6..=10). The level exactly at the mid (150) is left untouched.
        assert!(orders
            .iter()
            .all(|o| (o.side == Side::Buy && o.price < 150.0)
                || (o.side == Side::Sell && o.price > 150.0)));
        assert!(orders.iter().all(|o| !o.reduce_only));
        assert!(orders.iter().any(|o| o.level == 4 && o.side == Side::Buy));
        assert!(orders.iter().any(|o| o.level == 6 && o.side == Side::Sell));
        // Nothing is seeded exactly at the mid line.
        assert!(orders.iter().all(|o| o.level != 5));
        // Five buys (levels 0..=4) and five sells (levels 6..=10).
        assert_eq!(orders.len(), 10);
    }

    #[test]
    fn neutral_fills_oscillate_without_reduce_only() {
        let s = GridStrategy::new(neutral_params()).unwrap();
        // A buy fill seeds a (non-reduce-only) sell one line above.
        let after_buy = s.on_fill(4, Side::Buy);
        assert_eq!(after_buy.len(), 1);
        assert_eq!(after_buy[0].side, Side::Sell);
        assert_eq!(after_buy[0].level, 5);
        assert!(!after_buy[0].reduce_only);
        // A sell fill seeds a (non-reduce-only) buy one line below.
        let after_sell = s.on_fill(6, Side::Sell);
        assert_eq!(after_sell.len(), 1);
        assert_eq!(after_sell[0].side, Side::Buy);
        assert_eq!(after_sell[0].level, 5);
        assert!(!after_sell[0].reduce_only);
    }

    #[test]
    fn neutral_respects_grid_boundaries() {
        let s = GridStrategy::new(neutral_params()).unwrap();
        // buy filled at the top cannot seed a sell above it.
        assert!(s.on_fill(s.last_level(), Side::Buy).is_empty());
        // sell filled at the bottom cannot seed a buy below it.
        assert!(s.on_fill(0, Side::Sell).is_empty());
    }
}
