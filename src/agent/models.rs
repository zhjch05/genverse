//! The models and reasoning levels each agent CLI offers, for the chat's model picker,
//! and the one the player picked.

use serde_json::Value;

use super::{config, prefs, runner::Provider};

/// The player's pick for one agent. `None` leaves it to the agent (or, for Codex's
/// reasoning, to the game's own default).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choice {
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Choice {
    fn key(provider: Provider, what: &str) -> String {
        let agent = match provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        };
        format!("{what}.{agent}")
    }

    /// What was picked last time, or `GENVERSE_MODEL` / `GENVERSE_EFFORT` when set.
    pub fn load(provider: Provider) -> Self {
        let get = |what: &str, var: &str| {
            std::env::var(var)
                .ok()
                .or_else(|| {
                    prefs::get(&Self::key(provider, what))?
                        .as_str()
                        .map(str::to_string)
                })
                .filter(|v| !v.is_empty())
        };
        Self {
            model: get("model", "GENVERSE_MODEL"),
            effort: get("effort", "GENVERSE_EFFORT"),
        }
    }

    pub fn save(&self, provider: Provider) {
        let value = |v: &Option<String>| v.clone().map_or(Value::Null, Value::String);
        prefs::set(&Self::key(provider, "model"), value(&self.model));
        prefs::set(&Self::key(provider, "effort"), value(&self.effort));
    }

    /// The model's name, for the picker.
    pub fn label(&self, models: &[Model]) -> String {
        match &self.model {
            // "Default (opus)" reads as just "opus" once picked.
            None => models
                .first()
                .and_then(|m| m.label.strip_prefix("Default (")?.strip_suffix(')'))
                .unwrap_or("Default")
                .to_string(),
            Some(id) => models
                .iter()
                .find(|m| m.id.as_deref() == Some(id))
                .map_or_else(|| id.clone(), |m| m.label.clone()),
        }
    }
}

/// One model in the picker.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    /// What's passed to the CLI; `None` is the default entry.
    pub id: Option<String>,
    pub label: String,
    /// The reasoning levels it takes, lowest first.
    pub efforts: Vec<String>,
}

/// Claude Code's `--effort` levels.
const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// Codex's usual levels, when its cache doesn't say.
const CODEX_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

fn levels(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// The models to offer for `provider`, the default entry first.
pub fn list(provider: Provider) -> Vec<Model> {
    match provider {
        Provider::Claude => claude(config::claude().model),
        Provider::Codex => {
            let cache = config::codex_home()
                .and_then(|dir| std::fs::read_to_string(dir.join("models_cache.json")).ok())
                .and_then(|text| serde_json::from_str(&text).ok());
            codex(&config::codex(), cache.as_ref())
        }
    }
}

fn claude(configured: Option<String>) -> Vec<Model> {
    let default = match &configured {
        Some(model) => format!("Default ({model})"),
        None => "Default".into(),
    };
    let entry = |id: Option<&str>, label: &str| Model {
        id: id.map(str::to_string),
        label: label.into(),
        efforts: levels(&CLAUDE_EFFORTS),
    };
    vec![
        entry(None, &default),
        entry(Some("fable"), "Fable"),
        entry(Some("opus"), "Opus"),
        entry(Some("sonnet"), "Sonnet"),
        entry(Some("haiku"), "Haiku"),
    ]
}

fn codex(config: &config::Codex, cache: Option<&Value>) -> Vec<Model> {
    let default = match &config.model {
        Some(model) => format!("Default ({model})"),
        None => "Default".into(),
    };
    // OpenAI's list says nothing about a custom provider's models.
    let listed: Vec<&Value> = match (&config.custom_provider, cache) {
        (None, Some(cache)) => cache["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["visibility"] == "list" && m["slug"].is_string())
            .collect(),
        _ => Vec::new(),
    };
    let efforts_of = |model: &Value| -> Vec<String> {
        let found: Vec<String> = model["supported_reasoning_levels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|level| level["effort"].as_str().map(str::to_string))
            .collect();
        if found.is_empty() {
            levels(&CODEX_EFFORTS)
        } else {
            found
        }
    };
    // The default's levels are the configured model's, if the list has it.
    let default_efforts = config
        .model
        .as_deref()
        .and_then(|id| listed.iter().find(|m| m["slug"] == id))
        .map(|m| efforts_of(m))
        .unwrap_or_else(|| levels(&CODEX_EFFORTS));
    let mut models = vec![Model {
        id: None,
        label: default,
        efforts: default_efforts,
    }];
    let mut listed = listed;
    listed.sort_by_key(|m| m["priority"].as_i64().unwrap_or(i64::MAX));
    models.extend(listed.into_iter().map(|m| {
        Model {
            id: m["slug"].as_str().map(str::to_string),
            label: m["display_name"]
                .as_str()
                .or(m["slug"].as_str())
                .unwrap_or_default()
                .to_string(),
            efforts: efforts_of(m),
        }
    }));
    models
}

/// The level Codex runs at when none is picked: the game's own on OpenAI, or whatever the
/// player set for their own endpoint.
pub fn codex_default_effort() -> Option<String> {
    let config = config::codex();
    match config.custom_provider {
        None => Some(super::runner::CODEX_EFFORT.to_string()),
        Some(_) => config.effort,
    }
}

/// The reasoning levels `choice`'s model takes; for a model not in the list (typed in),
/// the usual ones.
pub fn efforts(models: &[Model], choice: &Choice, provider: Provider) -> Vec<String> {
    let found = models.iter().find(|m| m.id == choice.model);
    match found {
        Some(model) => model.efforts.clone(),
        None if provider == Provider::Claude => levels(&CLAUDE_EFFORTS),
        None => levels(&CODEX_EFFORTS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_lists_openais_models_unless_the_provider_is_custom() {
        let cache = json!({ "models": [
            { "slug": "b", "display_name": "B", "priority": 2, "visibility": "list",
              "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "high" }] },
            { "slug": "a", "display_name": "A", "priority": 1, "visibility": "list" },
            { "slug": "hidden", "visibility": "hide" }
        ]});
        let openai = config::Codex {
            model: Some("b".into()),
            ..Default::default()
        };
        let models = codex(&openai, Some(&cache));
        let ids: Vec<_> = models.iter().map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, [None, Some("a"), Some("b")]);
        assert_eq!(models[0].label, "Default (b)");
        assert_eq!(models[0].efforts, ["low", "high"]);
        assert_eq!(models[1].efforts, CODEX_EFFORTS);

        let custom = config::Codex {
            model: Some("qwen".into()),
            custom_provider: Some("qwen".into()),
            ..Default::default()
        };
        let models = codex(&custom, Some(&cache));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].label, "Default (qwen)");
    }

    #[test]
    fn a_choice_reads_as_its_model_and_effort() {
        let models = claude(None);
        let mut choice = Choice::default();
        assert_eq!(choice.label(&models), "Default");
        choice.model = Some("sonnet".into());
        assert_eq!(choice.label(&models), "Sonnet");
        choice.model = Some("claude-custom-1".into());
        assert_eq!(choice.label(&models), "claude-custom-1");
        assert_eq!(efforts(&models, &choice, Provider::Claude), CLAUDE_EFFORTS);
    }
}
