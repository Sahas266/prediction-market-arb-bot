use anyhow::{Context, Result};
use base64::Engine;
use chrono::Utc;
use reqwest::Client;
use rsa::pkcs1v15::SigningKey;
use rsa::signature::SignatureEncoding;
use rsa::signature::Signer;
use rsa::RsaPrivateKey;
use rust_decimal::Decimal;
use sha2::Sha256;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::config::KalshiConfig;
use crate::models::{CanonicalBook, Side, Venue};

pub struct KalshiAdapter {
    config: KalshiConfig,
    client: Option<Client>,
    private_key: Option<RsaPrivateKey>,
}

impl KalshiAdapter {
    pub fn new(config: KalshiConfig) -> Self {
        let private_key = if !config.rsa_private_key_pem.is_empty() {
            use rsa::pkcs8::DecodePrivateKey;
            match RsaPrivateKey::from_pkcs8_pem(&config.rsa_private_key_pem) {
                Ok(key) => Some(key),
                Err(e) => {
                    // Try PKCS1
                    use rsa::pkcs1::DecodeRsaPrivateKey;
                    match RsaPrivateKey::from_pkcs1_pem(&config.rsa_private_key_pem) {
                        Ok(key) => Some(key),
                        Err(e2) => {
                            warn!("Failed to parse Kalshi RSA key (PKCS8: {}, PKCS1: {})", e, e2);
                            None
                        }
                    }
                }
            }
        } else {
            None
        };

        Self {
            config,
            client: None,
            private_key,
        }
    }

    fn sign(&self, timestamp_ms: u64, method: &str, path: &str) -> Result<String> {
        let key = self
            .private_key
            .as_ref()
            .context("No RSA private key configured")?;
        let message = format!("{}{}{}", timestamp_ms, method, path);
        let signing_key = SigningKey::<Sha256>::new(key.clone());
        let signature: rsa::pkcs1v15::Signature = signing_key.sign(message.as_bytes());
        Ok(base64::engine::general_purpose::STANDARD.encode(signature.to_vec()))
    }

    fn auth_headers(&self, method: &str, path: &str) -> Result<Vec<(String, String)>> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis() as u64;
        let sig = self.sign(ts, method, path)?;
        Ok(vec![
            ("KALSHI-ACCESS-KEY".to_string(), self.config.api_key_id.clone()),
            ("KALSHI-ACCESS-TIMESTAMP".to_string(), ts.to_string()),
            ("KALSHI-ACCESS-SIGNATURE".to_string(), sig),
        ])
    }

    fn client(&self) -> &Client {
        self.client.as_ref().expect("Kalshi adapter not connected")
    }

    async fn get_json(&self, path: &str, auth: bool) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.config.rest_url, path);
        let mut req = self.client().get(&url);

        if auth {
            for (k, v) in self.auth_headers("GET", path)? {
                req = req.header(&k, &v);
            }
        }

        for attempt in 0..3 {
            let resp = req
                .try_clone()
                .unwrap()
                .send()
                .await?;

            if resp.status().as_u16() == 429 {
                let wait = (attempt + 1) as u64;
                warn!("Kalshi rate limited on GET {}, waiting {}s", path, wait);
                sleep(Duration::from_secs(wait)).await;
                continue;
            }

            let status = resp.status();
            let body = resp.text().await?;
            if !status.is_success() {
                anyhow::bail!("Kalshi GET {} returned {}: {}", path, status, body);
            }
            return Ok(serde_json::from_str(&body)?);
        }

        anyhow::bail!("Kalshi GET {} failed after 3 retries", path)
    }

    pub async fn connect(&mut self) -> Result<()> {
        self.client = Some(
            Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        );
        info!("Kalshi adapter connected");
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        self.client = None;
        Ok(())
    }

    pub async fn get_orderbook(&self, ticker: &str, depth: u32) -> Result<serde_json::Value> {
        let path = format!("/markets/{}/orderbook?depth={}", ticker, depth);
        self.get_json(&path, false).await
    }

    pub async fn get_book(&self, native_market_id: &str) -> Result<CanonicalBook> {
        let ob = self
            .get_orderbook(native_market_id, self.config.orderbook_depth)
            .await?;
        let now = Utc::now();

        let ob_fp = ob
            .get("orderbook_fp")
            .or_else(|| ob.get("orderbook"))
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let yes_levels = ob_fp
            .get("yes_dollars")
            .or_else(|| ob_fp.get("yes"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let no_levels = ob_fp
            .get("no_dollars")
            .or_else(|| ob_fp.get("no"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let (best_yes_bid, depth_yes_bid) = if let Some(last) = yes_levels.last() {
            let arr = last.as_array().unwrap();
            (
                parse_decimal(&arr[0]),
                parse_decimal(&arr[1]),
            )
        } else {
            (Decimal::ZERO, Decimal::ZERO)
        };

        let (best_no_bid, depth_no_bid) = if let Some(last) = no_levels.last() {
            let arr = last.as_array().unwrap();
            (
                parse_decimal(&arr[0]),
                parse_decimal(&arr[1]),
            )
        } else {
            (Decimal::ZERO, Decimal::ZERO)
        };

        let buy_yes = if best_no_bid > Decimal::ZERO {
            Decimal::ONE - best_no_bid
        } else {
            Decimal::ONE
        };
        let buy_no = if best_yes_bid > Decimal::ZERO {
            Decimal::ONE - best_yes_bid
        } else {
            Decimal::ONE
        };

        Ok(CanonicalBook {
            venue: Venue::Kalshi,
            native_market_id: native_market_id.to_string(),
            canonical_id: String::new(),
            buy_yes,
            buy_no,
            depth_buy_yes: depth_no_bid,
            depth_buy_no: depth_yes_bid,
            fee_rate: Decimal::from_str("0.01").unwrap(),
            tick_size: Decimal::from_str("0.01").unwrap(),
            min_order_size: Decimal::ONE,
            ts_exchange: None,
            ts_received: now,
        })
    }

    pub async fn place_order(
        &self,
        native_market_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
    ) -> Result<String> {
        let path = "/portfolio/orders";
        let url = format!("{}{}", self.config.rest_url, path);

        let mut body = serde_json::json!({
            "ticker": native_market_id,
            "side": side.to_string(),
            "action": "buy",
            "type": "limit",
            "count_fp": size.to_string(),
            "time_in_force": "fill_or_kill",
        });

        match side {
            Side::Yes => {
                body["yes_price_dollars"] = serde_json::Value::String(price.to_string());
            }
            Side::No => {
                body["no_price_dollars"] = serde_json::Value::String(price.to_string());
            }
        }

        let mut req = self.client().post(&url).json(&body);
        for (k, v) in self.auth_headers("POST", path)? {
            req = req.header(&k, &v);
        }

        let resp = req.send().await?;
        let data: serde_json::Value = resp.json().await?;
        let order = data.get("order").unwrap_or(&data);
        let order_id = order
            .get("order_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        info!(
            "Kalshi order placed: {} side={} price={} size={} → {}",
            native_market_id, side, price, size, order_id
        );
        Ok(order_id)
    }

    pub async fn cancel_order(&self, native_order_id: &str) -> Result<bool> {
        let path = format!("/portfolio/orders/{}", native_order_id);
        let url = format!("{}{}", self.config.rest_url, path);
        let mut req = self.client().delete(&url);
        for (k, v) in self.auth_headers("DELETE", &path)? {
            req = req.header(&k, &v);
        }
        let resp = req.send().await?;
        Ok(resp.status().is_success())
    }
}

fn parse_decimal(val: &serde_json::Value) -> Decimal {
    match val {
        serde_json::Value::Number(n) => {
            Decimal::from_str(&n.to_string()).unwrap_or(Decimal::ZERO)
        }
        serde_json::Value::String(s) => {
            Decimal::from_str(s).unwrap_or(Decimal::ZERO)
        }
        _ => Decimal::ZERO,
    }
}
