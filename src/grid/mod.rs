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
    /// For a short-only grid this seeds a sell order on every line strictly
    /// above the mid price. Buy (take-profit) legs are only created later, once
    /// a sell has been filled, so the net position can never become positive.
    pub fn initial_orders(&self, mid: f64, active_levels: &[usize]) -> Vec<DesiredOrder> {
        let size = self.params.order_size;
        let mut out = Vec::new();
        for (level, &price) in self.levels.iter().enumerate() {
            if active_levels.contains(&level) {
                continue;
            }
            match self.params.mode {
                GridMode::ShortOnly => {
                    if price > mid {
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
                    if price < mid {
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
    fn short_only_initial_orders_are_all_sells_above_mid() {
        let s = GridStrategy::new(short_params()).unwrap();
        // mid in the middle of the grid (150). Levels are 100,110,...,200.
        let orders = s.initial_orders(150.0, &[]);
        assert!(!orders.is_empty());
        for o in &orders {
            assert_eq!(o.side, Side::Sell);
            assert!(!o.reduce_only);
            assert!(o.price > 150.0);
        }
        // levels strictly above 150: 160,170,180,190,200 => 5 orders.
        assert_eq!(orders.len(), 5);
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
}
