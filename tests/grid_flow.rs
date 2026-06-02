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
fn strategy_with(mode: GridMode) -> GridStrategy {
    GridStrategy::new(GridParams {
        coin: "XMR".into(),
        lower_price: 80.0,
        upper_price: 120.0,
        grid_count: 4,
        spacing: Spacing::Arithmetic,
        order_size: 1.0,
        mode,
    })
    .unwrap()
}

fn make_bot(exchange: Arc<dyn Exchange>, store: Store) -> Bot {
    make_bot_with(exchange, store, GridMode::ShortOnly)
}

fn make_bot_with(exchange: Arc<dyn Exchange>, store: Store, mode: GridMode) -> Bot {
    Bot::new(
        exchange,
        store,
        strategy_with(mode),
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
        "DROP TABLE IF EXISTS grid_orders, fills, position_snapshots, \
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

/// Long-only mirror of `startup_seeds_immediate_shorts_and_resting_grid`:
/// starting at mid = 100 over the 80..120 grid seeds two immediate longs (at 100
/// and 110), two resting buys (80, 90), and two reduce-only take-profit sells
/// (at 110 and 120).
#[tokio::test]
async fn startup_seeds_immediate_longs_and_resting_grid() {
    let Some(url) = db_url() else { return };
    let store = make_store(&url).await;
    let mock = Arc::new(MockExchange::new(100.0));
    let exchange: Arc<dyn Exchange> = mock.clone();
    let bot = make_bot_with(exchange, store.clone(), GridMode::LongOnly);

    bot.bootstrap().await.unwrap();

    // Two longs opened immediately at 100 and 110.
    assert_eq!(mock.net_position(), 2.0, "two longs opened");

    let open = store.open_orders("XMR").await.unwrap();
    // Resting buys at levels 0 (80) and 1 (90).
    assert!(
        open.iter().any(|o| o.level == 0 && o.side == Side::Buy),
        "resting buy at 80"
    );
    assert!(
        open.iter().any(|o| o.level == 1 && o.side == Side::Buy),
        "resting buy at 90"
    );
    // Reduce-only take-profit sells at levels 3 (110) and 4 (120).
    let sell110 = open.iter().find(|o| o.level == 3).expect("sell at 110");
    assert_eq!(sell110.side, Side::Sell);
    assert!(sell110.reduce_only);
    let sell120 = open.iter().find(|o| o.level == 4).expect("sell at 120");
    assert_eq!(sell120.side, Side::Sell);
    assert!(sell120.reduce_only);
    // No resting buy at the immediate level 2 (it filled), and the highest line
    // only hosts its take-profit sell.
    assert!(open.iter().all(|o| !(o.level == 2 && o.side == Side::Buy)));
    assert_eq!(open.len(), 4, "80/90 buys + 110/120 reduce-only sells");
}

/// Long-only mirror of `restart_reconciles_offline_fills_and_reseeds`: resting
/// buys that fill while offline are reconciled by closing the longs with
/// reduce-only sells and re-seeding the buys, leaving the existing grid intact.
#[tokio::test]
async fn restart_reconciles_offline_long_fills_and_reseeds() {
    let Some(url) = db_url() else { return };
    let store = make_store(&url).await;
    let mock = Arc::new(MockExchange::new(100.0));
    let exchange: Arc<dyn Exchange> = mock.clone();

    // --- First run: seed the grid at mid = 100. ---
    let bot = make_bot_with(exchange.clone(), store.clone(), GridMode::LongOnly);
    bot.bootstrap().await.unwrap();
    assert_eq!(mock.net_position(), 2.0);

    // --- While "offline": price falls and the resting 80 and 90 buys fill. ---
    let resting_buys: Vec<_> = store
        .open_orders("XMR")
        .await
        .unwrap()
        .into_iter()
        .filter(|o| o.side == Side::Buy)
        .collect();
    assert_eq!(resting_buys.len(), 2, "80 and 90 resting before restart");
    for o in &resting_buys {
        // Drop the mid below the order so it crosses, then fill it.
        mock.push_mid(o.price - 1.0);
        mock.fill(o.exchange_oid.unwrap() as u64).unwrap();
    }
    // Two more longs opened offline (from the 80 and 90 buys).
    assert_eq!(mock.net_position(), 4.0, "longs from 80 and 90 fills");

    // Price returns to 100 before the restart.
    mock.push_mid(100.0);

    // --- Second run: a fresh bot reconciles and re-seeds. ---
    let bot2 = make_bot_with(exchange.clone(), store.clone(), GridMode::LongOnly);
    bot2.bootstrap().await.unwrap();

    // The 80/90 longs are closed by reduce-only sells at the current price; the
    // original 100/110 longs are left untouched (their take-profit sells are
    // still resting, so the grid is not re-opened). Net returns to 2.
    assert_eq!(
        mock.net_position(),
        2.0,
        "offline longs closed, existing grid untouched"
    );

    let open = store.open_orders("XMR").await.unwrap();
    // The 80 and 90 buys are re-seeded and resting again.
    assert!(
        open.iter().any(|o| o.level == 0 && o.side == Side::Buy),
        "buy re-seeded at 80"
    );
    assert!(
        open.iter().any(|o| o.level == 1 && o.side == Side::Buy),
        "buy re-seeded at 90"
    );
    // Take-profit sells for the live longs rest at 110 and 120.
    assert!(open.iter().any(|o| o.level == 3 && o.side == Side::Sell));
    assert!(open.iter().any(|o| o.level == 4 && o.side == Side::Sell));
}

/// Neutral mode seeds buys below the mid and sells above it, opening no position
/// at startup (nothing crosses), and oscillates as the market moves.
#[tokio::test]
async fn startup_seeds_two_sided_neutral_grid() {
    let Some(url) = db_url() else { return };
    let store = make_store(&url).await;
    let mock = Arc::new(MockExchange::new(100.0));
    let exchange: Arc<dyn Exchange> = mock.clone();
    let bot = make_bot_with(exchange, store.clone(), GridMode::Neutral);

    bot.bootstrap().await.unwrap();

    // Nothing crosses at startup, so no position is opened.
    assert_eq!(
        mock.net_position(),
        0.0,
        "neutral opens no position at start"
    );

    let open = store.open_orders("XMR").await.unwrap();
    // Buys below the mid (80, 90) and sells above it (110, 120); the line at the
    // mid (100) is left untouched.
    assert!(open.iter().any(|o| o.level == 0 && o.side == Side::Buy));
    assert!(open.iter().any(|o| o.level == 1 && o.side == Side::Buy));
    assert!(open.iter().any(|o| o.level == 3 && o.side == Side::Sell));
    assert!(open.iter().any(|o| o.level == 4 && o.side == Side::Sell));
    assert!(
        open.iter().all(|o| o.level != 2),
        "nothing seeded at the mid"
    );
    assert!(open.iter().all(|o| !o.reduce_only), "no reduce-only legs");
    assert_eq!(open.len(), 4, "80/90 buys + 110/120 sells");
}
