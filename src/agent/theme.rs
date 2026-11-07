//! The builder's look: Discord's layout with a voxel world's materials. Corners stay
//! nearly square, avatars and buttons are little blocks with a darker bottom lip, and
//! the colors come from the world itself: grass, sky, terracotta, sand, and either
//! sunlit parchment (light) or dusk slate (dark). Applies to every egui window.

use bevy_egui::egui::{self, Color32, CornerRadius, FontId, Margin, Stroke, TextStyle, Theme};

use super::prefs;

/// Corner radius for blocks: just enough to soften the pixels.
pub const ROUND: u8 = 3;

pub struct Palette {
    /// The panel and message list.
    pub panel: Color32,
    /// Cards, the header's controls, plain buttons at rest.
    pub raised: Color32,
    /// The name bar at the bottom.
    pub footer: Color32,
    pub input: Color32,
    pub hover: Color32,
    pub line: Color32,
    /// Floating hint bars over the world.
    pub overlay: Color32,
    pub text: Color32,
    /// Headings and names.
    pub strong: Color32,
    pub muted: Color32,
    pub faint: Color32,
    /// Grass: the main action.
    pub primary: Color32,
    pub on_primary: Color32,
    /// Lava: delete and leave.
    pub danger: Color32,
    /// Sky: a selected build.
    pub select: Color32,
    /// Sand: a draft that can't go here.
    pub warn: Color32,
    pub online: Color32,
    pub me: Color32,
    pub claude: Color32,
    pub codex: Color32,
}

pub const DARK: Palette = Palette {
    panel: Color32::from_rgb(31, 37, 40),
    raised: Color32::from_rgb(42, 50, 54),
    footer: Color32::from_rgb(24, 29, 32),
    input: Color32::from_rgb(46, 55, 59),
    hover: Color32::from_rgb(37, 44, 48),
    line: Color32::from_rgb(18, 22, 24),
    overlay: Color32::from_rgba_premultiplied(22, 27, 29, 225),
    text: Color32::from_rgb(232, 226, 211),
    strong: Color32::from_rgb(250, 246, 236),
    muted: Color32::from_rgb(164, 162, 145),
    faint: Color32::from_rgb(116, 120, 108),
    primary: Color32::from_rgb(124, 193, 68),
    on_primary: Color32::from_rgb(22, 36, 12),
    danger: Color32::from_rgb(224, 83, 61),
    select: Color32::from_rgb(98, 184, 232),
    warn: Color32::from_rgb(242, 193, 78),
    online: Color32::from_rgb(124, 193, 68),
    me: Color32::from_rgb(78, 160, 214),
    claude: Color32::from_rgb(214, 118, 82),
    codex: Color32::from_rgb(52, 172, 146),
};

pub const LIGHT: Palette = Palette {
    panel: Color32::from_rgb(246, 241, 229),
    raised: Color32::from_rgb(234, 226, 208),
    footer: Color32::from_rgb(227, 217, 196),
    input: Color32::from_rgb(255, 252, 244),
    hover: Color32::from_rgb(238, 231, 214),
    line: Color32::from_rgb(208, 196, 170),
    overlay: Color32::from_rgba_premultiplied(242, 235, 220, 232),
    text: Color32::from_rgb(52, 46, 36),
    strong: Color32::from_rgb(30, 26, 20),
    muted: Color32::from_rgb(112, 102, 84),
    faint: Color32::from_rgb(156, 146, 126),
    primary: Color32::from_rgb(84, 150, 46),
    on_primary: Color32::from_rgb(250, 255, 240),
    danger: Color32::from_rgb(196, 68, 46),
    select: Color32::from_rgb(52, 140, 200),
    warn: Color32::from_rgb(196, 132, 28),
    online: Color32::from_rgb(84, 150, 46),
    me: Color32::from_rgb(52, 132, 192),
    claude: Color32::from_rgb(196, 98, 62),
    codex: Color32::from_rgb(30, 146, 120),
};

/// The palette for the theme being shown.
pub fn palette(ctx: &egui::Context) -> &'static Palette {
    match ctx.theme() {
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    }
}

/// Styles both themes and shows the one chosen last time (dark the first time).
pub fn apply(ctx: &egui::Context) {
    ctx.style_mut_of(Theme::Dark, |style| restyle(style, &DARK, true));
    ctx.style_mut_of(Theme::Light, |style| restyle(style, &LIGHT, false));
    let light = prefs::get("theme").is_some_and(|v| v == "light");
    ctx.set_theme(if light { Theme::Light } else { Theme::Dark });
}

/// Switches between light and dark, and remembers it.
pub fn toggle(ctx: &egui::Context) {
    let theme = match ctx.theme() {
        Theme::Dark => Theme::Light,
        Theme::Light => Theme::Dark,
    };
    ctx.set_theme(theme);
    prefs::set("theme", if theme == Theme::Light { "light" } else { "dark" });
}

fn restyle(style: &mut egui::Style, p: &Palette, dark: bool) {
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.5)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(13.5)),
        (TextStyle::Heading, FontId::proportional(16.0)),
        (TextStyle::Monospace, FontId::monospace(12.5)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 24.0;

    let v = &mut style.visuals;
    *v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.panel;
    v.window_fill = p.panel;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_corner_radius = CornerRadius::same(ROUND + 1);
    v.menu_corner_radius = CornerRadius::same(ROUND);
    v.extreme_bg_color = p.input;
    v.text_edit_bg_color = Some(p.input);
    v.faint_bg_color = p.raised;
    v.code_bg_color = p.raised;
    v.hyperlink_color = p.select;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = p.primary.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, p.primary);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = CornerRadius::same(ROUND);
    let (rest, lit, pressed) = if dark {
        (p.raised, Color32::from_rgb(56, 66, 71), Color32::from_rgb(66, 78, 84))
    } else {
        (p.raised, Color32::from_rgb(222, 212, 190), Color32::from_rgb(210, 198, 172))
    };
    for (state, fill) in [(&mut w.inactive, rest), (&mut w.hovered, lit), (&mut w.active, pressed), (&mut w.open, lit)] {
        state.weak_bg_fill = fill;
        state.bg_fill = p.input;
        state.bg_stroke = Stroke::new(1.0, p.line);
        state.corner_radius = CornerRadius::same(ROUND);
        state.expansion = 0.0;
        state.fg_stroke = Stroke::new(1.0, p.text);
    }
    w.hovered.fg_stroke = Stroke::new(1.0, p.strong);
    w.active.fg_stroke = Stroke::new(1.0, p.strong);
}

/// What a [`Block`] button is for, which picks its material.
#[derive(Clone, Copy)]
pub enum Tone {
    Plain,
    Primary,
    Danger,
}

/// A chunky block button: nearly square corners and a darker bottom lip that
/// disappears while it's pressed, as if pushed in.
pub struct Block {
    text: String,
    tone: Tone,
}

impl Block {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self { text: text.into(), tone }
    }
}

impl egui::Widget for Block {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let p = palette(ui.ctx());
        let (fill, text) = match self.tone {
            Tone::Plain => (p.raised, p.text),
            Tone::Primary => (p.primary, p.on_primary),
            Tone::Danger => (p.danger, Color32::WHITE),
        };
        let response = ui.add(
            egui::Button::new(egui::RichText::new(self.text).color(text))
                .fill(fill)
                .stroke(Stroke::new(1.0, shade(fill, 0.8)))
                .corner_radius(ROUND),
        );
        let rect = response.rect;
        let painter = ui.painter();
        if response.hovered() && ui.is_enabled() {
            let glow = if ui.visuals().dark_mode { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(12) };
            painter.rect_filled(rect, ROUND, glow);
        }
        if !response.is_pointer_button_down_on() {
            let lip = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 3.0), rect.max);
            painter.rect_filled(lip, CornerRadius { nw: 0, ne: 0, sw: ROUND, se: ROUND }, shade(fill, 0.72));
        }
        response
    }
}

/// The same color, darker, like a block's shaded side.
pub fn shade(color: Color32, by: f32) -> Color32 {
    let [r, g, b, a] = color.to_array();
    let dim = |c: u8| (c as f32 * by) as u8;
    Color32::from_rgba_premultiplied(dim(r), dim(g), dim(b), a)
}

/// The same color, lighter, like a block's sunlit top.
fn light(color: Color32, by: f32) -> Color32 {
    let [r, g, b, a] = color.to_array();
    let lift = |c: u8| (c as f32 + (255.0 - c as f32) * by) as u8;
    Color32::from_rgba_premultiplied(lift(r), lift(g), lift(b), a)
}

/// A frameless icon button, like the ones in Discord's headers.
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str) -> bool {
    let muted = palette(ui.ctx()).muted;
    let response = ui.add(egui::Button::new(egui::RichText::new(icon).size(18.0).color(muted)).frame(false));
    response.on_hover_text(tip).clicked()
}

/// An avatar as a little block: the name's first letter on a cube face, lit on top
/// and shaded along the bottom.
pub fn avatar(ui: &mut egui::Ui, name: &str, color: Color32, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter();
    let radius = ROUND + 2;
    painter.rect_filled(rect, radius, shade(color, 0.72));
    let face = egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), rect.bottom() - size * 0.1));
    painter.rect_filled(face, radius, color);
    let top = egui::Rect::from_min_max(face.min, egui::pos2(face.right(), face.top() + size * 0.14));
    painter.rect_filled(top, CornerRadius { nw: radius, ne: radius, sw: 0, se: 0 }, light(color, 0.22));
    let initial = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    painter.text(face.center(), egui::Align2::CENTER_CENTER, initial, FontId::proportional(size * 0.46), Color32::WHITE);
    response
}

/// An embed-style card with a colored left edge.
pub fn card<R>(ui: &mut egui::Ui, accent: Color32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let p = palette(ui.ctx());
    let inner = egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(ROUND)
        .inner_margin(Margin { left: 14, right: 10, top: 8, bottom: 9 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    let rect = inner.response.rect;
    let edge = egui::Rect::from_min_size(rect.min, egui::vec2(4.0, rect.height()));
    ui.painter().rect_filled(edge, CornerRadius { nw: ROUND, sw: ROUND, ne: 0, se: 0 }, accent);
    inner.inner
}

/// A card's title line: an icon and a bold name.
pub fn card_title(ui: &mut egui::Ui, icon: &str, title: &str) {
    let strong = palette(ui.ctx()).strong;
    ui.label(egui::RichText::new(format!("{icon} {title}")).strong().color(strong));
}

/// Small print inside cards and footers.
pub fn note(ui: &mut egui::Ui, text: impl Into<String>) {
    let muted = palette(ui.ctx()).muted;
    ui.label(egui::RichText::new(text.into()).small().color(muted));
}

/// An uppercase section heading, like Discord's channel categories.
pub fn section(ui: &mut egui::Ui, text: &str) {
    let muted = palette(ui.ctx()).muted;
    ui.label(egui::RichText::new(text.to_uppercase()).size(11.0).strong().color(muted));
}

/// A small tag, such as AGENT next to an agent's name.
pub fn tag(ui: &mut egui::Ui, text: &str, fill: Color32, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(9.5), color);
    let (rect, _) = ui.allocate_exact_size(galley.size() + egui::vec2(8.0, 2.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2, fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
}

/// A keyboard key, for shortcut hints: a keycap with a shaded lip. Quiet keys are
/// smaller and flatter, for hints that are always there.
pub fn key(ui: &mut egui::Ui, text: &str, quiet: bool) {
    let p = palette(ui.ctx());
    let (size, color) = if quiet { (10.5, p.text) } else { (11.5, p.strong) };
    let response = egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(ROUND)
        .inner_margin(Margin { left: 5, right: 5, top: 1, bottom: if quiet { 2 } else { 3 } })
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(size).strong().color(color));
        })
        .response;
    let rect = response.rect;
    let lip = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - if quiet { 1.5 } else { 2.0 }), rect.max);
    ui.painter().rect_filled(lip, CornerRadius { nw: 0, ne: 0, sw: ROUND, se: ROUND }, p.line);
}
