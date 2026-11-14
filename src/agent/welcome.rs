//! The welcome a new player goes through from the start screen's "Get started", in
//! three steps: who you are (name and color), who builds for you (which agent CLI, and
//! whether it's installed), and the few keys that matter. The world keeps loading,
//! dimmed, behind it. The game menu's "Getting started" goes through it again.
//!
//! `GENVERSE_WELCOME=1` opens it at launch even in a scripted run (for screenshots);
//! `GENVERSE_WELCOME_STEP=2` starts it on a later step.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use egui_phosphor::regular as icon;

use super::{
    Chat, prefs, presence,
    loading::Loading,
    menu::GameMenu,
    runner::Provider,
    theme::{self, Tone},
};
use crate::world::ledger::Ledger;

const WIDTH: f32 = 440.0;
const STEPS: [&str; 3] = ["You", "Your builder", "The basics"];

/// What looking for an agent's CLI found.
#[derive(Clone)]
enum Found {
    Looking,
    Missing,
    /// Its path, and what `--version` printed.
    Installed(PathBuf, String),
}

#[derive(Resource, Default)]
pub struct Welcome {
    pub open: bool,
    step: usize,
    /// The name being typed.
    name: String,
    /// What was found for each of [`Provider::ALL`], filled in from a thread.
    found: Arc<Mutex<Vec<Found>>>,
}

impl Welcome {
    /// Opens it on the first step, looking for the agents again (one may have been
    /// installed since).
    pub fn show(&mut self, ledger: &Ledger) {
        self.open = true;
        self.step = 0;
        self.name = if ledger.profile.name == "Player" { String::new() } else { ledger.profile.name.clone() };
        self.check();
    }

    fn check(&mut self) {
        let found = Arc::new(Mutex::new(vec![Found::Looking; Provider::ALL.len()]));
        self.found = found.clone();
        std::thread::spawn(move || {
            for (i, provider) in Provider::ALL.into_iter().enumerate() {
                let result = look_for(provider);
                if let Ok(mut found) = found.lock() {
                    found[i] = result;
                }
            }
        });
    }

    fn found(&self) -> Vec<Found> {
        self.found.lock().map(|f| f.clone()).unwrap_or_default()
    }
}

fn command(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

fn install_page(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "https://code.claude.com/docs/en/setup",
        Provider::Codex => "https://github.com/openai/codex",
    }
}

/// Finds the CLI and asks its version, giving up on one that doesn't answer.
fn look_for(provider: Provider) -> Found {
    let Some(path) = crate::platform::find_program(command(provider)) else { return Found::Missing };
    let child = Command::new(&path).arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let Ok(mut child) = child else { return Found::Installed(path, String::new()) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
        let _ = child.wait();
        return Found::Installed(path, String::new());
    }
    let version = child.wait_with_output().map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().to_string()).unwrap_or_default();
    Found::Installed(path, version)
}

/// `GENVERSE_WELCOME=1` opens it at launch, for screenshots.
pub fn open_if_asked(mut welcome: ResMut<Welcome>, ledger: Res<Ledger>, mut done: Local<bool>) {
    if *done {
        return;
    }
    *done = true;
    if std::env::var_os("GENVERSE_WELCOME").is_some() {
        welcome.show(&ledger);
        welcome.step = std::env::var("GENVERSE_WELCOME_STEP").ok().and_then(|s| s.parse().ok()).unwrap_or(0).min(STEPS.len() - 1);
    }
}

enum Go {
    Back,
    Next,
}

pub fn welcome_window(
    mut contexts: EguiContexts,
    mut welcome: ResMut<Welcome>,
    mut ledger: ResMut<Ledger>,
    mut chat: ResMut<Chat>,
    mut menu_state: ResMut<GameMenu>,
    loading: Res<Loading>,
) -> Result {
    if !welcome.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let screen = ctx.viewport_rect();
    let found = welcome.found();
    // The agent picked must be installed; otherwise the first one that is.
    let installed = |provider: Provider| Provider::ALL.iter().position(|p| *p == provider).is_some_and(|i| matches!(found.get(i), Some(Found::Installed(..))));
    if !installed(chat.provider)
        && let Some(first) = Provider::ALL.into_iter().find(|p| installed(*p))
    {
        chat.provider = first;
    }
    let none = found.iter().all(|f| matches!(f, Found::Missing));
    let last = welcome.step == STEPS.len() - 1;
    let ready = loading.is_done();
    let mut go = None;

    egui::Area::new("welcome".into())
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            // Takes the clicks meant for the world behind it.
            let _ = ui.allocate_exact_size(screen.size(), egui::Sense::click_and_drag());
            let height = ui.ctx().data(|d| d.get_temp::<f32>(egui::Id::new("welcome_height"))).unwrap_or(420.0);
            let top = screen.top() + ((screen.height() - height) / 2.0).max(16.0);
            // Centered on the world, beside the builder panel.
            let middle = screen.center().x - chat.covered * screen.width() / 2.0;
            let column = egui::Rect::from_min_size(egui::pos2(middle - WIDTH / 2.0, top), egui::vec2(WIDTH, screen.height()));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(column));
            let inner = egui::Frame::new()
                .fill(p.panel)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(theme::ROUND + 2)
                .inner_margin(egui::Margin::same(20))
                .shadow(egui::Shadow { offset: [0, 10], blur: 28, spread: 0, color: egui::Color32::from_black_alpha(90) })
                .show(&mut child, |ui| {
                    ui.set_width(WIDTH - 40.0);
                    steps(ui, welcome.step);
                    ui.add_space(12.0);
                    match welcome.step {
                        0 => {
                            ui.label(egui::RichText::new("Welcome to Genverse").size(22.0).strong().color(p.strong));
                            ui.label(egui::RichText::new("Describe what you want, and an agent builds it into the world.").size(13.0).color(p.muted));
                            ui.add_space(14.0);
                            theme::section(ui, "Your name");
                            let field = ui.add(egui::TextEdit::singleline(&mut welcome.name).desired_width(f32::INFINITY).char_limit(32).hint_text("what others see over your head"));
                            if ui.memory(|m| m.focused().is_none()) {
                                field.request_focus();
                            }
                            ui.add_space(8.0);
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
                            theme::note(ui, "Your ring, outlines and name tag when you play with others");
                        }
                        1 => {
                            ui.label(egui::RichText::new("Who builds for you").size(22.0).strong().color(p.strong));
                            ui.label(
                                egui::RichText::new("Builds, land and games are made by a coding agent on this computer. Pick the one you use.")
                                    .size(13.0)
                                    .color(p.muted),
                            );
                            ui.add_space(14.0);
                            for (i, provider) in Provider::ALL.into_iter().enumerate() {
                                agent_row(ui, provider, found.get(i).cloned().unwrap_or(Found::Looking), &mut chat.provider);
                            }
                            ui.add_space(4.0);
                            if none {
                                ui.label(
                                    egui::RichText::new(format!("{} Neither is installed. You can still explore, play games and join friends; install one to build.", icon::WARNING))
                                        .size(12.0)
                                        .color(p.warn),
                                );
                                ui.add_space(4.0);
                                if ui.add(theme::Block::new(format!("{} Look again", icon::ARROWS_CLOCKWISE), Tone::Plain)).clicked() {
                                    welcome.check();
                                }
                            } else if !found.iter().any(|f| matches!(f, Found::Looking)) {
                                theme::note(ui, "Sign in once from a terminal: run it with no arguments. Requests use your own plan.");
                            }
                        }
                        _ => {
                            ui.label(egui::RichText::new("The basics").size(22.0).strong().color(p.strong));
                            ui.label(egui::RichText::new("The bar along the bottom always shows the keys that work right now.").size(13.0).color(p.muted));
                            ui.add_space(14.0);
                            egui::Grid::new("welcome_keys").num_columns(2).spacing(egui::vec2(12.0, 6.0)).show(ui, |ui| {
                                for (keys, what) in [
                                    ("WASD · drag", "walk and look around"),
                                    ("T", "describe something to build"),
                                    ("click", "select a build, then T to change it"),
                                    ("G", "select ground to reshape"),
                                    ("Ctrl/Cmd+Z", "undo"),
                                    ("Esc", "the game menu"),
                                ] {
                                    ui.label(egui::RichText::new(keys).monospace().size(12.0).color(p.strong));
                                    ui.label(egui::RichText::new(what).size(12.5).color(p.text));
                                    ui.end_row();
                                }
                            });
                            if !none {
                                ui.add_space(10.0);
                                theme::note(ui, "Try first: \"a cozy wooden bench\", or select some ground and ask for \"a pond with reeds\".");
                            }
                        }
                    }
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        if welcome.step > 0 && ui.add(theme::Block::new(format!("{} Back", icon::ARROW_LEFT), Tone::Plain)).clicked() {
                            go = Some(Go::Back);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let label = match (last, ready, none) {
                                (false, ..) => format!("Next {}", icon::ARROW_RIGHT),
                                (true, false, _) => format!("Loading… {:.0}%", loading.progress() * 100.0),
                                (true, true, true) => format!("Explore {}", icon::ARROW_RIGHT),
                                (true, true, false) => format!("Start building {}", icon::ARROW_RIGHT),
                            };
                            let enabled = !last || ready;
                            if ui.add_enabled(enabled, theme::Block::new(label, Tone::Primary)).on_hover_text("Enter").clicked() {
                                go = Some(Go::Next);
                            }
                        });
                    });
                });
            let height = inner.response.rect.height();
            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("welcome_height"), height));
        });

    // Enter goes on, wherever the focus is.
    if go.is_none() && (!last || ready) && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
        go = Some(Go::Next);
    }
    match go {
        Some(Go::Back) => welcome.step -= 1,
        Some(Go::Next) if !last => {
            // The name is kept as soon as it's given, so it's there whatever comes next.
            if welcome.step == 0 {
                ledger.rename(&welcome.name);
            }
            welcome.step += 1;
        }
        Some(Go::Next) => {
            prefs::set("welcomed", true);
            prefs::set("agent", command(chat.provider));
            welcome.open = false;
            menu_state.open = false;
            // Straight to the first request, if there's an agent to send it to.
            if installed(chat.provider) {
                chat.open = true;
                chat.focus_input = true;
            }
        }
        None => {}
    }
    Ok(())
}

/// The steps along the top: done, current, still to come.
fn steps(ui: &mut egui::Ui, current: usize) {
    let p = theme::palette(ui.ctx());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (i, name) in STEPS.iter().enumerate() {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(if i == current { 22.0 } else { 8.0 }, 8.0), egui::Sense::hover());
            let color = if i <= current { p.primary } else { p.line };
            ui.painter().rect_filled(rect.translate(egui::vec2(0.0, 2.0)), 4, color);
            if i == current {
                ui.label(egui::RichText::new(format!("{} of {} · {name}", i + 1, STEPS.len())).size(11.0).color(p.muted));
            }
        }
    });
}

/// One agent: whether it's installed, picking it, or where to get it.
fn agent_row(ui: &mut egui::Ui, provider: Provider, found: Found, picked: &mut Provider) {
    let p = theme::palette(ui.ctx());
    ui.horizontal(|ui| {
        match &found {
            Found::Looking => {
                ui.spinner();
                ui.label(egui::RichText::new(provider.label()).color(p.text));
                ui.label(egui::RichText::new("looking…").size(12.0).color(p.muted));
            }
            Found::Missing => {
                ui.label(egui::RichText::new(icon::X_CIRCLE).size(16.0).color(p.faint));
                ui.label(egui::RichText::new(provider.label()).color(p.muted));
                ui.label(egui::RichText::new("not installed").size(12.0).color(p.muted));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.hyperlink_to(format!("Install {}", icon::ARROW_SQUARE_OUT), install_page(provider));
                });
            }
            Found::Installed(path, version) => {
                ui.radio_value(picked, provider, "");
                ui.label(egui::RichText::new(provider.label()).color(p.strong));
                let shown = if version.is_empty() { "installed".to_string() } else { version.clone() };
                ui.label(egui::RichText::new(shown).size(12.0).color(p.muted)).on_hover_text(path.display().to_string());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(icon::CHECK_CIRCLE).size(16.0).color(p.online));
                });
            }
        }
    });
}
