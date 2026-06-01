//! Hyperliquid implementation of the [`Exchange`] trait.

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::StreamExt;
use hypersdk::hypercore::types::{
    BatchCancel, BatchOrder, Cancel, Incoming, OrderGrouping, OrderRequest, OrderResponseStatus,
    OrderTypePlacement, Side as HlSide, Subscription, TimeInForce, UserEvent,
};
use hypersdk::hypercore::{self, HttpClient, PrivateKeySigner, WebSocket};
use hypersdk::{Address, Decimal};
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tracing::warn;

use super::{Exchange, ExchangeEvent, FillEvent, OpenOrder, PlaceOrder, PlacedOrder, Position};
use crate::config::Network;
use crate::grid::Side;

/// Live Hyperliquid exchange client.
pub struct HyperliquidExchange {
    client: HttpClient,
    signer: PrivateKeySigner,
    user_address: Address,
    network: Network,
    /// Maps a coin symbol (e.g. `"XMR"`) to its numeric perp asset index, which
    /// the signed order/cancel/leverage actions require.
    coin_to_asset: HashMap<String, usize>,
}

/// Builds an HTTP client for `network`.
fn http_client(network: Network) -> HttpClient {
    match network {
        Network::Mainnet => hypercore::mainnet(),
        Network::Testnet => hypercore::testnet(),
    }
}

/// Builds a websocket connection for `network`.
fn websocket(network: Network) -> WebSocket {
    match network {
        Network::Mainnet => hypercore::mainnet_ws(),
        Network::Testnet => hypercore::testnet_ws(),
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
    pub async fn new(private_key: &str, network: Network) -> anyhow::Result<Self> {
        let signer: PrivateKeySigner = private_key
            .trim()
            .trim_start_matches("0x")
            .parse()
            .context("invalid private key")?;
        let user_address = signer.address();
        let client = http_client(network);

        // The perp metadata gives us the coin -> asset-index mapping needed for
        // every signed action.
        let perps = client
            .perps()
            .await
            .map_err(|e| anyhow!("fetching perp metadata: {e}"))?;
        let coin_to_asset = perps.into_iter().map(|m| (m.name, m.index)).collect();

        Ok(Self {
            client,
            signer,
            user_address,
            network,
            coin_to_asset,
        })
    }

    /// The address derived from the configured wallet.
    pub fn address(&self) -> Address {
        self.user_address
    }

    /// Resolves a coin symbol to its numeric perp asset index.
    fn asset_index(&self, coin: &str) -> anyhow::Result<usize> {
        self.coin_to_asset
            .get(coin)
            .copied()
            .ok_or_else(|| anyhow!("unknown coin {coin}"))
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
        let asset = self.asset_index(&req.coin)?;
        let order = OrderRequest {
            asset,
            is_buy: req.side.is_buy(),
            limit_px: to_decimal(req.price)?,
            sz: to_decimal(req.size)?,
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

    async fn subscribe(&self, coin: &str) -> anyhow::Result<UnboundedReceiver<ExchangeEvent>> {
        // A dedicated websocket connection owns the subscription so the shared
        // `self.client` used for REST queries is untouched.
        let mut ws = websocket(self.network);
        ws.subscribe(Subscription::AllMids { dex: None });
        ws.subscribe(Subscription::UserEvents {
            user: self.user_address,
        });

        let (tx, rx) = unbounded_channel();
        let coin = coin.to_string();
        tokio::spawn(async move {
            // `ws` is moved in to keep the connection alive for the task.
            while let Some(event) = ws.next().await {
                let message = match event {
                    hypercore::ws::Event::Message(message) => message,
                    hypercore::ws::Event::Connected => continue,
                    hypercore::ws::Event::Disconnected => {
                        warn!("hyperliquid websocket disconnected; reconnecting");
                        continue;
                    }
                };
                match message {
                    Incoming::AllMids { mids, .. } => {
                        if let Some(px) = mids.get(&coin) {
                            if tx.send(ExchangeEvent::Mid(to_f64(*px))).is_err() {
                                break;
                            }
                        }
                    }
                    Incoming::UserEvents(UserEvent::Fills { fills }) => {
                        for fill in fills {
                            if fill.coin != coin {
                                continue;
                            }
                            let event = FillEvent {
                                oid: fill.oid,
                                coin: fill.coin.clone(),
                                side: map_side(fill.side),
                                price: to_f64(fill.px),
                                size: to_f64(fill.sz),
                            };
                            if tx.send(ExchangeEvent::Fill(event)).is_err() {
                                return;
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(rx)
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
        assert_eq!(resting, PlacedOrder { oid: 7, resting: true });

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
