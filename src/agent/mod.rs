//! Builder chat (desktop only): ask a local Claude Code or Codex CLI to design a voxel
//! object, preview it in front of you, then place, move, or discard it. Click a placed
//! build to select it, then edit (move, turn, or revise it with the agent) or delete it.
//!
//! Flow: chat message -> [`runner`] runs the CLI headless with the build schema ->
//! [`spec::compile`] validates and rasterizes the answer (asking the agent to fix it once
//! if it breaks a limit) -> a draft appears in the world -> Place saves it.
//!
//! Land works the same way ([`landscape`]): select an area with G, describe it, and
//! the agent's terrain operations are previewed in the world until kept. Asked for
//! gameplay there ("a turnip farm"), the agent answers with a data package that's
//! installed on the area's parcels and played with [`game`].

mod commands;
mod creations;
mod fps;
mod game;
#[cfg(test)]
mod gate;
mod history;
mod hud;
mod inventory;
mod lan;
mod landscape;
mod loading;
mod mcp;
mod ops;
mod panels;
mod online;
mod prefs;
mod presence;
mod prompt;
mod registry;
mod runner;
mod stream;
mod activity;
mod settings;
mod theme;
use genverse_rules::build as spec;
mod tools;
mod workspace;
mod present;
#[cfg(not(target_arch = "wasm32"))]
mod player_scripts;
mod worlds;

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use bevy::{camera::SubCameraView, prelude::*, window::PrimaryWindow};
use bevy_egui::{
    EguiContexts, EguiGlobalSettings, EguiPlugin, EguiPrimaryContextPass, PrimaryEguiContext, egui,
    input::EguiWantsInput,
};
use egui_phosphor::regular as icon;

use crate::bindings::Bindings;
use crate::{
    player::{self, InputBlocked, Player, PointerOverUi},
    graphics::{self, ScreenCamera},
    voxel::{
        VoxelModel,
        query::{Hit, overlaps_box, raycast},
    },
    world::{VoxelWorld, ledger::Ledger},
};
use genverse_rules::{
    land::area_parcels,
    log::Action as LogAction,
    logic::{Scope, data::GameSpec},
    settings::WorldRules,
};
use creations::{
    Adjust, Creation, Creations, DraftMarker, load_creations, outline_creations, placement_for_hit, stream_creations,
};
use landscape::{
    AreaGizmos, Landscape, ParcelOverlay, PreviewError, draw_parcels, configure_area_gizmos, describe_stats, land_summary, draw_area, highlight_area, select_area,
};
use mcp::McpServer;
use runner::{AgentEvent, AgentJob, Provider};
use spec::compile;
use tools::{BuildSite, answer_tool_calls, update_build_site};

const HISTORY: usize = 8;
/// Messages from one sender this close together share a name line, as in Discord.
const GROUP: Duration = Duration::from_secs(5 * 60);

pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin::default())
            // Clicks, drags, and keys over the panel must not reach the game (it's off by default).
            // The interface draws on the screen camera, which stays on the window when the
            // world is drawn at a lower render scale.
            .insert_resource(EguiGlobalSettings {
                enable_absorb_bevy_input_system: true,
                auto_create_primary_context: false,
                ..default()
            })
            .init_resource::<Chat>()
            .init_resource::<Creations>()
            .init_resource::<BuildSite>()
            .init_resource::<Landscape>()
            .init_resource::<ParcelOverlay>()
            .init_resource::<presence::Peers>()
            .init_resource::<history::History>()
            .init_resource::<inventory::Inventory>()
            .init_resource::<fps::Fps>()
            .init_resource::<loading::Loading>()
            .init_resource::<settings::SettingsWindow>()
            .init_resource::<lan::Lan>()
            .init_resource::<game::Play>()
            .init_resource::<present::LocalShows>()
            .init_resource::<tools::Looks>()
            .init_resource::<worlds::Travel>()
            .init_resource::<worlds::WorldsWindow>()
            .init_gizmo_group::<AreaGizmos>()
            .init_gizmo_group::<presence::RingGizmos>()
            .add_systems(Startup, (attach_interface.after(graphics::spawn_screen), settings::load_settings))
            .init_resource::<panels::Panels>()
            .add_systems(Startup, (ops::load_bindings, panels::load_panels))
            .add_systems(EguiPrimaryContextPass, panels::panel_windows.after(chat_panel))
            .add_systems(Startup, (load_creations, configure_area_gizmos, presence::configure_ring_gizmos, start_tool_server, lan::start_listening))
            .add_systems(
                Update,
                (
                    block_game_input,
                    center_view,
                    stream_creations,
                    (online::sync_online, lan::update_lan, update_build_site, answer_tool_calls, poll_agent, finish_turn, resume_turn, creations::show_builds).chain(),
                    (presence::send_presence, presence::move_avatars, presence::draw_peers, history::toggle_history, (loading::track_loading, fps::track_frames).chain()),
                    auto_prompt,
                    (game::script_play.after(auto_prompt), game::run_timers, game::play_keys, game::sync_pieces, game::walk_pieces, game::follow_teleports, game::follow_travels, game::follow_body, game::sync_game_actions, game::use_pieces, worlds::use_portals, hud::draw_game_world, history::follow_ledger)
                        .chain(),
                    worlds::toggle_worlds,
                    (inventory::sync_inventory, inventory::inventory_keys, inventory::use_held, shortcuts, select_area, follow_cursor, check_clearance, click_in_world, outline_creations, draw_area, highlight_area, draw_parcels)
                        .chain(),
                ),
            )
            .add_systems(EguiPrimaryContextPass, (install_icon_font, chat_panel, shortcut_hints, game::game_labels, inventory::hotbar, loading::loading_screen).chain())
            .add_systems(
                EguiPrimaryContextPass,
                (presence::name_tags, presence::hover_credit, history::history_window, fps::fps_overlay, settings::settings_window, worlds::portal_prompt),
            )
            .add_systems(EguiPrimaryContextPass, (worlds::worlds_window, hud::game_hud))
            // How games show things: effects, sounds, the sky, heroes' looks.
            .add_systems(Update, (present::play_shows, present::update_particles, present::apply_sky, present::dress_hero))
            .add_systems(Update, tools::take_looks.after(crate::player::follow_camera))
            // Before games act, so they read the ground as it is.
            .add_systems(Update, crate::world::ledger::copy_edits.before(game::script_play).before(game::run_timers))
            // Trips are taken last, once everything else this frame is done with the world.
            .add_systems(PostUpdate, worlds::travel);
        // The games' player scripts: before the hero moves, after the pieces do.
        #[cfg(not(target_arch = "wasm32"))]
        app.insert_non_send(player_scripts::PlayerScripts::default())
            .init_resource::<player_scripts::Poses>()
            .add_systems(
                Update,
                (
                    player_scripts::run_player_scripts.after(game::sync_pieces).before(crate::player::move_player),
                    player_scripts::pose_pieces.after(game::walk_pieces).after(player_scripts::run_player_scripts),
                ),
            );
    }
}

#[derive(PartialEq)]
enum Role {
    Player,
    /// Carries which agent answered.
    Agent(&'static str),
    System,
}

struct Message {
    role: Role,
    text: String,
    at: Instant,
}

struct Pending {
    job: Mutex<AgentJob>,
    provider: Provider,
    /// The player message that started this request.
    prompt: String,
    repairs: u8,
    /// This request turned the selected build into a pinned draft; if no new build
    /// comes back, put the original back.
    restore: bool,
}

#[derive(Resource)]
struct Chat {
    open: bool,
    provider: Provider,
    input: String,
    messages: Vec<Message>,
    /// What the agent has done and said so far on the request it's working on.
    activity: activity::Feed,
    pending: Option<Pending>,
    /// Set by the T/Enter shortcuts; the panel focuses the input on its next frame.
    focus_input: bool,
    /// The name being typed in the profile editor, while it's open.
    name: Option<String>,
    /// The profile editor just opened; its name field takes the keyboard.
    focus_name: bool,
    /// The highlighted suggestion while typing a slash command.
    suggestion: usize,
    /// The marker color picked (an index into [`presence::MARKERS`]); none for the one
    /// the player's id gives them.
    marker: Option<u8>,
    /// The server address being typed, to join.
    server_addr: String,
    /// Play together was asked for (by `/play`); the LAN system picks it up.
    play_together: bool,
    /// The Play together section above the name is unfolded.
    network: bool,
    /// The share of the window's width the panel covers (0 when hidden).
    covered: f32,
    /// Where the panel is on screen, in egui points, while any of it shows.
    panel: Option<egui::Rect>,
    /// The shortcut hints at the bottom of the view are showing (F1 toggles them).
    hints: bool,
    /// Recent errors from the world's game scripts, for the agent to fix.
    script_errors: Vec<String>,
    /// Something the agent wants to do that waits for the player's yes.
    asking: Option<tools::Ask>,
    /// What the agent set going that waits for its turn to end.
    after: tools::AfterTurn,
    /// A request that goes on in the world the player is on their way to.
    resume: Option<tools::Resume>,
    /// How many worlds this request has gone on into.
    hops: u8,
}

/// How many worlds one request may go on into, so two worlds can't send an agent back
/// and forth forever.
const MAX_HOPS: u8 = 3;

impl Default for Chat {
    fn default() -> Self {
        Self {
            open: true,
            provider: Provider::Claude,
            input: String::new(),
            messages: vec![Message {
                role: Role::System,
                text: "Describe something to build, e.g. \"a cozy wooden bench\" or \"a glowing mushroom lamp\". \
                       It appears in front of you as a draft. Click a placed build to select it; while it's \
                       selected, messages change that build. To reshape the land, press G, drag across the \
                       ground, and describe it: \"a pond with reeds\", \"a winding path\", \"terraced hillside\". \
                       Or ask for anything else in your game: \"a new world called dungeon and a portal to it\", \
                       \"undo that\", \"save this as before the party\"."
                    .into(),
                at: Instant::now(),
            }],
            activity: activity::Feed::default(),
            pending: None,
            focus_input: false,
            name: None,
            focus_name: false,
            suggestion: 0,
            marker: prefs::get("marker").and_then(|v| v.as_u64()).map(|m| m as u8 % presence::MARKERS.len() as u8),
            server_addr: String::new(),
            play_together: false,
            network: false,
            covered: 0.0,
            panel: None,
            hints: prefs::get("hints").and_then(|v| v.as_bool()).unwrap_or(true),
            script_errors: Vec::new(),
            asking: None,
            after: tools::AfterTurn::default(),
            resume: None,
            hops: 0,
        }
    }
}

impl Chat {
    /// The marker color this player shows others.
    fn marker(&self, me: &str) -> u8 {
        self.marker.unwrap_or_else(|| presence::default_marker(me))
    }

    pub(super) fn say(&mut self, role: Role, text: impl Into<String>) {
        self.messages.push(Message { role, text: text.into(), at: Instant::now() });
    }

    fn start(
        &mut self,
        tools: Option<&McpServer>,
        rules: &WorldRules,
        provider: Provider,
        prompt: String,
        request: String,
        repairs: u8,
        restore: bool,
    ) {
        let job = runner::start(provider, prompt::system(tools.is_some(), rules), request, tools, workspace::current());
        self.activity = activity::Feed::default();
        self.pending = Some(Pending { job: Mutex::new(job), provider, prompt, repairs, restore });
    }

    /// Sends the input. With no draft but a selected build, the message revises that
    /// build: it becomes a pinned draft the agent edits in place. Otherwise, with an
    /// area selected (`area` describes it), the message reshapes that land.
    fn send(
        &mut self,
        creations: &mut Creations,
        commands: &mut Commands,
        tools: Option<&McpServer>,
        ledger: &Ledger,
        area: Option<String>,
        here: Option<String>,
    ) {
        let rules = &ledger.rules;
        let text = self.input.trim().to_string();
        if text.is_empty() || self.pending.is_some() {
            return;
        }
        self.input.clear();
        // A new request: what the last one left waiting is dropped.
        self.after = tools::AfterTurn::default();
        self.resume = None;
        self.hops = 0;
        let restore = area.is_none() && creations.draft.is_none() && creations.edit_selected(commands, false).is_some();
        let turns: Vec<_> = self
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| prompt::Turn { from_player: m.role == Role::Player, text: &m.text })
            .collect();
        let history = &turns[turns.len().saturating_sub(HISTORY)..];
        let draft = creations.draft.as_ref().map(|d| &d.spec);
        let game = folder_context(ledger, here.as_deref(), &self.script_errors);
        let request = prompt::request(history, draft, area.as_deref(), game.as_deref(), &text);
        // `GENVERSE_SHOW_REQUEST=1` logs what the agent is told, to see the world through its eyes.
        if std::env::var("GENVERSE_SHOW_REQUEST").is_ok() {
            info!("request to the agent:\n{request}");
        }
        self.say(Role::Player, text.clone());
        self.start(tools, rules, self.provider, text, request, 0, restore);
    }
}

/// Writes the world as files for the agent to look through and edit the game in, and
/// returns what the request says about it (or, if the folder can't be written, the
/// world's game logic in full).
fn folder_context(ledger: &Ledger, here: Option<&str>, errors: &[String]) -> Option<String> {
    match workspace::write(ledger, here, errors) {
        Ok(dir) => {
            info!("the world's folder for the agent: {}", dir.display());
            Some(format!(
                "{}Your folder is this world as files: WORLD.md has the overview, packages/ the game logic.",
                here.map(|h| format!("{h}\n")).unwrap_or_default()
            ))
        }
        Err(why) => {
            warn!("couldn't write the world's folder: {why}");
            game_context(ledger, errors).map(|g| match here {
                Some(here) => format!("{here}\n{g}"),
                None => g,
            })
        }
    }
}

impl Chat {
    /// Starts the agent again in the world its trip led to, to do what it left itself
    /// there: the same request, with the conversation so far and this world's folder.
    fn carry_on(&mut self, tools: Option<&McpServer>, ledger: &Ledger, here: String, resume: tools::Resume, world: &str) {
        let turns: Vec<_> = self
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| prompt::Turn { from_player: m.role == Role::Player, text: &m.text })
            .collect();
        let history = &turns[turns.len().saturating_sub(HISTORY)..];
        let game = folder_context(ledger, Some(&here), &self.script_errors);
        let message = format!(
            "(From the game, not the player.) You're carrying on with the request \"{}\". You did its part in {} and brought the \
             player here, to {world}, to finish it. Your note to yourself: {}\nDo that now, in this world: your folder and tools are \
             this world's.",
            resume.prompt, resume.from, resume.note
        );
        let request = prompt::request(history, None, None, game.as_deref(), &message);
        self.say(Role::System, format!("The agent carries on here: {}", resume.note));
        self.after = tools::AfterTurn::default();
        self.hops += 1;
        self.start(tools, &ledger.rules, self.provider, resume.prompt, request, 0, false);
    }
}

/// Once the player has arrived where the agent's trip led, starts the agent again there
/// with what it left itself to do. Gives up if they never get there, and steps aside if
/// the player has asked for something else.
fn resume_turn(
    tools: Option<Res<McpServer>>,
    mut chat: ResMut<Chat>,
    ledger: Res<Ledger>,
    world: Res<VoxelWorld>,
    loading: Res<loading::Loading>,
    travel: Res<worlds::Travel>,
    players: Query<(&Player, &Transform)>,
    cameras: Query<&GlobalTransform, With<Camera3d>>,
) {
    let Some(resume) = &chat.resume else { return };
    if chat.pending.is_some() {
        chat.resume = None;
        return;
    }
    let here = worlds::current_title(&ledger);
    let arrived = travel.next.is_none() && here != resume.from && loading.is_done();
    let Some((_, hero)) = players.single().ok().filter(|(player, _)| arrived && player.has_landed()) else {
        if resume.since.elapsed() > std::time::Duration::from_secs(90) {
            chat.resume = None;
            chat.say(Role::System, "The trip didn't happen, so what the agent meant to do there is left undone.");
        }
        return;
    };
    let Some(resume) = chat.resume.take() else { return };
    let context = here_context(&world, ledger.log.terrain(), hero.translation, cameras.single().ok());
    chat.carry_on(tools.as_deref(), &ledger, context, resume, &here);
}

/// Where the player stands, in the world's own meters, and the ground around them: so
/// "next to me" and "20 m east" become positions a game can use (its scripts hold them),
/// without anything to put down first.
fn here_context(world: &VoxelWorld, terrain: genverse_rules::terrain::Terrain, hero: Vec3, camera: Option<&GlobalTransform>) -> String {
    use genverse_rules::logic::Ground;
    const STEP_M: f32 = 4.0;
    const REACH: i32 = 6;
    let facing = camera.map(|c| c.forward().xz()).filter(|f| f.length() > 0.1).map_or("north", |f| {
        if f.x.abs() > f.y.abs() { if f.x > 0.0 { "east (+x)" } else { "west (-x)" } } else if f.y > 0.0 { "south (+z)" } else { "north (-z)" }
    });
    let column = |m: f32| (m / crate::world::terrain::VOXEL_SIZE).floor() as i32;
    let step = (STEP_M / crate::world::terrain::VOXEL_SIZE) as i32;
    let min = [column(hero.x) - REACH * step, column(hero.z) - REACH * step];
    let n = 2 * REACH + 1;
    // Where a piece stands in a column: the loaded ground (edits and all), or where it
    // isn't loaded yet, the world's own terrain (exact where nobody has changed it).
    let stand = |x: i32, z: i32| -> Option<f32> {
        let y = match world.heights([x, z], 1, [1, 1]) {
            Some(h) => h[0]?,
            None => {
                let h = terrain.height(x, z);
                if !terrain.flat && h < terrain.sea_level() {
                    return None;
                }
                h
            }
        };
        Some((y + 1) as f32 * crate::world::terrain::VOXEL_SIZE)
    };
    let feet = stand(column(hero.x), column(hero.z)).unwrap_or(hero.y);
    let mut out = format!(
        "The player stands at x={:.1}, y={feet:.2}, z={:.1} (meters; y is the ground they stand on), facing {facing}. \
         Where pieces stand around them (y in meters, every {STEP_M} m; rows north to south, columns west to east; ~ water):\n",
        hero.x, hero.z
    );
    out.push_str(&format!("  z \\ x: {}\n", (0..n).map(|i| format!("{:.0}", hero.x + (i - REACH) as f32 * STEP_M)).collect::<Vec<_>>().join(" ")));
    for j in 0..n {
        let row: Vec<String> =
            (0..n).map(|i| stand(min[0] + i * step, min[1] + j * step).map_or("~".into(), |y| format!("{y:.2}"))).collect();
        out.push_str(&format!("  {:.0}: {}\n", hero.z + (j - REACH) as f32 * STEP_M, row.join(" ")));
    }
    out
}

/// What the agent is told about the selected area, if this message is about land.
fn area_context(
    creations: &Creations,
    landscape: &Landscape,
    world: &VoxelWorld,
    ledger: &Ledger,
    player: Option<Vec3>,
    camera: Option<&GlobalTransform>,
) -> Option<String> {
    if creations.draft.is_some() || creations.selected.is_some() {
        return None;
    }
    landscape.context(world, ledger, player, camera.map(|c| *c.forward()))
}

/// The world's game logic as it is, so each request adds to it: every package's
/// definitions and script files in full, its models by name (the agent sends one again
/// only to change it); and the errors its scripts hit lately, so "fix it" is enough.
fn game_context(ledger: &Ledger, errors: &[String]) -> Option<String> {
    let logic = &ledger.logic;
    let specs: Vec<String> = logic
        .installed
        .values()
        .filter_map(|i| logic.package(i))
        .map(|p| {
            let spec = GameSpec::from_package(p);
            let models: Vec<String> = spec.models.iter().map(|m| format!("{} ({})", m.name, m.build.size.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("×"))).collect();
            let mut text = format!(
                "Package \"{}\" (\"{}\", v{}):\n- defs: {}\n- models: {}\n",
                spec.name,
                spec.title,
                p.manifest.version,
                serde_json::to_string(&spec.defs).unwrap_or_default(),
                if models.is_empty() { "none".into() } else { models.join(", ") }
            );
            for script in &spec.scripts {
                text.push_str(&format!("- script {}:\n{}\n", script.name, script.source));
            }
            text
        })
        .collect();
    let names: Vec<String> = match &ledger.online {
        Some(online) => online.game.as_ref().map(|g| g.worlds().into_iter().map(|(n, _)| n).collect()).unwrap_or_default(),
        None => crate::world::ledger::list_worlds().into_iter().map(|w| w.name).collect(),
    };
    let here = ledger.online.as_ref().and_then(|o| o.world.clone()).unwrap_or_else(crate::world::ledger::world_name);
    let names: Vec<String> = names.into_iter().filter(|n| *n != here).collect();
    let mut out = format!(
        "The other worlds of this game (for portals, by name; another game of the player's is \"game/world\"): {}.",
        if names.is_empty() { "none yet".into() } else { names.join(", ") }
    );
    if specs.is_empty() {
        out.push_str("\nThe world has no game logic yet: a \"game\" you send starts it (name it \"world\").");
        return Some(out);
    }
    out.push_str(&format!(
        "\nThe world's game logic, as it runs now. Your \"game\" changes it: send only what's new or different \
         (same package name; files, defs and models by name), and list what to take out in \"remove\".\n{}",
        specs.join("\n")
    ));
    if !errors.is_empty() {
        out.push_str(&format!("\nRecent errors from these scripts while players played:\n- {}", errors.join("\n- ")));
    }
    Some(out)
}

/// Why a message about the selected land can't go to the agent: the land rules don't
/// let the player change it, so no answer would help.
fn land_refusal(creations: &Creations, landscape: &Landscape, ledger: &Ledger) -> Option<String> {
    if creations.draft.is_some() || creations.selected.is_some() {
        return None;
    }
    let area = landscape.target()?;
    let why = ledger.check(&area_parcels(area)).and_then(|_| ledger.held(vec![area])).err()?;
    Some(format!("⚠ Can't change this land: {why}. Select an area you can change."))
}

/// `GENVERSE_PROMPT="a red mailbox" cargo run` sends one message once the hero has
/// landed. Pair with `GENVERSE_SHOT` to script a build end to end. `GENVERSE_AREA=12`
/// selects a 12 m square of ground in front of the hero (first, or on its own).
fn auto_prompt(
    mut commands: Commands,
    tools: Option<Res<McpServer>>,
    mut sent: Local<bool>,
    mut queue: Local<Vec<String>>,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut overlay: ResMut<ParcelOverlay>,
    mut world: ResMut<VoxelWorld>,
    mut ledger: ResMut<Ledger>,
    mut travel: ResMut<worlds::Travel>,
    (mut binds, mut graphics, mut panels): (ResMut<Bindings>, ResMut<crate::graphics::Graphics>, ResMut<panels::Panels>),
    players: Query<(&Player, &Transform)>,
    cameras: Query<&GlobalTransform, With<Camera3d>>,
) {
    let Ok((player, hero)) = players.single() else {
        return;
    };
    // `GENVERSE_PROMPT="first || second || third"`: each once the agent has answered the
    // one before, like a player who keeps asking.
    if *sent {
        if chat.pending.is_none() && !queue.is_empty() {
            chat.input = queue.remove(0);
            let area = area_context(&creations, &landscape, &world, &ledger, Some(hero.translation), cameras.single().ok());
            let here = here_context(&world, ledger.log.terrain(), hero.translation, cameras.single().ok());
            chat.send(&mut creations, &mut commands, tools.as_deref(), &ledger, area, Some(here));
        }
        return;
    }
    if !player.has_landed() {
        return;
    }
    *sent = true;
    if std::env::var("GENVERSE_PARCELS").is_ok() {
        overlay.visible = true;
    }
    if let Some(size) = std::env::var("GENVERSE_AREA").ok().and_then(|s| s.parse::<f32>().ok()) {
        let forward = (hero.rotation * Vec3::NEG_Z).xz().normalize_or_zero();
        let center = hero.translation.xz() + forward * (size / 2.0 + 2.0);
        let half = Vec2::splat(size / 2.0);
        let cell = |p: Vec2| (p / crate::world::terrain::VOXEL_SIZE).floor().as_ivec2();
        landscape.area = Some(genverse_rules::sculpt::Area::from_corners(cell(center - half), cell(center + half) - 1));
    }
    if let Ok(text) = std::env::var("GENVERSE_PROMPT") {
        if let Ok(provider) = std::env::var("GENVERSE_AGENT") {
            chat.provider = if provider == "codex" { Provider::Codex } else { Provider::Claude };
        }
        // `GENVERSE_SELECT_LAST=1` targets the most recent build, like clicking it first.
        if std::env::var("GENVERSE_SELECT_LAST").is_ok() {
            creations.selected = creations.placed.last().map(|(e, _)| *e);
        }
        // Commands, `;`-separated: `GENVERSE_PROMPT="/rate 2; /history"`.
        if text.trim_start().starts_with('/') {
            for command in text.split(';') {
                let cx = &mut ops::Context {
                    ledger: &mut ledger,
                    world: &mut world,
                    creations: &mut creations,
                    landscape: &mut landscape,
                    travel: &mut travel,
                    at: hero.translation.to_array(),
                    agent: None,
                    binds: &mut binds,
                    graphics: &mut graphics,
                    panels: &mut panels,
                };
                run_command(&mut chat, command.trim(), cx);
            }
            return;
        }
        let mut prompts: Vec<String> = text.split("||").map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
        if prompts.is_empty() {
            return;
        }
        chat.input = prompts.remove(0);
        *queue = prompts;
        if let Some(why) = land_refusal(&creations, &landscape, &ledger) {
            info!("prompt refused: {why}");
            chat.input.clear();
            chat.say(Role::System, why);
            return;
        }
        let area = area_context(&creations, &landscape, &world, &ledger, Some(hero.translation), cameras.single().ok());
        let here = here_context(&world, ledger.log.terrain(), hero.translation, cameras.single().ok());
        chat.send(&mut creations, &mut commands, tools.as_deref(), &ledger, area, Some(here));
    }
}

/// Echoes a chat command and its reply.
fn run_command(chat: &mut Chat, text: &str, cx: &mut ops::Context) {
    chat.say(Role::Player, text);
    // Play together is the panel's, not a world command.
    if text.trim() == "/play" {
        chat.play_together = true;
        chat.say(Role::System, "Looking for worlds on your network…");
        return;
    }
    let reply = commands::run(text, cx);
    info!("{text} -> {reply}");
    if !reply.is_empty() {
        chat.say(Role::System, reply);
    }
}

/// Does what the agent left for the end of its turn: what would have swapped the world
/// under it while it worked (hosting, joining), then the trip it asked for. Waits for a
/// draft to be placed or discarded, as going elsewhere does for the player.
fn finish_turn(
    mut chat: ResMut<Chat>,
    mut ledger: ResMut<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut travel: ResMut<worlds::Travel>,
    (mut binds, mut graphics, mut panels): (ResMut<Bindings>, ResMut<crate::graphics::Graphics>, ResMut<panels::Panels>),
    players: Query<&Transform, With<Player>>,
) {
    if chat.pending.is_some() || chat.after.is_empty() {
        return;
    }
    if creations.draft.is_some() || landscape.draft.is_some() {
        if !chat.after.told {
            chat.after.told = true;
            chat.say(Role::System, "Place or discard the draft, and the rest of what the agent set up (going elsewhere) follows.");
        }
        return;
    }
    let after = std::mem::take(&mut chat.after);
    for (name, args) in after.ops {
        let cx = &mut ops::Context {
            ledger: &mut ledger,
            world: &mut world,
            creations: &mut creations,
            landscape: &mut landscape,
            travel: &mut travel,
            at: players.single().map_or([0.0; 3], |t| t.translation.to_array()),
            agent: None,
            binds: &mut binds,
            graphics: &mut graphics,
            panels: &mut panels,
        };
        let reply = ops::said(ops::run(name, args, cx));
        info!("after the agent's turn, {name} -> {reply}");
        if !reply.is_empty() {
            chat.say(Role::System, reply);
        }
    }
    if let Some(trip) = after.trip {
        travel.next = Some(trip);
        travel.stop_hosting |= after.stop_hosting;
        // The request goes on where the trip leads, a few worlds at most.
        if let Some((note, prompt)) = after.then.filter(|_| chat.hops < MAX_HOPS) {
            chat.resume = Some(tools::Resume { from: worlds::current_title(&ledger), note, prompt, since: std::time::Instant::now() });
        }
    }
}

/// Serves the world tools to agents. Without it, agents still work blind.
fn start_tool_server(mut commands: Commands, ledger: Res<Ledger>) {
    match McpServer::start(tools::definitions(&ledger.rules), tools::INSTRUCTIONS) {
        Ok(server) => {
            info!("agent world tools at {}", server.url);
            commands.insert_resource(server);
        }
        Err(e) => warn!("couldn't start the agent tool server, agents won't see the world: {e}"),
    }
}

/// Keeps WASD and friends from moving the hero while typing in the chat.
fn block_game_input(
    egui: Res<EguiWantsInput>,
    chat: Res<Chat>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut blocked: ResMut<InputBlocked>,
    mut over_ui: ResMut<PointerOverUi>,
) {
    blocked.0 = egui.wants_keyboard_input();
    // egui only knows about its windows and popups; the panel is drawn on the background,
    // so the pointer counts as over it by its rectangle.
    let cursor = windows.single().ok().and_then(|w| w.cursor_position());
    let over_panel = chat.panel.zip(cursor).is_some_and(|(panel, at)| panel.contains(egui::pos2(at.x, at.y)));
    over_ui.0 = egui.is_pointer_over_area() || over_panel;
}

fn poll_agent(
    mut commands: Commands,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut world: ResMut<VoxelWorld>,
    mut ledger: ResMut<Ledger>,
    site: Res<BuildSite>,
    tools: Option<Res<McpServer>>,
    mut panels: ResMut<panels::Panels>,
    drafts: Query<&Transform, With<DraftMarker>>,
) {
    let Some(pending) = &chat.pending else {
        return;
    };
    let mut finished = None;
    let mut activities = Vec::new();
    if let Ok(job) = pending.job.lock() {
        while let Ok(event) = job.events.try_recv() {
            match event {
                AgentEvent::Activity(activity) => activities.push(activity),
                AgentEvent::Finished(result) => finished = Some(*result),
            }
        }
    }
    for activity in activities {
        chat.activity.apply(activity);
    }
    let Some(result) = finished else {
        return;
    };
    let pending = chat.pending.take().expect("checked above");
    // The panels it wrote in its folder are the player's now.
    if let Some(folder) = workspace::current() {
        let (done, wrong) = panels::take_from(&folder);
        if !done.is_empty() || !wrong.is_empty() {
            panels.reload();
        }
        for note in done {
            chat.say(Role::System, note);
        }
        for why in wrong {
            chat.say(Role::System, format!("⚠ {why}"));
        }
    }
    // The feed folds into one line once the work is over.
    if let Some(worked) = chat.activity.summary() {
        chat.say(Role::System, worked);
    }

    // An unchanged revision draft goes back to being the original build.
    let mut keep_original = |creations: &mut Creations, chat: &mut Chat| {
        if pending.restore && creations.draft.as_ref().is_some_and(|d| d.replaces.is_some()) {
            creations.discard_draft(&mut commands);
            chat.say(Role::System, "Left it unchanged.");
        }
    };
    let answer = match result {
        Ok(answer) => answer,
        Err(e) => {
            warn!("{} failed: {e}", pending.provider.label());
            chat.say(Role::System, format!("⚠ {} failed: {e}", pending.provider.label()));
            // It didn't finish: what waited for the end of its turn doesn't happen.
            chat.after = tools::AfterTurn::default();
            keep_original(&mut creations, &mut chat);
            return;
        }
    };
    info!("{} replied: {}", pending.provider.label(), answer.reply);
    if !answer.reply.trim().is_empty() {
        chat.say(Role::Agent(pending.provider.label()), answer.reply.trim().to_string());
    }
    // Land first: a rejected change asks the agent for a fix (and drops any build with it).
    let mut repairing = false;
    if let Some(terrain) = answer.terrain {
        let name = terrain.name.clone();
        let provider = pending.provider.label().to_string();
        match landscape.target() {
            None => chat.say(Role::System, "The agent sent land changes, but no area is selected (G)."),
            Some(area) => match landscape.preview(&mut world, &ledger, terrain.clone(), pending.prompt.clone(), provider) {
                Ok(stats) => {
                    info!("terrain preview {name:?}: {stats:?} from {}", serde_json::to_string(&terrain).unwrap_or_default());
                    chat.say(
                        Role::System,
                        format!(
                            "Preview \"{name}\": {}. Enter keeps it, Esc puts the ground back, or ask for changes.",
                            describe_stats(area, &stats)
                        ),
                    );
                    // `GENVERSE_KEEP=1` keeps a scripted preview, as if Enter was pressed.
                    if std::env::var("GENVERSE_KEEP").is_ok() {
                        keep_terrain(&mut landscape, &mut world, &mut ledger, &mut chat);
                    }
                }
                Err(PreviewError::Refused(why)) => {
                    chat.say(Role::System, format!("⚠ Can't change this land: {why}."));
                }
                Err(PreviewError::Invalid(errors)) if pending.repairs == 0 => {
                    warn!("terrain rejected, asking for a fix: {errors:?}");
                    chat.say(Role::System, format!("Land change broke {} rule(s); asking for a fix…", errors.len()));
                    let request = prompt::repair_terrain(&terrain, &errors);
                    let rules = &ledger.rules;
                    chat.start(tools.as_deref(), rules, pending.provider, pending.prompt.clone(), request, 1, false);
                    repairing = true;
                }
                Err(PreviewError::Invalid(errors)) => {
                    warn!("terrain rejected again: {errors:?}");
                    chat.say(Role::System, format!("⚠ Land change rejected:\n• {}", errors.join("\n• ")));
                }
            },
        }
    }
    // What the agent edited in the world's folder, with what it said in "game".
    let edited = match workspace::current().map(|dir| workspace::changed_game(&dir, &ledger)) {
        Some(Ok(change)) => change,
        Some(Err(why)) => {
            chat.say(Role::System, format!("⚠ {why}"));
            None
        }
        None => None,
    };
    let describe = answer.describe.clone().unwrap_or_default();
    if let Some(change) = workspace::merge(edited, answer.game.map(|g| *g)) {
        let provider = pending.provider.label();
        // A change to what the world runs: it adds to it, by name.
        let base = ledger.logic.installed.get(&change.name).and_then(|i| ledger.logic.package(i)).map(GameSpec::from_package);
        let summary = change.change_summary(base.as_ref());
        let game = change.onto(base.as_ref());
        let version = ledger.logic.installed.get(&game.name).map_or(1, |i| i.package.version + 1);
        let package = game.to_package(version);
        match package.check() {
            Err(errors) if pending.repairs == 0 && !repairing => {
                warn!("game rules rejected, asking for a fix: {errors:?}");
                chat.say(Role::System, format!("Game rules broke {} rule(s); asking for a fix…", errors.len()));
                let request = prompt::repair_game(&game, &errors);
                chat.start(tools.as_deref(), &ledger.rules, pending.provider, pending.prompt.clone(), request, 1, false);
                repairing = true;
            }
            Err(errors) => chat.say(Role::System, format!("⚠ Game rules rejected:\n• {}", errors.join("\n• "))),
            // Live: the next version, at once, for everyone. The world is saved just before,
            // so loading that goes back to how it was, data and all.
            Ok(()) => {
                let before = (version > 1).then(|| format!("before {} v{version}", game.name).chars().take(40).collect::<String>());
                if let Some(name) = &before {
                    ledger.record(None, None, genverse_rules::log::Action::Save { name: name.clone() });
                    ledger.keep_saves(&world.edit_list());
                }
                match (ledger.install(package, Scope::World, Some(provider), Some(&pending.prompt)), before) {
                    (Ok(_), Some(before)) => {
                        chat.script_errors.clear();
                        chat.say(Role::System, format!("\"{}\" v{version} is live: {summary}. (Saved \"{before}\" first: /load it to go back.)", game.title));
                    }
                    (Ok(_), None) => {
                        chat.script_errors.clear();
                        chat.say(Role::System, format!("\"{}\" is live in this world: {summary}.", game.title));
                    }
                    (Err(why), _) => chat.say(Role::System, format!("⚠ Couldn't install \"{}\": {why}.", game.title)),
                }
            }
        }
    }
    // What things mean, for later requests.
    for meaning in describe {
        ledger.record(Some(pending.provider.label()), Some(&pending.prompt), genverse_rules::log::Action::Describe { meaning });
    }
    let portal = match answer.portal.as_deref().map(genverse_rules::log::PortalTo::parse) {
        Some(Ok(to)) => Some(to),
        Some(Err(why)) => {
            chat.say(Role::System, format!("⚠ The portal can't lead there: {why}."));
            None
        }
        None => None,
    };
    let Some(spec) = answer.build else {
        // A portal for the build that's selected, without changing how it looks.
        if let (Some(to), Some(build)) = (portal, creations.selected().map(|b| b.build)) {
            let label = to.label().to_string();
            ledger.record(None, None, genverse_rules::log::Action::SetPortal { build, to: Some(to) });
            chat.say(Role::System, format!("It's a portal to {label} now: walk up to it and press E."));
        }
        keep_original(&mut creations, &mut chat);
        return;
    };
    match compile(&spec, &ledger.rules.build) {
        Ok(build) => {
            // A revision stays where the current draft stands; a new build goes on the
            // site the agent was shown.
            let current = creations.draft.as_ref().and_then(|d| drafts.get(d.entity).ok());
            let at = match current {
                Some(t) => (t.translation, t.rotation.to_euler(EulerRot::YXZ).0),
                None => (site.origin, site.yaw),
            };
            let s = build.stats;
            info!("draft {:?}: {:?}", spec.name, s);
            let next = if current.is_some() && !creations.draft.as_ref().is_some_and(|d| d.follow) {
                "Place to keep the change, or Cancel to restore the original."
            } else {
                "Click to place it, or ask for changes."
            };
            chat.say(
                Role::System,
                format!(
                    "Draft \"{}\": {}×{}×{} voxels, {} solid, {} faces. {next}",
                    spec.name, s.size.x, s.size.y, s.size.z, s.voxels, s.faces
                ),
            );
            let provider = pending.provider.label().to_string();
            creations.set_draft(&mut commands, &build.model, (spec, s, pending.prompt, provider), at);
            if let Some(draft) = &mut creations.draft {
                draft.portal = portal;
            }
            // `GENVERSE_KEEP=1` places a scripted build where it was drafted, as a click would.
            if std::env::var("GENVERSE_KEEP").is_ok() {
                // On the ground under where it was drafted.
                let column = (at.0.xz() / crate::world::terrain::VOXEL_SIZE).floor().as_ivec2();
                let ground = (ledger.log.terrain().height(column.x, column.y) + 1) as f32 * crate::world::terrain::VOXEL_SIZE;
                let transform = Transform::from_translation(at.0.with_y(ground)).with_rotation(Quat::from_rotation_y(at.1));
                match creations.place_draft(&mut commands, &transform, &mut ledger) {
                    Ok(name) => chat.say(Role::System, format!("Placed \"{name}\".")),
                    Err(why) => chat.say(Role::System, format!("⚠ {why}")),
                }
            }
        }
        Err(errors) if pending.repairs == 0 && !repairing => {
            warn!("build rejected, asking for a fix: {errors:?}");
            chat.say(Role::System, format!("Build broke {} rule(s); asking for a fix…", errors.len()));
            let request = prompt::repair(&spec, &errors);
            chat.start(
                tools.as_deref(),
                &ledger.rules,
                pending.provider,
                pending.prompt.clone(),
                request,
                pending.repairs + 1,
                pending.restore,
            );
        }
        Err(errors) => {
            warn!("build rejected again: {errors:?}");
            chat.say(Role::System, format!("⚠ Build rejected:\n• {}", errors.join("\n• ")));
            keep_original(&mut creations, &mut chat);
        }
    }
}

/// Keyboard shortcuts. They depend on context (a draft, a selection, or neither) and
/// are ignored while typing in the chat.
fn shortcuts(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    binds: Res<Bindings>,
    blocked: Res<InputBlocked>,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut world: ResMut<VoxelWorld>,
    mut ledger: ResMut<Ledger>,
    mut overlay: ResMut<ParcelOverlay>,
    mut transforms: Query<&mut Transform, With<DraftMarker>>,
) {
    if blocked.0 {
        return;
    }
    let command = keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight, KeyCode::SuperLeft, KeyCode::SuperRight]);
    // Ctrl/Cmd+Z undoes, with Shift redoes.
    if command && keys.just_pressed(KeyCode::KeyZ) {
        let redo = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
        if creations.draft.is_none() && landscape.draft.is_none() {
            undo(&mut ledger, &mut chat, redo);
        }
        return;
    }
    if command {
        return;
    }
    if binds.just_pressed("hints") {
        chat.hints = !chat.hints;
        prefs::set("hints", chat.hints);
        return;
    }
    // T opens the chat to talk to the agent, like Minecraft's chat key.
    if binds.just_pressed("chat") {
        chat.open = true;
        chat.focus_input = true;
        return;
    }

    // P: outline the protected spawn around you.
    if binds.just_pressed("parcels") {
        overlay.visible = !overlay.visible;
        return;
    }

    // G: select an area of ground to reshape.
    if binds.just_pressed("select_area") && creations.draft.is_none() && landscape.draft.is_none() && chat.pending.is_none() {
        landscape.selecting = !landscape.selecting;
        if landscape.selecting {
            creations.selected = None;
        }
        return;
    }

    if let Some(draft) = &creations.draft {
        let (entity, follow) = (draft.entity, draft.follow);
        if keys.just_pressed(KeyCode::Escape) {
            creations.discard_draft(&mut commands);
            return;
        }
        let Ok(mut transform) = transforms.get_mut(entity) else {
            return;
        };
        if keys.just_pressed(KeyCode::Enter) {
            let at = *transform;
            match creations.place_draft(&mut commands, &at, &mut ledger) {
                Ok(name) => chat.say(Role::System, format!("Placed \"{name}\".")),
                Err(why) => chat.say(Role::System, why),
            }
            return;
        }
        if binds.just_pressed("move_build") && !follow {
            if let Some(draft) = &mut creations.draft {
                draft.follow = true;
            }
            return;
        }
        for (action, how) in [
            ("turn_left", Adjust::Turn(1.0)),
            ("turn_right", Adjust::Turn(-1.0)),
            ("raise", Adjust::Raise(1.0)),
            ("lower", Adjust::Raise(-1.0)),
        ] {
            if binds.just_pressed(action) {
                creations.adjust_draft(&mut transform, how);
            }
        }
        return;
    }

    if landscape.draft.is_some() {
        if keys.just_pressed(KeyCode::Enter) {
            keep_terrain(&mut landscape, &mut world, &mut ledger, &mut chat);
        } else if keys.just_pressed(KeyCode::Escape) {
            landscape.discard(&mut world);
            chat.say(Role::System, "Put the ground back.");
        }
        return;
    }
    if landscape.selecting {
        if keys.just_pressed(KeyCode::Escape) {
            landscape.selecting = false;
        }
        return;
    }

    if creations.selected.is_some() {
        if binds.just_pressed("move_build") {
            if let Some(name) = creations.edit_selected(&mut commands, true) {
                chat.say(Role::System, format!("Moving \"{name}\": point and click to place it."));
            }
        } else if binds.just_pressed("remove_build") {
            match creations.delete_selected(&mut commands, &mut ledger) {
                Ok(name) => chat.say(Role::System, format!("Deleted \"{name}\". Ctrl/Cmd+Z brings it back.")),
                Err(why) => chat.say(Role::System, why),
            }
        } else if keys.just_pressed(KeyCode::Escape) {
            creations.selected = None;
        } else if keys.just_pressed(KeyCode::Enter) {
            chat.open = true;
            chat.focus_input = true;
        }
    } else if keys.just_pressed(KeyCode::Escape) {
        landscape.area = None;
    } else if keys.just_pressed(KeyCode::Enter) {
        chat.open = true;
        chat.focus_input = true;
    }
}

/// Undoes the local player's latest change that can still be undone (`redo`: their
/// latest undo, which redoes it). The world follows once the ledger has it.
fn undo(ledger: &mut Ledger, chat: &mut Chat, redo: bool) {
    let Some(id) = ledger.latest_undoable(redo) else {
        chat.say(Role::System, if redo { "Nothing to redo." } else { "Nothing of yours left to undo." });
        return;
    };
    let what = match ledger.log.change(id).map(|c| &c.action) {
        Some(LogAction::Undo { change }) => ledger.log.change(*change).map(|c| c.action.describe()),
        other => other.map(LogAction::describe),
    }
    .unwrap_or_default();
    match ledger.undo(id) {
        Ok(()) if redo => chat.say(Role::System, format!("Redid: {what}. Ctrl/Cmd+Z undoes it again.")),
        Ok(()) => chat.say(Role::System, format!("Undid: {what}. Ctrl/Cmd+Shift+Z redoes it.")),
        Err(why) => chat.say(Role::System, why),
    }
}

fn keep_terrain(landscape: &mut Landscape, world: &mut VoxelWorld, ledger: &mut Ledger, chat: &mut Chat) {
    if let Some((_, name)) = landscape.keep(world, ledger) {
        chat.say(Role::System, format!("Kept \"{name}\". Ctrl/Cmd+Z undoes it; the area stays selected for more changes."));
    }
}

const REACH: f32 = 40.0;

/// The world ray under the mouse cursor, unless the cursor is over the panel.
fn cursor_ray(
    windows: &Query<&Window, With<PrimaryWindow>>,
    cameras: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    over_ui: &PointerOverUi,
) -> Option<(Vec2, Ray3d)> {
    if over_ui.0 {
        return None;
    }
    let cursor = windows.single().ok()?.cursor_position()?;
    let (camera, transform) = cameras.single().ok()?;
    Some((cursor, camera.viewport_to_world(transform, cursor).ok()?))
}

/// The closest thing under the cursor: terrain, or a placed build (with its entity).
fn pick(
    ray: Ray3d,
    world: &VoxelWorld,
    builds: &Query<(Entity, &VoxelModel, &GlobalTransform), (With<Creation>, Without<DraftMarker>)>,
) -> Option<(Hit, Option<Entity>)> {
    let terrain = world.raycast(ray, REACH).map(|hit| (hit, None));
    builds
        .iter()
        .filter_map(|(e, model, t)| raycast(model, t, ray, REACH).map(|hit| (hit, Some(e))))
        .chain(terrain)
        .min_by(|a, b| a.0.distance.total_cmp(&b.0.distance))
}

/// The draft sticks to whatever surface is under the cursor, like a block preview.
fn follow_cursor(
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    over_ui: Res<PointerOverUi>,
    world: Res<VoxelWorld>,
    creations: Res<Creations>,
    builds: Query<(Entity, &VoxelModel, &GlobalTransform), (With<Creation>, Without<DraftMarker>)>,
    mut drafts: Query<(&mut Transform, &Creation), With<DraftMarker>>,
) {
    let Some(draft) = creations.draft.as_ref().filter(|d| d.follow) else {
        return;
    };
    let Some((_, ray)) = cursor_ray(&windows, &cameras, &over_ui) else {
        return;
    };
    let Ok((mut transform, creation)) = drafts.get_mut(draft.entity) else {
        return;
    };
    let Some((hit, _)) = pick(ray, &world, &builds) else {
        return;
    };
    let size = (transform.rotation * creation.size).abs();
    if let Some(at) = placement_for_hit(&world, ray.get_point(hit.distance), hit.normal, size) {
        transform.translation = at + Vec3::Y * draft.lift;
    }
}

/// Flags a draft that would trap the hero inside it, or that the land rules don't
/// allow where it stands.
fn check_clearance(
    mut creations: ResMut<Creations>,
    ledger: Res<Ledger>,
    players: Query<&Transform, With<Player>>,
    drafts: Query<(&VoxelModel, &Transform), With<DraftMarker>>,
) {
    let Some(draft) = &creations.draft else {
        return;
    };
    let (Ok(hero), Ok((model, transform))) = (players.single(), drafts.get(draft.entity)) else {
        return;
    };
    let (lo, hi) = player::bounds(hero.translation);
    let blocked = overlaps_box(model, &GlobalTransform::from(*transform), lo, hi);
    let denied = creations.check_draft(transform, &ledger);
    if let Some(draft) = &mut creations.draft {
        draft.blocked = blocked;
        draft.denied = denied;
    }
}

/// Mouse, in the world (not over the panel). A click is a press and release without
/// dragging the camera.
/// - With a following draft: left places it, right cancels it.
/// - With a pinned revision: right cancels it (it's kept from the panel or with Enter).
/// - Otherwise: left selects the build under the cursor, double-click picks it up to
///   move it, middle copies it into a new draft, right clears the selection.
fn click_in_world(
    mut commands: Commands,
    time: Res<Time>,
    binds: Res<Bindings>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    over_ui: Res<PointerOverUi>,
    world: Res<VoxelWorld>,
    builds: Query<(Entity, &VoxelModel, &GlobalTransform), (With<Creation>, Without<DraftMarker>)>,
    drafts: Query<&Transform, With<DraftMarker>>,
    mut creations: ResMut<Creations>,
    mut chat: ResMut<Chat>,
    landscape: Res<Landscape>,
    mut ledger: ResMut<Ledger>,
    mut pressed_at: Local<[Option<Vec2>; 3]>,
    mut last_select: Local<Option<(Entity, f32)>>,
) {
    if landscape.selecting {
        *pressed_at = default();
        return;
    }
    let Some((cursor, ray)) = cursor_ray(&windows, &cameras, &over_ui) else {
        return;
    };
    // A click is a press and a release in the same spot (a drag looks around), on
    // whatever the player has these three on.
    let mut clicked = [false; 3];
    for (i, action) in ["click", "cancel", "copy"].into_iter().enumerate() {
        if binds.just_pressed(action) {
            pressed_at[i] = Some(cursor);
        }
        if binds.just_released(action) {
            clicked[i] = pressed_at[i].take().is_some_and(|start| start.distance(cursor) <= 5.0);
        }
    }
    let [left, right, middle] = clicked;

    if let Some(draft) = &creations.draft {
        if right {
            creations.discard_draft(&mut commands);
        } else if !draft.follow {
            // Pinned revisions are confirmed from the panel or with Enter.
        } else if left && let Ok(transform) = drafts.get(draft.entity).copied() {
            match creations.place_draft(&mut commands, &transform, &mut ledger) {
                Ok(name) => chat.say(Role::System, format!("Placed \"{name}\".")),
                Err(why) => chat.say(Role::System, why),
            }
        }
        return;
    }
    if right {
        creations.selected = None;
    }
    let target = || pick(ray, &world, &builds).and_then(|(_, entity)| entity);
    if middle && let Some(entity) = target() {
        let at = builds.get(entity).map(|(_, _, t)| t.compute_transform()).unwrap_or_default();
        if let Some(name) = creations.copy_as_draft(&mut commands, entity, &at) {
            chat.say(Role::System, format!("Copied \"{name}\": point and click to place the copy."));
        }
        return;
    }
    if left {
        let hit = target();
        let now = time.elapsed_secs();
        let double = hit.is_some() && last_select.is_some_and(|(e, t)| Some(e) == hit && now - t < 0.4);
        creations.selected = hit;
        *last_select = hit.map(|e| (e, now));
        if double && let Some(name) = creations.edit_selected(&mut commands, true) {
            *last_select = None;
            chat.say(Role::System, format!("Moving \"{name}\": point and click to place it."));
        }
    }
}

/// What each key does right now, at the bottom of the view: the actions for what you're
/// doing on top, and moving around beneath, quieter.
fn shortcut_hints(
    mut contexts: EguiContexts,
    chat: Res<Chat>,
    creations: Res<Creations>,
    landscape: Res<Landscape>,
    play: Res<game::Play>,
    inventory: Res<inventory::Inventory>,
    binds: Res<Bindings>,
) -> Result {
    // Each hint names the keys the player has on that action now.
    let k = |action: &str| binds.label(action);
    // What the mouse does by pointing keeps its usual name ("Click") until it's moved.
    let m = |action: &str, usual: &str| if binds.is_usual(action) { usual.to_string() } else { binds.label(action) };
    let pair = |a: &str, b: &str| format!("{}/{}", binds.short(a), binds.short(b));
    let hand = || if (1..=9).all(|i| binds.short(&format!("hand_{i}")) == i.to_string()) { "1-9".to_string() } else { format!("{}…", binds.short("hand_1")) };
    let actions: Vec<(String, &str)> = match &creations.draft {
        Some(d) if d.follow => vec![
            (m("click", "Click"), "place"),
            (pair("turn_left", "turn_right"), "turn"),
            (pair("raise", "lower"), "raise / lower"),
            (k("chat"), "ask for changes"),
            (format!("Esc or {}", m("cancel", "right-click")), "cancel"),
        ],
        Some(_) => vec![("Enter".into(), "keep the change"), (k("move_build"), "move it"), (k("chat"), "ask for more"), ("Esc".into(), "cancel")],
        None if landscape.draft.is_some() => {
            vec![("Enter".into(), "keep the change"), (k("chat"), "ask for changes"), ("Esc".into(), "put the ground back")]
        }
        None if landscape.selecting => vec![(m("click", "Drag"), "select an area (a click: 6 m)"), (format!("{} or Esc", k("select_area")), "stop")],
        None if play.playing.is_some() => {
            vec![(binds.movement(), "move"), (k("reset"), "reset"), (k("use"), "action"), ("Esc".into(), "leave the game")]
        }
        None if creations.selected.is_some() => vec![
            (k("chat"), "change it"),
            (format!("{} or double-click", k("move_build")), "move"),
            (binds.short("remove_build").into(), "remove"),
            (m("copy", "Middle-click"), "copy"),
            ("Ctrl/Cmd+Z".into(), "undo"),
            ("Esc".into(), "deselect"),
        ],
        None if chat.pending.is_some() => Vec::new(),
        None if landscape.area.is_some() => {
            vec![(k("chat"), "describe what this land should become"), (k("select_area"), "select again"), ("Esc".into(), "clear the area")]
        }
        None if inventory.open => {
            vec![("Click".into(), "pick up or put down a stack"), (hand(), "what's in hand"), (format!("{} or Esc", k("inventory")), "close")]
        }
        None if play.near.is_some() && inventory.held().is_some() => vec![
            (k("use"), "use"),
            (k("hit"), "hit"),
            (m("use_held", "Right-click"), "use what's in hand on it"),
            (hand(), "hand"),
            (k("inventory"), "inventory"),
            (k("chat"), "describe a build"),
            (k("history"), "history"),
        ],
        None if play.near.is_some() => vec![
            (k("use"), "use"),
            (k("hit"), "hit"),
            (k("chat"), "describe a build"),
            (k("select_area"), "reshape the ground"),
            (m("click", "Click"), "select a build"),
            (k("parcels"), "land"),
            (k("history"), "history"),
        ],
        None if inventory.held().is_some() => vec![
            (m("use_held", "Right-click"), "put down what's in hand"),
            (hand(), "hand"),
            (k("inventory"), "inventory"),
            (k("chat"), "describe a build"),
            (k("select_area"), "reshape the ground"),
            (m("click", "Click"), "select a build"),
            (k("history"), "history"),
        ],
        None => vec![
            (k("chat"), "describe a build"),
            (k("select_area"), "reshape the ground"),
            (m("click", "Click"), "select a build"),
            (m("copy", "Middle-click"), "copy a build"),
            (k("parcels"), "land"),
            (k("history"), "history"),
            (k("game_window"), "worlds"),
        ],
    };
    let actions: Vec<(&str, &str)> = actions.iter().map(|(keys, what)| (keys.as_str(), *what)).collect();
    let actions = actions.as_slice();
    // Looking around turns to the right button while the left one selects ground.
    let look = if !binds.is_usual("look") || !binds.is_usual("click") {
        format!("Hold {}", binds.label("look"))
    } else if landscape.selecting || landscape.draft.is_some() {
        "Right-drag".to_string()
    } else {
        "Drag".to_string()
    };
    let builder = if chat.open { "hide builder" } else { "builder" };
    let (movement, run, jump, view, panel, hints) = (binds.movement(), k("run"), k("jump"), k("view"), k("builder"), k("hints"));
    let moving = [
        (movement.as_str(), "move"),
        (run.as_str(), "run"),
        (jump.as_str(), "jump"),
        (look.as_str(), "look"),
        ("Scroll", "zoom"),
        (view.as_str(), "view"),
        (panel.as_str(), builder),
        (hints.as_str(), "hide hints"),
    ];

    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let screen = ctx.viewport_rect();
    let covered = chat.covered * screen.width();
    // Hidden, the hints leave a small reminder of how to bring them back.
    if !chat.hints {
        egui::Area::new("shortcut_hints".into())
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(-covered / 2.0, -16.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(p.overlay)
                    .stroke(egui::Stroke::new(1.0, p.line))
                    .corner_radius(theme::ROUND + 1)
                    .inner_margin(egui::Margin::symmetric(8, 5))
                    .show(ui, |ui| ui.horizontal(|ui| hint_row(ui, &[(hints.as_str(), "show hints")], true)));
            });
        return Ok(());
    }
    egui::Area::new("shortcut_hints".into())
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(-covered / 2.0, -16.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.overlay)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(theme::ROUND + 1)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_max_width(screen.width() - covered - 48.0);
                    ui.spacing_mut().item_spacing = egui::vec2(5.0, 6.0);
                    let mut divider = None;
                    if !actions.is_empty() {
                        ui.horizontal_wrapped(|ui| hint_row(ui, actions, false));
                        divider = Some(ui.cursor().top() + 1.0);
                        ui.add_space(4.0);
                    }
                    ui.horizontal_wrapped(|ui| hint_row(ui, &moving, true));
                    // Drawn last so it spans the wider of the two rows.
                    if let Some(y) = divider {
                        ui.painter().hline(ui.min_rect().x_range(), y, egui::Stroke::new(1.0, p.line));
                    }
                });
        });
    Ok(())
}

/// One row of hints: keycaps, then what they do. Quiet rows are smaller and muted.
fn hint_row(ui: &mut egui::Ui, hints: &[(&str, &str)], quiet: bool) {
    let p = theme::palette(ui.ctx());
    let (size, color) = if quiet { (11.5, p.muted) } else { (13.0, p.text) };
    for (i, (keys, what)) in hints.iter().enumerate() {
        if i > 0 {
            ui.add_space(if quiet { 8.0 } else { 10.0 });
        }
        for (j, alternative) in keys.split(" or ").enumerate() {
            if j > 0 {
                ui.label(egui::RichText::new("or").size(size - 1.0).color(p.faint));
            }
            // "Q/E" is two keys; "Ctrl/Cmd+Z" is one.
            let parts: Vec<&str> = alternative.split('/').collect();
            if parts.len() > 1 && parts.iter().all(|k| k.chars().count() == 1) {
                for k in parts {
                    theme::key(ui, k, quiet);
                }
            } else {
                theme::key(ui, &mouse_key(alternative), quiet);
            }
        }
        ui.label(egui::RichText::new(*what).size(size).color(color));
    }
}

/// Mouse buttons get a mouse icon on their keycap.
fn mouse_key(key: &str) -> String {
    let (icon, name) = match key.to_lowercase().as_str() {
        "click" => (icon::MOUSE_LEFT_CLICK, "Click"),
        "double-click" => (icon::MOUSE_LEFT_CLICK, "Double-click"),
        "right-click" => (icon::MOUSE_RIGHT_CLICK, "Right-click"),
        "middle-click" => (icon::MOUSE_MIDDLE_CLICK, "Middle-click"),
        "drag" => (icon::MOUSE_LEFT_CLICK, "Drag"),
        "right-drag" => (icon::MOUSE_RIGHT_CLICK, "Right-drag"),
        "scroll" => (icon::MOUSE_SCROLL, "Scroll"),
        _ => return key.to_string(),
    };
    format!("{icon} {name}")
}

fn attach_interface(mut commands: Commands, screens: Query<Entity, With<ScreenCamera>>) {
    for screen in &screens {
        commands.entity(screen).insert(PrimaryEguiContext);
    }
}

/// Adds the Phosphor icon font as a fallback, so icons render inside any label, and
/// applies the builder's look.
fn install_icon_font(mut contexts: EguiContexts, mut done: Local<bool>) -> Result {
    if *done {
        return Ok(());
    }
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    let ctx = contexts.ctx_mut()?;
    ctx.set_fonts(fonts);
    theme::apply(ctx);
    *done = true;
    Ok(())
}

/// Shifts the picture so the player stays centered in the part of the window the panel
/// leaves uncovered: an off-center projection, so the view doesn't shrink or stretch.
fn center_view(
    chat: Res<Chat>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut cameras: Query<&mut Camera, With<Camera3d>>,
) {
    let (Ok(window), Ok(mut camera)) = (windows.single(), cameras.single_mut()) else { return };
    let size = window.physical_size();
    let covered = (chat.covered * size.x as f32).round() as u32;
    let view = (covered > 0 && size.x > 0 && size.y > 0).then(|| SubCameraView {
        full_size: UVec2::new(size.x + covered, size.y),
        offset: Vec2::new(covered as f32, 0.0),
        size,
    });
    if camera.sub_camera_view != view {
        camera.sub_camera_view = view;
    }
}

/// The name a message is shown with, and the color of its avatar and name.
fn sender(role: &Role, me: &str, mine: egui::Color32, p: &theme::Palette) -> (String, egui::Color32) {
    match role {
        Role::Player => (me.to_string(), mine),
        Role::Agent(name) if name.starts_with("Claude") => (name.to_string(), p.claude),
        Role::Agent(name) => (name.to_string(), p.codex),
        Role::System => (String::new(), p.faint),
    }
}

/// How long ago, for a message's name line ("3 min ago") or its hover gutter ("3m").
fn ago(at: Instant, short: bool) -> String {
    match (at.elapsed().as_secs(), short) {
        (s, false) if s < 60 => "just now".into(),
        (s, true) if s < 60 => "now".into(),
        (s, false) if s < 3600 => format!("{} min ago", s / 60),
        (s, true) if s < 3600 => format!("{}m", s / 60),
        (s, _) => format!("{}h{}", s / 3600, if short { "" } else { " ago" }),
    }
}

/// The conversation in Discord's layout: an avatar and name line start each run of
/// messages from one sender, and a row lights up under the pointer.
fn show_messages(ui: &mut egui::Ui, messages: &[Message], me: &str, mine: egui::Color32) {
    let p = theme::palette(ui.ctx());
    let mut previous: Option<&Message> = None;
    for m in messages {
        let first = !previous.is_some_and(|p| p.role == m.role && m.role != Role::System && m.at.duration_since(p.at) < GROUP);
        if first && previous.is_some() {
            ui.add_space(12.0);
        }
        previous = Some(m);
        let (name, color) = sender(&m.role, me, mine, p);
        let background = ui.painter().add(egui::Shape::Noop);
        let row = ui.horizontal_top(|ui| {
            ui.add_space(12.0);
            if m.role == Role::System {
                ui.add_sized([36.0, 18.0], egui::Label::new(egui::RichText::new(icon::INFO).size(15.0).color(p.faint)));
            } else if first {
                theme::avatar(ui, &name, color, 36.0);
            } else {
                ui.allocate_exact_size(egui::vec2(36.0, 1.0), egui::Sense::hover());
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width() - 12.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                if m.role == Role::System {
                    ui.label(egui::RichText::new(&m.text).size(13.0).color(p.muted));
                    return;
                }
                if first {
                    ui.horizontal(|ui| {
                        // Light markers would vanish on parchment; names go darker there.
                        let readable = if ui.visuals().dark_mode { color } else { theme::shade(color, 0.62) };
                        ui.label(egui::RichText::new(&name).strong().color(readable));
                        if matches!(m.role, Role::Agent(_)) {
                            theme::tag(ui, "AGENT", p.primary, p.on_primary);
                        }
                        ui.label(egui::RichText::new(ago(m.at, false)).size(11.0).color(p.faint));
                    });
                }
                ui.label(egui::RichText::new(&m.text).color(p.text));
            });
        });
        let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), row.response.rect.y_range().expand(2.0));
        if ui.rect_contains_pointer(rect) {
            ui.painter().set(background, egui::Shape::rect_filled(rect, 0.0, p.hover));
            if !first && m.role != Role::System {
                let at = egui::pos2(rect.left() + 30.0, rect.top() + 11.0);
                ui.painter().text(at, egui::Align2::CENTER_CENTER, ago(m.at, true), egui::FontId::proportional(10.0), p.faint);
            }
        }
    }
}

/// The commands matching what's typed after `/`, Discord-style: the highlighted one is
/// what Tab or Enter fills in, kept in view when the arrows move it. Returns the one
/// clicked.
fn command_suggestions(
    ui: &mut egui::Ui,
    matches: &[&ops::Op],
    highlighted: usize,
    moved: bool,
    typed: &str,
    admin: bool,
) -> Option<&'static str> {
    let p = theme::palette(ui.ctx());
    let mut clicked = None;
    egui::Frame::new()
        .fill(p.raised)
        .stroke(egui::Stroke::new(1.0, p.line))
        .corner_radius(theme::ROUND)
        .inner_margin(egui::Margin::symmetric(6, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                let title = if typed.is_empty() { "Commands".to_string() } else { format!("Commands matching /{typed}") };
                theme::section(ui, &title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let keys = format!("{}{} choose · Tab or Enter fills in", icon::ARROW_UP, icon::ARROW_DOWN);
                    ui.label(egui::RichText::new(keys).size(10.5).color(p.faint));
                });
            });
            ui.add_space(2.0);
            egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                for (i, command) in matches.iter().enumerate() {
                    let row = command_row(ui, command, i == highlighted, admin);
                    if i == highlighted && moved {
                        row.scroll_to_me(None);
                    }
                    if row.clicked() {
                        clicked = Some(command.name);
                    }
                }
            });
        });
    clicked
}

/// One suggested command: its name and arguments, what it does, and whether it's for
/// admins. Lit when highlighted or under the pointer.
fn command_row(ui: &mut egui::Ui, command: &ops::Op, highlighted: bool, admin: bool) -> egui::Response {
    let p = theme::palette(ui.ctx());
    let background = ui.painter().add(egui::Shape::Noop);
    let allowed = admin || !command.admin;
    let inner = ui.horizontal(|ui| {
        ui.set_width(ui.available_width());
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                let name = if allowed { p.strong } else { p.muted };
                ui.label(egui::RichText::new(format!("/{}", command.name)).strong().color(name));
                if !command.args.is_empty() {
                    ui.label(egui::RichText::new(command.args).monospace().size(11.5).color(p.muted));
                }
                if command.admin {
                    theme::tag(ui, "ADMIN", if allowed { p.warn } else { p.faint }, p.footer);
                }
            });
            let about = if allowed { command.about.to_string() } else { format!("{} · admins only", command.about) };
            ui.label(egui::RichText::new(about).size(11.5).color(p.muted));
            ui.add_space(3.0);
        });
    });
    let rect = inner.response.rect;
    let response = ui.interact(rect, ui.id().with(("command", command.name)), egui::Sense::click());
    if highlighted || response.hovered() {
        let fill = if highlighted { p.primary.gamma_multiply(0.22) } else { p.hover };
        ui.painter().set(background, egui::Shape::rect_filled(rect, theme::ROUND, fill));
    }
    if highlighted {
        let edge = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
        ui.painter().rect_filled(edge, theme::ROUND, p.primary);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// How to finish the command being typed, once its name is in.
fn command_usage(ui: &mut egui::Ui, command: &ops::Op, admin: bool) {
    let p = theme::palette(ui.ctx());
    egui::Frame::new()
        .fill(p.raised)
        .stroke(egui::Stroke::new(1.0, p.line))
        .corner_radius(theme::ROUND)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(format!("/{}", command.name)).strong().color(p.strong));
                if !command.args.is_empty() {
                    ui.label(egui::RichText::new(command.args).monospace().size(11.5).color(p.primary));
                }
                let mut about = command.about.to_string();
                if command.admin && !admin {
                    about.push_str(" · admins only");
                }
                ui.label(egui::RichText::new(about).size(11.5).color(p.muted));
            });
        });
}

/// Your name and marker color, as others see them. The color applies as you pick it;
/// the name when you save it. Both reach other players while you're online.
fn profile_section(ui: &mut egui::Ui, chat: &mut Chat, ledger: &mut Ledger) {
    theme::section(ui, "Your name");
    let mut save = false;
    if let Some(name) = &mut chat.name {
        let field = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY).char_limit(32).hint_text("your name"));
        if std::mem::take(&mut chat.focus_name) {
            field.request_focus();
        }
        save = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    }
    ui.add_space(4.0);
    theme::section(ui, "Your color");
    let current = chat.marker(&ledger.profile.id);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        for (i, (label, _)) in presence::MARKERS.iter().enumerate() {
            let color = theme::color(presence::marker_color(i as u8));
            if theme::swatch(ui, color, i as u8 == current).on_hover_text(*label).clicked() {
                chat.marker = Some(i as u8);
                prefs::set("marker", i as u64);
            }
        }
    });
    theme::note(ui, "Your ring, outlines and name tag in multiplayer");
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let typed = chat.name.as_deref().is_some_and(|n| !n.trim().is_empty());
        save |= ui.add_enabled(typed, theme::Block::new(format!("{} Save", icon::CHECK), theme::Tone::Primary)).on_hover_text("Enter").clicked();
        if ui.add(theme::Block::new("Cancel", theme::Tone::Plain)).clicked() {
            chat.name = None;
        }
    });
    if save && let Some(name) = chat.name.take() {
        ledger.rename(&name);
    }
}

/// Playing with others: worlds on your network, joining by address, or hosting this one.
fn network_section(ui: &mut egui::Ui, chat: &mut Chat, ledger: &Ledger, lan: &lan::Lan, action: &mut Option<Action>) {
    let p = theme::palette(ui.ctx());
    theme::section(ui, "Play together");
    let Some(online) = &ledger.online else {
        ui.horizontal(|ui| {
            let label = if lan.play_until.is_some() { "Looking…" } else { "Play together" };
            if ui
                .add_enabled(lan.play_until.is_none(), theme::Block::new(format!("{} {label}", icon::USERS_THREE), theme::Tone::Primary))
                .on_hover_text("Join a world on your network, or open this one if there's none")
                .clicked()
            {
                *action = Some(Action::PlayTogether);
            }
            if ui.add(theme::Block::new("Host", theme::Tone::Plain)).on_hover_text("Open this world to others on port 7878").clicked() {
                *action = Some(Action::Host);
            }
        });
        if lan.worlds.is_empty() {
            theme::note(ui, "No worlds on your network yet");
        }
        for world in &lan.worlds {
            ui.horizontal(|ui| {
                let b = &world.beacon;
                ui.label(egui::RichText::new(icon::GLOBE).color(p.muted));
                ui.label(egui::RichText::new(&b.world).strong());
                theme::note(ui, format!("{} playing", b.players));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let join = ui
                        .add_enabled(b.compatible(), theme::Block::new("Join", theme::Tone::Primary))
                        .on_disabled_hover_text("Made with another version of the game");
                    if join.clicked() {
                        *action = Some(Action::JoinLan(world.address()));
                    }
                });
            });
        }
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut chat.server_addr)
                    .desired_width(ui.available_width() - 52.0)
                    .hint_text("Join by address: host:port"),
            );
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let filled = !chat.server_addr.trim().is_empty();
            if (ui.add_enabled(filled, theme::Block::new("Join", theme::Tone::Plain)).on_hover_text("Play on that server").clicked() || enter) && filled {
                *action = Some(Action::Join);
            }
        });
        return;
    };
    ui.horizontal(|ui| {
        let status = match (&online.closed, &ledger.hosting) {
            (Some(_), _) => format!("{} · reconnecting…", online.addr),
            (None, Some(server)) => format!("Others can join on port {}", server.addr.port()),
            (None, None) => format!("Playing at {}", online.addr),
        };
        theme::note(ui, status);
        let (label, tip) = if ledger.hosting.is_some() {
            ("Stop hosting", "Close this world to others and go back to your local world")
        } else {
            ("Leave", "Back to your local world")
        };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(theme::Block::new(label, theme::Tone::Danger)).on_hover_text(tip).clicked() {
                *action = Some(Action::Leave);
            }
        });
    });
}

enum Action {
    Send,
    Stop,
    Place,
    Discard,
    Undo,
    Adjust(Adjust),
    Move,
    Delete,
    Deselect,
    KeepTerrain,
    DiscardTerrain,
    /// Yes or no to what the agent asked to do.
    Allow,
    Deny,
    SelectArea,
    ClearArea,
    Join,
    JoinLan(String),
    PlayTogether,
    Host,
    Leave,
    History,
}

fn chat_panel(
    mut contexts: EguiContexts,
    (mut binds, mut graphics, mut panels): (ResMut<Bindings>, ResMut<crate::graphics::Graphics>, ResMut<panels::Panels>),
    egui_input: Res<EguiWantsInput>,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
    mut commands: Commands,
    tools: Option<Res<McpServer>>,
    mut transforms: Query<&mut Transform, Without<Player>>,
    mut landscape: ResMut<Landscape>,
    mut world: ResMut<VoxelWorld>,
    mut ledger: ResMut<Ledger>,
    (mut history, mut settings, mut travel, mut worlds_window): (
        ResMut<history::History>,
        ResMut<settings::SettingsWindow>,
        ResMut<worlds::Travel>,
        ResMut<worlds::WorldsWindow>,
    ),
    mut lan: ResMut<lan::Lan>,
    time: Res<Time>,
    players: Query<&Transform, With<Player>>,
    cameras: Query<&GlobalTransform, With<Camera3d>>,
) -> Result {
    if binds.just_pressed("builder") && !egui_input.wants_keyboard_input() {
        chat.open = !chat.open;
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let mut action = None;
    let mut hide = false;
    // egui 0.36 panels attach to a Ui; this one covers the whole window behind other layers.
    let screen = ctx.viewport_rect();
    let mut viewport = egui::Ui::new(
        ctx.clone(),
        "viewport".into(),
        egui::UiBuilder::new().layer_id(egui::LayerId::background()).max_rect(screen),
    );
    let me = ledger.profile.name.clone();
    let mine = theme::color(presence::marker_color(chat.marker(&ledger.profile.id)));
    let mut open = chat.open;

    let panel = egui::Panel::right("builder")
        .default_size(360.0)
        .size_range(300.0..=560.0)
        .resizable(true)
        .drag_to_open(false)
        .frame(egui::Frame::new().fill(p.panel))
        .show_collapsible(&mut viewport, &mut open, |ui| {
            let chat = &mut *chat;
            egui::Panel::top("builder_header")
                .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(12, 8)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon::HASH).size(20.0).color(p.faint));
                        ui.label(egui::RichText::new("builder").size(16.0).strong().color(p.strong))
                            .on_hover_text("Describe something; an agent builds it in front of you");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::icon_button(ui, icon::SIDEBAR_SIMPLE, "Hide (Tab)") {
                                hide = true;
                            }
                            let (sun, tip) = if ui.visuals().dark_mode { (icon::SUN, "Light theme") } else { (icon::MOON, "Dark theme") };
                            if theme::icon_button(ui, sun, tip) {
                                theme::toggle(ui.ctx());
                            }
                            if theme::icon_button(ui, icon::GAUGE, "Settings: display and graphics") {
                                settings.open = !settings.open;
                            }
                            if theme::icon_button(ui, icon::CLOCK_COUNTER_CLOCKWISE, "History (H)") {
                                action = Some(Action::History);
                            }
                            if theme::icon_button(ui, icon::GLOBE_HEMISPHERE_WEST, "Worlds, saves and portals (O)") {
                                worlds_window.open = !worlds_window.open;
                            }
                            ui.add_enabled_ui(chat.pending.is_none(), |ui| {
                                egui::ComboBox::from_id_salt("provider")
                                    .selected_text(format!("{} {}", icon::ROBOT, chat.provider.label()))
                                    .show_ui(ui, |ui| {
                                        for p in Provider::ALL {
                                            ui.selectable_value(&mut chat.provider, p, p.label());
                                        }
                                    })
                                    .response
                                    .on_hover_text("Which agent builds for you");
                            });
                        });
                    });
                });

            // Bottom-up: the footer with your name, then the composer and whatever is in
            // hand; messages fill what's left.
            egui::Panel::bottom("builder_footer")
                .frame(egui::Frame::new().fill(p.footer).inner_margin(egui::Margin::symmetric(10, 8)))
                .show_separator_line(false)
                .show(ui, |ui| {
                    if chat.name.is_some() {
                        profile_section(ui, chat, &mut ledger);
                        ui.add_space(6.0);
                    } else if chat.network {
                        network_section(ui, chat, &ledger, &lan, &mut action);
                        ui.add_space(6.0);
                    }
                    ui.horizontal(|ui| {
                        let face = theme::avatar(ui, &me, mine, 32.0)
                            .interact(egui::Sense::click())
                            .on_hover_text("Edit your name and color");
                        // A status pip on the avatar, grass-green while playing with others.
                        let dot = face.rect.right_bottom() - egui::vec2(3.0, 3.0);
                        ui.painter().rect_filled(egui::Rect::from_center_size(dot, egui::vec2(11.0, 11.0)), 2, p.footer);
                        let online = ledger.online.as_ref().is_some_and(|o| o.closed.is_none());
                        let pip = if online { p.online } else { p.faint };
                        ui.painter().rect_filled(egui::Rect::from_center_size(dot, egui::vec2(7.0, 7.0)), 1, pip);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(egui::RichText::new(&me).size(14.0).strong().color(p.strong));
                            let status = match (&ledger.online, &ledger.hosting) {
                                (None, _) => "Local world".to_string(),
                                (Some(online), _) if online.closed.is_some() => "Reconnecting…".to_string(),
                                (Some(_), Some(server)) => format!("Hosting on port {}", server.addr.port()),
                                (Some(online), None) => format!("Online at {}", online.addr),
                            };
                            ui.label(egui::RichText::new(status).size(11.5).color(p.muted));
                        });
                        let mut edit = face.clicked();
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let tip = if chat.network { "Hide Play together" } else { "Play together" };
                            let color = if chat.network && chat.name.is_none() { p.strong } else { p.muted };
                            let button = ui
                                .add(egui::Button::new(egui::RichText::new(icon::USERS_THREE).size(18.0).color(color)).frame(false))
                                .on_hover_text(tip);
                            if button.clicked() {
                                chat.network = chat.name.is_some() || !chat.network;
                                chat.name = None;
                            }
                            // Worlds found on the network, as an unread-style badge.
                            if !lan.worlds.is_empty() && ledger.online.is_none() {
                                let at = button.rect.right_top() + egui::vec2(-2.0, 3.0);
                                ui.painter().rect_filled(egui::Rect::from_center_size(at, egui::vec2(14.0, 14.0)), 2, p.danger);
                                ui.painter().text(
                                    at,
                                    egui::Align2::CENTER_CENTER,
                                    lan.worlds.len().to_string(),
                                    egui::FontId::proportional(10.0),
                                    egui::Color32::WHITE,
                                );
                            }
                            let color = if chat.name.is_some() { p.strong } else { p.muted };
                            edit |= ui
                                .add(egui::Button::new(egui::RichText::new(icon::PENCIL_SIMPLE).size(18.0).color(color)).frame(false))
                                .on_hover_text("Edit your name and color")
                                .clicked();
                        });
                        // Opens the editor, or closes it without saving the name.
                        if edit {
                            chat.name = match chat.name {
                                Some(_) => None,
                                None => Some(ledger.profile.name.clone()),
                            };
                            chat.focus_name = chat.name.is_some();
                        }
                    });
                });

            egui::Panel::bottom("builder_input")
                .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin { left: 12, right: 12, top: 4, bottom: 6 }))
                .show_separator_line(false)
                .show(ui, |ui| {
                    if let Some(draft) = &creations.draft {
                        let accent = if draft.denied.is_some() || draft.blocked { p.warn } else { p.primary };
                        theme::card(ui, accent, |ui| {
                            theme::card_title(ui, icon::CUBE, &format!("Draft · {}", draft.spec.name));
                            let s = draft.stats;
                            theme::note(ui, format!("{}×{}×{} · {} voxels · {} faces", s.size.x, s.size.y, s.size.z, s.voxels, s.faces));
                            if let Ok(transform) = transforms.get(draft.entity) {
                                let yaw = transform.rotation.to_euler(EulerRot::YXZ).0;
                                let parcels = genverse_rules::land::build_parcels(transform.translation, yaw, &draft.spec.size);
                                theme::note(ui, ledger.parcel_line(&parcels));
                            }
                            if let Some(why) = &draft.denied {
                                ui.label(egui::RichText::new(format!("{} Not here: {why}", icon::WARNING)).color(p.warn));
                            }
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                let why_not = if draft.blocked { Some("It would overlap you") } else { draft.denied.as_deref() };
                                let place = ui
                                    .add_enabled(why_not.is_none(), theme::Block::new(format!("{} Place", icon::CHECK), theme::Tone::Primary))
                                    .on_hover_text("Enter, or click in the world")
                                    .on_disabled_hover_text(why_not.unwrap_or_default());
                                if place.clicked() {
                                    action = Some(Action::Place);
                                }
                                let discard = if draft.replaces.is_some() { "Cancel edit" } else { "Discard" };
                                if ui.add(theme::Block::new(format!("{} {discard}", icon::X), theme::Tone::Plain)).on_hover_text("Esc or right-click").clicked() {
                                    action = Some(Action::Discard);
                                }
                                if !draft.follow
                                    && ui
                                        .add(theme::Block::new(format!("{} Move", icon::ARROWS_OUT_CARDINAL), theme::Tone::Plain))
                                        .on_hover_text("M: pick it up and place it somewhere else")
                                        .clicked()
                                {
                                    action = Some(Action::Move);
                                }
                            });
                            ui.horizontal(|ui| {
                                let tip = |r: egui::Response, t: &str| r.on_hover_text(t.to_string()).clicked();
                                if tip(ui.add(theme::Block::new(icon::ARROW_COUNTER_CLOCKWISE, theme::Tone::Plain)), "Turn left (Q)") {
                                    action = Some(Action::Adjust(Adjust::Turn(1.0)));
                                }
                                if tip(ui.add(theme::Block::new(icon::ARROW_CLOCKWISE, theme::Tone::Plain)), "Turn right (E)") {
                                    action = Some(Action::Adjust(Adjust::Turn(-1.0)));
                                }
                                if tip(ui.add(theme::Block::new(icon::ARROW_LINE_UP, theme::Tone::Plain)), "Raise (R)") {
                                    action = Some(Action::Adjust(Adjust::Raise(1.0)));
                                }
                                if tip(ui.add(theme::Block::new(icon::ARROW_LINE_DOWN, theme::Tone::Plain)), "Lower (F)") {
                                    action = Some(Action::Adjust(Adjust::Raise(-1.0)));
                                }
                                if !draft.follow {
                                    theme::note(ui, "Revising in place");
                                }
                            });
                            theme::note(
                                ui,
                                if draft.follow {
                                    "Point at a spot and click to place · right-click or Esc cancels"
                                } else {
                                    "Ask for more changes · Place keeps them · Cancel restores"
                                },
                            );
                        });
                        ui.add_space(6.0);
                    } else if let Some(selected) = creations.selected() {
                        theme::card(ui, p.select, |ui| {
                            theme::card_title(ui, icon::CUBE, &selected.name);
                            ui.label(egui::RichText::new(format!("\"{}\"", selected.prompt)).italics().size(12.5).color(p.muted));
                            if let Some((made, changed)) = ledger.credits(selected.build) {
                                let mut credit = format!("Made by {}", ledger.describe(made));
                                if let Some(changed) = changed {
                                    credit.push_str(&format!(" · changed by {}", ledger.describe(changed)));
                                }
                                theme::note(ui, credit);
                            }
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add(theme::Block::new(format!("{} Move", icon::ARROWS_OUT_CARDINAL), theme::Tone::Plain))
                                    .on_hover_text("M or double-click: pick it up and place it somewhere else")
                                    .clicked()
                                {
                                    action = Some(Action::Move);
                                }
                                if ui
                                    .add(theme::Block::new(format!("{} Delete", icon::TRASH), theme::Tone::Danger))
                                    .on_hover_text("Delete or Backspace")
                                    .clicked()
                                {
                                    action = Some(Action::Delete);
                                }
                                if ui.add(theme::Block::new("Deselect", theme::Tone::Plain)).on_hover_text("Esc or right-click").clicked() {
                                    action = Some(Action::Deselect);
                                }
                            });
                            theme::note(ui, "Type below to change it · middle-click copies it");
                        });
                        ui.add_space(6.0);
                    }
                    if let Some(draft) = &landscape.draft {
                        theme::card(ui, p.online, |ui| {
                            theme::card_title(ui, icon::MOUNTAINS, &format!("Land preview · {}", draft.spec.name));
                            theme::note(ui, describe_stats(draft.area, &draft.stats));
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                if ui.add(theme::Block::new(format!("{} Keep", icon::CHECK), theme::Tone::Primary)).on_hover_text("Enter").clicked() {
                                    action = Some(Action::KeepTerrain);
                                }
                                if ui.add(theme::Block::new(format!("{} Put back", icon::X), theme::Tone::Plain)).on_hover_text("Esc").clicked() {
                                    action = Some(Action::DiscardTerrain);
                                }
                            });
                            theme::note(ui, "Ask for changes · Keep applies it · Put back restores the ground");
                        });
                        ui.add_space(6.0);
                    } else if let Some(area) = landscape.area.filter(|_| creations.draft.is_none() && creations.selected.is_none()) {
                        theme::card(ui, p.online, |ui| {
                            let size = area.size_m();
                            theme::card_title(ui, icon::SELECTION, &format!("Area · {:.1} × {:.1} m", size.x, size.y));
                            ui.label(egui::RichText::new(land_summary(&ledger, area)).size(12.5));
                            theme::note(ui, ledger.player_line());
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                if ui.add(theme::Block::new(format!("{} Select again", icon::SELECTION), theme::Tone::Plain)).on_hover_text("G").clicked() {
                                    action = Some(Action::SelectArea);
                                }
                                if ui.add(theme::Block::new("Clear", theme::Tone::Plain)).on_hover_text("Esc").clicked() {
                                    action = Some(Action::ClearArea);
                                }
                            });
                            theme::note(ui, "Describe what it should become: a pond, a path, a hill…");
                        });
                        ui.add_space(6.0);
                    } else if landscape.selecting {
                        theme::card(ui, p.online, |ui| {
                            theme::card_title(ui, icon::SELECTION, "Select an area");
                            theme::note(ui, "Drag across the ground; a click selects 6 m");
                        });
                        ui.add_space(6.0);
                    }

                    // The composer: one rounded box, like Discord's message bar.
                    let hint = if let Some(job) = &chat.pending {
                        format!("{} is working…", job.provider.label())
                    } else if creations.selected.is_some() {
                        "Change it…  (T)".to_string()
                    } else if landscape.target().is_some() && creations.draft.is_none() {
                        "Make this land…  (T)".to_string()
                    } else {
                        "Build me…  (T)".to_string()
                    };
                    let idle = chat.pending.is_none();
                    // Typing a slash command: the commands it could be, or how to finish it.
                    let composer = egui::Id::new("composer");
                    let admin = ledger.land.is_admin(&ledger.profile.id);
                    let typed = chat.input.trim_start().to_string();
                    let (word, rest) = typed.strip_prefix('/').map_or((None, None), |t| match t.split_once(' ') {
                        Some((word, rest)) => (Some(word), Some(rest)),
                        None => (Some(t), None),
                    });
                    let matches: Vec<&ops::Op> = match (word, rest) {
                        (Some(word), None) => {
                            let word = word.to_lowercase();
                            ops::commands().filter(|c| c.name.starts_with(&word)).collect()
                        }
                        _ => Vec::new(),
                    };
                    let usage = word.filter(|_| rest.is_some()).and_then(|w| {
                        let w = w.to_lowercase();
                        ops::commands().find(|c| c.name == w)
                    });
                    chat.suggestion = chat.suggestion.min(matches.len().saturating_sub(1));
                    let mut complete = None;
                    let mut moved = false;
                    if !matches.is_empty() && ui.memory(|m| m.has_focus(composer)) {
                        let (up, down, tab) = ui.input_mut(|i| {
                            (
                                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                                i.consume_key(egui::Modifiers::NONE, egui::Key::Tab),
                            )
                        });
                        let n = matches.len();
                        moved = up || down;
                        if up {
                            chat.suggestion = (chat.suggestion + n - 1) % n;
                        }
                        if down {
                            chat.suggestion = (chat.suggestion + 1) % n;
                        }
                        if tab {
                            complete = Some(matches[chat.suggestion].name);
                        }
                    }
                    if !matches.is_empty() {
                        if let Some(name) = command_suggestions(ui, &matches, chat.suggestion, moved, word.unwrap_or_default(), admin) {
                            complete = Some(name);
                        }
                        ui.add_space(6.0);
                    } else if let Some(command) = usage {
                        command_usage(ui, command, admin);
                        ui.add_space(6.0);
                    }
                    egui::Frame::new()
                        .fill(p.input)
                        .stroke(egui::Stroke::new(1.0, p.line))
                        .corner_radius(theme::ROUND)
                        .inner_margin(egui::Margin { left: 12, right: 6, top: 7, bottom: 7 })
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let input = ui.add_enabled(
                                    idle,
                                    egui::TextEdit::singleline(&mut chat.input)
                                        .id(composer)
                                        // Tab fills in commands instead of moving focus on.
                                        .lock_focus(true)
                                        .frame(egui::Frame::NONE)
                                        .margin(egui::Margin::ZERO)
                                        .hint_text(egui::RichText::new(hint).color(p.faint))
                                        .desired_width(ui.available_width() - 30.0),
                                );
                                if std::mem::take(&mut chat.focus_input) {
                                    input.request_focus();
                                }
                                let mut enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                // Enter on a half-typed command finishes it; on a whole one
                                // that needs nothing more, it runs it.
                                if enter && let Some(pick) = matches.get(chat.suggestion) {
                                    let whole = word.is_some_and(|w| w.eq_ignore_ascii_case(pick.name));
                                    if !(whole && pick.args.is_empty()) {
                                        complete = Some(pick.name);
                                        enter = false;
                                    }
                                }
                                if let Some(name) = complete.take() {
                                    chat.input = if ops::commands().any(|c| c.name == name && !c.args.is_empty()) {
                                        format!("/{name} ")
                                    } else {
                                        format!("/{name}")
                                    };
                                    chat.suggestion = 0;
                                    // The caret goes after what was filled in.
                                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), composer) {
                                        let end = egui::text::CCursor::new(chat.input.chars().count());
                                        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                                        state.store(ui.ctx(), composer);
                                    }
                                    input.request_focus();
                                }
                                let ready = idle && !chat.input.trim().is_empty();
                                let color = if ready { p.primary } else { p.faint };
                                let send = ui
                                    .add_enabled(ready, egui::Button::new(egui::RichText::new(icon::PAPER_PLANE_RIGHT).size(18.0).color(color)).frame(false))
                                    .on_hover_text("Send (Enter)");
                                if (send.clicked() || enter) && ready {
                                    action = Some(Action::Send);
                                    input.request_focus();
                                }
                            });
                        });

                    // Under the composer: who's working (like "is typing…"), or the tally.
                    ui.horizontal(|ui| {
                        ui.set_min_height(22.0);
                        if let Some(job) = &chat.pending {
                            ui.add(egui::Spinner::new().size(12.0).color(p.muted));
                            ui.label(egui::RichText::new(job.provider.label()).size(12.0).strong().color(p.text));
                            ui.label(egui::RichText::new("is working…").size(12.0).color(p.muted));
                            if ui.add(theme::Block::new(format!("{} Stop", icon::STOP), theme::Tone::Plain)).clicked() {
                                action = Some(Action::Stop);
                            }
                        } else {
                            let placed = creations.placed.len();
                            let builds = if placed == 1 { "build" } else { "builds" };
                            ui.label(egui::RichText::new(format!("{placed} {builds} placed · / for commands")).size(11.5).color(p.faint));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let undo = ui
                                .add_enabled(
                                    creations.draft.is_none() && landscape.draft.is_none() && ledger.latest_undoable(false).is_some(),
                                    egui::Button::new(egui::RichText::new(format!("{} Undo", icon::ARROW_U_UP_LEFT)).size(11.5)).frame(false),
                                )
                                .on_hover_text("Ctrl/Cmd+Z");
                            if undo.clicked() {
                                action = Some(Action::Undo);
                            }
                        });
                    });
                });

            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add_space(8.0);
                    show_messages(ui, &chat.messages, &me, mine);
                    if let Some(pending) = &chat.pending {
                        let (name, color) = sender(&Role::Agent(pending.provider.label()), &me, mine, p);
                        ui.add_space(12.0);
                        chat.activity.show(ui, &name, color);
                        if let Some(ask) = &chat.asking {
                            ui.add_space(8.0);
                            theme::card(ui, p.warn, |ui| {
                                theme::card_title(ui, icon::QUESTION, &format!("{name} wants to"));
                                ui.label(egui::RichText::new(&ask.what).color(p.strong));
                                ui.add_space(2.0);
                                ui.horizontal(|ui| {
                                    if ui.add(theme::Block::new(format!("{} Allow", icon::CHECK), theme::Tone::Primary)).clicked() {
                                        action = Some(Action::Allow);
                                    }
                                    if ui.add(theme::Block::new(format!("{} No", icon::X), theme::Tone::Plain)).clicked() {
                                        action = Some(Action::Deny);
                                    }
                                });
                            });
                        }
                    }
                    ui.add_space(12.0);
                });
        });
    // How much of the window the panel covers, mid-slide included, so the view recenters.
    chat.panel = panel.map(|panel| panel.response.rect.intersect(screen)).filter(|r| r.width() > 0.0);
    chat.covered = chat.panel.map_or(0.0, |r| r.width() / screen.width().max(1.0));
    chat.open = open && !hide;

    // While hidden, a button in the corner brings the panel back.
    if !chat.open {
        egui::Area::new("builder_reopen".into())
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
            .show(ctx, |ui| {
                let busy = chat.pending.is_some();
                let label = if busy { format!("{} Working…", icon::CHATS_CIRCLE) } else { icon::CHATS_CIRCLE.to_string() };
                let button = egui::Button::new(egui::RichText::new(label).size(18.0).color(p.strong))
                    .fill(p.overlay)
                    .corner_radius(theme::ROUND)
                    .min_size(egui::vec2(38.0, 38.0));
                if ui.add(button).on_hover_text("Builder (Tab)").clicked() {
                    chat.open = true;
                }
            });
    }

    match action {
        Some(Action::Send) if chat.input.trim_start().starts_with('/') => {
            let text = std::mem::take(&mut chat.input);
            let cx = &mut ops::Context {
                ledger: &mut ledger,
                world: &mut world,
                creations: &mut creations,
                landscape: &mut landscape,
                travel: &mut travel,
                at: players.single().map_or([0.0; 3], |t| t.translation.to_array()),
                agent: None,
                binds: &mut binds,
                graphics: &mut graphics,
                panels: &mut panels,
            };
            run_command(&mut chat, text.trim(), cx);
        }
        Some(Action::Send) => {
            if let Some(why) = land_refusal(&creations, &landscape, &ledger) {
                chat.say(Role::System, why);
            } else {
                let player = players.single().ok().map(|t| t.translation);
                let area = area_context(&creations, &landscape, &world, &ledger, player, cameras.single().ok());
                let here = player.map(|p| here_context(&world, ledger.log.terrain(), p, cameras.single().ok()));
                chat.send(&mut creations, &mut commands, tools.as_deref(), &ledger, area, here);
            }
        }
        Some(Action::KeepTerrain) => keep_terrain(&mut landscape, &mut world, &mut ledger, &mut chat),
        Some(Action::DiscardTerrain) => {
            landscape.discard(&mut world);
            chat.say(Role::System, "Put the ground back.");
        }
        Some(Action::SelectArea) => landscape.selecting = true,
        Some(Action::ClearArea) => landscape.area = None,
        Some(Action::History) => history.open = !history.open,
        Some(Action::Join | Action::JoinLan(_) | Action::PlayTogether | Action::Host | Action::Leave)
            if creations.draft.is_some() || landscape.draft.is_some() =>
        {
            chat.say(Role::System, "Keep or discard your draft first.");
        }
        Some(Action::Join) => {
            let addr = chat.server_addr.trim().to_string();
            let reply = ops::said(online::join(&mut ledger, &addr));
            chat.say(Role::System, reply);
        }
        Some(Action::Host) => {
            let reply = ops::said(online::host(&mut ledger));
            chat.say(Role::System, reply);
        }
        Some(Action::JoinLan(addr)) => {
            let reply = ops::said(online::join(&mut ledger, &addr));
            chat.say(Role::System, reply);
        }
        Some(Action::PlayTogether) => {
            lan.play_together(time.elapsed_secs());
            chat.say(Role::System, "Looking for worlds on your network…");
        }
        Some(Action::Leave) => {
            let reply = ops::said(online::leave(&mut ledger, &mut travel));
            if !reply.is_empty() {
                chat.say(Role::System, reply);
            }
        }
        Some(Action::Allow) => {
            if let Some(ask) = chat.asking.take() {
                let at = players.single().map_or([0.0; 3], |t| t.translation.to_array());
                ask.allow(&mut chat, &mut ledger, &mut world, &mut creations, &mut landscape, (&mut binds, &mut graphics, &mut panels), at);
            }
        }
        Some(Action::Deny) => {
            if let Some(ask) = chat.asking.take() {
                ask.refuse("the player said no");
            }
        }
        Some(Action::Stop) => {
            if let Some(p) = &chat.pending
                && let Ok(job) = p.job.lock()
            {
                job.cancel();
            }
        }
        Some(Action::Place) => {
            let transform = creations.draft.as_ref().and_then(|d| transforms.get(d.entity).ok()).copied();
            if let Some(transform) = transform {
                match creations.place_draft(&mut commands, &transform, &mut ledger) {
                    Ok(name) => chat.say(Role::System, format!("Placed \"{name}\".")),
                    Err(why) => chat.say(Role::System, why),
                }
            }
        }
        Some(Action::Discard) => creations.discard_draft(&mut commands),
        Some(Action::Adjust(how)) => {
            if let Some(entity) = creations.draft.as_ref().map(|d| d.entity)
                && let Ok(mut transform) = transforms.get_mut(entity)
            {
                creations.adjust_draft(&mut transform, how);
            }
        }
        Some(Action::Move) => {
            if let Some(draft) = &mut creations.draft {
                draft.follow = true;
            } else if let Some(name) = creations.edit_selected(&mut commands, true) {
                chat.say(Role::System, format!("Moving \"{name}\": point and click to place it."));
            }
        }
        Some(Action::Delete) => {
            match creations.delete_selected(&mut commands, &mut ledger) {
                Ok(name) => chat.say(Role::System, format!("Deleted \"{name}\".")),
                Err(why) => chat.say(Role::System, why),
            }
        }
        Some(Action::Deselect) => creations.selected = None,
        Some(Action::Undo) => undo(&mut ledger, &mut chat, false),
        None => {}
    }
    Ok(())
}
