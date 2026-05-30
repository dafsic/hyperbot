-- Initial schema for the Hyperliquid grid bot (PostgreSQL).

-- One row per grid order the bot manages.
CREATE TABLE IF NOT EXISTS grid_orders (
    id            BIGSERIAL PRIMARY KEY,
    coin          TEXT             NOT NULL,
    level         INTEGER          NOT NULL,
    side          TEXT             NOT NULL,
    price         DOUBLE PRECISION NOT NULL,
    size          DOUBLE PRECISION NOT NULL,
    reduce_only   BOOLEAN          NOT NULL DEFAULT FALSE,
    status        TEXT             NOT NULL DEFAULT 'pending',
    exchange_oid  BIGINT,
    created_at    TIMESTAMPTZ      NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ      NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_grid_orders_coin_status
    ON grid_orders (coin, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_grid_orders_exchange_oid
    ON grid_orders (exchange_oid)
    WHERE exchange_oid IS NOT NULL;

-- Append-only record of every fill we observe.
CREATE TABLE IF NOT EXISTS fills (
    id            BIGSERIAL PRIMARY KEY,
    exchange_oid  BIGINT           NOT NULL,
    coin          TEXT             NOT NULL,
    side          TEXT             NOT NULL,
    price         DOUBLE PRECISION NOT NULL,
    size          DOUBLE PRECISION NOT NULL,
    created_at    TIMESTAMPTZ      NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_fills_coin ON fills (coin);

-- Periodic position snapshots for observability / recovery.
CREATE TABLE IF NOT EXISTS position_snapshots (
    id             BIGSERIAL PRIMARY KEY,
    coin           TEXT             NOT NULL,
    size           DOUBLE PRECISION NOT NULL,
    entry_price    DOUBLE PRECISION,
    unrealized_pnl DOUBLE PRECISION NOT NULL,
    created_at     TIMESTAMPTZ      NOT NULL DEFAULT now()
);

-- Simple key/value store for bot runtime state.
CREATE TABLE IF NOT EXISTS bot_state (
    key        TEXT PRIMARY KEY,
    value      TEXT        NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
