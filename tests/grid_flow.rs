//! Integration test for the end-to-end grid flow using the in-memory mock
//! exchange and a real PostgreSQL database.
//!
//! Set `TEST_DATABASE_URL` to a writable PostgreSQL instance to run it;
//! otherwise the test is skipped so the suite stays green without a database.

use std::sync::Arc;

use hyperbot::bot::Bot;
use hyperbot::config::RiskConfig;
use hyperbot::exchange::{Exchange, MockExchange};
use hyperbot::grid::{GridMode, GridParams, GridStrategy, Side, Spacing};
use hyperbot::risk::RiskManager;
use hyperbot::store::Store;
use sqlx::postgres::PgPoolOptions;

fn strategy() -> GridStrategy {
    GridStrategy::new(GridParams {
        coin: "XMR".into(),
        lower_price: 100.0,
        upper_price: 200.0,
        grid_count: 10,
        spacing: Spacing::Arithmetic,
        order_size: 1.0,
        mode: GridMode::ShortOnly,
    })
    .unwrap()
}

async fn make_store(url: &str) -> Store {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("connect");
    // Clean slate so the test is repeatable.
    sqlx::query(
        "DROP TABLE IF EXISTS grid_orders, fills, position_snapshots, bot_state, \
         _sqlx_migrations CASCADE",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = Store::from_pool(pool);
    store.migrate().await.unwrap();
    store
}

#[tokio::test]
async fn short_grid_seeds_sells_and_takes_profit() {
    let url = match std::env::var("TEST_DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("TEST_DATABASE_URL not set; skipping integration test");
            return;
        }
    };

    let store = make_store(&url).await;
    let mock = Arc::new(MockExchange::new(150.0));
    let exchange: Arc<dyn Exchange> = mock.clone();
    let bot = Bot::new(
        exchange,
        store.clone(),
        strategy(),
        RiskManager::new(RiskConfig::default()),
        1,
        false,
    );

    // Bootstrap: mid = 150, so sells should be seeded at levels 6..10 (5 of them).
    bot.bootstrap().await.unwrap();
    assert_eq!(mock.open_count(), 5, "5 sells above mid");
    assert_eq!(mock.net_position(), 0.0);

    // The lowest resting sell is at level 6 (price 160).
    let open = store.open_orders("XMR").await.unwrap();
    let lowest_sell = open.iter().min_by_key(|o| o.level).unwrap();
    assert_eq!(lowest_sell.level, 6);
    let oid = lowest_sell.exchange_oid.unwrap() as u64;

    // Fill it: short opened, and a reduce-only buy is seeded at level 5.
    let fill = mock.fill(oid).unwrap();
    bot.handle_fill(&fill).await.unwrap();
    assert_eq!(mock.net_position(), -1.0, "short opened");

    let open = store.open_orders("XMR").await.unwrap();
    let buy = open.iter().find(|o| o.level == 5).expect("buy at level 5");
    assert!(buy.reduce_only, "take-profit buy is reduce-only");
    assert_eq!(buy.side, Side::Buy);

    // Fill the buy: short closed, a fresh sell is re-seeded at level 6.
    let buy_oid = buy.exchange_oid.unwrap() as u64;
    let buy_fill = mock.fill(buy_oid).unwrap();
    bot.handle_fill(&buy_fill).await.unwrap();
    assert_eq!(mock.net_position(), 0.0, "short closed for profit");

    let open = store.open_orders("XMR").await.unwrap();
    assert!(
        open.iter().any(|o| o.level == 6 && o.side == Side::Sell),
        "sell re-seeded at level 6"
    );
}
