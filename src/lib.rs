//! # RouterCrabs 🧭
//!
//! Un proxy intelligent qui route les requêtes LLM vers le modèle
//! le plus adapté selon **deux critères** :
//!
//! 1. **Mots-clés de domaine** — ex: « agriculture » → AgriLLM, « code » → Pro
//! 2. **Heuristiques de complexité** — prompt court et simple → Flash, long et technique → Pro
//!
//! Chaque tier est défini dans un fichier YAML. Une section `fallback` optionnelle
//! active le routage par complexité quand aucun mot-clé de domaine ne matche.
//!
//! Supporte tous les providers OpenAI-compatibles : DeepSeek, OpenAI,
//! Groq, OpenRouter, Anthropic, Mistral, Together AI…
//!
//! ## Usage — Binaire
//!
//! ```bash
//! cargo install router-crabs
//! router-crabs  # lit tiers.yaml dans le répertoire courant
//! ```
//!
//! ## Usage — Librairie
//!
//! ```rust,no_run
//! use router_crabs::{TiersConfig, Message, MessageContent, select_tier};
//!
//! # fn main() -> anyhow::Result<()> {
//! let config = TiersConfig::load("tiers.yaml")?;
//! let messages = vec![
//!     Message { role: "user".into(), content: Some(MessageContent::Text(
//!         "Explique-moi l'architecture des microservices".into()
//!     )) },
//! ];
//! let (tier, reason) = select_tier(&config, &messages);
//! // Si fallback configuré : complexité → tier.model = "deepseek-v4-pro"
//! # Ok(())
//! # }
//! ```
//!
//! ## Format `tiers.yaml`
//!
//! ```yaml
//! port: 8001
//!
//! # Tiers par domaine (optionnels)
//! tiers:
//!   - model: "agrillm-v2"
//!     api_base: "https://api.agrillm.com/v1"
//!     api_key: "${AGRI_API_KEY}"
//!     keywords: [agriculture, agronomie, sol, plante, récolte]
//!     weight: 20
//!
//! # Routage par complexité (optionnel)
//! fallback:
//!   threshold: 3          # seuil de complexité
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

// ── Configuration YAML ──────────────────────────────────────────────────

/// Tier brut, désérialisé depuis le YAML.
/// Contient encore les variables `${VAR}` non résolues.
#[derive(Debug, Deserialize, Clone)]
pub struct RawTier {
    /// Identifiant du modèle (ex: `"deepseek-v4-pro"`)
    pub model: String,
    /// URL de base de l'API (ex: `"https://api.deepseek.com"`)
    pub api_base: String,
    /// Clé API (supporte `${VAR}` pour les variables d'environnement)
    pub api_key: String,
    /// Nom du header d'authentification (défaut: `"Bearer"`).
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
    /// Liste des mots-clés pour scorer ce tier.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Poids multiplicatif du tier (défaut: 1).
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Tier utilisé quand aucun mot-clé ni complexité ne matche.
    #[serde(default)]
    pub default: bool,
}

fn default_auth_header() -> String { "Bearer".into() }
fn default_weight() -> u32 { 1 }

/// Configuration brute d'un tier de fallback.
#[derive(Debug, Deserialize, Clone)]
pub struct RawFallbackTier {
    pub model: String,
    pub api_base: String,
    pub api_key: String,
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
}

/// Configuration brute du fallback par complexité.
#[derive(Debug, Deserialize, Clone)]
pub struct RawFallbackConfig {
    /// Seuil de complexité pour basculer vers le tier "complex" (défaut: 3)
    #[serde(default = "default_complexity_threshold")]
    pub threshold: u32,
    pub simple: RawFallbackTier,
    pub complex: RawFallbackTier,
}

fn default_complexity_threshold() -> u32 { 3 }

/// Configuration brute telle que lue dans le fichier YAML.
#[derive(Debug, Deserialize)]
pub struct RawConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub tiers: Vec<RawTier>,
    pub fallback: Option<RawFallbackConfig>,
}

fn default_port() -> u16 { 8001 }

// ── Tier résolu (variables d'environnement interpolées) ─────────────────

/// Un tier entièrement résolu — les `${VAR}` ont été remplacés
/// par leurs valeurs depuis l'environnement.
#[derive(Debug, Clone)]
pub struct Tier {
    /// Nom du tier (dérivé du champ `model`)
    pub name: String,
    /// Identifiant du modèle
    pub model: String,
    /// URL de base de l'API
    pub api_base: String,
    /// Clé API (résolue)
    pub api_key: String,
    /// Header d'authentification
    pub auth_header: String,
    /// Mots-clés de ce tier
    pub keywords: Vec<String>,
    /// Poids multiplicatif
    pub weight: u32,
    /// Tier par défaut ?
    pub default: bool,
}

impl Tier {
    /// Convertit un [`RawTier`] en [`Tier`] en résolvant les variables
    /// d'environnement dans `api_base` et `api_key`.
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

/// Un tier de fallback résolu (utilisé pour le routage par complexité).
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

/// Configuration du routage par complexité (quand aucun mot-clé ne matche).
#[derive(Debug, Clone)]
pub struct FallbackConfig {
    /// Seuil de complexité (score >= seuil → tier complex)
    pub threshold: u32,
    /// Tier pour les requêtes simples
    pub simple: FallbackTier,
    /// Tier pour les requêtes complexes
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

/// Résout les variables `${NOM}` dans une chaîne en les remplaçant
/// par les variables d'environnement correspondantes.
///
/// Les variables non définies sont remplacées par une chaîne vide.
///
/// # Exemple
///
/// ```rust
/// use router_crabs::resolve_env_vars;
///
/// std::env::set_var("CLEF", "valeur123");
/// let s = resolve_env_vars("https://api.example.com?key=${CLEF}");
/// assert_eq!(s, "https://api.example.com?key=valeur123");
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

/// Configuration complète chargée depuis un fichier YAML.
#[derive(Debug)]
pub struct TiersConfig {
    /// Port d'écoute pour le mode binaire
    pub port: u16,
    /// Tiers de domaine résolus
    pub tiers: Vec<Tier>,
    /// Configuration du routage par complexité
    pub fallback: Option<FallbackConfig>,
}

impl TiersConfig {
    /// Charge et résout une configuration depuis un fichier YAML.
    ///
    /// # Arguments
    /// * `path` — Chemin vers le fichier `tiers.yaml`.
    ///
    /// # Erreurs
    /// Retourne une erreur si le fichier est illisible, le YAML invalide,
    /// ou si ni tier avec `default: true` ni section `fallback` n'est présent.
    ///
    /// # Exemple
    /// ```rust,no_run
    /// use router_crabs::TiersConfig;
    ///
    /// # fn main() -> anyhow::Result<()> {
    /// let config = TiersConfig::load("tiers.yaml")?;
    /// println!("{} tiers chargés", config.tiers.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let yaml = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Impossible de lire {}: {}", path, e))?;

        let raw: RawConfig = serde_yaml::from_str(&yaml)
            .map_err(|e| anyhow::anyhow!("YAML invalide dans {}: {}", path, e))?;

        let has_default = raw.tiers.iter().any(|t| t.default);
        let has_fallback = raw.fallback.is_some();

        if raw.tiers.is_empty() && !has_fallback {
            anyhow::bail!("Aucun tier ni fallback défini dans {}", path);
        }
        if !raw.tiers.is_empty() && !has_default && !has_fallback {
            anyhow::bail!(
                "Aucun tier avec `default: true` et pas de section `fallback` dans {}",
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

// ── Types OpenAI-compatibles ─────────────────────────────────────────────

/// Requête chat au format OpenAI.
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub stream: Option<bool>,
}

/// Un message dans une conversation.
#[derive(Debug, Deserialize, Clone)]
pub struct Message {
    #[allow(dead_code)]
    pub role: String,
    /// Contenu du message. `None` pour les tool calls.
    pub content: Option<MessageContent>,
}

/// Contenu d'un message — texte simple ou tableau multimodal.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum MessageContent {
    /// Texte simple
    Text(String),
    /// Contenu multimodal (texte + images)
    MultiPart(Vec<ContentPart>),
}

impl MessageContent {
    /// Extrait le contenu textuel, quelle que soit la variante.
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
    /// Retourne le contenu textuel du message, ou `""` s'il est vide.
    pub fn text(&self) -> String {
        match &self.content {
            Some(c) => c.as_text(),
            None => String::new(),
        }
    }
}

/// Partie d'un contenu multimodal.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    #[allow(dead_code)]
    ImageUrl { image_url: serde_json::Value },
}

// ── Heuristiques de complexité ──────────────────────────────────────────

/// Calcule un score de complexité (0–12) pour une liste de messages.
///
/// Heuristiques utilisées :
///
/// | Critère | Score |
/// |---------|-------|
/// | Prompt > 2000 caractères | +3 |
/// | Prompt > 800 caractères | +2 |
/// | Prompt > 300 caractères | +1 |
/// | ≥ 3 marqueurs de code (```, `fn`, `class`, etc.) | +3 |
/// | ≥ 1 marqueur de code | +2 |
/// | ≥ 4 mots-clés techniques | +3 |
/// | ≥ 2 mots-clés techniques | +2 |
/// | ≥ 1 mot-clé technique | +1 |
/// | Image présente | +5 |
/// | Question ouverte (? + mot interrogatif) | +1 |
///
/// # Exemple
///
/// ```rust
/// use router_crabs::{Message, MessageContent, score_complexity};
///
/// let messages = vec![
///     Message {
///         role: "user".into(),
///         content: Some(MessageContent::Text(
///             "Explique l'architecture des microservices, compare les tradeoffs.".into()
///         )),
///     },
/// ];
/// let score = score_complexity(&messages);
/// assert!(score >= 3); // prompt long + technique → score élevé
/// ```
pub fn score_complexity(messages: &[Message]) -> u32 {
    let full_text: String = messages.iter().map(|m| m.text()).collect::<Vec<_>>().join(" ");
    let lower = full_text.to_lowercase();
    let len = full_text.len();

    let mut score: u32 = 0;

    // ── 1. Longueur du prompt ─────────────
    if len > 2000 {
        score += 3;
    } else if len > 800 {
        score += 2;
    } else if len > 300 {
        score += 1;
    }

    // ── 2. Présence de code ───────────────
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

    // ── 3. Mots-clés techniques ───────────
    let tech_keywords = [
        // Français
        "explique", "analyse", "compare", "pourquoi", "comment",
        "architecture", "design pattern", "complexité",
        "optimise", "optimisation", "algorithme", "sécurité",
        "debug", "thread", "concurrent", "parallèle",
        "mémoire", "cache", "distribué", "microservice",
        "kubernetes", "benchmark", "tradeoff", "trade-off",
        "meilleure pratique", "différence entre",
        // Anglais
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

    // ── 5. Question ouverte ────────────────
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

// ── Sélection du tier (hybride : mots-clés + complexité) ────────────────

/// Sélectionne le tier le plus pertinent pour une liste de messages.
///
/// **Fonctionnement en deux phases :**
///
/// 1. **Phase mots-clés** — Pour chaque tier, compte combien de ses mots-clés
///    apparaissent dans le prompt. Score = match_count × weight.
///    Le meilleur score l'emporte. Si des mots-clés matchent, cette phase
///    gagne (les domaines explicites priment sur la complexité).
///
/// 2. **Phase complexité** — Si aucun mot-clé ne matche et qu'une section
///    `fallback` est configurée, le score de complexité du prompt détermine
///    le tier : complexité ≥ seuil → tier complex, sinon → tier simple.
///
/// 3. **Fallback par défaut** — Sans section `fallback`, le tier marqué
///    `default: true` est utilisé (compatibilité ascendante).
///
/// # Arguments
/// * `config` — Configuration chargée via [`TiersConfig::load`]
/// * `messages` — Messages de la conversation
///
/// # Retourne
/// `(tier_sélectionné, raison_du_choix)`
///
/// # Exemple
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
///             "Bonjour !".into()
///         )),
///     },
/// ];
/// let (tier, reason) = select_tier(&config, &messages);
/// // "Bonjour" → score complexité = 0 → tier simple (flash)
/// println!("→ {} (raison: {})", tier.model, reason);
/// # Ok(())
/// # }
/// ```
pub fn select_tier<'a>(
    config: &'a TiersConfig,
    messages: &[Message],
) -> (Cow<'a, Tier>, String) {
    let full_text: String = messages.iter().map(|m| m.text()).collect::<Vec<_>>().join(" ");
    let lower = full_text.to_lowercase();

    // ── Phase 1 : mots-clés par domaine ────
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
            "domaine: {} (matches: [{}], score: {})",
            tier.name,
            best_matches.join(", "),
            best_score,
        );
        return (Cow::Borrowed(tier), reason);
    }

    // ── Phase 2 : complexité (fallback) ────
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
                    "complexité: élevée (score: {}, seuil: {})",
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
                    "complexité: faible (score: {}, seuil: {})",
                    complexity, fb.threshold
                ),
            );
        }
    }

    // ── Phase 3 : fallback par défaut ──────
    let default = config
        .tiers
        .iter()
        .find(|t| t.default)
        .expect("default tier requis (ni mots-clés, ni fallback, ni default)");
    (
        Cow::Borrowed(default),
        "default (aucun mot-clé matché, pas de fallback)".into(),
    )
}

// ── Proxy vers le provider upstream ─────────────────────────────────────

/// Transmet une requête au provider upstream sélectionné.
///
/// Remplace le champ `model` dans le body JSON par celui du tier,
/// ajoute le header d'authentification approprié, et transmet
/// la réponse (normale ou streamée) au client.
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
