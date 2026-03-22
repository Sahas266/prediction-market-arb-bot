use anyhow::Result;
use rust_decimal::Decimal;
use std::collections::HashMap;
use tracing::{error, info, warn};

use crate::adapters::kalshi::KalshiAdapter;
use crate::adapters::polymarket::PolymarketAdapter;
use crate::models::{Opportunity, Side, Venue};

pub struct Executor {
    pub kalshi: KalshiAdapter,
    pub polymarket: PolymarketAdapter,
}

impl Executor {
    pub fn new(kalshi: KalshiAdapter, polymarket: PolymarketAdapter) -> Self {
        Self { kalshi, polymarket }
    }

    pub async fn execute_locked_arb(
        &self,
        opp: &Opportunity,
        venue_market_map: &HashMap<(Venue, String), String>,
        size: Decimal,
    ) -> (Option<String>, Option<String>) {
        let yes_market = venue_market_map
            .get(&(opp.yes_venue, opp.canonical_id.clone()))
            .cloned()
            .unwrap_or_default();
        let no_market = venue_market_map
            .get(&(opp.no_venue, opp.canonical_id.clone()))
            .cloned()
            .unwrap_or_default();

        if yes_market.is_empty() || no_market.is_empty() {
            error!("Missing market mapping for {}", opp.canonical_id);
            return (None, None);
        }

        // Determine leg order: Kalshi (fragile) first
        let legs: Vec<(&str, Venue, &str, Decimal, Side)> =
            if opp.yes_venue == Venue::Kalshi {
                vec![
                    ("yes", opp.yes_venue, &yes_market, opp.buy_yes_price, Side::Yes),
                    ("no", opp.no_venue, &no_market, opp.buy_no_price, Side::No),
                ]
            } else if opp.no_venue == Venue::Kalshi {
                vec![
                    ("no", opp.no_venue, &no_market, opp.buy_no_price, Side::No),
                    ("yes", opp.yes_venue, &yes_market, opp.buy_yes_price, Side::Yes),
                ]
            } else {
                vec![
                    ("yes", opp.yes_venue, &yes_market, opp.buy_yes_price, Side::Yes),
                    ("no", opp.no_venue, &no_market, opp.buy_no_price, Side::No),
                ]
            };

        // Leg 1
        let (label1, venue1, market1, price1, side1) = &legs[0];
        info!("Leg 1 ({}): {} {} at {} size {}", label1, venue1, market1, price1, size);
        let first_id = match self.place_on_venue(*venue1, market1, *side1, *price1, size).await {
            Ok(id) if !id.is_empty() => id,
            Ok(_) => {
                warn!("Leg 1 returned no order ID — aborting");
                return (None, None);
            }
            Err(e) => {
                error!("Leg 1 failed: {}", e);
                return (None, None);
            }
        };

        // Leg 2
        let (label2, venue2, market2, price2, side2) = &legs[1];
        info!("Leg 2 ({}): {} {} at {} size {}", label2, venue2, market2, price2, size);
        let second_id = match self.place_on_venue(*venue2, market2, *side2, *price2, size).await {
            Ok(id) => id,
            Err(e) => {
                error!("Leg 2 failed after leg 1 filled: {} — RESIDUAL EXPOSURE", e);
                return (Some(first_id), None);
            }
        };

        info!(
            "Both legs executed: {}, {} | net_edge={}",
            first_id, second_id, opp.net_edge
        );
        (Some(first_id), Some(second_id))
    }

    async fn place_on_venue(
        &self,
        venue: Venue,
        market: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
    ) -> Result<String> {
        match venue {
            Venue::Kalshi => self.kalshi.place_order(market, side, price, size).await,
            Venue::Polymarket => {
                // Polymarket place_order requires token_id — for now pass market as token
                // In production this would use the proper token_id from the mapping
                anyhow::bail!("Polymarket order execution requires py-clob-client SDK (not yet ported to Rust)")
            }
        }
    }
}
