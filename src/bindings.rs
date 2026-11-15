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
//! A game adds actions of its own ([`GameAction`]: a fireball on Q), which sit among the
//! player's controls while that game is on.
//!
//! Not here: Enter keeps and Esc cancels, Ctrl/Cmd+Z undoes, the wheel zooms, and
//! clicking in the app's own windows.

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
    Input {
        name,
        label,
        keys,
        buttons: &[],
    }
}

const fn mouse(name: &'static str, label: &'static str, button: &'static [MouseButton]) -> Input {
    Input {
        name,
        label,
        keys: &[],
        buttons: button,
    }
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
    key(
        "ctrl",
        "Ctrl",
        &[KeyCode::ControlLeft, KeyCode::ControlRight],
    ),
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

impl PartialEq for Input {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

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
        keys.any_pressed(self.keys.iter().copied())
            || buttons.any_pressed(self.buttons.iter().copied())
    }

    pub fn just_pressed(
        &self,
        keys: &ButtonInput<KeyCode>,
        buttons: &ButtonInput<MouseButton>,
    ) -> bool {
        keys.any_just_pressed(self.keys.iter().copied())
            || buttons.any_just_pressed(self.buttons.iter().copied())
    }

    pub fn just_released(
        &self,
        keys: &ButtonInput<KeyCode>,
        buttons: &ButtonInput<MouseButton>,
    ) -> bool {
        keys.any_just_released(self.keys.iter().copied())
            || buttons.any_just_released(self.buttons.iter().copied())
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

const fn action(
    group: &'static str,
    name: &'static str,
    title: &'static str,
    defaults: &'static [&'static str],
) -> Action {
    Action {
        name,
        title,
        group,
        defaults,
    }
}

/// Every action, in the order the Controls list shows them.
pub static ACTIONS: &[Action] = &[
    action(
        "Moving",
        "forward",
        "Walk forward (in a grid game: move away from the camera)",
        &["w", "up"],
    ),
    action("Moving", "back", "Walk back", &["s", "down"]),
    action("Moving", "left", "Walk left", &["a", "left"]),
    action("Moving", "right", "Walk right", &["d", "right"]),
    action("Moving", "run", "Run while held", &["shift"]),
    action("Moving", "jump", "Jump", &["space"]),
    action(
        "Playing",
        "use",
        "Use the piece or portal in reach (in a grid game: its action)",
        &["e"],
    ),
    action("Playing", "hit", "Hit the piece in reach", &["f"]),
    action("Playing", "reset", "Reset the grid game you're in", &["r"]),
    action(
        "Playing",
        "inventory",
        "Open and close the inventory",
        &["i"],
    ),
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
    action(
        "Building",
        "select_area",
        "Select an area of ground to reshape",
        &["g"],
    ),
    action(
        "Building",
        "move_build",
        "Pick up the selected build or pinned draft to move it",
        &["m"],
    ),
    action(
        "Building",
        "remove_build",
        "Remove the selected build",
        &["delete", "backspace"],
    ),
    action(
        "Building",
        "turn_left",
        "Turn the draft a quarter turn left",
        &["q"],
    ),
    action(
        "Building",
        "turn_right",
        "Turn the draft a quarter turn right",
        &["e"],
    ),
    action("Building", "raise", "Raise the draft", &["r"]),
    action("Building", "lower", "Lower the draft", &["f"]),
    action(
        "Building",
        "spawn_area",
        "Show the spawn area: the ground around the start where only admins build",
        &["p"],
    ),
    action(
        "Pointing",
        "click",
        "Place the draft, select a build, drag out an area of ground",
        &["mouse_left"],
    ),
    action(
        "Pointing",
        "cancel",
        "Drop the draft, clear the selection",
        &["mouse_right"],
    ),
    action(
        "Pointing",
        "use_held",
        "Put down what's in hand, or use it on the piece pointed at",
        &["mouse_right"],
    ),
    action(
        "Pointing",
        "copy",
        "Copy the build pointed at into a new draft",
        &["mouse_middle"],
    ),
    action(
        "Pointing",
        "look",
        "Look around while held, by moving the mouse",
        &["mouse_left", "mouse_right"],
    ),
    action(
        "Windows",
        "builder",
        "Show and hide the builder panel",
        &["tab"],
    ),
    action("Windows", "history", "Open the history panel", &["h"]),
    action(
        "Windows",
        "game_window",
        "Open the Game window (worlds, saves, portals)",
        &["o"],
    ),
    action("Windows", "hints", "Show and hide the key hints", &["f1"]),
    action(
        "Windows",
        "stats",
        "Show and hide the frame statistics",
        &["f3"],
    ),
];

/// The action called `name`.
pub fn action_named(name: &str) -> Option<&'static Action> {
    let name = name.trim().to_lowercase();
    ACTIONS.iter().find(|action| action.name == name)
}

fn defaults(action: &Action) -> Vec<&'static Input> {
    action
        .defaults
        .iter()
        .filter_map(|name| Input::named(name))
        .collect()
}

/// An action a game declares for itself (a package's `action` definition): a fireball,
/// a dash. It's among the player's controls while the game is on in the world they're in.
#[derive(Clone, Debug, PartialEq)]
pub struct GameAction {
    /// Its whole name, `package:name`.
    pub name: String,
    pub title: String,
    /// What its group is called among the controls: the game's title.
    pub group: String,
    /// The key the game starts it on, if it's one there is.
    pub defaults: Vec<&'static Input>,
}

impl GameAction {
    /// Its name within its package.
    pub fn short(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }

    pub fn package(&self) -> &str {
        self.name.split(':').next().unwrap_or(&self.name)
    }
}

/// An action as the controls list it: the app's own, or a game's.
#[derive(Debug)]
pub struct Listed {
    pub name: String,
    pub title: String,
    pub group: String,
    usual: Vec<&'static Input>,
}

/// The player's bindings, and which actions are held or were just pressed this frame.
#[derive(Resource)]
pub struct Bindings {
    bound: BTreeMap<String, Vec<&'static Input>>,
    /// The actions of the games on in this world.
    game: Vec<GameAction>,
    /// The player's keys for games' actions that aren't here now, to come back with
    /// them: `{action: [inputs]}`.
    away: BTreeMap<String, Vec<String>>,
    held: HashSet<String>,
    pressed: HashSet<String>,
    released: HashSet<String>,
    /// The inputs that are down.
    down: HashSet<&'static str>,
    /// A controls list is waiting for a key to bind: keys do nothing meanwhile.
    pub capturing: bool,
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            bound: ACTIONS
                .iter()
                .map(|a| (a.name.to_string(), defaults(a)))
                .collect(),
            game: Vec::new(),
            away: BTreeMap::new(),
            held: HashSet::new(),
            pressed: HashSet::new(),
            released: HashSet::new(),
            down: HashSet::new(),
            capturing: false,
        }
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

    /// Whether an input bound to `action` came up this frame.
    pub fn just_released(&self, action: &str) -> bool {
        self.released.contains(action)
    }

    /// Whether an input bound to `action` is down that isn't also bound to `other`:
    /// dragging to look with the button that isn't busy selecting.
    pub fn pressed_without(&self, action: &str, other: &str) -> bool {
        let taken = self.inputs(other);
        self.inputs(action).iter().any(|input| {
            self.down.contains(input.name) && !taken.iter().any(|t| t.name == input.name)
        })
    }

    /// The inputs bound to `action`.
    pub fn inputs(&self, action: &str) -> &[&'static Input] {
        self.bound
            .get(action)
            .map_or(&[], |inputs| inputs.as_slice())
    }

    /// The inputs bound to `action` for a hint: "Space / J", or "unbound".
    pub fn label(&self, action: &str) -> String {
        let labels: Vec<&str> = self
            .inputs(action)
            .iter()
            .map(|input| input.label)
            .collect();
        if labels.is_empty() {
            "unbound".into()
        } else {
            labels.join(" / ")
        }
    }

    /// The first input bound to `action`, for a short hint ("E").
    pub fn short(&self, action: &str) -> &'static str {
        self.inputs(action)
            .first()
            .map_or("unbound", |input| input.label)
    }

    /// The four movement actions for a hint: "WASD" while they're the usual keys.
    pub fn movement(&self) -> String {
        let moves = ["forward", "left", "back", "right"];
        if moves.iter().all(|name| self.is_usual(name)) {
            return "WASD".into();
        }
        moves
            .iter()
            .map(|name| self.short(name))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Every action there is now: the games' first (what's particular to this world),
    /// then the app's own.
    pub fn all(&self) -> Vec<Listed> {
        let own = ACTIONS.iter().map(|a| Listed {
            name: a.name.into(),
            title: a.title.into(),
            group: a.group.into(),
            usual: defaults(a),
        });
        let games = self.game.iter().map(|a| Listed {
            name: a.name.clone(),
            title: a.title.clone(),
            group: a.group.clone(),
            usual: a.defaults.clone(),
        });
        games.chain(own).collect()
    }

    /// The action `name` means: one of the app's, or a game's by its whole name
    /// (`magic:fireball`) or, if only one game has it, its short one (`fireball`).
    pub fn find(&self, name: &str) -> Result<Listed, String> {
        let name = name.trim().to_lowercase();
        let all = self.all();
        let short = |listed: &Listed| listed.name.rsplit(':').next().map(str::to_string);
        let exact = all.iter().position(|a| a.name == name);
        let by_short: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, a)| a.name.contains(':') && short(a).as_deref() == Some(name.as_str()))
            .map(|(i, _)| i)
            .collect();
        let at = match (exact, by_short.as_slice()) {
            (Some(at), _) => at,
            (None, [at]) => *at,
            (None, []) => {
                return Err(format!(
                    "There's no action \"{name}\". Actions: {}.",
                    all.iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            (None, many) => {
                return Err(format!(
                    "\"{name}\" could be {}: give its whole name.",
                    many.iter()
                        .map(|i| all[*i].name.as_str())
                        .collect::<Vec<_>>()
                        .join(" or ")
                ));
            }
        };
        Ok(all.into_iter().nth(at).expect("a position in the list"))
    }

    /// The other actions `input` is bound to.
    pub fn also(&self, input: &Input, action: &str) -> Vec<String> {
        self.bound
            .iter()
            .filter(|(name, inputs)| {
                name.as_str() != action && inputs.iter().any(|i| i.name == input.name)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Binds `input` to `action` as well as what it has. Says what else the input does,
    /// since both will happen where both apply.
    pub fn bind(&mut self, action: &str, input: &str) -> Result<String, String> {
        let action = self.find(action)?;
        let input = Input::named(input).ok_or_else(|| format!("\"{input}\" isn't a key that can be bound (Enter and Esc are fixed). Keys are named like j, space, shift, f6, mouse_back."))?;
        let bound = self.bound.entry(action.name.clone()).or_default();
        if bound.iter().any(|i| i.name == input.name) {
            return Ok(format!("{} already does \"{}\".", input.label, action.name));
        }
        bound.push(input);
        let also = self.also(input, &action.name);
        let also = if also.is_empty() {
            String::new()
        } else {
            format!(
                " It also does {} (unbind it there if they clash).",
                also.join(", ")
            )
        };
        Ok(format!(
            "{} now does \"{}\" ({}): {}.{also}",
            input.label,
            action.name,
            action.title,
            self.label(&action.name)
        ))
    }

    /// Takes `input` off `action`; with no input, takes every input off it.
    pub fn unbind(&mut self, action: &str, input: Option<&str>) -> Result<String, String> {
        let action = self.find(action)?;
        let bound = self.bound.entry(action.name.clone()).or_default();
        match input {
            None => bound.clear(),
            Some(name) => {
                let input = Input::named(name)
                    .ok_or_else(|| format!("\"{name}\" isn't a key that can be bound."))?;
                if !bound.iter().any(|i| i.name == input.name) {
                    return Err(format!(
                        "{} isn't bound to \"{}\" ({}).",
                        input.label,
                        action.name,
                        self.label(&action.name)
                    ));
                }
                self.bound
                    .entry(action.name.clone())
                    .or_default()
                    .retain(|i| i.name != input.name);
            }
        }
        Ok(format!(
            "\"{}\" is now on {}.",
            action.name,
            self.label(&action.name)
        ))
    }

    /// Puts `action` back to its default inputs; with no action, all of them.
    pub fn reset(&mut self, action: Option<&str>) -> Result<String, String> {
        match action {
            None => {
                self.bound = self.all().into_iter().map(|a| (a.name, a.usual)).collect();
                self.away.clear();
                Ok("Every key is back to its default.".into())
            }
            Some(name) => {
                let action = self.find(name)?;
                self.bound.insert(action.name.clone(), action.usual);
                Ok(format!(
                    "\"{}\" is back on {}.",
                    action.name,
                    self.label(&action.name)
                ))
            }
        }
    }

    /// Whether `action` has the inputs it started with.
    pub fn is_usual(&self, action: &str) -> bool {
        let now: Vec<&str> = self.inputs(action).iter().map(|i| i.name).collect();
        self.all()
            .iter()
            .find(|a| a.name == action)
            .is_none_or(|a| a.usual.iter().map(|i| i.name).collect::<Vec<_>>() == now)
    }

    /// What differs from the defaults, to keep between runs: `{action: [inputs]}`,
    /// with the keys for games' actions that aren't here now.
    pub fn changed(&self) -> BTreeMap<String, Vec<String>> {
        let here = self
            .all()
            .into_iter()
            .filter(|a| !self.is_usual(&a.name))
            .map(|a| {
                let inputs = self
                    .inputs(&a.name)
                    .iter()
                    .map(|i| i.name.to_string())
                    .collect();
                (a.name, inputs)
            });
        self.away.clone().into_iter().chain(here).collect()
    }

    /// Takes on what [`Self::changed`] kept. Keys for actions that aren't here (a
    /// game's, while it's off) wait for them.
    pub fn restore<'a>(&mut self, kept: impl IntoIterator<Item = (&'a str, Vec<&'a str>)>) {
        for (name, inputs) in kept {
            // Called "parcels" while land was claimed parcel by parcel.
            let name = if name == "parcels" {
                "spawn_area"
            } else {
                name
            };
            if self.all().iter().any(|a| a.name == name) {
                self.bound.insert(
                    name.to_string(),
                    inputs.into_iter().filter_map(Input::named).collect(),
                );
            } else if name.contains(':') {
                self.away.insert(
                    name.to_string(),
                    inputs.into_iter().map(str::to_string).collect(),
                );
            }
        }
    }

    /// The games' actions.
    pub fn game(&self) -> &[GameAction] {
        &self.game
    }

    /// Makes the games' actions these (the games on in the world changed). The player's
    /// keys for an action that goes are kept for when it comes back.
    pub fn set_game(&mut self, actions: Vec<GameAction>) {
        if self.game == actions {
            return;
        }
        for gone in self
            .game
            .iter()
            .filter(|old| !actions.iter().any(|a| a.name == old.name))
        {
            if let Some(inputs) = self.bound.remove(&gone.name)
                && inputs
                    .iter()
                    .map(|i| i.name)
                    .ne(gone.defaults.iter().map(|i| i.name))
            {
                self.away.insert(
                    gone.name.clone(),
                    inputs.iter().map(|i| i.name.to_string()).collect(),
                );
            }
        }
        for action in &actions {
            if self.bound.contains_key(&action.name) {
                continue;
            }
            let inputs = match self.away.remove(&action.name) {
                Some(kept) => kept.iter().filter_map(|name| Input::named(name)).collect(),
                None => action.defaults.clone(),
            };
            self.bound.insert(action.name.clone(), inputs);
        }
        self.game = actions;
    }

    /// Every action and its inputs, a line each, by group.
    pub fn list(&self) -> String {
        let mut out = String::new();
        let mut group = String::new();
        for action in self.all() {
            if action.group != group {
                group = action.group.clone();
                out.push_str(&format!(
                    "{}{group}\n",
                    if out.is_empty() { "" } else { "\n" }
                ));
            }
            out.push_str(&format!(
                "- {}: {} ({})\n",
                action.name,
                self.label(&action.name),
                action.title
            ));
        }
        out
    }
}

/// Works out which actions are held, just pressed and just released, once the frame's
/// input is in.
fn read_actions(
    mut bindings: ResMut<Bindings>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
) {
    let bindings = &mut *bindings;
    bindings.held.clear();
    bindings.pressed.clear();
    bindings.released.clear();
    bindings.down.clear();
    if bindings.capturing {
        return;
    }
    bindings.down.extend(
        INPUTS
            .iter()
            .filter(|input| input.held(&keys, &buttons))
            .map(|input| input.name),
    );
    for (action, inputs) in &bindings.bound {
        if inputs.iter().any(|input| input.held(&keys, &buttons)) {
            bindings.held.insert(action.clone());
        }
        if inputs
            .iter()
            .any(|input| input.just_pressed(&keys, &buttons))
        {
            bindings.pressed.insert(action.clone());
        }
        if inputs
            .iter()
            .any(|input| input.just_released(&keys, &buttons))
        {
            bindings.released.insert(action.clone());
        }
    }
}

pub struct BindingsPlugin;

impl Plugin for BindingsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Bindings>()
            .add_systems(PreUpdate, read_actions.after(InputSystems));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_is_a_real_input_and_names_are_unique() {
        let mut inputs = HashSet::new();
        for input in INPUTS {
            assert!(
                inputs.insert(input.name),
                "two inputs are called {}",
                input.name
            );
            assert!(
                !input.keys.is_empty() || !input.buttons.is_empty(),
                "{} is no key or button",
                input.name
            );
            assert_eq!(Input::named(input.name).map(|i| i.name), Some(input.name));
        }
        let mut actions = HashSet::new();
        for action in ACTIONS {
            assert!(
                actions.insert(action.name),
                "two actions are called {}",
                action.name
            );
            for name in action.defaults {
                assert!(
                    Input::named(name).is_some(),
                    "{}'s default {name} isn't an input",
                    action.name
                );
            }
        }
        assert!(
            Input::named("Escape").is_none() && Input::named("enter").is_none(),
            "Esc and Enter stay fixed"
        );
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
        assert_eq!(
            kept.get("jump"),
            Some(&vec!["space".to_string(), "j".to_string()])
        );
        let mut again = Bindings::default();
        again.restore(kept.iter().map(|(action, inputs)| {
            (action.as_str(), inputs.iter().map(String::as_str).collect())
        }));
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
    fn a_games_actions_come_and_go_with_it_and_keep_the_players_keys() {
        let fireball = || GameAction {
            name: "magic:fireball".into(),
            title: "Throw a fireball".into(),
            group: "Magic".into(),
            defaults: vec![Input::named("q").unwrap()],
        };
        let mut bindings = Bindings::default();
        assert!(bindings.find("fireball").is_err());
        bindings.set_game(vec![fireball()]);
        assert_eq!(bindings.label("magic:fireball"), "Q");
        assert_eq!(
            bindings.find("fireball").map(|a| a.name),
            Ok("magic:fireball".to_string())
        );
        assert!(
            bindings
                .list()
                .contains("Magic\n- magic:fireball: Q (Throw a fireball)")
        );
        assert!(
            bindings.changed().is_empty(),
            "on its own key, there's nothing to keep"
        );
        // The player moves it; the game goes and comes back; their key is still there.
        bindings
            .bind("fireball", "mouse_middle")
            .expect("by its short name");
        bindings
            .unbind("magic:fireball", Some("q"))
            .expect("by its whole name");
        bindings.set_game(Vec::new());
        assert!(bindings.find("fireball").is_err() && bindings.inputs("magic:fireball").is_empty());
        assert_eq!(
            bindings.changed().get("magic:fireball"),
            Some(&vec!["mouse_middle".to_string()]),
            "kept while it's away"
        );
        bindings.set_game(vec![fireball()]);
        assert_eq!(bindings.label("magic:fireball"), "Middle click");
        // Kept between runs too, before the game is on.
        let kept = bindings.changed();
        let mut later = Bindings::default();
        later.restore(kept.iter().map(|(action, inputs)| {
            (action.as_str(), inputs.iter().map(String::as_str).collect())
        }));
        later.set_game(vec![fireball()]);
        assert_eq!(later.label("magic:fireball"), "Middle click");
        // Two games with the same short name need the whole one.
        later.set_game(vec![
            fireball(),
            GameAction {
                name: "fire:fireball".into(),
                ..fireball()
            },
        ]);
        assert!(later.find("fireball").unwrap_err().contains("whole name"));
        later.reset(None).expect("always");
        assert_eq!(later.label("magic:fireball"), "Q");
    }

    #[test]
    fn looking_leaves_the_button_thats_busy_selecting() {
        let mut app = App::new();
        app.init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ButtonInput<MouseButton>>()
            .init_resource::<Bindings>();
        app.add_systems(Update, read_actions);
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        app.update();
        let bindings = app.world().resource::<Bindings>();
        assert!(bindings.pressed("look") && bindings.just_pressed("click"));
        assert!(
            !bindings.pressed_without("look", "click"),
            "the left button is the click's"
        );
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Right);
        app.update();
        assert!(
            app.world()
                .resource::<Bindings>()
                .pressed_without("look", "click")
        );
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .clear();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .release(MouseButton::Left);
        app.update();
        assert!(app.world().resource::<Bindings>().just_released("click"));
    }

    #[test]
    fn actions_follow_the_keys_bound_to_them() {
        let mut app = App::new();
        app.init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ButtonInput<MouseButton>>()
            .init_resource::<Bindings>();
        app.add_systems(Update, read_actions);
        app.world_mut()
            .resource_mut::<Bindings>()
            .bind("jump", "mouse_back")
            .expect("a mouse button can jump");
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyJ);
        app.update();
        assert!(
            !app.world().resource::<Bindings>().just_pressed("jump"),
            "J isn't bound yet"
        );

        app.world_mut()
            .resource_mut::<Bindings>()
            .bind("jump", "j")
            .expect("j can jump");
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::KeyJ);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyJ);
        app.update();
        let bindings = app.world().resource::<Bindings>();
        assert!(
            bindings.just_pressed("jump") && bindings.pressed("jump") && !bindings.pressed("run")
        );

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::KeyJ);
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Back);
        app.update();
        assert!(
            app.world().resource::<Bindings>().pressed("jump"),
            "the mouse button holds it too"
        );
    }
}
