//! Hyperliquid implementation of the [`Exchange`] trait.

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use hypersdk::hypercore::types::{
    BatchCancel, BatchOrder, Cancel, OrderGrouping, OrderRequest, OrderResponseStatus,
    OrderTypePlacement, Side as HlSide, TimeInForce,
};
use hypersdk::hypercore::{self, HttpClient, PerpMarket, PrivateKeySigner};
use hypersdk::{Address, Decimal};
use rust_decimal::prelude::ToPrimitive;

use super::{Exchange, OpenOrder, OrderState, PlaceOrder, PlacedOrder, Position};
use crate::config::Network;
use crate::grid::Side;

/// Live Hyperliquid exchange client.
pub struct HyperliquidExchange {
    client: HttpClient,
    signer: PrivateKeySigner,
    user_address: Address,
    /// Maps a coin symbol (e.g. `"XMR"`) to its perp market metadata, which
    /// supplies the asset index plus the tick/lot sizing the signed
    /// order/cancel/leverage actions require.
    coin_to_market: HashMap<String, PerpMarket>,
}

/// Builds an HTTP client for `network`.
fn http_client(network: Network) -> HttpClient {
    match network {
        Network::Mainnet => hypercore::mainnet(),
        Network::Testnet => hypercore::testnet(),
    }
}

/// Hyperliquid encodes bids as buys and asks as sells.
fn map_side(side: HlSide) -> Side {
    match side {
        HlSide::Bid => Side::Buy,
        HlSide::Ask => Side::Sell,
    }
}

/// Converts a [`Decimal`] to `f64`, defaulting to `0.0` on the (practically
/// impossible) conversion failure so a single bad field can't abort a stream.
fn to_f64(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

/// Converts an `f64` price/size to the [`Decimal`] the exchange expects.
fn to_decimal(value: f64) -> anyhow::Result<Decimal> {
    Decimal::try_from(value).map_err(|e| anyhow!("invalid decimal {value}: {e}"))
}

/// A fresh, monotonically-increasing nonce based on the wall clock, as required
/// by the Hyperliquid signing scheme.
fn nonce() -> u64 {
    chrono::Utc::now().timestamp_millis() as u64
}

impl HyperliquidExchange {
    /// Connects to Hyperliquid using `private_key` on `network`.
    ///
    /// `account_address`, when non-empty, is the master account to run all info
    /// queries against; pass it when `private_key` is an agent / API wallet key
    /// whose derived address is not the funded account. When empty, the address
    /// derived from `private_key` is used.
    pub async fn new(
        private_key: &str,
        account_address: &str,
        network: Network,
    ) -> anyhow::Result<Self> {
        let signer: PrivateKeySigner = private_key
            .trim()
            .trim_start_matches("0x")
            .parse()
            .context("invalid private key")?;
        let user_address = if account_address.trim().is_empty() {
            signer.address()
        } else {
            account_address
                .trim()
                .parse()
                .context("invalid account_address")?
        };
        let client = http_client(network);

        // The perp metadata gives us the coin -> market mapping needed for
        // every signed action and for rounding prices/sizes to valid ticks.
        let perps = client
            .perps()
            .await
            .map_err(|e| anyhow!("fetching perp metadata: {e}"))?;
        let coin_to_market = perps.into_iter().map(|m| (m.name.clone(), m)).collect();

        tracing::info!(
            signer = %signer.address(),
            query_address = %user_address,
            "hyperliquid client ready"
        );

        Ok(Self {
            client,
            signer,
            user_address,
            coin_to_market,
        })
    }

    /// The address derived from the configured wallet.
    pub fn address(&self) -> Address {
        self.user_address
    }

    /// Resolves a coin symbol to its perp market metadata.
    fn market(&self, coin: &str) -> anyhow::Result<&PerpMarket> {
        self.coin_to_market
            .get(coin)
            .ok_or_else(|| anyhow!("unknown coin {coin}"))
    }

    /// Resolves a coin symbol to its numeric perp asset index.
    fn asset_index(&self, coin: &str) -> anyhow::Result<usize> {
        Ok(self.market(coin)?.index)
    }
}

#[async_trait]
impl Exchange for HyperliquidExchange {
    async fn mid_price(&self, coin: &str) -> anyhow::Result<f64> {
        let mids = self
            .client
            .all_mids(None)
            .await
            .map_err(|e| anyhow!("all_mids: {e}"))?;
        let px = mids
            .get(coin)
            .ok_or_else(|| anyhow!("no mid price for {coin}"))?;
        Ok(to_f64(*px))
    }

    async fn update_leverage(&self, coin: &str, leverage: u32, cross: bool) -> anyhow::Result<()> {
        let asset = self.asset_index(coin)?;
        self.client
            .update_leverage(&self.signer, asset, cross, leverage, nonce(), None, None)
            .await
            .map_err(|e| anyhow!("update_leverage: {e}"))?;
        Ok(())
    }

    async fn place_order(&self, req: &PlaceOrder) -> anyhow::Result<PlacedOrder> {
        let market = self.market(&req.coin)?;
        // Hyperliquid rejects orders whose price is not on a valid tick or whose
        // size exceeds the market's size precision; round both before signing.
        let price = market
            .round_price(to_decimal(req.price)?)
            .ok_or_else(|| anyhow!("invalid price {} for {}", req.price, req.coin))?;
        let sz_decimals = market.sz_decimals.clamp(0, u32::MAX as i64) as u32;
        let sz = to_decimal(req.size)?.round_dp(sz_decimals);
        let order = OrderRequest {
            asset: market.index,
            is_buy: req.side.is_buy(),
            limit_px: price,
            sz,
            reduce_only: req.reduce_only,
            order_type: OrderTypePlacement::Limit {
                tif: TimeInForce::Gtc,
            },
            cloid: Default::default(),
        };
        let batch = BatchOrder {
            orders: vec![order],
            grouping: OrderGrouping::Na,
            builder: None,
        };
        let statuses = self
            .client
            .place(&self.signer, batch, nonce(), None, None)
            .await
            .map_err(|e| anyhow!("placing order: {e}"))?;
        let status = statuses
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("order response had no statuses"))?;
        parse_place_response(status)
    }

    async fn cancel_order(&self, coin: &str, oid: u64) -> anyhow::Result<()> {
        let asset = self.asset_index(coin)?;
        let batch = BatchCancel {
            cancels: vec![Cancel { asset, oid }],
        };
        let statuses = self
            .client
            .cancel(&self.signer, batch, nonce(), None, None)
            .await
            .map_err(|e| anyhow!("cancelling order: {e}"))?;
        for status in statuses {
            if let OrderResponseStatus::Error(e) = status {
                return Err(anyhow!("cancel rejected: {e}"));
            }
        }
        Ok(())
    }

    async fn open_orders(&self, coin: &str) -> anyhow::Result<Vec<OpenOrder>> {
        let orders = self
            .client
            .open_orders(self.user_address, None)
            .await
            .map_err(|e| anyhow!("open_orders: {e}"))?;
        let mut out = Vec::new();
        for o in orders {
            if o.coin != coin {
                continue;
            }
            out.push(OpenOrder {
                oid: o.oid,
                coin: o.coin,
                side: map_side(o.side),
                price: to_f64(o.limit_px),
                size: to_f64(o.sz),
            });
        }
        Ok(out)
    }

    async fn order_status(&self, _coin: &str, oid: u64) -> anyhow::Result<OrderState> {
        match self
            .client
            .order_status(self.user_address, either::Left(oid))
            .await
            .map_err(|e| anyhow!("order_status: {e}"))?
        {
            None => Ok(OrderState::Cancelled),
            Some(update) => {
                if update.status.is_filled() {
                    Ok(OrderState::Filled)
                } else if update.status.is_finished() {
                    Ok(OrderState::Cancelled)
                } else if update.order.sz < update.order.orig_sz {
                    // Still resting but some quantity was consumed.
                    Ok(OrderState::PartialFill)
                } else {
                    Ok(OrderState::Open)
                }
            }
        }
    }

    async fn position(&self, coin: &str) -> anyhow::Result<Option<Position>> {
        let state = self
            .client
            .clearinghouse_state(self.user_address, None)
            .await
            .map_err(|e| anyhow!("clearinghouse_state: {e}"))?;
        for ap in state.asset_positions {
            let position = ap.position;
            if position.coin == coin {
                return Ok(Some(Position {
                    coin: coin.to_string(),
                    size: to_f64(position.szi),
                    entry_price: position.entry_px.map(to_f64),
                    unrealized_pnl: to_f64(position.unrealized_pnl),
                }));
            }
        }
        Ok(None)
    }
}

/// Interprets an order placement status, returning the resting/filled oid.
fn parse_place_response(status: OrderResponseStatus) -> anyhow::Result<PlacedOrder> {
    match status {
        OrderResponseStatus::Resting { oid, .. } => Ok(PlacedOrder { oid, resting: true }),
        OrderResponseStatus::Filled { oid, .. } => Ok(PlacedOrder {
            oid,
            resting: false,
        }),
        OrderResponseStatus::Error(e) => Err(anyhow!("order rejected: {e}")),
        other => Err(anyhow!("unexpected order status: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_hyperliquid_sides() {
        assert_eq!(map_side(HlSide::Bid), Side::Buy);
        assert_eq!(map_side(HlSide::Ask), Side::Sell);
    }

    #[test]
    fn parses_resting_and_filled_responses() {
        let resting = parse_place_response(OrderResponseStatus::Resting {
            oid: 7,
            cloid: None,
        })
        .unwrap();
        assert_eq!(
            resting,
            PlacedOrder {
                oid: 7,
                resting: true
            }
        );

        let filled = parse_place_response(OrderResponseStatus::Filled {
            total_sz: Decimal::ZERO,
            avg_px: Decimal::ZERO,
            oid: 9,
        })
        .unwrap();
        assert_eq!(
            filled,
            PlacedOrder {
                oid: 9,
                resting: false
            }
        );

        assert!(parse_place_response(OrderResponseStatus::Error("nope".into())).is_err());
    }

    #[test]
    fn converts_between_decimal_and_f64() {
        let d = to_decimal(123.5).unwrap();
        assert_eq!(to_f64(d), 123.5);
    }
}
