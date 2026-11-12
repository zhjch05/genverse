//! The game menu Esc opens: resume, the app's windows, play together, the theme, quit.
//!
//! Esc means "back out of what you're in", one step at a time:
//! 1. whatever is in progress cancels (a draft, a selection, a board, the inventory),
//!    each where it's handled;
//! 2. then open windows close, back to this menu if they were opened from it;
//! 3. then the menu opens, and Esc again resumes.
//!
//! The world doesn't stop while it's open: it may be shared, and games keep time. The
//! hero just stops listening to the keys. The world is a log written as it changes, so
//! there's nothing to save before quitting.
//!
//! Drawn in Rust, not as a [panel](super::panels): it opens windows that aren't ops
//! (settings, history), and lies over everything else.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui, input::EguiWantsInput};
use egui_phosphor::regular as icon;

use super::{
    Chat, Role,
    creations::Creations,
    game::Play,
    history::History,
    inventory::Inventory,
    landscape::Landscape,
    panels::Panels,
    presence,
    settings::SettingsWindow,
    theme::{self, Tone},
    worlds::{self, WorldsWindow},
};
use crate::{bindings::Bindings, world::ledger::Ledger};

#[derive(Resource, Default)]
pub struct GameMenu {
    pub open: bool,
    /// A window the menu opened is open: Esc goes back to the menu.
    back_to_menu: bool,
    /// Quit was pressed once; the second press quits.
    quitting: bool,
}

/// Esc, once nothing in progress takes it. Runs before the systems that cancel those,
/// so it sees the frame as it was when the key went down.
pub fn toggle_menu(
    keys: Res<ButtonInput<KeyCode>>,
    egui_input: Res<EguiWantsInput>,
    binds: Res<Bindings>,
    mut menu: ResMut<GameMenu>,
    creations: Res<Creations>,
    landscape: Res<Landscape>,
    inventory: Res<Inventory>,
    play: Res<Play>,
    (mut settings, mut worlds_window, mut history, mut panels): (ResMut<SettingsWindow>, ResMut<WorldsWindow>, ResMut<History>, ResMut<Panels>),
    mut started: Local<bool>,
) {
    // `GENVERSE_MENU=1` opens it at start.
    if !*started {
        *started = true;
        menu.open = std::env::var("GENVERSE_MENU").is_ok();
    }
    let windows = settings.open || worlds_window.open || history.open || panels.any_open();
    // Closed with their own ✕: Esc no longer leads back.
    if !windows && !menu.open {
        menu.back_to_menu = false;
    }
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    if menu.open {
        menu.open = false;
        menu.quitting = false;
        return;
    }
    // Typing, or waiting for a key to bind: Esc is theirs.
    if egui_input.wants_keyboard_input() || binds.capturing {
        return;
    }
    let busy = creations.draft.is_some()
        || creations.selected.is_some()
        || landscape.draft.is_some()
        || landscape.selecting
        || landscape.area.is_some()
        || inventory.open
        || play.playing.is_some();
    if busy {
        return;
    }
    if windows {
        settings.open = false;
        worlds_window.open = false;
        history.open = false;
        panels.close_all();
        menu.open = std::mem::take(&mut menu.back_to_menu);
        return;
    }
    menu.open = true;
}

/// While the menu is up the world dims, and offline its games wait for you.
pub fn follow_menu(menu: Res<GameMenu>, mut dim: ResMut<crate::graphics::Dim>, mut ledger: ResMut<Ledger>) {
    // Linear light: 0.15 shows at about 40%.
    let want = if menu.open { 0.15 } else { 1.0 };
    if dim.0 != want {
        dim.0 = want;
    }
    let hold = menu.open && !ledger.is_online();
    if ledger.hold != hold {
        ledger.hold = hold;
    }
}

enum Pick {
    Resume,
    Settings,
    Controls,
    Worlds,
    History,
    PlayTogether,
    PauseWorld,
    Theme,
    Quit,
}

/// The menu: the world dimmed behind it, a card in the middle of the view.
pub fn menu_window(
    mut contexts: EguiContexts,
    mut menu: ResMut<GameMenu>,
    binds: Res<Bindings>,
    ledger: Res<Ledger>,
    mut chat: ResMut<Chat>,
    (mut settings, mut worlds_window, mut history, mut panels): (ResMut<SettingsWindow>, ResMut<WorldsWindow>, ResMut<History>, ResMut<Panels>),
    mut exit: MessageWriter<AppExit>,
) -> Result {
    if !menu.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let screen = ctx.viewport_rect();
    let (world, status, online) = place(&ledger);
    let me = ledger.profile.name.clone();
    let mine = theme::color(presence::marker_color(chat.marker(&ledger.profile.id)));
    let dark = ctx.theme() == egui::Theme::Dark;
    let admin = ledger.land.is_admin(&ledger.profile.id);
    let world_paused = ledger.rules.logic.paused;
    let badge = match (ledger.is_online(), world_paused) {
        (_, true) => ("WORLD PAUSED", "An admin paused the games for everyone"),
        (false, false) => ("PAUSED", "Games wait while you're in the menu"),
        (true, false) => ("LIVE", "Others play on: only your hero waits"),
    };
    let mut pick = None;

    egui::Area::new("game_menu".into())
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            // The backdrop takes every click and scroll meant for the world behind it; the
            // world dims under it (`Dim`).
            let (_, _) = ui.allocate_exact_size(screen.size(), egui::Sense::click_and_drag());

            let card = egui::Rect::from_center_size(screen.center(), egui::vec2(MENU_WIDTH, 0.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::pos2(card.left(), screen.top()), egui::vec2(MENU_WIDTH, screen.height()))));
            child.with_layout(egui::Layout::top_down(egui::Align::Center).with_main_justify(false), |ui| {
                // Centered by height once the card's height is known (from the last frame).
                let height = ui.ctx().data(|d| d.get_temp::<f32>(egui::Id::new("game_menu_height"))).unwrap_or(420.0);
                ui.add_space(((screen.height() - height) / 2.0).max(16.0));
                let inner = egui::Frame::new()
                    .fill(p.panel)
                    .stroke(egui::Stroke::new(1.0, p.line))
                    .corner_radius(theme::ROUND + 2)
                    .inner_margin(egui::Margin::same(18))
                    .shadow(egui::Shadow { offset: [0, 10], blur: 28, spread: 0, color: egui::Color32::from_black_alpha(90) })
                    .show(ui, |ui| {
                        ui.set_width(MENU_WIDTH - 36.0);
                        header(ui, &world, &status, online, &me, mine, badge);
                        ui.add_space(14.0);
                        let item = |ui: &mut egui::Ui, glyph: &str, label: &str, key: &str, tone: Tone| entry(ui, glyph, label, key, tone).clicked();
                        let resume = entry(ui, icon::PLAY, "Resume", "Esc", Tone::Primary);
                        if resume.clicked() {
                            pick = Some(Pick::Resume);
                        }
                        ui.add_space(8.0);
                        theme::section(ui, "Game");
                        if item(ui, icon::GLOBE_HEMISPHERE_WEST, "Worlds and saves", binds.short("game_window"), Tone::Plain) {
                            pick = Some(Pick::Worlds);
                        }
                        if item(ui, icon::CLOCK_COUNTER_CLOCKWISE, "History", binds.short("history"), Tone::Plain) {
                            pick = Some(Pick::History);
                        }
                        if item(ui, icon::USERS_THREE, "Play together", "", Tone::Plain) {
                            pick = Some(Pick::PlayTogether);
                        }
                        // Admins pause everyone's games; offline the menu pauses them anyway.
                        if admin && (online || world_paused) {
                            let (glyph, label) = if world_paused { (icon::PLAY_CIRCLE, "Resume world") } else { (icon::PAUSE_CIRCLE, "Pause world for everyone") };
                            if item(ui, glyph, label, "", Tone::Plain) {
                                pick = Some(Pick::PauseWorld);
                            }
                        }
                        ui.add_space(8.0);
                        theme::section(ui, "Options");
                        if item(ui, icon::GAUGE, "Settings", "", Tone::Plain) {
                            pick = Some(Pick::Settings);
                        }
                        if item(ui, icon::KEYBOARD, "Controls", "", Tone::Plain) {
                            pick = Some(Pick::Controls);
                        }
                        let (glyph, label) = if dark { (icon::SUN, "Light theme") } else { (icon::MOON, "Dark theme") };
                        if item(ui, glyph, label, "", Tone::Plain) {
                            pick = Some(Pick::Theme);
                        }
                        if cfg!(not(target_arch = "wasm32")) {
                            ui.add_space(10.0);
                            let label = if menu.quitting { "Click again to quit" } else { "Quit to desktop" };
                            if item(ui, icon::SIGN_OUT, label, "", Tone::Danger) {
                                pick = Some(Pick::Quit);
                            }
                        }
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| theme::note(ui, "Your world is saved as it changes."));
                    });
                let height = inner.response.rect.height();
                ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("game_menu_height"), height));
            });
        });

    let Some(pick) = pick else { return Ok(()) };
    if !matches!(pick, Pick::Quit) {
        menu.quitting = false;
    }
    // A window opened from here: the menu steps aside, and Esc comes back to it.
    let opens = |menu: &mut GameMenu| {
        menu.open = false;
        menu.back_to_menu = true;
    };
    match pick {
        Pick::Resume => menu.open = false,
        Pick::Settings => {
            settings.open = true;
            opens(&mut menu);
        }
        Pick::Controls => match panels.open("controls") {
            Ok(_) => opens(&mut menu),
            Err(why) => chat.say(Role::System, why),
        },
        Pick::Worlds => {
            worlds_window.open = true;
            worlds_window.refresh();
            opens(&mut menu);
        }
        Pick::History => {
            history.open = true;
            opens(&mut menu);
        }
        Pick::PlayTogether => {
            // The builder's footer is where joining and hosting happen.
            chat.open = true;
            chat.network = true;
            chat.name = None;
            menu.open = false;
        }
        Pick::PauseWorld => panels.ask("pause", serde_json::json!({})),
        Pick::Theme => theme::toggle(ctx),
        Pick::Quit if menu.quitting => {
            exit.write(AppExit::Success);
        }
        Pick::Quit => menu.quitting = true,
    }
    Ok(())
}

const MENU_WIDTH: f32 = 320.0;

/// Where you are: the world, and whether it's yours alone, hosted or someone else's.
fn place(ledger: &Ledger) -> (String, String, bool) {
    let world = worlds::world_of(ledger);
    let game = worlds::game_of(ledger);
    let world = if world.is_empty() { game.clone() } else { world };
    match (&ledger.online, &ledger.hosting) {
        (None, _) if game == world || game == "local" => (world, "Local world".into(), false),
        (None, _) => (world, format!("Local world · {game}"), false),
        (Some(online), _) if online.closed.is_some() => (world, "Reconnecting…".into(), false),
        (Some(online), None) if online.state.is_none() => (world, "Joining…".into(), false),
        (Some(_), Some(server)) => (world, format!("Hosting on port {}", server.addr.port()), true),
        (Some(online), None) => (world, format!("Online at {}", crate::world::ledger::shown_addr(&online.addr)), true),
    }
}

fn header(ui: &mut egui::Ui, world: &str, status: &str, online: bool, me: &str, mine: egui::Color32, (badge, tip): (&str, &str)) {
    let p = theme::palette(ui.ctx());
    ui.horizontal(|ui| {
        theme::avatar(ui, me, mine, 40.0);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(egui::RichText::new(world).size(18.0).strong().color(p.strong));
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                ui.painter().rect_filled(dot.translate(egui::vec2(0.0, 1.0)), 1, if online { p.online } else { p.faint });
                ui.label(egui::RichText::new(status).size(11.5).color(p.muted));
            });
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            let color = if badge == "WORLD PAUSED" { p.warn } else { p.faint };
            ui.label(egui::RichText::new(badge).size(10.0).strong().color(color)).on_hover_text(tip);
        });
    });
}

/// A menu entry: a wide block with an icon, its label, and its key on the right.
fn entry(ui: &mut egui::Ui, glyph: &str, label: &str, key: &str, tone: Tone) -> egui::Response {
    let p = theme::palette(ui.ctx());
    let (fill, text) = match tone {
        Tone::Plain => (p.raised, p.text),
        Tone::Primary => (p.primary, p.on_primary),
        Tone::Danger => (p.danger, egui::Color32::WHITE),
    };
    let size = egui::vec2(ui.available_width(), if matches!(tone, Tone::Primary) { 42.0 } else { 36.0 });
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let pressed = response.is_pointer_button_down_on();
    let painter = ui.painter();
    // Pushed in: the face drops onto where the lip was.
    let face = if pressed { rect.translate(egui::vec2(0.0, 2.0)).with_max_y(rect.bottom()) } else { rect.with_max_y(rect.bottom() - 3.0) };
    if !pressed {
        painter.rect_filled(rect, theme::ROUND, theme::shade(fill, 0.72));
    }
    painter.rect_filled(face, theme::ROUND, fill);
    painter.rect_stroke(face, theme::ROUND, egui::Stroke::new(1.0, theme::shade(fill, 0.8)), egui::StrokeKind::Inside);
    if response.hovered() || response.has_focus() {
        let glow = if ui.visuals().dark_mode { egui::Color32::from_white_alpha(22) } else { egui::Color32::from_black_alpha(14) };
        painter.rect_filled(face, theme::ROUND, glow);
    }
    if response.has_focus() {
        painter.rect_stroke(rect.expand(2.0), theme::ROUND + 1, egui::Stroke::new(1.5, p.select), egui::StrokeKind::Outside);
    }
    let middle = face.left_center() + egui::vec2(14.0, 0.0);
    let size = if matches!(tone, Tone::Primary) { 15.5 } else { 14.0 };
    painter.text(middle, egui::Align2::LEFT_CENTER, glyph, egui::FontId::proportional(size + 3.0), text);
    painter.text(middle + egui::vec2(28.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(size), text);
    if !key.is_empty() && key != "unbound" {
        let galley = painter.layout_no_wrap(key.to_string(), egui::FontId::proportional(10.5), text);
        let cap = egui::Rect::from_center_size(face.right_center() - egui::vec2(14.0 + galley.size().x / 2.0 + 5.0, 0.0), galley.size() + egui::vec2(10.0, 4.0));
        painter.rect_filled(cap, theme::ROUND, theme::shade(fill, 0.82));
        painter.galley(cap.center() - galley.size() / 2.0, galley, text);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// While an admin has the world paused, a note at the top says so, for everyone.
pub fn paused_banner(mut contexts: EguiContexts, ledger: Res<Ledger>, menu: Res<GameMenu>, chat: Res<Chat>) -> Result {
    if !ledger.rules.logic.paused || menu.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let covered = chat.covered * ctx.viewport_rect().width();
    let admin = ledger.land.is_admin(&ledger.profile.id);
    egui::Area::new("paused_banner".into())
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(-covered / 2.0, 14.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.overlay)
                .stroke(egui::Stroke::new(1.0, p.warn))
                .corner_radius(theme::ROUND)
                .inner_margin(egui::Margin::symmetric(12, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon::PAUSE_CIRCLE).size(16.0).color(p.warn));
                        ui.label(egui::RichText::new("World paused").strong().color(p.strong));
                        let why = if admin { "games stand still · /pause or the Esc menu resumes" } else { "games stand still until an admin resumes them" };
                        ui.label(egui::RichText::new(why).size(12.0).color(p.muted));
                    });
                });
        });
    Ok(())
}
