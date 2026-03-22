use chrono::Utc;
use rust_decimal::Decimal;
use std::collections::HashMap;
use uuid::Uuid;

use crate::config::DetectorConfig;
use crate::models::{CanonicalBook, Opportunity};

pub struct OpportunityDetector {
    config: DetectorConfig,
}

impl OpportunityDetector {
    pub fn new(config: DetectorConfig) -> Self {
        Self { config }
    }

    fn check_direction(
        &self,
        yes_book: &CanonicalBook,
        no_book: &CanonicalBook,
        combined_fee: Decimal,
    ) -> Option<Opportunity> {
        let gross = Decimal::ONE - yes_book.buy_yes - no_book.buy_no;
        let net = gross - combined_fee - self.config.slippage_buffer;
        let size = yes_book
            .depth_buy_yes
            .min(no_book.depth_buy_no)
            .min(self.config.max_trade_size);

        if net >= self.config.min_net_edge
            && size >= self.config.min_trade_size
            && yes_book.depth_buy_yes >= self.config.min_depth
            && no_book.depth_buy_no >= self.config.min_depth
        {
            Some(Opportunity {
                opportunity_id: Uuid::new_v4().to_string(),
                canonical_id: yes_book.canonical_id.clone(),
                yes_venue: yes_book.venue,
                no_venue: no_book.venue,
                buy_yes_price: yes_book.buy_yes,
                buy_no_price: no_book.buy_no,
                gross_edge: gross,
                net_edge: net,
                max_size: size,
                detected_at: Utc::now(),
                yes_book_age_ms: yes_book.age_ms(),
                no_book_age_ms: no_book.age_ms(),
            })
        } else {
            None
        }
    }

    pub fn detect_for_pair(&self, book_a: &CanonicalBook, book_b: &CanonicalBook) -> Vec<Opportunity> {
        if book_a.canonical_id != book_b.canonical_id {
            return vec![];
        }
        if !book_a.is_fresh(self.config.max_stale_ms) || !book_b.is_fresh(self.config.max_stale_ms) {
            return vec![];
        }

        let fee = book_a.fee_rate + book_b.fee_rate;
        let mut opps = Vec::new();

        // Direction 1: buy YES on A, buy NO on B
        if let Some(opp) = self.check_direction(book_a, book_b, fee) {
            opps.push(opp);
        }
        // Direction 2: buy YES on B, buy NO on A
        if let Some(opp) = self.check_direction(book_b, book_a, fee) {
            opps.push(opp);
        }

        opps
    }
}

pub fn find_all_opportunities(
    books: &[CanonicalBook],
    detector: &OpportunityDetector,
) -> Vec<Opportunity> {
    let mut grouped: HashMap<&str, Vec<&CanonicalBook>> = HashMap::new();
    for b in books {
        grouped.entry(b.canonical_id.as_str()).or_default().push(b);
    }

    let mut opps = Vec::new();
    for (_cid, contract_books) in &grouped {
        for i in 0..contract_books.len() {
            for j in (i + 1)..contract_books.len() {
                opps.extend(detector.detect_for_pair(contract_books[i], contract_books[j]));
            }
        }
    }

    opps.sort_by(|a, b| b.net_edge.cmp(&a.net_edge));
    opps
}
