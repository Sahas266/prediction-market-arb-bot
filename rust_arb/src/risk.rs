use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::config::RiskConfig;
use crate::models::Opportunity;

pub struct RiskManager {
    config: RiskConfig,
    trade_timestamps: VecDeque<Instant>,
    consecutive_failures: u32,
    api_error_timestamps: VecDeque<Instant>,
    killed: bool,
    /// Per-contract notional positions: canonical_id -> total notional
    positions: HashMap<String, Decimal>,
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        Self {
            config,
            trade_timestamps: VecDeque::new(),
            consecutive_failures: 0,
            api_error_timestamps: VecDeque::new(),
            killed: false,
            positions: HashMap::new(),
        }
    }

    pub fn is_killed(&self) -> bool {
        self.killed
    }

    fn kill(&mut self, reason: &str) {
        self.killed = true;
        tracing::error!("KILL SWITCH activated: {}", reason);
    }

    /// Record a successful trade and update position tracking.
    pub fn record_trade(&mut self, canonical_id: &str, notional: Decimal) {
        self.trade_timestamps.push_back(Instant::now());
        self.consecutive_failures = 0;

        // Update position tracking
        let entry = self.positions.entry(canonical_id.to_string()).or_insert(Decimal::ZERO);
        *entry += notional;
    }

    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= self.config.max_consecutive_failures {
            self.kill(&format!(
                "{} consecutive execution failures",
                self.consecutive_failures
            ));
        }
    }

    pub fn record_api_error(&mut self) {
        let now = Instant::now();
        self.api_error_timestamps.push_back(now);
        let cutoff = now - std::time::Duration::from_secs(60);
        while self
            .api_error_timestamps
            .front()
            .is_some_and(|t| *t < cutoff)
        {
            self.api_error_timestamps.pop_front();
        }
        if self.api_error_timestamps.len() as u32 >= self.config.max_api_errors_per_minute {
            self.kill(&format!(
                "{} API errors in 1 minute",
                self.api_error_timestamps.len()
            ));
        }
    }

    fn trades_per_minute(&mut self) -> u32 {
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs(60);
        while self.trade_timestamps.front().is_some_and(|t| *t < cutoff) {
            self.trade_timestamps.pop_front();
        }
        self.trade_timestamps.len() as u32
    }

    pub fn check_opportunity(&mut self, opp: &Opportunity) -> (bool, String) {
        if self.killed {
            return (false, "kill switch active".to_string());
        }

        if self.trades_per_minute() >= self.config.max_trades_per_minute {
            return (false, "rate limit: too many trades per minute".to_string());
        }

        let existing = self.positions.get(&opp.canonical_id).copied().unwrap_or(Decimal::ZERO);
        let new_notional = opp.max_size * opp.buy_yes_price + opp.max_size * opp.buy_no_price;
        if existing + new_notional > self.config.max_notional_per_contract {
            return (
                false,
                format!(
                    "per-contract limit exceeded ({} existing + {} new > {} limit)",
                    existing, new_notional, self.config.max_notional_per_contract
                ),
            );
        }

        let grand_total: Decimal = self.positions.values().sum();
        if grand_total + new_notional > self.config.max_notional_total {
            return (
                false,
                format!("total notional limit exceeded ({} + {} > {})", grand_total, new_notional, self.config.max_notional_total),
            );
        }

        (true, "approved".to_string())
    }

    /// Returns the approved trade size, accounting for existing positions.
    pub fn approved_size(&self, opp: &Opportunity) -> Decimal {
        let existing = self.positions.get(&opp.canonical_id).copied().unwrap_or(Decimal::ZERO);
        let remaining = (self.config.max_notional_per_contract - existing).max(Decimal::ZERO);
        opp.max_size.min(remaining).min(self.config.max_notional_per_contract)
    }
}
