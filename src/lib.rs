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
use std::collections::{HashMap, HashSet};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use axum::{
    body::Body,
    response::{IntoResponse, Response},
    Json,
};

// ── Complexity Scoring Keywords ────────────────────────────────────────

/// Metadata attached to a keyword for lifecycle management (decay/removal).
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct KeywordMeta {
    /// Cumulative match count (updated by the analyzer).
    #[serde(default)]
    pub hits: u64,
    /// ISO 8601 timestamp of the last match (updated by the analyzer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    /// ISO 8601 timestamp when this keyword was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}

/// A single resolved keyword: term + signed weight + lifecycle metadata.
///
/// `weight` sign encodes direction:
///   - **positive** → pushes toward Pro (complex)
///   - **negative** → pushes toward Flash (simple)
///   - **zero**     → neutral (e.g. question words)
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct WeightedKeyword {
    /// The keyword term (matched case-insensitively).
    pub term: String,
    /// Signed weight, typically in `[-5, +5]`.
    #[serde(default)]
    pub weight: i32,
    /// Lifecycle metadata (hits / last_seen / added_at).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<KeywordMeta>,
}

impl WeightedKeyword {
    pub fn new(term: impl Into<String>, weight: i32) -> Self {
        Self {
            term: term.into(),
            weight,
            meta: None,
        }
    }
}

/// A keyword list entry, deserialized from YAML.
///
/// Supports two forms:
///   - **string** form: `- "explique"` → resolved with the group's default weight
///   - **object** form: `- { term: "design pattern", weight: 4, hits: 12 }`
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawKeywordEntry {
    String(String),
    Object {
        term: String,
        #[serde(default)]
        weight: Option<i32>,
        #[serde(default)]
        hits: u64,
        #[serde(default)]
        last_seen: Option<String>,
        #[serde(default)]
        added_at: Option<String>,
    },
}

impl RawKeywordEntry {
    /// Resolves this entry into a [`WeightedKeyword`], applying `default_weight`
    /// when the entry is a bare string *or* an object that omitted `weight`.
    pub fn resolve(self, default_weight: i32) -> WeightedKeyword {
        match self {
            RawKeywordEntry::String(term) => WeightedKeyword::new(term, default_weight),
            RawKeywordEntry::Object {
                term,
                weight,
                hits,
                last_seen,
                added_at,
            } => WeightedKeyword {
                term,
                weight: weight.unwrap_or(default_weight),
                meta: Some(KeywordMeta {
                    hits,
                    last_seen,
                    added_at,
                }),
            },
        }
    }
}

/// Raw scoring keywords deserialized from `keywords.yaml`.
///
/// Supports two formats:
///   - **v0.3+** (language-based): `languages: { french: { technical_keywords: [...] }, ... }`
///   - **v0.2.x** (flat): `technical_keywords: [...]`, `question_words: [...]`
#[derive(Debug, Deserialize)]
struct RawScoringKeywords {
    code_markers: Option<Vec<String>>,
    // ── v0.2.x flat format (backward compat) ──
    #[serde(default)]
    technical_keywords: Option<Vec<RawKeywordEntry>>,
    #[serde(default)]
    question_words: Option<Vec<RawKeywordEntry>>,
    #[serde(default)]
    simple_keywords: Option<Vec<RawKeywordEntry>>,
    // ── v0.3+ language-based format ──
    #[serde(default)]
    languages: Option<HashMap<String, RawLanguageKeywords>>,
}

/// A single language block inside `languages:`.
#[derive(Debug, Deserialize)]
struct RawLanguageKeywords {
    #[serde(default)]
    technical_keywords: Option<Vec<RawKeywordEntry>>,
    #[serde(default)]
    question_words: Option<Vec<RawKeywordEntry>>,
    #[serde(default)]
    simple_keywords: Option<Vec<RawKeywordEntry>>,
}

/// Complexity scoring keywords loaded from a YAML file.
/// Falls back to built-in defaults when the file is missing or a section is empty.
#[derive(Debug, Clone)]
pub struct ScoringKeywords {
    /// Code markers (e.g. ```, fn , class , SELECT )
    pub code_markers: Vec<String>,
    /// Technical vocabulary indicating complexity (positive weights → Pro)
    pub technical_keywords: Vec<WeightedKeyword>,
    /// Question words used to detect open-ended prompts (neutral, weight 0)
    pub question_words: Vec<WeightedKeyword>,
    /// Simple vocabulary that *forces* toward Flash (negative weights → Flash)
    pub simple_keywords: Vec<WeightedKeyword>,
}

impl ScoringKeywords {
    /// Returns the terms (as strings) for a given keyword group, for
    /// journaling / introspection. Group is one of `"technical"`,
    /// `"question"`, or `"simple"`.
    pub fn terms(&self, group: &str) -> Vec<&str> {
        let list = match group {
            "technical" => &self.technical_keywords,
            "question" => &self.question_words,
            "simple" => &self.simple_keywords,
            _ => return vec![],
        };
        list.iter().map(|k| k.term.as_str()).collect()
    }
}

fn default_code_markers() -> Vec<String> {
    vec![
        "```".into(), "fn ".into(), "pub fn".into(), "async fn".into(),
        "def ".into(), "class ".into(), "import ".into(), "package ".into(),
        "#include".into(), "impl ".into(), "struct ".into(), "enum ".into(),
        "trait ".into(), "let mut".into(), "const ".into(), "var ".into(),
        "function".into(), "export".into(), "require".into(),
        "SELECT ".into(), "INSERT ".into(), "UPDATE ".into(), "DELETE ".into(),
    ]
}

fn default_technical_keywords() -> Vec<String> {
    vec![
        // ── French ──
        "explique".into(), "analyse".into(), "compare".into(),
        "architecture".into(), "design pattern".into(),
        "complexité".into(), "optimise".into(), "optimisation".into(),
        "algorithme".into(), "sécurité".into(), "debug".into(), "thread".into(),
        "concurrent".into(), "parallèle".into(), "mémoire".into(), "cache".into(),
        "distribué".into(), "microservice".into(), "kubernetes".into(),
        "benchmark".into(), "tradeoff".into(), "trade-off".into(),
        "meilleure pratique".into(), "différence entre".into(),
        "implémente".into(), "configure".into(), "déploie".into(),
        "compile".into(), "refactorise".into(), "abstrait".into(),
        "hérite".into(), "polymorphisme".into(), "encapsule".into(),
        "middleware".into(), "endpoint".into(), "authentification".into(),
        "autorisation".into(), "chiffre".into(), "déchiffre".into(),
        "certificat".into(), "protocole".into(), "latence".into(),
        "scalabilité".into(), "résilience".into(), "transaction".into(),
        "index".into(), "requête".into(), "schéma".into(), "normalise".into(),
        "migre".into(), "test unitaire".into(), "mock".into(), "stub".into(),
        "intégration".into(), "pipeline".into(), "conteneur".into(),
        "orchestre".into(), "monitor".into(), "alerte".into(),
        "sauvegarde".into(), "restaure".into(), "framework".into(),
        // ── English ──
        "explain".into(), "analyze".into(), "compare".into(),
        "architecture".into(), "design pattern".into(),
        "complexity".into(), "optimize".into(), "optimization".into(),
        "algorithm".into(), "security".into(), "debug".into(), "thread".into(),
        "concurrent".into(), "parallel".into(), "memory".into(), "cache".into(),
        "distributed".into(), "microservice".into(), "kubernetes".into(),
        "benchmark".into(), "tradeoff".into(), "trade-off".into(),
        "best practice".into(), "difference between".into(),
        "implement".into(), "configure".into(), "deploy".into(), "compile".into(),
        "refactor".into(), "abstract".into(), "inherit".into(),
        "polymorphism".into(), "encapsulate".into(), "middleware".into(),
        "endpoint".into(), "authentication".into(), "authorization".into(),
        "encrypt".into(), "decrypt".into(), "certificate".into(),
        "protocol".into(), "latency".into(), "scalability".into(),
        "resilience".into(), "transaction".into(), "index".into(),
        "query".into(), "schema".into(), "normalize".into(), "migrate".into(),
        "unit test".into(), "mock".into(), "stub".into(), "integration".into(),
        "pipeline".into(), "container".into(), "orchestrate".into(),
        "monitor".into(), "alert".into(), "backup".into(), "restore".into(),
        "framework".into(),
        // ── Arabic ──
        "اشرح".into(), "حلل".into(), "قارن".into(),
        "معمارية".into(), "نمط تصميم".into(), "تعقيد".into(),
        "حسّن".into(), "تحسين".into(), "خوارزمية".into(), "أمان".into(),
        "أمن".into(), "تصحيح".into(), "خيط".into(), "تزامن".into(),
        "متزامن".into(), "متوازي".into(), "ذاكرة".into(), "تخزين مؤقت".into(),
        "موزع".into(), "توزيع".into(), "خدمة مصغرة".into(), "كوبرنتيس".into(),
        "مقارنة".into(), "أفضل ممارسة".into(), "فرق بين".into(),
        "نفذ".into(), "تنفيذ".into(), "إعداد".into(), "انشر".into(),
        "نشر".into(), "ترجم".into(), "ترجمة".into(), "إعادة هيكلة".into(),
        "تجريد".into(), "وراثة".into(), "تعدد أشكال".into(), "تغليف".into(),
        "وسيط".into(), "نقطة نهاية".into(), "مصادقة".into(), "تفويض".into(),
        "تشفير".into(), "فك تشفير".into(), "شهادة".into(), "بروتوكول".into(),
        "كمون".into(), "قابلية توسع".into(), "مرونة".into(), "معاملة".into(),
        "فهرس".into(), "استعلام".into(), "مخطط".into(), "هجرة".into(),
        "قاعدة بيانات".into(), "اختبار وحدة".into(), "خط أنابيب".into(),
        "حاوية".into(), "راقب".into(), "مراقبة".into(), "سجل".into(),
        "تنبيه".into(), "نسخ احتياطي".into(), "استعادة".into(), "إطار عمل".into(),
    ]
}

fn default_question_words() -> Vec<String> {
    vec![
        // ── French ──
        "pourquoi".into(), "comment".into(), "qu'est-ce que".into(),
        "quelle est".into(), "peux-tu".into(), "quel est".into(),
        "que".into(), "qui".into(), "où".into(), "quand".into(), "lequel".into(),
        // ── English ──
        "how".into(), "why".into(), "what is".into(), "can you".into(),
        "what".into(), "who".into(), "where".into(), "when".into(), "which".into(),
        // ── Arabic ──
        "لماذا".into(), "كيف".into(), "ما هو".into(), "هل يمكنك".into(),
        "ما".into(), "من".into(), "أين".into(), "متى".into(), "أي".into(),
    ]
}

/// Light / simple vocabulary that pushes routing toward Flash (negative weights).
/// These are the **counterweight** — without them the router drifts toward
/// "everything → Pro" as technical keywords accumulate.
fn default_simple_keywords() -> Vec<String> {
    vec![
        // ── French ──
        "salut".into(), "bonjour".into(), "bonsoir".into(), "merci".into(),
        "coucou".into(), "ça va".into(), "au revoir".into(), "s'il te plaît".into(),
        // ── English ──
        "hello".into(), "hi".into(), "hey".into(), "thanks".into(),
        "thank you".into(), "good morning".into(), "good night".into(), "please".into(),
        // ── Arabic ──
        "مرحبا".into(), "أهلا".into(), "شكرا".into(), "صباح الخير".into(),
        "مساء الخير".into(), "مع السلامة".into(), "من فضلك".into(),
    ]
}

impl Default for ScoringKeywords {
    fn default() -> Self {
        Self {
            code_markers: default_code_markers(),
            technical_keywords: default_technical_keywords()
                .into_iter()
                .map(|t| WeightedKeyword::new(t, 1))
                .collect(),
            question_words: default_question_words()
                .into_iter()
                .map(|t| WeightedKeyword::new(t, 0))
                .collect(),
            simple_keywords: default_simple_keywords()
                .into_iter()
                .map(|t| WeightedKeyword::new(t, -3))
                .collect(),
        }
    }
}

impl ScoringKeywords {
    /// Loads scoring keywords from a YAML file.
    ///
    /// Falls back to built-in defaults if the file is missing,
    /// the YAML is invalid, or a section is empty.
    pub fn load(path: &str) -> Self {
        let yaml = match std::fs::read_to_string(path) {
            Ok(y) => y,
            Err(_) => {
                tracing::warn!(
                    "Keywords file '{}' not found — using built-in defaults",
                    path
                );
                return Self::default();
            }
        };
        let raw: RawScoringKeywords = match serde_yaml::from_str(&yaml) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "Invalid keywords YAML '{}': {} — using built-in defaults",
                    path, e
                );
                return Self::default();
            }
        };
        Self {
            code_markers: raw.code_markers
                .filter(|v| !v.is_empty())
                .unwrap_or_else(default_code_markers),
            technical_keywords: resolve_keywords(
                &raw.languages,
                raw.technical_keywords.as_ref(),
                    |l| l.technical_keywords.as_ref(),
                default_technical_keywords,
                1,
                "technical_keywords",
            ),
            question_words: resolve_keywords(
                &raw.languages,
                raw.question_words.as_ref(),
                    |l| l.question_words.as_ref(),
                default_question_words,
                0,
                "question_words",
            ),
            simple_keywords: resolve_keywords(
                &raw.languages,
                raw.simple_keywords.as_ref(),
                    |l| l.simple_keywords.as_ref(),
                default_simple_keywords,
                -3,
                "simple_keywords",
            ),
        }
    }
}

/// Resolves keywords from either the language-based (`languages:`) or
/// flat v0.2.x format. Returns built-in defaults when neither is present.
/// Each entry is resolved to a [`WeightedKeyword`] with the group's default
/// weight applied to bare-string entries (and to objects that omit `weight`).
fn resolve_keywords<F>(
    languages: &Option<HashMap<String, RawLanguageKeywords>>,
    flat: Option<&Vec<RawKeywordEntry>>,
    extract: F,
    default: fn() -> Vec<String>,
    default_weight: i32,
    label: &str,
) -> Vec<WeightedKeyword>
where
    F: Fn(&RawLanguageKeywords) -> Option<&Vec<RawKeywordEntry>>,
{
    // v0.3+: flatten all languages into one pool
    if let Some(langs) = languages {
        if !langs.is_empty() {
            let mut merged: Vec<WeightedKeyword> = Vec::new();
            for (_name, block) in langs {
                if let Some(entries) = extract(block) {
                    if !entries.is_empty() {
                        merged.extend(
                            entries
                                .iter()
                                .cloned()
                                .map(|e| e.resolve(default_weight)),
                        );
                    }
                }
            }
            if !merged.is_empty() {
                // De-duplicate by term (case-insensitive). A term that appears
                // in several language blocks (e.g. "kubernetes" in both french
                // and english) must be scored once, not once per language.
                let mut seen: HashSet<String> = HashSet::new();
                merged.retain(|kw| seen.insert(kw.term.to_lowercase()));
                tracing::debug!("Loaded {} keyword(s) from {} language(s)", merged.len(), langs.len());
                return merged;
            }
        }
    }

    // v0.2.x: use flat lists (backward compat)
    if let Some(entries) = flat {
        if !entries.is_empty() {
            return entries.iter().cloned().map(|e| e.resolve(default_weight)).collect();
        }
    }

    // Fallback: built-in defaults
    tracing::warn!(
        "No {} found in keywords.yaml — using built-in defaults",
        label
    );
    default()
        .into_iter()
        .map(|t| WeightedKeyword::new(t, default_weight))
        .collect()
}

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
    #[serde(default = "default_host")]
    pub host: String,
    /// Optional shared secret for proxy-level authentication.
    /// When set, clients must send an `X-RouterCrabs-Key` header with this value.
    #[serde(default)]
    pub proxy_key: Option<String>,
    /// Path to the complexity scoring keywords file (default: `keywords.yaml`)
    #[serde(default = "default_keywords_path")]
    pub keywords_path: String,
    /// Path to the append-only JSONL journal (default: `journal.jsonl`)
    #[serde(default = "default_journal_path")]
    pub journal_path: String,
    #[serde(default)]
    pub tiers: Vec<RawTier>,
    pub fallback: Option<RawFallbackConfig>,
}

fn default_port() -> u16 { 8001 }
fn default_host() -> String { "127.0.0.1".into() }
fn default_keywords_path() -> String { "keywords.yaml".into() }
fn default_journal_path() -> String { "journal.jsonl".into() }

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

/// The semantic "side" of the complexity axis a tier represents.
///
/// RouterCrabs is model-agnostic: it does not know the names `pro` or
/// `flash`. It only distinguishes *simple* (fast/cheap) from *complex*
/// (strong/expensive) tiers, plus domain tiers that sit off that axis
/// entirely. Feedback and the RSI analyzer operate on this classification,
/// never on hardcoded model identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierKind {
    /// The fast/cheap fallback tier (`fallback.simple`).
    Simple,
    /// The strong/expensive fallback tier (`fallback.complex`).
    Complex,
    /// A domain tier (`tiers[]`), or an unrecognized name.
    Other,
}

/// Full configuration loaded from a YAML file.
#[derive(Debug)]
pub struct TiersConfig {
    /// Listening port for binary mode
    pub port: u16,
    /// Listening address (default: `127.0.0.1`)
    pub host: String,
    /// Optional shared secret for proxy-level authentication.
    /// When set, clients must send an `X-RouterCrabs-Key` header with this value.
    pub proxy_key: Option<String>,
    /// Resolved domain tiers
    pub tiers: Vec<Tier>,
    /// Complexity-based routing configuration
    pub fallback: Option<FallbackConfig>,
    /// Complexity scoring keywords (loaded from `keywords_path`)
    pub keywords: ScoringKeywords,
    /// Path to the complexity scoring keywords file (default: `keywords.yaml`)
    pub keywords_path: String,
    /// Path to the append-only JSONL journal
    pub journal_path: String,
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
        let keywords = ScoringKeywords::load(&raw.keywords_path);

        Ok(Self { port: raw.port, host: raw.host, proxy_key: raw.proxy_key, tiers, fallback, keywords, keywords_path: raw.keywords_path, journal_path: raw.journal_path })
    }

    /// Classifies a feedback `correction` string into its [`TierKind`].
    ///
    /// The value may be a configured model name (matched case-insensitively)
    /// or a generic semantic alias. This is the single place where a routing
    /// direction is decided, so no model identifier ever needs to be
    /// hardcoded elsewhere.
    ///
    /// Aliases (generic router vocabulary, not provider-specific):
    /// * `simple` / `flash` / `fast` / `easy` / `cheap` / `low` → [`TierKind::Simple`]
    /// * `complex` / `pro` / `hard` / `strong` / `smart` / `high` → [`TierKind::Complex`]
    pub fn classify(&self, s: &str) -> TierKind {
        let s = s.trim().to_lowercase();
        if s.is_empty() {
            return TierKind::Other;
        }

        if let Some(fb) = &self.fallback {
            if fb.simple.model.to_lowercase() == s {
                return TierKind::Simple;
            }
            if fb.complex.model.to_lowercase() == s {
                return TierKind::Complex;
            }
        }

        // Domain tiers sit off the simple/complex axis.
        if self.tiers.iter().any(|t| t.model.to_lowercase() == s) {
            return TierKind::Other;
        }

        match s.as_str() {
            "simple" | "flash" | "fast" | "easy" | "cheap" | "low" | "mini" | "weak" => {
                TierKind::Simple
            }
            "complex" | "pro" | "hard" | "strong" | "smart" | "high" | "max" | "heavy" => {
                TierKind::Complex
            }
            _ => TierKind::Other,
        }
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

/// Computes a complexity score for a list of messages.
///
/// The score is a **signed weighted sum**: technical keywords push it up
/// (positive weights → Pro), simple keywords push it down (negative weights
/// → Flash), question words are neutral (weight 0). The total can be
/// negative — that is the counterweight preventing drift toward "everything
/// is Pro".
///
/// Objective heuristics (independent of keyword sign) are still applied:
///
/// | Criterion | Score |
/// |-----------|-------|
/// | Prompt > 2000 characters | +3 |
/// | Prompt > 800 characters | +2 |
/// | Prompt > 300 characters | +1 |
/// | ≥ 3 code markers (```, `fn`, `class`, etc.) | +3 |
/// | ≥ 1 code marker | +2 |
/// | Image present | +5 |
///
/// # Example
///
/// ```rust
/// use router_crabs::{Message, MessageContent, ScoringKeywords, score_complexity};
///
/// let kw = ScoringKeywords::default();
/// let messages = vec![
///     Message {
///         role: "user".into(),
///         content: Some(MessageContent::Text(
///             "Explain microservices architecture, compare tradeoffs.".into()
///         )),
///     },
/// ];
/// let score = score_complexity(&messages, &kw);
/// assert!(score >= 2); // technical terms with positive weights → high score
/// ```
pub fn score_complexity(messages: &[Message], keywords: &ScoringKeywords) -> i32 {
    // Only score the LAST user message — not the full conversation history.
    // System prompts and history would otherwise inflate the score for every question.
    let last_user_text = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.text())
        .unwrap_or_default();
    let lower = last_user_text.to_lowercase();
    let len = last_user_text.len();

    // Signed: simple keywords subtract, technical keywords add, and the total
    // may legitimately go negative. That is intentional (the counterweight).
    let mut score: i32 = 0;

    // ── 1. Prompt length (objective) ──────
    if len > 2000 {
        score += 3;
    } else if len > 800 {
        score += 2;
    } else if len > 300 {
        score += 1;
    }

    // ── 2. Code presence (objective) ──────
    let code_count = keywords.code_markers.iter()
        .filter(|m| lower.contains(m.as_str()))
        .count();
    if code_count >= 3 {
        score += 3;
    } else if code_count >= 1 {
        score += 2;
    }

    // ── 3. Weighted keyword sum ────────────
    // Iterate all three groups and add each matched keyword's *signed* weight:
    //   - technical_keywords → positive (push toward Pro)
    //   - simple_keywords    → negative (push toward Flash)
    //   - question_words     → zero (neutral; no open-question bonus anymore)
    for group in [&keywords.technical_keywords, &keywords.question_words, &keywords.simple_keywords] {
        for kw in group.iter() {
            if lower.contains(kw.term.as_str()) {
                score += kw.weight;
            }
        }
    }

    // ── 4. Images (objective) ─────────────
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

    score
}

/// Returns the technical keywords that matched the last user message.
///
/// Mirrors the keyword-matching logic inside [`score_complexity`] so the
/// journal can record *which* terms fired (for later analysis), not just
/// the final score. Matching is case-insensitive substring over the last
/// user message only — the same text [`score_complexity`] scores.
///
/// # Example
///
/// ```rust
/// use router_crabs::{Message, MessageContent, ScoringKeywords, matched_technical_keywords};
///
/// let kw = ScoringKeywords::default();
/// let messages = vec![
///     Message {
///         role: "user".into(),
///         content: Some(MessageContent::Text(
///             "Explique comment implémenter un cache distribué".into()
///         )),
///     },
/// ];
/// let matched = matched_technical_keywords(&messages, &kw);
/// assert!(!matched.is_empty());
/// ```
pub fn matched_technical_keywords(
    messages: &[Message],
    keywords: &ScoringKeywords,
) -> Vec<String> {
    let last_user_text = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.text())
        .unwrap_or_default();
    let lower = last_user_text.to_lowercase();

    keywords
        .technical_keywords
        .iter()
        .filter(|kw| lower.contains(kw.term.as_str()))
        .map(|kw| kw.term.clone())
        .collect()
}

/// Returns every matched keyword (all three groups) with its signed weight.
///
/// Used by the request journal so a later analyzer can see *why* a prompt
/// scored a given value — which terms fired and whether each pushed toward
/// Pro (positive) or Flash (negative). Mirrors the weighted-sum loop inside
/// [`score_complexity`] over the last user message only.
pub fn matched_keyword_weights(
    messages: &[Message],
    keywords: &ScoringKeywords,
) -> Vec<(String, i32)> {
    let last_user_text = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.text())
        .unwrap_or_default();
    let lower = last_user_text.to_lowercase();

    let mut out: Vec<(String, i32)> = Vec::new();
    for group in [&keywords.technical_keywords, &keywords.question_words, &keywords.simple_keywords] {
        for kw in group.iter() {
            if lower.contains(kw.term.as_str()) {
                out.push((kw.term.clone(), kw.weight));
            }
        }
    }
    out
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
        let complexity = score_complexity(messages, &config.keywords);
        // `threshold` is u32 by config schema; cast to i32 so a negative
        // (simple-weighted) score correctly falls *below* the threshold
        // instead of wrapping to a huge unsigned value (→ wrong Pro route).
        if complexity >= fb.threshold as i32 {
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

// ── Self-improvement: journal analysis & guarded apply ──────────────────

/// A single keyword-change proposal, pending human approval.
/// Serialized to YAML in the proposals queue directory. **Never applied
/// automatically** — only surfaced for a human to approve or reject.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Proposal {
    pub id: String,
    pub created_at: String,
    #[serde(rename = "type")]
    pub prop_type: String, // "add" | "adjust" | "remove"
    pub target: String,    // "technical_keywords" | "simple_keywords" | "question_words"
    pub language: String,  // "french" | "english" | "arabic"
    pub term: String,
    pub weight: i32,
    pub reason: String,
    pub evidence: Evidence,
}

/// Evidence backing a [`Proposal`], for human review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub req_ids: Vec<String>,
    pub correct_tier_ratio: f64,
}

/// Thresholds controlling how conservative the analyzer is.
#[derive(Debug, Clone)]
pub struct AnalyzerConfig {
    /// Promote a new term into the config after this many same-direction corrections.
    pub promote_min_reqs: usize,
    /// Adjust an existing term after this many opposite-direction corrections.
    pub adjust_min_reqs: usize,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            promote_min_reqs: 5,
            adjust_min_reqs: 3,
        }
    }
}

#[derive(Debug, Clone)]
struct FbEntry {
    req_id: String,
    correct_tier: String,
}

#[derive(Debug, Clone, Default)]
struct TermAgg {
    complex_votes: usize,
    simple_votes: usize,
    req_ids: Vec<String>,
}

/// Best-effort language detection for a term: Arabic script → `arabic`,
/// otherwise `french` (this operator's primary language). Language only
/// affects *where* a term is filed in `keywords.yaml` — all languages are
/// flattened into one pool at runtime, so a mislabel has no scoring impact.
fn detect_language(term: &str) -> &'static str {
    if term
        .chars()
        .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c))
    {
        "arabic"
    } else {
        "french"
    }
}

/// Locates an existing term among the three keyword groups, returning the
/// group name and its current weight (or `None` if the term is unlisted).
fn find_existing(keywords: &ScoringKeywords, term: &str) -> Option<(String, i32)> {
    for (name, list) in [
        ("technical_keywords", &keywords.technical_keywords),
        ("simple_keywords", &keywords.simple_keywords),
        ("question_words", &keywords.question_words),
    ] {
        if let Some(k) = list.iter().find(|k| k.term == term) {
            return Some((name.to_string(), k.weight));
        }
    }
    None
}

/// Extracts matched terms (with signed weight) from a journal `req` object.
/// Prefers the rich `weights` field; falls back to `matched` (technical terms
/// only, weight assumed +1) for lines written before the `weights` field.
fn req_terms(v: &serde_json::Value) -> Vec<(String, i32)> {
    if let Some(arr) = v.get("weights").and_then(|w| w.as_array()) {
        let terms: Vec<(String, i32)> = arr
            .iter()
            .filter_map(|e| {
                let pair = e.as_array()?;
                let term = pair.get(0)?.as_str()?.to_string();
                let weight = pair.get(1).and_then(|n| n.as_i64()).unwrap_or(1) as i32;
                Some((term, weight))
            })
            .collect();
        if !terms.is_empty() {
            return terms;
        }
    }
    v.get("matched")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str().map(|s| (s.to_string(), 1)))
                .collect()
        })
        .unwrap_or_default()
}

/// Aggregates a journal (one JSON object per line) and produces keyword-change
/// proposals. Pure and deterministic — no file I/O, no config mutation.
///
/// Feedback `correct_tier` values are classified through
/// [`TiersConfig::classify`], so any configured model (or a semantic alias)
/// is understood — nothing here knows the names `pro` or `flash`.
pub fn analyze_journal(lines: &[&str], config: &TiersConfig) -> Vec<Proposal> {
    analyze_journal_with(
        lines,
        &config.keywords,
        &AnalyzerConfig::default(),
        &|s| config.classify(s),
    )
}

/// [`analyze_journal`] with an explicit classifier and thresholds (used by
/// tests and by callers that want a custom tier mapping).
pub fn analyze_journal_with(
    lines: &[&str],
    keywords: &ScoringKeywords,
    cfg: &AnalyzerConfig,
    classify: &dyn Fn(&str) -> TierKind,
) -> Vec<Proposal> {
    // 1. Parse req/fb lines and index requests by id.
    let mut reqs: HashMap<String, serde_json::Value> = HashMap::new();
    let mut fbs: Vec<FbEntry> = Vec::new();
    for line in lines {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "req" => {
                if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                    reqs.insert(id.to_string(), v);
                }
            }
            "fb" => {
                if let (Some(rid), Some(ct)) = (
                    v.get("req_id").and_then(|r| r.as_str()),
                    v.get("correct_tier").and_then(|c| c.as_str()),
                ) {
                    fbs.push(FbEntry {
                        req_id: rid.to_string(),
                        correct_tier: ct.to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    // 2. Aggregate per term: how many corrections pushed toward complex vs simple.
    let mut agg: HashMap<String, TermAgg> = HashMap::new();
    for fb in &fbs {
        if let Some(req) = reqs.get(&fb.req_id) {
            for (term, _w) in req_terms(req) {
                let e = agg.entry(term.clone()).or_default();
                e.req_ids.push(fb.req_id.clone());
                match classify(&fb.correct_tier) {
                    TierKind::Complex => e.complex_votes += 1,
                    TierKind::Simple => e.simple_votes += 1,
                    TierKind::Other => {}
                }
            }
        }
    }

    // 3. Generate proposals from the aggregate.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut proposals: Vec<Proposal> = Vec::new();
    let mut counter = 0usize;

    for (term, a) in agg.iter() {
        let lang = detect_language(term);
        let ratio =
            (a.complex_votes as f64) / ((a.complex_votes + a.simple_votes).max(1) as f64);

        match find_existing(keywords, term) {
            // ── Existing term: adjust or remove ──
            Some((target, cur)) => {
                let evidence = Evidence {
                    req_ids: a.req_ids.clone(),
                    correct_tier_ratio: ratio,
                };
                // Technical term repeatedly corrected toward simple → weaken it.
                if target == "technical_keywords" && a.simple_votes >= cfg.adjust_min_reqs {
                    counter += 1;
                    let new_weight = cur - 1;
                    proposals.push(if new_weight < 0 {
                        Proposal {
                            id: format!("prop-{:03}", counter),
                            created_at: now.clone(),
                            prop_type: "remove".into(),
                            target,
                            language: lang.to_string(),
                            term: term.clone(),
                            weight: 0,
                            reason: format!(
                                "{} requête(s) corrigée(s) vers un tier simple, poids déjà au minimum",
                                a.simple_votes
                            ),
                            evidence,
                        }
                    } else {
                        Proposal {
                            id: format!("prop-{:03}", counter),
                            created_at: now.clone(),
                            prop_type: "adjust".into(),
                            target,
                            language: lang.to_string(),
                            term: term.clone(),
                            weight: new_weight,
                            reason: format!(
                                "{} requête(s) corrigée(s) vers un tier simple",
                                a.simple_votes
                            ),
                            evidence,
                        }
                    });
                }
                // Simple term repeatedly corrected toward complex → weaken the counterweight.
                else if target == "simple_keywords" && a.complex_votes >= cfg.adjust_min_reqs {
                    counter += 1;
                    let new_weight = cur + 1;
                    proposals.push(if new_weight >= 0 {
                        Proposal {
                            id: format!("prop-{:03}", counter),
                            created_at: now.clone(),
                            prop_type: "remove".into(),
                            target,
                            language: lang.to_string(),
                            term: term.clone(),
                            weight: 0,
                            reason: format!(
                                "{} requête(s) corrigée(s) vers un tier complex, contre-poids devenu inutile",
                                a.complex_votes
                            ),
                            evidence,
                        }
                    } else {
                        Proposal {
                            id: format!("prop-{:03}", counter),
                            created_at: now.clone(),
                            prop_type: "adjust".into(),
                            target,
                            language: lang.to_string(),
                            term: term.clone(),
                            weight: new_weight,
                            reason: format!(
                                "{} requête(s) corrigée(s) vers un tier complex",
                                a.complex_votes
                            ),
                            evidence,
                        }
                    });
                }
            }
            // ── Unlisted term: promote into the config ──
            None => {
                if a.complex_votes >= cfg.promote_min_reqs {
                    counter += 1;
                    proposals.push(Proposal {
                        id: format!("prop-{:03}", counter),
                        created_at: now.clone(),
                        prop_type: "add".into(),
                        target: "technical_keywords".into(),
                        language: lang.to_string(),
                        term: term.clone(),
                        weight: 1,
                        reason: format!(
                            "{} requête(s) corrigée(s) vers un tier complex contenaient ce terme",
                            a.complex_votes
                        ),
                        evidence: Evidence {
                            req_ids: a.req_ids.clone(),
                            correct_tier_ratio: ratio,
                        },
                    });
                } else if a.simple_votes >= cfg.promote_min_reqs {
                    counter += 1;
                    proposals.push(Proposal {
                        id: format!("prop-{:03}", counter),
                        created_at: now.clone(),
                        prop_type: "add".into(),
                        target: "simple_keywords".into(),
                        language: lang.to_string(),
                        term: term.clone(),
                        weight: -3,
                        reason: format!(
                            "{} requête(s) corrigée(s) vers un tier simple contenaient ce terme",
                            a.simple_votes
                        ),
                        evidence: Evidence {
                            req_ids: a.req_ids.clone(),
                            correct_tier_ratio: ratio,
                        },
                    });
                }
            }
        }
    }

    proposals
}

/// Applies a single approved proposal to the raw `keywords.yaml` text and
/// returns the new YAML. Pure — no file I/O, no backup, no reload; the caller
/// is responsible for those guarded steps. Fails on invalid input so the
/// caller can abort before touching the real file.
pub fn apply_proposal_to_yaml(yaml: &str, proposal: &Proposal) -> anyhow::Result<String> {
    let mut value: serde_yaml::Value = serde_yaml::from_str(yaml)?;
    let list = yaml_navigate_mut(&mut value, &proposal.language, &proposal.target)?;

    match proposal.prop_type.as_str() {
        "add" => {
            // Guard against silent duplicates (invariant: one term, one list).
            if list.iter().any(|v| yaml_entry_term(v) == Some(&proposal.term)) {
                anyhow::bail!("term '{}' already exists in {}", proposal.term, proposal.target);
            }
            list.push(serde_yaml::to_value(&serde_json::json!({
                "term": proposal.term,
                "weight": proposal.weight,
            }))?);
        }
        "adjust" => {
            let idx = list
                .iter()
                .position(|v| yaml_entry_term(v) == Some(&proposal.term))
                .ok_or_else(|| {
                    anyhow::anyhow!("term '{}' not found in {}", proposal.term, proposal.target)
                })?;
            list[idx] = serde_yaml::to_value(&serde_json::json!({
                "term": proposal.term,
                "weight": proposal.weight,
            }))?;
        }
        "remove" => {
            let before = list.len();
            list.retain(|v| yaml_entry_term(v) != Some(&proposal.term));
            if list.len() == before {
                anyhow::bail!("term '{}' not found in {}", proposal.term, proposal.target);
            }
        }
        other => anyhow::bail!("unknown proposal type: {}", other),
    }

    Ok(format!("# RouterCrabs keywords — modified by apply ({})\n", proposal.id)
        + &serde_yaml::to_string(&value)?)
}

/// Returns the `term` of a YAML list entry, whether it's a bare string
/// (`- "explique"`) or an object (`- {term: "explique", weight: 1}`).
fn yaml_entry_term(v: &serde_yaml::Value) -> Option<&str> {
    match v {
        serde_yaml::Value::String(s) => Some(s.as_str()),
        serde_yaml::Value::Mapping(m) => m
            .get(&serde_yaml::Value::String("term".into()))
            .and_then(|t| t.as_str()),
        _ => None,
    }
}

/// Navigates to `languages.<language>.<target>` and returns a mutable handle
/// to the keyword list (sequence), creating missing ancestors as needed.
fn yaml_navigate_mut<'a>(
    root: &'a mut serde_yaml::Value,
    language: &str,
    target: &str,
) -> anyhow::Result<&'a mut Vec<serde_yaml::Value>> {
    let languages = root
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("root is not a mapping"))?
        .entry(serde_yaml::Value::String("languages".into()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    let lang = languages
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("'languages' is not a mapping"))?
        .entry(serde_yaml::Value::String(language.into()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    let list = lang
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("language '{}' is not a mapping", language))?
        .entry(serde_yaml::Value::String(target.into()))
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));

    list.as_sequence_mut()
        .ok_or_else(|| anyhow::anyhow!("target '{}' is not a sequence", target))
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> ScoringKeywords {
        let raw: RawScoringKeywords = serde_yaml::from_str(yaml)
            .expect("test YAML should parse");
        ScoringKeywords {
            code_markers: raw
                .code_markers
                .filter(|v| !v.is_empty())
                .unwrap_or_else(default_code_markers),
            technical_keywords: resolve_keywords(
                &raw.languages,
                raw.technical_keywords.as_ref(),
                |l| l.technical_keywords.as_ref(),
                default_technical_keywords,
                1,
                "technical_keywords",
            ),
            question_words: resolve_keywords(
                &raw.languages,
                raw.question_words.as_ref(),
                |l| l.question_words.as_ref(),
                default_question_words,
                0,
                "question_words",
            ),
            simple_keywords: resolve_keywords(
                &raw.languages,
                raw.simple_keywords.as_ref(),
                |l| l.simple_keywords.as_ref(),
                default_simple_keywords,
                -3,
                "simple_keywords",
            ),
        }
    }

    #[test]
    fn string_form_gets_default_weight() {
        let kw = parse(
            r#"
languages:
  english:
    technical_keywords: ["explain", "architecture"]
    simple_keywords: ["hello"]
"#,
        );
        let explain = kw.technical_keywords.iter().find(|k| k.term == "explain").unwrap();
        assert_eq!(explain.weight, 1);
        let hello = kw.simple_keywords.iter().find(|k| k.term == "hello").unwrap();
        assert_eq!(hello.weight, -3);
    }

    #[test]
    fn object_form_reads_explicit_weight_and_meta() {
        let kw = parse(
            r#"
languages:
  english:
    technical_keywords:
      - { term: "design pattern", weight: 4, hits: 12, last_seen: "2026-08-20T10:00:00Z", added_at: "2026-08-01T00:00:00Z" }
"#,
        );
        let dp = kw.technical_keywords.iter().find(|k| k.term == "design pattern").unwrap();
        assert_eq!(dp.weight, 4);
        let meta = dp.meta.as_ref().expect("meta should be present");
        assert_eq!(meta.hits, 12);
        assert_eq!(meta.last_seen.as_deref(), Some("2026-08-20T10:00:00Z"));
        assert_eq!(meta.added_at.as_deref(), Some("2026-08-01T00:00:00Z"));
    }

    #[test]
    fn object_form_omitting_weight_uses_default() {
        let kw = parse(
            r#"
languages:
  french:
    simple_keywords:
      - { term: "merci" }
"#,
        );
        let merci = kw.simple_keywords.iter().find(|k| k.term == "merci").unwrap();
        assert_eq!(merci.weight, -3); // default for simple_keywords, not 0
    }

    #[test]
    fn simple_keywords_negative_weight_flows_into_scoring() {
        let kw = parse(
            r#"
languages:
  french:
    technical_keywords: ["explique"]
    simple_keywords: ["salut"]
"#,
        );
        let neg = kw.simple_keywords.iter().find(|k| k.term == "salut").unwrap();
        assert!(neg.weight < 0, "simple keyword weight must be negative, got {}", neg.weight);
    }

    #[test]
    fn backward_compat_flat_v02_format() {
        // Flat v0.2.x format: technical_keywords at top level, no languages block.
        let kw = parse(
            r#"
technical_keywords: ["explique", "analyse"]
question_words: ["pourquoi"]
"#,
        );
        assert!(kw.technical_keywords.iter().any(|k| k.term == "explique"));
        assert!(kw.question_words.iter().any(|k| k.term == "pourquoi"));
    }

    #[test]
    fn cross_language_duplicate_is_deduplicated() {
        // The same term ("kubernetes") in both french and english must resolve
        // to a single WeightedKeyword, not one per language — otherwise the
        // score double-counts bilingual technical terms (kubernetes +2 instead
        // of +1, inflating the route toward Pro).
        let kw = parse(
            r#"
languages:
  french:
    technical_keywords: ["kubernetes", "configurer"]
  english:
    technical_keywords: ["kubernetes", "configure"]
"#,
        );
        let kube: Vec<_> = kw
            .technical_keywords
            .iter()
            .filter(|k| k.term == "kubernetes")
            .collect();
        assert_eq!(kube.len(), 1, "bilingual term must be deduplicated");
    }

    #[test]
    fn bare_string_entries_have_no_meta() {
        let kw = parse(
            r#"
languages:
  english:
    technical_keywords: ["explain"]
"#,
        );
        let explain = kw.technical_keywords.iter().find(|k| k.term == "explain").unwrap();
        assert!(explain.meta.is_none(), "bare string entries carry no lifecycle metadata");
    }

    fn user_msg(text: &str) -> Vec<Message> {
        vec![Message {
            role: "user".into(),
            content: Some(MessageContent::Text(text.into())),
        }]
    }

    #[test]
    fn simple_keyword_pushes_score_negative() {
        // "salut" is a default simple keyword with weight -3; no technical
        // term, short prompt, no code → the weighted sum goes negative.
        let kw = ScoringKeywords::default();
        let score = score_complexity(&user_msg("salut"), &kw);
        assert!(score < 0, "simple greeting should score negative, got {score}");
    }

    #[test]
    fn technical_keywords_push_score_positive() {
        // Several technical terms should accumulate positive weight and clear
        // the default complexity threshold.
        let kw = ScoringKeywords::default();
        let score = score_complexity(
            &user_msg("Explique l'architecture d'un cache distribué et compare les tradeoffs"),
            &kw,
        );
        assert!(score >= 2, "technical prompt should score >= 2, got {score}");
    }

    #[test]
    fn question_words_are_neutral_no_bonus() {
        // A bare "pourquoi ?" used to earn +1 (open-question bonus). Now the
        // question word is weight 0 and there is no bonus, so the score must
        // not be inflated by interrogatives alone.
        let kw = ScoringKeywords::default();
        let score = score_complexity(&user_msg("pourquoi ?"), &kw);
        assert_eq!(score, 0, "bare question word should be neutral, got {score}");
    }

    #[test]
    fn matched_keyword_weights_include_signs() {
        let kw = ScoringKeywords::default();
        let weights = matched_keyword_weights(&user_msg("salut"), &kw);
        assert!(weights.iter().any(|(t, w)| t == "salut" && *w < 0));
    }

    // ── analyze / apply ────────────────────────────────────────────────

    fn req_line(id: &str, matched: &[&str]) -> String {
        serde_json::json!({
            "type": "req",
            "id": id,
            "ts": "2026-08-27T21:00:00Z",
            "profile": "test",
            "prompt": "test",
            "score": 0,
            "matched": matched,
            "routed": "pro",
        })
        .to_string()
    }

    fn fb_line(req_id: &str, correct_tier: &str) -> String {
        serde_json::json!({
            "type": "fb",
            "req_id": req_id,
            "ts": "2026-08-27T21:01:00Z",
            "correct_tier": correct_tier,
            "source": "slash",
        })
        .to_string()
    }

    // Maps a test correction string to a TierKind, mirroring a real
    // TiersConfig whose fallback models are "acme-mini" (simple) /
    // "acme-max" (complex). Kept independent of the classifier under test.
    fn classify(s: &str) -> TierKind {
        match s {
            "pro" | "complex" | "acme-max" => TierKind::Complex,
            "flash" | "simple" | "acme-mini" => TierKind::Simple,
            _ => TierKind::Other,
        }
    }

    fn test_config() -> TiersConfig {
        TiersConfig {
            port: 8001,
            host: "127.0.0.1".into(),
            proxy_key: None,
            tiers: vec![],
            fallback: Some(FallbackConfig {
                threshold: 2,
                simple: FallbackTier {
                    model: "acme-mini".into(),
                    api_base: "https://example.com".into(),
                    api_key: "k".into(),
                    auth_header: "Bearer".into(),
                },
                complex: FallbackTier {
                    model: "acme-max".into(),
                    api_base: "https://example.com".into(),
                    api_key: "k".into(),
                    auth_header: "Bearer".into(),
                },
            }),
            keywords: ScoringKeywords::default(),
            keywords_path: "keywords.yaml".into(),
            journal_path: "journal.jsonl".into(),
        }
    }

    #[test]
    fn tiers_config_classifies_model_names_and_aliases() {
        let cfg = test_config();
        // Exact model names (case-insensitive).
        assert_eq!(cfg.classify("acme-mini"), TierKind::Simple);
        assert_eq!(cfg.classify("ACME-MAX"), TierKind::Complex);
        // Generic aliases work even without a matching model name.
        assert_eq!(cfg.classify("pro"), TierKind::Complex);
        assert_eq!(cfg.classify("complex"), TierKind::Complex);
        assert_eq!(cfg.classify("flash"), TierKind::Simple);
        assert_eq!(cfg.classify("simple"), TierKind::Simple);
        // Unknown / empty → Other.
        assert_eq!(cfg.classify("gpt-4o"), TierKind::Other);
        assert_eq!(cfg.classify(""), TierKind::Other);
    }

    #[test]
    fn analyze_accepts_semantic_kinds() {
        // The analyzer must understand "complex"/"simple" (the normalized
        // feedback values) exactly like the legacy "pro"/"flash" aliases.
        let kw = ScoringKeywords::default();
        let lines: Vec<String> = (0..5)
            .flat_map(|i| {
                vec![
                    req_line(&format!("r{i}"), &["refactoriser"]),
                    fb_line(&format!("r{i}"), "complex"),
                ]
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let proposals = analyze_journal_with(&refs, &kw, &AnalyzerConfig::default(), &classify);
        assert!(proposals
            .iter()
            .any(|p| p.prop_type == "add" && p.term == "refactoriser"));
    }

    #[test]
    fn analyze_promotes_unlisted_term_to_technical() {
        // 5 reqs all corrected to Pro, matching "refactoriser" (unlisted).
        let kw = ScoringKeywords::default();
        let lines: Vec<String> = (0..5)
            .flat_map(|i| {
                vec![
                    req_line(&format!("r{i}"), &["refactoriser"]),
                    fb_line(&format!("r{i}"), "pro"),
                ]
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();

        let proposals = analyze_journal_with(&refs, &kw, &AnalyzerConfig::default(), &classify);
        let add = proposals
            .iter()
            .find(|p| p.prop_type == "add" && p.term == "refactoriser")
            .expect("should propose adding refactoriser");
        assert_eq!(add.target, "technical_keywords");
        assert_eq!(add.weight, 1);
    }

    #[test]
    fn analyze_adjusts_technical_term_corrected_to_flash() {
        // "explique" is a technical term (weight 1); 3 corrections to Flash
        // should propose weakening it (adjust, weight 0).
        let kw = ScoringKeywords::default();
        let lines: Vec<String> = (0..3)
            .flat_map(|i| {
                vec![
                    req_line(&format!("r{i}"), &["explique"]),
                    fb_line(&format!("r{i}"), "flash"),
                ]
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();

        let proposals = analyze_journal_with(&refs, &kw, &AnalyzerConfig::default(), &classify);
        let adjust = proposals
            .iter()
            .find(|p| p.term == "explique")
            .expect("should propose adjusting explique");
        assert_eq!(adjust.prop_type, "adjust");
        assert_eq!(adjust.weight, 0); // 1 - 1
    }

    #[test]
    fn analyze_promotes_unlisted_term_to_simple() {
        // 5 corrections to Flash matching "refactoriser" → add to simple_keywords
        let kw = ScoringKeywords::default();
        let lines: Vec<String> = (0..5)
            .flat_map(|i| {
                vec![
                    req_line(&format!("r{i}"), &["refactoriser"]),
                    fb_line(&format!("r{i}"), "flash"),
                ]
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();

        let proposals = analyze_journal_with(&refs, &kw, &AnalyzerConfig::default(), &classify);
        let add = proposals
            .iter()
            .find(|p| p.prop_type == "add" && p.term == "refactoriser")
            .expect("should propose adding refactoriser to simple");
        assert_eq!(add.target, "simple_keywords");
        assert_eq!(add.weight, -3);
    }

    #[test]
    fn analyze_ignores_uncorrected_requests() {
        // Requests with no fb line produce no aggregate → no proposals.
        let kw = ScoringKeywords::default();
        let lines: Vec<String> = (0..3).map(|i| req_line(&format!("r{i}"), &["explique"])).collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        assert!(analyze_journal_with(&refs, &kw, &AnalyzerConfig::default(), &classify).is_empty());
    }

    #[test]
    fn apply_add_inserts_term_object() {
        let yaml = "languages:\n  french:\n    technical_keywords:\n      - \"explique\"\n";
        let p = Proposal {
            id: "prop-001".into(),
            created_at: "2026-08-27T21:00:00Z".into(),
            prop_type: "add".into(),
            target: "technical_keywords".into(),
            language: "french".into(),
            term: "refactoriser".into(),
            weight: 2,
            reason: "test".into(),
            evidence: Evidence { req_ids: vec![], correct_tier_ratio: 1.0 },
        };
        let out = apply_proposal_to_yaml(yaml, &p).expect("apply should succeed");
        assert!(out.contains("refactoriser"), "new term should appear: {out}");
        // Re-parse to confirm it's valid YAML and the term landed in the list.
        let parsed: RawScoringKeywords = serde_yaml::from_str(&out).expect("reparse");
        let langs = parsed.languages.expect("languages present");
        let fr = langs.get("french").expect("french present");
        let terms: Vec<&str> = fr
            .technical_keywords
            .as_ref()
            .expect("technical present")
            .iter()
            .map(|e| match e {
                RawKeywordEntry::String(s) => s.as_str(),
                RawKeywordEntry::Object { term, .. } => term.as_str(),
            })
            .collect();
        assert!(terms.contains(&"refactoriser"));
    }

    #[test]
    fn apply_add_rejects_duplicate() {
        let yaml = "languages:\n  french:\n    technical_keywords:\n      - \"explique\"\n";
        let p = Proposal {
            id: "prop-001".into(),
            created_at: "2026-08-27T21:00:00Z".into(),
            prop_type: "add".into(),
            target: "technical_keywords".into(),
            language: "french".into(),
            term: "explique".into(),
            weight: 2,
            reason: "test".into(),
            evidence: Evidence { req_ids: vec![], correct_tier_ratio: 1.0 },
        };
        assert!(apply_proposal_to_yaml(yaml, &p).is_err(), "duplicate add must fail");
    }

    #[test]
    fn apply_remove_deletes_term() {
        let yaml = "languages:\n  french:\n    technical_keywords:\n      - \"explique\"\n      - \"analyse\"\n";
        let p = Proposal {
            id: "prop-001".into(),
            created_at: "2026-08-27T21:00:00Z".into(),
            prop_type: "remove".into(),
            target: "technical_keywords".into(),
            language: "french".into(),
            term: "explique".into(),
            weight: 0,
            reason: "test".into(),
            evidence: Evidence { req_ids: vec![], correct_tier_ratio: 1.0 },
        };
        let out = apply_proposal_to_yaml(yaml, &p).expect("remove should succeed");
        assert!(!out.contains("explique"), "term should be removed: {out}");
        assert!(out.contains("analyse"), "other terms must survive");
    }
}
