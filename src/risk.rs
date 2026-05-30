//! Risk controls.
//!
//! The [`RiskManager`] is a small, pure component that decides whether the bot
//! may keep trading or must trip its circuit breaker and stop opening new
//! positions.

use crate::config::RiskConfig;

/// Outcome of a risk evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum RiskVerdict {
    /// Everything within limits.
    Ok,
    /// A limit was breached; the contained message explains which one.
    Breached(String),
}

impl RiskVerdict {
    /// Returns `true` if a limit was breached.
    pub fn is_breached(&self) -> bool {
        matches!(self, RiskVerdict::Breached(_))
    }
}

/// Evaluates positions and PnL against the configured limits.
#[derive(Debug, Clone)]
pub struct RiskManager {
    cfg: RiskConfig,
}

impl RiskManager {
    /// Creates a new manager from configuration.
    pub fn new(cfg: RiskConfig) -> Self {
        Self { cfg }
    }

    /// Evaluates the current absolute position size and unrealised PnL.
    ///
    /// * `position_size` — signed position (contracts); the absolute value is
    ///   checked against `max_position`.
    /// * `unrealized_pnl` — current unrealised PnL in USDC (negative = loss).
    pub fn evaluate(&self, position_size: f64, unrealized_pnl: f64) -> RiskVerdict {
        if self.cfg.max_position > 0.0 && position_size.abs() > self.cfg.max_position {
            return RiskVerdict::Breached(format!(
                "position {:.4} exceeds max_position {:.4}",
                position_size.abs(),
                self.cfg.max_position
            ));
        }
        if self.cfg.max_loss_usd > 0.0 && unrealized_pnl < -self.cfg.max_loss_usd {
            return RiskVerdict::Breached(format!(
                "unrealised loss {:.2} exceeds max_loss_usd {:.2}",
                -unrealized_pnl, self.cfg.max_loss_usd
            ));
        }
        RiskVerdict::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RiskConfig {
        RiskConfig {
            max_position: 5.0,
            max_loss_usd: 100.0,
            max_leverage: 10,
        }
    }

    #[test]
    fn ok_within_limits() {
        let rm = RiskManager::new(cfg());
        assert_eq!(rm.evaluate(3.0, -50.0), RiskVerdict::Ok);
        assert_eq!(rm.evaluate(-4.9, 20.0), RiskVerdict::Ok);
    }

    #[test]
    fn breaches_on_position() {
        let rm = RiskManager::new(cfg());
        assert!(rm.evaluate(-6.0, 0.0).is_breached());
    }

    #[test]
    fn breaches_on_loss() {
        let rm = RiskManager::new(cfg());
        assert!(rm.evaluate(0.0, -150.0).is_breached());
    }

    #[test]
    fn zero_limits_disable_checks() {
        let rm = RiskManager::new(RiskConfig {
            max_position: 0.0,
            max_loss_usd: 0.0,
            max_leverage: 0,
        });
        assert_eq!(rm.evaluate(1_000.0, -1_000_000.0), RiskVerdict::Ok);
    }
}
