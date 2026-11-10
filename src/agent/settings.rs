//! The settings window (the gauge in the builder's header): display (window mode,
//! resolution, render scale, vsync) and graphics, one dropdown per effect plus a preset
//! that sets them all, like most desktop games. Both are kept in saves/settings.json.

use bevy::{
    prelude::*,
    window::{
        Monitor, MonitorSelection, PresentMode, PrimaryMonitor, PrimaryWindow, VideoMode, VideoModeSelection,
        WindowMode, WindowPosition,
    },
};
use bevy_egui::{EguiContexts, egui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::bindings::{ACTIONS, Bindings, INPUTS};
use super::{prefs, theme};
use crate::graphics::{Antialias, Graphics, Level, Quality, RENDER_SCALES};

/// Window sizes offered in windowed mode, in points (a Retina screen draws twice as
/// many pixels each way).
const WINDOW_SIZES: [(u32, u32); 8] =
    [(1280, 720), (1366, 768), (1440, 900), (1600, 900), (1680, 1050), (1920, 1080), (2560, 1440), (3840, 2160)];

#[derive(Resource, Default)]
pub struct SettingsWindow {
    pub open: bool,
    display: Display,
    /// The action waiting for a key press, in Controls.
    capturing: Option<&'static str>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
enum Mode {
    #[default]
    Windowed,
    /// A window the size of the screen, without a frame.
    Borderless,
    /// The screen switches to the chosen resolution.
    Fullscreen,
}

impl Mode {
    const ALL: [Mode; 3] = [Mode::Windowed, Mode::Borderless, Mode::Fullscreen];

    fn label(self) -> &'static str {
        match self {
            Mode::Windowed => "Windowed",
            Mode::Borderless => "Borderless fullscreen",
            Mode::Fullscreen => "Fullscreen",
        }
    }
}

/// The display settings as saved; the window itself holds the live state.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Display {
    mode: Mode,
    /// Windowed size, in points; `None` keeps whatever the window is.
    window: Option<(u32, u32)>,
    /// Fullscreen resolution in pixels and refresh rate in millihertz; `None` keeps
    /// the screen's own.
    fullscreen: Option<(u32, u32, u32)>,
    vsync: bool,
}

impl Default for Display {
    fn default() -> Self {
        Self { mode: Mode::Windowed, window: None, fullscreen: None, vsync: true }
    }
}

impl Display {
    fn apply(&self, window: &mut Window, monitor: Option<&Monitor>) {
        window.present_mode = if self.vsync { PresentMode::AutoVsync } else { PresentMode::AutoNoVsync };
        window.mode = match self.mode {
            Mode::Windowed => WindowMode::Windowed,
            Mode::Borderless => WindowMode::BorderlessFullscreen(MonitorSelection::Current),
            Mode::Fullscreen => {
                let wanted = self.fullscreen.and_then(|(w, h, hz)| {
                    monitor?.video_modes.iter().find(|m| m.physical_size == UVec2::new(w, h) && m.refresh_rate_millihertz == hz)
                });
                let video = wanted.map_or(VideoModeSelection::Current, |m| VideoModeSelection::Specific(*m));
                WindowMode::Fullscreen(MonitorSelection::Current, video)
            }
        };
        if let (Mode::Windowed, Some((w, h))) = (self.mode, self.window) {
            window.resolution.set(w as f32, h as f32);
        }
    }
}

/// Saved settings, applied at the start. `GENVERSE_QUALITY=low|medium|high|ultra`
/// starts from that preset instead, for one run; `GENVERSE_SETTINGS=1` opens the window.
pub fn load_settings(
    mut settings: ResMut<SettingsWindow>,
    mut graphics: ResMut<Graphics>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    monitors: Query<&Monitor, With<PrimaryMonitor>>,
) {
    if let Some(saved) = prefs::get("graphics").and_then(|v| serde_json::from_value(v).ok()) {
        *graphics = saved;
    }
    if let Some(q) = std::env::var("GENVERSE_QUALITY").ok().and_then(|k| Quality::from_key(&k)) {
        *graphics = q.settings(graphics.render_scale, graphics.auto_scale);
    }
    let display: Display = prefs::get("display").and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    settings.display = display;
    settings.open = std::env::var("GENVERSE_SETTINGS").is_ok();
    if display != Display::default()
        && let Ok(mut window) = windows.single_mut()
    {
        display.apply(&mut window, monitors.single().ok());
    }
}

/// The screen the window is on, as far as we can tell: the one holding its corner, or
/// the main one.
fn current_monitor<'a>(window: &Window, monitors: &'a Query<(&Monitor, Has<PrimaryMonitor>)>) -> Option<&'a Monitor> {
    if let WindowPosition::At(at) = window.position {
        let holding = monitors.iter().find(|(m, _)| {
            let size = IVec2::new(m.physical_width as i32, m.physical_height as i32);
            at.cmpge(m.physical_position).all() && at.cmplt(m.physical_position + size).all()
        });
        if let Some((m, _)) = holding {
            return Some(m);
        }
    }
    monitors.iter().find(|(_, primary)| *primary).or(monitors.iter().next()).map(|(m, _)| m)
}

/// The modes a screen offers, largest first, each size once at its fastest refresh.
fn fullscreen_modes(monitor: &Monitor) -> Vec<VideoMode> {
    let mut modes = monitor.video_modes.clone();
    modes.sort_by_key(|m| std::cmp::Reverse((m.physical_size.x, m.physical_size.y, m.refresh_rate_millihertz)));
    modes.dedup_by_key(|m| m.physical_size);
    modes
}

pub fn settings_window(
    mut contexts: EguiContexts,
    mut settings: ResMut<SettingsWindow>,
    mut graphics: ResMut<Graphics>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    monitors: Query<(&Monitor, Has<PrimaryMonitor>)>,
    mut binds: ResMut<Bindings>,
    (keys, buttons): (Res<ButtonInput<KeyCode>>, Res<ButtonInput<MouseButton>>),
) -> Result {
    if !settings.open {
        settings.capturing = None;
        binds.capturing = false;
        return Ok(());
    }
    // Controls waiting for a key: the next one pressed is bound (Esc cancels). A left
    // click is how the list itself is used, so it's bound from the chat instead.
    if let Some(action) = settings.capturing {
        if keys.just_pressed(KeyCode::Escape) {
            settings.capturing = None;
        } else if let Some(input) = INPUTS.iter().find(|input| input.name != "mouse_left" && input.just_pressed(&keys, &buttons)) {
            let _ = binds.bind(action, input.name);
            super::ops::keep_bindings(&binds);
            settings.capturing = None;
        }
    }
    binds.capturing = settings.capturing.is_some();
    let mut capturing = settings.capturing;
    let mut rebind = None;
    let Ok(mut window) = windows.single_mut() else { return Ok(()) };
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let monitor = current_monitor(&window, &monitors);
    let mut display = settings.display;
    let before = display;
    let mut g = *graphics;
    let mut open = settings.open;

    theme::centered(egui::Window::new(format!("{} Settings", icon::GAUGE)), ctx, 0.0)
        .open(&mut open)
        .collapsible(false)
        .default_width(380.0)
        .show(ctx, |ui| {
            let physical = window.physical_size();
            theme::section(ui, "Display");
            grid(ui, "display_grid", |ui| {
                ui.label("Window mode");
                pick(ui, "window_mode", &mut display.mode, &Mode::ALL, |m| m.label().into());
                ui.end_row();

                ui.label("Resolution");
                match display.mode {
                    Mode::Windowed => {
                        let size = window.size().round().as_uvec2();
                        let fits = |&(w, h): &(u32, u32)| {
                            monitor.is_none_or(|m| {
                                let screen = Vec2::new(m.physical_width as f32, m.physical_height as f32) / m.scale_factor as f32;
                                w as f32 <= screen.x && h as f32 <= screen.y
                            })
                        };
                        let sizes: Vec<_> = WINDOW_SIZES.into_iter().filter(fits).map(Some).collect();
                        let mut current = Some((size.x, size.y));
                        let shown = |s: &Option<(u32, u32)>| s.map_or(String::new(), |(w, h)| format!("{w} × {h}"));
                        if pick(ui, "window_size", &mut current, &sizes, shown) {
                            display.window = current;
                        }
                    }
                    Mode::Borderless => {
                        ui.label(egui::RichText::new(format!("{} × {} (the screen's)", physical.x, physical.y)).color(p.muted));
                    }
                    Mode::Fullscreen => match monitor {
                        Some(monitor) => {
                            let modes: Vec<_> = fullscreen_modes(monitor).into_iter().map(Some).collect();
                            let mut current = display.fullscreen.and_then(|(w, h, hz)| {
                                modes.iter().flatten().find(|m| m.physical_size == UVec2::new(w, h) && m.refresh_rate_millihertz == hz).copied()
                            });
                            let shown = |m: &Option<VideoMode>| match m {
                                Some(m) => format!(
                                    "{} × {} · {} Hz",
                                    m.physical_size.x,
                                    m.physical_size.y,
                                    (m.refresh_rate_millihertz as f32 / 1000.0).round()
                                ),
                                None => "The screen's own".into(),
                            };
                            let mut choices = vec![None];
                            choices.extend(modes);
                            if pick(ui, "fullscreen_mode", &mut current, &choices, shown) {
                                display.fullscreen =
                                    current.map(|m| (m.physical_size.x, m.physical_size.y, m.refresh_rate_millihertz));
                            }
                        }
                        None => {
                            ui.label(egui::RichText::new("The screen's own").color(p.muted));
                        }
                    },
                }
                ui.end_row();

                ui.label("Render scale").on_hover_text("Draws the world at fewer pixels and stretches it to fit; the interface stays sharp");
                let shown = |s: &f32| {
                    let px = (physical.as_vec2() * *s).round();
                    format!("{:.0}%  ({} × {})", s * 100.0, px.x, px.y)
                };
                let mut scale = RENDER_SCALES.into_iter().min_by(|a, b| (a - g.render_scale).abs().total_cmp(&(b - g.render_scale).abs())).unwrap_or(1.0);
                if pick(ui, "render_scale", &mut scale, &RENDER_SCALES, shown) {
                    g.render_scale = scale;
                }
                ui.end_row();

                ui.label("Auto render scale").on_hover_text(
                    "Draws at fewer pixels while frames run slow (under about 55 fps), and back up to the render scale when there's room",
                );
                toggle(ui, "auto_scale", &mut g.auto_scale);
                ui.end_row();

                ui.label("VSync").on_hover_text("Waits for the screen's refresh: no tearing, at most its rate");
                toggle(ui, "vsync", &mut display.vsync);
                ui.end_row();
            });

            ui.add_space(10.0);
            theme::section(ui, "Graphics");
            grid(ui, "graphics_grid", |ui| {
                ui.label("Preset");
                let mut preset = g.preset();
                let choices: Vec<_> = Quality::ALL.into_iter().map(Some).collect();
                let shown = |q: &Option<Quality>| q.map_or("Custom", |q| q.label()).to_string();
                if pick(ui, "preset", &mut preset, &choices, shown)
                    && let Some(q) = preset
                {
                    g = q.settings(g.render_scale, g.auto_scale);
                }
                ui.end_row();

                ui.label("Anti-aliasing").on_hover_text("FXAA is cheap; TAA is smoother and also softens shadows");
                pick(ui, "antialias", &mut g.antialias, &Antialias::ALL, |a| a.label().into());
                ui.end_row();

                ui.label("Ambient occlusion").on_hover_text("Shading in corners and under things; the costliest effect");
                pick(ui, "ambient_occlusion", &mut g.ambient_occlusion, &Level::ALL, |l| l.label().into());
                ui.end_row();

                ui.label("Shadows").on_hover_text("How far shadows reach and how sharp they are");
                pick(ui, "shadows", &mut g.shadows, &Level::ALL, |l| l.label().into());
                ui.end_row();

                ui.label("Contact shadows").on_hover_text("Fine shadows where things touch");
                ui.add_enabled_ui(g.shadows != Level::Off, |ui| toggle(ui, "contact_shadows", &mut g.contact_shadows));
                ui.end_row();

                ui.label("Bloom").on_hover_text("A glow around bright things");
                toggle(ui, "bloom", &mut g.bloom);
                ui.end_row();
            });
            ui.add_space(6.0);
            theme::note(ui, format!("{} shows the frame rate.", binds.label("stats")));

            ui.add_space(10.0);
            theme::section(ui, "Controls");
            theme::note(ui, "+ adds the next key you press; a key's button takes it off. Or ask the agent: \"let J jump too\".");
            egui::ScrollArea::vertical().max_height(280.0).auto_shrink([false, true]).show(ui, |ui| {
                let mut group = "";
                for action in ACTIONS {
                    if action.group != group {
                        group = action.group;
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(group).strong().color(p.strong));
                    }
                    ui.horizontal(|ui| {
                        let name = action.name.replace('_', " ");
                        // A column of names, left-aligned, so the keys line up.
                        ui.allocate_ui_with_layout(egui::vec2(110.0, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(110.0);
                            ui.label(egui::RichText::new(name).color(p.text)).on_hover_text(action.title);
                        });
                        for input in binds.inputs(action.name) {
                            if ui.small_button(format!("{} {}", input.label, icon::X)).on_hover_text("Take this key off").clicked() {
                                rebind = Some(Rebind::Unbind(action.name, input.name));
                            }
                        }
                        if capturing == Some(action.name) {
                            ui.label(egui::RichText::new("press a key… (Esc cancels)").small().color(p.primary));
                        } else if ui.small_button(icon::PLUS).on_hover_text("Add a key").clicked() {
                            capturing = Some(action.name);
                        }
                        if !binds.is_default(action) && ui.small_button(icon::ARROW_COUNTER_CLOCKWISE).on_hover_text("Back to its default").clicked() {
                            rebind = Some(Rebind::Reset(Some(action.name)));
                        }
                    });
                }
            });
            ui.add_space(4.0);
            if ui.button("Reset every key").clicked() {
                rebind = Some(Rebind::Reset(None));
            }
        });

    settings.capturing = capturing.filter(|_| open);
    if let Some(rebind) = rebind {
        let _ = match rebind {
            Rebind::Unbind(action, input) => binds.unbind(action, Some(input)),
            Rebind::Reset(action) => binds.reset(action),
        };
        super::ops::keep_bindings(&binds);
    }
    settings.open = open;
    if display != before {
        settings.display = display;
    settings.open = std::env::var("GENVERSE_SETTINGS").is_ok();
        display.apply(&mut window, monitor);
        prefs::set("display", serde_json::to_value(display).unwrap_or_default());
    }
    if g != *graphics {
        *graphics = g;
        prefs::set("graphics", serde_json::to_value(g).unwrap_or_default());
    }
    Ok(())
}

/// A change made in the Controls list.
enum Rebind {
    Unbind(&'static str, &'static str),
    Reset(Option<&'static str>),
}

fn grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([16.0, 8.0]).min_col_width(130.0).show(ui, add);
}

/// A dropdown of `choices`; true when the pick changed.
fn pick<T: PartialEq + Clone>(ui: &mut egui::Ui, id: &str, value: &mut T, choices: &[T], label: impl Fn(&T) -> String) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).width(200.0).selected_text(label(value)).show_ui(ui, |ui| {
        for choice in choices {
            if ui.selectable_label(*value == *choice, label(choice)).clicked() && *value != *choice {
                *value = choice.clone();
                changed = true;
            }
        }
    });
    changed
}

fn toggle(ui: &mut egui::Ui, id: &str, on: &mut bool) {
    pick(ui, id, on, &[true, false], |&b| if b { "On" } else { "Off" }.into());
}
