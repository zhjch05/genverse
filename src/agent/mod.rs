//! Builder chat (desktop only): ask a local Claude Code or Codex CLI to design a voxel
//! object, preview it in front of you, then place, move, or discard it.
//!
//! Flow: chat message -> [`runner`] runs the CLI headless with the build schema ->
//! [`spec::compile`] validates and rasterizes the answer (asking the agent to fix it once
//! if it breaks a limit) -> a draft appears in the world -> Place saves it.

mod creations;
mod prompt;
mod runner;
mod spec;

use std::sync::Mutex;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPlugin, EguiPrimaryContextPass, egui, input::EguiWantsInput};

use crate::{
    player::{InputBlocked, Player},
    world::VoxelWorld,
};
use creations::{Creations, anchor_in_front, load_creations, outline_draft};
use runner::{AgentEvent, AgentJob, Provider};
use spec::compile;

const HISTORY: usize = 8;

pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin::default())
            .init_resource::<Chat>()
            .init_resource::<Creations>()
            .add_systems(Startup, load_creations)
            .add_systems(Update, (block_game_input, poll_agent, outline_draft, auto_prompt))
            .add_systems(EguiPrimaryContextPass, chat_panel);
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
                       It appears in front of you as a draft."
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

    fn start(&mut self, provider: Provider, prompt: String, request: String, repairs: u8) {
        let job = runner::start(provider, prompt::system(), request);
        self.status = format!("{} is working…", provider.label());
        self.pending = Some(Pending { job: Mutex::new(job), provider, prompt, repairs });
    }

    fn send(&mut self, creations: &Creations) {
        let text = self.input.trim().to_string();
        if text.is_empty() || self.pending.is_some() {
            return;
        }
        self.input.clear();
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
        self.start(self.provider, text, request, 0);
    }
}

/// `CUBELARK_PROMPT="a red mailbox" cargo run` sends one message once the hero has
/// landed. Pair with `CUBELARK_SHOT` to script a build end to end.
fn auto_prompt(mut sent: Local<bool>, mut chat: ResMut<Chat>, creations: Res<Creations>, players: Query<&Player>) {
    if *sent || !players.single().is_ok_and(|p| p.has_landed()) {
        return;
    }
    *sent = true;
    if let Ok(text) = std::env::var("CUBELARK_PROMPT") {
        if let Ok(provider) = std::env::var("CUBELARK_AGENT") {
            chat.provider = if provider == "codex" { Provider::Codex } else { Provider::Claude };
        }
        chat.input = text;
        chat.send(&creations);
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
    world: Res<VoxelWorld>,
    players: Query<&Transform, With<Player>>,
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

    let answer = match result {
        Ok(answer) => answer,
        Err(e) => {
            warn!("{} failed: {e}", pending.provider.label());
            chat.say(Role::System, format!("⚠ {} failed: {e}", pending.provider.label()));
            return;
        }
    };
    info!("{} replied: {}", pending.provider.label(), answer.reply);
    if !answer.reply.trim().is_empty() {
        chat.say(Role::Agent(pending.provider.label()), answer.reply.trim().to_string());
    }
    let Some(spec) = answer.build else {
        return;
    };
    match compile(&spec) {
        Ok(build) => {
            let Ok(player) = players.single() else {
                return;
            };
            let at = anchor_in_front(player, &world, build.stats.size);
            let s = build.stats;
            info!("draft {:?}: {:?}", spec.name, s);
            chat.say(
                Role::System,
                format!(
                    "Draft \"{}\": {}×{}×{} voxels, {} solid, {} faces. Place it, or ask for changes.",
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
            chat.start(pending.provider, pending.prompt, request, pending.repairs + 1);
        }
        Err(errors) => {
            warn!("build rejected again: {errors:?}");
            chat.say(Role::System, format!("⚠ Build rejected:\n• {}", errors.join("\n• ")));
        }
    }
}

enum Action {
    Send,
    Stop,
    Place,
    MoveHere,
    Discard,
    Undo,
}

fn chat_panel(
    mut contexts: EguiContexts,
    keys: Res<ButtonInput<KeyCode>>,
    egui_input: Res<EguiWantsInput>,
    mut chat: ResMut<Chat>,
    mut creations: ResMut<Creations>,
    mut commands: Commands,
    world: Res<VoxelWorld>,
    players: Query<&Transform, With<Player>>,
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
                            if ui.button("✔ Place").clicked() {
                                action = Some(Action::Place);
                            }
                            if ui.button("Move here").clicked() {
                                action = Some(Action::MoveHere);
                            }
                            if ui.button("✖ Discard").clicked() {
                                action = Some(Action::Discard);
                            }
                        });
                    });
                }
                if chat.pending.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&chat.status);
                        if ui.small_button("Stop").clicked() {
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
                            .desired_width(ui.available_width() - 56.0),
                    );
                    let enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.add_enabled(chat.pending.is_none(), egui::Button::new("Send")).clicked() || enter)
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
        Some(Action::Send) => {
            let creations = &*creations;
            chat.send(creations);
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
            if let Some(transform) = transform
                && let Some(name) = creations.place_draft(&mut commands, &transform)
            {
                chat.say(Role::System, format!("Placed \"{name}\"."));
            }
        }
        Some(Action::MoveHere) => {
            if let (Some(draft), Ok(player)) = (&creations.draft, players.single())
                && let Ok(mut transform) = transforms.get_mut(draft.entity)
            {
                let (position, yaw) = anchor_in_front(player, &world, draft.stats.size);
                transform.translation = position;
                transform.rotation = Quat::from_rotation_y(yaw);
            }
        }
        Some(Action::Discard) => creations.discard_draft(&mut commands),
        Some(Action::Undo) => {
            if let Some(name) = creations.undo(&mut commands) {
                chat.say(Role::System, format!("Removed \"{name}\"."));
            }
        }
        None => {}
    }
    Ok(())
}
