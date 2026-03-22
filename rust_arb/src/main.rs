mod adapters;
mod config;
mod db;
mod detector;
mod executor;
mod models;
mod registry;
mod risk;

use anyhow::Result;
use tokio::signal;
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::adapters::kalshi::KalshiAdapter;
use crate::adapters::polymarket::PolymarketAdapter;
use crate::config::load_config;
use crate::db::{get_connection, init_db};
use crate::detector::{find_all_opportunities, OpportunityDetector};
use crate::models::CanonicalBook;
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

    loop {
        if *shutdown_rx.borrow() || risk_mgr.is_killed() {
            break;
        }

        cycle += 1;
        let mut books: Vec<CanonicalBook> = Vec::new();

        for (cid, pm_mapping, km_mapping) in &pairs {
            match kalshi.get_book(&km_mapping.native_market_id).await {
                Ok(mut kb) => {
                    kb.canonical_id = cid.clone();
                    db::log_book(&conn, &kb).ok();
                    books.push(kb);
                }
                Err(e) => {
                    error!("Error fetching Kalshi book for {}: {}", cid, e);
                    risk_mgr.record_api_error();
                }
            }

            let yes_tid = pm_mapping.yes_token_id.as_deref().unwrap_or("");
            let no_tid = pm_mapping.no_token_id.as_deref().unwrap_or("");
            match polymarket
                .get_book(&pm_mapping.native_market_id, yes_tid, no_tid)
                .await
            {
                Ok(mut pb) => {
                    pb.canonical_id = cid.clone();
                    db::log_book(&conn, &pb).ok();
                    books.push(pb);
                }
                Err(e) => {
                    error!("Error fetching Polymarket book for {}: {}", cid, e);
                    risk_mgr.record_api_error();
                }
            }
        }

        // Detect opportunities
        let opps = find_all_opportunities(&books, &detector);
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

            db::log_opportunity(&conn, opp).ok();

            let (approved, reason) = risk_mgr.check_opportunity(opp);
            if !approved {
                info!("  Skipped: {}", reason);
            }
        }

        if cycle % 30 == 0 {
            info!(
                "Cycle {}: {} opportunities (monitoring {} pairs)",
                cycle,
                opps.len(),
                pairs.len()
            );
        }

        // Sleep before next poll
        tokio::select! {
            _ = sleep(poll_interval) => {},
            _ = shutdown_rx.changed() => { break; },
        }
    }

    // Cleanup
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
        // Verify tables exist
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

        // Two books with no edge (prices sum to 1)
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

        // Book A: YES cheap at 0.40, Book B: NO cheap at 0.50 → gross = 1 - 0.40 - 0.50 = 0.10
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
        // Prices should be between 0 and 1
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

        // Fed rate cut market
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
