use axum::{routing::get, Router};
use rust_decimal::Decimal;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

/// Shared state updated by the main loop and read by the health endpoint.
#[derive(Debug, Clone, Default)]
pub struct HealthState {
    pub cycle: u64,
    pub last_cycle_at: Option<std::time::Instant>,
    pub ws_connected: bool,
    pub kill_switch_active: bool,
    pub opportunities_last_cycle: usize,
    pub total_notional: Decimal,
    pub api_errors_per_minute: u32,
}

pub type SharedHealth = Arc<RwLock<HealthState>>;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    cycle: u64,
    last_cycle_age_secs: Option<f64>,
    ws_connected: bool,
    kill_switch_active: bool,
    opportunities_last_cycle: usize,
    total_notional: String,
    api_errors_per_minute: u32,
}

pub async fn serve(state: SharedHealth, port: u16) {
    let app = Router::new()
        .route("/health", get(move || health_handler(state.clone())))
        .route("/", get(|| async { "arb_monitor running" }));

    let addr = format!("0.0.0.0:{}", port);
    info!("Health server listening on http://{}/health", addr);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Failed to bind health server on {}: {}", addr, e);
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        tracing::warn!("Health server error: {}", e);
    }
}

async fn health_handler(state: SharedHealth) -> axum::Json<HealthResponse> {
    let s = state.read().await;
    let last_cycle_age_secs = s
        .last_cycle_at
        .map(|t| t.elapsed().as_secs_f64());
    let status = if s.kill_switch_active {
        "killed"
    } else if last_cycle_age_secs.map(|a| a > 30.0).unwrap_or(true) {
        "degraded"
    } else {
        "ok"
    };

    axum::Json(HealthResponse {
        status,
        cycle: s.cycle,
        last_cycle_age_secs,
        ws_connected: s.ws_connected,
        kill_switch_active: s.kill_switch_active,
        opportunities_last_cycle: s.opportunities_last_cycle,
        total_notional: s.total_notional.to_string(),
        api_errors_per_minute: s.api_errors_per_minute,
    })
}
