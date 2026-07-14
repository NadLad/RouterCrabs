//! # RouterCrabs 🧭
//!
//! An intelligent proxy that routes LLM requests to the most suitable
//! model based on **two criteria**:
//!
//! 1. **Domain keywords** — e.g. "agriculture" → AgriLLM, "code" → Pro
//! 2. **Complexity heuristics** — short & simple prompt → Flash, long & technical → Pro
//!
//! Each tier is defined in a YAML file. An optional `fallback` section
//! enables complexity-based routing when no domain keywords match.
//!
//! Supports all OpenAI-compatible providers: DeepSeek, OpenAI,
//! Groq, OpenRouter, Anthropic, Mistral, Together AI…
//!
//! ## Usage — Binary
//!
//! ```bash
//! cargo install router-crabs
//! router-crabs  # reads tiers.yaml from the current directory
//! ```
//!
//! ## Usage — Library
//!
//! ```rust,no_run
//! use router_crabs::{TiersConfig, Message, MessageContent, select_tier};
//!
//! # fn main() -> anyhow::Result<()> {
//! let config = TiersConfig::load("tiers.yaml")?;
//! let messages = vec![
//!     Message { role: "user".into(), content: Some(MessageContent::Text(
//!         "Explain microservices architecture to me".into()
//!     )) },
//! ];
//! let (tier, reason) = select_tier(&config, &messages);
//! // If fallback is configured: complexity → tier.model = "deepseek-v4-pro"
//! # Ok(())
//! # }
//! ```
//!
//! ## `tiers.yaml` format
//!
//! ```yaml
//! port: 8001
//!
//! # Domain tiers (optional)
//! tiers:
//!   - model: "agrillm-v2"
//!     api_base: "https://api.agrillm.com/v1"
//!     api_key: "${AGRI_API_KEY}"
//!     keywords: [agriculture, agronomy, soil, plant, harvest]
//!     weight: 20
//!
//! # Complexity-based routing (optional)
//! fallback:
//!   threshold: 3          # complexity threshold
//!   simple:
//!     model: "deepseek-v4-flash"
//!     api_base: "https://api.deepseek.com"
//!     api_key: "${DEEPSEEK_API_KEY}"
//!   complex:
//!     model: "deepseek-v4-pro"
//!     api_base: "https://api.deepseek.com"
//!     api_key: "${DEEPSEEK_API_KEY}"
//! ```

use std::borrow::Cow;
use reqwest::Client;
use serde::Deserialize;
use tokio_stream::StreamExt;
use axum::{
    body::Body,
    response::{IntoResponse, Response},
    Json,
};

// ── YAML Configuration ──────────────────────────────────────────────────

/// Raw tier, deserialized from YAML.
/// Still contains unresolved `${VAR}` placeholders.
#[derive(Debug, Deserialize, Clone)]
pub struct RawTier {
    /// Model identifier (e.g. `"deepseek-v4-pro"`)
    pub model: String,
    /// API base URL (e.g. `"https://api.deepseek.com"`)
    pub api_base: String,
    /// API key (supports `${VAR}` for environment variables)
    pub api_key: String,
    /// Authentication header name (default: `"Bearer"`).
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
    /// List of keywords used to score this tier.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Multiplicative weight for the tier (default: 1).
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Tier used when no keywords match and no complexity fallback is active.
    #[serde(default)]
    pub default: bool,
}

fn default_auth_header() -> String { "Bearer".into() }
fn default_weight() -> u32 { 1 }

/// Raw fallback tier configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct RawFallbackTier {
    pub model: String,
    pub api_base: String,
    pub api_key: String,
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
}

/// Raw complexity-based fallback configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct RawFallbackConfig {
    /// Complexity threshold to switch to the "complex" tier (default: 3)
    #[serde(default = "default_complexity_threshold")]
    pub threshold: u32,
    pub simple: RawFallbackTier,
    pub complex: RawFallbackTier,
}

fn default_complexity_threshold() -> u32 { 3 }

/// Raw configuration as read from the YAML file.
#[derive(Debug, Deserialize)]
pub struct RawConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub tiers: Vec<RawTier>,
    pub fallback: Option<RawFallbackConfig>,
}

fn default_port() -> u16 { 8001 }

// ── Resolved tier (environment variables interpolated) ───────────────────

/// A fully resolved tier — `${VAR}` placeholders have been replaced
/// with their values from the environment.
#[derive(Debug, Clone)]
pub struct Tier {
    /// Tier name (derived from the `model` field)
    pub name: String,
    /// Model identifier
    pub model: String,
    /// API base URL
    pub api_base: String,
    /// API key (resolved)
    pub api_key: String,
    /// Authentication header
    pub auth_header: String,
    /// Keywords for this tier
    pub keywords: Vec<String>,
    /// Multiplicative weight
    pub weight: u32,
    /// Is this the default tier?
    pub default: bool,
}

impl Tier {
    /// Converts a [`RawTier`] into a [`Tier`] by resolving environment
    /// variables in `api_base` and `api_key`.
    pub fn from_raw(raw: RawTier, name: String) -> Self {
        Self {
            name,
            model: raw.model,
            api_base: resolve_env_vars(&raw.api_base),
            api_key: resolve_env_vars(&raw.api_key),
            auth_header: raw.auth_header,
            keywords: raw.keywords,
            weight: raw.weight,
            default: raw.default,
        }
    }
}

/// A resolved fallback tier (used for complexity-based routing).
#[derive(Debug, Clone)]
pub struct FallbackTier {
    pub model: String,
    pub api_base: String,
    pub api_key: String,
    pub auth_header: String,
}

impl FallbackTier {
    fn from_raw(raw: RawFallbackTier) -> Self {
        Self {
            model: raw.model,
            api_base: resolve_env_vars(&raw.api_base),
            api_key: resolve_env_vars(&raw.api_key),
            auth_header: raw.auth_header,
        }
    }
}

/// Complexity-based routing configuration (used when no keywords match).
#[derive(Debug, Clone)]
pub struct FallbackConfig {
    /// Complexity threshold (score >= threshold → complex tier)
    pub threshold: u32,
    /// Tier for simple requests
    pub simple: FallbackTier,
    /// Tier for complex requests
    pub complex: FallbackTier,
}

impl FallbackConfig {
    fn from_raw(raw: RawFallbackConfig) -> Self {
        Self {
            threshold: raw.threshold,
            simple: FallbackTier::from_raw(raw.simple),
            complex: FallbackTier::from_raw(raw.complex),
        }
    }
}

/// Resolves `${NAME}` variables in a string by replacing them
/// with the corresponding environment variable values.
///
/// Undefined variables are replaced with an empty string.
///
/// # Example
///
/// ```rust
/// use router_crabs::resolve_env_vars;
///
/// std::env::set_var("KEY", "value123");
/// let s = resolve_env_vars("https://api.example.com?key=${KEY}");
/// assert_eq!(s, "https://api.example.com?key=value123");
/// ```
pub fn resolve_env_vars(s: &str) -> String {
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

/// Full configuration loaded from a YAML file.
#[derive(Debug)]
pub struct TiersConfig {
    /// Listening port for binary mode
    pub port: u16,
    /// Resolved domain tiers
    pub tiers: Vec<Tier>,
    /// Complexity-based routing configuration
    pub fallback: Option<FallbackConfig>,
}

impl TiersConfig {
    /// Loads and resolves a configuration from a YAML file.
    ///
    /// # Arguments
    /// * `path` — Path to the `tiers.yaml` file.
    ///
    /// # Errors
    /// Returns an error if the file is unreadable, the YAML is invalid,
    /// or if neither a tier with `default: true` nor a `fallback` section is present.
    ///
    /// # Example
    /// ```rust,no_run
    /// use router_crabs::TiersConfig;
    ///
    /// # fn main() -> anyhow::Result<()> {
    /// let config = TiersConfig::load("tiers.yaml")?;
    /// println!("{} tiers loaded", config.tiers.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let yaml = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", path, e))?;

        let raw: RawConfig = serde_yaml::from_str(&yaml)
            .map_err(|e| anyhow::anyhow!("Invalid YAML in {}: {}", path, e))?;

        let has_default = raw.tiers.iter().any(|t| t.default);
        let has_fallback = raw.fallback.is_some();

        if raw.tiers.is_empty() && !has_fallback {
            anyhow::bail!("No tier nor fallback defined in {}", path);
        }
        if !raw.tiers.is_empty() && !has_default && !has_fallback {
            anyhow::bail!(
                "No tier with `default: true` and no `fallback` section in {}",
                path
            );
        }

        let tier_names = raw.tiers.iter().map(|t| t.model.clone()).collect::<Vec<_>>();
        let tiers: Vec<Tier> = raw.tiers
            .into_iter()
            .zip(tier_names)
            .map(|(raw, name)| Tier::from_raw(raw, name))
            .collect();

        let fallback = raw.fallback.map(FallbackConfig::from_raw);

        Ok(Self { port: raw.port, tiers, fallback })
    }
}

// ── OpenAI-compatible types ──────────────────────────────────────────────

/// A chat request in OpenAI format.
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub stream: Option<bool>,
}

/// A message in a conversation.
#[derive(Debug, Deserialize, Clone)]
pub struct Message {
    #[allow(dead_code)]
    pub role: String,
    /// Message content. `None` for tool calls.
    pub content: Option<MessageContent>,
}

/// Message content — plain text or multimodal array.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text
    Text(String),
    /// Multimodal content (text + images)
    MultiPart(Vec<ContentPart>),
}

impl MessageContent {
    /// Extracts the textual content, regardless of variant.
    pub fn as_text(&self) -> String {
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

impl Message {
    /// Returns the text content of the message, or `""` if empty.
    pub fn text(&self) -> String {
        match &self.content {
            Some(c) => c.as_text(),
            None => String::new(),
        }
    }
}

/// A part of multimodal content.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    #[allow(dead_code)]
    ImageUrl { image_url: serde_json::Value },
}

// ── Complexity heuristics ────────────────────────────────────────────────

/// Computes a complexity score (0–12) for a list of messages.
///
/// Heuristics used:
///
/// | Criterion | Score |
/// |-----------|-------|
/// | Prompt > 2000 characters | +3 |
/// | Prompt > 800 characters | +2 |
/// | Prompt > 300 characters | +1 |
/// | ≥ 3 code markers (```, `fn`, `class`, etc.) | +3 |
/// | ≥ 1 code marker | +2 |
/// | ≥ 4 technical keywords | +3 |
/// | ≥ 2 technical keywords | +2 |
/// | ≥ 1 technical keyword | +1 |
/// | Image present | +5 |
/// | Open-ended question (? + interrogative word) | +1 |
///
/// # Example
///
/// ```rust
/// use router_crabs::{Message, MessageContent, score_complexity};
///
/// let messages = vec![
///     Message {
///         role: "user".into(),
///         content: Some(MessageContent::Text(
///             "Explain microservices architecture, compare tradeoffs.".into()
///         )),
///     },
/// ];
/// let score = score_complexity(&messages);
/// assert!(score >= 3); // long prompt + technical → high score
/// ```
pub fn score_complexity(messages: &[Message]) -> u32 {
    let full_text: String = messages.iter().map(|m| m.text()).collect::<Vec<_>>().join(" ");
    let lower = full_text.to_lowercase();
    let len = full_text.len();

    let mut score: u32 = 0;

    // ── 1. Prompt length ─────────────────
    if len > 2000 {
        score += 3;
    } else if len > 800 {
        score += 2;
    } else if len > 300 {
        score += 1;
    }

    // ── 2. Code presence ──────────────────
    let code_markers = [
        "```", "fn ", "def ", "class ", "import ", "package ",
        "#include", "pub fn", "impl ", "struct ", "enum ",
        "trait ", "async fn", "SELECT ", "INSERT ",
    ];
    let code_count = code_markers.iter().filter(|m| lower.contains(*m)).count();
    if code_count >= 3 {
        score += 3;
    } else if code_count >= 1 {
        score += 2;
    }

    // ── 3. Technical keywords ─────────────
    let tech_keywords = [
        // French
        "explique", "analyse", "compare", "pourquoi", "comment",
        "architecture", "design pattern", "complexité",
        "optimise", "optimisation", "algorithme", "sécurité",
        "debug", "thread", "concurrent", "parallèle",
        "mémoire", "cache", "distribué", "microservice",
        "kubernetes", "benchmark", "tradeoff", "trade-off",
        "meilleure pratique", "différence entre",
        // English
        "explain", "analyze", "why", "architecture", "algorithm",
        "optimize", "debug", "concurrent", "security", "distributed",
    ];
    let tech_count = tech_keywords.iter().filter(|kw| lower.contains(*kw)).count();
    if tech_count >= 4 {
        score += 3;
    } else if tech_count >= 2 {
        score += 2;
    } else if tech_count >= 1 {
        score += 1;
    }

    // ── 4. Images ──────────────────────────
    let has_image = messages.iter().any(|m| {
        if let Some(MessageContent::MultiPart(ref parts)) = m.content {
            parts.iter().any(|p| matches!(p, ContentPart::ImageUrl { .. }))
        } else {
            false
        }
    });
    if has_image {
        score += 5;
    }

    // ── 5. Open-ended question ─────────────
    let question_words = [
        "pourquoi", "comment", "qu'est-ce que", "quelle est",
        "peux-tu", "how", "why", "what is", "can you",
    ];
    let has_question = full_text.contains('?')
        && question_words.iter().any(|w| lower.contains(w));
    if has_question {
        score += 1;
    }

    score
}

// ── Tier selection (hybrid: keywords + complexity) ──────────────────────

/// Selects the most relevant tier for a list of messages.
///
/// **Two-phase operation:**
///
/// 1. **Keyword phase** — For each tier, counts how many of its keywords
///    appear in the prompt. Score = match_count × weight.
///    The highest score wins. If keywords match, this phase
///    wins (explicit domains take priority over complexity).
///
/// 2. **Complexity phase** — If no keywords match and a `fallback`
///    section is configured, the prompt's complexity score determines
///    the tier: complexity ≥ threshold → complex tier, otherwise → simple tier.
///
/// 3. **Default fallback** — Without a `fallback` section, the tier marked
///    `default: true` is used (backward compatibility).
///
/// # Arguments
/// * `config` — Configuration loaded via [`TiersConfig::load`]
/// * `messages` — Conversation messages
///
/// # Returns
/// `(selected_tier, reason_for_choice)`
///
/// # Example
///
/// ```rust,no_run
/// use router_crabs::{TiersConfig, Message, MessageContent, select_tier};
///
/// # fn main() -> anyhow::Result<()> {
/// let config = TiersConfig::load("tiers.yaml")?;
/// let messages = vec![
///     Message {
///         role: "user".into(),
///         content: Some(MessageContent::Text(
///             "Hello!".into()
///         )),
///     },
/// ];
/// let (tier, reason) = select_tier(&config, &messages);
/// // "Hello" → complexity score = 0 → simple tier (flash)
/// println!("→ {} (reason: {})", tier.model, reason);
/// # Ok(())
/// # }
/// ```
pub fn select_tier<'a>(
    config: &'a TiersConfig,
    messages: &[Message],
) -> (Cow<'a, Tier>, String) {
    let full_text: String = messages.iter().map(|m| m.text()).collect::<Vec<_>>().join(" ");
    let lower = full_text.to_lowercase();

    // ── Phase 1: domain keywords ──────────
    let mut best: Option<&Tier> = None;
    let mut best_score: u32 = 0;
    let mut best_matches: Vec<String> = vec![];

    for tier in &config.tiers {
        if tier.keywords.is_empty() {
            continue;
        }

        let matched: Vec<&String> = tier
            .keywords
            .iter()
            .filter(|kw| lower.contains(&kw.to_lowercase()))
            .collect();

        let match_count = matched.len() as u32;
        if match_count == 0 {
            continue;
        }

        let score = match_count * tier.weight;

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

    if let Some(tier) = best {
        let reason = format!(
            "domain: {} (matches: [{}], score: {})",
            tier.name,
            best_matches.join(", "),
            best_score,
        );
        return (Cow::Borrowed(tier), reason);
    }

    // ── Phase 2: complexity (fallback) ────
    if let Some(ref fb) = config.fallback {
        let complexity = score_complexity(messages);
        if complexity >= fb.threshold {
            let tier = Tier {
                name: "complex-fallback".into(),
                model: fb.complex.model.clone(),
                api_base: fb.complex.api_base.clone(),
                api_key: fb.complex.api_key.clone(),
                auth_header: fb.complex.auth_header.clone(),
                keywords: vec![],
                weight: 0,
                default: false,
            };
            return (
                Cow::Owned(tier),
                format!(
                    "complexity: high (score: {}, threshold: {})",
                    complexity, fb.threshold
                ),
            );
        } else {
            let tier = Tier {
                name: "simple-fallback".into(),
                model: fb.simple.model.clone(),
                api_base: fb.simple.api_base.clone(),
                api_key: fb.simple.api_key.clone(),
                auth_header: fb.simple.auth_header.clone(),
                keywords: vec![],
                weight: 0,
                default: false,
            };
            return (
                Cow::Owned(tier),
                format!(
                    "complexity: low (score: {}, threshold: {})",
                    complexity, fb.threshold
                ),
            );
        }
    }

    // ── Phase 3: default fallback ──────────
    let default = config
        .tiers
        .iter()
        .find(|t| t.default)
        .expect("default tier required (no keywords, no fallback, no default)");
    (
        Cow::Borrowed(default),
        "default (no keywords matched, no fallback)".into(),
    )
}

// ── Proxy to the upstream provider ─────────────────────────────────────

/// Forwards a request to the selected upstream provider.
///
/// Replaces the `model` field in the JSON body with the tier's model,
/// adds the appropriate authentication header, and forwards
/// the response (normal or streamed) to the client.
pub async fn forward_request(
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
        anyhow::bail!("Upstream error {}: {}", status.as_u16(), text);
    }

    if stream {
        let byte_stream = resp.bytes_stream();
        let body = Body::from_stream(
            byte_stream.map(|result| {
                result.map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                })
            })
        );
        let response = Response::builder()
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(body)
            .unwrap();
        Ok(response)
    } else {
        let text = resp.text().await?;
        Ok(Json(serde_json::from_str::<serde_json::Value>(&text)?).into_response())
    }
}
