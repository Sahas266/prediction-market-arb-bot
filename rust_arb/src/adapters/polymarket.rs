use anyhow::Result;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info};

use crate::config::PolymarketConfig;
use crate::models::{CanonicalBook, Venue};

pub struct PolymarketAdapter {
    config: PolymarketConfig,
    client: Option<Client>,
    fee_cache: Arc<RwLock<HashMap<String, Decimal>>>,
    book_cache: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    ws_handle: Option<tokio::task::JoinHandle<()>>,
}

impl PolymarketAdapter {
    pub fn new(config: PolymarketConfig) -> Self {
        Self {
            config,
            client: None,
            fee_cache: Arc::new(RwLock::new(HashMap::new())),
            book_cache: Arc::new(RwLock::new(HashMap::new())),
            ws_handle: None,
        }
    }

    fn client(&self) -> &Client {
        self.client.as_ref().expect("Polymarket adapter not connected")
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
            .client()
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await?;
        let body = resp.text().await?;
        Ok(serde_json::from_str(&body)?)
    }

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
                cache.get(yes_token_id).cloned()
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
                cache.get(no_token_id).cloned()
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

        let handle = tokio::spawn(async move {
            ws_loop(ws_url, token_ids, heartbeat_interval, book_cache).await;
        });

        self.ws_handle = Some(handle);
        info!("Polymarket WebSocket task started");
        Ok(())
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

        let url = format!("{}/fee-rate", self.config.clob_url);
        let rate = match self
            .client()
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
    book_cache: Arc<RwLock<HashMap<String, serde_json::Value>>>,
) {
    loop {
        match connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
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
                                    handle_ws_message(&book_cache, &m).await;
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
            Err(e) => {
                error!("WS connection error: {} — reconnecting in 5s", e);
            }
        }
        sleep(Duration::from_secs(5)).await;
    }
}

async fn handle_ws_message(
    book_cache: &Arc<RwLock<HashMap<String, serde_json::Value>>>,
    msg: &serde_json::Value,
) {
    let event_type = msg.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
    let asset_id = msg
        .get("asset_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match event_type {
        "book" => {
            let mut cache = book_cache.write().await;
            cache.insert(asset_id, msg.clone());
        }
        "price_change" => {
            let mut cache = book_cache.write().await;
            if let Some(book) = cache.get_mut(&asset_id) {
                apply_price_change(book, msg);
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
            // Sort
            levels.sort_by(|a, b| {
                let pa: f64 = a
                    .get("price")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let pb: f64 = b
                    .get("price")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                if side == "BUY" {
                    pb.partial_cmp(&pa).unwrap()
                } else {
                    pa.partial_cmp(&pb).unwrap()
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
