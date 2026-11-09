//! What games put in front of players, as data a script builds: one small tree of
//! [`Ui`] nodes, drawn by one renderer, for both the HUD (pinned to a player's screen)
//! and panels (a dialogue, a shop, a quest log, a scoreboard: anything with buttons).
//! Values and bars show a player's stats and items live, so the HUD keeps itself current
//! without the script redrawing it. A button gives its id to the package's `on_choose`.
//!
//! Older shapes still read: a widget `counter` or `items` is a `value`, and a panel's
//! `text` and `choices` become a column of text and buttons.

use serde::{Deserialize, Serialize};

use super::EntityId;

/// The most nodes one tree may have, and how deep it may nest.
pub const MAX_NODES: usize = 80;
pub const MAX_DEPTH: usize = 6;
/// The most text one tree may hold, over all its nodes.
pub const MAX_TEXT: usize = 3000;
/// The most HUD entries one player may have.
pub const MAX_HUD: usize = 16;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ui {
    /// Words. `style`: "title", "small" or "muted" (plain if none).
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<String>,
    },
    /// A label and a live number: one of the player's stats, or how many of an item they
    /// carry.
    #[serde(alias = "counter", alias = "items")]
    Value {
        #[serde(default)]
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stat: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<String>,
    },
    /// A bar filled to a stat's share of its maximum.
    Bar {
        #[serde(default)]
        label: String,
        stat: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
    },
    /// An item's tile, with how many the player carries.
    Item { item: String },
    /// Pressing it gives `id` to the package's `on_choose` (in a panel).
    Button {
        id: String,
        label: String,
        /// A note beside it ("5 gold").
        #[serde(default, skip_serializing_if = "String::is_empty")]
        detail: String,
    },
    Row { items: Vec<Ui> },
    Column { items: Vec<Ui> },
    Space,
}

impl Ui {
    /// Every node, this one first.
    pub fn nodes(&self) -> Vec<&Ui> {
        let mut out = vec![self];
        if let Ui::Row { items } | Ui::Column { items } = self {
            out.extend(items.iter().flat_map(Ui::nodes));
        }
        out
    }

    fn depth(&self) -> usize {
        match self {
            Ui::Row { items } | Ui::Column { items } => 1 + items.iter().map(Ui::depth).max().unwrap_or(0),
            _ => 1,
        }
    }

    /// Its buttons' ids.
    pub fn buttons(&self) -> Vec<&str> {
        self.nodes().into_iter().filter_map(|n| if let Ui::Button { id, .. } = n { Some(id.as_str()) } else { None }).collect()
    }

    /// The stats and items it shows.
    pub fn names(&self) -> Vec<&str> {
        self.nodes()
            .into_iter()
            .flat_map(|n| match n {
                Ui::Value { stat, item, .. } => [stat.as_deref(), item.as_deref()],
                Ui::Bar { stat, .. } => [Some(stat.as_str()), None],
                Ui::Item { item } => [Some(item.as_str()), None],
                _ => [None, None],
            })
            .flatten()
            .collect()
    }

    /// What's wrong with it, phrased for the agent that wrote it.
    pub fn check(&self) -> Result<(), String> {
        let nodes = self.nodes();
        if nodes.len() > MAX_NODES || self.depth() > MAX_DEPTH {
            return Err(format!("a UI holds at most {MAX_NODES} nodes, {MAX_DEPTH} deep"));
        }
        let mut text = 0;
        for node in nodes {
            match node {
                Ui::Text { text: t, .. } => text += t.chars().count(),
                Ui::Value { label, stat, item } => {
                    text += label.chars().count();
                    if stat.is_some() == item.is_some() {
                        return Err("a value shows a stat or an item: {kind = \"value\", label = \"Gold\", stat = \"gold\"}".into());
                    }
                }
                Ui::Bar { label, .. } => text += label.chars().count(),
                Ui::Button { id, label, detail } => {
                    if id.is_empty() || id.len() > 40 || label.chars().count() > 60 || detail.chars().count() > 40 {
                        return Err("a button has an id (up to 40 characters), a label of up to 60, and a detail of up to 40".into());
                    }
                    text += label.chars().count() + detail.chars().count();
                }
                Ui::Item { .. } | Ui::Row { .. } | Ui::Column { .. } | Ui::Space => {}
            }
        }
        if text > MAX_TEXT {
            return Err(format!("a UI holds at most {MAX_TEXT} characters of text"));
        }
        Ok(())
    }

    /// Its words and buttons on one line, for reports: "Welcome! [room: Rent a room]".
    pub fn summary(&self) -> String {
        self.nodes()
            .into_iter()
            .filter_map(|n| match n {
                Ui::Text { text, .. } => Some(text.clone()),
                Ui::Value { label, stat, item } => Some(format!("{label} ({})", stat.as_deref().or(item.as_deref()).unwrap_or_default())),
                Ui::Bar { label, stat, .. } => Some(format!("{label} [bar {stat}]")),
                Ui::Item { item } => Some(format!("[item {item}]")),
                Ui::Button { id, label, .. } => Some(format!("[{id}: {label}]")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A panel as a package asks for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "PanelWire", into = "PanelWire")]
pub struct PanelSpec {
    /// The package's name for it, given back with the choice made.
    pub id: String,
    pub title: String,
    pub body: Ui,
    /// What it's about (the shopkeeper): where it's open.
    pub entity: Option<EntityId>,
}

/// A panel as written: a `body`, or the older `text` and `choices`.
#[derive(Serialize, Deserialize)]
struct PanelWire {
    id: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<Ui>,
    #[serde(default, skip_serializing)]
    text: String,
    #[serde(default, skip_serializing)]
    choices: Vec<OldChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entity: Option<EntityId>,
}

#[derive(Deserialize)]
struct OldChoice {
    id: String,
    label: String,
    #[serde(default)]
    detail: String,
}

impl From<PanelWire> for PanelSpec {
    fn from(w: PanelWire) -> Self {
        let body = w.body.unwrap_or_else(|| {
            let mut items = Vec::new();
            if !w.text.is_empty() {
                items.push(Ui::Text { text: w.text, style: None });
            }
            items.extend(w.choices.into_iter().map(|c| Ui::Button { id: c.id, label: c.label, detail: c.detail }));
            Ui::Column { items }
        });
        Self { id: w.id, title: w.title, body, entity: w.entity }
    }
}

impl From<PanelSpec> for PanelWire {
    fn from(p: PanelSpec) -> Self {
        Self { id: p.id, title: p.title, body: Some(p.body), text: String::new(), choices: Vec::new(), entity: p.entity }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_widgets_and_panels_still_read() {
        let counter: Ui = serde_json::from_value(json!({ "kind": "counter", "label": "Gold", "stat": "inn:gold" })).unwrap();
        assert_eq!(counter, Ui::Value { label: "Gold".into(), stat: Some("inn:gold".into()), item: None });
        let items: Ui = serde_json::from_value(json!({ "kind": "items", "label": "Keys", "item": "inn:key" })).unwrap();
        assert_eq!(items, Ui::Value { label: "Keys".into(), stat: None, item: Some("inn:key".into()) });
        let panel: PanelSpec = serde_json::from_value(json!({ "id": "inn", "title": "The innkeeper", "text": "Hello",
            "choices": [{ "id": "room", "label": "Rent a room", "detail": "5 gold" }] }))
        .unwrap();
        assert_eq!(panel.body.buttons(), vec!["room"]);
        assert!(panel.body.summary().contains("Hello"));
        // Written back, it's a body; read again, the same.
        let again: PanelSpec = serde_json::from_value(serde_json::to_value(&panel).unwrap()).unwrap();
        assert_eq!(again, panel);
    }

    #[test]
    fn trees_have_limits() {
        let button = |id: &str| Ui::Button { id: id.into(), label: "Buy".into(), detail: String::new() };
        let shop = Ui::Column { items: vec![Ui::Text { text: "Torches".into(), style: Some("title".into()) }, Ui::Row { items: vec![Ui::Item { item: "t".into() }, button("buy")] }] };
        assert!(shop.check().is_ok());
        assert_eq!(shop.buttons(), vec!["buy"]);
        assert_eq!(shop.names(), vec!["t"]);
        let deep = (0..8).fold(Ui::Space, |inner, _| Ui::Row { items: vec![inner] });
        assert!(deep.check().is_err());
        assert!(Ui::Value { label: "x".into(), stat: None, item: None }.check().is_err());
        assert!(button("").check().is_err());
    }
}
