//! Auto-discovery tool for cross-venue prediction market pairs.
//!
//! Fetches active markets from Polymarket (Gamma API) and Kalshi,
//! applies text normalization and fuzzy matching to find candidate pairs,
//! and writes results to `mappings/candidate_pairs.json` for human review.
//!
//! Usage: cargo run --release --bin discover_pairs

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use std::path::PathBuf;

// ── Gamma API response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    condition_id: Option<String>,
    question: Option<String>,
    /// JSON-encoded array string: "[\"token1\", \"token2\"]"
    #[serde(default)]
    clob_token_ids: Option<String>,
    /// Array like ["Yes", "No"]
    #[serde(default)]
    outcomes: Option<String>,
    neg_risk: Option<bool>,
    neg_risk_market_id: Option<String>,
    active: Option<bool>,
    closed: Option<bool>,
    end_date_iso: Option<String>,
    slug: Option<String>,
}

impl GammaMarket {
    /// Parse the clobTokenIds JSON string into a vec of token ID strings
    fn parsed_token_ids(&self) -> Vec<String> {
        self.clob_token_ids
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default()
    }

    /// Parse the outcomes JSON string into a vec of outcome strings
    fn parsed_outcomes(&self) -> Vec<String> {
        self.outcomes
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default()
    }

    /// Get (yes_token_id, no_token_id) by matching outcomes to token IDs
    fn yes_no_tokens(&self) -> (Option<String>, Option<String>) {
        let ids = self.parsed_token_ids();
        let outcomes = self.parsed_outcomes();
        let mut yes = None;
        let mut no = None;
        for (i, outcome) in outcomes.iter().enumerate() {
            if let Some(tid) = ids.get(i) {
                match outcome.as_str() {
                    "Yes" => yes = Some(tid.clone()),
                    "No" => no = Some(tid.clone()),
                    _ => {}
                }
            }
        }
        yes = yes.or_else(|| ids.first().cloned());
        no = no.or_else(|| ids.get(1).cloned());
        (yes, no)
    }
}

// ── Kalshi API response types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct KalshiMarketsResponse {
    markets: Option<Vec<KalshiMarket>>,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KalshiMarket {
    ticker: Option<String>,
    title: Option<String>,
    event_ticker: Option<String>,
    status: Option<String>,
    market_type: Option<String>,
    category: Option<String>,
    close_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KalshiEventResponse {
    event: Option<KalshiEvent>,
}

#[derive(Debug, Deserialize)]
struct KalshiEvent {
    event_ticker: Option<String>,
    title: Option<String>,
    sub_title: Option<String>,
    category: Option<String>,
}

// ── Output types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct CandidatePair {
    /// Suggested canonical ID (auto-generated from normalized titles)
    suggested_id: String,
    /// Similarity score (0.0–1.0)
    score: f64,
    /// Why this was matched
    match_reason: String,
    polymarket: PmCandidate,
    kalshi: KalshiCandidate,
    /// Human reviewer should set to true/false
    approved: Option<bool>,
}

#[derive(Debug, Serialize)]
struct PmCandidate {
    condition_id: String,
    question: String,
    yes_token_id: Option<String>,
    no_token_id: Option<String>,
    neg_risk: bool,
    neg_risk_market_id: Option<String>,
    slug: Option<String>,
    end_date: Option<String>,
}

#[derive(Debug, Serialize)]
struct KalshiCandidate {
    ticker: String,
    /// The market-level title (may be ugly for multi-leg sports markets)
    title: String,
    /// Event-level question — cleaner, human-readable
    event_question: Option<String>,
    event_ticker: Option<String>,
    category: Option<String>,
    close_time: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiscoveryOutput {
    generated_at: String,
    polymarket_markets_fetched: usize,
    kalshi_markets_fetched: usize,
    candidates: Vec<CandidatePair>,
    /// Polymarket markets with no Kalshi match
    unmatched_polymarket: Vec<PmCandidate>,
    /// Kalshi markets with no Polymarket match
    unmatched_kalshi: Vec<KalshiCandidate>,
}

// ── Text normalization ──────────────────────────────────────────────────────

fn normalize(text: &str) -> String {
    let s = text.to_lowercase();
    // Remove punctuation, collapse whitespace
    let s: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
        .collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract meaningful keywords, dropping stop words
fn keywords(text: &str) -> Vec<String> {
    let stop_words = [
        "the", "a", "an", "in", "on", "at", "to", "for", "of", "by", "with",
        "will", "be", "is", "are", "was", "were", "has", "have", "had",
        "do", "does", "did", "can", "could", "would", "should", "may", "might",
        "this", "that", "these", "those", "it", "its", "and", "or", "but",
        "if", "then", "than", "so", "as", "from", "up", "out", "off",
        "before", "after", "during", "between", "through", "into",
        "any", "each", "every", "all", "both", "few", "more", "most",
        "other", "some", "such", "no", "not", "only", "same",
        "end", "yes", "market", "markets",
    ];
    let norm = normalize(text);
    norm.split_whitespace()
        .filter(|w| w.len() > 1 && !stop_words.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Jaccard similarity of keyword sets
fn keyword_similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let set_a: std::collections::HashSet<&str> = a.iter().map(|s| s.as_str()).collect();
    let set_b: std::collections::HashSet<&str> = b.iter().map(|s| s.as_str()).collect();
    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Check if two sets of keywords share important domain-specific terms
fn domain_boost(a: &[String], b: &[String]) -> f64 {
    let domain_terms = [
        "recession", "fed", "rate", "cut", "senate", "house", "democrat", "republican",
        "dem", "rep", "gop", "trump", "biden", "election", "midterm", "2026", "2027",
        "inflation", "gdp", "unemployment", "bitcoin", "btc", "eth", "ethereum",
        "president", "congress", "supreme", "court", "war", "ukraine", "russia",
        "china", "tariff", "trade", "impeach", "resign", "pope", "ai",
    ];
    let set_a: std::collections::HashSet<&str> = a.iter().map(|s| s.as_str()).collect();
    let set_b: std::collections::HashSet<&str> = b.iter().map(|s| s.as_str()).collect();
    let shared_domain: usize = domain_terms
        .iter()
        .filter(|t| set_a.contains(*t) && set_b.contains(*t))
        .count();
    shared_domain as f64 * 0.15
}

fn compute_score(pm_question: &str, kalshi_title: &str) -> (f64, String) {
    let kw_pm = keywords(pm_question);
    let kw_k = keywords(kalshi_title);
    let jaccard = keyword_similarity(&kw_pm, &kw_k);
    let boost = domain_boost(&kw_pm, &kw_k);
    let score = (jaccard + boost).min(1.0);

    let shared: Vec<String> = {
        let set_a: std::collections::HashSet<&str> = kw_pm.iter().map(|s| s.as_str()).collect();
        let set_b: std::collections::HashSet<&str> = kw_k.iter().map(|s| s.as_str()).collect();
        set_a.intersection(&set_b).map(|s| s.to_string()).collect()
    };

    let reason = if shared.is_empty() {
        "low keyword overlap".to_string()
    } else {
        format!("shared keywords: {}", shared.join(", "))
    };

    (score, reason)
}

fn suggest_id(pm_question: &str) -> String {
    let kw = keywords(pm_question);
    let id = kw.iter().take(5).cloned().collect::<Vec<_>>().join("_");
    if id.is_empty() {
        "unknown".to_string()
    } else {
        id
    }
}

// ── API fetching ────────────────────────────────────────────────────────────

async fn fetch_polymarket_markets(client: &Client, gamma_url: &str) -> Result<Vec<GammaMarket>> {
    let mut all_markets = Vec::new();
    let mut offset = 0;
    let limit = 100;

    loop {
        let url = format!(
            "{}/markets?limit={}&offset={}&closed=false&active=true",
            gamma_url, limit, offset
        );
        println!("  Fetching Polymarket offset={}...", offset);
        let resp = client.get(&url).send().await?;
        let text = resp.text().await?;
        let markets: Vec<GammaMarket> = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("  Warning: failed to parse Polymarket response at offset {}: {}", offset, e);
                break;
            }
        };

        let count = markets.len();
        all_markets.extend(markets);

        if count < limit {
            break;
        }
        offset += limit;

        // Safety limit
        if offset > 5000 {
            println!("  Reached offset limit, stopping Polymarket fetch");
            break;
        }
    }

    // Filter to markets that have condition_id and token IDs
    let valid: Vec<GammaMarket> = all_markets
        .into_iter()
        .filter(|m| {
            m.condition_id.is_some()
                && !m.parsed_token_ids().is_empty()
                && m.active.unwrap_or(false)
                && !m.closed.unwrap_or(true)
        })
        .collect();

    Ok(valid)
}

/// Fetch Kalshi events first, then their markets.
/// This avoids the multi-variate sports market flood from /markets.
async fn fetch_kalshi_markets(
    client: &Client,
    rest_url: &str,
    event_titles_out: &mut std::collections::HashMap<String, String>,
) -> Result<Vec<KalshiMarket>> {
    // Step 1: fetch events
    let mut all_events = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut url = format!("{}/events?limit=200&status=open", rest_url);
        if let Some(ref c) = cursor {
            url.push_str(&format!("&cursor={}", c));
        }
        println!("  Fetching Kalshi events (cursor={})...", cursor.as_deref().unwrap_or("start"));
        let resp = client.get(&url).send().await?;
        let text = resp.text().await?;

        #[derive(Deserialize)]
        struct EventsResp {
            events: Option<Vec<KalshiEvent>>,
            cursor: Option<String>,
        }

        let data: EventsResp = serde_json::from_str(&text)?;

        if let Some(events) = data.events {
            let count = events.len();
            all_events.extend(events);
            if count < 200 {
                break;
            }
        } else {
            break;
        }

        match data.cursor {
            Some(c) if !c.is_empty() => cursor = Some(c),
            _ => break,
        }

        if all_events.len() > 5000 {
            println!("  Reached event limit");
            break;
        }
    }

    println!("  Found {} events", all_events.len());

    // Store event titles
    for ev in &all_events {
        if let (Some(ref et), Some(ref title)) = (&ev.event_ticker, &ev.title) {
            event_titles_out.insert(et.clone(), title.clone());
        }
    }

    // Step 2: fetch markets for each event
    let mut all_markets = Vec::new();
    let event_tickers: Vec<String> = all_events
        .iter()
        .filter_map(|e| e.event_ticker.clone())
        .collect();

    println!("  Fetching markets for {} events...", event_tickers.len());
    for (i, et) in event_tickers.iter().enumerate() {
        if i > 0 && i % 20 == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let url = format!("{}/markets?event_ticker={}&limit=100", rest_url, et);
        match client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(text) = resp.text().await {
                    if let Ok(data) = serde_json::from_str::<KalshiMarketsResponse>(&text) {
                        if let Some(markets) = data.markets {
                            all_markets.extend(markets);
                        }
                    }
                }
            }
            Err(_) => {}
        }
    }

    println!("  Total markets fetched: {}", all_markets.len());

    // Filter to active binary markets
    let valid: Vec<KalshiMarket> = all_markets
        .into_iter()
        .filter(|m| {
            m.ticker.is_some()
                && m.title.is_some()
                && matches!(m.status.as_deref(), Some("open") | Some("active"))
                && m.market_type.as_deref() == Some("binary")
        })
        .collect();

    Ok(valid)
}

// ── Main ────────────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
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

#[tokio::main]
async fn main() -> Result<()> {
    let root = project_root();

    // Read config for URLs
    let config_content = std::fs::read_to_string(root.join("config.yaml"))?;
    let config: serde_yaml::Value = serde_yaml::from_str(&config_content)?;
    let gamma_url = config["venues"]["polymarket"]["gamma_url"]
        .as_str()
        .unwrap_or("https://gamma-api.polymarket.com");
    let kalshi_url = config["venues"]["kalshi"]["rest_url"]
        .as_str()
        .unwrap_or("https://api.elections.kalshi.com/trade-api/v2");

    // Load existing manual mappings to exclude already-paired markets
    let manual_path = root.join("mappings").join("manual_mappings.json");
    let existing_pm_ids: std::collections::HashSet<String> = if manual_path.exists() {
        let content = std::fs::read_to_string(&manual_path)?;
        let data: serde_json::Value = serde_json::from_str(&content)?;
        data["mappings"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        m["venues"]["polymarket"]["condition_id"]
                            .as_str()
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };
    let existing_kalshi_tickers: std::collections::HashSet<String> = if manual_path.exists() {
        let content = std::fs::read_to_string(&manual_path)?;
        let data: serde_json::Value = serde_json::from_str(&content)?;
        data["mappings"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        m["venues"]["kalshi"]["ticker"]
                            .as_str()
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    println!("=== Cross-Venue Market Discovery ===\n");

    // Fetch from both venues
    println!("[1/4] Fetching Polymarket markets...");
    let pm_markets = fetch_polymarket_markets(&client, gamma_url).await?;
    println!("  Found {} active Polymarket markets\n", pm_markets.len());

    println!("[2/4] Fetching Kalshi binary markets via events API...");
    let mut event_titles = std::collections::HashMap::new();
    let kalshi_markets = fetch_kalshi_markets(&client, kalshi_url, &mut event_titles).await?;
    println!("  Found {} binary Kalshi markets ({} events)\n", kalshi_markets.len(), event_titles.len());

    // Build keyword index for Kalshi markets (using event title when available)
    println!("[3/4] Matching candidates...");
    let kalshi_keywords: Vec<(usize, Vec<String>)> = kalshi_markets
        .iter()
        .enumerate()
        .map(|(i, m)| {
            // Prefer event title (cleaner) over market title for matching
            let event_title = m
                .event_ticker
                .as_deref()
                .and_then(|et| event_titles.get(et))
                .map(|s| s.as_str())
                .unwrap_or("");
            let market_title = m.title.as_deref().unwrap_or("");
            // Combine both for keyword extraction
            let combined = if event_title.is_empty() {
                market_title.to_string()
            } else {
                format!("{} {}", event_title, market_title)
            };
            (i, keywords(&combined))
        })
        .collect();

    let min_score = 0.25; // Minimum score to consider a candidate
    let mut candidates = Vec::new();
    let mut matched_kalshi_indices = std::collections::HashSet::new();
    let mut matched_pm_indices = std::collections::HashSet::new();

    for (pm_idx, pm) in pm_markets.iter().enumerate() {
        let pm_question = pm.question.as_deref().unwrap_or("");
        if pm_question.is_empty() {
            continue;
        }
        let pm_cid = pm.condition_id.as_deref().unwrap_or("");
        if existing_pm_ids.contains(pm_cid) {
            continue; // Already in manual mappings
        }

        let mut best_score = 0.0;
        let mut best_idx = 0;
        let mut best_reason = String::new();

        for (k_idx, _k_kw) in &kalshi_keywords {
            let km = &kalshi_markets[*k_idx];
            let k_ticker = km.ticker.as_deref().unwrap_or("");
            if existing_kalshi_tickers.contains(k_ticker) {
                continue;
            }

            // Use event title for scoring when available (cleaner than market title)
            let event_title = km
                .event_ticker
                .as_deref()
                .and_then(|et| event_titles.get(et))
                .map(|s| s.as_str())
                .unwrap_or("");
            let match_target = if event_title.is_empty() {
                km.title.as_deref().unwrap_or("")
            } else {
                event_title
            };
            let (score, reason) = compute_score(pm_question, match_target);
            if score > best_score {
                best_score = score;
                best_idx = *k_idx;
                best_reason = reason;
            }
        }

        if best_score >= min_score {
            let km = &kalshi_markets[best_idx];

            let (yes_token, no_token) = pm.yes_no_tokens();

            candidates.push(CandidatePair {
                suggested_id: suggest_id(pm_question),
                score: (best_score * 1000.0).round() / 1000.0,
                match_reason: best_reason,
                polymarket: PmCandidate {
                    condition_id: pm_cid.to_string(),
                    question: pm_question.to_string(),
                    yes_token_id: yes_token,
                    no_token_id: no_token,
                    neg_risk: pm.neg_risk.unwrap_or(false),
                    neg_risk_market_id: pm.neg_risk_market_id.clone(),
                    slug: pm.slug.clone(),
                    end_date: pm.end_date_iso.clone(),
                },
                kalshi: KalshiCandidate {
                    ticker: km.ticker.clone().unwrap_or_default(),
                    title: km.title.clone().unwrap_or_default(),
                    event_question: km
                        .event_ticker
                        .as_deref()
                        .and_then(|et| event_titles.get(et))
                        .cloned(),
                    event_ticker: km.event_ticker.clone(),
                    category: km.category.clone(),
                    close_time: km.close_time.clone(),
                },
                approved: None,
            });
            matched_pm_indices.insert(pm_idx);
            matched_kalshi_indices.insert(best_idx);
        }
    }

    // Sort by score descending
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());

    // Build unmatched lists (top 50 each, for reference)
    let unmatched_pm: Vec<PmCandidate> = pm_markets
        .iter()
        .enumerate()
        .filter(|(i, m)| {
            !matched_pm_indices.contains(i)
                && !existing_pm_ids.contains(m.condition_id.as_deref().unwrap_or(""))
        })
        .take(50)
        .map(|(_, m)| {
            let (yes_token, no_token) = m.yes_no_tokens();
            PmCandidate {
                condition_id: m.condition_id.clone().unwrap_or_default(),
                question: m.question.clone().unwrap_or_default(),
                yes_token_id: yes_token,
                no_token_id: no_token,
                neg_risk: m.neg_risk.unwrap_or(false),
                neg_risk_market_id: m.neg_risk_market_id.clone(),
                slug: m.slug.clone(),
                end_date: m.end_date_iso.clone(),
            }
        })
        .collect();

    let unmatched_k: Vec<KalshiCandidate> = kalshi_markets
        .iter()
        .enumerate()
        .filter(|(i, m)| {
            !matched_kalshi_indices.contains(i)
                && !existing_kalshi_tickers.contains(m.ticker.as_deref().unwrap_or(""))
        })
        .take(50)
        .map(|(_, m)| KalshiCandidate {
            ticker: m.ticker.clone().unwrap_or_default(),
            title: m.title.clone().unwrap_or_default(),
            event_question: m
                .event_ticker
                .as_deref()
                .and_then(|et| event_titles.get(et))
                .cloned(),
            event_ticker: m.event_ticker.clone(),
            category: m.category.clone(),
            close_time: m.close_time.clone(),
        })
        .collect();

    let output = DiscoveryOutput {
        generated_at: chrono::Utc::now().to_rfc3339(),
        polymarket_markets_fetched: pm_markets.len(),
        kalshi_markets_fetched: kalshi_markets.len(),
        candidates,
        unmatched_polymarket: unmatched_pm,
        unmatched_kalshi: unmatched_k,
    };

    // Write output
    let out_path = root.join("mappings").join("candidate_pairs.json");
    std::fs::create_dir_all(out_path.parent().unwrap())?;
    let json = serde_json::to_string_pretty(&output)?;
    std::fs::write(&out_path, &json)?;

    // Print summary
    println!("\n[4/4] Results:\n");
    println!("  Polymarket markets: {}", output.polymarket_markets_fetched);
    println!("  Kalshi markets:     {}", output.kalshi_markets_fetched);
    println!("  Candidate pairs:    {}", output.candidates.len());
    println!();

    if !output.candidates.is_empty() {
        println!("  Top candidates (score >= 0.25):");
        println!("  {:<6} {:<45} {:<45}", "Score", "Polymarket Question", "Kalshi Question");
        println!("  {}", "-".repeat(96));
        for c in output.candidates.iter().take(25) {
            let pm_q = if c.polymarket.question.len() > 43 {
                format!("{}...", &c.polymarket.question[..40])
            } else {
                c.polymarket.question.clone()
            };
            // Show event question (clean) if available, otherwise market title
            let k_q_raw = c
                .kalshi
                .event_question
                .as_deref()
                .unwrap_or(&c.kalshi.title);
            let k_q = if k_q_raw.len() > 43 {
                format!("{}...", &k_q_raw[..40])
            } else {
                k_q_raw.to_string()
            };
            println!("  {:<6.3} {:<45} {:<45}", c.score, pm_q, k_q);
        }
    }

    println!("\n  Output written to: {}", out_path.display());
    println!("  Review candidates and add verified pairs to mappings/manual_mappings.json");

    Ok(())
}
