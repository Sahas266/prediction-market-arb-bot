use reqwest::Client;
use tracing::warn;

/// Send a plain-text alert to a configured webhook URL (Slack, Discord, Telegram, PagerDuty, etc.)
/// This is fire-and-forget — failures are logged at warn level and never panic.
pub async fn send_alert(client: &Client, webhook_url: &str, message: &str) {
    if webhook_url.is_empty() {
        return;
    }

    // Support both Slack-style {"text": "..."} and generic webhook bodies
    let body = serde_json::json!({ "text": message });

    match client
        .post(webhook_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => warn!("Alert webhook returned {}", resp.status()),
        Err(e) => warn!("Alert webhook failed: {}", e),
    }
}
