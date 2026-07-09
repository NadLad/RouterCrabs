//! # RouterCrabs 🧭
//!
//! Un proxy intelligent qui route les requêtes LLM vers le modèle
//! le plus adapté selon le domaine détecté (agriculture, recherche, code…).
//!
//! Chaque tier est défini dans un fichier YAML avec des mots-clés.
//! Le routeur sélectionne le tier ayant le plus de mots-clés matchés,
//! pondéré par son poids. Si aucun mot-clé ne matche, il utilise
//! le tier marqué `default: true`.
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
//!         "Comment améliorer le rendement de mon blé ?".into()
//!     )) },
//! ];
//! let (tier, reason) = select_tier(&config.tiers, &messages);
//! // tier.model → "agrillm-v2" (si le tier agri matche)
//! # Ok(())
//! # }
//! ```
//!
//! ## Format `tiers.yaml`
//!
//! ```yaml
//! port: 8001
//! tiers:
//!   - model: "deepseek-v4-pro"
//!     api_base: "https://api.deepseek.com"
//!     api_key: "${DEEPSEEK_API_KEY}"    # interpolé depuis l'environnement
//!     keywords: [recherche, scientifique, papier, étude]
//!     weight: 10
//!   - model: "deepseek-v4-flash"
//!     api_base: "https://api.deepseek.com"
//!     api_key: "${DEEPSEEK_API_KEY}"
//!     keywords: []                      # vide → jamais scoré
//!     default: true                     # fallback
//! ```

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
    /// Mettre `"x-api-key"` pour Anthropic, `"Authorization"` pour OpenAI, etc.
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
    /// Liste des mots-clés pour scorer ce tier.
    /// Si vide, le tier n'est jamais scoré (utilisable comme fallback uniquement).
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Poids multiplicatif du tier (défaut: 1).
    /// Score final = nombre_de_mots_clés_matchés × weight.
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Si `true`, ce tier est utilisé quand aucun mot-clé d'aucun tier ne matche.
    /// Un seul tier doit avoir cette option.
    #[serde(default)]
    pub default: bool,
}

fn default_auth_header() -> String { "Bearer".into() }
fn default_weight() -> u32 { 1 }

/// Configuration brute telle que lue dans le fichier YAML.
#[derive(Debug, Deserialize)]
pub struct RawConfig {
    /// Port d'écoute (défaut: 8001)
    #[serde(default = "default_port")]
    pub port: u16,
    /// Liste des tiers
    pub tiers: Vec<RawTier>,
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
    /// Tiers résolus, prêts à être utilisés avec [`select_tier`].
    pub tiers: Vec<Tier>,
}

impl TiersConfig {
    /// Charge et résout une configuration depuis un fichier YAML.
    ///
    /// # Arguments
    /// * `path` — Chemin vers le fichier `tiers.yaml`.
    ///
    /// # Erreurs
    /// Retourne une erreur si le fichier est illisible, le YAML invalide,
    /// ou si aucun tier n'a `default: true`.
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

// ── Types OpenAI-compatibles ─────────────────────────────────────────────

/// Requête chat au format OpenAI.
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// Historique des messages
    pub messages: Vec<Message>,
    /// Mode streaming
    pub stream: Option<bool>,
}

/// Un message dans une conversation.
#[derive(Debug, Deserialize, Clone)]
pub struct Message {
    /// Rôle : `"user"`, `"assistant"`, `"system"`, etc.
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
    /// Partie texte
    #[serde(rename = "text")]
    Text {
        /// Contenu textuel
        text: String,
    },
    /// Partie image
    #[serde(rename = "image_url")]
    #[allow(dead_code)]
    ImageUrl {
        /// URL ou base64 de l'image
        image_url: serde_json::Value,
    },
}

// ── Sélection du tier ───────────────────────────────────────────────────

/// Sélectionne le tier le plus pertinent pour une liste de messages.
///
/// Pour chaque tier, compte combien de ses mots-clés apparaissent dans
/// le prompt complet (concaténation de tous les messages, insensible
/// à la casse, substring match).
///
/// **Score** = nombre_de_matchs × poids_du_tier.
///
/// En cas d'égalité : le poids le plus élevé l'emporte, puis `default: true`.
/// Si aucun mot-clé ne matche aucun tier, retourne le tier `default: true`.
///
/// # Arguments
/// * `tiers` — Liste des tiers (depuis [`TiersConfig::load`])
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
///             "Comment améliorer mon rendement agricole ?".into()
///         )),
///     },
/// ];
/// let (tier, reason) = select_tier(&config.tiers, &messages);
/// println!("→ {} (raison: {})", tier.model, reason);
/// # Ok(())
/// # }
/// ```
pub fn select_tier<'a>(tiers: &'a [Tier], messages: &[Message]) -> (&'a Tier, String) {
    let full_text: String = messages
        .iter()
        .map(|m| m.text())
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

// ── Proxy vers le provider upstream ─────────────────────────────────────

/// Transmet une requête au provider upstream sélectionné.
///
/// Remplace le champ `model` dans le body JSON par celui du tier,
/// ajoute le header d'authentification approprié, et transmet
/// la réponse (normale ou streamée) au client.
///
/// # Arguments
/// * `client` — Client HTTP réutilisable
/// * `tier` — Tier sélectionné par [`select_tier`]
/// * `body` — Corps JSON de la requête OpenAI
///
/// # Erreurs
/// Retourne une erreur si le provider upstream répond avec un
/// code d'erreur (4xx/5xx) ou si la requête échoue.
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
