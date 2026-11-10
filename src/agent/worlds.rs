//! Games and their worlds (`docs/worlds.md`): going from one world to another, the Game
//! window (O, or the globe button), and portals.
//!
//! - **Trips:** every way into a world (the window, `/world`, a portal, joining or
//!   leaving a server) asks for a [`Trip`]; [`travel`] takes it at the end of the frame.
//!   Locally, the world left behind is saved, the new one loads, the ground resets to its
//!   seed, and the hero lands at its spawn (or by a portal leading back) behind the
//!   loading screen. On a server, the server is asked; its welcome does the rest
//!   ([`super::online`]).
//! - **The window:** Worlds (enter, new, remove), Saves (this world's and the game's),
//!   Portals (go, link, unlink), Rules (the game's packages, and which run here), Games
//!   (local games, servers, hosting).
//! - **Portals:** builds linked by [`Action::SetPortal`]. Walking up to one shows where
//!   it leads; E goes.

use std::time::SystemTime;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use genverse_rules::{
    build::VOXEL_SIZE as BUILD_VOXEL,
    game::GameAction,
    log::{Action, BuildState, PortalTo},
    logic::Package,
};
use egui_phosphor::regular as icon;
use serde_json::json;

use super::{
    Chat, Role,
    creations::Creations,
    game::Play,
    landscape::Landscape,
    loading::Loading,
    ops::{self, Context},
    theme,
};
use crate::{
    player::{InputBlocked, Player, spawn_point},
    world::{
        SEED, VoxelWorld,
        ledger::{self, Ledger, game_name, world_name},
    },
};

/// Where to go next.
#[derive(Clone, Debug, PartialEq)]
pub enum Trip {
    /// A world in one of the local games.
    Local { game: String, world: String },
    /// Another world of the game the server we're on hosts.
    Remote { world: String },
    /// A hosted game, `host:port`.
    Server(String),
}

#[derive(Resource, Default)]
pub struct Travel {
    pub next: Option<Trip>,
    /// Going through a portal from this place (as a portal there would name it):
    /// arrive by a portal back.
    from_portal: Option<String>,
    /// Where we were before, for Back.
    back: Option<Trip>,
    /// The portal the hero stands at: its build and where it leads.
    near: Option<(u64, PortalTo)>,
    /// Close the server we host before the next trip (Leave while hosting).
    pub stop_hosting: bool,
}

impl Travel {
    /// Where the portal the hero stands at leads, if they stand at one.
    pub fn near(&self) -> Option<&PortalTo> {
        self.near.as_ref().map(|(_, to)| to)
    }

    /// Asks to go through a portal to `to`, or says why it can't.
    pub fn portal(&mut self, to: &PortalTo, ledger: &Ledger) -> Result<(), String> {
        let (trip, from) = match to {
            PortalTo::Server { addr } => (Trip::Server(addr.clone()), here(ledger)),
            PortalTo::World { name } if ledger.is_online() => (Trip::Remote { world: name.clone() }, world_of(ledger)),
            PortalTo::World { name } => (Trip::Local { game: game_name(), world: name.clone() }, world_name()),
            PortalTo::Game { game, world } => (Trip::Local { game: game.clone(), world: world.clone() }, here(ledger)),
        };
        if let Trip::Local { game, world } = &trip
            && ledger::read_game(game).world(world).is_none()
        {
            return Err(format!("There's no world \"{world}\" in the game \"{game}\" on this computer."));
        }
        self.next = Some(trip);
        self.from_portal = Some(from);
        Ok(())
    }
}

/// The world we're in, by name.
fn world_of(ledger: &Ledger) -> String {
    match &ledger.online {
        Some(online) => online.world.clone().unwrap_or_default(),
        None => world_name(),
    }
}

/// Where we are, as a portal elsewhere would name it: `game/world` locally, the
/// address on a server.
fn here(ledger: &Ledger) -> String {
    match &ledger.online {
        Some(online) => online.addr.clone(),
        None => format!("{}/{}", game_name(), world_name()),
    }
}

/// Where we are, for people: "home · local", "arena · 192.168.1.4:7878".
pub fn current_title(ledger: &Ledger) -> String {
    match &ledger.online {
        Some(online) => format!("{} · {}", online.world.as_deref().unwrap_or("…"), online.addr),
        None => format!("{} · {}", world_name(), game_name()),
    }
}

/// The trip back to where we are now.
fn trip_here(ledger: &Ledger) -> Trip {
    match &ledger.online {
        Some(online) => Trip::Server(online.addr.clone()),
        None => Trip::Local { game: game_name(), world: world_name() },
    }
}

/// Takes the trip asked for, if any.
#[allow(clippy::too_many_arguments)]
pub fn travel(
    mut commands: Commands,
    mut travel: ResMut<Travel>,
    mut ledger: ResMut<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut play: ResMut<Play>,
    mut loading: ResMut<Loading>,
    mut chat: ResMut<Chat>,
    mut heroes: Query<(&mut Transform, &mut Player)>,
) {
    let Some(trip) = travel.next.take() else { return };
    let from_portal = travel.from_portal.take();
    if creations.draft.is_some() || landscape.draft.is_some() {
        chat.say(Role::System, "Keep or discard your draft first.");
        return;
    }
    let leaving = trip_here(&ledger);
    if std::mem::take(&mut travel.stop_hosting)
        && let Some(server) = ledger.hosting.take()
    {
        // Closed before the trip, so its logs are done with when the local world loads them.
        server.stop();
        chat.say(Role::System, "Stopped hosting; the others were disconnected.");
    }
    match trip {
        Trip::Remote { world: name } => {
            if let Err(why) = ledger.enter_remote(&name, from_portal) {
                chat.say(Role::System, why);
            }
        }
        _ if ledger.hosting.is_some() => {
            chat.say(Role::System, "You're hosting this game, so you stay; Leave closes it.");
        }
        Trip::Server(addr) => {
            ledger.online = None;
            match ledger.go_online(&addr) {
                Ok(()) => chat.say(Role::System, format!("Going to {addr}…")),
                Err(e) => chat.say(Role::System, format!("⚠ {e}")),
            }
            if let Some(online) = &mut ledger.online {
                online.arriving_from = from_portal;
            }
            travel.back = Some(leaving);
        }
        Trip::Local { game, world: name } => {
            if !ledger.is_online() && game == game_name() && name == world_name() {
                chat.say(Role::System, format!("You're in {name} already."));
                return;
            }
            if ledger::read_game(&game).world(&name).is_none() {
                chat.say(Role::System, format!("There's no world \"{name}\" in {game}."));
                return;
            }
            // Within a game, what the player carries goes with them.
            let me = ledger.profile.id.clone();
            let carried = ledger.logic.state.carried(&me);
            let stats = ledger.logic.state.carried_stats(&me);
            let carry = !ledger.is_online() && game == game_name() && !(carried.is_empty() && stats.is_empty()) && ledger.land.is_admin(&me);
            if carry {
                let depart = Action::Depart { player: me.clone(), to: name.clone(), items: carried.clone(), stats: stats.clone() };
                ledger.record(None, None, depart);
            }
            let left_world = world_name();
            // The world left behind loads fast next time.
            if !ledger.is_online() {
                ledger::write_snapshot(&ledger, world.edit_list());
            }
            ledger.online = None;
            ledger::set_place(&game, &name);
            let (fresh, edits) = ledger::load_local(SEED);
            *ledger = fresh;
            if carry {
                ledger.record(None, None, Action::Arrive { player: me, from: left_world, items: carried, stats });
            }
            world.reset(&mut commands, ledger.log.terrain(), edits);
            creations.resync(&mut commands, &ledger.builds);
            landscape.forget_history();
            play.leave_world();
            let spot = from_portal
                .and_then(|from| arrival(&ledger, &from))
                .unwrap_or_else(|| spawn_point(ledger.log.terrain(), IVec2::ZERO));
            if let Ok((mut transform, mut player)) = heroes.single_mut() {
                transform.translation = spot;
                player.arrive();
            }
            loading.restart(&name);
            travel.back = Some(leaving);
            let ground = if ledger.log.flat { "flat" } else { "natural" };
            chat.say(Role::System, format!("Entered {} ({ground}, {} changes).", current_title(&ledger), ledger.log.changes.len()));
        }
    }
}

/// In front of a portal here that leads back to `from` (as [`PortalTo::label`] names
/// it), if there is one.
pub fn arrival(ledger: &Ledger, from: &str) -> Option<Vec3> {
    let (id, _) = ledger.portals.iter().find(|(_, to)| to.label() == from)?;
    let b = ledger.builds.get(id)?;
    let depth = b.spec.size.get(2).copied().unwrap_or(8) as f32 * BUILD_VOXEL / 2.0 + 1.5;
    let front = Quat::from_rotation_y(b.yaw) * Vec3::Z * depth;
    Some(Vec3::from_array(b.position) + front + Vec3::Y * 0.5)
}

/// How close to a portal's footprint counts as at it, in meters.
const REACH: f32 = 1.5;

/// Whether `feet` stands at `build`: inside its footprint, or within a step of it.
fn at_portal(build: &BuildState, feet: Vec3) -> bool {
    let size = |i: usize| build.spec.size.get(i).copied().unwrap_or(0) as f32 * BUILD_VOXEL;
    let local = Quat::from_rotation_y(-build.yaw) * (feet - Vec3::from_array(build.position));
    local.x.abs() <= size(0) / 2.0 + REACH && local.z.abs() <= size(2) / 2.0 + REACH && local.y > -1.0 && local.y < size(1) + 0.5
}

/// Notices the portal the hero stands at; E goes through (unless a game piece is in
/// reach, whose E comes first).
#[allow(clippy::too_many_arguments)]
pub fn use_portals(
    keys: Res<ButtonInput<KeyCode>>,
    blocked: Res<InputBlocked>,
    ledger: Res<Ledger>,
    play: Res<Play>,
    loading: Res<Loading>,
    mut travel: ResMut<Travel>,
    mut chat: ResMut<Chat>,
    heroes: Query<&Transform, With<Player>>,
) {
    let Ok(hero) = heroes.single() else { return };
    travel.near = ledger
        .portals
        .iter()
        .find(|(id, _)| ledger.builds.get(id).is_some_and(|b| at_portal(b, hero.translation)))
        .map(|(id, to)| (*id, to.clone()));
    if blocked.0 || !loading.is_done() || play.near.is_some() || play.playing.is_some() || !keys.just_pressed(KeyCode::KeyE) {
        return;
    }
    if let Some(to) = travel.near().cloned()
        && let Err(why) = travel.portal(&to, &ledger)
    {
        chat.say(Role::System, why);
    }
}

/// Where the portal the hero stands at leads, over the view.
pub fn portal_prompt(mut contexts: EguiContexts, travel: Res<Travel>, chat: Res<Chat>) -> Result {
    let Some(to) = travel.near() else { return Ok(()) };
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let covered = chat.covered * ctx.viewport_rect().width();
    egui::Area::new("portal_prompt".into())
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(-covered / 2.0, -60.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.overlay)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(theme::ROUND + 1)
                .inner_margin(egui::Margin::symmetric(14, 8))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        let kind = match to {
                            PortalTo::World { .. } => "A portal to a world of this game",
                            PortalTo::Game { .. } => "A portal to another of your games",
                            PortalTo::Server { .. } => "A portal to a hosted game",
                        };
                        ui.label(egui::RichText::new(format!("{} {}", icon::SPIRAL, to.label())).size(18.0).color(p.strong));
                        ui.label(egui::RichText::new(format!("{kind} · E to go")).color(p.muted).small());
                    });
                });
        });
    Ok(())
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Tab {
    #[default]
    Worlds,
    Saves,
    Portals,
    Rules,
    Games,
}

/// A world as the window lists it.
struct Row {
    name: String,
    flat: bool,
    /// How many changes and when last changed, when it's on this computer.
    detail: Option<(usize, Option<SystemTime>)>,
}

#[derive(Resource)]
pub struct WorldsWindow {
    pub open: bool,
    tab: Tab,
    /// The worlds and local games, read when the window opens, after changes, and in
    /// each new place.
    rows: Option<(Vec<Row>, Vec<ledger::GameInfo>)>,
    /// The game's packages, read with the rows.
    catalog: Vec<std::sync::Arc<Package>>,
    /// Where they were read.
    listed_in: String,
    new_name: String,
    new_flat: bool,
    new_game: String,
    /// A world to remove, waiting for "yes".
    removing: Option<String>,
    save_name: String,
    game_save_name: String,
    /// The save to fork, and the new world's name.
    fork: Option<(String, String)>,
    link: String,
    server: String,
}

impl Default for WorldsWindow {
    fn default() -> Self {
        Self {
            open: false,
            tab: Tab::default(),
            rows: None,
            catalog: Vec::new(),
            listed_in: String::new(),
            new_name: String::new(),
            new_flat: true,
            new_game: String::new(),
            removing: None,
            save_name: String::new(),
            game_save_name: String::new(),
            fork: None,
            link: String::new(),
            server: String::new(),
        }
    }
}

impl WorldsWindow {
    /// Reads the lists again next frame (the game changed).
    pub fn refresh(&mut self) {
        self.rows = None;
    }
}

/// O opens and closes it; `GENVERSE_WORLDS=1` opens it at start.
pub fn toggle_worlds(keys: Res<ButtonInput<KeyCode>>, blocked: Res<InputBlocked>, mut window: ResMut<WorldsWindow>, mut started: Local<bool>) {
    if !*started {
        *started = true;
        window.open = std::env::var("GENVERSE_WORLDS").is_ok();
    }
    if !blocked.0 && keys.just_pressed(KeyCode::KeyO) {
        window.open = !window.open;
        window.rows = None;
    }
}

enum Do {
    Go(Trip),
    /// Back to the local game; closes the game we host, if we host one.
    Leave,
    NewWorld,
    RemoveWorld(String),
    Save,
    Load(u64),
    Fork(String, String),
    SaveGame,
    LoadGame(String),
    NewGame,
    Portal(PortalTo),
    TurnOn(String),
    TurnOff(String),
    RemovePackage(String),
    Link(u64),
    Unlink(u64),
    Host,
}

fn ago(at: Option<SystemTime>) -> String {
    let Some(seconds) = at.and_then(|t| t.elapsed().ok()).map(|d| d.as_secs()) else { return "never".into() };
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

/// The recent servers, newest first.
fn recent_servers() -> Vec<String> {
    super::prefs::get("recent_servers").and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

fn remember_server(addr: &str) {
    let mut list = recent_servers();
    list.retain(|a| a != addr);
    list.insert(0, addr.to_string());
    list.truncate(8);
    super::prefs::set("recent_servers", serde_json::json!(list));
}

/// How a new world's ground is named in an op's arguments.
fn ground(flat: bool) -> &'static str {
    if flat { "flat" } else { "natural" }
}

/// A seed for a new natural world.
pub fn fresh_seed(flat: bool) -> u32 {
    if flat { SEED } else { SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(SEED, |d| d.subsec_nanos()) }
}

/// The current game's worlds, and the local games.
fn rows(ledger: &Ledger) -> (Vec<Row>, Vec<ledger::GameInfo>) {
    let worlds = match &ledger.online {
        Some(online) => online
            .game
            .as_ref()
            .map(|g| g.worlds().into_iter().map(|(name, ground)| Row { name, flat: ground.flat, detail: None }).collect())
            .unwrap_or_default(),
        None => ledger::list_worlds()
            .into_iter()
            .map(|w| Row { name: w.name, flat: w.flat, detail: Some((w.changes, w.modified)) })
            .collect(),
    };
    (worlds, ledger::list_games())
}

#[allow(clippy::too_many_arguments)]
pub fn worlds_window(
    mut contexts: EguiContexts,
    mut window: ResMut<WorldsWindow>,
    mut travel: ResMut<Travel>,
    mut ledger: ResMut<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut chat: ResMut<Chat>,
    heroes: Query<&Transform, With<Player>>,
) -> Result {
    if !window.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let title = current_title(&ledger);
    if window.listed_in != title {
        window.rows = None;
        window.listed_in = title.clone();
    }
    if window.rows.is_none() {
        window.rows = Some(rows(&ledger));
        let catalog = match &ledger.online {
            Some(online) => online.game.as_ref().map(|g| g.packages()).unwrap_or_default(),
            None => ledger::read_game(&game_name()).packages(),
        };
        window.catalog = catalog.into_values().collect();
    }
    let (worlds, games) = window.rows.take().unwrap_or_default();
    let online = ledger.is_online();
    let here_world = world_of(&ledger);
    let game = match &ledger.online {
        Some(online) => online.game.as_ref().map_or(online.addr.clone(), |g| g.name.clone()),
        None => game_name(),
    };
    let admin = ledger.land.is_admin(&ledger.profile.id);
    let hero = heroes.single().map(|t| t.translation).unwrap_or_default();
    let mut todo = None;
    let mut open = true;

    egui::Window::new(format!("{} {game}", icon::GLOBE_HEMISPHERE_WEST))
        .id(egui::Id::new("game_window"))
        .open(&mut open)
        .default_width(580.0)
        .default_height(480.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("You're in {here_world}")).strong().color(p.strong));
                let hosting = match (&ledger.hosting, &ledger.online) {
                    (Some(server), _) => {
                        format!("hosting on port {} · {} playing", server.addr.port(), server.players.load(std::sync::atomic::Ordering::Relaxed))
                    }
                    (None, Some(online)) => format!("on {}", online.addr),
                    (None, None) => "on this computer".into(),
                };
                ui.label(egui::RichText::new(hosting).small().color(p.muted));
                if let Some(back) = travel.back.clone() {
                    let label = match &back {
                        Trip::Local { world, .. } | Trip::Remote { world } => world.clone(),
                        Trip::Server(addr) => addr.clone(),
                    };
                    if ui.button(format!("{} Back to {label}", icon::ARROW_BEND_UP_LEFT)).clicked() {
                        todo = Some(Do::Go(back));
                    }
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let tabs = [(Tab::Worlds, "Worlds"), (Tab::Saves, "Saves"), (Tab::Portals, "Portals"), (Tab::Rules, "Rules"), (Tab::Games, "Games")];
                for (tab, title) in tabs {
                    ui.selectable_value(&mut window.tab, tab, title);
                }
            });
            ui.separator();
            match window.tab {
                Tab::Worlds => {
                    if admin {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut window.new_name).hint_text("new world name").desired_width(160.0));
                            ui.selectable_value(&mut window.new_flat, true, "Flat").on_hover_text("Empty and flat: for games and building");
                            ui.selectable_value(&mut window.new_flat, false, "Natural").on_hover_text("A fresh landscape of its own");
                            let ok = ledger::clean_name(&window.new_name).is_some();
                            if ui.add_enabled(ok, egui::Button::new(format!("{} New world", icon::PLUS))).clicked() {
                                todo = Some(Do::NewWorld);
                            }
                        });
                    } else {
                        theme::note(ui, "The game's admins make and remove worlds.");
                    }
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        for w in &worlds {
                            let current = w.name == here_world;
                            ui.horizontal(|ui| {
                                let name = egui::RichText::new(&w.name).strong();
                                ui.label(if current { name.color(p.primary) } else { name.color(p.strong) });
                                let ground = if w.flat { "flat" } else { "natural" };
                                let detail = match w.detail {
                                    Some((changes, at)) => format!("{ground} · {changes} changes · {}", ago(at)),
                                    None => ground.to_string(),
                                };
                                ui.label(egui::RichText::new(detail).small().color(p.muted));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if window.removing.as_deref() == Some(w.name.as_str()) {
                                        if ui.button("Yes, remove it").clicked() {
                                            todo = Some(Do::RemoveWorld(w.name.clone()));
                                        }
                                        if ui.button("Keep").clicked() {
                                            window.removing = None;
                                        }
                                        ui.label(egui::RichText::new("Its files go to the trash.").small());
                                    } else if admin
                                        && !current
                                        && worlds.len() > 1
                                        && theme::icon_button(ui, icon::TRASH, "Remove it (its files go to the trash)")
                                    {
                                        window.removing = Some(w.name.clone());
                                    }
                                    if current {
                                        ui.label(egui::RichText::new("here").small().color(p.primary));
                                    } else if ui.button("Enter").clicked() {
                                        let trip = if online {
                                            Trip::Remote { world: w.name.clone() }
                                        } else {
                                            Trip::Local { game: game_name(), world: w.name.clone() }
                                        };
                                        todo = Some(Do::Go(trip));
                                    }
                                });
                            });
                            ui.separator();
                        }
                    });
                }
                Tab::Saves => {
                    ui.label(egui::RichText::new("The whole game").strong());
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut window.game_save_name).hint_text("game save name").desired_width(180.0));
                        let ok = !window.game_save_name.trim().is_empty();
                        if ui.add_enabled(ok, egui::Button::new(format!("{} Save every world", icon::FLOPPY_DISK))).clicked() {
                            todo = Some(Do::SaveGame);
                        }
                    });
                    let game_log = match &ledger.online {
                        Some(online) => online.game.clone(),
                        None => Some(ledger::read_game(&game_name())),
                    };
                    if let Some(game_log) = &game_log {
                        let mut seen = std::collections::HashSet::new();
                        for (c, name, worlds) in game_log.saves().rev().filter(|(_, n, _)| seen.insert(n.to_lowercase())).take(8) {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(name).color(p.strong));
                                let who = ledger.describe(&c.by);
                                ui.label(egui::RichText::new(format!("{} worlds · {who}", worlds.len())).small().color(p.muted));
                                if admin && ui.small_button("Load").on_hover_text("Put every world back to this save").clicked() {
                                    todo = Some(Do::LoadGame(name.to_string()));
                                }
                            });
                        }
                    }
                    ui.separator();
                    ui.label(egui::RichText::new(format!("This world ({here_world})")).strong());
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut window.save_name).hint_text("save name").desired_width(180.0));
                        let ok = !window.save_name.trim().is_empty();
                        if ui.add_enabled(ok, egui::Button::new(format!("{} Save now", icon::FLOPPY_DISK))).clicked() {
                            todo = Some(Do::Save);
                        }
                    });
                    theme::note(ui, "Anyone can save. Loading puts it back for everyone (admins); the world is saved first, so you can load it back.");
                    let mut seen = std::collections::HashSet::new();
                    let saves: Vec<_> = ledger.log.saves().rev().filter(|(_, n)| seen.insert(n.to_lowercase())).collect();
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        for (c, name) in saves {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(name).strong().color(p.strong));
                                ui.label(egui::RichText::new(ledger.describe(&c.by)).small().color(p.muted));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if !online && admin && ui.button("Fork…").on_hover_text("A new world in this game, from this save").clicked() {
                                        window.fork = Some((name.to_string(), format!("{here_world}-{name}").to_lowercase()));
                                    }
                                    if admin && ui.button("Load").on_hover_text("Put the world back to this save, for everyone").clicked() {
                                        todo = Some(Do::Load(c.id));
                                    }
                                });
                            });
                            if let Some((save, world_name)) = &mut window.fork
                                && save == name
                            {
                                ui.horizontal(|ui| {
                                    ui.label("New world:");
                                    ui.add(egui::TextEdit::singleline(world_name).desired_width(160.0));
                                    if ui.button("Fork").clicked() {
                                        todo = Some(Do::Fork(save.clone(), world_name.clone()));
                                    }
                                });
                            }
                            ui.separator();
                        }
                    });
                }
                Tab::Portals => {
                    match creations.selected().map(|b| (b.build, b.name.clone())) {
                        Some((build, name)) => {
                            ui.horizontal(|ui| {
                                ui.label(format!("Link \"{name}\" to"));
                                egui::ComboBox::from_id_salt("portal_world")
                                    .selected_text(if window.link.is_empty() { "a world…".to_string() } else { window.link.clone() })
                                    .show_ui(ui, |ui| {
                                        for w in worlds.iter().filter(|w| w.name != here_world) {
                                            ui.selectable_value(&mut window.link, w.name.clone(), &w.name);
                                        }
                                    });
                                ui.add(egui::TextEdit::singleline(&mut window.link).hint_text("or game/world, host:port").desired_width(150.0));
                                if ui.add_enabled(!window.link.trim().is_empty(), egui::Button::new("Link")).clicked() {
                                    todo = Some(Do::Link(build));
                                }
                            });
                        }
                        None => theme::note(ui, "Select a build to make it a portal, or ask the agent: \"a stone arch, a portal to sokoban\"."),
                    }
                    ui.add_space(6.0);
                    if ledger.portals.is_empty() {
                        theme::note(ui, "No portals in this world yet.");
                    }
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        for (id, to) in &ledger.portals {
                            let Some(b) = ledger.builds.get(id) else { continue };
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&b.name).strong().color(p.strong));
                                ui.label(format!("{} {}", icon::ARROW_RIGHT, to.label()));
                                let far = Vec3::from_array(b.position).distance(hero);
                                ui.label(egui::RichText::new(format!("{far:.0} m away")).small().color(p.muted));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("Unlink").clicked() {
                                        todo = Some(Do::Unlink(*id));
                                    }
                                    if ui.button("Go").on_hover_text("Travel there now").clicked() {
                                        todo = Some(Do::Portal(to.clone()));
                                    }
                                });
                            });
                            ui.separator();
                        }
                    });
                }
                Tab::Rules => {
                    theme::note(ui, "The game's rules are installed once for the whole game; each world turns on the ones it plays. A new version reaches every world that runs it.");
                    if window.catalog.is_empty() {
                        theme::note(ui, "None yet: ask the agent for a game, and its rules land here.");
                    }
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        for package in &window.catalog {
                            let name = &package.manifest.name;
                            let here = ledger.logic.installed.get(name);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&package.manifest.title).strong().color(p.strong));
                                let state = match here {
                                    Some(i) if i.package.hash == package.hash() => "on here",
                                    Some(_) => "on here (an older version)",
                                    None => "off here",
                                };
                                ui.label(egui::RichText::new(format!("{name} v{} · {state}", package.manifest.version)).small().color(p.muted));
                                if admin {
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if theme::icon_button(ui, icon::TRASH, "Take it out of the game (every world stops running it)") {
                                            todo = Some(Do::RemovePackage(name.clone()));
                                        }
                                        if here.is_some() {
                                            if ui.button("Turn off here").clicked() {
                                                todo = Some(Do::TurnOff(name.clone()));
                                            }
                                        } else if ui.button("Turn on here").clicked() {
                                            todo = Some(Do::TurnOn(name.clone()));
                                        }
                                    });
                                }
                            });
                            ui.separator();
                        }
                    });
                }
                Tab::Games => {
                    ui.label(egui::RichText::new("On this computer").strong());
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut window.new_game).hint_text("new game name").desired_width(160.0));
                        ui.selectable_value(&mut window.new_flat, true, "Flat");
                        ui.selectable_value(&mut window.new_flat, false, "Natural");
                        let ok = ledger::clean_name(&window.new_game).is_some();
                        if ui.add_enabled(ok, egui::Button::new(format!("{} New game", icon::PLUS))).clicked() {
                            todo = Some(Do::NewGame);
                        }
                    });
                    for g in &games {
                        ui.horizontal(|ui| {
                            let current = !online && g.name == game_name();
                            let name = egui::RichText::new(&g.name).strong();
                            ui.label(if current { name.color(p.primary) } else { name.color(p.strong) });
                            ui.label(egui::RichText::new(format!("{} worlds · {}", g.worlds, ago(g.modified))).small().color(p.muted));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if current {
                                    ui.label(egui::RichText::new("here").small().color(p.primary));
                                } else if ui.button("Enter").clicked()
                                    && let Some(world) = ledger::read_game(&g.name).start()
                                {
                                    todo = Some(Do::Go(Trip::Local { game: g.name.clone(), world }));
                                }
                            });
                        });
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("Hosted games").strong());
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut window.server).hint_text("host:port").desired_width(200.0));
                        if ui.add_enabled(!window.server.trim().is_empty(), egui::Button::new("Join")).clicked() {
                            todo = Some(Do::Go(Trip::Server(window.server.trim().to_string())));
                        }
                    });
                    for addr in recent_servers() {
                        ui.horizontal(|ui| {
                            ui.label(&addr);
                            if ui.small_button("Join").clicked() {
                                todo = Some(Do::Go(Trip::Server(addr.clone())));
                            }
                        });
                    }
                    ui.separator();
                    if online {
                        let (label, tip) = if ledger.hosting.is_some() {
                            ("Stop hosting", "Close this game to others and go back to your local world")
                        } else {
                            ("Leave", "Back to your own game")
                        };
                        if ui.button(label).on_hover_text(tip).clicked() {
                            todo = Some(Do::Leave);
                        }
                    } else if ui
                        .button(format!("Host {}", game_name()))
                        .on_hover_text("Others on your network can join every world in it until you stop hosting")
                        .clicked()
                    {
                        todo = Some(Do::Host);
                    }
                }
            }
        });
    window.open = open;
    window.rows = Some((worlds, games));

    let Some(todo) = todo else { return Ok(()) };
    window.rows = None;
    // Going somewhere is the window's own; everything else is an op, as in the chat.
    let (op, args) = match todo {
        Do::Go(trip) => {
            if let Trip::Server(addr) = &trip {
                remember_server(addr);
            }
            travel.next = Some(trip);
            return Ok(());
        }
        Do::Portal(to) => {
            if let Err(why) = travel.portal(&to, &ledger) {
                chat.say(Role::System, why);
            }
            return Ok(());
        }
        Do::Leave => ("leave", json!({})),
        Do::Host => ("host", json!({})),
        Do::NewWorld => ("newworld", json!({ "name": std::mem::take(&mut window.new_name), "ground": ground(window.new_flat) })),
        Do::RemoveWorld(name) => {
            window.removing = None;
            ("removeworld", json!({ "name": name }))
        }
        Do::Save => ("save", json!({ "name": std::mem::take(&mut window.save_name) })),
        Do::SaveGame => ("savegame", json!({ "name": std::mem::take(&mut window.game_save_name) })),
        Do::LoadGame(name) => ("loadgame", json!({ "name": name })),
        Do::Fork(save, name) => {
            window.fork = None;
            ("fork", json!({ "save": save, "world": name }))
        }
        Do::Load(id) => ("load", json!({ "id": id })),
        Do::NewGame => ("newgame", json!({ "name": std::mem::take(&mut window.new_game), "ground": ground(window.new_flat) })),
        Do::TurnOn(name) => ("turn_on", json!({ "name": name })),
        Do::TurnOff(name) => ("uninstall", json!({ "name": name })),
        Do::RemovePackage(name) => ("remove_package", json!({ "name": name })),
        Do::Link(build) => ("portal", json!({ "build": build, "to": window.link })),
        Do::Unlink(build) => ("portal", json!({ "build": build, "to": "off" })),
    };
    let cx = &mut Context {
        ledger: &mut ledger,
        world: &mut world,
        creations: &mut creations,
        landscape: &mut landscape,
        travel: &mut travel,
        at: hero.to_array(),
        agent: None,
    };
    let reply = Some(ops::said(ops::run(op, args, cx)));
    if let Some(reply) = reply.filter(|r| !r.is_empty()) {
        chat.say(Role::System, reply);
    }
    Ok(())
}

/// What to do about a change to the game the server sent: say it, if it's ours.
pub fn game_reply(action: Option<&GameAction>) -> Option<String> {
    Some(match action? {
        GameAction::CreateWorld { name, .. } => format!("Made the world {name}; enter it from the Game window (O)."),
        GameAction::RemoveWorld { name } => format!("Removed {name}."),
        GameAction::SetAdmins { .. } => "Changed who runs the game.".into(),
        GameAction::Save { name, worlds } => format!("Saved the game as \"{name}\" ({} worlds).", worlds.len()),
        GameAction::AddPackage { package } => format!("The game has {} v{} now.", package.manifest.title, package.manifest.version),
        GameAction::RemovePackage { name } => format!("Took {name} out of the game."),
        GameAction::LoadSave { name, .. } => format!("Loaded the game save \"{name}\": every world went back."),
    })
}
