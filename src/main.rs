use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response, sse::{Event, Sse}},
    routing::{get, post},
    Json, Router,
};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

// ── Config YAML ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct RawTier {
    model: String,
    api_base: String,
    api_key: String,
    #[serde(default = "default_auth_header")]
    auth_header: String,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default = "default_weight")]
    weight: u32,
    #[serde(default)]
    default: bool,
}

fn default_auth_header() -> String { "Bearer".into() }
fn default_weight() -> u32 { 1 }

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default = "default_port")]
    port: u16,
    tiers: Vec<RawTier>,
}

fn default_port() -> u16 { 8001 }

// ── Tier résolu (env vars interpolés) ──────────────────────────────────

#[derive(Debug, Clone)]
struct Tier {
    name: String,
    model: String,
    api_base: String,
    api_key: String,
    auth_header: String,
    keywords: Vec<String>,
    weight: u32,
    default: bool,
}

impl Tier {
    fn from_raw(raw: RawTier, name: String) -> Self {
        Self {
            name,
            model: raw.model,
            api_base: interpolate_env(&raw.api_base),
            api_key: interpolate_env(&raw.api_key),
            auth_header: raw.auth_header,
            keywords: raw.keywords,
            weight: raw.weight,
            default: raw.default,
        }
    }
}

fn interpolate_env(s: &str) -> String {
    let mut result = s.to_string();
    let mut start = 0;
    while let Some(begin) = result[start..].find("${") {
        let abs_begin = start + begin;
        if let Some(end) = result[abs_begin..].find('}') {
            let abs_end = abs_begin + end;
            let var_name = &result[abs_begin + 2..abs_end];
            let value = std::env::var(var_name).unwrap_or_default();
            result.replace_range(abs_begin..=abs_end, &value);
            start = abs_begin + value.len();
        } else {
            break;
        }
    }
    result
}

#[derive(Debug)]
struct AppConfig {
    port: u16,
    tiers: Vec<Tier>,
}

impl AppConfig {
    fn load(path: &str) -> anyhow::Result<Self> {
        let yaml = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Impossible de lire {}: {}", path, e))?;

        let raw: RawConfig = serde_yaml::from_str(&yaml)
            .map_err(|e| anyhow::anyhow!("YAML invalide dans {}: {}", path, e))?;

        if raw.tiers.is_empty() {
            anyhow::bail!("Aucun tier défini dans {}", path);
        }

        let tier_names = raw.tiers.iter().map(|t| t.model.clone()).collect::<Vec<_>>();
        let tiers: Vec<Tier> = raw.tiers
            .into_iter()
            .zip(tier_names)
            .map(|(raw, name)| Tier::from_raw(raw, name))
            .collect();

        let has_default = tiers.iter().any(|t| t.default);
        if !has_default {
            anyhow::bail!("Aucun tier avec `default: true` dans {}", path);
        }

        Ok(Self { port: raw.port, tiers })
    }
}

// ── OpenAI-compatible types ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatRequest {
    messages: Vec<Message>,
    stream: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
struct Message {
    #[allow(dead_code)]
    role: String,
    content: MessageContent,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    MultiPart(Vec<ContentPart>),
}

impl MessageContent {
    fn as_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::MultiPart(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    #[allow(dead_code)]
    ImageUrl { image_url: serde_json::Value },
}

// ── Sélection du tier ──────────────────────────────────────────────────
//
// Pour chaque tier, on compte combien de ses mots-clés apparaissent
// dans le prompt (insensible à la casse, substring match).
// Score = nombre_de_matchs × poids_du_tier.
// On choisit le tier au score le plus élevé.
// En cas d'égalité : poids le plus élevé, puis `default: true`.

fn select_tier<'a>(tiers: &'a [Tier], messages: &[Message]) -> (&'a Tier, String) {
    let full_text: String = messages
        .iter()
        .map(|m| m.content.as_text())
        .collect::<Vec<_>>()
        .join(" ");
    let lower = full_text.to_lowercase();

    let mut best: Option<&Tier> = None;
    let mut best_score: u32 = 0;
    let mut best_matches: Vec<String> = vec![];

    for tier in tiers {
        if tier.keywords.is_empty() {
            continue; // tier sans mots-clés → fallback, pas scoré
        }

        let matched: Vec<&String> = tier.keywords
            .iter()
            .filter(|kw| lower.contains(&kw.to_lowercase()))
            .collect();

        let match_count = matched.len() as u32;
        if match_count == 0 {
            continue;
        }

        let score = match_count * tier.weight;

        debug!(
            tier = %tier.name,
            matches = ?matched.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            match_count,
            weight = tier.weight,
            score,
            "Score tier"
        );

        let is_better = match best {
            None => true,
            Some(_b) if score > best_score => true,
            Some(_b) if score == best_score && tier.weight > _b.weight => true,
            Some(_b) if score == best_score && tier.weight == _b.weight && tier.default => true,
            _ => false,
        };

        if is_better {
            best = Some(tier);
            best_score = score;
            best_matches = matched.iter().map(|s| s.to_string()).collect();
        }
    }

    // Si aucun tier n'a matché, prendre le tier par défaut
    if best.is_none() {
        let default = tiers.iter().find(|t| t.default).expect("default tier requis");
        return (default, "default (aucun mot-clé matché)".into());
    }

    let reason = format!(
        "{} (matches: [{}], score: {})",
        best.unwrap().name,
        best_matches.join(", "),
        best_score,
    );

    (best.unwrap(), reason)
}

// ── Proxy vers le provider upstream ────────────────────────────────────

async fn forward_request(
    client: &Client,
    tier: &Tier,
    body: serde_json::Value,
) -> anyhow::Result<Response> {
    let mut body = body;
    body["model"] = serde_json::Value::String(tier.model.clone());

    let url = format!("{}/v1/chat/completions", tier.api_base);
    let stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json");

    if tier.auth_header == "Bearer" {
        req = req.header("Authorization", format!("Bearer {}", tier.api_key));
    } else {
        req = req.header(&tier.auth_header, &tier.api_key);
    }

    let resp = req.json(&body).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        warn!(status = status.as_u16(), body = %text, "Upstream error");
        anyhow::bail!("Upstream error {}: {}", status.as_u16(), text);
    }

    if stream {
        let byte_stream = resp.bytes_stream();
        let sse_stream = byte_stream.map(|chunk| {
            let bytes = chunk.unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).to_string();
            Ok::<_, std::convert::Infallible>(Event::default().data(text))
        });
        Ok(Sse::new(sse_stream).into_response())
    } else {
        let text = resp.text().await?;
        Ok(Json(serde_json::from_str::<serde_json::Value>(&text)?).into_response())
    }
}

// ── Handlers ───────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "OK — iziRouter"
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
                Json(serde_json::json!({"error": format!("Requête invalide: {}", e)})),
            )
                .into_response();
        }
    };

    let (tier, reason) = select_tier(&state.config.tiers, &req.messages);

    info!(
        tier = %tier.name,
        model = %tier.model,
        provider = %tier.api_base,
        reason,
        stream = req.stream.unwrap_or(false),
        "→ Routage"
    );

    match forward_request(&state.client, tier, body).await {
        Ok(mut response) => {
            response.headers_mut().insert(
                "X-iziRouter-Tier",
                tier.name.parse().unwrap(),
            );
            response.headers_mut().insert(
                "X-iziRouter-Model",
                tier.model.parse().unwrap(),
            );
            response
                .headers_mut()
                .insert("X-iziRouter-Reason", reason.parse().unwrap());
            response
        }
        Err(e) => {
            tracing::error!("Erreur proxy: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("Erreur proxy: {}", e)})),
            )
                .into_response()
        }
    }
}

struct AppState {
    client: Client,
    config: AppConfig,
}

// ── Main ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,izi_router=debug".into()),
        )
        .init();

    let config_path = std::env::var("TIERS_CONFIG")
        .unwrap_or_else(|_| "tiers.yaml".into());

    let config = AppConfig::load(&config_path)?;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let port = config.port;
    let state = Arc::new(AppState { client, config });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(Arc::clone(&state));

    let addr = format!("0.0.0.0:{}", port);
    info!("🚀 iziRouter démarré sur http://{}", addr);
    info!("   Config: {}", config_path);
    info!("   Tiers chargés:");
    for tier in &state.config.tiers {
        let badge = if tier.default { " 🏠" } else { "" };
        let kw_count = tier.keywords.len();
        info!(
            "     {:<20} → {:30}  [{} mots-clés, poids={}]{}",
            tier.name, tier.model, kw_count, tier.weight, badge
        );
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
