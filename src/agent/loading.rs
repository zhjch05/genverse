//! The loading screen: covers the world while the chunks around the hero generate,
//! with a progress bar, then fades out once the hero stands on the ground and frames
//! have settled. Frames while it shows are hitchy by nature (every core is building
//! chunks, and the GPU is taking their meshes and preparing its pipelines), so the
//! frame rate overlay leaves them out of its numbers.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};

use super::theme;
use crate::{player::Player, voxel::Meshing, world::VoxelWorld};

/// How long the screen takes to fade out, in seconds.
const FADE: f32 = 0.4;
/// Frames in a row at or under [`STEADY_MS`] that count as settled.
const STEADY_FRAMES: u32 = 30;
const STEADY_MS: f32 = 25.0;
/// The longest to wait for frames to settle once the world is in, in seconds.
const SETTLE_LIMIT: f32 = 3.0;

#[derive(Resource, Default)]
pub struct Loading {
    /// Chunks within view that are loaded, out of how many there are.
    chunks: (usize, usize),
    /// When the chunks were in and the hero had landed, in seconds since startup.
    settling_since: Option<f32>,
    /// Smooth frames in a row since then.
    steady: u32,
    /// When the world was ready, in seconds since startup; it stays ready from then on.
    ready_at: Option<f32>,
}

impl Loading {
    pub fn is_done(&self) -> bool {
        self.ready_at.is_some()
    }
}

pub fn track_loading(
    time: Res<Time<Real>>,
    world: Res<VoxelWorld>,
    player: Query<(&Player, &Transform)>,
    meshing: Query<(), With<Meshing>>,
    mut loading: ResMut<Loading>,
) {
    if loading.is_done() {
        return;
    }
    let Ok((player, transform)) = player.single() else {
        return;
    };
    let now = time.elapsed_secs();
    loading.chunks = world.view_loaded(transform.translation);
    let (loaded, total) = loading.chunks;
    // Builds in view (and the hero) mesh in the background too.
    if loaded < total || !player.has_landed() || !meshing.is_empty() {
        return;
    }
    let since = *loading.settling_since.get_or_insert(now);
    loading.steady = if time.delta_secs() * 1000.0 <= STEADY_MS { loading.steady + 1 } else { 0 };
    if loading.steady >= STEADY_FRAMES || now - since >= SETTLE_LIMIT {
        loading.ready_at = Some(now);
    }
}

pub fn loading_screen(mut contexts: EguiContexts, time: Res<Time<Real>>, loading: Res<Loading>) -> Result {
    let shown = loading.ready_at.map_or(1.0, |at| 1.0 - (time.elapsed_secs() - at) / FADE);
    if shown <= 0.0 {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let (loaded, total) = loading.chunks;
    let progress = if total == 0 { 0.0 } else { loaded as f32 / total as f32 };
    let screen = ctx.content_rect();
    egui::Area::new("loading_screen".into())
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.set_opacity(shown);
            // Takes the clicks too, so nothing behind it reacts while it's up.
            let (rect, _) = ui.allocate_exact_size(screen.size(), egui::Sense::click_and_drag());
            ui.painter().rect_filled(rect, 0, p.panel);
            let card = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width().min(360.0) - 32.0, 120.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Cubewish").size(30.0).strong().color(p.strong));
                    ui.add_space(4.0);
                    let status = if loaded < total {
                        "Growing the world"
                    } else if loading.settling_since.is_none() {
                        "Finding your feet"
                    } else {
                        "Almost there"
                    };
                    ui.label(egui::RichText::new(status).color(p.muted));
                    ui.add_space(14.0);
                    progress_bar(ui, progress);
                    ui.add_space(6.0);
                    theme::note(ui, format!("{loaded} / {total} chunks"));
                });
            });
        });
    Ok(())
}

/// A trough with a grass block filling it: lit on top, a darker lip along the bottom.
fn progress_bar(ui: &mut egui::Ui, progress: f32) {
    let p = theme::palette(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 16.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, theme::ROUND, p.input);
    painter.rect_stroke(rect, theme::ROUND, egui::Stroke::new(1.0, p.line), egui::StrokeKind::Inside);
    if progress <= 0.0 {
        return;
    }
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * progress.min(1.0), rect.height()));
    painter.rect_filled(fill, theme::ROUND, theme::shade(p.primary, 0.72));
    let face = egui::Rect::from_min_max(fill.min, egui::pos2(fill.right(), fill.bottom() - 3.0));
    painter.rect_filled(face, theme::ROUND, p.primary);
}
