//! Hyperliquid implementation of the [`Exchange`] trait.

use std::str::FromStr;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use ethers::signers::{LocalWallet, Signer};
use ethers::types::H160;
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient, Message, Subscription, UserData,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tracing::{error, warn};

use super::{Exchange, ExchangeEvent, FillEvent, OpenOrder, PlaceOrder, PlacedOrder, Position};
use crate::config::Network;
use crate::grid::Side;

/// Live Hyperliquid exchange client.
pub struct HyperliquidExchange {
    info: InfoClient,
    exchange: ExchangeClient,
    user_address: H160,
    base_url: BaseUrl,
}

fn base_url(network: Network) -> BaseUrl {
    match network {
        Network::Mainnet => BaseUrl::Mainnet,
        Network::Testnet => BaseUrl::Testnet,
    }
}

/// Hyperliquid encodes bids (buys) as side `"B"` and asks (sells) as `"A"`.
fn parse_side(side: &str) -> Side {
    if side.eq_ignore_ascii_case("B") {
        Side::Buy
    } else {
        Side::Sell
    }
}

impl HyperliquidExchange {
    /// Connects to Hyperliquid using `private_key` on `network`.
    pub async fn new(private_key: &str, network: Network) -> anyhow::Result<Self> {
        let wallet = LocalWallet::from_str(private_key.trim_start_matches("0x"))
            .context("invalid private key")?;
        let user_address = wallet.address();
        let url = base_url(network);

        let info = InfoClient::new(None, Some(url))
            .await
            .map_err(|e| anyhow!("creating info client: {e}"))?;
        let exchange = ExchangeClient::new(None, wallet, Some(url), None, None)
            .await
            .map_err(|e| anyhow!("creating exchange client: {e}"))?;

        Ok(Self {
            info,
            exchange,
            user_address,
            base_url: url,
        })
    }

    /// The address derived from the configured wallet.
    pub fn address(&self) -> H160 {
        self.user_address
    }
}

#[async_trait]
impl Exchange for HyperliquidExchange {
    async fn mid_price(&self, coin: &str) -> anyhow::Result<f64> {
        let mids = self
            .info
            .all_mids()
            .await
            .map_err(|e| anyhow!("all_mids: {e}"))?;
        let raw = mids
            .get(coin)
            .ok_or_else(|| anyhow!("no mid price for {coin}"))?;
        raw.parse::<f64>().context("parsing mid price")
    }

    async fn update_leverage(&self, coin: &str, leverage: u32, cross: bool) -> anyhow::Result<()> {
        self.exchange
            .update_leverage(leverage, coin, cross, None)
            .await
            .map_err(|e| anyhow!("update_leverage: {e}"))?;
        Ok(())
    }

    async fn place_order(&self, req: &PlaceOrder) -> anyhow::Result<PlacedOrder> {
        let order = ClientOrderRequest {
            asset: req.coin.clone(),
            is_buy: req.side.is_buy(),
            reduce_only: req.reduce_only,
            limit_px: req.price,
            sz: req.size,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Gtc".to_string(),
            }),
        };
        let resp = self
            .exchange
            .order(order, None)
            .await
            .map_err(|e| anyhow!("placing order: {e}"))?;
        parse_place_response(resp)
    }

    async fn cancel_order(&self, coin: &str, oid: u64) -> anyhow::Result<()> {
        let resp = self
            .exchange
            .cancel(
                ClientCancelRequest {
                    asset: coin.to_string(),
                    oid,
                },
                None,
            )
            .await
            .map_err(|e| anyhow!("cancelling order: {e}"))?;
        match resp {
            ExchangeResponseStatus::Ok(_) => Ok(()),
            ExchangeResponseStatus::Err(e) => Err(anyhow!("cancel rejected: {e}")),
        }
    }

    async fn open_orders(&self, coin: &str) -> anyhow::Result<Vec<OpenOrder>> {
        let orders = self
            .info
            .open_orders(self.user_address)
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
                side: parse_side(&o.side),
                price: o.limit_px.parse().unwrap_or(0.0),
                size: o.sz.parse().unwrap_or(0.0),
            });
        }
        Ok(out)
    }

    async fn position(&self, coin: &str) -> anyhow::Result<Option<Position>> {
        let state = self
            .info
            .user_state(self.user_address)
            .await
            .map_err(|e| anyhow!("user_state: {e}"))?;
        for ap in state.asset_positions {
            if ap.position.coin == coin {
                let size = ap.position.szi.parse::<f64>().unwrap_or(0.0);
                let entry_price = ap
                    .position
                    .entry_px
                    .as_deref()
                    .and_then(|s| s.parse::<f64>().ok());
                let unrealized_pnl = ap.position.unrealized_pnl.parse::<f64>().unwrap_or(0.0);
                return Ok(Some(Position {
                    coin: coin.to_string(),
                    size,
                    entry_price,
                    unrealized_pnl,
                }));
            }
        }
        Ok(None)
    }

    async fn subscribe(&self, coin: &str) -> anyhow::Result<UnboundedReceiver<ExchangeEvent>> {
        // A dedicated InfoClient owns the websocket subscription so that the
        // shared `self.info` used for REST queries is never mutably borrowed.
        let mut ws = InfoClient::with_reconnect(None, Some(self.base_url))
            .await
            .map_err(|e| anyhow!("creating ws client: {e}"))?;

        let (raw_tx, mut raw_rx) = unbounded_channel();
        ws.subscribe(Subscription::AllMids, raw_tx.clone())
            .await
            .map_err(|e| anyhow!("subscribe AllMids: {e}"))?;
        ws.subscribe(
            Subscription::UserEvents {
                user: self.user_address,
            },
            raw_tx,
        )
        .await
        .map_err(|e| anyhow!("subscribe UserEvents: {e}"))?;

        let (tx, rx) = unbounded_channel();
        let coin = coin.to_string();
        tokio::spawn(async move {
            // Keep the websocket client alive for the lifetime of the task.
            let _ws = ws;
            while let Some(message) = raw_rx.recv().await {
                match message {
                    Message::AllMids(all_mids) => {
                        if let Some(mid) = all_mids.data.mids.get(&coin) {
                            if let Ok(px) = mid.parse::<f64>() {
                                if tx.send(ExchangeEvent::Mid(px)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Message::User(user) => {
                        if let UserData::Fills(fills) = user.data {
                            for fill in fills {
                                if fill.coin != coin {
                                    continue;
                                }
                                let event = FillEvent {
                                    oid: fill.oid,
                                    coin: fill.coin.clone(),
                                    side: parse_side(&fill.side),
                                    price: fill.px.parse().unwrap_or(0.0),
                                    size: fill.sz.parse().unwrap_or(0.0),
                                };
                                if tx.send(ExchangeEvent::Fill(event)).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Message::NoData => {
                        warn!("websocket returned NoData (disconnected)");
                    }
                    Message::HyperliquidError(e) => error!("hyperliquid ws error: {e}"),
                    _ => {}
                }
            }
        });

        Ok(rx)
    }
}

/// Interprets an order placement response, returning the resting/filled oid.
fn parse_place_response(resp: ExchangeResponseStatus) -> anyhow::Result<PlacedOrder> {
    match resp {
        ExchangeResponseStatus::Ok(resp) => {
            let data = resp
                .data
                .ok_or_else(|| anyhow!("order response missing data"))?;
            let status = data
                .statuses
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("order response had no statuses"))?;
            match status {
                ExchangeDataStatus::Resting(o) => Ok(PlacedOrder {
                    oid: o.oid,
                    resting: true,
                }),
                ExchangeDataStatus::Filled(o) => Ok(PlacedOrder {
                    oid: o.oid,
                    resting: false,
                }),
                ExchangeDataStatus::Error(e) => Err(anyhow!("order rejected: {e}")),
                other => Err(anyhow!("unexpected order status: {other:?}")),
            }
        }
        ExchangeResponseStatus::Err(e) => Err(anyhow!("order request failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hyperliquid_sides() {
        assert_eq!(parse_side("B"), Side::Buy);
        assert_eq!(parse_side("b"), Side::Buy);
        assert_eq!(parse_side("A"), Side::Sell);
    }

    #[test]
    fn maps_networks_to_base_urls() {
        // Smoke test that both variants are handled.
        let _ = base_url(Network::Mainnet);
        let _ = base_url(Network::Testnet);
    }
}
