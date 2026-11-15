//! The frame rate overlay (F3): frames per second, how long frames take, a graph of
//! the last few seconds, and what the world is doing (chunks loaded and building,
//! entities, where you are), the graphics settings, and where the mouse and keys go
//! (the game, or the interface and why). The choice is kept in saves/settings.json.

use std::collections::VecDeque;

use bevy::{prelude::*, window::PrimaryWindow};
use bevy_egui::{EguiContexts, egui, input::EguiWantsInput};

use super::{loading::Loading, prefs, theme};
use crate::{
    graphics::{AutoScale, Graphics},
    player::{InputBlocked, Player, PointerOverUi},
    world::VoxelWorld,
};

/// How many frames the graph and the lows cover.
const FRAMES: usize = 240;
/// How often the numbers update, so they can be read.
const REFRESH: f32 = 0.25;
/// Frame times at or under this are on target (60 fps); twice it is slow.
const TARGET_MS: f32 = 1000.0 / 60.0;

#[derive(Resource)]
pub struct Fps {
    pub open: bool,
    /// The most recent frame times, in milliseconds.
    frames: VecDeque<f32>,
    /// The numbers on show, held between refreshes.
    shown: Stats,
    since: f32,
}

impl Default for Fps {
    fn default() -> Self {
        Self {
            open: prefs::get("fps").and_then(|v| v.as_bool()).unwrap_or(false),
            frames: VecDeque::with_capacity(FRAMES),
            shown: Stats::default(),
            since: 0.0,
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Stats {
    fps: f32,
    ms: f32,
    /// The average of the slowest 1% of frames, as fps.
    low: f32,
    worst_ms: f32,
}

impl Fps {
    fn stats(&self) -> Stats {
        if self.frames.is_empty() {
            return Stats::default();
        }
        // Average over the last half second or so, not one jittery frame.
        let recent: Vec<f32> = self.frames.iter().rev().take(30).copied().collect();
        let ms = recent.iter().sum::<f32>() / recent.len() as f32;
        let mut sorted: Vec<f32> = self.frames.iter().copied().collect();
        sorted.sort_by(|a, b| b.total_cmp(a));
        let slowest = &sorted[..sorted.len().div_ceil(100)];
        let low_ms = slowest.iter().sum::<f32>() / slowest.len() as f32;
        Stats {
            fps: 1000.0 / ms,
            ms,
            low: 1000.0 / low_ms,
            worst_ms: sorted[0],
        }
    }
}

/// F3 shows and hides it, like Minecraft's debug screen; it measures either way, from
/// the end of the loading screen.
pub fn track_frames(
    binds: Res<crate::bindings::Bindings>,
    blocked: Res<InputBlocked>,
    time: Res<Time<Real>>,
    loading: Res<Loading>,
    mut fps: ResMut<Fps>,
) {
    if !blocked.0 && binds.just_pressed("stats") {
        fps.open = !fps.open;
        prefs::set("fps", fps.open);
    }
    let ms = time.delta_secs() * 1000.0;
    if ms <= 0.0 || !loading.is_done() {
        return;
    }
    if fps.frames.len() == FRAMES {
        fps.frames.pop_front();
    }
    fps.frames.push_back(ms);
    fps.since += time.delta_secs();
    if fps.since >= REFRESH {
        fps.since = 0.0;
        fps.shown = fps.stats();
    }
}

pub fn fps_overlay(
    mut contexts: EguiContexts,
    fps: Res<Fps>,
    graphics: Res<Graphics>,
    auto: Res<AutoScale>,
    world: Res<VoxelWorld>,
    entities: Query<Entity>,
    player: Query<&Transform, With<Player>>,
    (egui_input, over_ui, buttons, windows): (
        Res<EguiWantsInput>,
        Res<PointerOverUi>,
        Res<ButtonInput<MouseButton>>,
        Query<&Window, With<PrimaryWindow>>,
    ),
) -> Result {
    if !fps.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let s = fps.shown;
    // Green on target, amber below it, red under half of it.
    let tone = |ms: f32| {
        if ms <= TARGET_MS * 1.1 {
            p.online
        } else if ms <= TARGET_MS * 2.0 {
            p.warn
        } else {
            p.danger
        }
    };
    let (loaded, building) = world.chunk_counts();
    egui::Area::new("fps_overlay".into())
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.overlay)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(theme::ROUND + 1)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 3.0);
                    ui.horizontal(|ui| {
                        let big = egui::RichText::new(format!("{:.0}", s.fps))
                            .size(22.0)
                            .strong()
                            .monospace();
                        ui.label(big.color(tone(s.ms)));
                        ui.label(egui::RichText::new("fps").size(12.0).color(p.muted));
                    });
                    let row =
                        |ui: &mut egui::Ui, name: &str, value: String, color: egui::Color32| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(name).size(11.5).color(p.muted));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            egui::RichText::new(value)
                                                .size(11.5)
                                                .monospace()
                                                .color(color),
                                        );
                                    },
                                );
                            });
                        };
                    ui.set_width(170.0);
                    row(ui, "frame", format!("{:.1} ms", s.ms), p.text);
                    row(
                        ui,
                        "1% low",
                        format!("{:.0} fps", s.low),
                        tone(1000.0 / s.low.max(1.0)),
                    );
                    row(
                        ui,
                        "worst",
                        format!("{:.1} ms", s.worst_ms),
                        tone(s.worst_ms),
                    );
                    graph(ui, &fps.frames, tone);
                    let preset = graphics.preset().map_or("Custom", |q| q.label());
                    let auto_note = if graphics.auto_scale { " auto" } else { "" };
                    row(
                        ui,
                        "graphics",
                        format!("{preset} · {:.0}%{auto_note}", auto.scale * 100.0),
                        p.text,
                    );
                    row(ui, "chunks", format!("{loaded} (+{building})"), p.text);
                    row(ui, "entities", entities.iter().len().to_string(), p.text);
                    if let Ok(t) = player.single() {
                        let at = t.translation;
                        row(
                            ui,
                            "at",
                            format!("{:.0} {:.0} {:.0}", at.x, at.y, at.z),
                            p.text,
                        );
                    }
                    let cursor = windows.single().ok().and_then(|w| w.cursor_position());
                    row(
                        ui,
                        "cursor",
                        cursor.map_or("outside".into(), |c| format!("{:.0} {:.0}", c.x, c.y)),
                        p.text,
                    );
                    // Who gets the mouse and the keys: the game, or the interface (and why).
                    let mouse = if egui_input.is_popup_open() {
                        "UI · menu open"
                    } else if egui_input.is_using_pointer() {
                        "UI · dragging"
                    } else if egui_input.is_pointer_over_area() || egui_input.wants_pointer_input()
                    {
                        "UI · over it"
                    } else if over_ui.0 {
                        "UI · panel"
                    } else {
                        "game"
                    };
                    row(
                        ui,
                        "mouse",
                        mouse.into(),
                        if mouse == "game" { p.text } else { p.warn },
                    );
                    let held: Vec<_> = [
                        (MouseButton::Left, "left"),
                        (MouseButton::Right, "right"),
                        (MouseButton::Middle, "middle"),
                    ]
                    .into_iter()
                    .filter(|(b, _)| buttons.pressed(*b))
                    .map(|(_, name)| name)
                    .collect();
                    row(
                        ui,
                        "held",
                        if held.is_empty() {
                            "–".into()
                        } else {
                            held.join(" ")
                        },
                        p.text,
                    );
                    let keys = if egui_input.wants_keyboard_input() {
                        "UI · typing"
                    } else if egui_input.is_popup_open() {
                        "UI · menu open"
                    } else {
                        "game"
                    };
                    row(
                        ui,
                        "keys",
                        keys.into(),
                        if keys == "game" { p.text } else { p.warn },
                    );
                });
        });
    Ok(())
}

/// One bar per frame, newest on the right, taller for slower frames; the line marks 60 fps.
fn graph(ui: &mut egui::Ui, frames: &VecDeque<f32>, tone: impl Fn(f32) -> egui::Color32) {
    let p = theme::palette(ui.ctx());
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, theme::ROUND, p.input);
    // Room for frames up to 30 fps; slower ones are clipped at the top.
    let scale = rect.height() / (TARGET_MS * 2.0);
    let width = rect.width() / FRAMES as f32;
    let start = FRAMES - frames.len();
    for (i, &ms) in frames.iter().enumerate() {
        let x = rect.left() + (start + i) as f32 * width;
        let top = (rect.bottom() - ms * scale).max(rect.top());
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x, top), egui::pos2(x + width, rect.bottom())),
            0,
            tone(ms),
        );
    }
    let y = rect.bottom() - TARGET_MS * scale;
    painter.hline(rect.x_range(), y, egui::Stroke::new(1.0, p.faint));
}
