//! Panels as data: windows of the app described by a small tree of [`Node`]s in a JSON
//! file, drawn here with egui. A panel never runs code. It shows what the game's lists
//! hold (settings, key bindings, worlds, saves, ops) and its buttons run [ops](super::ops),
//! so a panel can do exactly what a player can, under the same checks.
//!
//! This is how the agent adds a window of its own or reshapes one of the app's: it
//! writes `panels/<id>.json` in its folder and opens it. The built-in ones ([`BUILT_IN`])
//! are panels too; a file of the same id replaces one, and deleting the file brings the
//! built-in back. The player's panels are kept in `saves/panels/`.
//!
//! - Layout: `text`, `row`, `column`, `space`, `separator`, `section` (folds), `scroll`,
//!   `tabs`.
//! - Live data: `setting` (the control for one [setting](super::registry)), `keys` (an
//!   action's keys, with add and remove), and `each`, which repeats a node for every row
//!   of a list (`actions`, `settings`, `worlds`, ...), filling `{row.field}` in its text.
//! - Doing: `button` (an op and its arguments) and `input` (text a button's arguments
//!   use as `{input:id}`).

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Chat, Role,
    creations::Creations,
    landscape::Landscape,
    ops,
    registry::{self, SETTINGS, Scope, Stores},
    theme,
    worlds::Travel,
};
use crate::{
    bindings::{Bindings, INPUTS},
    graphics::Graphics,
    player::Player,
    world::{VoxelWorld, ledger::Ledger},
};

/// The most nodes one panel's file may hold, and how deep they may nest.
pub const MAX_NODES: usize = 400;
pub const MAX_DEPTH: usize = 12;
/// The most rows one `each` draws.
const MAX_ROWS: usize = 500;

const DIR: &str = "saves/panels";

/// The app's own panels, as the files they'd be: `(id, JSON)`.
pub const BUILT_IN: &[(&str, &str)] = &[("controls", include_str!("panels/controls.json"))];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    pub title: String,
    /// How wide it opens, in points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    pub body: Node,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub title: String,
    pub items: Vec<Node>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    /// Words. `style`: "title", "small" or "muted". `width` gives it a fixed column to
    /// sit in, so rows line up; `hint` shows when the pointer rests on it.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    Row {
        items: Vec<Node>,
    },
    Column {
        items: Vec<Node>,
    },
    Space,
    Separator,
    /// A heading its items fold under.
    Section {
        title: String,
        #[serde(default = "yes")]
        open: bool,
        items: Vec<Node>,
    },
    /// Its items in a box that scrolls, `height` points tall at most.
    Scroll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<f32>,
        items: Vec<Node>,
    },
    Tabs {
        tabs: Vec<Tab>,
    },
    /// Runs an op with these arguments when pressed. Text in the arguments is filled in
    /// like any text (`{input:name}`, `{world.name}`).
    Button {
        label: String,
        op: String,
        #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
        args: serde_json::Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// A line of text to type, for buttons' arguments: `{input:<id>}`.
    Input {
        id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        hint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<f32>,
    },
    /// The control for one setting, by what it holds: a number to drag, a switch, a
    /// choice. Changing it is the `set` op.
    Setting {
        name: String,
    },
    /// The keys on one action, each a button that takes it off, and a + that binds the
    /// next key pressed.
    Keys {
        action: String,
    },
    /// `item`, once for every row of the list `of`, with the row's fields filled into
    /// its text as `{<name>.<field>}`. `where` keeps the rows whose fields equal these.
    Each {
        of: String,
        #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, rename = "where", skip_serializing_if = "BTreeMap::is_empty")]
        only: BTreeMap<String, String>,
        item: Box<Node>,
    },
}

/// The lists `each` goes over: its name, what a row is called, and a row's fields.
pub const LISTS: &[(&str, &str, &str)] = &[
    ("actions", "action", "name, label, title, group, keys"),
    ("action_groups", "group", "name"),
    (
        "settings",
        "setting",
        "name, label, title, group, scope (world or player), value, default, range",
    ),
    ("setting_groups", "group", "name, scope"),
    (
        "worlds",
        "world",
        "name, ground, here (yes where the player is)",
    ),
    ("saves", "save", "name, change"),
    ("panels", "panel", "id, title"),
    (
        "ops",
        "op",
        "name, usage, about, admin (yes if only admins may)",
    ),
    ("landscapes", "landscape", "name"),
    (
        "packages",
        "package",
        "name, title, on (yes if it runs in this world)",
    ),
    (
        "blocks",
        "block",
        "name, color (#rrggbb), material, kind (own or ground), recolored (yes for a family of the ground the world gave a color)",
    ),
];

impl Node {
    fn children(&self) -> Vec<&Node> {
        match self {
            Node::Row { items }
            | Node::Column { items }
            | Node::Section { items, .. }
            | Node::Scroll { items, .. } => items.iter().collect(),
            Node::Tabs { tabs } => tabs.iter().flat_map(|t| &t.items).collect(),
            Node::Each { item, .. } => vec![item],
            _ => Vec::new(),
        }
    }

    fn count(&self) -> usize {
        1 + self.children().iter().map(|c| c.count()).sum::<usize>()
    }

    fn depth(&self) -> usize {
        1 + self.children().iter().map(|c| c.depth()).max().unwrap_or(0)
    }

    /// What's wrong with it, phrased for whoever wrote it.
    fn check(&self) -> Result<(), String> {
        // A name with `{...}` in it is filled in when it's drawn: checked then.
        let fixed = |text: &str| !text.contains('{');
        match self {
            Node::Button { op, label, .. } if !ops::OPS.iter().any(|o| o.name == op) => {
                return Err(format!(
                    "the button \"{label}\" runs \"{op}\", which isn't an op (the ops list has them: set, bind, newworld, ...)"
                ));
            }
            Node::Setting { name } if fixed(name) => {
                registry::find(name)?;
            }
            // A game's action has a package's name before it, and is there while its game is on.
            Node::Keys { action }
                if fixed(action)
                    && !action.contains(':')
                    && crate::bindings::action_named(action).is_none() =>
            {
                return Err(format!("keys: there's no action \"{action}\""));
            }
            Node::Each { of, .. } if !LISTS.iter().any(|(list, _, _)| list == of) => {
                return Err(format!(
                    "each: there's no list \"{of}\" ({})",
                    LISTS.iter().map(|l| l.0).collect::<Vec<_>>().join(", ")
                ));
            }
            Node::Text {
                style: Some(style), ..
            } if !["title", "small", "muted"].contains(&style.as_str()) => {
                return Err(format!(
                    "a text's style is title, small or muted, not \"{style}\""
                ));
            }
            _ => {}
        }
        self.children().into_iter().try_for_each(Node::check)
    }
}

impl Panel {
    /// Reads a panel's file, saying what's wrong with it if it isn't one.
    pub fn read(text: &str) -> Result<Self, String> {
        let panel: Panel =
            serde_json::from_str(text).map_err(|e| format!("it isn't a panel: {e}"))?;
        if panel.body.count() > MAX_NODES || panel.body.depth() > MAX_DEPTH {
            return Err(format!(
                "a panel holds at most {MAX_NODES} nodes, {MAX_DEPTH} deep (use each to repeat rows)"
            ));
        }
        panel.body.check()?;
        Ok(panel)
    }
}

/// A panel's id from its file name: letters, digits, - and _.
fn id_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.strip_suffix(".json")?;
    (!name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then(|| name.to_lowercase())
}

fn files_in(dir: &Path) -> BTreeMap<String, String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|e| Some((id_of(&e.path())?, std::fs::read_to_string(e.path()).ok()?)))
        .collect()
}

/// Every panel there is, as its file's text: the built-in ones, with the player's own
/// over them.
pub fn sources() -> BTreeMap<String, String> {
    let mut all: BTreeMap<String, String> = BUILT_IN
        .iter()
        .map(|(id, text)| (id.to_string(), text.to_string()))
        .collect();
    all.extend(files_in(Path::new(DIR)));
    all
}

fn same_json(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<Value>(a),
        serde_json::from_str::<Value>(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Takes the panels the agent wrote in its folder (`<folder>/panels/*.json`) as the
/// player's: new and changed ones are kept, one put back as the built-in stops
/// replacing it, and one deleted goes. Returns what happened, and what was wrong with
/// the ones that aren't panels (those are left as they were).
pub fn take_from(folder: &Path) -> (Vec<String>, Vec<String>) {
    let (mut done, mut wrong) = (Vec::new(), Vec::new());
    if !folder.join("panels").is_dir() {
        return (done, wrong);
    }
    // A file emptied is a file deleted: the agent's tools write files, but don't remove them.
    let written: BTreeMap<String, String> = files_in(&folder.join("panels"))
        .into_iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .collect();
    let kept = files_in(Path::new(DIR));
    let built_in = |id: &str| {
        BUILT_IN
            .iter()
            .find(|(b, _)| *b == id)
            .map(|(_, text)| *text)
    };
    let path = |id: &str| PathBuf::from(DIR).join(format!("{id}.json"));
    for (id, text) in &written {
        let before = kept.get(id).map(String::as_str).or(built_in(id));
        if before.is_some_and(|before| same_json(before, text)) {
            continue;
        }
        if built_in(id).is_some_and(|b| same_json(b, text)) {
            let _ = std::fs::remove_file(path(id));
            done.push(format!("The panel \"{id}\" is the app's own again."));
            continue;
        }
        match Panel::read(text) {
            Ok(panel) => {
                match std::fs::create_dir_all(DIR).and_then(|_| std::fs::write(path(id), text)) {
                    Ok(()) => done.push(format!(
                        "Kept the panel \"{id}\" ({}): /panel {id} opens it.",
                        panel.title
                    )),
                    Err(e) => wrong.push(format!("panels/{id}.json couldn't be kept: {e}")),
                }
            }
            Err(why) => wrong.push(format!("panels/{id}.json: {why}")),
        }
    }
    for id in kept.keys().filter(|id| !written.contains_key(*id)) {
        let _ = std::fs::remove_file(path(id));
        done.push(match built_in(id) {
            Some(_) => format!("The panel \"{id}\" is the app's own again."),
            None => format!("Removed the panel \"{id}\"."),
        });
    }
    (done, wrong)
}

/// Removes the player's panel `id` (and the agent's copy of it in `folder`): one of
/// their own goes, one in place of the app's gives way to it again.
pub fn remove(id: &str, folder: Option<&Path>) -> Result<String, String> {
    let id = id.trim().to_lowercase();
    let kept = Path::new(DIR).join(format!("{id}.json"));
    let built_in = BUILT_IN
        .iter()
        .find(|(b, _)| *b == id)
        .map(|(_, text)| *text);
    let copy = folder.map(|folder| folder.join("panels").join(format!("{id}.json")));
    let had_copy = copy.as_ref().is_some_and(|copy| copy.exists());
    if !kept.exists() && (!had_copy || built_in.is_some()) {
        return Err(match built_in {
            Some(_) => format!(
                "The panel \"{id}\" is the app's own: it can be changed (write panels/{id}.json), not removed."
            ),
            None => format!("There's no panel \"{id}\"."),
        });
    }
    let _ = std::fs::remove_file(&kept);
    // The agent's folder shows what's there now: the app's own again, or nothing.
    if let Some(copy) = copy {
        let _ = match built_in {
            Some(text) => std::fs::write(&copy, text),
            None => std::fs::remove_file(&copy),
        };
    }
    Ok(match built_in {
        Some(_) => format!("The panel \"{id}\" is the app's own again."),
        None => format!("Removed the panel \"{id}\"."),
    })
}

/// What a panel remembers while it's open.
#[derive(Default)]
struct State {
    inputs: HashMap<String, String>,
    tabs: HashMap<String, usize>,
    /// The action waiting for a key press.
    capturing: Option<String>,
    /// Settings being dragged: applied when the pointer lets go.
    drafts: HashMap<&'static str, Value>,
}

/// The panels there are, and the ones that are open.
#[derive(Resource, Default)]
pub struct Panels {
    loaded: BTreeMap<String, Arc<Result<Panel, String>>>,
    open: Vec<String>,
    state: HashMap<String, State>,
    /// Ops asked for from outside a panel (the game menu), run with the panels' next.
    queued: Vec<Asked>,
}

impl Panels {
    /// Reads the panels again (they changed on disk).
    pub fn reload(&mut self) {
        self.loaded = sources()
            .into_iter()
            .map(|(id, text)| (id, Arc::new(Panel::read(&text))))
            .collect();
        self.open.retain(|id| self.loaded.contains_key(id));
    }

    /// Opens the panel `id`, or says why it can't.
    pub fn open(&mut self, id: &str) -> Result<String, String> {
        let id = id.trim().to_lowercase();
        let panel = self
            .loaded
            .get(&id)
            .ok_or_else(|| format!("There's no panel \"{id}\". {}", self.list()))?;
        let title = match panel.as_ref() {
            Ok(panel) => panel.title.clone(),
            Err(why) => return Err(format!("The panel \"{id}\" can't be shown: {why}")),
        };
        if !self.open.contains(&id) {
            self.open.push(id.clone());
        }
        Ok(format!("Opened \"{title}\"."))
    }

    pub fn close(&mut self, id: &str) -> Result<String, String> {
        let id = id.trim().to_lowercase();
        if !self.open.contains(&id) {
            return Err(format!("The panel \"{id}\" isn't open."));
        }
        self.open.retain(|open| *open != id);
        self.state.remove(&id);
        Ok(format!("Closed \"{id}\"."))
    }

    /// Runs `op` as the player next frame, as a panel's button would.
    pub fn ask(&mut self, op: &str, args: Value) {
        self.queued.push((op.to_string(), args));
    }

    pub fn any_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// Closes every open panel, as Esc does.
    pub fn close_all(&mut self) {
        self.open.clear();
        self.state.clear();
    }

    /// Every panel, in a line.
    pub fn list(&self) -> String {
        let panels: Vec<String> = self
            .loaded
            .iter()
            .map(|(id, panel)| {
                let title = panel
                    .as_ref()
                    .as_ref()
                    .map_or("can't be shown".to_string(), |p| p.title.clone());
                let whose = match (
                    BUILT_IN.iter().any(|(b, _)| b == id),
                    Path::new(DIR).join(format!("{id}.json")).exists(),
                ) {
                    (true, false) => "the app's",
                    (true, true) => "yours, in place of the app's",
                    _ => "yours",
                };
                format!(
                    "{id} (\"{title}\", {whose}{})",
                    if self.open.contains(id) { ", open" } else { "" }
                )
            })
            .collect();
        format!("Panels: {}. /panel <id> opens one.", panels.join(" · "))
    }
}

/// Reads the panels at start; `GENVERSE_PANEL=<id>` opens one.
pub fn load_panels(mut panels: ResMut<Panels>) {
    panels.reload();
    if let Ok(id) = std::env::var("GENVERSE_PANEL") {
        let _ = panels.open(&id);
    }
}

/// What drawing a panel reads from.
struct Shown<'a> {
    binds: &'a Bindings,
    stores: Stores<'a>,
    ledger: &'a Ledger,
    panels: &'a [(String, String)],
    admin: bool,
}

/// The rows a list holds now, with what a row is called.
fn rows(of: &str, shown: &Shown) -> (&'static str, Vec<BTreeMap<&'static str, String>>) {
    let row =
        |fields: &[(&'static str, String)]| fields.iter().cloned().collect::<BTreeMap<_, _>>();
    let spaced = |name: &str| name.rsplit('.').next().unwrap_or(name).replace('_', " ");
    match of {
        // The app's actions, and those of the games on in this world.
        "actions" => (
            "action",
            shown
                .binds
                .all()
                .into_iter()
                .map(|a| {
                    let (label, keys) = (
                        spaced(a.name.rsplit(':').next().unwrap_or(&a.name)),
                        shown.binds.label(&a.name),
                    );
                    row(&[
                        ("name", a.name),
                        ("label", label),
                        ("title", a.title),
                        ("group", a.group),
                        ("keys", keys),
                    ])
                })
                .collect(),
        ),
        "action_groups" => {
            let mut groups: Vec<String> = Vec::new();
            shown.binds.all().into_iter().for_each(|a| {
                if !groups.contains(&a.group) {
                    groups.push(a.group)
                }
            });
            (
                "group",
                groups.into_iter().map(|g| row(&[("name", g)])).collect(),
            )
        }
        "settings" => (
            "setting",
            SETTINGS
                .iter()
                .map(|s| {
                    row(&[
                        ("name", s.name.into()),
                        ("label", spaced(s.name)),
                        ("title", s.title.into()),
                        ("group", s.group().into()),
                        (
                            "scope",
                            if s.scope() == Scope::World {
                                "world"
                            } else {
                                "player"
                            }
                            .into(),
                        ),
                        (
                            "value",
                            registry::show(s, &registry::value(s, &shown.stores)),
                        ),
                        ("default", registry::show(s, &registry::default(s))),
                        ("range", registry::range(s)),
                    ])
                })
                .collect(),
        ),
        "setting_groups" => {
            let mut groups: Vec<(&str, Scope)> = Vec::new();
            SETTINGS.iter().for_each(|s| {
                if !groups.iter().any(|(g, _)| *g == s.group()) {
                    groups.push((s.group(), s.scope()))
                }
            });
            (
                "group",
                groups
                    .into_iter()
                    .map(|(g, scope)| {
                        row(&[
                            ("name", g.into()),
                            (
                                "scope",
                                if scope == Scope::World {
                                    "world"
                                } else {
                                    "player"
                                }
                                .into(),
                            ),
                        ])
                    })
                    .collect(),
            )
        }
        "worlds" => {
            let here = super::worlds::current_title(shown.ledger);
            let worlds: Vec<(String, String)> = match &shown.ledger.online {
                Some(online) => online
                    .game
                    .as_ref()
                    .map(|g| g.worlds())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(n, g)| (n, if g.flat { "flat" } else { "natural" }.to_string()))
                    .collect(),
                None => crate::world::ledger::list_worlds()
                    .into_iter()
                    .map(|w| (w.name, if w.flat { "flat" } else { "natural" }.to_string()))
                    .collect(),
            };
            (
                "world",
                worlds
                    .into_iter()
                    .map(|(name, ground)| {
                        row(&[
                            (
                                "here",
                                if here.starts_with(&name) { "yes" } else { "" }.into(),
                            ),
                            ("name", name),
                            ("ground", ground),
                        ])
                    })
                    .collect(),
            )
        }
        "saves" => {
            let mut seen = std::collections::HashSet::new();
            let saves = shown
                .ledger
                .log
                .saves()
                .rev()
                .filter(|(_, name)| seen.insert(name.to_lowercase()));
            (
                "save",
                saves
                    .map(|(change, name)| {
                        row(&[("name", name.into()), ("change", change.id.to_string())])
                    })
                    .collect(),
            )
        }
        "panels" => (
            "panel",
            shown
                .panels
                .iter()
                .map(|(id, title)| row(&[("id", id.clone()), ("title", title.clone())]))
                .collect(),
        ),
        "ops" => (
            "op",
            ops::OPS
                .iter()
                .map(|op| {
                    row(&[
                        ("name", op.name.into()),
                        ("usage", op.args.into()),
                        ("about", op.about.into()),
                        ("admin", if op.admin { "yes" } else { "" }.into()),
                    ])
                })
                .collect(),
        ),
        "landscapes" => (
            "landscape",
            std::iter::once("flat")
                .chain(genverse_rules::recipe::Recipe::NAMES)
                .map(|name| row(&[("name", name.into())]))
                .collect(),
        ),
        "packages" => {
            let logic = &shown.ledger.logic;
            let on = logic.installed.values().map(|i| {
                (
                    i.package.name.clone(),
                    logic
                        .package(i)
                        .map_or(i.package.name.clone(), |p| p.manifest.title.clone()),
                    "yes",
                )
            });
            let off = ops::catalog(shown.ledger)
                .into_iter()
                .filter(|(name, _)| !logic.installed.contains_key(name))
                .map(|(name, package)| (name, package.manifest.title.clone(), ""));
            (
                "package",
                on.chain(off)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|(name, title, on)| {
                        row(&[("name", name), ("title", title), ("on", on.into())])
                    })
                    .collect(),
            )
        }
        "blocks" => {
            let content = &shown.ledger.rules.content;
            let palette = content.palette(shown.ledger.log.terrain());
            let own = content.own().map(|(_, b)| {
                row(&[
                    ("name", b.name.clone()),
                    ("color", b.hex.clone()),
                    ("material", b.material.clone()),
                    ("kind", "own".into()),
                    ("recolored", String::new()),
                ])
            });
            let ground = genverse_rules::recipe::FAMILIES
                .iter()
                .map(|(family, middle, _)| {
                    let hex = palette
                        .find(middle)
                        .map(|id| {
                            bevy::color::Srgba::from(palette.entry(id).color)
                                .to_hex()
                                .to_lowercase()
                        })
                        .unwrap_or_default();
                    let recolored = if content.colors.contains_key(*family) {
                        "yes"
                    } else {
                        ""
                    };
                    row(&[
                        ("name", family.to_string()),
                        ("color", hex.chars().take(7).collect()),
                        ("material", "matte".into()),
                        ("kind", "ground".into()),
                        ("recolored", recolored.into()),
                    ])
                });
            ("block", own.chain(ground).collect())
        }
        _ => ("row", Vec::new()),
    }
}

/// The rows' fields in reach while drawing: `(name.field, value)`, innermost last.
type Vars = Vec<(String, String)>;

/// `text` with every `{...}` it can fill filled in: a row's field (`{action.name}`), what
/// was typed (`{input:id}`), a setting's value (`{setting:hero.run_speed}`), an action's
/// keys (`{keys:jump}`), or `{world}`, `{game}`, `{player}`.
fn fill(text: &str, vars: &Vars, shown: &Shown, state: &State) -> String {
    if !text.contains('{') {
        return text.to_string();
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            // An open brace with no close is just text.
            rest = &rest[start..];
            break;
        };
        let key = &rest[start + 1..start + end];
        let value = vars
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .or_else(|| match key.split_once(':') {
                Some(("input", id)) => Some(state.inputs.get(id).cloned().unwrap_or_default()),
                Some(("setting", name)) => registry::find(name)
                    .ok()
                    .map(|s| registry::show(s, &registry::value(s, &shown.stores))),
                Some(("keys", action)) => Some(shown.binds.label(action)),
                _ => match key {
                    "world" => Some(super::worlds::current_title(shown.ledger)),
                    "game" => Some(crate::world::ledger::game_name()),
                    "player" => Some(shown.ledger.profile.name.clone()),
                    _ => None,
                },
            });
        match value {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..=start + end]),
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// A JSON value with its text filled in, all the way down.
fn fill_value(value: &Value, vars: &Vars, shown: &Shown, state: &State) -> Value {
    match value {
        Value::String(text) => Value::String(fill(text, vars, shown, state)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| fill_value(v, vars, shown, state))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), fill_value(v, vars, shown, state)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// An op a panel asked for.
type Asked = (String, Value);

fn draw_all(
    ui: &mut egui::Ui,
    items: &[Node],
    vars: &mut Vars,
    shown: &Shown,
    state: &mut State,
    asked: &mut Vec<Asked>,
    path: &str,
) {
    for (i, node) in items.iter().enumerate() {
        draw(ui, node, vars, shown, state, asked, &format!("{path}/{i}"));
    }
}

/// Draws one node. `path` is where it is in the tree and its rows, for egui's ids.
fn draw(
    ui: &mut egui::Ui,
    node: &Node,
    vars: &mut Vars,
    shown: &Shown,
    state: &mut State,
    asked: &mut Vec<Asked>,
    path: &str,
) {
    let p = theme::palette(ui.ctx());
    match node {
        Node::Text {
            text,
            style,
            width,
            hint,
        } => {
            let text = fill(text, vars, shown, state);
            let rich = match style.as_deref() {
                Some("title") => egui::RichText::new(text)
                    .size(15.0)
                    .strong()
                    .color(p.strong),
                Some("small") => egui::RichText::new(text).size(11.5).color(p.text),
                Some("muted") => egui::RichText::new(text).size(11.5).color(p.muted),
                _ => egui::RichText::new(text).color(p.text),
            };
            let hint = hint.as_ref().map(|hint| fill(hint, vars, shown, state));
            let label = |ui: &mut egui::Ui| {
                let response = ui.label(rich);
                if let Some(hint) = hint {
                    response.on_hover_text(hint);
                }
            };
            match width {
                Some(width) => {
                    ui.allocate_ui_with_layout(
                        egui::vec2(*width, 18.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_min_width(*width);
                            label(ui);
                        },
                    );
                }
                None => label(ui),
            }
        }
        Node::Row { items } => {
            ui.horizontal_wrapped(|ui| draw_all(ui, items, vars, shown, state, asked, path));
        }
        Node::Column { items } => {
            ui.vertical(|ui| draw_all(ui, items, vars, shown, state, asked, path));
        }
        Node::Space => ui.add_space(8.0),
        Node::Separator => {
            ui.separator();
        }
        Node::Section { title, open, items } => {
            let title = fill(title, vars, shown, state);
            egui::CollapsingHeader::new(egui::RichText::new(title).strong().color(p.strong))
                .id_salt(path)
                .default_open(*open)
                .show(ui, |ui| {
                    draw_all(ui, items, vars, shown, state, asked, path)
                });
        }
        Node::Scroll { height, items } => {
            egui::ScrollArea::vertical()
                .id_salt(path)
                .max_height(height.unwrap_or(240.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    draw_all(ui, items, vars, shown, state, asked, path)
                });
        }
        Node::Tabs { tabs } => {
            let mut at = state
                .tabs
                .get(path)
                .copied()
                .unwrap_or(0)
                .min(tabs.len().saturating_sub(1));
            ui.horizontal(|ui| {
                for (i, tab) in tabs.iter().enumerate() {
                    ui.selectable_value(&mut at, i, fill(&tab.title, vars, shown, state));
                }
            });
            ui.separator();
            state.tabs.insert(path.to_string(), at);
            if let Some(tab) = tabs.get(at) {
                draw_all(
                    ui,
                    &tab.items,
                    vars,
                    shown,
                    state,
                    asked,
                    &format!("{path}/tab{at}"),
                );
            }
        }
        Node::Button {
            label,
            op,
            args,
            hint,
        } => {
            let allowed = shown.admin || !ops::OPS.iter().any(|o| o.name == op && o.admin);
            let mut response =
                ui.add_enabled(allowed, egui::Button::new(fill(label, vars, shown, state)));
            if let Some(hint) = hint {
                response = response.on_hover_text(fill(hint, vars, shown, state));
            }
            if !allowed {
                response = response.on_disabled_hover_text("Only admins of this game can do that.");
            }
            if response.clicked() {
                asked.push((
                    op.clone(),
                    fill_value(&Value::Object(args.clone()), vars, shown, state),
                ));
            }
        }
        Node::Input { id, hint, width } => {
            let hint = fill(hint, vars, shown, state);
            let text = state.inputs.entry(id.clone()).or_default();
            ui.add(
                egui::TextEdit::singleline(text)
                    .hint_text(hint)
                    .desired_width(width.unwrap_or(160.0)),
            );
        }
        Node::Setting { name } => {
            let name = fill(name, vars, shown, state);
            let Ok(setting) = registry::find(&name) else {
                ui.label(egui::RichText::new(format!("no setting \"{name}\"")).color(p.danger));
                return;
            };
            let now = registry::value(setting, &shown.stores);
            let showing = state
                .drafts
                .get(setting.name)
                .cloned()
                .unwrap_or_else(|| now.clone());
            let may = setting.scope() == Scope::Player || shown.admin;
            let mut changed = None;
            ui.push_id(path, |ui| {
                ui.add_enabled_ui(may, |ui| {
                    changed = super::settings::edit(ui, setting, &showing)
                });
            });
            if let Some(value) = changed {
                state.drafts.insert(setting.name, value);
            }
            // Applied when the pointer lets go, so dragging a number is one change.
            if !ui.input(|i| i.pointer.any_down())
                && let Some(value) = state.drafts.remove(setting.name)
                && value != now
            {
                asked.push((
                    "set".into(),
                    json!({ "name": setting.name, "value": registry::show(setting, &value) }),
                ));
            }
        }
        Node::Keys { action } => {
            let action = fill(action, vars, shown, state);
            for input in shown.binds.inputs(&action) {
                if ui
                    .small_button(format!("{} {}", input.label, icon::X))
                    .on_hover_text("Take this key off")
                    .clicked()
                {
                    asked.push((
                        "unbind".into(),
                        json!({ "action": action, "key": input.name }),
                    ));
                }
            }
            if state.capturing.as_deref() == Some(action.as_str()) {
                ui.label(
                    egui::RichText::new("press a key… (Esc cancels)")
                        .small()
                        .color(p.primary),
                );
            } else if ui
                .small_button(icon::PLUS)
                .on_hover_text("Add a key")
                .clicked()
            {
                state.capturing = Some(action.clone());
            }
            let usual = shown.binds.is_usual(&action);
            if !usual
                && ui
                    .small_button(icon::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Back to its default")
                    .clicked()
            {
                asked.push(("resetkeys".into(), json!({ "action": action })));
            }
        }
        Node::Each {
            of,
            name,
            only,
            item,
        } => {
            let (row_name, rows) = rows(of, shown);
            let name = name.as_deref().unwrap_or(row_name);
            let only: Vec<(&String, String)> = only
                .iter()
                .map(|(field, value)| (field, fill(value, vars, shown, state)))
                .collect();
            let kept = rows.iter().filter(|row| {
                only.iter()
                    .all(|(field, value)| row.get(field.as_str()) == Some(value))
            });
            for (i, row) in kept.take(MAX_ROWS).enumerate() {
                let before = vars.len();
                vars.extend(
                    row.iter()
                        .map(|(field, value)| (format!("{name}.{field}"), value.clone())),
                );
                draw(ui, item, vars, shown, state, asked, &format!("{path}#{i}"));
                vars.truncate(before);
            }
        }
    }
}

/// Draws the open panels as windows, and does what their buttons asked for.
pub fn panel_windows(
    mut contexts: EguiContexts,
    mut panels: ResMut<Panels>,
    mut chat: ResMut<Chat>,
    (mut ledger, mut world, mut creations, mut landscape, mut travel): (
        ResMut<Ledger>,
        ResMut<VoxelWorld>,
        ResMut<Creations>,
        ResMut<Landscape>,
        ResMut<Travel>,
    ),
    (mut binds, mut graphics): (ResMut<Bindings>, ResMut<Graphics>),
    (keys, buttons): (Res<ButtonInput<KeyCode>>, Res<ButtonInput<MouseButton>>),
    players: Query<&Transform, With<Player>>,
) -> Result {
    if panels.open.is_empty() && panels.queued.is_empty() {
        return Ok(());
    }
    let mut asked: Vec<Asked> = std::mem::take(&mut panels.queued);
    // A panel waiting for a key: the next one pressed is bound (Esc cancels). A left
    // click is how the panel itself is used, so it's bound from the chat instead.
    let waiting: Vec<(String, String)> = panels
        .state
        .iter()
        .filter_map(|(id, state)| Some((id.clone(), state.capturing.clone()?)))
        .collect();
    for (id, action) in &waiting {
        let pressed = INPUTS
            .iter()
            .find(|input| input.name != "mouse_left" && input.just_pressed(&keys, &buttons));
        if keys.just_pressed(KeyCode::Escape) || pressed.is_some() {
            if let Some(input) = pressed {
                asked.push((
                    "bind".into(),
                    json!({ "action": action, "key": input.name }),
                ));
            }
            if let Some(state) = panels.state.get_mut(id) {
                state.capturing = None;
            }
        }
    }
    binds.capturing = panels.state.values().any(|state| state.capturing.is_some());

    let ctx = contexts.ctx_mut()?;
    let titles: Vec<(String, String)> = panels
        .loaded
        .iter()
        .filter_map(|(id, panel)| Some((id.clone(), panel.as_ref().as_ref().ok()?.title.clone())))
        .collect();
    let mut closed = Vec::new();
    for id in panels.open.clone() {
        let Some(panel) = panels.loaded.get(&id).cloned() else {
            continue;
        };
        let state = panels.state.entry(id.clone()).or_default();
        let shown = Shown {
            binds: &binds,
            stores: Stores {
                rules: &ledger.rules,
                graphics: &graphics,
            },
            ledger: &ledger,
            panels: &titles,
            admin: ledger.land.is_admin(&ledger.profile.id),
        };
        let mut open = true;
        let (title, width) = match panel.as_ref() {
            Ok(panel) => (panel.title.clone(), panel.width.unwrap_or(380.0)),
            Err(_) => (id.clone(), 380.0),
        };
        theme::centered(egui::Window::new(title), ctx, 0.0)
            .id(egui::Id::new(("panel", &id)))
            .collapsible(false)
            .open(&mut open)
            .default_width(width.clamp(200.0, 900.0))
            .default_height(520.0)
            .vscroll(true)
            .show(ctx, |ui| match panel.as_ref() {
                Ok(panel) => draw(
                    ui,
                    &panel.body,
                    &mut Vec::new(),
                    &shown,
                    state,
                    &mut asked,
                    &id,
                ),
                Err(why) => {
                    ui.label(
                        egui::RichText::new(format!("This panel can't be shown: {why}"))
                            .color(theme::palette(ui.ctx()).danger),
                    );
                }
            });
        if !open {
            closed.push(id);
        }
    }
    for id in closed {
        let _ = panels.close(&id);
    }
    binds.capturing = panels.state.values().any(|state| state.capturing.is_some());

    // What the buttons asked for, as the player, like a chat command.
    for (op, args) in asked {
        let cx = &mut ops::Context {
            ledger: &mut ledger,
            world: &mut world,
            creations: &mut creations,
            landscape: &mut landscape,
            travel: &mut travel,
            at: players
                .single()
                .map_or([0.0; 3], |t| t.translation.to_array()),
            agent: None,
            binds: &mut binds,
            graphics: &mut graphics,
            panels: &mut panels,
        };
        let reply = ops::said(ops::run(&op, args, cx));
        info!("panel: {op} -> {reply}");
        // Settings and keys show themselves in the panel; the rest say what they did.
        if !reply.is_empty() && !["set", "bind", "unbind", "resetkeys"].contains(&op.as_str()) {
            chat.say(Role::System, reply);
        }
    }
    Ok(())
}

/// What a panel file is, for the agent.
pub const DOC: &str = r##"# Panels

A panel is a window of the app described by a JSON file: `panels/<id>.json` in your
folder. Write one to add a window of your own; edit one that's there to change it (the
app's own are there too: `controls.json` is the key bindings window). Then open it with
the `panel` tool, which also tells you if the file isn't a valid panel. The
`removepanel` tool removes one of yours, or brings the app's own back. A panel is data: it shows what the
game's lists hold and its buttons run ops, so it can do what the player can, no more.

    {"title": "Speed", "width": 320, "body": {"kind": "column", "items": [
      {"kind": "text", "text": "How fast everyone runs here", "style": "muted"},
      {"kind": "row", "items": [
        {"kind": "text", "text": "run speed", "width": 110},
        {"kind": "setting", "name": "hero.run_speed"}]},
      {"kind": "button", "label": "Back to normal", "op": "set",
       "args": {"name": "hero.run_speed", "value": "default"}}]}}

Nodes ("kind"):
- text {text, style?: "title"|"small"|"muted", width?, hint?}: words. width (points)
  gives it a fixed column so rows line up; hint shows on hover.
- row {items}, column {items}, space, separator.
- section {title, open?: true, items}: a heading its items fold under.
- scroll {height?, items}: a box that scrolls.
- tabs {tabs: [{title, items}]}.
- button {label, op, args?, hint?}: runs an op (the same ones as your tools and the
  chat's commands: set, bind, newworld, world, save, panel, ...) with these arguments.
  A button for an admins' op is greyed out for others.
- input {id, hint?, width?}: a line to type; a button's args use it as "{input:<id>}".
- setting {name}: the control for one setting (SETTINGS.md): a number to drag, a
  switch or a choice. Changing it is the set op.
- keys {action}: the keys on one action, each a button that takes it off, with a + that
  binds the next key pressed.
- each {of, as?, where?, item}: draws item once for every row of a list, with the row's
  fields filled into its text as "{<row>.<field>}". where keeps the rows whose fields
  equal the values given.

Lists for each (what a row is called: its fields):
- actions (action: name, label, title, group, keys)
- action_groups (group: name)
- settings (setting: name, label, title, group, scope, value, default, range)
- setting_groups (group: name, scope)
- worlds (world: name, ground, here)
- saves (save: name, change)
- panels (panel: id, title)
- ops (op: name, usage, about, admin)

Text anywhere (labels, hints, args, names) also fills in "{input:<id>}",
"{setting:<name>}" (its value now), "{keys:<action>}", "{world}", "{game}" and
"{player}".

A list of every world with a button to go there:

    {"kind": "each", "of": "worlds", "item": {"kind": "row", "items": [
      {"kind": "text", "text": "{world.name} ({world.ground})", "width": 160},
      {"kind": "button", "label": "Go", "op": "world", "args": {"name": "{world.name}"}}]}}

Limits: 400 nodes in a file, 12 deep; each repeats, so lists cost one node.
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_apps_own_panels_are_panels() {
        for (id, text) in BUILT_IN {
            Panel::read(text).unwrap_or_else(|why| panic!("{id}: {why}"));
        }
        // The examples the agent is given are panels too.
        let speed = r#"{"title": "Speed", "width": 320, "body": {"kind": "column", "items": [
            {"kind": "text", "text": "How fast everyone runs here", "style": "muted"},
            {"kind": "row", "items": [{"kind": "text", "text": "run speed", "width": 110}, {"kind": "setting", "name": "hero.run_speed"}]},
            {"kind": "button", "label": "Back to normal", "op": "set", "args": {"name": "hero.run_speed", "value": "default"}}]}}"#;
        assert!(Panel::read(speed).is_ok());
        assert!(DOC.contains("hero.run_speed"));
    }

    #[test]
    fn what_isnt_a_panel_says_why() {
        let with = |body: &str| Panel::read(&format!(r#"{{"title": "T", "body": {body}}}"#));
        assert!(
            with(r#"{"kind": "button", "label": "Go", "op": "world", "args": {"name": "home"}}"#)
                .is_ok()
        );
        assert!(
            with(r#"{"kind": "button", "label": "Go", "op": "launch_rocket"}"#)
                .unwrap_err()
                .contains("isn't an op")
        );
        assert!(with(r#"{"kind": "setting", "name": "hero.wingspan"}"#).is_err());
        assert!(
            with(r#"{"kind": "keys", "action": "fly"}"#)
                .unwrap_err()
                .contains("no action")
        );
        assert!(
            with(r#"{"kind": "each", "of": "planets", "item": {"kind": "space"}}"#)
                .unwrap_err()
                .contains("no list")
        );
        assert!(
            with(r#"{"kind": "text", "text": "hi", "colour": "red"}"#).is_err(),
            "a field it doesn't have"
        );
        assert!(
            with(r#"{"kind": "script", "code": "os.exit()"}"#).is_err(),
            "a panel never runs code"
        );
        // A name filled in from a row is checked when it's drawn, not here.
        assert!(with(r#"{"kind": "each", "of": "actions", "item": {"kind": "keys", "action": "{action.name}"}}"#).is_ok());
        // Too deep.
        let deep = (0..MAX_DEPTH + 1).fold(r#"{"kind": "space"}"#.to_string(), |inner, _| {
            format!(r#"{{"kind": "column", "items": [{inner}]}}"#)
        });
        assert!(with(&deep).unwrap_err().contains("at most"));
    }

    #[test]
    fn text_is_filled_from_rows_inputs_settings_and_keys() {
        let (binds, rules, graphics) = (
            Bindings::default(),
            genverse_rules::settings::WorldRules::default(),
            Graphics::default(),
        );
        let ledger = Ledger::for_tests();
        let shown = Shown {
            binds: &binds,
            stores: Stores {
                rules: &rules,
                graphics: &graphics,
            },
            ledger: &ledger,
            panels: &[],
            admin: true,
        };
        let mut state = State::default();
        state.inputs.insert("name".into(), "dungeon".into());
        let vars = vec![
            ("action.name".to_string(), "jump".to_string()),
            ("group.name".to_string(), "outer".to_string()),
            ("group.name".to_string(), "inner".to_string()),
        ];
        let fill = |text: &str| fill(text, &vars, &shown, &state);
        assert_eq!(fill("{action.name} is on {keys:jump}"), "jump is on Space");
        assert_eq!(fill("{group.name}"), "inner", "the innermost row wins");
        assert_eq!(
            fill("go to {input:name}; run {setting:hero.run_speed}"),
            "go to dungeon; run 7"
        );
        assert_eq!(
            fill("{nothing.here} stays, and so does an open {brace"),
            "{nothing.here} stays, and so does an open {brace"
        );
        let args = fill_value(
            &json!({"name": "{input:name}", "n": 3, "list": ["{action.name}"]}),
            &vars,
            &shown,
            &state,
        );
        assert_eq!(args, json!({"name": "dungeon", "n": 3, "list": ["jump"]}));

        // The lists hold what the game's tables hold.
        let (name, actions) = rows("actions", &shown);
        assert_eq!(
            (name, actions.len()),
            ("action", crate::bindings::ACTIONS.len())
        );
        assert!(actions.iter().any(|row| row["name"] == "jump"
            && row["keys"] == "Space"
            && row["group"] == "Moving"));
        let (_, settings) = rows("settings", &shown);
        assert!(settings.iter().any(|row| row["name"] == "hero.run_speed"
            && row["value"] == "7"
            && row["scope"] == "world"));
        for (list, row, fields) in LISTS {
            let (called, rows) = rows(list, &shown);
            assert_eq!(called, *row, "{list}");
            if let Some(first) = rows.first() {
                for field in fields.split(", ").map(|f| f.split(' ').next().unwrap_or(f)) {
                    assert!(first.contains_key(field), "{list} rows have {field}");
                }
            }
        }
    }
}
