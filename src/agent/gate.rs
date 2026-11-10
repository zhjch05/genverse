//! The foundation's gate: what a player can do, the agent can do, and what can be
//! changed is in a table. These tests fail when something is added outside that.
//!
//! Each registry has the same fronts ([`REGISTRIES`]): ops that list and change it,
//! which are the agent's tools and the chat's commands; a page in the agent's folder;
//! and a list a data panel can show. What's hard-wired on purpose says why here, by
//! name ([`RAW_INPUT`], [`NATIVE_WINDOWS`], [`CHAT_ONLY`]), so a new key read outside
//! the bindings, a new window outside the panels or an op the agent can't reach is a
//! decision somebody wrote down, not something that slipped in.
//!
//! The other gates sit with their tables: every field of the rules and the graphics is
//! a setting (`registry.rs`), every kind of change has an op (`ops.rs`), every default
//! key is a real input (`bindings.rs`), every voxel has a look (`content.rs`), and the
//! answer schema lists what the rules take (`build.rs`).

use std::collections::BTreeSet;
use std::path::Path;

use super::{ops::OPS, panels::LISTS};

/// Whether a registry has one of its fronts, or why it doesn't.
enum Front {
    Has(&'static str),
    No(&'static str),
}
use Front::{Has, No};

/// A table of things that can be changed, and how it's reached.
struct Registry {
    name: &'static str,
    /// The op that says what's in it.
    list: Front,
    /// The ops that change it.
    change: &'static [&'static str],
    /// The page in the agent's folder that shows it, or says how it works.
    page: Front,
    /// The list a data panel repeats over.
    rows: Front,
}

const REGISTRIES: &[Registry] = &[
    Registry { name: "ops", list: Has("help"), change: &[], page: No("the agent's tools are made from the ops, with their notes"), rows: Has("ops") },
    Registry { name: "settings", list: Has("settings"), change: &["set"], page: Has("SETTINGS.md"), rows: Has("settings") },
    Registry { name: "bindings", list: Has("bindings"), change: &["bind", "unbind", "resetkeys"], page: Has("KEYS.md"), rows: Has("actions") },
    Registry { name: "panels", list: Has("panels"), change: &["panel", "removepanel"], page: Has("docs/panels.md"), rows: Has("panels") },
    Registry { name: "blocks", list: Has("blocks"), change: &["block", "removeblock"], page: Has("BLOCKS.md"), rows: Has("blocks") },
    Registry { name: "landscapes", list: Has("landscapes"), change: &["newworld"], page: Has("docs/terrain.md"), rows: Has("landscapes") },
    Registry { name: "worlds", list: Has("worlds"), change: &["newworld", "removeworld", "world"], page: Has("GAME.md"), rows: Has("worlds") },
    Registry { name: "saves", list: Has("saves"), change: &["save", "load", "fork"], page: Has("GAME.md"), rows: Has("saves") },
    Registry { name: "packages", list: Has("packages"), change: &["turn_on", "uninstall", "remove_package"], page: Has("packages/"), rows: Has("packages") },
];

/// Lists that go with another list, not a registry of their own.
const GROUPINGS: &[&str] = &["action_groups", "setting_groups"];

/// Ops that aren't the agent's tools, and why.
const CHAT_ONLY: &[(&str, &str)] = &[
    ("play", "opens the window for joining a world on the network, which the player picks from; host, join and leave are the tools"),
    ("help", "lists the chat's commands; the agent has its tools"),
];

/// Keys and buttons read outside the bindings, and why they aren't actions.
const RAW_INPUT: &[(&str, &str, &str)] = &[
    ("agent/mod.rs", "KeyCode::Escape", "Esc and Enter stay fixed: they cancel and confirm, in every dialog"),
    ("agent/mod.rs", "KeyCode::Enter", "Esc and Enter stay fixed"),
    ("agent/game.rs", "KeyCode::Escape", "Esc leaves a grid game, as it leaves everything"),
    ("agent/inventory.rs", "KeyCode::Escape", "Esc closes the inventory"),
    ("agent/panels.rs", "KeyCode::Escape", "Esc stops waiting for a key to bind"),
    ("agent/menu.rs", "KeyCode::Escape", "Esc closes windows, then opens the game menu, once nothing else takes it"),
    ("agent/mod.rs", "KeyCode::KeyZ", "Ctrl/Cmd+Z: a chord, and bindings are single inputs so far"),
    ("agent/mod.rs", "KeyCode::ControlLeft", "the chord's modifier"),
    ("agent/mod.rs", "KeyCode::ControlRight", "the chord's modifier"),
    ("agent/mod.rs", "KeyCode::SuperLeft", "the chord's modifier"),
    ("agent/mod.rs", "KeyCode::SuperRight", "the chord's modifier"),
    ("agent/mod.rs", "KeyCode::ShiftLeft", "with the chord, redo"),
    ("agent/mod.rs", "KeyCode::ShiftRight", "with the chord, redo"),
    ("agent/fps.rs", "MouseButton::Left", "the statistics show which buttons are held, whatever they do"),
    ("agent/fps.rs", "MouseButton::Right", "the statistics show which buttons are held"),
    ("agent/fps.rs", "MouseButton::Middle", "the statistics show which buttons are held"),
];

/// Files that hold the input tables themselves.
const INPUT_TABLES: &[&str] = &["bindings.rs", "agent/gate.rs"];

/// Windows drawn in Rust rather than from a panel's data, and why.
const NATIVE_WINDOWS: &[(&str, &str)] = &[
    ("agent/panels.rs", "the window every data panel is drawn in"),
    ("agent/hud.rs", "the window a game's own panel is drawn in, from the game's data"),
    ("agent/settings.rs", "later a panel: its world section is made from the settings, and Controls is one already"),
    ("agent/worlds.rs", "later a panel: every button in it runs an op"),
    ("agent/history.rs", "a live view of the change log, with undo on each line"),
    ("agent/inventory.rs", "the grid of what the player carries, dragged between slots"),
];

/// Every Rust file under `src`, by its path from there, with its text.
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("src is there").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let name = path.strip_prefix(root).expect("under src").to_string_lossy().replace('\\', "/");
                out.push((name, std::fs::read_to_string(&path).expect("readable")));
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out
}

/// Every `Prefix::Name` in `text`.
fn named(text: &str, prefix: &str) -> BTreeSet<String> {
    text.match_indices(prefix)
        .map(|(at, _)| {
            let name: String = text[at + prefix.len()..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            format!("{prefix}{name}")
        })
        .collect()
}

#[test]
fn every_registry_has_its_fronts() {
    let op = |name: &str| OPS.iter().find(|op| op.name == name);
    let workspace = include_str!("workspace.rs");
    for r in REGISTRIES {
        for name in r.change.iter().copied().chain(if let Has(list) = r.list { Some(list) } else { None }) {
            let op = op(name).unwrap_or_else(|| panic!("{}: there's no op called {name}", r.name));
            assert!(op.agent || CHAT_ONLY.iter().any(|(chat, _)| *chat == name), "{}: {name} isn't one of the agent's tools", r.name);
        }
        match r.page {
            Has(page) => assert!(workspace.contains(page), "{}: nothing writes {page} into the agent's folder", r.name),
            No(why) => assert!(!why.is_empty()),
        }
        match r.rows {
            Has(list) => assert!(LISTS.iter().any(|(name, ..)| *name == list), "{}: panels have no list called {list}", r.name),
            No(why) => assert!(!why.is_empty()),
        }
        if let No(why) = r.list {
            assert!(!why.is_empty());
        }
    }
    // And the other way: a list a panel can show is some registry's.
    for (list, ..) in LISTS {
        let owned = REGISTRIES.iter().any(|r| matches!(r.rows, Has(rows) if rows == *list)) || GROUPINGS.contains(list);
        assert!(owned, "the panel list {list} isn't a registry's: add it to REGISTRIES with its ops and its page");
    }
}

#[test]
fn every_op_is_the_agents_or_says_why_not() {
    for op in OPS {
        let excused = CHAT_ONLY.iter().find(|(name, _)| *name == op.name);
        assert!(op.agent != excused.is_some(), "{} must be one of the agent's tools, or listed in CHAT_ONLY with the reason (and not both)", op.name);
        assert!(excused.is_none_or(|(_, why)| !why.is_empty()));
        assert!(!op.about.is_empty(), "{} doesn't say what it does", op.name);
    }
    for (name, _) in CHAT_ONLY {
        assert!(OPS.iter().any(|op| op.name == *name), "CHAT_ONLY names {name}, which isn't an op");
    }
}

#[test]
fn keys_and_buttons_are_read_through_the_bindings() {
    let mut found = BTreeSet::new();
    for (file, text) in sources() {
        if INPUT_TABLES.contains(&file.as_str()) {
            continue;
        }
        for input in named(&text, "KeyCode::").into_iter().chain(named(&text, "MouseButton::")) {
            let excused = RAW_INPUT.iter().any(|(f, i, why)| *f == file && *i == input && !why.is_empty());
            assert!(excused, "{file} reads {input} itself: make it an action in src/bindings.rs, or list it in RAW_INPUT with the reason");
            found.insert((file.clone(), input));
        }
    }
    for (file, input, _) in RAW_INPUT {
        assert!(found.contains(&(file.to_string(), input.to_string())), "RAW_INPUT excuses {input} in {file}, which isn't there any more");
    }
}

#[test]
fn windows_are_panels_or_say_why_not() {
    let mut found = BTreeSet::new();
    for (file, text) in sources() {
        if file != "agent/gate.rs" && text.contains("egui::Window::new") {
            let excused = NATIVE_WINDOWS.iter().any(|(f, why)| *f == file && !why.is_empty());
            assert!(excused, "{file} draws a window of its own: make it a panel (src/agent/panels/), or list it in NATIVE_WINDOWS with the reason");
            found.insert(file);
        }
    }
    for (file, _) in NATIVE_WINDOWS {
        assert!(found.contains(*file), "NATIVE_WINDOWS excuses {file}, which draws no window any more");
    }
}
