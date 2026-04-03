use alloy_primitives::U256;
use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::config::PolymarketConfig;
use crate::models::{CanonicalBook, Side, Venue};
use crate::polymarket_signer::{compute_amounts, OrderParams, PolymarketSigner};

/// Cached book entry with timestamp for staleness detection.
struct CachedBook {
    data: serde_json::Value,
    updated_at: std::time::Instant,
}

pub struct PolymarketAdapter {
    config: PolymarketConfig,
    client: Option<Client>,
    signer: Option<PolymarketSigner>,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    fee_cache: Arc<RwLock<HashMap<String, Decimal>>>,
    book_cache: Arc<RwLock<HashMap<String, CachedBook>>>,
    ws_handle: Option<tokio::task::JoinHandle<()>>,
}

impl PolymarketAdapter {
    pub fn new(config: PolymarketConfig) -> Self {
        let signer = if !config.polygon_private_key.is_empty() {
            match PolymarketSigner::new(&config.polygon_private_key) {
                Ok(s) => {
                    debug!("Polymarket signer initialized: {}", s.address_hex());
                    Some(s)
                }
                Err(e) => {
                    warn!("Failed to init Polymarket signer: {} — order placement disabled", e);
                    None
                }
            }
        } else {
            None
        };

        Self {
            config,
            client: None,
            signer,
            api_key: None,
            api_secret: None,
            api_passphrase: None,
            fee_cache: Arc::new(RwLock::new(HashMap::new())),
            book_cache: Arc::new(RwLock::new(HashMap::new())),
            ws_handle: None,
        }
    }

    fn client(&self) -> Result<&Client> {
        self.client.as_ref().context("Polymarket adapter not connected — call connect() first")
    }

    pub async fn connect(&mut self) -> Result<()> {
        self.client = Some(
            Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        );
        info!("Polymarket adapter connected");
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(handle) = self.ws_handle.take() {
            handle.abort();
        }
        self.client = None;
        Ok(())
    }

    async fn get_clob_book(&self, token_id: &str) -> Result<serde_json::Value> {
        let url = format!("{}/book", self.config.clob_url);
        let resp = self
            .client()?
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await?;
        let body = resp.text().await?;
        Ok(serde_json::from_str(&body)?)
    }

    /// Maximum age (in seconds) for cached WS data before falling back to REST.
    const MAX_CACHE_AGE_SECS: u64 = 10;

    pub async fn get_book(
        &self,
        native_market_id: &str,
        yes_token_id: &str,
        no_token_id: &str,
    ) -> Result<CanonicalBook> {
        let now = Utc::now();
        let mut buy_yes = Decimal::ONE;
        let mut depth_buy_yes = Decimal::ZERO;
        let mut buy_no = Decimal::ONE;
        let mut depth_buy_no = Decimal::ZERO;
        let mut tick_size = Decimal::from_str("0.01").unwrap();
        let mut min_order_size = Decimal::ONE;

        if !yes_token_id.is_empty() {
            let yes_book = {
                let cache = self.book_cache.read().await;
                cache.get(yes_token_id).and_then(|entry| {
                    if entry.updated_at.elapsed().as_secs() < Self::MAX_CACHE_AGE_SECS {
                        Some(entry.data.clone())
                    } else {
                        None // stale cache, fall back to REST
                    }
                })
            };
            let yes_book = match yes_book {
                Some(b) => b,
                None => self.get_clob_book(yes_token_id).await?,
            };

            if let Some(asks) = yes_book.get("asks").and_then(|v| v.as_array()) {
                if let Some(best) = asks.first() {
                    buy_yes = parse_decimal_field(best, "price");
                    depth_buy_yes = parse_decimal_field(best, "size");
                }
            }
            if let Some(ts) = yes_book.get("tick_size").and_then(|v| v.as_str()) {
                tick_size = Decimal::from_str(ts).unwrap_or(tick_size);
            }
            if let Some(ms) = yes_book.get("min_order_size").and_then(|v| v.as_str()) {
                min_order_size = Decimal::from_str(ms).unwrap_or(min_order_size);
            }
        }

        if !no_token_id.is_empty() {
            let no_book = {
                let cache = self.book_cache.read().await;
                cache.get(no_token_id).and_then(|entry| {
                    if entry.updated_at.elapsed().as_secs() < Self::MAX_CACHE_AGE_SECS {
                        Some(entry.data.clone())
                    } else {
                        None
                    }
                })
            };
            let no_book = match no_book {
                Some(b) => b,
                None => self.get_clob_book(no_token_id).await?,
            };

            if let Some(asks) = no_book.get("asks").and_then(|v| v.as_array()) {
                if let Some(best) = asks.first() {
                    buy_no = parse_decimal_field(best, "price");
                    depth_buy_no = parse_decimal_field(best, "size");
                }
            }
        }

        // Validate prices are in [0, 1] range
        if buy_yes < Decimal::ZERO || buy_yes > Decimal::ONE {
            anyhow::bail!("Invalid buy_yes price {} for {}", buy_yes, native_market_id);
        }
        if buy_no < Decimal::ZERO || buy_no > Decimal::ONE {
            anyhow::bail!("Invalid buy_no price {} for {}", buy_no, native_market_id);
        }

        let fee_rate = self.get_fee_rate(
            if !yes_token_id.is_empty() { yes_token_id } else { no_token_id }
        ).await;

        Ok(CanonicalBook {
            venue: Venue::Polymarket,
            native_market_id: native_market_id.to_string(),
            canonical_id: String::new(),
            buy_yes,
            buy_no,
            depth_buy_yes,
            depth_buy_no,
            fee_rate,
            tick_size,
            min_order_size,
            ts_exchange: None,
            ts_received: now,
        })
    }

    pub async fn ws_connect(&mut self, token_ids: Vec<String>) -> Result<()> {
        if token_ids.is_empty() {
            return Ok(());
        }

        let ws_url = self.config.ws_url.clone();
        let heartbeat_interval = self.config.ws_heartbeat_interval_s;
        let book_cache = self.book_cache.clone();
        let subscribed_ids = Arc::new(token_ids.iter().cloned().collect::<std::collections::HashSet<String>>());

        let handle = tokio::spawn(async move {
            ws_loop(ws_url, token_ids, heartbeat_interval, book_cache, subscribed_ids).await;
        });

        self.ws_handle = Some(handle);
        info!("Polymarket WebSocket task started");
        Ok(())
    }

    /// Derive a CLOB API key using L1 auth. Must be called before place_order or get_order_status.
    pub async fn derive_api_key(&mut self) -> Result<()> {
        let signer = self.signer.as_ref().context("No signer configured")?;
        let nonce = rand::random::<u64>();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs();
        let sig = signer.sign_l1_auth(nonce, timestamp)?;

        let url = format!("{}/auth/derive-api-key", self.config.clob_url);
        let resp = self
            .client()?
            .get(&url)
            .header("POLY_ADDRESS", signer.address_hex())
            .header("POLY_SIGNATURE", &sig)
            .header("POLY_TIMESTAMP", timestamp.to_string())
            .header("POLY_NONCE", nonce.to_string())
            .send()
            .await?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("derive-api-key failed ({})", status);
        }

        self.api_key = body.get("apiKey").and_then(|v| v.as_str()).map(String::from);
        self.api_secret = body.get("secret").and_then(|v| v.as_str()).map(String::from);
        self.api_passphrase = body.get("passphrase").and_then(|v| v.as_str()).map(String::from);

        if self.api_key.is_some() {
            info!("Polymarket API key derived successfully");
        } else {
            warn!("derive-api-key response missing expected fields");
        }

        Ok(())
    }

    fn l2_auth_headers(&self) -> Result<Vec<(String, String)>> {
        let key = self.api_key.as_ref().context("No API key — call derive_api_key first")?;
        let secret = self.api_secret.as_deref().unwrap_or("");
        let passphrase = self.api_passphrase.as_deref().unwrap_or("");
        let signer = self.signer.as_ref().context("No signer configured")?;

        Ok(vec![
            ("POLY_ADDRESS".to_string(), signer.address_hex()),
            ("POLY_API_KEY".to_string(), key.clone()),
            ("POLY_API_SECRET".to_string(), secret.to_string()),
            ("POLY_PASSPHRASE".to_string(), passphrase.to_string()),
        ])
    }

    pub async fn place_order(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        neg_risk: bool,
    ) -> Result<String> {
        let signer = self.signer.as_ref().context("No signer — POLYGON_PRIVATE_KEY not set")?;

        let fee_rate = self.get_fee_rate(token_id).await;
        let fee_rate_bps = (fee_rate * Decimal::from(10_000))
            .floor()
            .to_string()
            .parse::<u64>()
            .unwrap_or(0);

        let (maker_amount, taker_amount) = compute_amounts(price, size, true);

        // Use random nonce to ensure uniqueness
        let nonce = U256::from(rand::random::<u64>());

        let params = OrderParams {
            token_id: token_id.to_string(),
            maker_amount,
            taker_amount,
            side: 0, // BUY
            fee_rate_bps: U256::from(fee_rate_bps),
            nonce,
            expiration: U256::from(0u64), // GTC
        };

        let signed = signer.sign_order(&params, neg_risk)?;

        let order_body = serde_json::json!({
            "order": {
                "salt": signed.salt,
                "maker": signed.maker,
                "signer": signed.signer,
                "taker": signed.taker,
                "tokenId": signed.token_id,
                "makerAmount": signed.maker_amount,
                "takerAmount": signed.taker_amount,
                "expiration": signed.expiration,
                "nonce": signed.nonce,
                "feeRateBps": signed.fee_rate_bps,
                "side": signed.side,
                "signatureType": 0,
                "signature": signed.signature,
            },
            "owner": signed.maker,
            "orderType": "FOK",
        });

        let url = format!("{}/order", self.config.clob_url);
        let mut req = self.client()?.post(&url).json(&order_body);

        // Add L2 auth headers if available
        if let Ok(headers) = self.l2_auth_headers() {
            for (k, v) in headers {
                req = req.header(&k, &v);
            }
        }

        let resp = req.send().await?;
        let status = resp.status();
        let data: serde_json::Value = resp.json().await?;

        if !status.is_success() {
            anyhow::bail!(
                "Polymarket order failed ({}): {}",
                status,
                serde_json::to_string_pretty(&data).unwrap_or_default()
            );
        }

        let order_id = data
            .get("orderID")
            .or_else(|| data.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        info!(
            "Polymarket order placed: token={} side={} price={} size={} → {}",
            token_id, side, price, size, order_id
        );
        Ok(order_id)
    }

    pub async fn get_order_status(&self, order_id: &str) -> Result<serde_json::Value> {
        let url = format!("{}/order/{}", self.config.clob_url, order_id);
        let mut req = self.client()?.get(&url);

        if let Ok(headers) = self.l2_auth_headers() {
            for (k, v) in headers {
                req = req.header(&k, &v);
            }
        }

        let resp = req.send().await?;
        let data: serde_json::Value = resp.json().await?;
        Ok(data)
    }

    async fn get_fee_rate(&self, token_id: &str) -> Decimal {
        if token_id.is_empty() {
            return Decimal::ZERO;
        }

        // Check cache first
        {
            let cache = self.fee_cache.read().await;
            if let Some(&rate) = cache.get(token_id) {
                return rate;
            }
        }

        let client = match self.client() {
            Ok(c) => c,
            Err(_) => return Decimal::ZERO,
        };

        let url = format!("{}/fee-rate", self.config.clob_url);
        let rate = match client
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    let bps = data
                        .get("base_fee")
                        .and_then(|v| v.as_str())
                        .and_then(|s| Decimal::from_str(s).ok())
                        .unwrap_or(Decimal::ZERO);
                    bps / Decimal::from(10000)
                } else {
                    Decimal::ZERO
                }
            }
            Err(_) => Decimal::ZERO,
        };

        // Cache for future calls
        let mut cache = self.fee_cache.write().await;
        cache.insert(token_id.to_string(), rate);
        rate
    }
}

async fn ws_loop(
    ws_url: String,
    token_ids: Vec<String>,
    heartbeat_interval: u64,
    book_cache: Arc<RwLock<HashMap<String, CachedBook>>>,
    subscribed_ids: Arc<std::collections::HashSet<String>>,
) {
    loop {
        let connect_result = tokio::time::timeout(
            Duration::from_secs(15),
            connect_async(&ws_url),
        ).await;

        match connect_result {
            Ok(Ok((ws_stream, _))) => {
                let (mut write, mut read) = ws_stream.split();

                // Subscribe
                let sub_msg = serde_json::json!({
                    "assets_ids": token_ids,
                    "type": "market",
                });
                if let Err(e) = write.send(Message::Text(sub_msg.to_string().into())).await {
                    error!("WS send error: {}", e);
                    continue;
                }
                info!("WS subscribed to {} tokens", token_ids.len());

                // Heartbeat task
                let heartbeat_write = Arc::new(tokio::sync::Mutex::new(write));
                let hb_write = heartbeat_write.clone();
                let hb_handle = tokio::spawn(async move {
                    loop {
                        sleep(Duration::from_secs(heartbeat_interval)).await;
                        let mut w = hb_write.lock().await;
                        if w.send(Message::Text("PING".into())).await.is_err() {
                            break;
                        }
                    }
                });

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            let text_str: &str = &text;
                            if text_str == "PONG" {
                                continue;
                            }
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text_str)
                            {
                                let msgs = if parsed.is_array() {
                                    parsed
                                        .as_array()
                                        .unwrap()
                                        .clone()
                                } else {
                                    vec![parsed]
                                };
                                for m in msgs {
                                    handle_ws_message(&book_cache, &m, &subscribed_ids).await;
                                }
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Err(e) => {
                            error!("WS read error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }

                hb_handle.abort();
            }
            Ok(Err(e)) => {
                error!("WS connection error: {} — reconnecting in 5s", e);
            }
            Err(_) => {
                error!("WS connection timed out — reconnecting in 5s");
            }
        }
        sleep(Duration::from_secs(5)).await;
    }
}

async fn handle_ws_message(
    book_cache: &Arc<RwLock<HashMap<String, CachedBook>>>,
    msg: &serde_json::Value,
    subscribed_ids: &Arc<std::collections::HashSet<String>>,
) {
    let event_type = msg.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
    let asset_id = msg
        .get("asset_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Only accept messages for subscribed tokens
    if !asset_id.is_empty() && !subscribed_ids.contains(&asset_id) {
        warn!("Ignoring WS message for unsubscribed asset: {}", &asset_id[..asset_id.len().min(20)]);
        return;
    }

    match event_type {
        "book" => {
            let mut cache = book_cache.write().await;
            cache.insert(asset_id, CachedBook {
                data: msg.clone(),
                updated_at: std::time::Instant::now(),
            });
        }
        "price_change" => {
            let mut cache = book_cache.write().await;
            if let Some(entry) = cache.get_mut(&asset_id) {
                apply_price_change(&mut entry.data, msg);
                entry.updated_at = std::time::Instant::now();
            }
        }
        _ => {}
    }
}

fn apply_price_change(book: &mut serde_json::Value, change: &serde_json::Value) {
    let side = change
        .get("side")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let price = change
        .get("price")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let size = change
        .get("size")
        .and_then(|v| v.as_str())
        .unwrap_or("0");

    let key = if side == "BUY" { "bids" } else { "asks" };
    // Ensure the key exists
    if let Some(obj) = book.as_object_mut() {
        obj.entry(key).or_insert(serde_json::json!([]));
    }

    if let Some(levels) = book.get_mut(key).and_then(|v| v.as_array_mut()) {
        // Remove existing level at this price
        levels.retain(|l| {
            l.get("price").and_then(|v| v.as_str()).unwrap_or("") != price
        });

        // Add new level if size > 0
        if size != "0" {
            levels.push(serde_json::json!({"price": price, "size": size}));
            // Sort using Decimal for correctness (not f64)
            levels.sort_by(|a, b| {
                let pa = a
                    .get("price")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Decimal::from_str(s).ok())
                    .unwrap_or(Decimal::ZERO);
                let pb = b
                    .get("price")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Decimal::from_str(s).ok())
                    .unwrap_or(Decimal::ZERO);
                if side == "BUY" {
                    pb.cmp(&pa)
                } else {
                    pa.cmp(&pb)
                }
            });
        }
    }
}

fn parse_decimal_field(val: &serde_json::Value, field: &str) -> Decimal {
    val.get(field)
        .and_then(|v| v.as_str())
        .and_then(|s| Decimal::from_str(s).ok())
        .unwrap_or(Decimal::ZERO)
}
