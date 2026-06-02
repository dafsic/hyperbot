//! Hyperbot entry point.

use std::sync::Arc;

use hyperbot::bot::Bot;
use hyperbot::config::Config;
use hyperbot::exchange::{Exchange, HyperliquidExchange};
use hyperbot::grid::GridStrategy;
use hyperbot::risk::RiskManager;
use hyperbot::store::Store;
use hyperbot::telemetry;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init();

    let config = Config::load()?;
    info!(
        coin = %config.grid.coin,
        network = ?config.exchange.network,
        mode = ?config.grid.mode,
        "starting hyperbot"
    );

    let strategy = GridStrategy::new(config.grid.to_params())
        .map_err(|e| anyhow::anyhow!("invalid grid: {e}"))?;
    let risk = RiskManager::new(config.risk.clone());

    let store = Store::connect(&config.database.url, config.database.max_connections).await?;

    let exchange = HyperliquidExchange::new(
        &config.exchange.private_key,
        &config.exchange.account_address,
        config.exchange.network,
    )
    .await?;
    let exchange: Arc<dyn Exchange> = Arc::new(exchange);

    let bot = Bot::new(
        exchange,
        store,
        strategy,
        risk,
        config.exchange.leverage,
        config.exchange.cross_margin,
    )
    .with_cancel_on_exit(config.exchange.cancel_on_exit);

    let shutdown = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to listen for ctrl_c: {e}");
        }
    };

    bot.run(shutdown).await?;
    info!("hyperbot stopped");
    Ok(())
}
