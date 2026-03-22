use anyhow::Result;
use chrono::DateTime;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use tracing::info;

use crate::config::project_root;
use crate::models::{CanonicalContract, Venue, VenueMapping};

pub struct ContractRegistry {
    pub contracts: HashMap<String, CanonicalContract>,
    pub mappings: HashMap<(String, Venue), VenueMapping>,
    reverse: HashMap<(Venue, String), String>,
}

impl ContractRegistry {
    pub fn new() -> Self {
        Self {
            contracts: HashMap::new(),
            mappings: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    pub fn add_contract(&mut self, contract: CanonicalContract) {
        self.contracts.insert(contract.canonical_id.clone(), contract);
    }

    pub fn add_mapping(&mut self, mapping: VenueMapping) {
        let key = (mapping.canonical_id.clone(), mapping.venue);
        self.reverse.insert(
            (mapping.venue, mapping.native_market_id.clone()),
            mapping.canonical_id.clone(),
        );
        self.mappings.insert(key, mapping);
    }

    pub fn get_canonical_id(&self, venue: Venue, native_market_id: &str) -> Option<&String> {
        self.reverse.get(&(venue, native_market_id.to_string()))
    }

    pub fn get_mapping(&self, canonical_id: &str, venue: Venue) -> Option<&VenueMapping> {
        self.mappings.get(&(canonical_id.to_string(), venue))
    }

    pub fn get_paired_contracts(&self) -> Vec<(String, &VenueMapping, &VenueMapping)> {
        let mut pairs = Vec::new();
        for cid in self.contracts.keys() {
            let pm = self.mappings.get(&(cid.clone(), Venue::Polymarket));
            let km = self.mappings.get(&(cid.clone(), Venue::Kalshi));
            if let (Some(pm), Some(km)) = (pm, km) {
                if pm.is_verified && km.is_verified {
                    pairs.push((cid.clone(), pm, km));
                }
            }
        }
        pairs
    }

    pub fn load_manual_mappings(&mut self, path: Option<&Path>) -> Result<usize> {
        let default_path = project_root().join("mappings").join("manual_mappings.json");
        let path = path.unwrap_or(&default_path);

        if !path.exists() {
            tracing::warn!("Manual mappings file not found: {}", path.display());
            return Ok(0);
        }

        let content = std::fs::read_to_string(path)?;
        let data: MappingsFile = serde_json::from_str(&content)?;

        let mut count = 0;
        for entry in &data.mappings {
            let cutoff_str = entry.cutoff_time_utc.replace('Z', "+00:00");
            let cutoff = DateTime::parse_from_rfc3339(&cutoff_str)?.with_timezone(&chrono::Utc);

            let contract = CanonicalContract {
                canonical_id: entry.canonical_id.clone(),
                title: entry.title.clone().unwrap_or_default(),
                subject_key: entry.subject_key.clone().unwrap_or_default(),
                resolution_source: entry.resolution_source.clone().unwrap_or_default(),
                cutoff_time_utc: cutoff,
                category: entry.category.clone().unwrap_or_default(),
            };
            self.add_contract(contract);

            if let Some(pm) = &entry.venues.polymarket {
                self.add_mapping(VenueMapping {
                    canonical_id: entry.canonical_id.clone(),
                    venue: Venue::Polymarket,
                    native_market_id: pm.condition_id.clone(),
                    yes_token_id: pm.yes_token_id.clone(),
                    no_token_id: pm.no_token_id.clone(),
                    neg_risk: pm.neg_risk.unwrap_or(false),
                    confidence: Decimal::ONE,
                    method: "manual".to_string(),
                    is_verified: true,
                });
            }

            if let Some(km) = &entry.venues.kalshi {
                self.add_mapping(VenueMapping {
                    canonical_id: entry.canonical_id.clone(),
                    venue: Venue::Kalshi,
                    native_market_id: km.ticker.clone(),
                    yes_token_id: None,
                    no_token_id: None,
                    neg_risk: false,
                    confidence: Decimal::ONE,
                    method: "manual".to_string(),
                    is_verified: true,
                });
            }

            count += 1;
        }

        info!("Loaded {} manual mappings", count);
        Ok(count)
    }
}

#[derive(Deserialize)]
struct MappingsFile {
    mappings: Vec<MappingEntry>,
}

#[derive(Deserialize)]
struct MappingEntry {
    canonical_id: String,
    title: Option<String>,
    subject_key: Option<String>,
    category: Option<String>,
    cutoff_time_utc: String,
    resolution_source: Option<String>,
    venues: VenueEntries,
}

#[derive(Deserialize)]
struct VenueEntries {
    polymarket: Option<PolymarketEntry>,
    kalshi: Option<KalshiEntry>,
}

#[derive(Deserialize)]
struct PolymarketEntry {
    condition_id: String,
    yes_token_id: Option<String>,
    no_token_id: Option<String>,
    neg_risk: Option<bool>,
}

#[derive(Deserialize)]
struct KalshiEntry {
    ticker: String,
}
