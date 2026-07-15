use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use reqwest::Client;
use router_crabs::{ChatRequest, forward_request, select_tier, TiersConfig};
use std::sync::Arc;
use tracing::info;

struct AppState {
    client: Client,
    config: TiersConfig,
}

// ── Handlers ───────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "OK — RouterCrabs"
}

async fn list_models(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mut models: Vec<serde_json::Value> = state
        .config
        .tiers
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.model,
                "object": "model",
                "owned_by": "router-crabs",
            })
        })
        .collect();

    // Add fallback models if present
    if let Some(ref fb) = state.config.fallback {
        models.push(serde_json::json!({
            "id": fb.simple.model,
            "object": "model",
            "owned_by": "router-crabs",
        }));
        models.push(serde_json::json!({
            "id": fb.complex.model,
            "object": "model",
            "owned_by": "router-crabs",
        }));
    }

    Json(serde_json::json!({
        "object": "list",
        "data": models,
    }))
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let req: ChatRequest = match serde_json::from_value(body.clone()) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid request: {}", e)})),
            )
                .into_response();
        }
    };

    let (tier, reason) = select_tier(&state.config, &req.messages);

    info!(
        tier = %tier.name,
        model = %tier.model,
        provider = %tier.api_base,
        reason,
        stream = req.stream.unwrap_or(false),
        "→ Routing"
    );

    match forward_request(&state.client, &tier, body).await {
        Ok(mut response) => {
            response.headers_mut().insert(
                "X-RouterCrabs-Tier",
                tier.name.parse().unwrap(),
            );
            response.headers_mut().insert(
                "X-RouterCrabs-Model",
                tier.model.parse().unwrap(),
            );
            response
                .headers_mut()
                .insert("X-RouterCrabs-Reason", reason.parse().unwrap());
            response
        }
        Err(e) => {
            tracing::error!("Proxy error: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("Proxy error: {}", e)})),
            )
                .into_response()
        }
    }
}

// ── Main ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,router_crabs=debug".into()),
        )
        .init();

    let config_path = std::env::var("TIERS_CONFIG")
        .unwrap_or_else(|_| "tiers.yaml".into());

    let config = TiersConfig::load(&config_path)?;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let port = config.port;
    let state = Arc::new(AppState { client, config });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(Arc::clone(&state));

    let addr = format!("{}:{}", state.config.host, port);
    info!("🚀 RouterCrabs started on http://{}", addr);
    info!("   Config: {}", config_path);

    // Display domain tiers
    if !state.config.tiers.is_empty() {
        info!("   Domain tiers:");
        for tier in &state.config.tiers {
            let badge = if tier.default { " 🏠" } else { "" };
            let kw_count = tier.keywords.len();
            info!(
                "     {:<20} → {:30}  [{} keywords, weight={}]{}",
                tier.name, tier.model, kw_count, tier.weight, badge
            );
        }
    }

    // Display complexity fallback
    if let Some(ref fb) = state.config.fallback {
        info!(
            "   Complexity fallback (threshold: {})",
            fb.threshold
        );
        info!(
            "     simple   → {:30}",
            fb.simple.model
        );
        info!(
            "     complex  → {:30}",
            fb.complex.model
        );
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
