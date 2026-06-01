//! Integration tests for the end-to-end grid flow using the in-memory mock
//! exchange and a real PostgreSQL database.
//!
//! Set `TEST_DATABASE_URL` to a writable PostgreSQL instance to run them;
//! otherwise the tests are skipped so the suite stays green without a database.

use std::sync::Arc;

use hyperbot::bot::Bot;
use hyperbot::config::RiskConfig;
use hyperbot::exchange::{Exchange, MockExchange};
use hyperbot::grid::{GridMode, GridParams, GridStrategy, Side, Spacing};
use hyperbot::risk::RiskManager;
use hyperbot::store::Store;
use sqlx::postgres::PgPoolOptions;

/// Grid over 80..120 (step 10): levels 0=80, 1=90, 2=100, 3=110, 4=120.
fn strategy() -> GridStrategy {
    GridStrategy::new(GridParams {
        coin: "XMR".into(),
        lower_price: 80.0,
        upper_price: 120.0,
        grid_count: 4,
        spacing: Spacing::Arithmetic,
        order_size: 1.0,
        mode: GridMode::ShortOnly,
    })
    .unwrap()
}

fn make_bot(exchange: Arc<dyn Exchange>, store: Store) -> Bot {
    Bot::new(
        exchange,
        store,
        strategy(),
        RiskManager::new(RiskConfig::default()),
        1,
        false,
    )
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

fn db_url() -> Option<String> {
    match std::env::var("TEST_DATABASE_URL") {
        Ok(u) => Some(u),
        Err(_) => {
            eprintln!("TEST_DATABASE_URL not set; skipping integration test");
            None
        }
    }
}

/// Requirement 1: starting at mid = 100 over the 80..120 grid seeds two
/// immediate shorts (at 90 and 100), two resting sells (110, 120), and two
/// reduce-only take-profit buys (at 80 and 90).
#[tokio::test]
async fn startup_seeds_immediate_shorts_and_resting_grid() {
    let Some(url) = db_url() else { return };
    let store = make_store(&url).await;
    let mock = Arc::new(MockExchange::new(100.0));
    let exchange: Arc<dyn Exchange> = mock.clone();
    let bot = make_bot(exchange, store.clone());

    bot.bootstrap().await.unwrap();

    // Two shorts opened immediately at 90 and 100.
    assert_eq!(mock.net_position(), -2.0, "two shorts opened");

    let open = store.open_orders("XMR").await.unwrap();
    // Resting sells at levels 3 (110) and 4 (120).
    assert!(
        open.iter().any(|o| o.level == 3 && o.side == Side::Sell),
        "resting sell at 110"
    );
    assert!(
        open.iter().any(|o| o.level == 4 && o.side == Side::Sell),
        "resting sell at 120"
    );
    // Reduce-only take-profit buys at levels 0 (80) and 1 (90).
    let buy80 = open.iter().find(|o| o.level == 0).expect("buy at 80");
    assert_eq!(buy80.side, Side::Buy);
    assert!(buy80.reduce_only);
    let buy90 = open.iter().find(|o| o.level == 1).expect("buy at 90");
    assert_eq!(buy90.side, Side::Buy);
    assert!(buy90.reduce_only);
    // No resting sell at the immediate level 2 (it filled), and the lowest line
    // only hosts its take-profit buy.
    assert!(open.iter().all(|o| !(o.level == 2 && o.side == Side::Sell)));
    assert_eq!(open.len(), 4, "110/120 sells + 80/90 reduce-only buys");
}

/// Requirement 2: with orders preserved across a restart, the bot detects the
/// sells that filled while offline, closes the resulting shorts with reduce-only
/// buys at the current price, and re-seeds the sells once those buys fill.
#[tokio::test]
async fn restart_reconciles_offline_fills_and_reseeds() {
    let Some(url) = db_url() else { return };
    let store = make_store(&url).await;
    let mock = Arc::new(MockExchange::new(100.0));
    let exchange: Arc<dyn Exchange> = mock.clone();

    // --- First run: seed the grid at mid = 100. ---
    let bot = make_bot(exchange.clone(), store.clone());
    bot.bootstrap().await.unwrap();
    assert_eq!(mock.net_position(), -2.0);

    // --- While "offline": price rises and the resting 110 and 120 sells fill. ---
    let resting_sells: Vec<_> = store
        .open_orders("XMR")
        .await
        .unwrap()
        .into_iter()
        .filter(|o| o.side == Side::Sell)
        .collect();
    assert_eq!(resting_sells.len(), 2, "110 and 120 resting before restart");
    for o in &resting_sells {
        // Raise the mid above the order so it crosses, then fill it.
        mock.push_mid(o.price + 1.0);
        mock.fill(o.exchange_oid.unwrap() as u64).unwrap();
    }
    // Two more shorts opened offline (from the 110 and 120 sells).
    assert_eq!(mock.net_position(), -4.0, "shorts from 110 and 120 fills");

    // Price falls back to 100 before the restart.
    mock.push_mid(100.0);

    // --- Second run: a fresh bot reconciles and re-seeds. ---
    let bot2 = make_bot(exchange.clone(), store.clone());
    bot2.bootstrap().await.unwrap();

    // The 110/120 shorts are closed by reduce-only buys at the current price;
    // the original 90/100 shorts are left untouched (their take-profit buys are
    // still resting, so the grid is not re-opened). Net returns to -2.
    assert_eq!(
        mock.net_position(),
        -2.0,
        "offline shorts closed, existing grid untouched"
    );

    let open = store.open_orders("XMR").await.unwrap();
    // The 110 and 120 sells are re-seeded and resting again.
    assert!(
        open.iter().any(|o| o.level == 3 && o.side == Side::Sell),
        "sell re-seeded at 110"
    );
    assert!(
        open.iter().any(|o| o.level == 4 && o.side == Side::Sell),
        "sell re-seeded at 120"
    );
    // Take-profit buys for the live shorts rest at 80 and 90.
    assert!(open.iter().any(|o| o.level == 0 && o.side == Side::Buy));
    assert!(open.iter().any(|o| o.level == 1 && o.side == Side::Buy));
}

/// Exiting the bot leaves resting orders untouched when `cancel_on_exit` is the
/// default (false).
#[tokio::test]
async fn exit_keeps_resting_orders_by_default() {
    let Some(url) = db_url() else { return };
    let store = make_store(&url).await;
    let mock = Arc::new(MockExchange::new(100.0));
    let exchange: Arc<dyn Exchange> = mock.clone();
    let bot = make_bot(exchange, store.clone());

    bot.bootstrap().await.unwrap();
    let before = mock.open_count();
    assert!(before > 0, "grid seeded some resting orders");

    // Run with an already-resolved shutdown: the loop exits immediately.
    bot.run(async {}).await.unwrap();

    assert_eq!(
        mock.open_count(),
        before,
        "resting orders preserved on exit"
    );
    assert_eq!(
        store.open_orders("XMR").await.unwrap().len(),
        before,
        "DB still tracks the resting orders as open"
    );
}

/// With `cancel_on_exit` enabled the bot tears the grid down on shutdown.
#[tokio::test]
async fn exit_cancels_when_configured() {
    let Some(url) = db_url() else { return };
    let store = make_store(&url).await;
    let mock = Arc::new(MockExchange::new(100.0));
    let exchange: Arc<dyn Exchange> = mock.clone();
    let bot = make_bot(exchange, store.clone()).with_cancel_on_exit(true);

    bot.bootstrap().await.unwrap();
    assert!(mock.open_count() > 0);

    bot.run(async {}).await.unwrap();

    assert_eq!(mock.open_count(), 0, "all resting orders cancelled on exit");
}
