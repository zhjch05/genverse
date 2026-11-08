//! Many worlds (`docs/worlds.md`): going from one to another, the Worlds window (O, or
//! the globe button), and portals.
//!
//! - **Trips:** every way into a world (the window, `/world`, a portal, joining or
//!   leaving a server) asks for a [`Trip`]; [`travel`] takes it at the end of the frame.
//!   The world left behind is saved, the new one loads, the ground resets to its seed,
//!   and the hero lands at its spawn (or by a portal leading back) behind the loading
//!   screen.
//! - **The window:** Worlds (enter, new, fork, trash), Saves (save, load, fork),
//!   Portals (go, link, unlink), Servers (join, host, leave).
//! - **Portals:** builds linked to a world by [`Action::SetPortal`]. Walking up to one
//!   shows where it leads; E goes.

use std::time::SystemTime;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use cubewish_rules::{
    build::VOXEL_SIZE as BUILD_VOXEL,
    log::{Action, BuildState, PortalTo},
};
use egui_phosphor::regular as icon;

use super::{
    Chat, Role,
    commands::{self, Context},
    creations::Creations,
    game::Play,
    landscape::Landscape,
    loading::Loading,
    theme,
};
use crate::{
    player::{InputBlocked, Player, spawn_point},
    world::{
        SEED, VoxelWorld,
        ledger::{self, Ledger, WorldInfo, world_name},
    },
};

/// Where to go next.
#[derive(Clone, Debug, PartialEq)]
pub enum Trip {
    /// A local world (`None`: Home).
    Local(Option<String>),
    /// A server, `host:port`.
    Server(String),
}

#[derive(Resource, Default)]
pub struct Travel {
    pub next: Option<Trip>,
    /// Going through a portal: arrive by a portal back to the world left.
    through_portal: bool,
    /// The local world before this one, for Back.
    back: Option<Option<String>>,
    /// The portal the hero stands at: its build and where it leads.
    near: Option<(u64, PortalTo)>,
}

impl Travel {
    /// Where the portal the hero stands at leads, if they stand at one.
    pub fn near(&self) -> Option<&PortalTo> {
        self.near.as_ref().map(|(_, to)| to)
    }

    /// Asks to go through `portal`, or says why it can't.
    pub fn portal(&mut self, to: &PortalTo) -> Result<(), String> {
        self.next = Some(match to {
            PortalTo::Server { addr } => Trip::Server(addr.clone()),
            PortalTo::World { name } if name == "home" => Trip::Local(None),
            PortalTo::World { name } => {
                if !ledger::list_worlds().iter().any(|w| w.name.as_deref() == Some(name)) {
                    return Err(format!("There's no world \"{name}\" on this computer (it may be on someone else's)."));
                }
                Trip::Local(Some(name.clone()))
            }
        });
        self.through_portal = true;
        Ok(())
    }
}

/// What the current world is called, for people.
pub fn current_title(ledger: &Ledger) -> String {
    match &ledger.online {
        Some(online) => online.addr.clone(),
        None => world_name().unwrap_or("home".into()),
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
    let through_portal = std::mem::take(&mut travel.through_portal);
    if creations.draft.is_some() || landscape.draft.is_some() {
        chat.say(Role::System, "Keep or discard your draft first.");
        return;
    }
    if ledger.hosting.is_some() {
        chat.say(Role::System, "You're hosting this world, so you stay; it closes when you quit.");
        return;
    }
    let left = current_title(&ledger);
    let left_local = (!ledger.is_online()).then(world_name);
    match trip {
        Trip::Server(addr) => {
            ledger.online = None;
            match ledger.go_online(&addr) {
                Ok(()) => chat.say(Role::System, format!("Going to {addr}…")),
                Err(e) => chat.say(Role::System, format!("⚠ {e}")),
            }
            if let Some(local) = left_local {
                travel.back = Some(local);
            }
        }
        Trip::Local(name) => {
            if !ledger.is_online() && world_name() == name {
                chat.say(Role::System, format!("You're in {left} already."));
                return;
            }
            if let Some(n) = &name
                && !std::path::Path::new(&format!("saves/worlds/{n}")).exists()
            {
                chat.say(Role::System, format!("There's no world \"{n}\"."));
                return;
            }
            // The world left behind loads fast next time.
            if !ledger.is_online() {
                ledger::write_snapshot(&ledger, world.edit_list());
            }
            ledger.online = None;
            ledger::set_world(name.clone());
            let (fresh, edits) = ledger::load_local(SEED);
            *ledger = fresh;
            world.reset(&mut commands, ledger.log.terrain(), edits);
            creations.resync(&mut commands, &ledger.builds);
            landscape.forget_history();
            play.leave_world();
            let title = current_title(&ledger);
            let spot = through_portal
                .then(|| arrival(&ledger, &left))
                .flatten()
                .unwrap_or_else(|| spawn_point(ledger.log.terrain(), IVec2::ZERO));
            if let Ok((mut transform, mut player)) = heroes.single_mut() {
                transform.translation = spot;
                player.arrive();
            }
            loading.restart(&title);
            if left_local.as_ref() != Some(&name) {
                travel.back = left_local;
            }
            let ground = if ledger.log.flat { "flat" } else { "natural" };
            chat.say(Role::System, format!("Entered {title} ({ground}, {} changes).", ledger.log.changes.len()));
        }
    }
}

/// In front of a portal here that leads back to `from`, if there is one.
fn arrival(ledger: &Ledger, from: &str) -> Option<Vec3> {
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
    if let Some((_, to)) = travel.near.clone()
        && let Err(why) = travel.portal(&to)
    {
        chat.say(Role::System, why);
    }
}

/// Where the portal the hero stands at leads, over the view.
pub fn portal_prompt(mut contexts: EguiContexts, travel: Res<Travel>, chat: Res<Chat>) -> Result {
    let Some((_, to)) = &travel.near else { return Ok(()) };
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
                        let kind = if matches!(to, PortalTo::Server { .. }) { "server" } else { "world" };
                        ui.label(egui::RichText::new(format!("{} {}", icon::SPIRAL, to.label())).size(18.0).color(p.strong));
                        ui.label(egui::RichText::new(format!("A portal to the {kind} · E to go")).color(p.muted).small());
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
    Servers,
}

#[derive(Resource)]
pub struct WorldsWindow {
    pub open: bool,
    tab: Tab,
    /// The local worlds, read when the window opens, after changes, and in each world.
    list: Option<Vec<WorldInfo>>,
    /// The world the list was read in.
    listed_in: String,
    new_name: String,
    new_flat: bool,
    /// A world to trash, waiting for "yes".
    trashing: Option<String>,
    save_name: String,
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
            list: None,
            listed_in: String::new(),
            new_name: String::new(),
            new_flat: true,
            trashing: None,
            save_name: String::new(),
            fork: None,
            link: String::new(),
            server: String::new(),
        }
    }
}

/// O opens and closes it; `CUBEWISH_WORLDS=1` opens it at start.
pub fn toggle_worlds(keys: Res<ButtonInput<KeyCode>>, blocked: Res<InputBlocked>, mut window: ResMut<WorldsWindow>, mut started: Local<bool>) {
    if !*started {
        *started = true;
        window.open = std::env::var("CUBEWISH_WORLDS").is_ok();
    }
    if !blocked.0 && keys.just_pressed(KeyCode::KeyO) {
        window.open = !window.open;
        window.list = None;
    }
}

enum Do {
    Enter(Option<String>),
    Create,
    Trash(String),
    Save,
    Load(u64),
    Fork(String, String),
    Go(PortalTo),
    Link(u64),
    Unlink(u64),
    Join(String),
    Host,
    Leave,
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

#[allow(clippy::too_many_arguments)]
pub fn worlds_window(
    mut contexts: EguiContexts,
    mut window: ResMut<WorldsWindow>,
    mut travel: ResMut<Travel>,
    mut commands: Commands,
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
    let here = current_title(&ledger);
    if window.listed_in != here {
        window.list = None;
        window.listed_in = here.clone();
    }
    let list = window.list.get_or_insert_with(ledger::list_worlds).clone();
    let online = ledger.is_online();
    let admin = ledger.land.is_admin(&ledger.profile.id);
    let hero = heroes.single().map(|t| t.translation).unwrap_or_default();
    let mut todo = None;
    let mut open = true;

    egui::Window::new(format!("{} Worlds", icon::GLOBE_HEMISPHERE_WEST))
        .open(&mut open)
        .default_width(560.0)
        .default_height(460.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("You're in {here}")).strong().color(p.strong));
                if let Some(back) = travel.back.clone()
                    && ui.button(format!("{} Back to {}", icon::ARROW_BEND_UP_LEFT, back.as_deref().unwrap_or("home"))).clicked()
                {
                    todo = Some(Do::Enter(back));
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for (tab, title) in [(Tab::Worlds, "Worlds"), (Tab::Saves, "Saves"), (Tab::Portals, "Portals"), (Tab::Servers, "Servers")] {
                    ui.selectable_value(&mut window.tab, tab, title);
                }
            });
            ui.separator();
            match window.tab {
                Tab::Worlds => {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut window.new_name).hint_text("new world name").desired_width(160.0));
                        ui.selectable_value(&mut window.new_flat, true, "Flat").on_hover_text("Empty and flat: for games and building");
                        ui.selectable_value(&mut window.new_flat, false, "Natural").on_hover_text("A fresh landscape of its own");
                        let ok = ledger::clean_name(&window.new_name).is_some();
                        if ui.add_enabled(ok, egui::Button::new(format!("{} Create and enter", icon::PLUS))).clicked() {
                            todo = Some(Do::Create);
                        }
                    });
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        for w in &list {
                            let current = !online && w.name == world_name();
                            ui.horizontal(|ui| {
                                let name = egui::RichText::new(w.title()).strong();
                                ui.label(if current { name.color(p.primary) } else { name.color(p.strong) });
                                let ground = if w.flat { "flat" } else { "natural" };
                                ui.label(egui::RichText::new(format!("{ground} · {} changes · {}", w.changes, ago(w.modified))).small().color(p.muted));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if let Some(name) = &w.name {
                                        if window.trashing.as_deref() == Some(name.as_str()) {
                                            if ui.button("Yes, move it").clicked() {
                                                todo = Some(Do::Trash(name.clone()));
                                            }
                                            if ui.button("Keep").clicked() {
                                                window.trashing = None;
                                            }
                                            ui.label(egui::RichText::new("To saves/trash?").small());
                                        } else if !current
                                            && theme::icon_button(ui, icon::TRASH, "Move to trash (saves/trash; nothing is deleted)")
                                        {
                                            window.trashing = Some(name.clone());
                                        }
                                    }
                                    if current {
                                        ui.label(egui::RichText::new("here").small().color(p.primary));
                                    } else if ui.button("Enter").clicked() {
                                        todo = Some(Do::Enter(w.name.clone()));
                                    }
                                });
                            });
                            ui.separator();
                        }
                    });
                }
                Tab::Saves => {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut window.save_name).hint_text("save name").desired_width(180.0));
                        let ok = !window.save_name.trim().is_empty();
                        if ui.add_enabled(ok, egui::Button::new(format!("{} Save now", icon::FLOPPY_DISK))).clicked() {
                            todo = Some(Do::Save);
                        }
                    });
                    theme::note(ui, "Anyone can save. Loading puts the world back for everyone (admins), and can be undone.");
                    ui.add_space(6.0);
                    let undone = ledger.log.undone();
                    let mut seen = std::collections::HashSet::new();
                    let saves: Vec<_> = ledger.log.saves().rev().filter(|(_, n)| seen.insert(n.to_lowercase())).collect();
                    if saves.is_empty() {
                        theme::note(ui, "No saves yet.");
                    }
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        for (c, name) in saves {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(name).strong().color(p.strong));
                                let later = if undone.contains(&c.id) { " · after the world as it is" } else { "" };
                                ui.label(egui::RichText::new(format!("{}{later}", ledger.describe(&c.by))).small().color(p.muted));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("Fork…").on_hover_text("Start a new world of your own from this save").clicked() {
                                        window.fork = Some((name.to_string(), format!("{}-{}", here, name).to_lowercase()));
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
                    let selected = creations.selected().map(|b| (b.build, b.name.clone()));
                    match &selected {
                        Some((build, name)) => {
                            ui.horizontal(|ui| {
                                ui.label(format!("Link \"{name}\" to"));
                                egui::ComboBox::from_id_salt("portal_world")
                                    .selected_text(if window.link.is_empty() { "a world…".to_string() } else { window.link.clone() })
                                    .show_ui(ui, |ui| {
                                        for w in &list {
                                            ui.selectable_value(&mut window.link, w.title().to_string(), w.title());
                                        }
                                    });
                                ui.add(egui::TextEdit::singleline(&mut window.link).hint_text("or host:port").desired_width(120.0));
                                if ui.add_enabled(!window.link.trim().is_empty(), egui::Button::new("Link")).clicked() {
                                    todo = Some(Do::Link(*build));
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
                                        todo = Some(Do::Go(to.clone()));
                                    }
                                });
                            });
                            ui.separator();
                        }
                    });
                }
                Tab::Servers => {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut window.server).hint_text("host:port").desired_width(200.0));
                        if ui.add_enabled(!window.server.trim().is_empty(), egui::Button::new("Join")).clicked() {
                            todo = Some(Do::Join(window.server.trim().to_string()));
                        }
                    });
                    ui.add_space(4.0);
                    for addr in recent_servers() {
                        ui.horizontal(|ui| {
                            ui.label(&addr);
                            if ui.small_button("Join").clicked() {
                                todo = Some(Do::Join(addr.clone()));
                            }
                        });
                    }
                    ui.separator();
                    if let Some((addr, players)) = &ledger.hosting {
                        let n = players.load(std::sync::atomic::Ordering::Relaxed);
                        ui.label(format!("Hosting this world on port {} · {n} playing", addr.port()));
                    } else if online {
                        if ui.button(format!("Leave {here}")).clicked() {
                            todo = Some(Do::Leave);
                        }
                    } else if ui.button("Host this world").on_hover_text("Others on your network can join it until you quit").clicked() {
                        todo = Some(Do::Host);
                    }
                }
            }
        });
    window.open = open;

    let Some(todo) = todo else { return Ok(()) };
    let reply = match todo {
        Do::Enter(name) => {
            travel.next = Some(Trip::Local(name));
            None
        }
        Do::Create => {
            let flat = window.new_flat;
            let seed = if flat { SEED } else { SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(SEED, |d| d.subsec_nanos()) };
            match ledger::create_world(&window.new_name, flat, seed) {
                Ok(name) => {
                    window.new_name.clear();
                    travel.next = Some(Trip::Local(Some(name)));
                    None
                }
                Err(why) => Some(why),
            }
        }
        Do::Trash(name) => {
            window.trashing = None;
            Some(match ledger::trash_world(&name) {
                Ok(()) => format!("Moved {name} to saves/trash."),
                Err(why) => why,
            })
        }
        Do::Save => {
            let name = std::mem::take(&mut window.save_name);
            Some(commands::save(&mut ledger, &name))
        }
        Do::Fork(save, name) => {
            window.fork = None;
            Some(commands::fork(&ledger, &save, &name))
        }
        Do::Load(id) => {
            let cx = &mut Context {
                commands: &mut commands,
                ledger: &mut ledger,
                world: &mut world,
                creations: &mut creations,
                landscape: &mut landscape,
                travel: &mut travel,
            };
            Some(commands::restore(cx, id))
        }
        Do::Go(to) => travel.portal(&to).err(),
        Do::Link(build) => Some(match PortalTo::parse(&window.link) {
            Ok(to) => {
                let label = to.label().to_string();
                ledger.record(None, None, Action::SetPortal { build, to: Some(to) });
                format!("It's a portal to {label} now: walk up to it and press E.")
            }
            Err(why) => why,
        }),
        Do::Unlink(build) => {
            ledger.record(None, None, Action::SetPortal { build, to: None });
            Some("It's an ordinary build again.".into())
        }
        Do::Join(addr) => {
            remember_server(&addr);
            travel.next = Some(Trip::Server(addr));
            None
        }
        Do::Host => Some(super::online::host(&mut ledger)),
        Do::Leave => {
            travel.next = Some(Trip::Local(world_name()));
            None
        }
    };
    window.list = None;
    if let Some(reply) = reply {
        chat.say(Role::System, reply);
    }
    Ok(())
}
