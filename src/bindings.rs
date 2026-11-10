//! Which keys and mouse buttons do what. Everything a key does in the game is a named
//! [action](ACTIONS) ("jump", "use", "history"), and the game asks for the action, never
//! the key: [`Bindings`] says which [inputs](Input) are bound to it, and whether it's
//! held or was just pressed this frame.
//!
//! An action may have several inputs (jump on Space and J), and an input may do several
//! things in different moments (E uses a piece, and turns a draft). The player changes
//! them from the chat (`/bind jump j`), the Settings window's Controls, or by asking the
//! agent; they're the player's own, kept with their preferences.
//!
//! Not here: Enter keeps and Esc cancels, Ctrl/Cmd+Z undoes, and what the mouse does by
//! pointing (clicking to place or select, dragging to look, the wheel).

use std::collections::{BTreeMap, HashSet};

use bevy::{input::InputSystems, prelude::*};

/// A key or mouse button, by the name players, scripts and the agent use for it.
#[derive(Debug)]
pub struct Input {
    pub name: &'static str,
    /// As shown in hints and the Controls list.
    pub label: &'static str,
    keys: &'static [KeyCode],
    buttons: &'static [MouseButton],
}

const fn key(name: &'static str, label: &'static str, keys: &'static [KeyCode]) -> Input {
    Input { name, label, keys, buttons: &[] }
}

const fn mouse(name: &'static str, label: &'static str, button: &'static [MouseButton]) -> Input {
    Input { name, label, keys: &[], buttons: button }
}

/// Every input that can be bound, and that player scripts can read.
pub static INPUTS: &[Input] = &[
    key("a", "A", &[KeyCode::KeyA]),
    key("b", "B", &[KeyCode::KeyB]),
    key("c", "C", &[KeyCode::KeyC]),
    key("d", "D", &[KeyCode::KeyD]),
    key("e", "E", &[KeyCode::KeyE]),
    key("f", "F", &[KeyCode::KeyF]),
    key("g", "G", &[KeyCode::KeyG]),
    key("h", "H", &[KeyCode::KeyH]),
    key("i", "I", &[KeyCode::KeyI]),
    key("j", "J", &[KeyCode::KeyJ]),
    key("k", "K", &[KeyCode::KeyK]),
    key("l", "L", &[KeyCode::KeyL]),
    key("m", "M", &[KeyCode::KeyM]),
    key("n", "N", &[KeyCode::KeyN]),
    key("o", "O", &[KeyCode::KeyO]),
    key("p", "P", &[KeyCode::KeyP]),
    key("q", "Q", &[KeyCode::KeyQ]),
    key("r", "R", &[KeyCode::KeyR]),
    key("s", "S", &[KeyCode::KeyS]),
    key("t", "T", &[KeyCode::KeyT]),
    key("u", "U", &[KeyCode::KeyU]),
    key("v", "V", &[KeyCode::KeyV]),
    key("w", "W", &[KeyCode::KeyW]),
    key("x", "X", &[KeyCode::KeyX]),
    key("y", "Y", &[KeyCode::KeyY]),
    key("z", "Z", &[KeyCode::KeyZ]),
    key("0", "0", &[KeyCode::Digit0]),
    key("1", "1", &[KeyCode::Digit1]),
    key("2", "2", &[KeyCode::Digit2]),
    key("3", "3", &[KeyCode::Digit3]),
    key("4", "4", &[KeyCode::Digit4]),
    key("5", "5", &[KeyCode::Digit5]),
    key("6", "6", &[KeyCode::Digit6]),
    key("7", "7", &[KeyCode::Digit7]),
    key("8", "8", &[KeyCode::Digit8]),
    key("9", "9", &[KeyCode::Digit9]),
    key("space", "Space", &[KeyCode::Space]),
    key("shift", "Shift", &[KeyCode::ShiftLeft, KeyCode::ShiftRight]),
    key("ctrl", "Ctrl", &[KeyCode::ControlLeft, KeyCode::ControlRight]),
    key("alt", "Alt", &[KeyCode::AltLeft, KeyCode::AltRight]),
    key("tab", "Tab", &[KeyCode::Tab]),
    key("up", "Up", &[KeyCode::ArrowUp]),
    key("down", "Down", &[KeyCode::ArrowDown]),
    key("left", "Left", &[KeyCode::ArrowLeft]),
    key("right", "Right", &[KeyCode::ArrowRight]),
    key("delete", "Delete", &[KeyCode::Delete]),
    key("backspace", "Backspace", &[KeyCode::Backspace]),
    key("insert", "Insert", &[KeyCode::Insert]),
    key("home", "Home", &[KeyCode::Home]),
    key("end", "End", &[KeyCode::End]),
    key("pageup", "Page Up", &[KeyCode::PageUp]),
    key("pagedown", "Page Down", &[KeyCode::PageDown]),
    key("capslock", "Caps Lock", &[KeyCode::CapsLock]),
    key("minus", "-", &[KeyCode::Minus]),
    key("equal", "=", &[KeyCode::Equal]),
    key("comma", ",", &[KeyCode::Comma]),
    key("period", ".", &[KeyCode::Period]),
    key("semicolon", ";", &[KeyCode::Semicolon]),
    key("quote", "'", &[KeyCode::Quote]),
    key("backquote", "`", &[KeyCode::Backquote]),
    key("bracketleft", "[", &[KeyCode::BracketLeft]),
    key("bracketright", "]", &[KeyCode::BracketRight]),
    key("backslash", "\\", &[KeyCode::Backslash]),
    key("f1", "F1", &[KeyCode::F1]),
    key("f2", "F2", &[KeyCode::F2]),
    key("f3", "F3", &[KeyCode::F3]),
    key("f4", "F4", &[KeyCode::F4]),
    key("f5", "F5", &[KeyCode::F5]),
    key("f6", "F6", &[KeyCode::F6]),
    key("f7", "F7", &[KeyCode::F7]),
    key("f8", "F8", &[KeyCode::F8]),
    key("f9", "F9", &[KeyCode::F9]),
    key("f10", "F10", &[KeyCode::F10]),
    key("f11", "F11", &[KeyCode::F11]),
    key("f12", "F12", &[KeyCode::F12]),
    mouse("mouse_left", "Left click", &[MouseButton::Left]),
    mouse("mouse_right", "Right click", &[MouseButton::Right]),
    mouse("mouse_middle", "Middle click", &[MouseButton::Middle]),
    mouse("mouse_back", "Mouse back", &[MouseButton::Back]),
    mouse("mouse_forward", "Mouse forward", &[MouseButton::Forward]),
];

impl Input {
    /// The input called `name` ("j", "space", "mouse_back"), in any case.
    pub fn named(name: &str) -> Option<&'static Input> {
        let name = name.trim().to_lowercase();
        // What people call them, besides their names here.
        let name = match name.as_str() {
            "spacebar" => "space",
            "control" => "ctrl",
            "option" => "alt",
            "esc" | "escape" | "enter" | "return" => return None,
            "del" => "delete",
            "arrowup" => "up",
            "arrowdown" => "down",
            "arrowleft" => "left",
            "arrowright" => "right",
            other => other,
        };
        INPUTS.iter().find(|input| input.name == name)
    }

    pub fn held(&self, keys: &ButtonInput<KeyCode>, buttons: &ButtonInput<MouseButton>) -> bool {
        keys.any_pressed(self.keys.iter().copied()) || buttons.any_pressed(self.buttons.iter().copied())
    }

    pub fn just_pressed(&self, keys: &ButtonInput<KeyCode>, buttons: &ButtonInput<MouseButton>) -> bool {
        keys.any_just_pressed(self.keys.iter().copied()) || buttons.any_just_pressed(self.buttons.iter().copied())
    }
}

/// Something a key does.
pub struct Action {
    pub name: &'static str,
    /// What it does, for the Controls list and the agent.
    pub title: &'static str,
    pub group: &'static str,
    /// The inputs it has until the player changes them.
    pub defaults: &'static [&'static str],
}

const fn action(group: &'static str, name: &'static str, title: &'static str, defaults: &'static [&'static str]) -> Action {
    Action { name, title, group, defaults }
}

/// Every action, in the order the Controls list shows them.
pub static ACTIONS: &[Action] = &[
    action("Moving", "forward", "Walk forward (in a grid game: move away from the camera)", &["w", "up"]),
    action("Moving", "back", "Walk back", &["s", "down"]),
    action("Moving", "left", "Walk left", &["a", "left"]),
    action("Moving", "right", "Walk right", &["d", "right"]),
    action("Moving", "run", "Run while held", &["shift"]),
    action("Moving", "jump", "Jump", &["space"]),
    action("Moving", "view", "Switch between first and third person", &["f5"]),
    action("Playing", "use", "Use the piece or portal in reach (in a grid game: its action)", &["e"]),
    action("Playing", "hit", "Hit the piece in reach", &["f"]),
    action("Playing", "reset", "Reset the grid game you're in", &["r"]),
    action("Playing", "inventory", "Open and close the inventory", &["i"]),
    action("Playing", "hand_1", "Hold hotbar slot 1", &["1"]),
    action("Playing", "hand_2", "Hold hotbar slot 2", &["2"]),
    action("Playing", "hand_3", "Hold hotbar slot 3", &["3"]),
    action("Playing", "hand_4", "Hold hotbar slot 4", &["4"]),
    action("Playing", "hand_5", "Hold hotbar slot 5", &["5"]),
    action("Playing", "hand_6", "Hold hotbar slot 6", &["6"]),
    action("Playing", "hand_7", "Hold hotbar slot 7", &["7"]),
    action("Playing", "hand_8", "Hold hotbar slot 8", &["8"]),
    action("Playing", "hand_9", "Hold hotbar slot 9", &["9"]),
    action("Building", "chat", "Type to the agent", &["t"]),
    action("Building", "select_area", "Select an area of ground to reshape", &["g"]),
    action("Building", "move_build", "Pick up the selected build or pinned draft to move it", &["m"]),
    action("Building", "remove_build", "Remove the selected build", &["delete", "backspace"]),
    action("Building", "turn_left", "Turn the draft a quarter turn left", &["q"]),
    action("Building", "turn_right", "Turn the draft a quarter turn right", &["e"]),
    action("Building", "raise", "Raise the draft", &["r"]),
    action("Building", "lower", "Lower the draft", &["f"]),
    action("Building", "parcels", "Show who owns the land", &["p"]),
    action("Windows", "builder", "Show and hide the builder panel", &["tab"]),
    action("Windows", "history", "Open the history panel", &["h"]),
    action("Windows", "game_window", "Open the Game window (worlds, saves, portals)", &["o"]),
    action("Windows", "hints", "Show and hide the key hints", &["f1"]),
    action("Windows", "stats", "Show and hide the frame statistics", &["f3"]),
];

/// The action called `name`.
pub fn action_named(name: &str) -> Option<&'static Action> {
    let name = name.trim().to_lowercase();
    ACTIONS.iter().find(|action| action.name == name)
}

fn defaults(action: &Action) -> Vec<&'static Input> {
    action.defaults.iter().filter_map(|name| Input::named(name)).collect()
}

/// The player's bindings, and which actions are held or were just pressed this frame.
#[derive(Resource)]
pub struct Bindings {
    bound: BTreeMap<&'static str, Vec<&'static Input>>,
    held: HashSet<&'static str>,
    pressed: HashSet<&'static str>,
    /// The Controls list is waiting for a key to bind: keys do nothing meanwhile.
    pub capturing: bool,
}

impl Default for Bindings {
    fn default() -> Self {
        Self { bound: ACTIONS.iter().map(|a| (a.name, defaults(a))).collect(), held: HashSet::new(), pressed: HashSet::new(), capturing: false }
    }
}

impl Bindings {
    /// Whether an input bound to `action` is down.
    pub fn pressed(&self, action: &str) -> bool {
        self.held.contains(action)
    }

    /// Whether an input bound to `action` went down this frame.
    pub fn just_pressed(&self, action: &str) -> bool {
        self.pressed.contains(action)
    }

    /// The inputs bound to `action`.
    pub fn inputs(&self, action: &str) -> &[&'static Input] {
        self.bound.get(action).map_or(&[], |inputs| inputs.as_slice())
    }

    /// The inputs bound to `action` for a hint: "Space / J", or "unbound".
    pub fn label(&self, action: &str) -> String {
        let labels: Vec<&str> = self.inputs(action).iter().map(|input| input.label).collect();
        if labels.is_empty() { "unbound".into() } else { labels.join(" / ") }
    }

    /// The first input bound to `action`, for a short hint ("E").
    pub fn short(&self, action: &str) -> &'static str {
        self.inputs(action).first().map_or("unbound", |input| input.label)
    }

    /// The four movement actions for a hint: "WASD" while they're the usual keys.
    pub fn movement(&self) -> String {
        let moves = ["forward", "left", "back", "right"];
        if moves.iter().all(|name| action_named(name).is_some_and(|action| self.is_default(action))) {
            return "WASD".into();
        }
        moves.iter().map(|name| self.short(name)).collect::<Vec<_>>().join(" ")
    }

    /// The other actions `input` is bound to.
    pub fn also(&self, input: &Input, action: &str) -> Vec<&'static str> {
        self.bound.iter().filter(|(name, inputs)| **name != action && inputs.iter().any(|i| i.name == input.name)).map(|(name, _)| *name).collect()
    }

    /// Binds `input` to `action` as well as what it has. Says what else the input does,
    /// since both will happen where both apply.
    pub fn bind(&mut self, action: &str, input: &str) -> Result<String, String> {
        let action = action_named(action).ok_or_else(|| format!("There's no action \"{action}\". Actions: {}.", names()))?;
        let input = Input::named(input).ok_or_else(|| format!("\"{input}\" isn't a key that can be bound (Enter and Esc are fixed). Keys are named like j, space, shift, f6, mouse_back."))?;
        let bound = self.bound.entry(action.name).or_default();
        if bound.iter().any(|i| i.name == input.name) {
            return Ok(format!("{} already does \"{}\".", input.label, action.name));
        }
        bound.push(input);
        let also = self.also(input, action.name);
        let also = if also.is_empty() { String::new() } else { format!(" It also does {} (unbind it there if they clash).", also.join(", ")) };
        Ok(format!("{} now does \"{}\" ({}): {}.{also}", input.label, action.name, action.title, self.label(action.name)))
    }

    /// Takes `input` off `action`; with no input, takes every input off it.
    pub fn unbind(&mut self, action: &str, input: Option<&str>) -> Result<String, String> {
        let action = action_named(action).ok_or_else(|| format!("There's no action \"{action}\". Actions: {}.", names()))?;
        let bound = self.bound.entry(action.name).or_default();
        match input {
            None => bound.clear(),
            Some(name) => {
                let input = Input::named(name).ok_or_else(|| format!("\"{name}\" isn't a key that can be bound."))?;
                if !bound.iter().any(|i| i.name == input.name) {
                    return Err(format!("{} isn't bound to \"{}\" ({}).", input.label, action.name, self.label(action.name)));
                }
                self.bound.entry(action.name).or_default().retain(|i| i.name != input.name);
            }
        }
        Ok(format!("\"{}\" is now on {}.", action.name, self.label(action.name)))
    }

    /// Puts `action` back to its default inputs; with no action, all of them.
    pub fn reset(&mut self, action: Option<&str>) -> Result<String, String> {
        match action {
            None => {
                self.bound = ACTIONS.iter().map(|a| (a.name, defaults(a))).collect();
                Ok("Every key is back to its default.".into())
            }
            Some(name) => {
                let action = action_named(name).ok_or_else(|| format!("There's no action \"{name}\". Actions: {}.", names()))?;
                self.bound.insert(action.name, defaults(action));
                Ok(format!("\"{}\" is back on {}.", action.name, self.label(action.name)))
            }
        }
    }

    /// Whether `action` has the inputs it started with.
    pub fn is_default(&self, action: &Action) -> bool {
        let now: Vec<&str> = self.inputs(action.name).iter().map(|i| i.name).collect();
        now == action.defaults
    }

    /// What differs from the defaults, to keep between runs: `{action: [inputs]}`.
    pub fn changed(&self) -> BTreeMap<&'static str, Vec<&'static str>> {
        ACTIONS.iter().filter(|a| !self.is_default(a)).map(|a| (a.name, self.inputs(a.name).iter().map(|i| i.name).collect())).collect()
    }

    /// Takes on what [`Self::changed`] kept. Names it doesn't know are skipped.
    pub fn restore<'a>(&mut self, kept: impl IntoIterator<Item = (&'a str, Vec<&'a str>)>) {
        for (name, inputs) in kept {
            if let Some(action) = action_named(name) {
                self.bound.insert(action.name, inputs.into_iter().filter_map(Input::named).collect());
            }
        }
    }

    /// Every action and its inputs, a line each, by group.
    pub fn list(&self) -> String {
        let mut out = String::new();
        let mut group = "";
        for action in ACTIONS {
            if action.group != group {
                group = action.group;
                out.push_str(&format!("{}{group}\n", if out.is_empty() { "" } else { "\n" }));
            }
            out.push_str(&format!("- {}: {} ({})\n", action.name, self.label(action.name), action.title));
        }
        out
    }
}

fn names() -> String {
    ACTIONS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
}

/// Works out which actions are held and just pressed, once the frame's input is in.
fn read_actions(mut bindings: ResMut<Bindings>, keys: Res<ButtonInput<KeyCode>>, buttons: Res<ButtonInput<MouseButton>>) {
    let bindings = &mut *bindings;
    bindings.held.clear();
    bindings.pressed.clear();
    if bindings.capturing {
        return;
    }
    for (action, inputs) in &bindings.bound {
        if inputs.iter().any(|input| input.held(&keys, &buttons)) {
            bindings.held.insert(*action);
        }
        if inputs.iter().any(|input| input.just_pressed(&keys, &buttons)) {
            bindings.pressed.insert(*action);
        }
    }
}

pub struct BindingsPlugin;

impl Plugin for BindingsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Bindings>().add_systems(PreUpdate, read_actions.after(InputSystems));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_is_a_real_input_and_names_are_unique() {
        let mut inputs = HashSet::new();
        for input in INPUTS {
            assert!(inputs.insert(input.name), "two inputs are called {}", input.name);
            assert!(!input.keys.is_empty() || !input.buttons.is_empty(), "{} is no key or button", input.name);
            assert_eq!(Input::named(input.name).map(|i| i.name), Some(input.name));
        }
        let mut actions = HashSet::new();
        for action in ACTIONS {
            assert!(actions.insert(action.name), "two actions are called {}", action.name);
            for name in action.defaults {
                assert!(Input::named(name).is_some(), "{}'s default {name} isn't an input", action.name);
            }
        }
        assert!(Input::named("Escape").is_none() && Input::named("enter").is_none(), "Esc and Enter stay fixed");
    }

    #[test]
    fn an_action_takes_a_second_key_and_gives_it_back() {
        let mut bindings = Bindings::default();
        assert!(bindings.changed().is_empty());
        let said = bindings.bind("jump", "J").expect("j can jump");
        assert!(said.contains("Space / J"), "{said}");
        assert_eq!(bindings.label("jump"), "Space / J");
        // E already turns a draft: binding it elsewhere says so.
        assert!(bindings.bind("hit", "e").expect("allowed").contains("use"));
        assert!(bindings.bind("fly", "j").is_err() && bindings.bind("jump", "enter").is_err());

        // Only what changed is kept, and it comes back.
        let kept = bindings.changed();
        assert_eq!(kept.get("jump"), Some(&vec!["space", "j"]));
        let mut again = Bindings::default();
        again.restore(kept.iter().map(|(action, inputs)| (*action, inputs.clone())));
        assert_eq!(again.label("jump"), "Space / J");

        assert!(bindings.unbind("jump", Some("space")).is_ok());
        assert_eq!(bindings.label("jump"), "J");
        assert!(bindings.unbind("jump", Some("space")).is_err());
        bindings.reset(Some("jump")).expect("jump exists");
        assert_eq!(bindings.label("jump"), "Space");
        bindings.reset(None).expect("always");
        assert!(bindings.changed().is_empty());
    }

    #[test]
    fn actions_follow_the_keys_bound_to_them() {
        let mut app = App::new();
        app.init_resource::<ButtonInput<KeyCode>>().init_resource::<ButtonInput<MouseButton>>().init_resource::<Bindings>();
        app.add_systems(Update, read_actions);
        app.world_mut().resource_mut::<Bindings>().bind("jump", "mouse_back").expect("a mouse button can jump");
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(KeyCode::KeyJ);
        app.update();
        assert!(!app.world().resource::<Bindings>().just_pressed("jump"), "J isn't bound yet");

        app.world_mut().resource_mut::<Bindings>().bind("jump", "j").expect("j can jump");
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().release(KeyCode::KeyJ);
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().clear();
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(KeyCode::KeyJ);
        app.update();
        let bindings = app.world().resource::<Bindings>();
        assert!(bindings.just_pressed("jump") && bindings.pressed("jump") && !bindings.pressed("run"));

        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().release(KeyCode::KeyJ);
        app.world_mut().resource_mut::<ButtonInput<MouseButton>>().press(MouseButton::Back);
        app.update();
        assert!(app.world().resource::<Bindings>().pressed("jump"), "the mouse button holds it too");
    }
}
