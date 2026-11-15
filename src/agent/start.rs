//! The start screen: the first thing every launch shows. The world you were last in
//! loads behind it while the camera slowly circles your hero, and the menu on the left
//! goes on from there: continue (or, the first time, get started), other worlds, play
//! together, settings, quit.
//!
//! Runs driven by `GENVERSE_*` variables skip it; `GENVERSE_START=1` shows it anyway.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use egui_phosphor::regular as icon;

use super::{
    Chat,
    loading::Loading,
    menu, prefs, presence,
    settings::SettingsWindow,
    theme::{self, Tone},
    welcome::Welcome,
    worlds::{self, WorldsWindow},
};
use crate::{player::Showcase, world::ledger::Ledger};

const COLUMN: f32 = 320.0;

#[derive(Resource, Default)]
pub struct StartScreen {
    pub open: bool,
    /// The builder panel was open before the screen hid it.
    builder_was_open: bool,
    /// Quit was pressed once; the second press quits.
    quitting: bool,
}

impl StartScreen {
    /// Leaves for the world, the builder as it was.
    fn close(&mut self, chat: &mut Chat) {
        self.open = false;
        chat.open = self.builder_was_open;
    }
}

/// Whether this run is driven by variables, which a screen to click through would stall.
pub fn scripted() -> bool {
    [
        "GENVERSE_SHOT",
        "GENVERSE_PROMPT",
        "GENVERSE_PLAY",
        "GENVERSE_AS",
        "GENVERSE_MENU",
        "GENVERSE_SERVER",
        "GENVERSE_WORLDS",
        "GENVERSE_WORLD",
        "GENVERSE_GAME",
    ]
    .iter()
    .any(|v| std::env::var_os(v).is_some())
}

/// Whether the player has been through the welcome yet.
pub fn welcomed() -> bool {
    prefs::get("welcomed").is_some_and(|v| v.as_bool() == Some(true))
}

/// Opens the screen at launch, unless the run is scripted.
pub fn open_at_launch(mut start: ResMut<StartScreen>, mut chat: ResMut<Chat>) {
    if scripted() && std::env::var_os("GENVERSE_START").is_none() {
        return;
    }
    start.open = true;
    start.builder_was_open = chat.open;
    chat.open = false;
}

/// The camera circles the hero, and offline the games wait, while the screen is up.
pub fn follow_start(start: Res<StartScreen>, mut showcase: ResMut<Showcase>) {
    if showcase.0 != start.open {
        showcase.0 = start.open;
    }
}

enum Pick {
    Continue,
    GetStarted,
    Worlds,
    PlayTogether,
    Settings,
    Quit,
}

pub fn start_screen(
    mut contexts: EguiContexts,
    mut start: ResMut<StartScreen>,
    loading: Res<Loading>,
    ledger: Res<Ledger>,
    mut chat: ResMut<Chat>,
    mut welcome: ResMut<Welcome>,
    (mut settings, mut worlds_window): (ResMut<SettingsWindow>, ResMut<WorldsWindow>),
    mut exit: MessageWriter<AppExit>,
) -> Result {
    if !start.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let screen = ctx.viewport_rect();
    let new_player = !welcomed();
    let world = worlds::world_of(&ledger);
    let world = if world.is_empty() {
        worlds::game_of(&ledger)
    } else {
        world
    };
    let ready = loading.is_done();
    let mut pick = None;

    egui::Area::new("start_screen".into())
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            // Takes the clicks and scrolls meant for the world behind it.
            let _ = ui.allocate_exact_size(screen.size(), egui::Sense::click_and_drag());
            // The menu sits on a sidebar of its own, so it reads on any scene; the world
            // fills the rest.
            let margin = if screen.width() < 700.0 { 24.0 } else { 64.0 };
            let held = COLUMN + 2.0 * margin;
            let painter = ui.painter();
            let side =
                egui::Rect::from_x_y_ranges(screen.left()..=screen.left() + held, screen.y_range());
            painter.rect_filled(side, 0, p.panel);
            painter.vline(
                side.right(),
                screen.y_range(),
                egui::Stroke::new(1.0, p.line),
            );

            let height = ui
                .ctx()
                .data(|d| d.get_temp::<f32>(egui::Id::new("start_height")))
                .unwrap_or(460.0);
            let top = screen.top() + ((screen.height() - height) / 2.0).max(24.0);
            let column = egui::Rect::from_min_size(
                egui::pos2(screen.left() + margin, top),
                egui::vec2(COLUMN.min(screen.width() - 2.0 * margin), screen.height()),
            );
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(column));
            let inner = child.vertical(|ui| {
                ui.set_width(column.width());
                ui.label(
                    egui::RichText::new("Genverse")
                        .size(52.0)
                        .strong()
                        .color(p.strong),
                );
                ui.label(
                    egui::RichText::new("Build a world by describing it.")
                        .size(15.0)
                        .color(p.muted),
                );
                ui.add_space(28.0);

                if new_player {
                    if menu::entry(ui, icon::HAND_WAVING, "Get started", "Enter", Tone::Primary)
                        .clicked()
                    {
                        pick = Some(Pick::GetStarted);
                    }
                } else if ready {
                    if menu::entry(
                        ui,
                        icon::PLAY,
                        &format!("Continue · {world}"),
                        "Enter",
                        Tone::Primary,
                    )
                    .clicked()
                    {
                        pick = Some(Pick::Continue);
                    }
                } else {
                    let label = format!("Loading {world}… {:.0}%", loading.progress() * 100.0);
                    let _ = menu::entry(ui, icon::HOURGLASS_MEDIUM, &label, "", Tone::Primary);
                }
                ui.add_space(8.0);
                if menu::entry(ui, icon::GLOBE_HEMISPHERE_WEST, "Worlds", "", Tone::Plain).clicked()
                {
                    pick = Some(Pick::Worlds);
                }
                if menu::entry(ui, icon::USERS_THREE, "Play together", "", Tone::Plain).clicked() {
                    pick = Some(Pick::PlayTogether);
                }
                if menu::entry(ui, icon::GAUGE, "Settings", "", Tone::Plain).clicked() {
                    pick = Some(Pick::Settings);
                }
                ui.add_space(10.0);
                let quit = if start.quitting {
                    "Click again to quit"
                } else {
                    "Quit"
                };
                if menu::entry(ui, icon::SIGN_OUT, quit, "", Tone::Plain).clicked() {
                    pick = Some(Pick::Quit);
                }
            });
            let height = inner.response.rect.height();
            ui.ctx()
                .data_mut(|d| d.insert_temp(egui::Id::new("start_height"), height));

            // Who you are, and which version, along the bottom.
            let foot = egui::Rect::from_min_max(
                egui::pos2(screen.left() + margin, screen.bottom() - 56.0),
                egui::pos2(screen.left() + held - margin, screen.bottom() - 16.0),
            );
            let mut foot_ui = ui.new_child(egui::UiBuilder::new().max_rect(foot));
            foot_ui.horizontal(|ui| {
                if !new_player {
                    let mine =
                        theme::color(presence::marker_color(chat.marker(&ledger.profile.id)));
                    theme::avatar(ui, &ledger.profile.name, mine, 28.0);
                    ui.label(egui::RichText::new(&ledger.profile.name).color(p.text));
                    ui.add_space(10.0);
                }
                ui.label(
                    egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                        .size(11.5)
                        .color(p.muted),
                );
            });
        });

    // Enter takes the primary action, once there's one to take.
    if pick.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
        if new_player {
            pick = Some(Pick::GetStarted);
        } else if ready {
            pick = Some(Pick::Continue);
        }
    }
    let Some(pick) = pick else { return Ok(()) };
    if !matches!(pick, Pick::Quit) {
        start.quitting = false;
    }
    match pick {
        Pick::Continue => start.close(&mut chat),
        Pick::GetStarted => {
            start.close(&mut chat);
            welcome.show(&ledger);
        }
        Pick::Worlds => {
            start.close(&mut chat);
            worlds_window.open = true;
            worlds_window.refresh();
        }
        Pick::PlayTogether => {
            // The builder's footer is where joining and hosting happen.
            start.close(&mut chat);
            chat.open = true;
            chat.network = true;
            chat.name = None;
        }
        Pick::Settings => {
            start.close(&mut chat);
            settings.open = true;
        }
        Pick::Quit if start.quitting => {
            exit.write(AppExit::Success);
        }
        Pick::Quit => start.quitting = true,
    }
    Ok(())
}
