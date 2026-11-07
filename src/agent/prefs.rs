//! The player's own preferences for this game (theme, hints), kept between runs in
//! one small JSON object. Unlike the world's rules, they're nobody else's business.

use serde_json::{Map, Value};

const PATH: &str = "saves/settings.json";

fn load() -> Map<String, Value> {
    std::fs::read_to_string(PATH).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

pub fn get(key: &str) -> Option<Value> {
    load().remove(key)
}

/// Saves one preference, keeping the others.
pub fn set(key: &str, value: impl Into<Value>) {
    let mut all = load();
    all.insert(key.into(), value.into());
    let result = std::fs::create_dir_all("saves").and_then(|_| std::fs::write(PATH, Value::Object(all).to_string()));
    if let Err(e) = result {
        bevy::log::error!("couldn't save {PATH}: {e}");
    }
}
