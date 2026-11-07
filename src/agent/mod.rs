//! Builder chat (desktop only): ask a local Claude Code or Codex CLI to design a voxel
//! object, preview it in front of you, then place, move, or discard it. Click a placed
//! build to select it, then edit (move, turn, or revise it with the agent) or delete it.
//!
//! Flow: chat message -> [`runner`] runs the CLI headless with the build schema ->
//! [`spec::compile`] validates and rasterizes the answer (asking the agent to fix it once
//! if it breaks a limit) -> a draft appears in the world -> Place saves it.

mod creations;
mod mcp;
mod prompt;
mod runner;
mod spec;
mod tools;

use std::sync::Mutex;

use bevy::{prelude::*, window::PrimaryWindow};
use bevy_egui::{
    EguiContexts, EguiGlobalSettings, EguiPlugin, EguiPrimaryContextPass, egui, input::EguiWantsInput,
};
use egui_phosphor::regular as icon;

use crate::{
    player::{self, InputBlocked, Player},
    voxel::{
        VoxelModel,
        query::{Hit, overlaps_box, raycast},
    },
    world::VoxelWorld,
};
use creations::{
    Adjust, Creation, Creations, DraftMarker, load_creations, outline_creations, placement_for_hit,
};
use mcp::McpServer;
use runner::{AgentEvent, AgentJob, Provider};
use spec::compile;
use tools::{BuildSite, answer_tool_calls, update_build_site};

const HISTORY: usize = 8;

pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin::default())
            // Clicks, drags, and keys over the panel must not reach the game (it's off by default).
            .insert_resource(EguiGlobalSettings { enable_absorb_bevy_input_system: true, ..default() })
            .init_resource::<Chat>()
            .init_resource::<Creations>()
            .init_resource::<BuildSite>()
            .add_systems(Startup, (load_creations, start_tool_server))
            .add_systems(
                Update,
                (
                    block_game_input,
                    (update_build_site, answer_tool_calls, poll_agent).chain(),
                    auto_prompt,
                    (draft_keys, follow_cursor, check_clearance, click_in_world, outline_creations).chain(),
                ),
            )
            .add_systems(EguiPrimaryContextPass, (install_icon_font, chat_panel).chain());
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
                       selected, messages change that build."
                    .into(),
            }],
            status: String::new(),
            pending: None,
        }
    }
}

impl Chat {
    fn say(&mut self, role: Role, text: impl Into<String>) {
        self.messages.push(Message { role, text: text.into() });
    }

    fn start(
        &mut self,
        tools: Option<&McpServer>,
        provider: Provider,
        prompt: String,
        request: String,
        repairs: u8,
        restore: bool,
    ) {
        let job = runner::start(provider, prompt::system(tools.is_some()), request, tools);
        self.status = format!("{} is working…", provider.label());
        self.pending = Some(Pending { job: Mutex::new(job), provider, prompt, repairs, restore });
    }

    /// Sends the input. With no draft but a selected build, the message revises that
    /// build: it becomes a pinned draft the agent edits in place.
    fn send(&mut self, creations: &mut Creations, commands: &mut Commands, tools: Option<&McpServer>) {
        let text = self.input.trim().to_string();
        if text.is_empty() || self.pending.is_some() {
            return;
        }
        self.input.clear();
        let restore = creations.draft.is_none() && creations.edit_selected(commands, false).is_some();
        let turns: Vec<_> = self
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| prompt::Turn { from_player: m.role == Role::Player, text: &m.text })
            .collect();
        let history = &turns[turns.len().saturating_sub(HISTORY)..];
        let draft = creations.draft.as_ref().map(|d| &d.spec);
        let request = prompt::request(history, draft, &text);
        self.say(Role::Player, text.clone());
        self.start(tools, self.provider, text, request, 0, restore);
    }
}

/// `CUBELARK_PROMPT="a red mailbox" cargo run` sends one message once the hero has
/// landed. Pair with `CUBELARK_SHOT` to script a build end to end.
fn auto_prompt(
    mut commands: Commands,
    tools: Option<Res<McpServer>>,
    mut sent: Local<bool>,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
    players: Query<&Player>,
) {
    if *sent || !players.single().is_ok_and(|p| p.has_landed()) {
        return;
    }
    *sent = true;
    if let Ok(text) = std::env::var("CUBELARK_PROMPT") {
        if let Ok(provider) = std::env::var("CUBELARK_AGENT") {
            chat.provider = if provider == "codex" { Provider::Codex } else { Provider::Claude };
        }
        // `CUBELARK_SELECT_LAST=1` targets the most recent build, like clicking it first.
        if std::env::var("CUBELARK_SELECT_LAST").is_ok() {
            creations.selected = creations.placed.last().map(|(e, _)| *e);
        }
        chat.input = text;
        chat.send(&mut creations, &mut commands, tools.as_deref());
    }
}

/// Serves the read-only world tools to agents. Without it, agents still work blind.
fn start_tool_server(mut commands: Commands) {
    match McpServer::start(tools::definitions(), tools::INSTRUCTIONS) {
        Ok(server) => {
            info!("agent world tools at {}", server.url);
            commands.insert_resource(server);
        }
        Err(e) => warn!("couldn't start the agent tool server, agents won't see the world: {e}"),
    }
}

/// Keeps WASD and friends from moving the hero while typing in the chat.
fn block_game_input(egui: Res<EguiWantsInput>, mut blocked: ResMut<InputBlocked>) {
    blocked.0 = egui.wants_keyboard_input();
}

fn poll_agent(
    mut commands: Commands,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
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
    let Some(spec) = answer.build else {
        keep_original(&mut creations, &mut chat);
        return;
    };
    match compile(&spec) {
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
        Err(errors) if pending.repairs == 0 => {
            warn!("build rejected, asking for a fix: {errors:?}");
            chat.say(Role::System, format!("Build broke {} rule(s); asking for a fix…", errors.len()));
            let request = prompt::repair(&spec, &errors);
            chat.start(
                tools.as_deref(),
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

/// Q/E turn the draft, R/F raise and lower it, Esc cancels it.
fn draft_keys(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    blocked: Res<InputBlocked>,
    mut creations: ResMut<Creations>,
    mut transforms: Query<&mut Transform, With<DraftMarker>>,
) {
    let Some(entity) = creations.draft.as_ref().map(|d| d.entity) else {
        return;
    };
    if blocked.0 {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        creations.discard_draft(&mut commands);
        return;
    }
    let Ok(mut transform) = transforms.get_mut(entity) else {
        return;
    };
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
}

const REACH: f32 = 40.0;

/// The world ray under the mouse cursor, unless the cursor is over the panel.
fn cursor_ray(
    windows: &Query<&Window, With<PrimaryWindow>>,
    cameras: &Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    egui: &EguiWantsInput,
) -> Option<(Vec2, Ray3d)> {
    if egui.is_pointer_over_area() {
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
    egui: Res<EguiWantsInput>,
    world: Res<VoxelWorld>,
    creations: Res<Creations>,
    builds: Query<(Entity, &VoxelModel, &GlobalTransform), (With<Creation>, Without<DraftMarker>)>,
    mut drafts: Query<(&mut Transform, &Creation), With<DraftMarker>>,
) {
    let Some(draft) = creations.draft.as_ref().filter(|d| d.follow) else {
        return;
    };
    let Some((_, ray)) = cursor_ray(&windows, &cameras, &egui) else {
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

/// Flags a draft that would trap the hero inside it.
fn check_clearance(
    mut creations: ResMut<Creations>,
    players: Query<&Transform, With<Player>>,
    drafts: Query<(&VoxelModel, &Transform), With<DraftMarker>>,
) {
    let Some(draft) = &mut creations.draft else {
        return;
    };
    let (Ok(hero), Ok((model, transform))) = (players.single(), drafts.get(draft.entity)) else {
        return;
    };
    let (lo, hi) = player::bounds(hero.translation);
    draft.blocked = overlaps_box(model, &GlobalTransform::from(*transform), lo, hi);
}

/// A click (press and release without dragging the camera): with a draft, left places
/// it and right cancels it; otherwise left selects the build under the cursor.
fn click_in_world(
    mut commands: Commands,
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    egui: Res<EguiWantsInput>,
    world: Res<VoxelWorld>,
    builds: Query<(Entity, &VoxelModel, &GlobalTransform), (With<Creation>, Without<DraftMarker>)>,
    drafts: Query<&Transform, With<DraftMarker>>,
    mut creations: ResMut<Creations>,
    mut chat: ResMut<Chat>,
    mut pressed_at: Local<[Option<Vec2>; 2]>,
) {
    let Some((cursor, ray)) = cursor_ray(&windows, &cameras, &egui) else {
        return;
    };
    let mut clicked = [false; 2];
    for (i, button) in [MouseButton::Left, MouseButton::Right].into_iter().enumerate() {
        if buttons.just_pressed(button) {
            pressed_at[i] = Some(cursor);
        }
        if buttons.just_released(button) {
            clicked[i] = pressed_at[i].take().is_some_and(|start| start.distance(cursor) <= 5.0);
        }
    }
    let [left, right] = clicked;

    if let Some(draft) = &creations.draft {
        if right {
            creations.discard_draft(&mut commands);
        } else if !draft.follow {
            // Pinned revisions are confirmed from the panel, not by clicking the world.
        } else if left && draft.blocked {
            chat.say(Role::System, "Can't place it on top of you; point somewhere else.");
        } else if left
            && let Ok(transform) = drafts.get(draft.entity).copied()
            && let Some(name) = creations.place_draft(&mut commands, &transform)
        {
            chat.say(Role::System, format!("Placed \"{name}\"."));
        }
        return;
    }
    if left {
        creations.selected = pick(ray, &world, &builds).and_then(|(_, entity)| entity);
    }
}

/// Adds the Phosphor icon font as a fallback, so icons render inside any label.
fn install_icon_font(mut contexts: EguiContexts, mut done: Local<bool>) -> Result {
    if *done {
        return Ok(());
    }
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    contexts.ctx_mut()?.set_fonts(fonts);
    *done = true;
    Ok(())
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
) -> Result {
    if keys.just_pressed(KeyCode::Tab) && !egui_input.wants_keyboard_input() {
        chat.open = !chat.open;
    }
    if !chat.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let mut action = None;
    // egui 0.36 panels attach to a Ui; this one covers the whole window behind other layers.
    let mut viewport = egui::Ui::new(
        ctx.clone(),
        "viewport".into(),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(ctx.viewport_rect()),
    );

    egui::Panel::right("builder")
        .default_size(340.0)
        .resizable(true)
        .show(&mut viewport, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Builder");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(chat.pending.is_none(), |ui| {
                        egui::ComboBox::from_id_salt("provider")
                            .selected_text(chat.provider.label())
                            .show_ui(ui, |ui| {
                                for p in Provider::ALL {
                                    ui.selectable_value(&mut chat.provider, p, p.label());
                                }
                            });
                    });
                });
            });
            ui.label(egui::RichText::new("Tab hides this panel").small().weak());
            ui.separator();

            // Bottom-up: input, status, draft controls; messages fill what's left.
            egui::Panel::bottom("builder_input").show(ui, |ui| {
                if let Some(draft) = &creations.draft {
                    ui.add_space(4.0);
                    ui.group(|ui| {
                        ui.label(egui::RichText::new(format!("Draft: {}", draft.spec.name)).strong());
                        let s = draft.stats;
                        ui.label(format!("{}×{}×{} · {} voxels · {} faces", s.size.x, s.size.y, s.size.z, s.voxels, s.faces));
                        ui.horizontal(|ui| {
                            let tip = |r: egui::Response, t: &str| r.on_hover_text(t.to_string()).clicked();
                            if tip(ui.button(icon::ARROW_COUNTER_CLOCKWISE), "Turn left (Q)") {
                                action = Some(Action::Adjust(Adjust::Turn(1.0)));
                            }
                            if tip(ui.button(icon::ARROW_CLOCKWISE), "Turn right (E)") {
                                action = Some(Action::Adjust(Adjust::Turn(-1.0)));
                            }
                            if tip(ui.button(icon::ARROW_LINE_UP), "Raise (R)") {
                                action = Some(Action::Adjust(Adjust::Raise(1.0)));
                            }
                            if tip(ui.button(icon::ARROW_LINE_DOWN), "Lower (F)") {
                                action = Some(Action::Adjust(Adjust::Raise(-1.0)));
                            }
                            ui.separator();
                            let place = ui
                                .add_enabled(!draft.blocked, egui::Button::new(format!("{} Place", icon::CHECK)))
                                .on_disabled_hover_text("It would overlap you");
                            if place.clicked() {
                                action = Some(Action::Place);
                            }
                            let discard = if draft.replaces.is_some() { "Cancel edit" } else { "Discard" };
                            if ui.button(format!("{} {discard}", icon::X)).clicked() {
                                action = Some(Action::Discard);
                            }
                        });
                        if !draft.follow {
                            ui.horizontal(|ui| {
                                if ui
                                    .button(format!("{} Move", icon::ARROWS_OUT_CARDINAL))
                                    .on_hover_text("Pick it up and place it somewhere else")
                                    .clicked()
                                {
                                    action = Some(Action::Move);
                                }
                                ui.label(egui::RichText::new("Revising in place").small().weak());
                            });
                        }
                        let hint = if draft.follow {
                            "Point at a spot and click to place · right-click or Esc cancels"
                        } else {
                            "Ask for more changes · Place keeps them · Cancel restores"
                        };
                        ui.label(egui::RichText::new(hint).small().weak());
                    });
                } else if let Some(selected) = creations.selected() {
                    ui.add_space(4.0);
                    ui.group(|ui| {
                        ui.label(egui::RichText::new(format!("Selected: {}", selected.name)).strong());
                        ui.label(egui::RichText::new(format!("\"{}\" · {}", selected.prompt, selected.provider)).small().weak());
                        ui.horizontal(|ui| {
                            if ui
                                .button(format!("{} Move", icon::ARROWS_OUT_CARDINAL))
                                .on_hover_text("Pick it up and place it somewhere else")
                                .clicked()
                            {
                                action = Some(Action::Move);
                            }
                            if ui.button(format!("{} Delete", icon::TRASH)).clicked() {
                                action = Some(Action::Delete);
                            }
                            if ui.button("Deselect").clicked() {
                                action = Some(Action::Deselect);
                            }
                        });
                        ui.label(egui::RichText::new("Type below to change it").small().weak());
                    });
                }
                if chat.pending.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&chat.status);
                        if ui.small_button(format!("{} Stop", icon::STOP)).clicked() {
                            action = Some(Action::Stop);
                        }
                    });
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let input = ui.add_enabled(
                        chat.pending.is_none(),
                        egui::TextEdit::singleline(&mut chat.input)
                            .hint_text("Build me…")
                            .desired_width(ui.available_width() - 36.0),
                    );
                    let enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.add_enabled(chat.pending.is_none(), egui::Button::new(icon::PAPER_PLANE_RIGHT)).clicked() || enter)
                        && !chat.input.trim().is_empty()
                    {
                        action = Some(Action::Send);
                        input.request_focus();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{} placed", creations.placed.len())).small().weak());
                    if ui.add_enabled(!creations.placed.is_empty(), egui::Button::new("Undo last").small()).clicked() {
                        action = Some(Action::Undo);
                    }
                });
                ui.add_space(4.0);
            });

            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for m in &chat.messages {
                        let (who, color) = match m.role {
                            Role::Player => ("You", egui::Color32::from_rgb(140, 200, 255)),
                            Role::Agent(name) => (name, egui::Color32::from_rgb(255, 196, 140)),
                            Role::System => ("·", egui::Color32::GRAY),
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(who).color(color).strong());
                            ui.label(&m.text);
                        });
                        ui.add_space(6.0);
                    }
                });
        });

    match action {
        Some(Action::Send) => chat.send(&mut creations, &mut commands, tools.as_deref()),
        Some(Action::Stop) => {
            if let Some(p) = &chat.pending
                && let Ok(job) = p.job.lock()
            {
                job.cancel();
            }
        }
        Some(Action::Place) => {
            let transform = creations.draft.as_ref().and_then(|d| transforms.get(d.entity).ok()).copied();
            if let Some(transform) = transform
                && let Some(name) = creations.place_draft(&mut commands, &transform)
            {
                chat.say(Role::System, format!("Placed \"{name}\"."));
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
            if let Some(name) = creations.delete_selected(&mut commands) {
                chat.say(Role::System, format!("Deleted \"{name}\"."));
            }
        }
        Some(Action::Deselect) => creations.selected = None,
        Some(Action::Undo) => {
            if let Some(name) = creations.undo(&mut commands) {
                chat.say(Role::System, format!("Removed \"{name}\"."));
            }
        }
        None => {}
    }
    Ok(())
}
