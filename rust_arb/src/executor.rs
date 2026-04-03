use anyhow::{Context, Result};
use rust_decimal::Decimal;
use tracing::{error, info, warn};

use crate::adapters::kalshi::KalshiAdapter;
use crate::adapters::polymarket::PolymarketAdapter;
use crate::models::{Opportunity, Side, Venue, VenueMapping};

pub struct Executor<'a> {
    pub kalshi: &'a KalshiAdapter,
    pub polymarket: &'a PolymarketAdapter,
}

impl<'a> Executor<'a> {
    pub fn new(kalshi: &'a KalshiAdapter, polymarket: &'a PolymarketAdapter) -> Self {
        Self { kalshi, polymarket }
    }

    pub async fn execute_locked_arb(
        &self,
        opp: &Opportunity,
        pm_mapping: &VenueMapping,
        km_mapping: &VenueMapping,
        size: Decimal,
    ) -> (Option<String>, Option<String>) {
        // Build legs: fragile venue (Kalshi) first
        let legs: Vec<(&str, Venue, &VenueMapping, Decimal, Side)> =
            if opp.yes_venue == Venue::Kalshi {
                vec![
                    ("yes", Venue::Kalshi, km_mapping, opp.buy_yes_price, Side::Yes),
                    ("no", Venue::Polymarket, pm_mapping, opp.buy_no_price, Side::No),
                ]
            } else if opp.no_venue == Venue::Kalshi {
                vec![
                    ("no", Venue::Kalshi, km_mapping, opp.buy_no_price, Side::No),
                    ("yes", Venue::Polymarket, pm_mapping, opp.buy_yes_price, Side::Yes),
                ]
            } else {
                vec![
                    ("yes", opp.yes_venue, pm_mapping, opp.buy_yes_price, Side::Yes),
                    ("no", opp.no_venue, pm_mapping, opp.buy_no_price, Side::No),
                ]
            };

        // Leg 1
        let (label1, venue1, mapping1, price1, side1) = &legs[0];
        info!("Leg 1 ({}): {} {} at {} size {}", label1, venue1, mapping1.native_market_id, price1, size);
        let first_id = match self.place_on_venue(*venue1, mapping1, *side1, *price1, size).await {
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
        let (label2, venue2, mapping2, price2, side2) = &legs[1];
        info!("Leg 2 ({}): {} {} at {} size {}", label2, venue2, mapping2.native_market_id, price2, size);
        let second_id = match self.place_on_venue(*venue2, mapping2, *side2, *price2, size).await {
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
        mapping: &VenueMapping,
        side: Side,
        price: Decimal,
        size: Decimal,
    ) -> Result<String> {
        match venue {
            Venue::Kalshi => {
                self.kalshi
                    .place_order(&mapping.native_market_id, side, price, size)
                    .await
            }
            Venue::Polymarket => {
                let token_id = match side {
                    Side::Yes => mapping
                        .yes_token_id
                        .as_deref()
                        .context("missing yes_token_id for Polymarket")?,
                    Side::No => mapping
                        .no_token_id
                        .as_deref()
                        .context("missing no_token_id for Polymarket")?,
                };
                self.polymarket
                    .place_order(token_id, side, price, size, mapping.neg_risk)
                    .await
            }
        }
    }
}
