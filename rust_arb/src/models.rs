use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Venue {
    Polymarket,
    Kalshi,
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Venue::Polymarket => write!(f, "polymarket"),
            Venue::Kalshi => write!(f, "kalshi"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Yes => write!(f, "yes"),
            Side::No => write!(f, "no"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalContract {
    pub canonical_id: String,
    pub title: String,
    pub subject_key: String,
    pub resolution_source: String,
    pub cutoff_time_utc: DateTime<Utc>,
    pub category: String,
}

#[derive(Debug, Clone)]
pub struct VenueMapping {
    pub canonical_id: String,
    pub venue: Venue,
    pub native_market_id: String,
    pub yes_token_id: Option<String>,
    pub no_token_id: Option<String>,
    pub neg_risk: bool,
    pub confidence: Decimal,
    pub method: String,
    pub is_verified: bool,
}

#[derive(Debug, Clone)]
pub struct CanonicalBook {
    pub venue: Venue,
    pub native_market_id: String,
    pub canonical_id: String,
    pub buy_yes: Decimal,
    pub buy_no: Decimal,
    pub depth_buy_yes: Decimal,
    pub depth_buy_no: Decimal,
    pub fee_rate: Decimal,
    pub tick_size: Decimal,
    pub min_order_size: Decimal,
    pub ts_exchange: Option<DateTime<Utc>>,
    pub ts_received: DateTime<Utc>,
}

impl CanonicalBook {
    pub fn age_ms(&self) -> i64 {
        let delta = Utc::now() - self.ts_received;
        delta.num_milliseconds()
    }

    pub fn is_fresh(&self, max_age_ms: i64) -> bool {
        self.age_ms() <= max_age_ms
    }
}

#[derive(Debug, Clone)]
pub struct Opportunity {
    pub opportunity_id: String,
    pub canonical_id: String,
    pub yes_venue: Venue,
    pub no_venue: Venue,
    pub buy_yes_price: Decimal,
    pub buy_no_price: Decimal,
    pub gross_edge: Decimal,
    pub net_edge: Decimal,
    pub max_size: Decimal,
    pub detected_at: DateTime<Utc>,
    pub yes_book_age_ms: i64,
    pub no_book_age_ms: i64,
}
