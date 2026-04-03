//! Auto-discovery tool for cross-venue prediction market pairs.
//!
//! Fetches active markets from Polymarket (Gamma API) and Kalshi,
//! applies text normalization and fuzzy matching to find candidate pairs,
//! optionally runs LLM consensus via OpenRouter free models for mid-confidence
//! pairs (score 0.50–0.85), and writes results to `mappings/candidate_pairs.json`
//! for human review.
//!
//! Usage: cargo run --release --bin discover_pairs
//! LLM consensus requires OPENROUTER_API_KEY in .env (optional; skipped if absent)

use anyhow::Result;
use futures_util::future::join_all;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Semaphore;

use std::path::PathBuf;

// ── LLM consensus constants ─────────────────────────────────────────────────

/// Top 20 free text models on OpenRouter selected for semantic market classification.
///
/// Selection criteria for our use case (binary: do two markets resolve on the same event?):
///   - Strong natural language understanding and instruction following
///   - Effective reasoning about date/resolution-criteria nuance
///   - Dense or large-MoE active parameters (small active-param MoE excluded)
///   - General-purpose models preferred over domain-specialized (coding excluded)
///
/// Capped at 20 to respect the OpenRouter free-tier rate limit (20 req/min).
/// Firing all 20 in parallel exhausts the per-minute budget in a single pair check.
///
/// Weights reflect effective capability for this task:
///   3.0 = 100B+ dense or large frontier MoE
///   2.0 = 20B–80B dense or strong proprietary
///   1.0 = 9B–12B solid small models
///   0.5 = 4B borderline (included for diversity, down-weighted)
///
/// EXCLUDED (6 models):
///   qwen/qwen3-coder:free              — coding specialist, domain mismatch
///   google/gemma-3n-e4b-it:free        — 4B nano (mobile efficiency arch), weakest remaining
///   meta-llama/llama-3.2-3b-instruct:free — 3B too small for nuanced classification
///   google/gemma-3n-e2b-it:free        — 2B too small
///   liquid/lfm-2.5-1.2b-thinking:free  — 1.2B, even chain-of-thought can't compensate
///   liquid/lfm-2.5-1.2b-instruct:free  — 1.2B too small
const MODEL_WEIGHTS: &[(&str, f64)] = &[
    // Tier 1 — 100B+ dense or large frontier MoE (weight 3.0)  [5 models, 15.0 weight]
    ("nousresearch/hermes-3-llama-3.1-405b:free",               3.0), // 405B, top open instruct model
    ("openai/gpt-oss-120b:free",                                 3.0), // 120B, OpenAI quality
    ("nvidia/nemotron-3-super-120b-a12b:free",                   3.0), // 120B, strong reasoning
    ("qwen/qwen3.6-plus:free",                                   3.0), // large MoE, strong NLU
    ("qwen/qwen3.6-plus-preview:free",                           3.0), // preview variant, different behavior

    // Tier 2 — 20B–80B dense or strong proprietary (weight 2.0)  [9 models, 18.0 weight]
    ("qwen/qwen3-next-80b-a3b-instruct:free",                    2.0), // 80B instruct
    ("meta-llama/llama-3.3-70b-instruct:free",                   2.0), // 70B, Meta's best instruct
    ("minimax/minimax-m2.5:free",                                2.0), // strong proprietary model
    ("cognitivecomputations/dolphin-mistral-24b-venice-edition:free", 2.0), // 24B, fine-tuned instruct
    ("google/gemma-3-27b-it:free",                               2.0), // 27B instruction-tuned
    ("arcee-ai/trinity-large-preview:free",                      2.0), // enterprise NLU specialist
    ("openai/gpt-oss-20b:free",                                  2.0), // 20B, OpenAI quality
    ("z-ai/glm-4.5-air:free",                                    2.0), // GLM-4 Air, strong reasoning
    ("stepfun/step-3.5-flash:free",                              2.0), // StepFun tuned, solid NLU

    // Tier 3 — 9B–30B sparse/small capable models (weight 1.0)  [5 models, 5.0 weight]
    ("nvidia/nemotron-3-nano-30b-a3b:free",                      1.0), // 30B total / 3B active; NVIDIA-tuned sparse MoE
    ("nvidia/nemotron-nano-12b-v2-vl:free",                      1.0), // 12B, text + vision capable
    ("nvidia/nemotron-nano-9b-v2:free",                          1.0), // 9B, NVIDIA tuned
    ("google/gemma-3-12b-it:free",                               1.0), // 12B instruction-tuned
    ("arcee-ai/trinity-mini:free",                               1.0), // compact enterprise model

    // Tier 4 — 4B borderline model for ensemble diversity (weight 0.5)  [1 model, 0.5 weight]
    ("google/gemma-3-4b-it:free",                                0.5), // 4B, Google quality
    // Total: 20 models, 38.5 maximum possible weight
];

/// Pairs scoring below this are discarded without LLM check.
const FUZZY_DISCARD: f64 = 0.50;
/// Pairs scoring above this are tentative matches; LLM check is skipped.
const FUZZY_AUTO: f64 = 0.85;

// ── Gamma API response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    condition_id: Option<String>,
    question: Option<String>,
    /// Long-form description including resolution criteria
    description: Option<String>,
    /// Where/how the market resolves (URL or text)
    resolution_source: Option<String>,
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
    fn parsed_token_ids(&self) -> Vec<String> {
        self.clob_token_ids
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default()
    }

    fn parsed_outcomes(&self) -> Vec<String> {
        self.outcomes
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default()
    }

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
    /// Primary resolution rules text
    rules_primary: Option<String>,
    /// Secondary/additional rules
    rules_secondary: Option<String>,
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

// ── LLM consensus types ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Vote {
    Same,
    Different,
    Uncertain,
}

#[derive(Debug, Serialize, Clone)]
struct ModelVote {
    model: String,
    /// Capability weight assigned to this model
    weight: f64,
    vote: Vote,
    /// Raw first line + reasoning from the model
    reasoning: String,
}

#[derive(Debug, Serialize, Clone)]
struct LlmConsensus {
    /// "SAME", "DIFFERENT", "UNCERTAIN", or "INSUFFICIENT_VOTES"
    result: String,
    /// Weighted score for the winning verdict (0.0–1.0 share of total weight)
    confidence: f64,
    weighted_same: f64,
    weighted_different: f64,
    weighted_uncertain: f64,
    /// Sum of weights of all models that responded
    total_weight_responded: f64,
    /// Sum of weights of all models queried (responded + timed out)
    total_weight_possible: f64,
    /// Raw count of models that responded (for debugging)
    total_responded: usize,
    votes: Vec<ModelVote>,
}

// ── Output types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct CandidatePair {
    suggested_id: String,
    /// Fuzzy similarity score (0.0–1.0)
    score: f64,
    match_reason: String,
    polymarket: PmCandidate,
    kalshi: KalshiCandidate,
    /// Set when score is in [FUZZY_DISCARD, FUZZY_AUTO]; null for auto-tentative pairs
    llm_consensus: Option<LlmConsensus>,
    /// Human reviewer should set to true/false after reviewing llm_consensus
    approved: Option<bool>,
}

#[derive(Debug, Serialize, Clone)]
struct PmCandidate {
    condition_id: String,
    question: String,
    /// Long-form description including resolution criteria (truncated to 600 chars for storage)
    description: Option<String>,
    /// Resolution source — URL or short text
    resolution_source: Option<String>,
    yes_token_id: Option<String>,
    no_token_id: Option<String>,
    neg_risk: bool,
    neg_risk_market_id: Option<String>,
    slug: Option<String>,
    end_date: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct KalshiCandidate {
    ticker: String,
    title: String,
    event_question: Option<String>,
    event_ticker: Option<String>,
    category: Option<String>,
    close_time: Option<String>,
    /// Primary resolution rules (truncated to 400 chars for storage)
    rules_primary: Option<String>,
    /// Secondary rules if present
    rules_secondary: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiscoveryOutput {
    generated_at: String,
    polymarket_markets_fetched: usize,
    kalshi_markets_fetched: usize,
    candidates: Vec<CandidatePair>,
    unmatched_polymarket: Vec<PmCandidate>,
    unmatched_kalshi: Vec<KalshiCandidate>,
}

// ── Text normalization ──────────────────────────────────────────────────────

fn normalize(text: &str) -> String {
    let s = text.to_lowercase();
    let s: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
        .collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

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

fn keyword_similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let set_a: std::collections::HashSet<&str> = a.iter().map(|s| s.as_str()).collect();
    let set_b: std::collections::HashSet<&str> = b.iter().map(|s| s.as_str()).collect();
    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 { 0.0 } else { intersection as f64 / union as f64 }
}

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
    if id.is_empty() { "unknown".to_string() } else { id }
}

// ── LLM consensus ───────────────────────────────────────────────────────────

fn parse_vote(content: &str) -> Vote {
    // Try first line first (models that follow the instruction format)
    let first_line = content.lines().next().unwrap_or(content).trim().to_uppercase();
    if first_line.starts_with("SAME") {
        return Vote::Same;
    }
    if first_line.starts_with("DIFFERENT") {
        return Vote::Different;
    }
    if first_line.starts_with("UNCERTAIN") {
        return Vote::Uncertain;
    }
    // Fall back: find which keyword appears first in the full text
    let upper = content.to_uppercase();
    let same_pos = upper.find("SAME").unwrap_or(usize::MAX);
    let diff_pos = upper.find("DIFFERENT").unwrap_or(usize::MAX);
    let unc_pos = upper.find("UNCERTAIN").unwrap_or(usize::MAX);
    let min_pos = same_pos.min(diff_pos).min(unc_pos);
    if min_pos == usize::MAX {
        Vote::Uncertain
    } else if min_pos == same_pos {
        Vote::Same
    } else if min_pos == diff_pos {
        Vote::Different
    } else {
        Vote::Uncertain
    }
}

async fn call_openrouter(
    client: &Client,
    api_key: &str,
    model: &str,
    weight: f64,
    prompt: &str,
    sem: Arc<Semaphore>,
) -> Option<ModelVote> {
    // Acquire a permit before sending — caps total concurrent OpenRouter connections.
    let _permit = sem.acquire_owned().await.ok()?;

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 150,
    });

    let send_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send(),
    )
    .await;
    let resp = match send_result {
        Ok(Ok(r)) => r,
        _ => return None,
    };

    let json: serde_json::Value = resp.json().await.ok()?;

    // Handle API-level errors (rate limit, model unavailable, etc.)
    if json.get("error").is_some() {
        return None;
    }

    let content = json["choices"][0]["message"]["content"].as_str()?.trim().to_string();
    if content.is_empty() {
        return None;
    }

    let vote = parse_vote(&content);
    // Use first two lines as reasoning (trim noise from long outputs)
    let reasoning: String = content
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();

    Some(ModelVote {
        model: model.to_string(),
        weight,
        vote,
        reasoning,
    })
}

async fn run_llm_consensus(
    client: &Client,
    api_key: &str,
    pm: &PmCandidate,
    km: &KalshiCandidate,
    sem: Arc<Semaphore>,
) -> LlmConsensus {
    // Build rich context for each market, including resolution criteria when available.
    // Truncate long fields to keep the prompt under ~1500 tokens total.
    let pm_desc = pm.description.as_deref().unwrap_or("").chars().take(500).collect::<String>();
    let pm_res_src = pm.resolution_source.as_deref().unwrap_or("not specified");
    let km_rules = {
        let primary = km.rules_primary.as_deref().unwrap_or("");
        let secondary = km.rules_secondary.as_deref().unwrap_or("");
        let combined = if secondary.is_empty() {
            primary.chars().take(400).collect::<String>()
        } else {
            format!("{} | {}", primary, secondary).chars().take(400).collect::<String>()
        };
        combined
    };

    let prompt = format!(
        "You are evaluating whether two prediction markets from different platforms resolve on \
EXACTLY the same real-world outcome — same event, same timeframe, same resolution criteria.\n\n\
=== Market A (Polymarket) ===\n\
Title: {pm_title}\n\
Closes: {pm_close}\n\
Resolution source: {pm_res_src}\n\
Description/Rules: {pm_desc}\n\n\
=== Market B (Kalshi) ===\n\
Title: {km_title}\n\
Closes: {km_close}\n\
Rules: {km_rules}\n\n\
Focus on: (1) resolution criteria — do they define the same outcome? \
(2) timeframe — same deadline? \
(3) resolution authority — same source?\n\n\
Answer with exactly ONE word on the first line: SAME / DIFFERENT / UNCERTAIN\n\
Then one sentence explaining the key deciding factor.",
        pm_title = pm.question,
        pm_close = pm.end_date.as_deref().unwrap_or("unknown"),
        pm_res_src = pm_res_src,
        pm_desc = if pm_desc.is_empty() { "not provided".to_string() } else { pm_desc },
        km_title = km.event_question.as_deref().unwrap_or(&km.title),
        km_close = km.close_time.as_deref().unwrap_or("unknown"),
        km_rules = if km_rules.is_empty() { "not provided".to_string() } else { km_rules },
    );

    let total_weight_possible: f64 = MODEL_WEIGHTS.iter().map(|(_, w)| w).sum();

    let futures: Vec<_> = MODEL_WEIGHTS
        .iter()
        .map(|(model, weight)| call_openrouter(client, api_key, model, *weight, &prompt, sem.clone()))
        .collect();

    let results = join_all(futures).await;

    let mut votes = Vec::new();
    let mut w_same = 0.0f64;
    let mut w_different = 0.0f64;
    let mut w_uncertain = 0.0f64;

    for result in results {
        if let Some(mv) = result {
            match mv.vote {
                Vote::Same => w_same += mv.weight,
                Vote::Different => w_different += mv.weight,
                Vote::Uncertain => w_uncertain += mv.weight,
            }
            votes.push(mv);
        }
    }

    let total_responded = votes.len();
    let total_weight_responded = w_same + w_different + w_uncertain;

    // Require at least 3.0 total weight (≥1 tier-1 model or ≥2 tier-2 models) to produce a verdict
    let (result, confidence) = if total_weight_responded < 3.0 {
        ("INSUFFICIENT_VOTES".to_string(), 0.0)
    } else if w_same >= w_different && w_same >= w_uncertain {
        let conf = w_same / total_weight_responded;
        ("SAME".to_string(), conf)
    } else if w_different >= w_same && w_different >= w_uncertain {
        let conf = w_different / total_weight_responded;
        ("DIFFERENT".to_string(), conf)
    } else {
        let conf = w_uncertain / total_weight_responded;
        ("UNCERTAIN".to_string(), conf)
    };

    LlmConsensus {
        result,
        confidence: (confidence * 1000.0).round() / 1000.0,
        weighted_same: (w_same * 100.0).round() / 100.0,
        weighted_different: (w_different * 100.0).round() / 100.0,
        weighted_uncertain: (w_uncertain * 100.0).round() / 100.0,
        total_weight_responded: (total_weight_responded * 100.0).round() / 100.0,
        total_weight_possible: (total_weight_possible * 100.0).round() / 100.0,
        total_responded,
        votes,
    }
}

// ── API fetching ────────────────────────────────────────────────────────────

/// Fetches resolution rules for a single Kalshi market via the individual endpoint.
///
/// The bulk list endpoint (`GET /markets?event_ticker=...`) returns empty `rules_primary` /
/// `rules_secondary`. Only `GET /markets/{ticker}` includes the full resolution rules text.
/// This is called per-candidate after fuzzy matching narrows the field, so the LLM prompt
/// receives the actual resolution criteria rather than empty strings.
async fn fetch_kalshi_market_rules(
    client: &Client,
    rest_url: &str,
    ticker: &str,
) -> (Option<String>, Option<String>) {
    let url = format!("{}/markets/{}", rest_url, ticker);
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return (None, None),
    };
    let text = match resp.text().await {
        Ok(t) => t,
        Err(_) => return (None, None),
    };
    let data: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let m = &data["market"];
    let rules_primary = m["rules_primary"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(400).collect::<String>());
    let rules_secondary = m["rules_secondary"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(400).collect::<String>());
    (rules_primary, rules_secondary)
}

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

        if offset > 5000 {
            println!("  Reached offset limit, stopping Polymarket fetch");
            break;
        }
    }

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

async fn fetch_kalshi_markets(
    client: &Client,
    rest_url: &str,
    event_titles_out: &mut std::collections::HashMap<String, String>,
) -> Result<Vec<KalshiMarket>> {
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

    for ev in &all_events {
        if let (Some(ref et), Some(ref title)) = (&ev.event_ticker, &ev.title) {
            event_titles_out.insert(et.clone(), title.clone());
        }
    }

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

    // Load .env for OPENROUTER_API_KEY
    dotenvy::from_path(root.join(".env")).ok();
    let openrouter_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    let llm_enabled = !openrouter_key.is_empty();

    if !llm_enabled {
        println!("Note: OPENROUTER_API_KEY not set — LLM consensus will be skipped.");
        println!("      Add it to .env to enable semantic validation of mid-confidence pairs.\n");
    }

    let config_content = std::fs::read_to_string(root.join("config.yaml"))?;
    let config: serde_yaml::Value = serde_yaml::from_str(&config_content)?;
    let gamma_url = config["venues"]["polymarket"]["gamma_url"]
        .as_str()
        .unwrap_or("https://gamma-api.polymarket.com");
    let kalshi_url = config["venues"]["kalshi"]["rest_url"]
        .as_str()
        .unwrap_or("https://api.elections.kalshi.com/trade-api/v2");

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

    println!("[1/5] Fetching Polymarket markets...");
    let pm_markets = fetch_polymarket_markets(&client, gamma_url).await?;
    println!("  Found {} active Polymarket markets\n", pm_markets.len());

    println!("[2/5] Fetching Kalshi binary markets via events API...");
    let mut event_titles = std::collections::HashMap::new();
    let kalshi_markets = fetch_kalshi_markets(&client, kalshi_url, &mut event_titles).await?;
    println!(
        "  Found {} binary Kalshi markets ({} events)\n",
        kalshi_markets.len(),
        event_titles.len()
    );

    println!("[3/5] Scoring candidates (fuzzy matching)...");
    let kalshi_keywords: Vec<(usize, Vec<String>)> = kalshi_markets
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let event_title = m
                .event_ticker
                .as_deref()
                .and_then(|et| event_titles.get(et))
                .map(|s| s.as_str())
                .unwrap_or("");
            let market_title = m.title.as_deref().unwrap_or("");
            let combined = if event_title.is_empty() {
                market_title.to_string()
            } else {
                format!("{} {}", event_title, market_title)
            };
            (i, keywords(&combined))
        })
        .collect();

    let mut candidates: Vec<CandidatePair> = Vec::new();
    let mut matched_kalshi_indices = std::collections::HashSet::new();
    let mut matched_pm_indices = std::collections::HashSet::new();

    for (pm_idx, pm) in pm_markets.iter().enumerate() {
        let pm_question = pm.question.as_deref().unwrap_or("");
        if pm_question.is_empty() {
            continue;
        }
        let pm_cid = pm.condition_id.as_deref().unwrap_or("");
        if existing_pm_ids.contains(pm_cid) {
            continue;
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

        // Tiered pipeline: discard below FUZZY_DISCARD
        if best_score < FUZZY_DISCARD {
            continue;
        }

        let km = &kalshi_markets[best_idx];
        let (yes_token, no_token) = pm.yes_no_tokens();

        let match_reason = if best_score >= FUZZY_AUTO {
            format!("[AUTO-TENTATIVE score={:.3}] {}", best_score, best_reason)
        } else {
            format!("[LLM-GATE score={:.3}] {}", best_score, best_reason)
        };

        candidates.push(CandidatePair {
            suggested_id: suggest_id(pm_question),
            score: (best_score * 1000.0).round() / 1000.0,
            match_reason,
            polymarket: PmCandidate {
                condition_id: pm_cid.to_string(),
                question: pm_question.to_string(),
                description: pm.description.as_deref().map(|s| s.chars().take(600).collect()),
                resolution_source: pm.resolution_source.clone(),
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
                rules_primary: km.rules_primary.as_deref().map(|s| s.chars().take(400).collect()),
                rules_secondary: km.rules_secondary.as_deref().map(|s| s.chars().take(400).collect()),
            },
            llm_consensus: None,
            approved: None,
        });
        matched_pm_indices.insert(pm_idx);
        matched_kalshi_indices.insert(best_idx);
    }

    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    println!("  {} candidates above fuzzy threshold {}\n", candidates.len(), FUZZY_DISCARD);

    // ── Enrich mid-tier candidates with Kalshi resolution rules (parallel) ──
    // The bulk list endpoint returns empty rules_primary/rules_secondary.
    // Fetch individual market details for all mid-tier candidates in parallel
    // so the LLM prompt receives the actual resolution criteria text.
    let mid_tier_indices: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.score < FUZZY_AUTO)
        .map(|(i, _)| i)
        .collect();
    let mid_tier_count = mid_tier_indices.len();

    if mid_tier_count > 0 {
        print!(
            "[3.5/5] Fetching Kalshi resolution rules for {} mid-tier candidates (parallel)... ",
            mid_tier_count
        );
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let rules_futures: Vec<_> = mid_tier_indices
            .iter()
            .map(|&i| fetch_kalshi_market_rules(&client, kalshi_url, &candidates[i].kalshi.ticker))
            .collect();
        let rules_results = join_all(rules_futures).await;

        let mut rules_fetched = 0usize;
        for (&i, (rp, rs)) in mid_tier_indices.iter().zip(rules_results.into_iter()) {
            if rp.is_some() || rs.is_some() {
                rules_fetched += 1;
            }
            candidates[i].kalshi.rules_primary = rp;
            candidates[i].kalshi.rules_secondary = rs;
        }
        println!("{}/{} had rules\n", rules_fetched, mid_tier_count);
    }

    // ── LLM consensus for mid-tier candidates (all parallel) ───────────────
    println!("[4/5] LLM consensus ({} mid-tier pairs × {} models, all parallel)...", mid_tier_count, MODEL_WEIGHTS.len());

    if !llm_enabled {
        println!("  Skipped (no OPENROUTER_API_KEY)\n");
    } else if mid_tier_count == 0 {
        println!("  No mid-tier pairs to evaluate\n");
    } else {
        let llm_client = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;

        // Semaphore caps total concurrent OpenRouter HTTP connections across all
        // candidates. 20 = OpenRouter free-tier req/min limit; connections that
        // complete free their permit immediately, allowing the next model call to
        // proceed. This prevents thundering-herd 429s while keeping all candidates
        // running in parallel.
        let sem = Arc::new(Semaphore::new(20));

        // Clone candidate data for the async closures (candidates is mutably
        // borrowed later to write results back).
        let consensus_inputs: Vec<(usize, PmCandidate, KalshiCandidate)> = mid_tier_indices
            .iter()
            .map(|&i| (i, candidates[i].polymarket.clone(), candidates[i].kalshi.clone()))
            .collect();

        let consensus_futures: Vec<_> = consensus_inputs
            .iter()
            .map(|(_, pm, km)| run_llm_consensus(&llm_client, &openrouter_key, pm, km, sem.clone()))
            .collect();

        println!("  Firing all {} candidates simultaneously...", mid_tier_count);
        let consensus_results = join_all(consensus_futures).await;

        let mut total_same = 0usize;
        let mut total_diff = 0usize;

        for ((idx, pm, _), consensus) in consensus_inputs.iter().zip(consensus_results.into_iter()) {
            let short_pm = if pm.question.len() > 50 {
                format!("{}...", &pm.question[..47])
            } else {
                pm.question.clone()
            };
            println!(
                "  \"{}\" → {} (conf={:.1}%, wS={:.1}/wD={:.1}/wU={:.1} from {}/{} models)",
                short_pm,
                consensus.result,
                consensus.confidence * 100.0,
                consensus.weighted_same,
                consensus.weighted_different,
                consensus.weighted_uncertain,
                consensus.total_responded,
                MODEL_WEIGHTS.len(),
            );
            if consensus.result == "SAME" {
                total_same += 1;
            } else if consensus.result == "DIFFERENT" {
                total_diff += 1;
            }
            candidates[*idx].llm_consensus = Some(consensus);
        }

        println!(
            "\n  LLM gate results: {} SAME, {} DIFFERENT/UNCERTAIN out of {} evaluated\n",
            total_same, total_diff, mid_tier_count
        );
    }

    // ── Build unmatched lists ───────────────────────────────────────────────
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
                description: m.description.as_deref().map(|s| s.chars().take(600).collect()),
                resolution_source: m.resolution_source.clone(),
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
            rules_primary: m.rules_primary.as_deref().map(|s| s.chars().take(400).collect()),
            rules_secondary: m.rules_secondary.as_deref().map(|s| s.chars().take(400).collect()),
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

    // ── Write output ────────────────────────────────────────────────────────
    let out_path = root.join("mappings").join("candidate_pairs.json");
    std::fs::create_dir_all(out_path.parent().unwrap())?;
    let json = serde_json::to_string_pretty(&output)?;
    std::fs::write(&out_path, &json)?;

    println!("[5/5] Results:\n");
    println!("  Polymarket markets: {}", output.polymarket_markets_fetched);
    println!("  Kalshi markets:     {}", output.kalshi_markets_fetched);
    println!("  Candidate pairs:    {}", output.candidates.len());
    println!();

    if !output.candidates.is_empty() {
        let auto_count = output.candidates.iter().filter(|c| c.score >= FUZZY_AUTO).count();
        let same_count = output
            .candidates
            .iter()
            .filter(|c| c.llm_consensus.as_ref().map(|l| l.result == "SAME").unwrap_or(false))
            .count();
        let diff_count = output
            .candidates
            .iter()
            .filter(|c| {
                c.llm_consensus
                    .as_ref()
                    .map(|l| l.result == "DIFFERENT")
                    .unwrap_or(false)
            })
            .count();

        if llm_enabled {
            println!(
                "  Auto-tentative (score >{:.2}):  {}",
                FUZZY_AUTO, auto_count
            );
            println!("  LLM says SAME:                  {}", same_count);
            println!("  LLM says DIFFERENT/UNCERTAIN:   {}", diff_count);
            println!(
                "  Action: review SAME + AUTO pairs and promote verified ones to manual_mappings.json"
            );
        }

        println!();
        println!(
            "  {:<6} {:<12} {:<45} {:<45}",
            "Score", "LLM", "Polymarket Question", "Kalshi Question"
        );
        println!("  {}", "-".repeat(108));
        for c in output.candidates.iter().take(30) {
            let pm_q = if c.polymarket.question.len() > 43 {
                format!("{}...", &c.polymarket.question[..40])
            } else {
                c.polymarket.question.clone()
            };
            let k_q_raw = c.kalshi.event_question.as_deref().unwrap_or(&c.kalshi.title);
            let k_q = if k_q_raw.len() > 43 {
                format!("{}...", &k_q_raw[..40])
            } else {
                k_q_raw.to_string()
            };
            let llm_label = match c.llm_consensus.as_ref() {
                Some(l) => l.result.clone(),
                None if c.score >= FUZZY_AUTO => "AUTO".to_string(),
                None => "SKIPPED".to_string(),
            };
            println!(
                "  {:<6.3} {:<12} {:<45} {:<45}",
                c.score, llm_label, pm_q, k_q
            );
        }
    }

    println!("\n  Output written to: {}", out_path.display());
    println!("  Review candidates and add verified pairs to mappings/manual_mappings.json");

    Ok(())
}
