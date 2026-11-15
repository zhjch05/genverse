//! The few settings the agent CLIs take from the player's own config: which model and
//! provider to use, and how to reach it. Everything else in that config (other MCP
//! servers, hooks, approval rules, instructions) stays out, so a run is still isolated.
//!
//! Without these, a player who points Codex at their own endpoint (a `model_provider`
//! with its own `base_url`) or Claude Code at Bedrock, Vertex or a proxy (an `env` block
//! in `settings.json`) would get the default provider instead, which can't sign them in.

use std::path::PathBuf;

use serde_json::{Map, Value};

/// What a Codex run takes from `$CODEX_HOME/config.toml`.
#[derive(Debug, Default, PartialEq)]
pub struct Codex {
    /// `key=value` overrides for `-c`, values in TOML.
    pub overrides: Vec<String>,
    /// The model it names, if any.
    pub model: Option<String>,
    /// The reasoning level it names, if any.
    pub effort: Option<String>,
    /// A provider other than OpenAI's own, if it names one.
    pub custom_provider: Option<String>,
    /// Environment variables the provider reads its key (or headers) from.
    pub env: Vec<String>,
}

/// Keys about the model itself, kept when present. Codex sends some of them to the
/// endpoint, which a custom one may need set its own way.
const CODEX_MODEL_KEYS: [&str; 8] = [
    "model",
    "model_provider",
    "model_context_window",
    "model_auto_compact_token_limit",
    "model_max_output_tokens",
    "model_supports_reasoning_summaries",
    "model_reasoning_summary",
    "model_verbosity",
];

/// Providers built into Codex: naming one needs no table of its own.
const CODEX_BUILT_IN: [&str; 4] = ["openai", "oss", "ollama", "lmstudio"];

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
}

pub fn codex() -> Codex {
    codex_home()
        .and_then(|dir| std::fs::read_to_string(dir.join("config.toml")).ok())
        .map(|text| codex_from(&text))
        .unwrap_or_default()
}

fn codex_from(text: &str) -> Codex {
    let Ok(config) = text.parse::<toml::Table>() else {
        return Codex::default();
    };
    // A profile's keys win over the top level's.
    let profile = config
        .get("profile")
        .and_then(|p| p.as_str())
        .and_then(|name| config.get("profiles")?.get(name)?.as_table());
    let get = |key: &str| profile.and_then(|p| p.get(key)).or_else(|| config.get(key));
    let mut out = Codex::default();
    for key in CODEX_MODEL_KEYS {
        if let Some(value) = get(key) {
            out.overrides.push(format!("{key}={value}"));
        }
    }
    out.model = get("model").and_then(|v| v.as_str()).map(str::to_string);
    out.effort = get("model_reasoning_effort")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let provider = get("model_provider").and_then(|v| v.as_str());
    if let Some(name) = provider.filter(|name| !CODEX_BUILT_IN.contains(name)) {
        out.custom_provider = Some(name.to_string());
        let table = config
            .get("model_providers")
            .and_then(|t| t.get(name))
            .and_then(|t| t.as_table());
        // The override's key splits on dots, so a name with one can't be given whole.
        if let Some(table) = table.filter(|_| !name.contains('.')) {
            out.overrides
                .push(format!("model_providers.{name}={}", inline(table)));
            if let Some(key) = table.get("env_key").and_then(|v| v.as_str()) {
                out.env.push(key.to_string());
            }
            if let Some(headers) = table.get("env_http_headers").and_then(|v| v.as_table()) {
                out.env.extend(
                    headers
                        .values()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string),
                );
            }
        }
    }
    out
}

/// A table as one inline TOML value.
fn inline(table: &toml::Table) -> String {
    let fields: Vec<String> = table
        .iter()
        .map(|(key, value)| {
            let value = match value {
                toml::Value::Table(t) => inline(t),
                v => v.to_string(),
            };
            format!("{} = {value}", toml::Value::String(key.clone()))
        })
        .collect();
    format!("{{ {} }}", fields.join(", "))
}

/// What a Claude Code run takes from `~/.claude/settings.json`.
#[derive(Debug, Default, PartialEq)]
pub struct Claude {
    /// Settings for `--settings`, if there's anything to carry.
    pub settings: Option<Value>,
    /// The model it names, if any.
    pub model: Option<String>,
}

/// Settings about signing in and reaching a model.
const CLAUDE_KEYS: [&str; 4] = [
    "model",
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
];

/// Environment variables about the model and the provider, from the `env` block (or the
/// login shell, see [`crate::platform`]).
pub fn provider_env(name: &str) -> bool {
    const PREFIXES: [&str; 6] = [
        "ANTHROPIC_",
        "CLAUDE_CODE_USE_",
        "CLAUDE_CODE_SKIP_",
        "AWS_",
        "VERTEX_REGION_",
        "OPENAI_",
    ];
    const NAMES: [&str; 9] = [
        "CLOUD_ML_REGION",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GOOGLE_CLOUD_PROJECT",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "NODE_EXTRA_CA_CERTS",
        "API_TIMEOUT_MS",
        "CODEX_API_KEY",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p)) || NAMES.contains(&name)
}

pub fn claude_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".claude")))
}

pub fn claude() -> Claude {
    claude_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join("settings.json")).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .map(|settings| claude_from(&settings))
        .unwrap_or_default()
}

fn claude_from(settings: &Value) -> Claude {
    let mut kept = Map::new();
    for key in CLAUDE_KEYS {
        if let Some(value) = settings.get(key) {
            kept.insert(key.into(), value.clone());
        }
    }
    let env: Map<String, Value> = settings["env"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| provider_env(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let model = settings["model"]
        .as_str()
        .or(env.get("ANTHROPIC_MODEL").and_then(|m| m.as_str()))
        .map(str::to_string);
    if !env.is_empty() {
        kept.insert("env".into(), Value::Object(env));
    }
    Claude {
        settings: (!kept.is_empty()).then_some(Value::Object(kept)),
        model,
    }
}

/// Every variable a provider in the player's config reads, to bring from the login shell.
pub fn wanted_env() -> impl Fn(&str) -> bool {
    let codex = codex().env;
    move |name| provider_env(name) || codex.iter().any(|e| e == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_custom_codex_provider_comes_along_and_nothing_else() {
        let config = r#"
            model = "qwen4-max-preview"
            model_provider = "qwen"
            model_reasoning_effort = "high"
            approval_policy = "never"

            [model_providers.qwen]
            name = "Qwen"
            base_url = "https://example.test/v1"
            env_key = "QWEN_API_KEY"
            wire_api = "chat"
            http_headers = { "X-Team" = "a b" }

            [mcp_servers.other]
            command = "something"
        "#;
        let codex = codex_from(config);
        assert_eq!(codex.model.as_deref(), Some("qwen4-max-preview"));
        assert_eq!(codex.effort.as_deref(), Some("high"));
        assert_eq!(codex.custom_provider.as_deref(), Some("qwen"));
        assert_eq!(codex.env, ["QWEN_API_KEY"]);
        assert_eq!(codex.overrides[0], r#"model="qwen4-max-preview""#);
        assert_eq!(codex.overrides[1], r#"model_provider="qwen""#);
        let table = &codex.overrides[2];
        assert!(table.starts_with("model_providers.qwen={ "), "{table}");
        // What Codex gets back parses to the same table.
        let value = table.split_once('=').unwrap().1;
        let parsed: toml::Table = format!("v = {value}").parse().unwrap();
        let expected: toml::Table = config.parse().unwrap();
        assert_eq!(parsed["v"], expected["model_providers"]["qwen"]);
        assert!(
            codex
                .overrides
                .iter()
                .all(|o| !o.contains("mcp_servers") && !o.contains("approval"))
        );
    }

    #[test]
    fn a_codex_profile_wins_and_built_in_providers_need_no_table() {
        let codex = codex_from(
            r#"
            model = "a"
            profile = "local"
            [profiles.local]
            model = "b"
            model_provider = "ollama"
        "#,
        );
        assert_eq!(codex.model.as_deref(), Some("b"));
        assert_eq!(codex.custom_provider, None);
        assert_eq!(
            codex.overrides,
            [r#"model="b""#, r#"model_provider="ollama""#]
        );
        assert_eq!(codex_from("not = [toml"), Codex::default());
    }

    #[test]
    fn claude_keeps_the_provider_settings_only() {
        let claude = claude_from(&json!({
            "model": "opus",
            "apiKeyHelper": "~/bin/key",
            "hooks": { "Stop": [] },
            "permissions": { "allow": ["Bash"] },
            "env": {
                "ANTHROPIC_BASE_URL": "https://proxy.test",
                "CLAUDE_CODE_USE_BEDROCK": "1",
                "MY_SECRET": "x"
            }
        }));
        assert_eq!(claude.model.as_deref(), Some("opus"));
        assert_eq!(
            claude.settings,
            Some(json!({
                "model": "opus",
                "apiKeyHelper": "~/bin/key",
                "env": {
                    "ANTHROPIC_BASE_URL": "https://proxy.test",
                    "CLAUDE_CODE_USE_BEDROCK": "1"
                }
            }))
        );
        assert_eq!(claude_from(&json!({ "theme": "dark" })), Claude::default());
    }
}
