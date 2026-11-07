//! Builder chat (desktop only): ask a local Claude Code or Codex CLI to design a voxel
//! object, preview it in front of you, then place, move, or discard it. Click a placed
//! build to select it, then edit (move, turn, or revise it with the agent) or delete it.
//!
//! Flow: chat message -> [`runner`] runs the CLI headless with the build schema ->
//! [`spec::compile`] validates and rasterizes the answer (asking the agent to fix it once
//! if it breaks a limit) -> a draft appears in the world -> Place saves it.
//!
//! Land works the same way ([`landscape`]): select an area with G, describe it, and
//! the agent's terrain operations are previewed in the world until kept.

mod commands;
mod creations;
mod history;
mod lan;
mod landscape;
mod mcp;
mod online;
mod prefs;
mod presence;
mod prompt;
mod runner;
mod theme;
use cubelark_rules::build as spec;
mod tools;

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use bevy::{camera::SubCameraView, prelude::*, window::PrimaryWindow};
use bevy_egui::{
    EguiContexts, EguiGlobalSettings, EguiPlugin, EguiPrimaryContextPass, egui, input::EguiWantsInput,
};
use egui_phosphor::regular as icon;

use crate::{
    player::{self, InputBlocked, Player, PointerOverUi},
    voxel::{
        VoxelModel,
        query::{Hit, overlaps_box, raycast},
    },
    world::{VoxelWorld, ledger::Ledger},
};
use cubelark_rules::{
    land::area_parcels,
    log::Action as LogAction,
    settings::{LandMode, WorldRules},
};
use creations::{
    Adjust, Creation, Creations, DraftMarker, Undone, load_creations, outline_creations, placement_for_hit,
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
            .insert_resource(EguiGlobalSettings { enable_absorb_bevy_input_system: true, ..default() })
            .init_resource::<Chat>()
            .init_resource::<Creations>()
            .init_resource::<BuildSite>()
            .init_resource::<Landscape>()
            .init_resource::<ParcelOverlay>()
            .init_resource::<presence::Peers>()
            .init_resource::<history::History>()
            .init_resource::<lan::Lan>()
            .init_gizmo_group::<AreaGizmos>()
            .init_gizmo_group::<presence::RingGizmos>()
            .add_systems(Startup, (load_creations, configure_area_gizmos, presence::configure_ring_gizmos, start_tool_server, lan::start_listening))
            .add_systems(
                Update,
                (
                    block_game_input,
                    center_view,
                    (online::sync_online, lan::update_lan, update_build_site, answer_tool_calls, poll_agent).chain(),
                    (presence::send_presence, presence::move_avatars, presence::draw_peers, history::toggle_history),
                    auto_prompt,
                    (shortcuts, select_area, follow_cursor, check_clearance, click_in_world, outline_creations, draw_area, highlight_area, draw_parcels)
                        .chain(),
                ),
            )
            .add_systems(EguiPrimaryContextPass, (install_icon_font, chat_panel, shortcut_hints).chain())
            .add_systems(EguiPrimaryContextPass, (presence::name_tags, presence::hover_credit, history::history_window));
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
    status: String,
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
}

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
                       ground, and describe it: \"a pond with reeds\", \"a winding path\", \"terraced hillside\"."
                    .into(),
                at: Instant::now(),
            }],
            status: String::new(),
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
        }
    }
}

impl Chat {
    /// The marker color this player shows others.
    fn marker(&self, me: &str) -> u8 {
        self.marker.unwrap_or_else(|| presence::default_marker(me))
    }

    fn say(&mut self, role: Role, text: impl Into<String>) {
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
        let job = runner::start(provider, prompt::system(tools.is_some(), rules), request, tools);
        self.status = format!("{} is working…", provider.label());
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
        rules: &WorldRules,
        area: Option<String>,
    ) {
        let text = self.input.trim().to_string();
        if text.is_empty() || self.pending.is_some() {
            return;
        }
        self.input.clear();
        let restore = area.is_none() && creations.draft.is_none() && creations.edit_selected(commands, false).is_some();
        let turns: Vec<_> = self
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| prompt::Turn { from_player: m.role == Role::Player, text: &m.text })
            .collect();
        let history = &turns[turns.len().saturating_sub(HISTORY)..];
        let draft = creations.draft.as_ref().map(|d| &d.spec);
        let request = prompt::request(history, draft, area.as_deref(), &text);
        self.say(Role::Player, text.clone());
        self.start(tools, rules, self.provider, text, request, 0, restore);
    }
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

/// Why a message about the selected land can't go to the agent: the land rules don't
/// let the player change it, so no answer would help.
fn land_refusal(creations: &Creations, landscape: &Landscape, ledger: &Ledger) -> Option<String> {
    if creations.draft.is_some() || creations.selected.is_some() {
        return None;
    }
    let area = landscape.target()?;
    let why = ledger.check(&area_parcels(area)).err()?;
    Some(format!("⚠ Can't change this land: {why}. Select an area you can change (P shows parcels)."))
}

/// `CUBELARK_PROMPT="a red mailbox" cargo run` sends one message once the hero has
/// landed. Pair with `CUBELARK_SHOT` to script a build end to end. `CUBELARK_AREA=12`
/// selects a 12 m square of ground in front of the hero (first, or on its own).
fn auto_prompt(
    mut commands: Commands,
    tools: Option<Res<McpServer>>,
    mut sent: Local<bool>,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut overlay: ResMut<ParcelOverlay>,
    mut world: ResMut<VoxelWorld>,
    mut ledger: ResMut<Ledger>,
    players: Query<(&Player, &Transform)>,
    cameras: Query<&GlobalTransform, With<Camera3d>>,
) {
    let Ok((player, hero)) = players.single() else {
        return;
    };
    if *sent || !player.has_landed() {
        return;
    }
    *sent = true;
    if std::env::var("CUBELARK_PARCELS").is_ok() {
        overlay.visible = true;
    }
    if let Some(size) = std::env::var("CUBELARK_AREA").ok().and_then(|s| s.parse::<f32>().ok()) {
        let forward = (hero.rotation * Vec3::NEG_Z).xz().normalize_or_zero();
        let center = hero.translation.xz() + forward * (size / 2.0 + 2.0);
        let half = Vec2::splat(size / 2.0);
        let cell = |p: Vec2| (p / crate::world::terrain::VOXEL_SIZE).floor().as_ivec2();
        landscape.area = Some(cubelark_rules::sculpt::Area::from_corners(cell(center - half), cell(center + half) - 1));
    }
    if let Ok(text) = std::env::var("CUBELARK_PROMPT") {
        if let Ok(provider) = std::env::var("CUBELARK_AGENT") {
            chat.provider = if provider == "codex" { Provider::Codex } else { Provider::Claude };
        }
        // `CUBELARK_SELECT_LAST=1` targets the most recent build, like clicking it first.
        if std::env::var("CUBELARK_SELECT_LAST").is_ok() {
            creations.selected = creations.placed.last().map(|(e, _)| *e);
        }
        // Commands, `;`-separated: `CUBELARK_PROMPT="/rate 2; /history"`.
        if text.trim_start().starts_with('/') {
            for command in text.split(';') {
                let cx = &mut commands::Context {
                    commands: &mut commands,
                    ledger: &mut ledger,
                    world: &mut world,
                    creations: &mut creations,
                    landscape: &mut landscape,
                };
                run_command(&mut chat, command.trim(), cx);
            }
            return;
        }
        chat.input = text;
        if let Some(why) = land_refusal(&creations, &landscape, &ledger) {
            chat.input.clear();
            chat.say(Role::System, why);
            return;
        }
        let area = area_context(&creations, &landscape, &world, &ledger, Some(hero.translation), cameras.single().ok());
        chat.send(&mut creations, &mut commands, tools.as_deref(), &ledger.rules, area);
    }
}

/// Echoes a chat command and its reply.
fn run_command(chat: &mut Chat, text: &str, cx: &mut commands::Context) {
    chat.say(Role::Player, text);
    // Play together is the panel's, not a world command.
    if text.trim() == "/play" {
        chat.play_together = true;
        chat.say(Role::System, "Looking for worlds on your network…");
        return;
    }
    let reply = commands::run(text, cx);
    info!("{text} -> {reply}");
    chat.say(Role::System, reply);
}

/// Serves the read-only world tools to agents. Without it, agents still work blind.
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
    drafts: Query<&Transform, With<DraftMarker>>,
) {
    let Some(pending) = &chat.pending else {
        return;
    };
    let mut finished = None;
    let mut status = None;
    if let Ok(job) = pending.job.lock() {
        while let Ok(event) = job.events.try_recv() {
            match event {
                AgentEvent::Status(s) => status = Some(format!("{} — {s} ({}s)", pending.provider.label(), job.started.elapsed().as_secs())),
                AgentEvent::Finished(result) => finished = Some(result),
            }
        }
    }
    if let Some(status) = status {
        chat.status = status;
    }
    let Some(result) = finished else {
        return;
    };
    let pending = chat.pending.take().expect("checked above");
    chat.status.clear();

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
                    // `CUBELARK_KEEP=1` keeps a scripted preview, as if Enter was pressed.
                    if std::env::var("CUBELARK_KEEP").is_ok() {
                        keep_terrain(&mut creations, &mut landscape, &mut world, &mut ledger, &mut chat);
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
    let Some(spec) = answer.build else {
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
    if command && keys.just_pressed(KeyCode::KeyZ) {
        if landscape.draft.is_none() {
            undo(&mut commands, &mut creations, &mut landscape, &mut world, &mut ledger, &mut chat);
        }
        return;
    }
    if command {
        return;
    }
    if keys.just_pressed(KeyCode::F1) {
        chat.hints = !chat.hints;
        prefs::set("hints", chat.hints);
        return;
    }
    // T opens the chat to talk to the agent, like Minecraft's chat key.
    if keys.just_pressed(KeyCode::KeyT) {
        chat.open = true;
        chat.focus_input = true;
        return;
    }

    // P: show who owns the land around you.
    if keys.just_pressed(KeyCode::KeyP) {
        overlay.visible = !overlay.visible;
        return;
    }

    // G: select an area of ground to reshape.
    if keys.just_pressed(KeyCode::KeyG) && creations.draft.is_none() && landscape.draft.is_none() && chat.pending.is_none() {
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
        if keys.just_pressed(KeyCode::KeyM) && !follow {
            if let Some(draft) = &mut creations.draft {
                draft.follow = true;
            }
            return;
        }
        for (key, how) in [
            (KeyCode::KeyQ, Adjust::Turn(1.0)),
            (KeyCode::KeyE, Adjust::Turn(-1.0)),
            (KeyCode::KeyR, Adjust::Raise(1.0)),
            (KeyCode::KeyF, Adjust::Raise(-1.0)),
        ] {
            if keys.just_pressed(key) {
                creations.adjust_draft(&mut transform, how);
            }
        }
        return;
    }

    if landscape.draft.is_some() {
        if keys.just_pressed(KeyCode::Enter) {
            keep_terrain(&mut creations, &mut landscape, &mut world, &mut ledger, &mut chat);
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
        if keys.just_pressed(KeyCode::KeyM) {
            if let Some(name) = creations.edit_selected(&mut commands, true) {
                chat.say(Role::System, format!("Moving \"{name}\": point and click to place it."));
            }
        } else if keys.any_just_pressed([KeyCode::Delete, KeyCode::Backspace]) {
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

/// Undoes the latest build or terrain change, whichever came last.
fn undo(
    commands: &mut Commands,
    creations: &mut Creations,
    landscape: &mut Landscape,
    world: &mut VoxelWorld,
    ledger: &mut Ledger,
    chat: &mut Chat,
) {
    match creations.undo(commands, ledger) {
        Some(Undone::Message(message)) => chat.say(Role::System, message),
        Some(Undone::Terrain(id)) => {
            if let Some(message) = landscape.undo(world, ledger, id) {
                chat.say(Role::System, message);
            }
        }
        None => {}
    }
}

fn keep_terrain(creations: &mut Creations, landscape: &mut Landscape, world: &mut VoxelWorld, ledger: &mut Ledger, chat: &mut Chat) {
    if let Some((id, name)) = landscape.keep(world, ledger) {
        creations.record_terrain(id);
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
    buttons: Res<ButtonInput<MouseButton>>,
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
    let mut clicked = [false; 3];
    for (i, button) in [MouseButton::Left, MouseButton::Right, MouseButton::Middle].into_iter().enumerate() {
        if buttons.just_pressed(button) {
            pressed_at[i] = Some(cursor);
        }
        if buttons.just_released(button) {
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
) -> Result {
    let actions: &[(&str, &str)] = match &creations.draft {
        Some(d) if d.follow => &[
            ("Click", "place"),
            ("Q/E", "turn"),
            ("R/F", "raise / lower"),
            ("T", "ask for changes"),
            ("Esc or right-click", "cancel"),
        ],
        Some(_) => &[("Enter", "keep the change"), ("M", "move it"), ("T", "ask for more"), ("Esc", "cancel")],
        None if landscape.draft.is_some() => {
            &[("Enter", "keep the change"), ("T", "ask for changes"), ("Esc", "put the ground back")]
        }
        None if landscape.selecting => &[("Drag", "select an area (a click: 6 m)"), ("G or Esc", "stop")],
        None if creations.selected.is_some() => &[
            ("T", "change it"),
            ("M or double-click", "move"),
            ("Delete", "remove"),
            ("Middle-click", "copy"),
            ("Ctrl/Cmd+Z", "undo"),
            ("Esc", "deselect"),
        ],
        None if chat.pending.is_some() => &[],
        None if landscape.area.is_some() => {
            &[("T", "describe what this land should become"), ("G", "select again"), ("Esc", "clear the area")]
        }
        None => &[
            ("T", "describe a build"),
            ("G", "reshape the ground"),
            ("Click", "select a build"),
            ("Middle-click", "copy a build"),
            ("P", "land"),
            ("H", "history"),
        ],
    };
    // Looking around turns to the right button while the left one selects ground.
    let look = if landscape.selecting || landscape.draft.is_some() { "Right-drag" } else { "Drag" };
    let builder = if chat.open { "hide builder" } else { "builder" };
    let moving =
        [("WASD", "move"), ("Shift", "run"), ("Space", "jump"), (look, "look"), ("Scroll", "zoom"), ("Tab", builder), ("F1", "hide hints")];

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
                    .show(ui, |ui| ui.horizontal(|ui| hint_row(ui, &[("F1", "show hints")], true)));
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
    matches: &[&commands::Command],
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
fn command_row(ui: &mut egui::Ui, command: &commands::Command, highlighted: bool, admin: bool) -> egui::Response {
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
fn command_usage(ui: &mut egui::Ui, command: &commands::Command, admin: bool) {
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
            (None, Some((addr, _))) => format!("Others can join on port {}", addr.port()),
            (None, None) => format!("Playing at {}", online.addr),
        };
        theme::note(ui, status);
        if ledger.hosting.is_none() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(theme::Block::new("Leave", theme::Tone::Danger)).on_hover_text("Back to your local world").clicked() {
                    *action = Some(Action::Leave);
                }
            });
        }
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
    SelectArea,
    ClearArea,
    Claim,
    Release,
    Join,
    JoinLan(String),
    PlayTogether,
    Host,
    Leave,
    History,
}

fn chat_panel(
    mut contexts: EguiContexts,
    keys: Res<ButtonInput<KeyCode>>,
    egui_input: Res<EguiWantsInput>,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
    mut commands: Commands,
    tools: Option<Res<McpServer>>,
    mut transforms: Query<&mut Transform, Without<Player>>,
    mut landscape: ResMut<Landscape>,
    mut world: ResMut<VoxelWorld>,
    mut ledger: ResMut<Ledger>,
    mut history: ResMut<history::History>,
    mut lan: ResMut<lan::Lan>,
    time: Res<Time>,
    players: Query<&Transform, With<Player>>,
    cameras: Query<&GlobalTransform, With<Camera3d>>,
) -> Result {
    if keys.just_pressed(KeyCode::Tab) && !egui_input.wants_keyboard_input() {
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
                            if theme::icon_button(ui, icon::CLOCK_COUNTER_CLOCKWISE, "History (H)") {
                                action = Some(Action::History);
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
                                (Some(_), Some((addr, _))) => format!("Hosting on port {}", addr.port()),
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
                                let parcels = cubelark_rules::land::build_parcels(transform.translation, yaw, &draft.spec.size);
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
                                if ledger.land.mode == LandMode::Parcels {
                                    if ui
                                        .add(theme::Block::new(format!("{} Claim", icon::FLAG), theme::Tone::Primary))
                                        .on_hover_text("Make the unclaimed parcels here yours, so only you can change them")
                                        .clicked()
                                    {
                                        action = Some(Action::Claim);
                                    }
                                    if ui.add(theme::Block::new("Release", theme::Tone::Plain)).on_hover_text("Give up your parcels here").clicked() {
                                        action = Some(Action::Release);
                                    }
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
                    let matches: Vec<&commands::Command> = match (word, rest) {
                        (Some(word), None) => {
                            let word = word.to_lowercase();
                            commands::COMMANDS.iter().filter(|c| c.name.starts_with(&word)).collect()
                        }
                        _ => Vec::new(),
                    };
                    let usage = word.filter(|_| rest.is_some()).and_then(|w| {
                        let w = w.to_lowercase();
                        commands::COMMANDS.iter().find(|c| c.name == w)
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
                                    chat.input = if commands::COMMANDS.iter().any(|c| c.name == name && !c.args.is_empty()) {
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
                                    creations.can_undo() && landscape.draft.is_none(),
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
            let cx = &mut commands::Context {
                commands: &mut commands,
                ledger: &mut ledger,
                world: &mut world,
                creations: &mut creations,
                landscape: &mut landscape,
            };
            run_command(&mut chat, text.trim(), cx);
        }
        Some(Action::Send) => {
            if let Some(why) = land_refusal(&creations, &landscape, &ledger) {
                chat.say(Role::System, why);
            } else {
                let player = players.single().ok().map(|t| t.translation);
                let area = area_context(&creations, &landscape, &world, &ledger, player, cameras.single().ok());
                chat.send(&mut creations, &mut commands, tools.as_deref(), &ledger.rules, area);
            }
        }
        Some(Action::KeepTerrain) => keep_terrain(&mut creations, &mut landscape, &mut world, &mut ledger, &mut chat),
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
            let reply = online::join(&mut ledger, &addr);
            chat.say(Role::System, reply);
        }
        Some(Action::Host) => {
            let reply = online::host(&mut ledger);
            chat.say(Role::System, reply);
        }
        Some(Action::JoinLan(addr)) => {
            let reply = online::join(&mut ledger, &addr);
            chat.say(Role::System, reply);
        }
        Some(Action::PlayTogether) => {
            lan.play_together(time.elapsed_secs());
            chat.say(Role::System, "Looking for worlds on your network…");
        }
        Some(Action::Leave) => {
            let reply = online::leave(&mut commands, &mut ledger, &mut world, &mut creations, &mut landscape);
            chat.say(Role::System, reply);
        }
        Some(Action::Claim) => {
            if let Some(area) = landscape.area {
                let message = match ledger.check(&area_parcels(area)) {
                    Ok(claims) if claims.is_empty() => "Nothing here to claim.".to_string(),
                    Ok(claims) => {
                        let parcels = claims.iter().map(|p| p.to_array()).collect();
                        ledger.record(None, None, LogAction::ClaimParcels { parcels });
                        format!("Claimed {} parcel(s); only you (and admins) can change them now.", claims.len())
                    }
                    Err(why) => format!("Can't claim it: {why}."),
                };
                chat.say(Role::System, message);
            }
        }
        Some(Action::Release) => {
            if let Some(area) = landscape.area {
                let id = ledger.profile.id.clone();
                let mine: Vec<[i32; 2]> = area_parcels(area)
                    .into_iter()
                    .filter(|p| ledger.land.parcels.get(p).is_some_and(|parcel| parcel.owner == id))
                    .map(|p| p.to_array())
                    .collect();
                let message = if mine.is_empty() {
                    "You don't own any parcels here.".to_string()
                } else {
                    let n = mine.len();
                    ledger.record(None, None, LogAction::ReleaseParcels { parcels: mine });
                    format!("Released {n} parcel(s).")
                };
                chat.say(Role::System, message);
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
        Some(Action::Undo) => undo(&mut commands, &mut creations, &mut landscape, &mut world, &mut ledger, &mut chat),
        None => {}
    }
    Ok(())
}
