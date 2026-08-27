use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use reqwest::Client;
use router_crabs::{
    ChatRequest, forward_request, matched_technical_keywords, score_complexity, select_tier,
    TiersConfig,
};
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::{Arc, Mutex};
use tracing::info;

struct AppState {
    client: Client,
    config: TiersConfig,
    journal: Journal,
    last_req_id: Mutex<Option<String>>,
}

/// Append-only JSONL journal. Every request and feedback signal is written
/// here as one JSON object per line. Opened with `O_APPEND` and `0600`, so
/// lines are never rewritten in place and the file stays private.
struct Journal {
    file: Mutex<File>,
}

impl Journal {
    fn new(path: &str) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        // Ensure 0600 even if the file pre-existed with looser perms.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn append(&self, value: &serde_json::Value) -> anyhow::Result<()> {
        let mut f = self.file.lock().unwrap();
        writeln!(f, "{}", serde_json::to_string(value)?)?;
        f.flush()?;
        Ok(())
    }
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
    headers: HeaderMap,
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

    // ── Phase 1: journal the request (append-only, never blocks routing) ──
    let req_id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let profile = headers
        .get("x-routercrabs-profile")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    let raw_prompt = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.text())
        .unwrap_or_default();
    let (prompt, prompt_truncated) = if raw_prompt.chars().count() > 2000 {
        (raw_prompt.chars().take(2000).collect::<String>(), true)
    } else {
        (raw_prompt, false)
    };

    let score = score_complexity(&req.messages, &state.config.keywords);
    let matched = matched_technical_keywords(&req.messages, &state.config.keywords);
    let routed = match tier.name.as_str() {
        "simple-fallback" => "flash",
        "complex-fallback" => "pro",
        other => other,
    };

    let entry = serde_json::json!({
        "type": "req",
        "id": req_id,
        "ts": ts,
        "profile": profile,
        "prompt": prompt,
        "prompt_truncated": prompt_truncated,
        "score": score,
        "matched": matched,
        "routed": routed,
        "reason": reason,
    });

    if let Err(e) = state.journal.append(&entry) {
        tracing::error!("journal append failed: {}", e);
    } else {
        *state.last_req_id.lock().unwrap() = Some(req_id);
    }

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

/// Feedback body: `{"correction": "pro"|"flash"}` (alias `correct_tier`).
/// `source` defaults to `"slash"` (the explicit `/pro` and `/flash` commands).
#[derive(Debug, Deserialize)]
struct FeedbackRequest {
    #[serde(alias = "correct_tier")]
    correction: String,
    #[serde(default = "default_source")]
    source: String,
}

fn default_source() -> String {
    "slash".into()
}

/// Records explicit feedback about the *last* routed request, as a `fb` line
/// in the journal referencing that request's id.
async fn record_feedback(
    State(state): State<Arc<AppState>>,
    Json(body): Json<FeedbackRequest>,
) -> Response {
    let correction = body.correction.to_lowercase();
    if correction != "pro" && correction != "flash" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "correction must be 'pro' or 'flash'"})),
        )
            .into_response();
    }

    let req_id = match state.last_req_id.lock().unwrap().clone() {
        Some(id) => id,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "no recent request to correct"})),
            )
                .into_response();
        }
    };

    let entry = serde_json::json!({
        "type": "fb",
        "req_id": req_id,
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "correct_tier": correction,
        "source": body.source,
    });

    match state.journal.append(&entry) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => {
            tracing::error!("journal append failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("journal append failed: {}", e)})),
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

    let journal = Journal::new(&config.journal_path)?;

    let port = config.port;
    let state = Arc::new(AppState {
        client,
        config,
        journal,
        last_req_id: Mutex::new(None),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/feedback", post(record_feedback))
        .with_state(Arc::clone(&state));

    let addr = format!("{}:{}", state.config.host, port);
    info!("🚀 RouterCrabs started on http://{}", addr);
    info!("   Config: {}", config_path);
    info!("   Journal: {}", state.config.journal_path);

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
