//! Configuration loading and validation.
//!
//! Configuration comes from two places:
//!
//! 1. A TOML file (path from `HYPERBOT_CONFIG`, default `config.toml`).
//! 2. Environment variables, which **override** the file. Secrets (the API
//!    wallet private key and the database URL) are intended to be supplied this
//!    way so they never have to be baked into an image.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::grid::{GridMode, GridParams, Spacing};

/// Which Hyperliquid network to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    Mainnet,
    #[default]
    Testnet,
}

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Exchange / wallet settings.
    #[serde(default)]
    pub exchange: ExchangeConfig,
    /// Grid strategy settings.
    #[serde(default)]
    pub grid: GridConfig,
    /// Risk-control settings.
    #[serde(default)]
    pub risk: RiskConfig,
    /// Database settings.
    #[serde(default)]
    pub database: DatabaseConfig,
}

/// Exchange and wallet configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    /// Network to trade on.
    #[serde(default)]
    pub network: Network,
    /// API wallet private key (hex, with or without `0x`). Inject via the
    /// `HYPERBOT_PRIVATE_KEY` environment variable; leave empty in the file.
    #[serde(default)]
    pub private_key: String,
    /// Main account address (hex, `0x`-prefixed) to query orders/fills/positions
    /// for. Set this when `private_key` is an **agent / API wallet** key, whose
    /// derived address differs from the funded master account: signing uses the
    /// agent key but all info queries (`open_orders`, `user_fills`,
    /// `clearinghouse_state`) must target the master account where orders, fills
    /// and positions actually live. Leave empty to use the key's own address.
    /// Inject via the `HYPERBOT_ACCOUNT_ADDRESS` environment variable.
    #[serde(default = "default_account_address")]
    pub account_address: String,
    /// Leverage to set for the traded coin before the grid starts.
    #[serde(default = "default_leverage")]
    pub leverage: u32,
    /// Whether to use cross (`true`) or isolated (`false`) margin.
    #[serde(default)]
    pub cross_margin: bool,
    /// Whether to cancel all resting orders when the bot shuts down. Defaults to
    /// `false` so resting grid orders are preserved across restarts and picked
    /// up again on the next startup.
    #[serde(default)]
    pub cancel_on_exit: bool,
}

impl Default for ExchangeConfig {
    fn default() -> Self {
        Self {
            network: Network::default(),
            private_key: String::new(),
            account_address: default_account_address(),
            leverage: default_leverage(),
            cross_margin: false,
            cancel_on_exit: false,
        }
    }
}

fn default_account_address() -> String {
    "0xf581803C5998FAb668Ee4E0826eCf8e2Ca3f469b".to_string()
}

fn default_leverage() -> u32 {
    3
}

/// Grid strategy configuration (a serializable mirror of [`GridParams`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridConfig {
    /// Perp coin symbol. For the XMR/USDC perp this is `"XMR"`.
    #[serde(default = "default_coin")]
    pub coin: String,
    /// Lower price bound.
    #[serde(default = "default_lower_price")]
    pub lower_price: f64,
    /// Upper price bound.
    #[serde(default = "default_upper_price")]
    pub upper_price: f64,
    /// Number of grid intervals.
    #[serde(default = "default_grid_count")]
    pub grid_count: usize,
    /// Spacing strategy.
    #[serde(default)]
    pub spacing: Spacing,
    /// Order size (contracts) per grid line.
    #[serde(default = "default_order_size")]
    pub order_size: f64,
    /// Trade direction restriction.
    #[serde(default)]
    pub mode: GridMode,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            coin: default_coin(),
            lower_price: default_lower_price(),
            upper_price: default_upper_price(),
            grid_count: default_grid_count(),
            spacing: Spacing::default(),
            order_size: default_order_size(),
            mode: GridMode::default(),
        }
    }
}

fn default_coin() -> String {
    "XMR".to_string()
}

fn default_lower_price() -> f64 {
    340.0
}

fn default_upper_price() -> f64 {
    360.0
}

fn default_grid_count() -> usize {
    10
}

fn default_order_size() -> f64 {
    0.1
}

impl GridConfig {
    /// Converts to the strategy parameter type.
    pub fn to_params(&self) -> GridParams {
        GridParams {
            coin: self.coin.clone(),
            lower_price: self.lower_price,
            upper_price: self.upper_price,
            grid_count: self.grid_count,
            spacing: self.spacing,
            order_size: self.order_size,
            mode: self.mode,
        }
    }
}

/// Risk-control configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Maximum absolute position size (contracts). `0` disables the check.
    #[serde(default)]
    pub max_position: f64,
    /// Maximum tolerated unrealised loss in USDC (positive number). `0`
    /// disables the check.
    #[serde(default)]
    pub max_loss_usd: f64,
    /// Hard cap on leverage; the configured leverage must not exceed it.
    #[serde(default = "default_max_leverage")]
    pub max_leverage: u32,
}

fn default_max_leverage() -> u32 {
    5
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_position: 0.0,
            max_loss_usd: 0.0,
            max_leverage: default_max_leverage(),
        }
    }
}

/// Database configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL connection string. Inject via `DATABASE_URL`.
    #[serde(default)]
    pub url: String,
    /// Maximum number of pooled connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            max_connections: default_max_connections(),
        }
    }
}

fn default_max_connections() -> u32 {
    5
}

/// Reads a systemd-supplied credential named `id`, if present.
///
/// When a unit declares `LoadCredentialEncrypted=<id>:<path>` (or
/// `LoadCredential=`), systemd decrypts it into `$CREDENTIALS_DIRECTORY/<id>` on
/// a per-service tmpfs that only this process can read, and never writes it to
/// disk in plaintext. Returns the trimmed contents, or `None` when the variable
/// is unset (not running under systemd) or the file is missing/empty.
fn load_credential(id: &str) -> Option<String> {
    let dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
    let path = Path::new(&dir).join(id);
    let contents = std::fs::read_to_string(&path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

impl Config {
    /// Loads configuration from the default path (or `HYPERBOT_CONFIG`),
    /// applying environment overrides, and validates the result.
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var("HYPERBOT_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
        Self::load_from(path)
    }

    /// Loads configuration from a specific path, applying environment overrides.
    ///
    /// When the file does not exist the built-in defaults are used, so the bot
    /// runs without any config file (secrets still come from the environment).
    pub fn load_from(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let mut cfg: Config = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            toml::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?
        } else {
            Config::default()
        };
        cfg.apply_env_overrides();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Overrides secret / deployment fields from the environment.
    pub fn apply_env_overrides(&mut self) {
        // Secrets first from systemd-supplied credentials, then from the
        // environment (the latter wins so an explicit override still applies).
        // systemd places decrypted credentials in $CREDENTIALS_DIRECTORY, one
        // file per credential id, on a private tmpfs readable only by this
        // service — strictly more secure than an env file.
        if let Some(v) = load_credential("private_key") {
            self.exchange.private_key = v;
        }
        if let Some(v) = load_credential("database_url") {
            self.database.url = v;
        }
        if let Some(v) = load_credential("account_address") {
            self.exchange.account_address = v;
        }

        if let Ok(v) = std::env::var("HYPERBOT_PRIVATE_KEY") {
            if !v.is_empty() {
                self.exchange.private_key = v;
            }
        }
        if let Ok(v) = std::env::var("HYPERBOT_ACCOUNT_ADDRESS") {
            if !v.is_empty() {
                self.exchange.account_address = v;
            }
        }
        if let Ok(v) = std::env::var("DATABASE_URL") {
            if !v.is_empty() {
                self.database.url = v;
            }
        }
        if let Ok(v) = std::env::var("HYPERBOT_NETWORK") {
            match v.to_ascii_lowercase().as_str() {
                "mainnet" => self.exchange.network = Network::Mainnet,
                "testnet" => self.exchange.network = Network::Testnet,
                _ => {}
            }
        }
    }

    /// Validates the fully-resolved configuration.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.exchange.private_key.trim().is_empty() {
            anyhow::bail!("exchange.private_key is empty; set HYPERBOT_PRIVATE_KEY");
        }
        if self.database.url.trim().is_empty() {
            anyhow::bail!("database.url is empty; set DATABASE_URL");
        }
        self.grid
            .to_params()
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid grid config: {e}"))?;
        if self.risk.max_leverage > 0 && self.exchange.leverage > self.risk.max_leverage {
            anyhow::bail!(
                "exchange.leverage ({}) exceeds risk.max_leverage ({})",
                self.exchange.leverage,
                self.risk.max_leverage
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests mutate the shared process environment, so they must not run
    // concurrently with one another.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const SAMPLE: &str = r#"
[exchange]
network = "testnet"
leverage = 2
cross_margin = false

[grid]
coin = "XMR"
lower_price = 100.0
upper_price = 200.0
grid_count = 20
spacing = "arithmetic"
order_size = 0.1
mode = "short_only"

[risk]
max_position = 5.0
max_loss_usd = 500.0
max_leverage = 10

[database]
max_connections = 5
"#;

    fn write_tmp(contents: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn loads_and_applies_env_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = write_tmp(SAMPLE);
        std::env::set_var("HYPERBOT_PRIVATE_KEY", "0xabc123");
        std::env::set_var("DATABASE_URL", "******localhost/db");
        let cfg = Config::load_from(f.path()).unwrap();
        assert_eq!(cfg.grid.coin, "XMR");
        assert_eq!(cfg.grid.mode, GridMode::ShortOnly);
        assert_eq!(cfg.grid.spacing, Spacing::Arithmetic);
        assert_eq!(cfg.exchange.private_key, "0xabc123");
        assert_eq!(cfg.database.url, "******localhost/db");
        std::env::remove_var("HYPERBOT_PRIVATE_KEY");
        std::env::remove_var("DATABASE_URL");
    }

    #[test]
    fn validation_requires_secrets() {
        let _guard = ENV_LOCK.lock().unwrap();
        let f = write_tmp(SAMPLE);
        std::env::remove_var("HYPERBOT_PRIVATE_KEY");
        std::env::remove_var("DATABASE_URL");
        // No secrets provided -> validation fails.
        assert!(Config::load_from(f.path()).is_err());
    }

    #[test]
    fn rejects_leverage_above_cap() {
        let _guard = ENV_LOCK.lock().unwrap();
        let cfg_str = SAMPLE.replace("leverage = 2", "leverage = 50");
        let f = write_tmp(&cfg_str);
        std::env::set_var("HYPERBOT_PRIVATE_KEY", "0xabc");
        std::env::set_var("DATABASE_URL", "postgres://x");
        let err = Config::load_from(f.path()).unwrap_err().to_string();
        assert!(err.contains("max_leverage"), "got: {err}");
        std::env::remove_var("HYPERBOT_PRIVATE_KEY");
        std::env::remove_var("DATABASE_URL");
    }
}
