use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone)]
pub struct AppConfig {
    pub polymarket: PolymarketConfig,
    pub kalshi: KalshiConfig,
    pub detector: DetectorConfig,
    pub risk: RiskConfig,
    pub monitoring: MonitoringConfig,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("polymarket", &self.polymarket)
            .field("kalshi", &self.kalshi)
            .field("detector", &self.detector)
            .field("risk", &self.risk)
            .field("monitoring", &self.monitoring)
            .finish()
    }
}

#[derive(Clone)]
pub struct PolymarketConfig {
    pub gamma_url: String,
    pub clob_url: String,
    pub ws_url: String,
    pub ws_heartbeat_interval_s: u64,
    pub polygon_private_key: String,
}

/// Custom Debug that redacts the private key
impl fmt::Debug for PolymarketConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolymarketConfig")
            .field("gamma_url", &self.gamma_url)
            .field("clob_url", &self.clob_url)
            .field("ws_url", &self.ws_url)
            .field("ws_heartbeat_interval_s", &self.ws_heartbeat_interval_s)
            .field("polygon_private_key", &if self.polygon_private_key.is_empty() { "<not set>" } else { "<REDACTED>" })
            .finish()
    }
}

#[derive(Clone)]
pub struct KalshiConfig {
    pub rest_url: String,
    pub ws_url: String,
    pub poll_interval_s: u64,
    pub orderbook_depth: u32,
    pub api_key_id: String,
    pub rsa_private_key_pem: String,
}

/// Custom Debug that redacts the RSA key
impl fmt::Debug for KalshiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KalshiConfig")
            .field("rest_url", &self.rest_url)
            .field("ws_url", &self.ws_url)
            .field("poll_interval_s", &self.poll_interval_s)
            .field("orderbook_depth", &self.orderbook_depth)
            .field("api_key_id", &self.api_key_id)
            .field("rsa_private_key_pem", &if self.rsa_private_key_pem.is_empty() { "<not set>" } else { "<REDACTED>" })
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct DetectorConfig {
    pub min_net_edge: Decimal,
    pub slippage_buffer: Decimal,
    pub max_stale_ms: i64,
    pub min_trade_size: Decimal,
    pub max_trade_size: Decimal,
    pub min_depth: Decimal,
    pub persistence_snapshots: u32,
    pub settlement_blackout_min: u32,
}

#[derive(Debug, Clone)]
pub struct MonitoringConfig {
    pub health_port: u16,
    pub alert_webhook_url: String,
    pub books_log_retention_days: u32,
}

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub max_notional_per_contract: Decimal,
    pub max_notional_total: Decimal,
    pub max_residual_per_contract: Decimal,
    pub max_trades_per_minute: u32,
    pub max_consecutive_failures: u32,
    pub max_api_errors_per_minute: u32,
}

#[derive(Deserialize)]
struct RawConfig {
    venues: RawVenues,
    detector: RawDetector,
    risk: RawRisk,
    monitoring: Option<RawMonitoring>,
}

#[derive(Deserialize)]
struct RawMonitoring {
    health_port: Option<u16>,
    alert_webhook_url: Option<String>,
    books_log_retention_days: Option<u32>,
}

#[derive(Deserialize)]
struct RawVenues {
    polymarket: RawPolymarket,
    kalshi: RawKalshi,
}

#[derive(Deserialize)]
struct RawPolymarket {
    gamma_url: String,
    clob_url: String,
    ws_url: String,
    ws_heartbeat_interval_s: u64,
}

#[derive(Deserialize)]
struct RawKalshi {
    rest_url: String,
    ws_url: String,
    poll_interval_s: u64,
    orderbook_depth: u32,
}

#[derive(Deserialize)]
struct RawDetector {
    min_net_edge: String,
    slippage_buffer: String,
    max_stale_ms: i64,
    min_trade_size: String,
    max_trade_size: String,
    min_depth: String,
    persistence_snapshots: u32,
    settlement_blackout_min: u32,
}

#[derive(Deserialize)]
struct RawRisk {
    max_notional_per_contract: String,
    max_notional_total: String,
    max_residual_per_contract: String,
    max_trades_per_minute: u32,
    max_consecutive_failures: u32,
    max_api_errors_per_minute: u32,
}

pub fn project_root() -> PathBuf {
    // Walk up from executable to find config.yaml
    let mut dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if dir.join("config.yaml").exists() {
            return dir;
        }
        if !dir.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
}

fn parse_env_rsa_key(env_path: &Path) -> (String, String) {
    let content = match std::fs::read_to_string(env_path) {
        Ok(c) => c,
        Err(_) => return (String::new(), String::new()),
    };

    let mut api_key_id = String::new();
    let mut private_key_lines = Vec::new();
    let mut in_private_key = false;

    for line in content.lines() {
        let stripped = line.trim();
        if stripped.starts_with("KALSHI_RSA_PUBLIC_KEY") {
            if let Some((_, val)) = stripped.split_once('=') {
                api_key_id = val.trim().to_string();
            }
        } else if stripped.starts_with("KALSHI_RSA_PRIVATE_KEY") {
            in_private_key = true;
            if let Some((_, val)) = stripped.split_once('=') {
                let val = val.trim();
                if !val.is_empty() {
                    private_key_lines.push(val.to_string());
                }
            }
        } else if in_private_key {
            let starts_with_known = stripped.starts_with("KALSHI_")
                || stripped.starts_with("POLYMARKET_")
                || stripped.starts_with("POLYGON_")
                || stripped.starts_with('#');
            if starts_with_known {
                in_private_key = false;
            } else if !stripped.is_empty() {
                private_key_lines.push(stripped.to_string());
            }
        }
    }

    (api_key_id, private_key_lines.join("\n"))
}

/// Validate config invariants that could cause financial loss if wrong.
fn validate_config(cfg: &AppConfig) -> Result<()> {
    if cfg.kalshi.poll_interval_s == 0 {
        anyhow::bail!("kalshi.poll_interval_s must be > 0");
    }
    if cfg.detector.min_net_edge <= Decimal::ZERO || cfg.detector.min_net_edge >= Decimal::ONE {
        anyhow::bail!("detector.min_net_edge must be in (0, 1), got {}", cfg.detector.min_net_edge);
    }
    if cfg.detector.max_trade_size < cfg.detector.min_trade_size {
        anyhow::bail!("detector.max_trade_size ({}) must be >= min_trade_size ({})", cfg.detector.max_trade_size, cfg.detector.min_trade_size);
    }
    if cfg.risk.max_notional_total < cfg.risk.max_notional_per_contract {
        anyhow::bail!("risk.max_notional_total ({}) must be >= max_notional_per_contract ({})", cfg.risk.max_notional_total, cfg.risk.max_notional_per_contract);
    }
    if cfg.risk.max_trades_per_minute == 0 {
        anyhow::bail!("risk.max_trades_per_minute must be > 0");
    }
    Ok(())
}

pub fn load_config(config_path: Option<&str>) -> Result<AppConfig> {
    let root = project_root();
    let env_path = root.join(".env");
    dotenvy::from_path(&env_path).ok();

    let path = match config_path {
        Some(p) => PathBuf::from(p),
        None => root.join("config.yaml"),
    };
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config: {}", path.display()))?;
    let raw: RawConfig = serde_yaml::from_str(&content)
        .with_context(|| "Failed to parse config.yaml")?;

    let (mut api_key_id, mut rsa_key) = parse_env_rsa_key(&env_path);
    if api_key_id.is_empty() {
        api_key_id = std::env::var("KALSHI_API_KEY_ID")
            .or_else(|_| std::env::var("KALSHI_RSA_PUBLIC_KEY"))
            .unwrap_or_default();
    }
    if rsa_key.is_empty() {
        rsa_key = std::env::var("KALSHI_RSA_PRIVATE_KEY").unwrap_or_default();
    }

    // Build PEM if needed
    let rsa_pem = if rsa_key.is_empty() {
        String::new()
    } else if rsa_key.starts_with("-----BEGIN") {
        rsa_key
    } else {
        format!("-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----", rsa_key)
    };

    let mon = raw.monitoring.as_ref();
    let cfg = AppConfig {
        polymarket: PolymarketConfig {
            gamma_url: raw.venues.polymarket.gamma_url,
            clob_url: raw.venues.polymarket.clob_url,
            ws_url: raw.venues.polymarket.ws_url,
            ws_heartbeat_interval_s: raw.venues.polymarket.ws_heartbeat_interval_s,
            polygon_private_key: std::env::var("POLYGON_PRIVATE_KEY").unwrap_or_default(),
        },
        kalshi: KalshiConfig {
            rest_url: raw.venues.kalshi.rest_url,
            ws_url: raw.venues.kalshi.ws_url,
            poll_interval_s: raw.venues.kalshi.poll_interval_s,
            orderbook_depth: raw.venues.kalshi.orderbook_depth,
            api_key_id,
            rsa_private_key_pem: rsa_pem,
        },
        detector: DetectorConfig {
            min_net_edge: Decimal::from_str(&raw.detector.min_net_edge)?,
            slippage_buffer: Decimal::from_str(&raw.detector.slippage_buffer)?,
            max_stale_ms: raw.detector.max_stale_ms,
            min_trade_size: Decimal::from_str(&raw.detector.min_trade_size)?,
            max_trade_size: Decimal::from_str(&raw.detector.max_trade_size)?,
            min_depth: Decimal::from_str(&raw.detector.min_depth)?,
            persistence_snapshots: raw.detector.persistence_snapshots,
            settlement_blackout_min: raw.detector.settlement_blackout_min,
        },
        risk: RiskConfig {
            max_notional_per_contract: Decimal::from_str(&raw.risk.max_notional_per_contract)?,
            max_notional_total: Decimal::from_str(&raw.risk.max_notional_total)?,
            max_residual_per_contract: Decimal::from_str(&raw.risk.max_residual_per_contract)?,
            max_trades_per_minute: raw.risk.max_trades_per_minute,
            max_consecutive_failures: raw.risk.max_consecutive_failures,
            max_api_errors_per_minute: raw.risk.max_api_errors_per_minute,
        },
        monitoring: MonitoringConfig {
            health_port: mon.and_then(|m| m.health_port).unwrap_or(8080),
            alert_webhook_url: mon.and_then(|m| m.alert_webhook_url.clone()).unwrap_or_default(),
            books_log_retention_days: mon.and_then(|m| m.books_log_retention_days).unwrap_or(7),
        },
    };

    validate_config(&cfg)?;
    Ok(cfg)
}
