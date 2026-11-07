//! The builder's look, after Discord: layered dark grays, a blurple accent, round
//! avatars, and cards with a colored edge. Applies to every egui window, not just the panel.

use bevy_egui::egui::{self, Color32, CornerRadius, FontId, Margin, Stroke, TextStyle};

pub const CHAT: Color32 = Color32::from_rgb(49, 51, 56);
pub const SIDE: Color32 = Color32::from_rgb(43, 45, 49);
pub const FOOTER: Color32 = Color32::from_rgb(35, 36, 40);
pub const INPUT: Color32 = Color32::from_rgb(56, 58, 64);
pub const HOVER: Color32 = Color32::from_rgb(46, 48, 53);
pub const CARD: Color32 = Color32::from_rgb(43, 45, 49);
pub const LINE: Color32 = Color32::from_rgb(31, 32, 35);
pub const TEXT: Color32 = Color32::from_rgb(219, 222, 225);
pub const MUTED: Color32 = Color32::from_rgb(148, 155, 164);
pub const FAINT: Color32 = Color32::from_rgb(110, 116, 124);
pub const BLURPLE: Color32 = Color32::from_rgb(88, 101, 242);
pub const GREEN: Color32 = Color32::from_rgb(36, 128, 70);
pub const ONLINE: Color32 = Color32::from_rgb(35, 165, 90);
pub const RED: Color32 = Color32::from_rgb(218, 55, 60);
pub const WARN: Color32 = Color32::from_rgb(255, 150, 120);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(13.5)),
            (TextStyle::Heading, FontId::proportional(16.0)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.interact_size.y = 24.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = None;
        v.panel_fill = CHAT;
        v.window_fill = CHAT;
        v.window_stroke = Stroke::new(1.0, LINE);
        v.window_corner_radius = CornerRadius::same(8);
        v.extreme_bg_color = Color32::from_rgb(30, 31, 34);
        v.text_edit_bg_color = Some(INPUT);
        v.faint_bg_color = SIDE;
        v.code_bg_color = Color32::from_rgb(30, 31, 34);
        v.hyperlink_color = Color32::from_rgb(0, 168, 252);
        v.selection.bg_fill = BLURPLE.gamma_multiply(0.6);
        v.selection.stroke = Stroke::new(1.0, BLURPLE);

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = CHAT;
        w.noninteractive.weak_bg_fill = CHAT;
        w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        for (state, fill) in [
            (&mut w.inactive, Color32::from_rgb(78, 80, 88)),
            (&mut w.hovered, Color32::from_rgb(109, 111, 120)),
            (&mut w.active, Color32::from_rgb(128, 132, 142)),
            (&mut w.open, Color32::from_rgb(78, 80, 88)),
        ] {
            state.weak_bg_fill = fill;
            state.bg_fill = INPUT;
            state.bg_stroke = Stroke::NONE;
            state.corner_radius = CornerRadius::same(4);
            state.expansion = 0.0;
        }
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        w.open.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    });
}

/// A filled button in one of Discord's action colors.
pub fn button(text: impl Into<String>, fill: Color32) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(Color32::WHITE)).fill(fill)
}

/// A frameless icon button, like the ones in Discord's headers.
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tip: &str) -> bool {
    let response = ui.add(egui::Button::new(egui::RichText::new(icon).size(18.0).color(MUTED)).frame(false));
    response.on_hover_text(tip).clicked()
}

/// A round avatar with the name's first letter.
pub fn avatar(ui: &mut egui::Ui, name: &str, color: Color32, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter();
    painter.circle_filled(rect.center(), size / 2.0, color);
    let initial = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    painter.text(rect.center(), egui::Align2::CENTER_CENTER, initial, FontId::proportional(size * 0.45), Color32::WHITE);
    response
}

/// An embed-style card with a colored left edge.
pub fn card<R>(ui: &mut egui::Ui, accent: Color32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let inner = egui::Frame::new()
        .fill(CARD)
        .corner_radius(6)
        .inner_margin(Margin { left: 14, right: 10, top: 8, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    let rect = inner.response.rect;
    let edge = egui::Rect::from_min_size(rect.min, egui::vec2(4.0, rect.height()));
    ui.painter().rect_filled(edge, CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 }, accent);
    inner.inner
}

/// A card's title line: an icon and a bold name.
pub fn card_title(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.label(egui::RichText::new(format!("{icon} {title}")).strong().color(Color32::WHITE));
}

/// Small print inside cards and footers.
pub fn note(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(egui::RichText::new(text.into()).small().color(MUTED));
}

/// An uppercase section heading, like Discord's channel categories.
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).size(11.0).strong().color(MUTED));
}

/// A small pill tag, such as AGENT next to an agent's name.
pub fn tag(ui: &mut egui::Ui, text: &str, fill: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(9.5), Color32::WHITE);
    let (rect, _) = ui.allocate_exact_size(galley.size() + egui::vec2(8.0, 2.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 3, fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// A keyboard key, for shortcut hints.
pub fn key(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(64, 66, 73))
        .corner_radius(4)
        .inner_margin(Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(11.5).strong().color(Color32::WHITE));
        });
}
