mod adapters;
mod alerting;
mod config;
mod db;
mod detector;
mod executor;
mod health;
mod models;
mod persistence;
mod polymarket_signer;
mod registry;
mod risk;

use anyhow::Result;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::{watch, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::adapters::kalshi::KalshiAdapter;
use crate::adapters::polymarket::PolymarketAdapter;
use crate::config::load_config;
use crate::db::{get_connection, init_db};
use crate::detector::{find_all_opportunities, OpportunityDetector};
use crate::executor::Executor;
use crate::health::{HealthState, SharedHealth};
use crate::models::CanonicalBook;
use crate::persistence::PersistenceTracker;
use crate::registry::ContractRegistry;
use crate::risk::RiskManager;

#[tokio::main]
async fn main() -> Result<()> {
    let log_dir = config::project_root().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .with_writer(std::io::stdout)
        .with_ansi(true)
        .init();

    let cfg = load_config(None)?;
    init_db()?;

    let mut registry = ContractRegistry::new();
    let n = registry.load_manual_mappings(None)?;
    if n == 0 {
        warn!("No manual mappings loaded");
    }

    let mut kalshi = KalshiAdapter::new(cfg.kalshi.clone());
    let mut polymarket = PolymarketAdapter::new(cfg.polymarket.clone());
    kalshi.connect().await?;
    polymarket.connect().await?;

    // Derive Polymarket API key for order placement (non-fatal if it fails)
    if let Err(e) = polymarket.derive_api_key().await {
        warn!("Could not derive Polymarket API key: {} — execution disabled", e);
    }

    let detector = OpportunityDetector::new(cfg.detector.clone());
    let mut risk_mgr = RiskManager::new(cfg.risk.clone());

    let conn = get_connection()?;
    let pairs = registry.get_paired_contracts();
    info!("Monitoring {} paired contracts", pairs.len());

    // Collect token IDs for WebSocket
    let mut ws_token_ids = Vec::new();
    for (_cid, pm_map, _km_map) in &pairs {
        if let Some(ref tid) = pm_map.yes_token_id {
            ws_token_ids.push(tid.clone());
        }
        if let Some(ref tid) = pm_map.no_token_id {
            ws_token_ids.push(tid.clone());
        }
    }
    if !ws_token_ids.is_empty() {
        polymarket.ws_connect(ws_token_ids).await?;
    }

    // Build executor (borrows the monitoring adapters)
    let executor = Executor::new(&kalshi, &polymarket);

    // Shared health state
    let health_state: SharedHealth = Arc::new(RwLock::new(HealthState::default()));

    // Start health server
    let health_port = cfg.monitoring.health_port;
    let health_state_srv = health_state.clone();
    tokio::spawn(async move {
        health::serve(health_state_srv, health_port).await;
    });

    // Alerting HTTP client (shared for webhook calls)
    let alert_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let alert_url = cfg.monitoring.alert_webhook_url.clone();

    // Persistence tracker
    let required_snapshots = cfg.detector.persistence_snapshots;
    let mut persistence = PersistenceTracker::new(required_snapshots);

    // Shutdown signal
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        signal::ctrl_c().await.ok();
        info!("Shutdown signal received");
        shutdown_tx.send(true).ok();
    });

    // Main loop
    let mut cycle: u64 = 0;
    let poll_interval = Duration::from_secs(cfg.kalshi.poll_interval_s);
    let retention_days = cfg.monitoring.books_log_retention_days;

    loop {
        if *shutdown_rx.borrow() || risk_mgr.is_killed() {
            if risk_mgr.is_killed() {
                alerting::send_alert(
                    &alert_client,
                    &alert_url,
                    "arb_monitor KILL SWITCH activated — bot has stopped trading",
                ).await;
            }
            break;
        }

        cycle = cycle.wrapping_add(1);
        let mut books: Vec<CanonicalBook> = Vec::new();

        // Fetch Kalshi + Polymarket books concurrently per pair
        for (cid, pm_mapping, km_mapping) in &pairs {
            let (kalshi_result, pm_result) = tokio::join!(
                kalshi.get_book(&km_mapping.native_market_id),
                polymarket.get_book(
                    &pm_mapping.native_market_id,
                    pm_mapping.yes_token_id.as_deref().unwrap_or(""),
                    pm_mapping.no_token_id.as_deref().unwrap_or(""),
                )
            );

            match kalshi_result {
                Ok(mut kb) => {
                    kb.canonical_id = cid.clone();
                    if let Err(e) = db::log_book(&conn, &kb) {
                        warn!("Failed to log Kalshi book: {}", e);
                    }
                    books.push(kb);
                }
                Err(e) => {
                    error!("Error fetching Kalshi book for {}: {}", cid, e);
                    risk_mgr.record_api_error();
                }
            }

            match pm_result {
                Ok(mut pb) => {
                    pb.canonical_id = cid.clone();
                    if let Err(e) = db::log_book(&conn, &pb) {
                        warn!("Failed to log Polymarket book: {}", e);
                    }
                    books.push(pb);
                }
                Err(e) => {
                    error!("Error fetching Polymarket book for {}: {}", cid, e);
                    risk_mgr.record_api_error();
                }
            }
        }

        // Detect raw opportunities
        let raw_opps = find_all_opportunities(&books, &detector);

        // Apply persistence filter — only act on opportunities seen N consecutive cycles
        let seen_keys: Vec<(String, String)> = raw_opps
            .iter()
            .map(|o| (o.canonical_id.clone(), o.yes_venue.to_string()))
            .collect();
        let persistent_keys = persistence.update(&seen_keys);

        let opps: Vec<_> = raw_opps
            .iter()
            .filter(|o| {
                required_snapshots <= 1
                    || persistent_keys.contains(&(o.canonical_id.clone(), o.yes_venue.to_string()))
            })
            .collect();

        for opp in &opps {
            info!(
                ">>> OPPORTUNITY: {} | YES@{}(${}) NO@{}(${}) | gross={:.4} net={:.4} size={}",
                opp.canonical_id,
                opp.yes_venue,
                opp.buy_yes_price,
                opp.no_venue,
                opp.buy_no_price,
                opp.gross_edge,
                opp.net_edge,
                opp.max_size,
            );

            if let Err(e) = db::log_opportunity(&conn, opp) {
                warn!("Failed to log opportunity: {}", e);
            }

            let (approved, reason) = risk_mgr.check_opportunity(opp);
            if !approved {
                info!("  Skipped: {}", reason);
                continue;
            }

            // Find the mappings for this opportunity's pair
            let pair = pairs.iter().find(|(cid, _, _)| *cid == opp.canonical_id);
            if let Some((_cid, pm_mapping, km_mapping)) = pair {
                let size = risk_mgr.approved_size(opp);
                info!("Executing arb for {} with size {}", opp.canonical_id, size);

                let (id1, id2) = executor
                    .execute_locked_arb(opp, pm_mapping, km_mapping, size)
                    .await;

                let leg1_local = uuid::Uuid::new_v4().to_string();
                let leg2_local = uuid::Uuid::new_v4().to_string();

                match (&id1, &id2) {
                    (Some(oid1), Some(oid2)) => {
                        let notional = size * opp.buy_yes_price + size * opp.buy_no_price;
                        risk_mgr.record_trade(&opp.canonical_id, notional);
                        if let Err(e) = db::log_order(
                            &conn, &leg1_local, &opp.yes_venue.to_string(), oid1,
                            &opp.opportunity_id, "yes", "buy",
                            &opp.buy_yes_price.to_string(), &size.to_string(), "filled",
                        ) {
                            warn!("Failed to log order: {}", e);
                        }
                        if let Err(e) = db::log_order(
                            &conn, &leg2_local, &opp.no_venue.to_string(), oid2,
                            &opp.opportunity_id, "no", "buy",
                            &opp.buy_no_price.to_string(), &size.to_string(), "filled",
                        ) {
                            warn!("Failed to log order: {}", e);
                        }
                    }
                    (Some(oid1), None) => {
                        risk_mgr.record_failure();
                        if let Err(e) = db::log_order(
                            &conn, &leg1_local, &opp.yes_venue.to_string(), oid1,
                            &opp.opportunity_id, "yes", "buy",
                            &opp.buy_yes_price.to_string(), &size.to_string(), "filled",
                        ) {
                            warn!("Failed to log order: {}", e);
                        }
                        if let Err(e) = db::log_order(
                            &conn, &leg2_local, &opp.no_venue.to_string(), "",
                            &opp.opportunity_id, "no", "buy",
                            &opp.buy_no_price.to_string(), &size.to_string(), "failed",
                        ) {
                            warn!("Failed to log order: {}", e);
                        }
                        error!("RESIDUAL EXPOSURE on {} — leg 1 filled, leg 2 failed", opp.canonical_id);
                        alerting::send_alert(
                            &alert_client,
                            &alert_url,
                            &format!(
                                "RESIDUAL EXPOSURE: {} — leg 1 (order {}) filled but leg 2 failed. Manual intervention required.",
                                opp.canonical_id, oid1
                            ),
                        ).await;
                    }
                    _ => {
                        risk_mgr.record_failure();
                    }
                }
            }
        }

        // Update shared health state
        {
            let total_notional: Decimal = Decimal::ZERO; // populated by risk_mgr in future
            let mut h = health_state.write().await;
            h.cycle = cycle;
            h.last_cycle_at = Some(std::time::Instant::now());
            h.kill_switch_active = risk_mgr.is_killed();
            h.opportunities_last_cycle = raw_opps.len();
            h.total_notional = total_notional;
        }

        if cycle % 30 == 0 {
            info!(
                "Cycle {}: {} raw / {} persistent opportunities (monitoring {} pairs)",
                cycle,
                raw_opps.len(),
                opps.len(),
                pairs.len()
            );
        }

        // Periodic books_log cleanup every ~1 hour (1800 cycles at 2s)
        if cycle % 1800 == 0 && retention_days > 0 {
            match db::prune_books_log(&conn, retention_days) {
                Ok(n) if n > 0 => info!("Pruned {} old books_log rows (>{} days)", n, retention_days),
                Ok(_) => {}
                Err(e) => warn!("books_log pruning failed: {}", e),
            }
        }

        // Sleep before next poll
        tokio::select! {
            _ = sleep(poll_interval) => {},
            _ = shutdown_rx.changed() => { break; },
        }
    }

    // Cleanup
    info!("Shutting down — stopping adapters");
    kalshi.disconnect().await?;
    polymarket.disconnect().await?;
    info!("Shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;
    use crate::registry::ContractRegistry;

    #[test]
    fn test_config_loads() {
        let cfg = load_config(None).expect("config should load");
        assert_eq!(cfg.kalshi.poll_interval_s, 2);
        assert_eq!(cfg.polymarket.ws_heartbeat_interval_s, 9);
        assert!(!cfg.polymarket.gamma_url.is_empty());
        assert_eq!(cfg.monitoring.health_port, 8080);
    }

    #[test]
    fn test_registry_loads_mappings() {
        let mut registry = ContractRegistry::new();
        let n = registry.load_manual_mappings(None).expect("mappings should load");
        assert_eq!(n, 6, "should load 6 manual mappings");
        let pairs = registry.get_paired_contracts();
        assert_eq!(pairs.len(), 6, "should have 6 paired contracts");
    }

    #[test]
    fn test_db_init() {
        db::init_db().expect("DB should initialize");
        let conn = db::get_connection().expect("DB connection should open");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM books_log", [], |r| r.get(0))
            .expect("books_log table should exist");
        assert!(count >= 0);
    }

    #[test]
    fn test_detector_no_false_positives() {
        use crate::detector::OpportunityDetector;
        use crate::models::{CanonicalBook, Venue};
        use chrono::Utc;
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let cfg = load_config(None).unwrap();
        let detector = OpportunityDetector::new(cfg.detector);

        let book_a = CanonicalBook {
            venue: Venue::Kalshi,
            native_market_id: "TEST".to_string(),
            canonical_id: "test_market".to_string(),
            buy_yes: Decimal::from_str("0.60").unwrap(),
            buy_no: Decimal::from_str("0.40").unwrap(),
            depth_buy_yes: Decimal::from(100),
            depth_buy_no: Decimal::from(100),
            fee_rate: Decimal::from_str("0.01").unwrap(),
            tick_size: Decimal::from_str("0.01").unwrap(),
            min_order_size: Decimal::ONE,
            ts_exchange: None,
            ts_received: Utc::now(),
        };
        let book_b = CanonicalBook {
            venue: Venue::Polymarket,
            native_market_id: "TEST_PM".to_string(),
            canonical_id: "test_market".to_string(),
            buy_yes: Decimal::from_str("0.60").unwrap(),
            buy_no: Decimal::from_str("0.40").unwrap(),
            depth_buy_yes: Decimal::from(100),
            depth_buy_no: Decimal::from(100),
            fee_rate: Decimal::from_str("0.00").unwrap(),
            tick_size: Decimal::from_str("0.01").unwrap(),
            min_order_size: Decimal::ONE,
            ts_exchange: None,
            ts_received: Utc::now(),
        };

        let opps = detector.detect_for_pair(&book_a, &book_b);
        assert!(opps.is_empty(), "no edge should produce no opportunities");
    }

    #[test]
    fn test_detector_finds_real_edge() {
        use crate::detector::OpportunityDetector;
        use crate::models::{CanonicalBook, Venue};
        use chrono::Utc;
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let cfg = load_config(None).unwrap();
        let detector = OpportunityDetector::new(cfg.detector);

        let book_a = CanonicalBook {
            venue: Venue::Kalshi,
            native_market_id: "TEST".to_string(),
            canonical_id: "arb_test".to_string(),
            buy_yes: Decimal::from_str("0.40").unwrap(),
            buy_no: Decimal::from_str("0.70").unwrap(),
            depth_buy_yes: Decimal::from(100),
            depth_buy_no: Decimal::from(100),
            fee_rate: Decimal::from_str("0.01").unwrap(),
            tick_size: Decimal::from_str("0.01").unwrap(),
            min_order_size: Decimal::ONE,
            ts_exchange: None,
            ts_received: Utc::now(),
        };
        let book_b = CanonicalBook {
            venue: Venue::Polymarket,
            native_market_id: "TEST_PM".to_string(),
            canonical_id: "arb_test".to_string(),
            buy_yes: Decimal::from_str("0.70").unwrap(),
            buy_no: Decimal::from_str("0.50").unwrap(),
            depth_buy_yes: Decimal::from(100),
            depth_buy_no: Decimal::from(100),
            fee_rate: Decimal::from_str("0.00").unwrap(),
            tick_size: Decimal::from_str("0.01").unwrap(),
            min_order_size: Decimal::ONE,
            ts_exchange: None,
            ts_received: Utc::now(),
        };

        let opps = detector.detect_for_pair(&book_a, &book_b);
        assert!(!opps.is_empty(), "10c gross edge should produce an opportunity");
        assert!(opps[0].gross_edge == Decimal::from_str("0.10").unwrap());
        assert!(opps[0].yes_venue == Venue::Kalshi);
        assert!(opps[0].no_venue == Venue::Polymarket);
    }

    #[test]
    fn test_persistence_tracker() {
        use crate::persistence::PersistenceTracker;

        let mut tracker = PersistenceTracker::new(2);
        let key = ("market_a".to_string(), "kalshi".to_string());

        // First cycle — not yet persistent
        let persistent = tracker.update(&[key.clone()]);
        assert!(persistent.is_empty(), "should not be persistent after 1 cycle");

        // Second cycle — now persistent
        let persistent = tracker.update(&[key.clone()]);
        assert!(!persistent.is_empty(), "should be persistent after 2 cycles");

        // Gap cycle — count resets
        let persistent = tracker.update(&[]);
        assert!(persistent.is_empty(), "should reset after missed cycle");

        // One cycle again — not persistent yet
        let persistent = tracker.update(&[key.clone()]);
        assert!(persistent.is_empty(), "should need 2 cycles again after reset");
    }

    #[tokio::test]
    async fn test_kalshi_get_book_live() {
        use crate::models::Venue;
        use rust_decimal::Decimal;
        let cfg = load_config(None).unwrap();
        let mut kalshi = crate::adapters::kalshi::KalshiAdapter::new(cfg.kalshi);
        kalshi.connect().await.unwrap();

        let book = kalshi.get_book("KXRATECUT-26DEC31").await.unwrap();
        assert_eq!(book.venue, Venue::Kalshi);
        assert!(book.buy_yes > Decimal::ZERO, "buy_yes should be > 0");
        assert!(book.buy_no > Decimal::ZERO, "buy_no should be > 0");
        assert!(book.buy_yes <= Decimal::ONE);
        assert!(book.buy_no <= Decimal::ONE);

        kalshi.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn test_polymarket_get_book_live() {
        use crate::models::Venue;
        use rust_decimal::Decimal;
        let cfg = load_config(None).unwrap();
        let mut pm = crate::adapters::polymarket::PolymarketAdapter::new(cfg.polymarket);
        pm.connect().await.unwrap();

        let book = pm.get_book(
            "0xc60022fe066abd6f96c375adb09f38d92c4931f09c10b805354581b4e5465e93",
            "85002355202646770038788297383084634166875614093071220064343011133051368772502",
            "55388042878106612984650771012856953294835512286392194574801105901687237873381",
        ).await.unwrap();
        assert_eq!(book.venue, Venue::Polymarket);
        assert!(book.buy_yes > Decimal::ZERO);
        assert!(book.buy_no > Decimal::ZERO);
        assert!(book.buy_yes <= Decimal::ONE);
        assert!(book.buy_no <= Decimal::ONE);

        pm.disconnect().await.unwrap();
    }
}
